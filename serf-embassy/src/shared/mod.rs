//! State shared between the [`Serf`](crate::Serf) handle and the
//! [`Runner`](crate::Runner) run loop.
//!
//! Single-executor (`!Send`) cooperative sharing: the engine lives behind a
//! [`RefCell`] and the two sides coordinate through `embassy-sync`
//! [`Signal`]s. The handle borrows the engine to enqueue work and parks on a
//! signal; the Runner (the only `pump` caller) drains the machine's events each
//! loop — actioning serf's mandatory driver-actioned events (a lost-conflict
//! [`Event::Shutdown`] stops the node, an [`Event::KeyRequest`] rotates the live
//! wire keyring) BEFORE buffering the observation — and pulses the parked waiters.
//!
//! Because [`SerfEngine::pump`](serf_embedded::SerfEngine::pump) is synchronous,
//! every `RefCell` borrow either side takes completes before the next `.await`,
//! so no borrow ever spans a suspension point.

use core::{
  cell::{Cell, RefCell},
  hash::Hash,
  net::SocketAddr,
};

use alloc::collections::VecDeque;

use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use memberlist_proto::{Instant, Rng, SeedableRng, SmallRng};
use serf_embedded::{DEFAULT_EVENT_BUFFER_CAP, Event, SerfEngine};

use crate::stream_io::SlotId;

/// The state both the handle and the run loop reach.
///
/// Shared via [`Rc`](alloc::rc::Rc) (single-core cooperative). `I` is the node id
/// type; `G` is the memberlist gossip RNG and `SR` is serf's own core RNG (both
/// defaulting to [`SmallRng`]).
pub(crate) struct Shared<I, G = SmallRng, SR = SmallRng>
where
  // Mandated by `SerfEngine`'s membership store keyed by `I`. Every impl bounds
  // `I: Id`, which implies these; the field type needs them here.
  I: Eq + Hash,
{
  /// The transport-agnostic serf driving core (serf's super-machine, the
  /// reliable-plane state + pool, the gossip codec, the join/await-result queues),
  /// behind interior mutability.
  pub(crate) engine: RefCell<SerfEngine<I, SlotId, G, SR>>,
  /// The pump loop's single wake. Producers: the handle (when it enqueues a
  /// command / join / leave) AND every worker (when it advances its mailbox).
  /// Sole consumer: the pump loop. A single-consumer, many-producer [`Signal`] is
  /// sound because the pump drains EVERY mailbox each tick, so one pulse re-pumps
  /// all pending work regardless of which producer fired it.
  pub(crate) pump_wake: Signal<NoopRawMutex, ()>,
  /// Pulsed by the Runner after each drain (the pump folds every push/pull
  /// completion into its await-result join), so a parked [`join`](crate::Serf::join)
  /// re-checks `poll_join`. A parked `join` also races a short timer, so a missed
  /// pulse (this `Signal` wakes only one of several concurrent joiners) costs at
  /// most that interval, never a hang.
  pub(crate) join_wake: Signal<NoopRawMutex, ()>,
  /// Application-facing events the Runner drained from the machine, buffered for
  /// the handle's [`Serf::poll_event`](crate::Serf::poll_event).
  ///
  /// The Runner is the sole `poll_event` caller on the engine (it drains events
  /// each pump to action the mandatory ones); draining is destructive, so every
  /// event — including the mandatory ones the driver has ALREADY acted on — is
  /// re-buffered here for the app. Bounded at [`DEFAULT_EVENT_BUFFER_CAP`] with
  /// drop-oldest so a never-polling app cannot grow it without bound.
  pub(crate) app_events: RefCell<VecDeque<Event<I, SocketAddr>>>,
  /// Count of app events shed from `app_events` because the app never drained
  /// [`poll_event`](crate::Serf::poll_event) fast enough and the backlog hit the
  /// cap. Summed with the engine's own load-shed counter by
  /// [`Serf::events_dropped`](crate::Serf::events_dropped).
  pub(crate) app_events_dropped: Cell<u64>,
  /// Set once the Runner observed a lost id-conflict [`Event::Shutdown`]; the
  /// handle reads it via [`Serf::is_shutdown`](crate::Serf::is_shutdown).
  pub(crate) shutdown: Cell<bool>,
  /// The local node's resolved advertise address, captured at construction (the
  /// engine does not surface it). Read by the handle's `advertise_address` and by
  /// the runner's gossip view (the self-delivery loopback destination).
  pub(crate) advertise: SocketAddr,
}

