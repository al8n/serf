//! `Delegate` composite — compio's per-driver observation hook surface for serf.
//!
//! Composes four observation sub-traits (`MemberDelegate` / `UserEventDelegate`
//! / `QueryDelegate` / `KeyringDelegate`) and a join-admission veto
//! (`MergeDelegate`). All observation hooks are `!Send`-tolerant (native
//! `async fn`) — the driver loop owns the delegate and fires hooks on the
//! single `!Send` compio thread.
//!
//! `KeyringDelegate` and `MergeDelegate` are separate from the observation
//! `Delegate` composite: `KeyringDelegate` is sync (keyring ops must not
//! block) and acknowledges each rotation's persistence, and `MergeDelegate`
//! is the machine's synchronous push/pull filter, supplied at construction
//! rather than an observation hook.

#[cfg(all(encryption, unix))]
mod keyring_file;
mod void;

pub use void::VoidDelegate;

#[cfg(all(encryption, unix))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(any(feature = "aes-gcm", feature = "chacha20-poly1305"), unix)))
)]
pub use keyring_file::{FileKeyringDelegate, KeyringFileError};

#[cfg(encryption)]
pub use void::VoidKeyringDelegate;

#[cfg(any(feature = "tcp", feature = "quic"))]
use std::sync::Arc;

#[cfg(any(feature = "tcp", feature = "quic"))]
use serf_proto::{event::QueryEvent, members::Member, typed::UserEventMessage};

#[cfg(encryption)]
use memberlist_proto::Keyring;

/// Async observation hooks for serf membership events.
///
/// Each method corresponds to one [`serf_proto::event::MemberEventKind`].
/// Default impls are no-ops; override what the application cares about.
///
/// `!Send`-tolerant: the driver loop owns the delegate and fires these hooks on
/// the compio thread.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
#[allow(async_fn_in_trait)]
pub trait MemberDelegate: 'static {
  /// Node identifier type.
  type Id;
  /// Address type.
  type Address;

  /// Called when a member joins the cluster.
  async fn notify_join(&self, member: Arc<Member<Self::Id, Self::Address>>) {
    let _ = member; // Unused: default no-op; override to handle.
  }

  /// Called when a member gracefully leaves or is reaped as dead.
  async fn notify_leave(&self, member: Arc<Member<Self::Id, Self::Address>>) {
    let _ = member; // Unused: default no-op; override to handle.
  }

  /// Called when a member is detected as failed (no graceful leave observed).
  async fn notify_failed(&self, member: Arc<Member<Self::Id, Self::Address>>) {
    let _ = member; // Unused: default no-op; override to handle.
  }

  /// Called when a member's tags or metadata are updated.
  async fn notify_update(&self, member: Arc<Member<Self::Id, Self::Address>>) {
    let _ = member; // Unused: default no-op; override to handle.
  }

  /// Called when a member is reaped from the membership store (tombstone
  /// expired).
  async fn notify_reap(&self, member: Arc<Member<Self::Id, Self::Address>>) {
    let _ = member; // Unused: default no-op; override to handle.
  }
}

/// Async observation hook for cluster-wide user-event broadcasts.
///
/// `!Send`-tolerant: the driver loop owns the delegate.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
#[allow(async_fn_in_trait)]
pub trait UserEventDelegate: 'static {
  /// Called when a user-event broadcast is received from the cluster.
  async fn notify_user_event(&self, event: &UserEventMessage) {
    let _ = event; // Unused: default no-op; override to handle.
  }
}

/// Async observation hook for inbound queries.
///
/// The driver calls `notify_query` when it receives `Event::Query`. The
/// application may respond through the `Serf` handle's `respond` method;
/// the delegate itself does not hold the respond path.
///
/// `!Send`-tolerant: the driver loop owns the delegate.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
#[allow(async_fn_in_trait)]
pub trait QueryDelegate: 'static {
  /// Node identifier type.
  type Id;
  /// Address type.
  type Address;

  /// Called when an inbound query arrives that the application may respond to.
  async fn notify_query(&self, event: &QueryEvent<Self::Id, Self::Address>) {
    let _ = event; // Unused: default no-op; override to handle.
  }
}

