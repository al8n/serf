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
  /// Test-only inbound message-drop hook returned via
  /// [`Delegate::message_dropper`]; `None` in every real build.
  #[cfg(feature = "test")]
  message_dropper: Option<std::sync::Arc<dyn serf_proto::MessageDropper>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> VoidDelegate<I, A> {
  /// Construct a `VoidDelegate`.
  #[inline]
  pub const fn new() -> Self {
    Self {
      _phantom: PhantomData,
      #[cfg(feature = "test")]
      message_dropper: None,
    }
  }

  /// Attach a test-only [`MessageDropper`](serf_proto::MessageDropper) surfaced
  /// through [`Delegate::message_dropper`], so a test node drops selected
  /// inbound membership messages. Test fault injection only.
  #[cfg(feature = "test")]
  #[cfg_attr(docsrs, doc(cfg(feature = "test")))]
  #[must_use]
  pub fn with_message_dropper(
    mut self,
    dropper: std::sync::Arc<dyn serf_proto::MessageDropper>,
  ) -> Self {
    self.message_dropper = Some(dropper);
    self
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

  #[cfg(feature = "test")]
  fn message_dropper(&self) -> Option<std::sync::Arc<dyn serf_proto::MessageDropper>> {
    self.message_dropper.clone()
  }
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
