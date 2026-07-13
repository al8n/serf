//! The single-task run loop: drive the engine `pump` plus the N per-slot workers
//! as sibling futures.
//!
//! One embassy task owns the [`Runner`] and calls [`Runner::run`]. Inside, two
//! kinds of future race under one `select`:
//!
//! - the **pump loop** — re-pump the engine over a fresh
//!   [`SerfGossip`](crate::SerfGossip) + [`SerfStream`](crate::SerfStream) view,
//!   drain the machine's events (actioning serf's mandatory events, then buffering
//!   observations), then sleep on whichever of {UDP recv-ready, a worker/handle
//!   pump-wake, the folded deadline timer} fires first;
//! - the **N workers** — each [`run_slot`](crate::worker::run_slot) owns one
//!   `TcpSocket` and its `RefCell<Mailbox>`, looping internally forever.
//!
//! Both diverge under normal operation, so the `select` never resolves and the task
//! runs forever. A lost id-conflict [`Event::Shutdown`](serf_embedded::Event) is the
//! one terminal exit: the pump loop observes the poisoned shared state and returns,
//! collapsing the `select` — the worker futures drop and every socket winds down.
//!
//! Only the pump loop re-pumps; the workers loop on their own. Because
//! [`SerfEngine::pump`](serf_embedded::SerfEngine::pump) is synchronous, the
//! pump's borrows of the engine and the mailboxes complete before its `.await`,
//! and each worker only borrows its own mailbox briefly around its socket awaits —
//! so the pump and the workers never hold overlapping `RefCell` borrows even
//! though they are siblings in one task.
//!
//! # Self-delivery quiescence
//!
//! embassy-net (smoltcp underneath) does not loop a self-addressed UDP datagram
//! back into recv like an OS socket, so [`SerfGossip`](crate::SerfGossip) diverts
//! a datagram this node addressed to its OWN advertise address into a driver
//! `loopback` buffer. The pump computes its deadline and egresses BEFORE the drain
//! runs, so a `respond_key` the drain just queued — or a self-addressed datagram
//! the egress just looped back — would not be reflected by that pass. The pump
//! loop therefore RE-PUMPS at the same `now` while a pass queued a key response OR
//! left the loopback non-empty (bounded by [`MAX_SELF_DELIVERY_ITERS`]), so a
//! self response is collected within the wake and a caller sleeping on the engine
//! deadline never strands it.

use core::{cell::RefCell, net::SocketAddr};

use alloc::{collections::VecDeque, rc::Rc, vec::Vec};

use embassy_futures::{
  join::join_array,
  select::{select, select3},
};
use embassy_net::{tcp::TcpSocket, udp::UdpSocket};
use embassy_time::Timer;
use memberlist_proto::{Instant, Rng, SeedableRng};

use crate::{
  gossip_io::SerfGossip,
  mailbox::Mailbox,
  shared::Shared,
  stream_io::{SerfStream, SlotId, SlotWake},
  time,
  worker::run_slot,
};

/// The most pump → drain passes one wake makes to reach quiescence.
///
/// A single wake re-pumps while a pass produced new work the current deadline /
/// egress has not yet reflected: a `respond_key` the drain just queued, or a
/// self-addressed datagram the pump's egress just looped back (see
/// [`SerfGossip`](crate::SerfGossip)). Each self query / key response settles in a
/// few passes; this caps a pathological self-delivery cycle so a single wake
/// cannot spin forever. On hitting the cap with work still pending, the loop folds
/// `now` into its returned deadline so the caller re-polls at once.
const MAX_SELF_DELIVERY_ITERS: usize = 8;

/// Returns the earlier of two optional deadlines. If only one is `Some`, that
/// deadline wins; if both are `None` the result is `None`.
fn min_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
  match (a, b) {
    (Some(x), Some(y)) => Some(core::cmp::min(x, y)),
    (x, y) => x.or(y),
  }
}

