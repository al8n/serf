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
//! block), and `MergeDelegate` is an async admission veto supplied at
//! construction rather than an observation hook.

mod void;

pub use void::{NoopMergeDelegate, VoidDelegate};

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
/// a channel the observer owns and drain it elsewhere.
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
/// preclude that.
///
/// `!Send`-agnostic: neither the trait object nor the future returned by
/// `notify_merge` carry a `Send` bound, so compio's `!Send` driver can
/// implement it without wrapping.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
#[allow(async_fn_in_trait)]
pub trait MergeDelegate<I, A>: 'static {
  /// The veto/error type this delegate reports when a merge is cancelled.
  type Error;

  /// Called before the driver accepts inbound push-pull peer state.
  ///
  /// `peers` is the slice of remote [`Member`]s the cluster is about to merge.
  /// Return `Ok(())` to proceed, or `Err(e)` to cancel the merge.
  ///
  /// The default implementation always permits the merge.
  async fn notify_merge(&self, peers: &[Arc<Member<I, A>>]) -> Result<(), Self::Error> {
    let _ = peers; // Unused in the default permit-all impl; an overriding delegate inspects it.
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use smol_str::SmolStr;
  use std::net::SocketAddr;

  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[test]
  fn void_delegate_satisfies_observation_composite() {
    fn assert_delegate<D>(_d: &D)
    where
      D: Delegate<Id = SmolStr, Address = SocketAddr>,
    {
    }
    let v: VoidDelegate<SmolStr, SocketAddr> = VoidDelegate::default();
    assert_delegate(&v);
  }

  /// Verify `NoopMergeDelegate` satisfies `MergeDelegate` with `Error =
  /// Infallible` — a type-level check; no I/O needed.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[test]
  fn noop_merge_delegate_satisfies_trait() {
    fn assert_merge<T: MergeDelegate<SmolStr, SocketAddr, Error = core::convert::Infallible>>(
      _: &T,
    ) {
    }
    assert_merge(&NoopMergeDelegate);
  }
}
