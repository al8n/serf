//! Pure, transport-agnostic key-management apply logic shared by the serf async
//! driver crates.
//!
//! An inbound key-management request must be applied to the driver's LIVE wire
//! keyring — the coordinator's [`EncryptionOptions`] keyring the gossip plane
//! (and, on the stream transports, the reliable plane) encrypts under — so a
//! completed rotation actually re-keys the wire instead of updating a caller-held
//! shadow the wire never sees. This module holds the runtime- and
//! endpoint-independent core of that operation: given the current [`Keyring`] and
//! a request, it computes the [`KeyResponseArgs`] answer plus the rotated ring to
//! publish. Each driver wraps it with the endpoint read-modify-write — read
//! [`encryption_options`], publish via [`set_encryption_options`], notify the
//! persistence observer — which stays per-driver because the endpoint handles
//! differ.
//!
//! Every keyed op is variant-exact. [`SecretKey`] equality is variant-inclusive
//! (an AES-256 key and a ChaCha20-Poly1305 key with the same 32 bytes are
//! DISTINCT), but [`Keyring::promote`] and [`Keyring::remove_secondary`] match on
//! raw bytes alone. To keep those byte-keyed ops unambiguous the drivers uphold
//! one invariant: the live keyring is cross-cipher-collision-free at all times —
//! no two keys share a byte value across different cipher variants. It is
//! established at construction (each driver's build preflight rejects a seed ring
//! carrying a twin via [`keyring_carries_cross_cipher_twin`]) and preserved here
//! (an `install` whose bytes twin an existing key of another variant is refused).
//! Under that invariant, after the exact (variant + bytes) membership check each
//! `use` / `remove` performs, the byte-keyed promote / remove resolve to exactly
//! the requested key.
//!
//! [`EncryptionOptions`]: memberlist_proto::EncryptionOptions
//! [`encryption_options`]: serf_proto::StreamEndpoint::encryption_options
//! [`set_encryption_options`]: serf_proto::StreamEndpoint::set_encryption_options

#[cfg(test)]
mod tests;

use memberlist_proto::{Keyring, SecretKey};
use serf_proto::{KeyRequestOperation, KeyResponseArgs};

/// The outcome of applying one key-management [`KeyRequest`] to a live keyring.
///
/// Carries the [`KeyResponseArgs`] to return to the originator, plus the rotated
/// ring to publish to the wire — `Some` only when the op actually mutated the
/// ring. A read-only `list`, a trivial promote of the current primary, and every
/// refusal leave it `None`, so the wire is never needlessly re-keyed.
pub struct KeyApplyOutcome {
  response: KeyResponseArgs,
  rotated: Option<Keyring>,
}

impl KeyApplyOutcome {
  /// The response to hand back to the originator via `respond_key`.
  #[inline]
  pub fn response(&self) -> &KeyResponseArgs {
    &self.response
  }

  /// The rotated ring to publish to the live wire keyring, or `None` for a
  /// read-only or refused op that left the ring unchanged.
  #[inline]
  pub fn rotated(&self) -> Option<&Keyring> {
    self.rotated.as_ref()
  }

  /// Split into the response and the optional rotated ring.
  #[inline]
  pub fn into_parts(self) -> (KeyResponseArgs, Option<Keyring>) {
    (self.response, self.rotated)
  }
}

/// Whether `keyring` already holds a key whose raw bytes equal `key`'s but whose
/// cipher variant differs — the cross-cipher collision that makes a byte-keyed
/// keyring lookup ambiguous.
///
/// [`SecretKey`] equality is variant-inclusive, yet [`Keyring::promote`] and
/// [`Keyring::remove_secondary`] match on raw bytes alone, so admitting such a
/// twin would let a byte-keyed op resolve to the wrong cipher's key. Both the
/// install path and the construction preflight refuse it.
pub fn keyring_has_cross_cipher_twin(keyring: &Keyring, key: &SecretKey) -> bool {
  core::iter::once(keyring.primary_ref())
    .chain(keyring.secondaries())
    .any(|installed| installed.as_bytes() == key.as_bytes() && installed != key)
}

/// Whether `keyring` already carries a cross-cipher byte twin among its own keys —
/// any two of {primary, secondaries} sharing a raw byte value across different
/// cipher variants. Such a ring makes the coordinator's byte-keyed rotation ops
/// ambiguous, so a driver's construction preflight refuses it up front.
pub fn keyring_carries_cross_cipher_twin(keyring: &Keyring) -> bool {
  core::iter::once(keyring.primary_ref())
    .chain(keyring.secondaries())
    .any(|key| keyring_has_cross_cipher_twin(keyring, key))
}

