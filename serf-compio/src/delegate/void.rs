//! Zero-cost default delegates — no-op impls of all observation and admission
//! hooks so drivers that do not need them can construct a node without
//! boilerplate.

use core::marker::PhantomData;

#[cfg(any(feature = "tcp", feature = "quic"))]
use super::{Delegate, MemberDelegate, QueryDelegate, UserEventDelegate};

#[cfg(encryption)]
use super::KeyringDelegate;

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
  I: 'static,
  A: 'static,
{
  type Id = I;
  type Address = A;
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> UserEventDelegate for VoidDelegate<I, A>
where
  I: 'static,
  A: 'static,
{
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> QueryDelegate for VoidDelegate<I, A>
where
  I: 'static,
  A: 'static,
{
  type Id = I;
  type Address = A;
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> Delegate for VoidDelegate<I, A>
where
  I: 'static,
  A: 'static,
{
  type Id = I;
  type Address = A;
}

/// A keyring delegate that persists nothing.
///
/// The default for nodes that do not need to observe key rotations. The driver
/// still applies every inbound key-management op to the live wire keyring and
/// answers from that live state; this delegate simply does not persist the
/// result. A node that wants to persist rotated key material supplies its own
/// [`KeyringDelegate`](super::KeyringDelegate), overriding
/// [`keyring_updated`](super::KeyringDelegate::keyring_updated).
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub struct VoidKeyringDelegate;

#[cfg(encryption)]
impl KeyringDelegate for VoidKeyringDelegate {}