/// The owned run-loop state for a node: the shared engine, the gossip UDP socket,
/// the `N` reliable-plane TCP sockets and their per-slot mailboxes + command
/// wakes, and the driver-side free-list.
///
/// `I` is the node id type; `N` is the TCP socket pool size; `G` is the gossip RNG
/// and `SR` serf's own core RNG (both defaulting to
/// [`SmallRng`](memberlist_proto::SmallRng)). Built by
/// [`Serf::new`](crate::Serf::new), which hands back the paired [`Serf`](crate::Serf)
/// handle.
pub struct Runner<
  'a,
  I,
  const N: usize,
  G = memberlist_proto::SmallRng,
  SR = memberlist_proto::SmallRng,
> where
  I: Eq + core::hash::Hash,
{
  pub(crate) shared: Rc<Shared<I, G, SR>>,
  pub(crate) udp: UdpSocket<'a>,
  pub(crate) tcp: [TcpSocket<'a>; N],
  pub(crate) mailboxes: [RefCell<Mailbox>; N],
  pub(crate) cmd_wakes: [SlotWake; N],
  /// Per-socket inactivity timeout (already in the embassy-time tick domain) each
  /// worker applies to its `TcpSocket` so a blocking connect/write/flush/read to an
  /// unresponsive peer cannot wedge it.
  pub(crate) socket_timeout: embassy_time::Duration,
  pub(crate) free: Vec<SlotId>,
}

impl<I, const N: usize, G, SR> Runner<'_, I, N, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Drive the node: pump the engine and run the `N` workers concurrently.
  ///
  /// Runs forever under normal operation — spawn it as an embassy task (or drive
  /// it with `select` against an operation in a test). It returns ONLY after the
  /// shared shutdown latch is set — a lost id-conflict
  /// [`Event::Shutdown`](serf_embedded::Event::Shutdown) or a public
  /// [`Serf::shutdown`](crate::Serf::shutdown): the pump loop observes the latch
  /// before its next egress-capable pump, performs one no-pump final drain, and
  /// returns, which resolves the `select` below and drops the worker futures — so
  /// this frame unwinds and the owned sockets (the TCP pool and the gossip UDP
  /// socket) close, taking the stopped node off the wire without flushing queued
  /// traffic.
  pub async fn run(self) {
    let Runner {
      shared,
      udp,
      mut tcp,
      mailboxes,
      cmd_wakes,
      socket_timeout,
      mut free,
    } = self;

    // The driver-owned self-delivery buffer: gossip datagrams this node addressed
    // to its own advertise address, replayed into the next pump's ingress. Lives
    // for the whole run so a self response queued in one wake is drained in the
    // same wake's re-pump loop.
    let loopback: RefCell<VecDeque<Vec<u8>>> = RefCell::new(VecDeque::new());

    // Build the N worker futures, each owning a distinct `&mut TcpSocket` (via
    // `each_mut`, which yields `N` non-aliasing mutable refs) paired with its
    // mailbox and command wake by index, plus the shared pump wake.
    let mut socket_iter = tcp.each_mut().into_iter();
    let workers = core::array::from_fn::<_, N, _>(|i| {
      let sock = socket_iter
        .next()
        .expect("from_fn yields indices 0..N and the socket array has exactly N elements");
      run_slot(
        sock,
        &mailboxes[i],
        &cmd_wakes[i],
        &shared.pump_wake,
        socket_timeout,
      )
    });

    // The pump loop and the workers race under one `select`. Under normal operation
    // the pump loop diverges (loops forever) and `join_array` of the diverging
    // workers likewise never completes, so the `select` never resolves and `run`
    // never returns. On a lost id-conflict shutdown the pump loop RETURNS after its
    // final drain; the `select` then completes and DROPS the worker futures,
    // releasing their `&mut TcpSocket` borrows. `run` then returns and the owned
    // sockets close as this frame unwinds — the structural teardown that stops the
    // losing node without any per-worker abort.
    //
    // Ignoring the `Either`: only the pump-loop arm can ever resolve (the workers
    // diverge), and its `()` output carries nothing — reaching here means shutdown,
    // and `run` simply returns.
    let _ = select(
      pump_loop(&shared, &udp, &mailboxes, &cmd_wakes, &mut free, &loopback),
      join_array(workers),
    )
    .await;
  }
}

