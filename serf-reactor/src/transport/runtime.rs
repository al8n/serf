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

use crate::{driver::options::RuntimeOptions, drop_counter::ReactorDropCounter, shared::Shared};

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
  /// The write half of the user-coalescer shed counter, injected into the
  /// endpoint by `T::run` so its increments land in the atomic the handle reads.
  pub(crate) user_drop: ReactorDropCounter,
  /// The write half of the member-coalescer shed counter.
  pub(crate) member_drop: ReactorDropCounter,
  /// Optional per-member reconnect-timeout override (Go serf `ReconnectDelegate`),
  /// installed into the endpoint by `T::run` before the endpoint moves into the
  /// detached pump. `None` keeps the flat configured reap timeouts.
  pub(crate) reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
  /// Optional join-merge veto predicate, installed into the machine by `T::run`
  /// before the endpoint moves into the detached pump. The machine consults it
  /// inline for EVERY push/pull merge; `None` admits every peer set.
  pub(crate) merge_delegate:
    Option<Box<dyn memberlist_proto::delegate::MergeDelegate<I, SocketAddr>>>,
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
    user_drop: ReactorDropCounter,
    member_drop: ReactorDropCounter,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    merge_delegate: Option<Box<dyn memberlist_proto::delegate::MergeDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Self {
    Self {
      delegate,
      shared,
      events_tx,
      driver_options,
      serf_options,
      user_drop,
      member_drop,
      reconnect_delegate,
      merge_delegate,
      #[cfg(encryption)]
      keyring,
    }
  }
}