/// compio's per-driver observation hook surface for serf.
///
/// A type that satisfies `Delegate` implements all three observation sub-traits
/// (`MemberDelegate`, `UserEventDelegate`, `QueryDelegate`) with matching
/// associated types. `!Send`-tolerant: the driver loop owns it and fires all
/// hooks on the compio thread.
///
/// The keyring delegate (`KeyringDelegate`) and join-admission veto
/// (`MergeDelegate`) are NOT part of this composite — they are supplied
/// separately to the driver constructor.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait Delegate:
  MemberDelegate<Id = <Self as Delegate>::Id, Address = <Self as Delegate>::Address>
  + UserEventDelegate
  + QueryDelegate<Id = <Self as Delegate>::Id, Address = <Self as Delegate>::Address>
{
  /// Node identifier type.
  type Id;
  /// Address type — always `SocketAddr` in compio.
  type Address;

  /// Test-only inbound message-drop hook. The driver installs the returned
  /// [`MessageDropper`](serf_proto::MessageDropper) on the machine so a test can
  /// drop selected inbound membership messages; `None` (the default) drops
  /// nothing. Gated behind the `test` feature — no production use.
  #[cfg(feature = "test")]
  #[cfg_attr(docsrs, doc(cfg(feature = "test")))]
  fn message_dropper(&self) -> Option<std::sync::Arc<dyn serf_proto::MessageDropper>> {
    None
  }
}

/// Observer the driver notifies after it rotates the LIVE wire keyring, so an
/// application can persist the new key material.
///
/// The wire keyring lives in the endpoint (the coordinator's `EncryptionOptions`),
/// and the driver is its single source of truth: on an inbound
/// [`Event::KeyRequest`](serf_proto::event::Event::KeyRequest) it read-modify-writes
/// that live keyring directly — install adds a secondary, use promotes the primary,
/// remove drops a secondary, every op variant-exact — and answers the originator
/// from the post-op live state via `respond_key`. This delegate does NOT author
/// those responses; it only OBSERVES a successful rotation, receiving the new live
/// [`Keyring`] so the application can persist it. A `list` and every refused or
/// no-op request do not fire it.
///
/// [`keyring_updated`](Self::keyring_updated) is **synchronous and non-blocking**:
/// it runs on the driver pump. If persistence needs I/O, hand the ring off to a
/// worker and return
/// [`KeyringPersistence::Pending`](serf_driver::KeyringPersistence::Pending);
/// the pump polls the receiver without blocking and defers the rotated op's
/// key response until it resolves, folding a persistence failure into that
/// response (`result = false` carrying the error) exactly as the reference
/// implementation folds a keyring-file write error — with the live wire
/// keyring keeping the rotation either way.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub trait KeyringDelegate: 'static {
  /// Called after a key-management request successfully rotated the live wire
  /// keyring, with the new ring the gossip and reliable planes now encrypt under.
  /// Not called for a `list` or any refused or no-op request. The default needs
  /// no out-of-band persistence and reports
  /// [`KeyringPersistence::Durable`](serf_driver::KeyringPersistence::Durable)
  /// — the rotation is applied to the wire regardless; overriding this only
  /// adds persistence and its acknowledgement.
  fn keyring_updated(&self, keyring: &Keyring) -> serf_driver::KeyringPersistence {
    let _ = keyring; // Unused: default no-op; override to persist the rotation.
    serf_driver::KeyringPersistence::Durable
  }
}

/// The join-merge veto predicate, re-exported from the machine.
///
/// Supplied at construction (the `merge_delegate` argument) and installed into
/// the memberlist machine, which consults it INLINE for every push/pull merge
/// — a join and a periodic anti-entropy refresh alike — before applying the
/// remote member state. Returning `false` cancels that merge: the vetoed peer
/// set is not applied from the exchange.
///
/// This is a PUSH/PULL FILTER, not an admission-control boundary: a rejected
/// peer can still enter membership moments later through gossiped Alive
/// messages, exactly as in the reference implementation. Do not rely on it
/// for durable exclusion or as an ACL — it bounds what a single state
/// exchange can bulk-admit, nothing more. The predicate is synchronous by
/// design: it runs inside the machine's drain, so an application needing
/// async I/O (an ACL service, say) resolves its policy ahead of time and
/// answers from that resolved state here.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use memberlist_proto::delegate::MergeDelegate;

#[cfg(test)]
mod tests;
