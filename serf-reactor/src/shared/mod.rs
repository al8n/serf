//! [`Shared`] — the uniform handle-to-driver shared state: a command queue + the
//! driver's parked waker, the published membership snapshot, the drop counters,
//! the teardown latch, and the live-handle count. The serf `StreamEndpoint` is
//! owned privately by the driver; handles reach it only by pushing [`Command`]s
//! and reading the snapshot.
//!
//! This is the `Send`/`Arc`/atomic sibling of serf-compio's `!Send` `Rc<Shared>`
//! plus `Rc<Cell<..>>` state, mirroring memberlist-reactor's `shared` module. The
//! command queue is a `Mutex<VecDeque<Command>>` woken by a stored [`Waker`] (NOT
//! a channel); the snapshot is an [`ArcSwap`] read lock-free; the counters and
//! latches are atomics.

use std::{
  collections::VecDeque,
  net::SocketAddr,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
  },
  task::Waker,
};

use arc_swap::ArcSwap;
use flume::{Receiver, Sender};

use crate::{command::Command, drop_counter::DropReader};
use serf_driver::SerfSnapshot;

/// The lock-guarded part of [`Shared`]: the command queue handles push onto, the
/// driver's parked waker, and the closed latch.
struct Inner<I> {
  commands: VecDeque<Command<I, SocketAddr>>,
  driver_waker: Option<Waker>,
  /// Set once the driver has exited; no further commands are accepted, so a
  /// handle never waits on a reply that can never come.
  closed: bool,
}

/// State shared between the `Serf` handle (and its clones) and the backend driver
/// task.
///
/// Generic over the wire id type `I`; the membership address is always
/// [`SocketAddr`], so the snapshot and command channel are pinned to it.
pub(crate) struct Shared<I> {
  inner: Mutex<Inner<I>>,
  snapshot: ArcSwap<SerfSnapshot<I, SocketAddr>>,
  /// Recoverable EventStream-forward drops (a slow subscriber).
  events_dropped: AtomicU64,
  /// Observation-channel drops (a slow delegate; may lose application data).
  observation_dropped: AtomicU64,
  /// Read-only view of the endpoint's cumulative user-coalescer drop count. The
  /// driver owns the endpoint, so a handle reads the shed count here — over the
  /// SAME atomic the endpoint's writer increments, so no publish step exists.
  coalesced_user_events_dropped: DropReader,
  /// Read-only view of the endpoint's cumulative member-coalescer drop count.
  coalesced_member_events_dropped: DropReader,
  /// Cumulative gossip payloads that rode the QUIC datagram plane (a
  /// `DatagramSendStatus::Queued`), as opposed to the plain-UDP fallback. Zero on
  /// the stream transports and on a QUIC endpoint in `UnreliableTransport::Udp`
  /// mode.
  datagrams_sent: AtomicU64,
  /// Set once the driver is shutting down; handle command methods observe it.
  shutdown: AtomicBool,
  /// Count of live `Serf` handles. The last to drop flips `shutdown` and wakes
  /// the driver so it exits.
  handles: AtomicUsize,
  /// The sole sender of the teardown-completion latch, held until the driver has
  /// freed its bind sockets (the UDP gossip socket and the TCP listener). The
  /// latch fires once those are released — NOT once every connected stream FD has
  /// closed. Dropping it disconnects `shutdown_complete_rx`; a late `shutdown()`
  /// caller (whose command the closed queue rejected) awaits that disconnect so
  /// it never returns into a still-bound port.
  shutdown_complete_tx: Mutex<Option<Sender<()>>>,
  /// The receiving end of the teardown-completion latch.
  shutdown_complete_rx: Receiver<()>,
}

impl<I> Shared<I> {
  /// Builds the shared state around an initial published snapshot, with one live
  /// handle.
  ///
  /// `coalesced_user_events_dropped` / `coalesced_member_events_dropped` are the
  /// read-only halves of the shed counters; the driver holds the write halves
  /// inside the endpoint, over the same backing atomics.
  pub(crate) fn new(
    initial: SerfSnapshot<I, SocketAddr>,
    coalesced_user_events_dropped: DropReader,
    coalesced_member_events_dropped: DropReader,
  ) -> Self {
    let (shutdown_complete_tx, shutdown_complete_rx) = flume::bounded(0);
    Self {
      inner: Mutex::new(Inner::<I> {
        commands: VecDeque::new(),
        driver_waker: None,
        closed: false,
      }),
      snapshot: ArcSwap::from_pointee(initial),
      events_dropped: AtomicU64::new(0),
      observation_dropped: AtomicU64::new(0),
      coalesced_user_events_dropped,
      coalesced_member_events_dropped,
      datagrams_sent: AtomicU64::new(0),
      shutdown: AtomicBool::new(false),
      handles: AtomicUsize::new(1),
      shutdown_complete_tx: Mutex::new(Some(shutdown_complete_tx)),
      shutdown_complete_rx,
    }
  }

  /// Pushes a command and wakes the driver. Returns `false` (dropping `cmd`) if
  /// the driver has already exited, so the caller can fail fast instead of
  /// awaiting a reply that will never arrive.
  pub(crate) fn push_command(&self, cmd: Command<I, SocketAddr>) -> bool {
    let mut inner = self.inner.lock().unwrap();
    if inner.closed {
      return false;
    }
    inner.commands.push_back(cmd);
    if let Some(waker) = inner.driver_waker.take() {
      waker.wake();
    }
    true
  }

