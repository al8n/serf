//! `Delegate` composite — the reactor driver's per-driver observation hook
//! surface for serf.
//!
//! Composes four observation sub-traits (`MemberDelegate` / `UserEventDelegate`
//! / `QueryDelegate` / `KeyringDelegate`) and a join-admission veto
//! (`MergeDelegate`). Every observation hook returns a `Send` future
//! (`-> impl Future<Output = ()> + Send`, not `async fn`) so the observation
//! task can run on a multi-threaded agnostic runtime; the delegate as a whole is
//! `Send + Sync + 'static` and is held behind an `Arc`.
//!
//! `KeyringDelegate` and `MergeDelegate` are separate from the observation
//! `Delegate` composite: `KeyringDelegate` is sync (keyring ops must not
//! block), and `MergeDelegate` is an async admission veto supplied at
//! construction rather than an observation hook.

#[cfg(encryption)]
mod keyring_file;
mod void;

pub use void::{NoopMergeDelegate, VoidDelegate};

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
/// those responses; it only OBSERVES a successful rotation, receiving the new live
/// [`Keyring`] so the application can persist it. A `list` and every refused or
/// no-op request do not fire it.
///
/// [`keyring_updated`](Self::keyring_updated) is **synchronous and non-blocking**:
/// it runs on the driver pump. If persistence needs async I/O, hand the ring off to
/// a channel the observer owns and drain it elsewhere. `Send + Sync + 'static`
/// because the driver holds it behind an `Arc` shared across worker threads.
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
  /// Not called for a `list` or any refused or no-op request. The default is a
  /// no-op — the rotation is applied to the wire regardless; overriding this only
  /// adds out-of-band persistence.
  fn keyring_updated(&self, keyring: &Keyring) {
    let _ = keyring; // Unused: default no-op; override to persist the rotation.
  }
}

/// Async veto hook invoked by the driver on the join path before accepting
/// remote member state from a push-pull exchange.
///
/// `Ok(())` permits the merge; `Err(Self::Error)` cancels it. The driver wraps
/// the concrete error into [`SerfError`](crate::SerfError) before forwarding it
/// to the join caller.
///
/// The hook is **async and driver-side** deliberately: the application may need
/// to consult an ACL service or other async resource before deciding whether to
/// accept a batch of remote peers. A synchronous (Sans-I/O) filter would
/// preclude that. `notify_merge` returns a `Send` future so the driver can drive
/// it on a multi-threaded runtime; the delegate is `Send + Sync + 'static`.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait MergeDelegate<I, A>: Send + Sync + 'static {
  /// The veto/error type this delegate reports when a merge is cancelled.
  type Error;

  /// Called before the driver accepts inbound push-pull peer state.
  ///
  /// `peers` is the slice of remote [`Member`]s the cluster is about to merge.
  /// Return `Ok(())` to proceed, or `Err(e)` to cancel the merge.
  ///
  /// The default implementation always permits the merge.
  fn notify_merge(
    &self,
    peers: &[Arc<Member<I, A>>],
  ) -> impl Future<Output = Result<(), Self::Error>> + Send + '_ {
    let _ = peers; // Unused in the default permit-all impl; an overriding delegate inspects it.
    async { Ok(()) }
  }
}

#[cfg(test)]
mod tests;