/// The engine-pump half of [`Runner::run`]: re-pump to self-delivery quiescence on
/// each wake, actioning serf's mandatory events and folding join completions.
async fn pump_loop<I, G, SR>(
  shared: &Shared<I, G, SR>,
  udp: &UdpSocket<'_>,
  mailboxes: &[RefCell<Mailbox>],
  cmd_wakes: &[SlotWake],
  free: &mut Vec<SlotId>,
  loopback: &RefCell<VecDeque<Vec<u8>>>,
) where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  let advertise: SocketAddr = shared.advertise;
  loop {
    // Check the terminal latch BEFORE any egress-capable pump. A public `shutdown()`
    // — or a lost id-conflict vote latched on a prior tick — that woke this loop must
    // not get one more pump: an abrupt stop does not flush in-flight traffic. Run one
    // no-pump final drain so this node's engine-buffered events still reach
    // `app_events` for the app, then stop. `drain_events` neither pumps nor egresses,
    // so it cannot re-arm work — any queued outbound (key responses, loopback
    // self-datagrams, undisseminated gossip) is dropped by the abrupt-stop contract.
    if shared.is_shutdown() {
      shared.drain_events(time::now());
      return;
    }

    let now = time::now();

    // Pump → drain, re-running at the same `now` until neither a queued key
    // response nor a looped-back self-datagram remains, so both are handled within
    // this wake. The gossip and stream views are synchronous: their borrows of the
    // engine and mailboxes complete inside the block, before any `.await`.
    let mut next = None;
    let mut settled = false;
    for _ in 0..MAX_SELF_DELIVERY_ITERS {
      next = {
        let mut gossip = SerfGossip::new(udp, loopback, advertise);
        let mut stream = SerfStream::new(mailboxes, cmd_wakes, free);
        shared
          .engine
          .borrow_mut()
          .pump(now, &mut gossip, &mut stream)
      };
      // Action serf's mandatory events (Shutdown → stop, KeyRequest → live-keyring
      // chokepoint) and buffer every observation. Returns whether a key response
      // was queued the egress above did not see.
      let queued = shared.drain_events(now);
      // Check the latch immediately after the drain, BEFORE the re-pump decision. The
      // drain may have observed a lost id-conflict `Event::Shutdown` and latched the
      // terminal state; that drain was then the FINAL one — it buffered this tick's
      // events into `app_events` — so stop now rather than re-pump. Any queued outbound
      // (a key response, a looped-back self-datagram) is dropped by the abrupt-stop
      // contract: a lost node does not gossip on the id it just lost, which would only
      // confuse the winner. Returning ends this future, which resolves `Runner::run`'s
      // `select` and collapses the workers so the sockets wind down.
      if shared.is_shutdown() {
        return;
      }
      if !queued && loopback.borrow().is_empty() {
        settled = true;
        break;
      }
    }

    // On an unsettled loop (work still pending at the cap), fold `now` into the
    // deadline so the next wake re-pumps at once rather than sleeping past it.
    if !settled {
      next = min_opt(next, Some(now));
    }

    // Wait for the next thing worth re-pumping for: an inbound gossip datagram, a
    // worker/handle pump-wake, or the folded deadline.
    match next {
      Some(deadline) => {
        let raw = time::machine_to_raw(deadline);
        // Ignoring the `Either3`: which arm woke is irrelevant — any wake re-runs
        // the whole pump, which re-derives all work and the next deadline.
        let _ = select3(
          udp.wait_recv_ready(),
          shared.pump_wake.wait(),
          Timer::at(raw),
        )
        .await;
      }
      // No scheduled machine work: wake only on I/O or a pump-wake.
      None => {
        // Ignoring the `Either`: as above, any wake simply re-runs the pump.
        let _ = select(udp.wait_recv_ready(), shared.pump_wake.wait()).await;
      }
    }
  }
}
