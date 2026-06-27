//! `TransportRuntime<T, D>` — the bundle handed to `T::run(self, runtime)`.
//!
//! Carries the serf observation delegate, command receiver, events sender,
//! snapshot cell, and driver / serf tuning knobs. The concrete machine
//! endpoint is NOT carried here: a generic `Serf::new` cannot build the
//! backend's record-layer config + dial closures, so each `T::run` body
//! builds its own endpoint from the transport's stored config and then
//! drives the shared stream or QUIC driver loop.
use std::{cell::Cell, net::SocketAddr, rc::Rc};

use flume::{Receiver, Sender};

use serf_proto::{event::Event, options::Options as SerfOptions};

use crate::{
  command::Command, delegate::Delegate, driver::options::RuntimeOptions, snapshot::SnapshotCell,
  transport::Transport,
};

#[cfg(encryption)]
use crate::delegate::KeyringDelegate;

/// Type alias for the command channel receiver, parameterised over the
/// transport so `Command::Respond` can carry `Node<I, SocketAddr>` without an
/// extra generic on `TransportRuntime`.
type CommandReceiver<T> = Receiver<Command<<T as Transport>::Id, SocketAddr>>;

/// Bundle handed to `Transport::run(self, runtime)`.
///
/// Carries the serf observation delegate, command receiver, events sender,
/// snapshot cell, and driver / serf tuning knobs. The machine endpoint is
/// built inside `T::run` (it needs the backend's private record-layer config),
/// so it is deliberately absent from this bundle.
///
/// Requires a stream or QUIC transport feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct TransportRuntime<T, D>
where
  T: Transport,
  D: Delegate<Id = T::Id, Address = SocketAddr>,
{
  pub(crate) delegate: D,
  pub(crate) commands_rx: CommandReceiver<T>,
  pub(crate) events_tx: Sender<Event<T::Id, SocketAddr>>,
  /// Counter for events dropped at the `EventStream` fan-out when the
  /// subscriber queue is full (slow consumer) — recoverable membership gaps.
  pub(crate) events_dropped: Rc<Cell<u64>>,
  /// Counter for events dropped at the delegate observation channel when the
  /// delegate fell behind — may include unrecoverable app-data.
  pub(crate) observation_dropped: Rc<Cell<u64>>,
  pub(crate) snapshot: SnapshotCell<T::Id>,
  pub(crate) shutdown_flag: Rc<Cell<bool>>,
  pub(crate) driver_options: RuntimeOptions,
  pub(crate) serf_options: SerfOptions,
  /// The driver's keyring delegate, applied to inbound key-management requests.
  /// Present only under an encryption backend.
  #[cfg(encryption)]
  pub(crate) keyring: Rc<dyn KeyringDelegate>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<T, D> TransportRuntime<T, D>
where
  T: Transport,
  D: Delegate<Id = T::Id, Address = SocketAddr>,
{
  /// Construct the runtime bundle. Called by the `Serf` handle constructor.
  #[allow(clippy::too_many_arguments)]
  #[inline]
  pub(crate) fn new(
    delegate: D,
    commands_rx: CommandReceiver<T>,
    events_tx: Sender<Event<T::Id, SocketAddr>>,
    events_dropped: Rc<Cell<u64>>,
    observation_dropped: Rc<Cell<u64>>,
    snapshot: SnapshotCell<T::Id>,
    shutdown_flag: Rc<Cell<bool>>,
    driver_options: RuntimeOptions,
    serf_options: SerfOptions,
    #[cfg(encryption)] keyring: Rc<dyn KeyringDelegate>,
  ) -> Self {
    Self {
      delegate,
      commands_rx,
      events_tx,
      events_dropped,
      observation_dropped,
      snapshot,
      shutdown_flag,
      driver_options,
      serf_options,
      #[cfg(encryption)]
      keyring,
    }
  }
}
