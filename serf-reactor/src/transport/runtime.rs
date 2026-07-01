//! [`TransportRuntime<I, D>`] — the bundle handed to `Transport::run(self,
//! runtime, gossip_rng)`.
//!
//! Carries the serf observation delegate, the handle-to-driver [`Shared`] state
//! (the reactor's command queue lives there, NOT in a channel), the events
//! sender, and the driver / serf tuning knobs. The concrete machine endpoint is
//! NOT carried here: a generic `Serf::new` cannot build the backend's
//! record-layer config + dial closures, so each `T::run` body builds its own
//! endpoint from the transport's stored config and then drives the shared stream
//! driver.

use std::{net::SocketAddr, sync::Arc};

use flume::Sender;
use serf_proto::{event::Event, options::Options as SerfOptions};

use crate::{driver::options::RuntimeOptions, shared::Shared};

#[cfg(encryption)]
use crate::delegate::KeyringDelegate;

/// Bundle handed to `Transport::run(self, runtime, gossip_rng)`.
///
/// Generic over the wire id `I` and the observation delegate `D`. The membership
/// address is always [`SocketAddr`], so the events channel and [`Shared`] state
/// are pinned to it. The machine endpoint is built inside `T::run` (it needs the
/// backend's private record-layer config), so it is deliberately absent here.
///
/// Requires a stream or QUIC transport feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct TransportRuntime<I, D> {
  pub(crate) delegate: D,
  pub(crate) shared: Arc<Shared<I>>,
  pub(crate) events_tx: Sender<Event<I, SocketAddr>>,
  pub(crate) driver_options: RuntimeOptions,
  pub(crate) serf_options: SerfOptions,
  /// The driver's keyring delegate, applied to inbound key-management requests.
  /// Present only under an encryption backend.
  #[cfg(encryption)]
  pub(crate) keyring: Arc<dyn KeyringDelegate>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, D> TransportRuntime<I, D> {
  /// Construct the runtime bundle. Called by the `Serf` handle constructor.
  #[allow(clippy::too_many_arguments)]
  #[inline]
  pub(crate) fn new(
    delegate: D,
    shared: Arc<Shared<I>>,
    events_tx: Sender<Event<I, SocketAddr>>,
    driver_options: RuntimeOptions,
    serf_options: SerfOptions,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Self {
    Self {
      delegate,
      shared,
      events_tx,
      driver_options,
      serf_options,
      #[cfg(encryption)]
      keyring,
    }
  }
}
