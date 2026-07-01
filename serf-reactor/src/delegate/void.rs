//! Zero-cost default delegates — no-op impls of all observation and admission
//! hooks so drivers that do not need them can construct a node without
//! boilerplate.

use core::marker::PhantomData;

#[cfg(any(feature = "tcp", feature = "quic"))]
use super::{Delegate, MemberDelegate, MergeDelegate, QueryDelegate, UserEventDelegate};

#[cfg(encryption)]
use super::KeyringDelegate;
#[cfg(encryption)]
use serf_proto::KeyResponseArgs;

/// Zero-cost default observation delegate. Every hook is a no-op.
///
/// Use when the application does not need to observe membership, user-event,
/// or query notifications. `VoidDelegate<I, A>` satisfies the
/// [`Delegate`](super::Delegate) composite.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct VoidDelegate<I, A> {
  _phantom: PhantomData<fn(I, A)>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> VoidDelegate<I, A> {
  /// Construct a `VoidDelegate`.
  #[inline]
  pub const fn new() -> Self {
    Self {
      _phantom: PhantomData,
    }
  }
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> Default for VoidDelegate<I, A> {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> MemberDelegate for VoidDelegate<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  type Id = I;
  type Address = A;
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> UserEventDelegate for VoidDelegate<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> QueryDelegate for VoidDelegate<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  type Id = I;
  type Address = A;
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> Delegate for VoidDelegate<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  type Id = I;
  type Address = A;
}

/// A merge delegate that always permits merges.
///
/// The default delegate for drivers that do not need join admission control.
/// Its associated error type is [`core::convert::Infallible`], reflecting that
/// `notify_merge` can never fail.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct NoopMergeDelegate;

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> MergeDelegate<I, A> for NoopMergeDelegate
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  type Error = core::convert::Infallible;
}

/// A keyring delegate that manages no key material.
///
/// The default for nodes that do not participate in key management: every
/// operation reports `result = false` with an explanatory message and an empty
/// key list, so a cluster-wide key query records this node as not key-managed
/// rather than silently timing out. A node that DOES manage encryption keys
/// supplies its own [`KeyringDelegate`](super::KeyringDelegate) bound to the
/// keyring it shares with the transport's encryption configuration.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub struct VoidKeyringDelegate;

#[cfg(encryption)]
impl VoidKeyringDelegate {
  /// The response returned by every operation: not key-managed.
  fn unmanaged() -> KeyResponseArgs {
    KeyResponseArgs {
      result: false,
      message: "node has no keyring delegate configured".into(),
      keys: Vec::new(),
      primary_key: None,
    }
  }
}

#[cfg(encryption)]
impl KeyringDelegate for VoidKeyringDelegate {
  #[inline]
  fn install(&self, _key: memberlist_proto::SecretKey) -> KeyResponseArgs {
    Self::unmanaged()
  }

  #[inline]
  fn use_key(&self, _key: memberlist_proto::SecretKey) -> KeyResponseArgs {
    Self::unmanaged()
  }

  #[inline]
  fn remove(&self, _key: memberlist_proto::SecretKey) -> KeyResponseArgs {
    Self::unmanaged()
  }

  #[inline]
  fn list(&self) -> KeyResponseArgs {
    Self::unmanaged()
  }
}