/// Apply one inbound key-management request — its [`KeyRequestOperation`] and
/// optional key — to `current` (a snapshot of the live wire keyring), returning
/// the answer built from the post-op state and the rotated ring to publish when
/// the op mutated it.
///
/// Takes the op and key rather than the wire [`KeyRequest`](serf_proto::event::KeyRequest)
/// so it is transport- and test-agnostic; each driver forwards `req.op()` /
/// `req.key()`.
///
/// The variant-exact semantics:
///
/// - `install` inserts the key as a secondary (an already-installed key is a
///   success with no mutation, so nothing is republished; a cross-cipher byte
///   twin of an already-present key is refused),
/// - `use` promotes the exact (variant + bytes) key to primary — a promote of the
///   current primary is a trivial success with no wire change,
/// - `remove` drops the exact secondary — refusing the current primary,
/// - `list` snapshots the keys and primary from the current state.
///
/// A missing key on a keyed op, or a key absent from the ring, is refused with no
/// mutation. The caller handles the no-keyring-configured case; this helper always
/// receives a ring.
pub fn apply_key_request(
  current: &Keyring,
  op: KeyRequestOperation,
  key: Option<&SecretKey>,
) -> KeyApplyOutcome {
  let mut keyring = current.clone();
  let (response, mutated) = match (op, key) {
    (KeyRequestOperation::Install, Some(key)) => {
      // A cross-cipher byte twin of an already-present key is refused to keep the
      // byte-keyed rotation ops unambiguous. Installing a key that is already the
      // primary or an installed secondary is a success WITHOUT mutation, so a
      // retried install never republishes an unchanged ring to the observer.
      if keyring_has_cross_cipher_twin(&keyring, key) {
        (refused("cross-cipher key collision"), false)
      } else if keyring.primary_ref() == key || keyring.secondaries().contains(key) {
        (success(), false)
      } else {
        keyring.insert_secondary(*key);
        (success(), true)
      }
    }
    (KeyRequestOperation::Use, Some(key)) => {
      // Verify exact (variant + bytes) membership before the byte-keyed promote.
      // Promoting the current primary is a trivial success with no wire change; a
      // key absent from the live ring is refused with no mutation.
      if keyring.primary_ref() == key {
        (success(), false)
      } else if keyring.secondaries().contains(key) {
        match keyring.promote(key.as_bytes()) {
          Ok(()) => (success(), true),
          // Unreachable given the exact secondary membership just verified plus
          // the collision-free invariant; handled fail-closed.
          Err(_) => (refused("requested key is not installed"), false),
        }
      } else {
        (refused("requested key is not installed"), false)
      }
    }
    (KeyRequestOperation::Remove, Some(key)) => {
      // Exact (variant + bytes) membership required, mirroring `use`. Removing the
      // current primary is refused (operators promote a secondary first).
      if keyring.primary_ref() == key {
        (
          refused("cannot remove the primary key; promote a secondary first"),
          false,
        )
      } else if keyring.secondaries().contains(key) {
        match keyring.remove_secondary(key.as_bytes()) {
          Ok(()) => (success(), true),
          // Unreachable given the exact secondary membership just verified plus
          // the collision-free invariant; handled fail-closed.
          Err(_) => (refused("requested key is not installed"), false),
        }
      } else {
        (refused("requested key is not installed"), false)
      }
    }
    (KeyRequestOperation::List, _) => {
      let mut keys = Vec::with_capacity(1 + keyring.secondaries().len());
      keys.push(*keyring.primary_ref());
      keys.extend(keyring.secondaries().iter().copied());
      (
        KeyResponseArgs {
          result: true,
          primary_key: Some(*keyring.primary_ref()),
          keys,
          ..Default::default()
        },
        false,
      )
    }
    (_, None) => (
      refused("key-management request missing its required key"),
      false,
    ),
  };
  KeyApplyOutcome {
    response,
    rotated: if mutated { Some(keyring) } else { None },
  }
}

/// A bare success response (`result = true`, empty message / keys).
#[inline]
fn success() -> KeyResponseArgs {
  KeyResponseArgs {
    result: true,
    ..Default::default()
  }
}

/// A refusal response carrying a human-readable reason, leaving the ring unchanged.
#[inline]
fn refused(message: &str) -> KeyResponseArgs {
  KeyResponseArgs {
    result: false,
    message: message.into(),
    ..Default::default()
  }
}