impl<I, G, SR> Shared<I, G, SR>
where
  I: memberlist_proto::Id,
{
  /// Wrap a constructed engine as shared state with empty signals/buffers.
  pub(crate) fn new(engine: SerfEngine<I, SlotId, G, SR>, advertise: SocketAddr) -> Self {
    Self {
      engine: RefCell::new(engine),
      pump_wake: Signal::new(),
      join_wake: Signal::new(),
      app_events: RefCell::new(VecDeque::new()),
      app_events_dropped: Cell::new(0),
      shutdown: Cell::new(false),
      advertise,
    }
  }

  /// Pop one buffered application event for the handle's `poll_event`.
  #[inline]
  pub(crate) fn pop_app_event(&self) -> Option<Event<I, SocketAddr>> {
    self.app_events.borrow_mut().pop_front()
  }

  /// Wake the pump loop (a handle op enqueued work).
  #[inline]
  pub(crate) fn wake_pump(&self) {
    self.pump_wake.signal(());
  }

  /// Whether the Runner has observed a lost id-conflict [`Event::Shutdown`].
  #[inline]
  pub(crate) fn is_shutdown(&self) -> bool {
    self.shutdown.get()
  }

  /// Poison the shared state on a lost id-conflict [`Event::Shutdown`]: latch the
  /// one-way shutdown flag, then wake the parked joins (so a join in flight
  /// resolves with the shutdown error rather than hang) and the pump loop (so it
  /// observes the latch after its final drain and stops, collapsing the workers so
  /// every socket winds down). Idempotent — a repeated shutdown re-signals but the
  /// latch never clears.
  #[inline]
  pub(crate) fn begin_shutdown(&self) {
    self.shutdown.set(true);
    self.join_wake.signal(());
    self.pump_wake.signal(());
  }

  /// Buffer one event for [`poll_event`](crate::Serf::poll_event), bounding the
  /// backlog at [`DEFAULT_EVENT_BUFFER_CAP`] with drop-oldest so a never-draining
  /// app cannot grow memory without bound.
  fn push_app_event(&self, ev: Event<I, SocketAddr>) {
    let mut q = self.app_events.borrow_mut();
    if q.len() >= DEFAULT_EVENT_BUFFER_CAP {
      q.pop_front();
      self
        .app_events_dropped
        .set(self.app_events_dropped.get() + 1);
    }
    q.push_back(ev);
  }
}

impl<I, G, SR> Shared<I, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Drain the engine's event queue (mandatory-first), routing each event through
  /// [`route_drained_event`](Self::route_drained_event) — the driver-owned side
  /// effect, then the app buffering — and pulse `join_wake` so parked joins
  /// re-check `poll_join`.
  ///
  /// Called by the Runner once per pump, AFTER `pump` (so it sees this tick's
  /// freshly-emitted events). Returns whether the drain queued outbound gossip work
  /// the current pump's egress did not see — a key response `handle_key_request`
  /// just queued — so the Runner knows to re-pump and egress it (and loop back a
  /// self-addressed one) within the same tick.
  pub(crate) fn drain_events(&self, now: Instant) -> bool {
    let mut queued = false;
    loop {
      let ev = self.engine.borrow_mut().poll_event();
      let Some(ev) = ev else { break };
      queued |= self.route_drained_event(ev, now);
    }

    // Every drain re-checks parked joins: the pump folds each completion into its
    // await-result join, so a resolved outcome is now visible to `poll_join`.
    self.join_wake.signal(());
    queued
  }

  /// Route one drained machine event: take serf's mandatory driver-owned side
  /// effect on it, then buffer it for the app's
  /// [`poll_event`](crate::Serf::poll_event). Returns whether the side effect
  /// queued outbound gossip work (a key response) the current pump's egress did not
  /// see.
  ///
  /// The mandatory ACTION runs BEFORE the lossy buffering. A lost id-conflict
  /// [`Event::Shutdown`] poisons the shared state
  /// ([`begin_shutdown`](Self::begin_shutdown)) — the node stops, the pump loop
  /// halts after this drain, and any parked join resolves with the shutdown error;
  /// this is the same terminal path the handle's
  /// [`Serf::shutdown`](crate::Serf::shutdown) reaches directly. An
  /// [`Event::KeyRequest`] is applied to the engine's LIVE wire keyring and answered
  /// in one call through `handle_key_request` — the live-keyring chokepoint, never a
  /// driver-local shadow. A dropped observation is fine; a dropped action is not, so
  /// the action is taken regardless of the app ever polling.
  fn route_drained_event(&self, ev: Event<I, SocketAddr>, now: Instant) -> bool {
    // `now` and the queued-outbound signal are consumed only by the encryption
    // `KeyRequest` arm; a build without an AEAD backend reads neither and queues no
    // key response.
    #[cfg(not(encryption))]
    let _ = now;
    #[cfg(encryption)]
    let mut queued = false;
    #[cfg(not(encryption))]
    let queued = false;

    match &ev {
      // A lost id-conflict vote means the local node MUST stop: poison the shared
      // state so the pump loop halts after this drain and any parked join resolves
      // with the shutdown error. The event still reaches the app via `poll_event`
      // (buffered below).
      Event::Shutdown => self.begin_shutdown(),
      // An inbound key-management request: apply the op to the engine's LIVE wire
      // keyring and answer the originator in one call. The response is a directed
      // gossip transmit egressed on the re-pump the runner performs while `queued`
      // is set.
      #[cfg(encryption)]
      Event::KeyRequest(req) => {
        // `Ok` means a key response was queued (re-pump to egress it). Ignoring
        // the Err case: `handle_key_request` has already applied the op to the
        // live keyring; an Err means only the best-effort response was
        // past-deadline or could not be routed, which queues no outbound work.
        queued |= self
          .engine
          .borrow_mut()
          .handle_key_request(req, now)
          .is_ok();
      }
      _ => {}
    }
    self.push_app_event(ev);
    queued
  }

  /// The number of app events shed because a consumer did not keep up: the
  /// engine's own passive-observation drops PLUS this driver's `poll_event`
  /// backlog drops.
  ///
  /// A single pump can shed observations INSIDE the engine (its bounded queue)
  /// before the driver's queue — freshly drained each pump — ever fills, so this
  /// sums BOTH counters; reporting only the driver's would under-count real loss.
  #[inline]
  pub(crate) fn events_dropped(&self) -> u64 {
    self
      .engine
      .borrow()
      .events_dropped()
      .saturating_add(self.app_events_dropped.get())
  }
}

#[cfg(test)]
mod tests;
