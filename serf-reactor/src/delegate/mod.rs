//! `Delegate` composite — the reactor driver's per-driver observation hook
//! surface for serf.
//!
//! Composes four observation sub-traits (`MemberDelegate` / `UserEventDelegate`
//! / `QueryDelegate` / `KeyringDelegate`) and a join-admission veto
//! (`MergeDelegate`, the machine's sync predicate). Every observation hook returns a `Send` future
//! (`-> impl Future<Output = ()> + Send`, not `async fn`) so the observation
//! task can run on a multi-threaded agnostic runtime; the delegate as a whole is
//! `Send + Sync + 'static` and is held behind an `Arc`.
//!
//! `KeyringDelegate` and `MergeDelegate` are separate from the observation
//! `Delegate` composite: `KeyringDelegate` is sync (keyring ops must not
//! block), and `MergeDelegate` is the machine's inline admission veto supplied at
//! construction rather than an observation hook.

#[cfg(encryption)]
mod keyring_file;
mod void;

pub use void::VoidDelegate;

#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use keyring_file::{FileKeyringDelegate, KeyringFileError};
#[cfg(encryption)]
pub use void::VoidKeyringDelegate;

#[cfg(any(feature = "tcp", feature = "quic"))]
use std::{future::Future, sync::Arc};

#[cfg(any(feature = "tcp", feature = "quic"))]
use serf_proto::{event::QueryEvent, members::Member, typed::UserEventMessage};

#[cfg(encryption)]
use memberlist_proto::Keyring;

/// Async observation hooks for serf membership events.
///
/// Each method corresponds to one [`serf_proto::event::MemberEventKind`].
/// Default impls are no-ops; override what the application cares about.
///
/// Every hook returns `-> impl Future<Output = ()> + Send + '_` so the
/// observation task can drive it on a multi-threaded runtime; the delegate is
/// `Send + Sync + 'static`.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait MemberDelegate: Send + Sync + 'static {
  /// Node identifier type.
  type Id;
  /// Address type.
  type Address;

  /// Called when a member joins the cluster.
  fn notify_join(
    &self,
    member: Arc<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = member; // Unused: default no-op; override to handle.
    async {}
  }

  /// Called when a member gracefully leaves or is reaped as dead.
  fn notify_leave(
    &self,
    member: Arc<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = member; // Unused: default no-op; override to handle.
    async {}
  }

  /// Called when a member is detected as failed (no graceful leave observed).
  fn notify_failed(
    &self,
    member: Arc<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = member; // Unused: default no-op; override to handle.
    async {}
  }

  /// Called when a member's tags or metadata are updated.
  fn notify_update(
    &self,
    member: Arc<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = member; // Unused: default no-op; override to handle.
    async {}
  }

  /// Called when a member is reaped from the membership store (tombstone
  /// expired).
  fn notify_reap(
    &self,
    member: Arc<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = member; // Unused: default no-op; override to handle.
    async {}
  }
}

/// Async observation hook for cluster-wide user-event broadcasts.
///
/// Returns a `Send` future so the observation task can drive it on a
/// multi-threaded runtime.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait UserEventDelegate: Send + Sync + 'static {
  /// Called when a user-event broadcast is received from the cluster.
  fn notify_user_event(&self, event: &UserEventMessage) -> impl Future<Output = ()> + Send + '_ {
    let _ = event; // Unused: default no-op; override to handle.
    async {}
  }
}

/// Async observation hook for inbound queries.
///
/// The driver calls `notify_query` when it receives `Event::Query`. The
/// application may respond through the `Serf` handle's `respond` method;
/// the delegate itself does not hold the respond path.
///
/// Returns a `Send` future so the observation task can drive it on a
/// multi-threaded runtime.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait QueryDelegate: Send + Sync + 'static {
  /// Node identifier type.
  type Id;
  /// Address type.
  type Address;

  /// Called when an inbound query arrives that the application may respond to.
  fn notify_query(
    &self,
    event: &QueryEvent<Self::Id, Self::Address>,
  ) -> impl Future<Output = ()> + Send + '_ {
    let _ = event; // Unused: default no-op; override to handle.
    async {}
  }
}

/// The reactor driver's per-driver observation hook surface for serf.
///
/// A type that satisfies `Delegate` implements all three observation sub-traits
/// (`MemberDelegate`, `UserEventDelegate`, `QueryDelegate`) with matching
/// associated types. `Send + Sync + 'static` (inherited from the sub-traits):
/// the driver holds it behind an `Arc` and the observation task fires the hooks
/// on the runtime's worker threads.
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
  /// Address type — always `SocketAddr` in the reactor driver.
  type Address;
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
/// those responses, but it does GATE them: the driver defers a rotated op's
/// response until the returned [`KeyringPersistence`] resolves, and folds a
/// persistence failure into that response (`result = false` carrying the error)
/// exactly as the reference implementation folds a keyring-file write error —
/// with the live wire keyring keeping the rotation either way. A `list` and
/// every refused or no-op request do not fire it.
///
/// [`keyring_updated`](Self::keyring_updated) is **synchronous and non-blocking**:
/// it runs on the driver pump. If persistence needs I/O, hand the ring off to a
/// worker and return [`KeyringPersistence::Pending`]; the pump polls the receiver
/// without blocking. `Send + Sync + 'static` because the driver holds it behind
/// an `Arc` shared across worker threads.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub trait KeyringDelegate: Send + Sync + 'static {
  /// Called after a key-management request successfully rotated the live wire
  /// keyring, with the new ring the gossip and reliable planes now encrypt under.
  /// Not called for a `list` or any refused or no-op request. The default needs
  /// no out-of-band persistence and reports [`KeyringPersistence::Durable`] —
  /// the rotation is applied to the wire regardless; overriding this only adds
  /// persistence and its acknowledgement.
  fn keyring_updated(&self, keyring: &Keyring) -> KeyringPersistence {
    let _ = keyring; // Unused: default no-op; override to persist the rotation.
    KeyringPersistence::Durable
  }
}

/// A persistence failure reported through [`KeyringPersistence::Pending`].
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub type KeyringPersistError = Box<dyn core::error::Error + Send + Sync>;

/// Receiver half of one rotation's persistence acknowledgement.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub type KeyringPersistRx = std::sync::mpsc::Receiver<Result<(), KeyringPersistError>>;

/// How one keyring rotation reaches durability, reported back from
/// [`KeyringDelegate::keyring_updated`].
///
/// The reference implementation writes its keyring file synchronously inside
/// the key-management query handler and folds a write failure into the
/// response. These drivers keep the pump non-blocking instead: a persisting
/// delegate hands back a receiver, the pump parks the key response, and sends
/// it once the receiver resolves — unchanged on success, downgraded to a
/// failed response carrying the error otherwise (a disconnected sender counts
/// as a failure: the worker vanished without acknowledging). The live wire
/// keyring keeps the rotation in every outcome.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
#[must_use = "dropping the acknowledgement silently un-gates the key response from persistence"]
pub enum KeyringPersistence {
  /// The rotation needs no out-of-band persistence (or completed inline):
  /// the key response is sent immediately.
  Durable,
  /// Persistence runs out-of-band; the pump defers the key response until
  /// the receiver resolves.
  Pending(KeyringPersistRx),
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