  /// Driver side: parks `waker` for the next push and returns all queued
  /// commands.
  pub(crate) fn drain_commands(&self, waker: &Waker) -> VecDeque<Command<I, SocketAddr>> {
    let mut inner = self.inner.lock().unwrap();
    match &inner.driver_waker {
      Some(w) if w.will_wake(waker) => {}
      _ => inner.driver_waker = Some(waker.clone()),
    }
    core::mem::take(&mut inner.commands)
  }

  /// Driver side, on exit: closes the queue (rejecting further pushes) and
  /// returns any still-queued commands so the driver can fail their repliers.
  pub(crate) fn close_and_drain(&self) -> VecDeque<Command<I, SocketAddr>> {
    let mut inner = self.inner.lock().unwrap();
    inner.closed = true;
    core::mem::take(&mut inner.commands)
  }

  /// Publishes a fresh membership snapshot for handles to read lock-free.
  pub(crate) fn publish(&self, snap: SerfSnapshot<I, SocketAddr>) {
    self.snapshot.store(Arc::new(snap));
  }

  /// Loads the latest published snapshot.
  pub(crate) fn load_snapshot(&self) -> Arc<SerfSnapshot<I, SocketAddr>> {
    self.snapshot.load_full()
  }

  /// Records `n` recoverable EventStream-forward drops.
  pub(crate) fn add_events_dropped(&self, n: u64) {
    self.events_dropped.fetch_add(n, Ordering::Relaxed);
  }

  /// Records `n` observation-channel drops.
  pub(crate) fn add_observation_dropped(&self, n: u64) {
    self.observation_dropped.fetch_add(n, Ordering::Relaxed);
  }

  /// Records `n` gossip payloads sent over the QUIC datagram plane. Only the QUIC
  /// driver reports datagram sends; the stream backends leave the counter at zero.
  #[cfg(feature = "quic")]
  pub(crate) fn add_datagrams_sent(&self, n: u64) {
    self.datagrams_sent.fetch_add(n, Ordering::Relaxed);
  }

  /// The cumulative recoverable EventStream-forward drop count.
  pub(crate) fn events_dropped(&self) -> u64 {
    self.events_dropped.load(Ordering::Relaxed)
  }

  /// The cumulative observation-channel drop count.
  pub(crate) fn observation_dropped(&self) -> u64 {
    self.observation_dropped.load(Ordering::Relaxed)
  }

  /// The endpoint's live cumulative user-coalescer drop count.
  pub(crate) fn coalesced_user_events_dropped(&self) -> u64 {
    self.coalesced_user_events_dropped.get()
  }

  /// The endpoint's live cumulative member-coalescer drop count.
  pub(crate) fn coalesced_member_events_dropped(&self) -> u64 {
    self.coalesced_member_events_dropped.get()
  }

  /// The cumulative count of gossip payloads sent over the QUIC datagram plane.
  pub(crate) fn datagrams_sent(&self) -> u64 {
    self.datagrams_sent.load(Ordering::Relaxed)
  }

  /// Whether the driver is shutting down.
  pub(crate) fn is_shutdown(&self) -> bool {
    self.shutdown.load(Ordering::Acquire)
  }

  /// Marks the driver as shutting down.
  pub(crate) fn begin_shutdown(&self) {
    self.shutdown.store(true, Ordering::Release);
  }

  /// Registers a freshly cloned handle.
  pub(crate) fn handle_cloned(&self) {
    self.handles.fetch_add(1, Ordering::Relaxed);
  }

  /// Deregisters a dropped handle; returns `true` if it was the last one (the
  /// caller should then begin shutdown and wake the driver).
  pub(crate) fn handle_dropped(&self) -> bool {
    self.handles.fetch_sub(1, Ordering::AcqRel) == 1
  }

  /// Wakes the driver without enqueuing a command (used on last-handle drop and
  /// by the accept / dial / bridge aux tasks after they enqueue for the pump).
  pub(crate) fn wake_driver(&self) {
    if let Some(waker) = self.inner.lock().unwrap().driver_waker.take() {
      waker.wake();
    }
  }

  /// Driver side, at the very end of teardown — once the bind sockets are free
  /// (the driver has dropped its UDP gossip socket and released the accept task's
  /// TCP listener): fire the completion latch so any late `shutdown()` caller
  /// parked in [`wait_shutdown_complete`](Self::wait_shutdown_complete) returns.
  /// Dropping the sole sender disconnects the receiver; idempotent across
  /// re-entrant polls.
  pub(crate) fn mark_shutdown_complete(&self) {
    // Ignoring the take result: dropping the sender fires the latch; a second
    // teardown poll finds `None` and is a no-op.
    let _ = self.shutdown_complete_tx.lock().unwrap().take();
  }

  /// Handle side: await teardown completion. Returns as soon as the driver has
  /// fired the latch via [`mark_shutdown_complete`](Self::mark_shutdown_complete),
  /// or immediately if it already has — i.e. once the bind address is free, not
  /// once every connected stream FD has closed. A `shutdown()` caller whose
  /// command the closed queue rejected waits here for the ports to free rather
  /// than returning into a still-bound address.
  pub(crate) async fn wait_shutdown_complete(&self) {
    // Ignoring the recv result: the latch fires by sender-disconnect, never by a
    // sent value, so recv resolves to Err exactly once teardown completes.
    let _ = self.shutdown_complete_rx.recv_async().await;
  }
}
