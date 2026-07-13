use super::*;

use core::{
  net::{IpAddr, Ipv4Addr},
  time::Duration,
};

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use memberlist_proto::{
  Node, PushPullKind, SeedableRng, SmallRng,
  typed::{Alive, Message, Ping},
};
use smol_str::SmolStr;

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use memberlist_proto::EncryptionOptions;

/// A fixed-seed gossip RNG for the engine constructors. These are single-node
/// state tests; a deterministic seed keeps them reproducible.
fn test_rng() -> SmallRng {
  SmallRng::seed_from_u64(42)
}

/// A [`GossipIo`] that never receives and discards every send — for tests that
/// only exercise construction and the machine tick, not the wire.
struct NoGossip;

impl GossipIo for NoGossip {
  fn recv(&mut self, _buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
    None
  }

  fn send(&mut self, _bytes: &[u8], _dest: SocketAddr) {}
}

/// A pooled [`StreamIo`] whose sockets never establish — enough to drive the
/// reliable-plane bookkeeping (dial defer, listener replenish) without a fabric.
struct NoStream {
  free: std::vec::Vec<u32>,
}

impl NoStream {
  fn with_pool(size: u32) -> Self {
    Self {
      free: (0..size).collect(),
    }
  }
}

impl StreamIo for NoStream {
  type Conn = u32;

  fn take_free(&mut self) -> Option<u32> {
    self.free.pop()
  }

  fn give(&mut self, c: u32) {
    self.free.push(c);
  }

  fn free_count(&self) -> usize {
    self.free.len()
  }

  fn listen(&mut self, _c: u32, _port: u16) -> Result<(), crate::StreamIoError> {
    Ok(())
  }

  fn accepted_peer(&self, _c: u32) -> Option<SocketAddr> {
    None
  }

  fn connect(
    &mut self,
    _c: u32,
    _remote: SocketAddr,
    _local_port: u16,
  ) -> Result<(), crate::StreamIoError> {
    Err(crate::StreamIoError::Busy)
  }

  fn may_send(&self, _c: u32) -> bool {
    false
  }

  fn may_recv(&self, _c: u32) -> bool {
    false
  }

  fn is_open(&self, _c: u32) -> bool {
    false
  }

  fn is_established(&self, _c: u32) -> bool {
    false
  }

  fn recv(&mut self, _c: u32, _buf: &mut [u8]) -> Option<usize> {
    None
  }

  fn recv_finished(&self, _c: u32) -> bool {
    false
  }

  fn send(&mut self, _c: u32, _bytes: &[u8]) -> usize {
    0
  }

  fn send_queue(&self, _c: u32) -> usize {
    0
  }

  fn close(&mut self, _c: u32) {}

  fn abort(&mut self, _c: u32) {}
}

fn node_addr(port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port)
}

fn make_engine() -> SerfEngine<SmolStr, u32> {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("test"), node_addr(7946));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  SerfEngine::try_new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    SerfOptions::new(),
    now,
    test_rng(),
  )
  .expect("valid configuration must construct without error")
}

/// A valid configuration constructs, and a freshly built endpoint is `Alive`
/// (running).
#[test]
fn construction_succeeds_and_is_alive() {
  let engine = make_engine();
  assert_eq!(engine.state(), SerfState::Alive);
  assert!(
    engine.ensure_running().is_ok(),
    "a freshly constructed endpoint must be running"
  );
}

/// A non-routable advertise address is rejected at construction rather than
/// gossiped cluster-wide (the reused advertise-routability guard).
#[test]
fn non_routable_advertise_is_rejected() {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  // The unspecified address is non-routable.
  let bad = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946);
  let ep_cfg = EndpointOptions::new(SmolStr::new("test"), bad);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let result = SerfEngine::<SmolStr, u32>::try_new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    SerfOptions::new(),
    now,
    test_rng(),
  );
  assert!(
    matches!(
      result,
      Err(InitError::Memberlist(
        crate::MemberlistInitError::NonRoutableAdvertiseAddr(_)
      ))
    ),
    "a non-routable advertise address must fail construction"
  );
}

/// An over-ceiling `max_user_event_size` is rejected in the engine's own
/// construction funnel, so a driver built directly on the engine cannot bypass
/// the serf-options validation the wrapping drivers enforce.
#[test]
fn over_ceiling_user_event_size_is_rejected_at_engine_construction() {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("test"), node_addr(7946));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let serf_opts =
    SerfOptions::new().with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1);
  let result = SerfEngine::<SmolStr, u32>::try_new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    serf_opts,
    now,
    test_rng(),
  );
  assert!(
    matches!(result, Err(InitError::InvalidSerfOptions(_))),
    "an over-ceiling max_user_event_size must fail engine construction"
  );
}

/// `start` then a single `pump` advances the machine without panicking and
/// returns a wakeup deadline; the single-node engine tracks no remote members.
#[test]
fn start_then_single_pump_does_not_panic() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);
  // Must not panic, and the SWIM schedulers armed by `start` yield a finite next
  // deadline.
  let deadline = engine.pump(now, &mut gossip, &mut stream);
  assert!(
    deadline.is_some(),
    "an armed engine must return a next wakeup deadline"
  );
  // Draining events must not panic on a quiescent single node.
  while engine.poll_event().is_some() {}
}

/// `join` announces the serf join intent and queues each routable seed; the pump
/// initiates a push/pull per seed. With an exhausted dial pool the exchange parks
/// as `PendingDial` rather than dropping the dial intent.
#[test]
fn join_queues_seed_then_pump_parks_pending_dial() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  // A listener is present but the dial pool is empty, so the Connect finds no free
  // slot and defers to PendingDial.
  engine.set_listener(9);
  engine
    .join(&[node_addr(7002)], false, now)
    .expect("join announces intent and queues the routable seed");

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.pending_dial_count(),
    1,
    "an exhausted-pool join seed must park as PendingDial, not drop the dial"
  );
}

/// A non-routable join seed is dropped rather than queued for a doomed dial; a
/// routable one is queued.
#[test]
fn join_drops_non_routable_seed() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  engine.set_listener(9);

  // Port 0 is non-routable and must not be queued.
  let dead = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 0);
  engine.join(&[dead], false, now).expect("join succeeds");

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.pending_dial_count(),
    0,
    "a non-routable seed must be dropped, never dialed"
  );
}

/// A user event is accepted while running.
#[test]
fn user_event_accepted_while_running() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  engine
    .user_event("deploy", Bytes::from_static(b"v2"), false, now)
    .expect("a user event is accepted while the node is running");
}

/// With user coalescing enabled and a small buffered-volume cap, a flood of
/// distinct-named coalescing user events fed past the cap is shed and counted, and
/// the running total surfaces through the engine's `coalesced_user_events_dropped`
/// forward (the endpoint counter reachable through the engine handle).
#[test]
fn coalesced_user_events_dropped_surfaces_overflow() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("test"), node_addr(7946));
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let mut engine: SerfEngine<SmolStr, u32> = SerfEngine::try_new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    serf_opts,
    now,
    test_rng(),
  )
  .expect("valid configuration must construct without error");
  engine.start(now);
  assert_eq!(
    engine.coalesced_user_events_dropped(),
    0,
    "no drops before any user event is fed"
  );

  let n: u32 = 20;
  for i in 0..n {
    engine
      .user_event(format!("evt-{i}"), Bytes::from_static(b"p"), true, now)
      .expect("a coalescing user event is accepted while running");
  }

  assert_eq!(
    engine.coalesced_user_events_dropped(),
    u64::from(n) - cap.get() as u64,
    "every distinct-named coalescing event past the cap is shed and counted"
  );
  assert_eq!(
    engine.coalesced_member_events_dropped(),
    0,
    "member coalescing is disabled, so its drop counter stays zero"
  );
}

/// `leave` transitions the endpoint out of `Alive`, and a subsequent `join` is
/// rejected (serf announces its own join intent only from `Alive`).
#[test]
fn leave_transitions_state_and_blocks_further_join() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  // Let the construction self-join sieve settle before leaving.
  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);
  engine.pump(now, &mut gossip, &mut stream);

  engine
    .leave(now)
    .expect("leave from a running node succeeds");
  assert_ne!(
    engine.state(),
    SerfState::Alive,
    "leave must transition the endpoint out of Alive"
  );
  assert!(
    matches!(
      engine.join(&[node_addr(7003)], false, now),
      Err(SerfError::BadJoinState(_))
    ),
    "a join after leave must be rejected with BadJoinState"
  );
}

// ── serf core RNG injection (finding 1) ──────────────────────────────────────

/// Build a fresh single-node engine seeding serf's core RNG from `serf_seed` (the
/// gossip RNG held fixed), start it, and return the `id` of its first issued
/// query — a `u32` drawn straight from serf's core RNG. The seed is the only
/// input that varies, so the query id is a pure function of the serf RNG seed.
fn first_query_id(serf_seed: u64) -> u32 {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("q"), node_addr(7946));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let mut engine: SerfEngine<SmolStr, u32> = SerfEngine::try_new_at_with_rng(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    SerfOptions::new(),
    now,
    SmallRng::seed_from_u64(1),
    SmallRng::seed_from_u64(serf_seed),
  )
  .expect("valid configuration must construct");
  engine.start(now);
  engine
    .query(
      "probe",
      Bytes::from_static(b"payload"),
      QueryParams::default(),
      now,
    )
    .expect("a query is issued while running")
    .id
}

/// Two engines seeded with DIFFERENT serf RNGs produce DIFFERENT first query ids,
/// and the SAME serf seed reproduces the SAME id — proving the injected serf RNG
/// (not a zero seed or some other entropy) is what threads through to query-id
/// generation, so fresh embedded nodes no longer share a `(ltime, id)` sequence.
#[test]
fn distinct_serf_rng_seeds_yield_distinct_query_ids() {
  assert_ne!(
    first_query_id(100),
    first_query_id(200),
    "distinct serf RNG seeds must produce distinct first query ids"
  );
  assert_eq!(
    first_query_id(100),
    first_query_id(100),
    "the same serf RNG seed must reproduce the same first query id"
  );
}

// ── core-owned join fan-out / correlation (finding 2) ─────────────────────────

/// A failed-dial `ignore_old` join resolves `Err(JoinFailed)` and is reaped: the
/// dial's `connect` errors, the machine terminalizes the push/pull `Failed`, and
/// `poll_join` folds that completion into the join — clearing the recorded
/// ignore-join stream and dropping the waiter (no leak of either the pending-join
/// entry or its machine ignore stream).
#[test]
fn failed_ignore_old_join_resolves_err_and_clears_ignore_streams() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  // One dial slot in the pool plus a listener; `NoStream::connect` errors, so the
  // dial this slot backs fails.
  engine.plane_mut().pool.push(5);
  engine.set_listener(9);

  let handle = engine
    .join(&[node_addr(7002)], true, now)
    .expect("join announces intent and mints a handle");
  assert_eq!(
    engine.pending_join_count(),
    1,
    "the in-flight join is tracked"
  );

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  // One pump dispatches the seed, captures the Connect, attempts the dial (which
  // errors), and the machine emits the terminal ExchangeCompleted(Failed).
  engine.pump(now, &mut gossip, &mut stream);

  // Draining events folds the failed completion into the join; poll_join then
  // resolves it.
  let mut outcome = None;
  for _ in 0..8 {
    while engine.poll_event().is_some() {}
    if let Some(res) = engine.poll_join(handle) {
      outcome = Some(res);
      break;
    }
    engine.pump(now, &mut gossip, &mut stream);
  }

  match outcome {
    Some(Err(jf)) => {
      assert_eq!(jf.requested(), 1, "one routable seed was dispatched");
      assert_eq!(jf.contacted(), 0, "the failed dial contacted no seed");
    }
    other => panic!("expected Some(Err(JoinFailed)), got {other:?}"),
  }
  assert_eq!(
    engine.pending_join_count(),
    0,
    "a resolved join must be reaped (its ignore stream cleared, not leaked)"
  );
}

// ── Two-engine in-memory reliable link ───────────────────────────────────────
//
// A faithful loopback so a real serf join push/pull runs end-to-end through BOTH
// engines' reliable planes, terminalizing the initiating exchange `Succeeded` —
// the only way to drive an `Event::ExchangeCompleted(PushPull, Succeeded)` the
// engine's core-owned join folds into its reached set. One end's `connect`
// registers a pending SYN on the shared fabric; the destination's listener
// completes the passive open when its `accepted_peer` is polled, after which bytes
// ferry both ways over the matched pipe. The link acks instantly by default
// (`send_queue` is 0), so a graceful close FINs without a `Closing` drain;
// [`LinkPair::hold_b_tx`] withholds B's acknowledgements so the drain-before-close
// path can be driven.

/// The two directional byte streams of one established pipe plus its FIN/reset
/// flags. The dialer writes `d2a` (the acceptor reads it) and vice versa.
#[derive(Default)]
struct Pipe {
  d2a: VecDeque<u8>,
  a2d: VecDeque<u8>,
  d_fin: bool,
  a_fin: bool,
  reset: bool,
  established: bool,
  /// Bytes the dialer end handed to `send` that the peer has not yet acknowledged —
  /// what `send_queue` reports for that end. The bytes are DELIVERED to the peer's rx
  /// immediately (so the FSM still completes); only the acknowledgement lingers, so a
  /// graceful close over this end parks in `Closing` until a test acks it. Zero unless
  /// the end's `hold_tx` is set.
  d_unacked: usize,
  /// As `d_unacked`, for the acceptor end.
  a_unacked: usize,
}

/// A SYN parked on the fabric by a dialer's `connect`, awaiting the destination's
/// listener to complete the passive open.
struct PendingSyn {
  dest: SocketAddr,
  src: SocketAddr,
  pipe: u64,
}

/// Shared fabric state: every established pipe plus the not-yet-accepted SYNs.
#[derive(Default)]
struct FabricInner {
  pipes: BTreeMap<u64, Pipe>,
  pending: Vec<PendingSyn>,
  next_pipe: u64,
}

impl FabricInner {
  fn fresh() -> Fabric {
    Rc::new(RefCell::new(FabricInner::default()))
  }
}

type Fabric = Rc<RefCell<FabricInner>>;

/// Which end of a pipe a slot is bound to, so `send`/`recv`/`close` route to the
/// correct buffer.
#[derive(Clone, Copy, PartialEq)]
enum End {
  Dialer,
  Acceptor,
}

/// What one of an engine's reliable slots is currently doing.
#[derive(Clone)]
enum SlotRole {
  Idle,
  Listening,
  Bound(u64, End),
}

/// One engine's reliable I/O over the shared fabric: its own slot pool and
/// per-slot role, plus this engine's advertised address (the SYN destination its
/// listener answers). `role` is a `RefCell` so the `&self` `accepted_peer` can
/// re-bind a Listening slot to the Acceptor end the instant it completes a passive
/// open — the same handle the engine keeps for the exchange.
struct LinkRel {
  fabric: Fabric,
  me: SocketAddr,
  free: std::vec::Vec<u32>,
  role: RefCell<BTreeMap<u32, SlotRole>>,
  /// When set, a `send` on this end ALSO accrues unacked tx (`send_queue` > 0),
  /// modelling a peer slow to acknowledge. A graceful close over such a connection
  /// parks in `Closing` until the tx drains, exercising the drain-before-close path.
  /// Off by default (the link acks instantly).
  hold_tx: bool,
}

impl LinkRel {
  fn new(fabric: Fabric, me: SocketAddr, handles: &[u32]) -> Self {
    let mut role = BTreeMap::new();
    for &h in handles {
      role.insert(h, SlotRole::Idle);
    }
    Self {
      fabric,
      me,
      free: handles.to_vec(),
      role: RefCell::new(role),
      hold_tx: false,
    }
  }

  fn role_of(&self, c: u32) -> Option<SlotRole> {
    self.role.borrow().get(&c).cloned()
  }
}

/// Bytes waiting for the end `end` to read (the OTHER end's tx buffer).
fn pipe_inbound(p: &Pipe, end: End) -> &VecDeque<u8> {
  match end {
    End::Dialer => &p.a2d,
    End::Acceptor => &p.d2a,
  }
}

/// Whether THIS end emitted its FIN.
fn pipe_end_fin(p: &Pipe, end: End) -> bool {
  match end {
    End::Dialer => p.d_fin,
    End::Acceptor => p.a_fin,
  }
}

/// Whether the PEER end emitted its FIN.
fn pipe_peer_fin(p: &Pipe, end: End) -> bool {
  match end {
    End::Dialer => p.a_fin,
    End::Acceptor => p.d_fin,
  }
}

impl StreamIo for LinkRel {
  type Conn = u32;

  fn take_free(&mut self) -> Option<u32> {
    self.free.pop()
  }

  fn give(&mut self, c: u32) {
    self.role.borrow_mut().insert(c, SlotRole::Idle);
    self.free.push(c);
  }

  fn free_count(&self) -> usize {
    self.free.len()
  }

  fn listen(&mut self, c: u32, _port: u16) -> Result<(), crate::StreamIoError> {
    self.role.borrow_mut().insert(c, SlotRole::Listening);
    Ok(())
  }

  fn accepted_peer(&self, c: u32) -> Option<SocketAddr> {
    if !matches!(self.role_of(c), Some(SlotRole::Listening)) {
      return None;
    }
    let mut fab = self.fabric.borrow_mut();
    let pos = fab.pending.iter().position(|s| s.dest == self.me)?;
    let syn = fab.pending.remove(pos);
    fab.pipes.entry(syn.pipe).or_default().established = true;
    drop(fab);
    self
      .role
      .borrow_mut()
      .insert(c, SlotRole::Bound(syn.pipe, End::Acceptor));
    Some(syn.src)
  }

  fn connect(
    &mut self,
    c: u32,
    remote: SocketAddr,
    _local_port: u16,
  ) -> Result<(), crate::StreamIoError> {
    let mut fab = self.fabric.borrow_mut();
    let pipe = fab.next_pipe;
    fab.next_pipe += 1;
    fab.pipes.insert(pipe, Pipe::default());
    fab.pending.push(PendingSyn {
      dest: remote,
      src: self.me,
      pipe,
    });
    drop(fab);
    self
      .role
      .borrow_mut()
      .insert(c, SlotRole::Bound(pipe, End::Dialer));
    Ok(())
  }

  fn may_send(&self, c: u32) -> bool {
    match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => {
        let fab = self.fabric.borrow();
        match fab.pipes.get(&pipe) {
          Some(p) => p.established && !p.reset && !pipe_end_fin(p, end),
          None => false,
        }
      }
      _ => false,
    }
  }

  fn may_recv(&self, c: u32) -> bool {
    match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => {
        let fab = self.fabric.borrow();
        fab
          .pipes
          .get(&pipe)
          .map(|p| !pipe_inbound(p, end).is_empty())
          .unwrap_or(false)
      }
      _ => false,
    }
  }

  fn is_open(&self, c: u32) -> bool {
    match self.role_of(c) {
      Some(SlotRole::Bound(pipe, _)) => {
        let fab = self.fabric.borrow();
        match fab.pipes.get(&pipe) {
          Some(p) => !p.reset && !(p.d_fin && p.a_fin),
          None => false,
        }
      }
      Some(SlotRole::Listening) => true,
      _ => false,
    }
  }

  fn is_established(&self, c: u32) -> bool {
    self.may_send(c)
  }

  fn recv(&mut self, c: u32, buf: &mut [u8]) -> Option<usize> {
    let (pipe, end) = match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => (pipe, end),
      _ => return None,
    };
    let mut fab = self.fabric.borrow_mut();
    let p = fab.pipes.get_mut(&pipe)?;
    let q = match end {
      End::Dialer => &mut p.a2d,
      End::Acceptor => &mut p.d2a,
    };
    if q.is_empty() {
      return None;
    }
    let n = q.len().min(buf.len());
    for (i, b) in q.drain(..n).enumerate() {
      buf[i] = b;
    }
    Some(n)
  }

  fn recv_finished(&self, c: u32) -> bool {
    match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => {
        let fab = self.fabric.borrow();
        match fab.pipes.get(&pipe) {
          Some(p) => !p.reset && pipe_peer_fin(p, end) && pipe_inbound(p, end).is_empty(),
          None => false,
        }
      }
      _ => false,
    }
  }

  fn send(&mut self, c: u32, bytes: &[u8]) -> usize {
    let (pipe, end) = match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => (pipe, end),
      _ => return 0,
    };
    let hold = self.hold_tx;
    let mut fab = self.fabric.borrow_mut();
    let Some(p) = fab.pipes.get_mut(&pipe) else {
      return 0;
    };
    if p.reset || !p.established {
      return 0;
    }
    // Deliver to the peer's rx immediately (the FSM sees the bytes); the link acks
    // instantly unless this end is holding, in which case the acknowledgement lingers
    // as unacked tx so a later graceful close parks in `Closing`.
    match end {
      End::Dialer => {
        p.d2a.extend(bytes.iter().copied());
        if hold {
          p.d_unacked += bytes.len();
        }
      }
      End::Acceptor => {
        p.a2d.extend(bytes.iter().copied());
        if hold {
          p.a_unacked += bytes.len();
        }
      }
    }
    bytes.len()
  }

  fn send_queue(&self, c: u32) -> usize {
    match self.role_of(c) {
      Some(SlotRole::Bound(pipe, end)) => {
        let fab = self.fabric.borrow();
        match fab.pipes.get(&pipe) {
          Some(p) => match end {
            End::Dialer => p.d_unacked,
            End::Acceptor => p.a_unacked,
          },
          None => 0,
        }
      }
      _ => 0,
    }
  }

  fn close(&mut self, c: u32) {
    if let Some(SlotRole::Bound(pipe, end)) = self.role_of(c) {
      let mut fab = self.fabric.borrow_mut();
      if let Some(p) = fab.pipes.get_mut(&pipe) {
        match end {
          End::Dialer => p.d_fin = true,
          End::Acceptor => p.a_fin = true,
        }
      }
    }
  }

  fn abort(&mut self, c: u32) {
    if let Some(SlotRole::Bound(pipe, _)) = self.role_of(c) {
      let mut fab = self.fabric.borrow_mut();
      if let Some(p) = fab.pipes.get_mut(&pipe) {
        p.reset = true;
      }
    }
  }
}

/// A paired gossip relay: datagrams `send`-emitted toward a peer's address land in
/// that peer's inbound queue (and vice versa), so the two engines also exchange
/// SWIM gossip. Each engine holds one end keyed by its own address.
#[derive(Clone)]
struct GossipWire {
  outbound: Rc<RefCell<std::vec::Vec<(SocketAddr, std::vec::Vec<u8>)>>>,
  inbound: Rc<RefCell<VecDeque<(SocketAddr, std::vec::Vec<u8>)>>>,
}

impl GossipIo for GossipWire {
  fn recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
    let (src, bytes) = self.inbound.borrow_mut().pop_front()?;
    let n = bytes.len().min(buf.len());
    buf[..n].copy_from_slice(&bytes[..n]);
    Some((src, n))
  }

  fn send(&mut self, bytes: &[u8], dest: SocketAddr) {
    self.outbound.borrow_mut().push((dest, bytes.to_vec()));
  }
}

/// A linked two-engine fixture sharing one reliable fabric and a cross-wired
/// gossip relay. `step` pumps both engines once and then ferries each side's
/// emitted gossip into the other side's inbound queue.
struct LinkPair {
  a: SerfEngine<SmolStr, u32>,
  b: SerfEngine<SmolStr, u32>,
  a_rel: LinkRel,
  b_rel: LinkRel,
  a_gossip: GossipWire,
  b_gossip: GossipWire,
  a_addr: SocketAddr,
  b_addr: SocketAddr,
}

impl LinkPair {
  /// Two running serf engines `a` (port 7946) and `b` (port 7947) on a shared
  /// fabric, each with `pool` dial slots plus a listener. A short `stream_timeout`
  /// keeps a wedged exchange from hanging the test.
  fn new(pool_handles_a: &[u32], pool_handles_b: &[u32]) -> Self {
    let now = Instant::from_origin(Duration::from_secs(86_400));
    let a_addr = node_addr(7946);
    let b_addr = node_addr(7947);

    let mk = |id: &str, port: u16, addr: SocketAddr| -> SerfEngine<SmolStr, u32> {
      let cfg = Options::new()
        .with_port(port)
        .with_close_timeout(Duration::from_secs(10));
      let ep_cfg =
        EndpointOptions::new(SmolStr::new(id), addr).with_stream_timeout(Duration::from_secs(5));
      let mut e: SerfEngine<SmolStr, u32> = SerfEngine::try_new_at(
        cfg,
        TransformOptions::default(),
        ep_cfg,
        SerfOptions::new(),
        now,
        test_rng(),
      )
      .expect("construct");
      e.start(now);
      e
    };

    let mut a = mk("a", 7946, a_addr);
    let mut b = mk("b", 7947, b_addr);

    let fabric = FabricInner::fresh();
    // The listener handle is the last in each pool; the rest are dial slots.
    let (a_listener, a_dials) = pool_handles_a.split_last().expect("at least one handle");
    let (b_listener, b_dials) = pool_handles_b.split_last().expect("at least one handle");
    for &h in a_dials {
      a.plane_mut().pool.push(h);
    }
    for &h in b_dials {
      b.plane_mut().pool.push(h);
    }
    a.set_listener(*a_listener);
    b.set_listener(*b_listener);

    let mut a_rel = LinkRel::new(fabric.clone(), a_addr, pool_handles_a);
    let mut b_rel = LinkRel::new(fabric, b_addr, pool_handles_b);
    // The engine already owns each pool/listener; remove the listeners from the
    // mock free-lists so a re-listen does not double-hand a listener, and arm them.
    a_rel.free.retain(|h| h != a_listener);
    b_rel.free.retain(|h| h != b_listener);
    a_rel.listen(*a_listener, 7946).expect("listen");
    b_rel.listen(*b_listener, 7947).expect("listen");

    let a2b: Rc<RefCell<std::vec::Vec<(SocketAddr, std::vec::Vec<u8>)>>> =
      Rc::new(RefCell::new(std::vec::Vec::new()));
    let b2a: Rc<RefCell<std::vec::Vec<(SocketAddr, std::vec::Vec<u8>)>>> =
      Rc::new(RefCell::new(std::vec::Vec::new()));
    let a_in: Rc<RefCell<VecDeque<(SocketAddr, std::vec::Vec<u8>)>>> =
      Rc::new(RefCell::new(VecDeque::new()));
    let b_in: Rc<RefCell<VecDeque<(SocketAddr, std::vec::Vec<u8>)>>> =
      Rc::new(RefCell::new(VecDeque::new()));
    let a_gossip = GossipWire {
      outbound: a2b,
      inbound: a_in,
    };
    let b_gossip = GossipWire {
      outbound: b2a,
      inbound: b_in,
    };

    LinkPair {
      a,
      b,
      a_rel,
      b_rel,
      a_gossip,
      b_gossip,
      a_addr,
      b_addr,
    }
  }

  /// Make B (the acceptor) accrue unacked tx on every `send`, so when B's bridge
  /// gracefully closes with its push/pull reply still unacknowledged the connection
  /// parks in `Closing` rather than FIN-ing at once — the drain-before-close path.
  fn hold_b_tx(&mut self) {
    self.b_rel.hold_tx = true;
  }

  /// Acknowledge up to `amount` of B's accrued unacked tx across its pipes, modelling
  /// the peer draining B's reply. Steps the `Closing` drain through its progress
  /// (partial ack) and terminal-FIN (fully drained) branches.
  fn ack_b(&mut self, amount: usize) {
    let mut fab = self.b_rel.fabric.borrow_mut();
    let mut left = amount;
    for p in fab.pipes.values_mut() {
      let take = p.a_unacked.min(left);
      p.a_unacked -= take;
      left -= take;
      if left == 0 {
        break;
      }
    }
  }

  /// Pump both engines once at `now`, then ferry each side's emitted gossip into
  /// the peer's inbound queue.
  fn step(&mut self, now: Instant) {
    self.a.pump(now, &mut self.a_gossip, &mut self.a_rel);
    self.b.pump(now, &mut self.b_gossip, &mut self.b_rel);
    let a_out: std::vec::Vec<_> = self.a_gossip.outbound.borrow_mut().drain(..).collect();
    for (dest, bytes) in a_out {
      if dest == self.b_addr {
        self
          .b_gossip
          .inbound
          .borrow_mut()
          .push_back((self.a_addr, bytes));
      }
    }
    let b_out: std::vec::Vec<_> = self.b_gossip.outbound.borrow_mut().drain(..).collect();
    for (dest, bytes) in b_out {
      if dest == self.a_addr {
        self
          .a_gossip
          .inbound
          .borrow_mut()
          .push_back((self.b_addr, bytes));
      }
    }
  }
}

/// A full serf join over the in-memory reliable link: `a.join([b])` drives the
/// push/pull to a `Succeeded` `ExchangeCompleted`, which the engine folds into the
/// await-result join — accumulating B's address into the reached set — so
/// `poll_join` resolves `Ok([b_addr])`. The join is then reaped and A has learned
/// B through the merge.
#[test]
fn two_engine_join_folds_reached_set_and_poll_join_resolves_ok() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut outcome = None;
  for _ in 0..40 {
    link.step(now);
    // Drain A's events — this folds the push/pull ExchangeCompleted into the join.
    while link.a.poll_event().is_some() {}
    // Drain B's events too so its queue cannot stall the bridge.
    while link.b.poll_event().is_some() {}
    if let Some(res) = link.a.poll_join(handle) {
      outcome = Some(res);
      break;
    }
  }

  match outcome {
    Some(Ok(reached)) => {
      assert!(
        reached.contains(&link.b_addr),
        "the reached set must contain B, folded from the Succeeded push/pull"
      );
    }
    other => panic!("expected Some(Ok(reached)), got {other:?}"),
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the resolved join must be reaped"
  );
  assert_eq!(
    link.a.num_members(),
    2,
    "A must have learned B through the push/pull merge"
  );
}

// ── leave / ignore_old decoupling ─────────────────────────────────────────────

/// An `ignore_old` join whose `leave()` races an in-flight push/pull: `leave` must
/// NOT clear the exchange's ignore token, because the exchange can still merge and
/// a merge past a removed token replays the seed's pre-join user events. The token
/// is instead cleared only on the exchange terminal (the pump fold), so it is
/// retained across leave yet never leaks. Regression for a `leave()` that cleared
/// the ignore streams before the exchange terminal.
#[test]
fn leave_retains_ignore_token_until_exchange_terminal() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  // A starts an `ignore_old` join to B and dispatches the dial with a single step,
  // so the push/pull is IN FLIGHT (its ignore token recorded, its exchange bound
  // into `pending`) — not yet terminal — when A leaves.
  let handle = link
    .a
    .join(&[link.b_addr], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");
  link.step(now);

  // The dispatched seed recorded exactly one ignore-join StreamId, and its exchange
  // is still in flight (no terminal `ExchangeCompleted` yet).
  let sid = {
    let pj = &link.a.pending_joins[&handle];
    assert_eq!(
      pj.started.len(),
      1,
      "the ignore_old seed recorded its StreamId"
    );
    assert!(
      !pj.pending.is_empty(),
      "its push/pull exchange is bound and in flight"
    );
    *pj.started.iter().next().expect("one started stream")
  };
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token is recorded while the exchange is in flight"
  );

  // A leaves mid-exchange. The fix RETAINS the token (its exchange can still
  // merge); the buggy leave cleared it right here, so a late merge would replay
  // the seed's pre-join events.
  link
    .a
    .leave(now)
    .expect("leave from a running node succeeds");
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "leave must NOT clear the ignore token while the exchange can still merge"
  );

  // Driving the exchange to its terminal clears the token via the pump fold — the
  // SOLE cleanup site — so the machine's ignore set never leaks.
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if !link.a.endpoint.test_has_ignore_join_stream(sid) {
      break;
    }
  }
  assert!(
    !link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token must be cleared on the exchange terminal (no leak)"
  );
}

/// A refused `leave` — the `LeaveClockExhausted` watermark guard, which leaves the
/// node `Alive` without mutating serf state — must NOT touch the in-flight join
/// state: the queued seed survives, the pending join is neither force-resolved nor
/// reaped, and the endpoint stays `Alive`. The engine calls `endpoint.leave`
/// BEFORE abandoning any join, so a refusal (`?`) returns with the join
/// bookkeeping untouched. Regression for a `leave()` that cleared seeds and
/// force-resolved joins before the machine leave was accepted.
#[test]
fn refused_leave_leaves_join_state_untouched() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  engine.set_listener(9);

  let handle = engine
    .join(&[node_addr(7002)], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");
  assert_eq!(
    engine.pending_join_count(),
    1,
    "the in-flight join is tracked"
  );
  assert_eq!(engine.pending_seeds.len(), 1, "the routable seed is queued");

  // Drive serf's member clock to the LTIME_MAX integrity floor so the next leave
  // stamp reaches LTIME_MAX and is refused — the watermark serf-proto's own
  // leave-watermark test drives the clock to. LTIME_MAX is serf-proto's private
  // integrity floor `1 << 63`.
  const LTIME_MAX: u64 = 1u64 << 63;
  engine.endpoint.test_set_clocks(LTIME_MAX - 1, 0, 0);

  let err = engine
    .leave(now)
    .expect_err("a leave whose stamp reaches LTIME_MAX must be refused");
  assert!(
    matches!(err, SerfError::LeaveClockExhausted),
    "expected LeaveClockExhausted, got {err:?}"
  );

  // The node stays Alive and every scrap of join bookkeeping is untouched.
  assert_eq!(
    engine.state(),
    SerfState::Alive,
    "a refused leave must leave the node Alive"
  );
  assert_eq!(
    engine.pending_seeds.len(),
    1,
    "the queued seed must survive a refused leave"
  );
  assert_eq!(
    engine.pending_join_count(),
    1,
    "a refused leave must not reap the pending join"
  );
  assert!(
    matches!(engine.pending_joins[&handle].reply, JoinReply::Pending),
    "a refused leave must not force-resolve the pending join"
  );
  assert!(
    engine.poll_join(handle).is_none(),
    "the refused-leave join is still in flight"
  );
}

// ── fold-before-leave / leak-free reap / cancel_join ──────────────────────────

/// A push/pull driven to `Succeeded` purely by pumps — never `poll_join`'d, with
/// A's events never drained — is folded and resolved `Ready(Ok)` BY THE PUMP; a
/// subsequent `leave()` then preserves that result (it neither re-folds nor
/// clobbers it) while the buffered events still deliver in order, exactly once. So
/// the caller gets `Ok(reached)` including B across a leave. Regression both for a
/// pump that failed to fold (→ the reply stays Pending and leave would freeze a
/// stale `JoinFailed`) and for a leave that double-folds or loses the buffered
/// events.
#[test]
fn pump_folds_success_then_leave_preserves_it() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  // Drive ONLY the shared reliable fabric (pumping both engines ferries it) and
  // NEVER the gossip relay, so A learns B SOLELY through the push/pull — making
  // `num_members()==2` a precise "the push/pull merged" signal. A's events are left
  // UNDRAINED, so the PUMP alone folds the terminal `ExchangeCompleted(Succeeded)`.
  for _ in 0..40 {
    link.a.pump(now, &mut link.a_gossip, &mut link.a_rel);
    link.b.pump(now, &mut link.b_gossip, &mut link.b_rel);
    // Drain B so its machine advances the push/pull response; discard both sides'
    // emitted gossip so it never teaches A about B out of band.
    while link.b.poll_event().is_some() {}
    link.a_gossip.outbound.borrow_mut().clear();
    link.b_gossip.outbound.borrow_mut().clear();
    if link.a.num_members() == 2 {
      break;
    }
  }
  assert_eq!(
    link.a.num_members(),
    2,
    "the push/pull merged B into A (the exchange completed at the machine level)"
  );
  // The PUMP folded the Succeeded completion with no poll_join and no A poll_event:
  // the reply resolved Ready with B in the reached set and the exchange is done.
  {
    let pj = &link.a.pending_joins[&handle];
    assert!(
      matches!(pj.reply, JoinReply::Ready(_)),
      "the pump folded + resolved the success (no poll_event required)"
    );
    assert!(
      pj.contacted.contains(&link.b_addr),
      "the pump-folded reached set includes B"
    );
    assert!(
      pj.pending.is_empty(),
      "the exchange terminal emptied pending in the pump"
    );
  }

  // leave preserves the pump-resolved reply (it neither re-folds nor clobbers it)
  // and never loses the buffered events.
  link
    .a
    .leave(now)
    .expect("leave from a running node succeeds");

  // The buffered (already-folded) events still deliver, in order and exactly once.
  while link.a.poll_event().is_some() {}
  match link.a.poll_join(handle) {
    Some(Ok(reached)) => {
      assert!(
        reached.contains(&link.b_addr),
        "the folded reached set must include B"
      );
      assert_eq!(
        reached.len(),
        1,
        "B is folded exactly once (no double-fold across the pump + leave)"
      );
    }
    other => panic!("expected Ok(reached) including B, got {other:?}"),
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the resolved join is reaped after delivery"
  );
}

/// A never-polled terminal join clears its MACHINE state IN THE PUMP on the
/// exchange terminal, retaining only the small caller-result entry until
/// delivery/cancel. The seed's dial fails synchronously inside the pump, so that
/// same pump folds the terminal `ExchangeCompleted(Failed)` and clears the recorded
/// `ignore_old` stream — with NO `poll_event` and NO `poll_join` — so the machine's
/// ignore set never leaks, while the resolved-but-undelivered result entry lingers
/// (it is NOT dropped out from under a caller that has not yet retrieved it).
/// `cancel_join` is the give-up that reaps that entry. Reverting the pump fold
/// leaves the token recorded after the pump (nothing drained events to fold it) and
/// the reply never resolves.
#[test]
fn dropped_never_polled_join_clears_machine_ignore_on_terminal_then_cancel_reaps() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  engine.plane_mut().pool.push(5);
  engine.set_listener(9);

  let handle = engine
    .join(&[node_addr(7002)], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  // One pump dispatches the seed (recording its ignore stream), captures the
  // Connect, fails the dial (`NoStream::connect` errors) — queuing the terminal
  // ExchangeCompleted(Failed) — AND folds that completion in-pump, clearing the
  // ignore stream. No poll_event / poll_join is ever called.
  engine.pump(now, &mut gossip, &mut stream);
  let sid = {
    let pj = &engine.pending_joins[&handle];
    assert_eq!(
      pj.started.len(),
      1,
      "the ignore_old seed recorded its StreamId"
    );
    *pj.started.iter().next().expect("one started stream")
  };
  // The MACHINE state is cleared IN THE PUMP on the exchange terminal — no
  // machine-ignore leak — with no caller polling at all.
  assert!(
    !engine.endpoint.test_has_ignore_join_stream(sid),
    "the pump must clear the ignore stream on the exchange terminal (no machine leak)"
  );
  // The small caller-result entry is RETAINED until delivery/cancel: it resolved
  // `Ready` in the pump but was never delivered, so the result is not lost by a
  // never-polled reap.
  assert_eq!(
    engine.pending_join_count(),
    1,
    "the resolved-but-undelivered entry is retained until poll_join or cancel_join"
  );
  assert!(
    matches!(engine.pending_joins[&handle].reply, JoinReply::Ready(_)),
    "its reply resolved Ready in the pump, awaiting delivery"
  );

  // cancel_join is the supported give-up for a dropped handle: it reaps the retained
  // terminal entry (its exchanges already terminal, ignore set already cleared).
  engine.cancel_join(handle);
  assert_eq!(
    engine.pending_join_count(),
    0,
    "cancel_join reaps the retained terminal entry (no pending-join leak)"
  );
}

/// `cancel_join` forgets an in-flight await-result join leak-free: the caller's
/// reply is discarded (a later `poll_join` yields nothing), the ignore token is
/// RETAINED while the push/pull can still merge, and once the exchange terminates
/// the pump clears that token and reaps the waiter — no pending-join or ignore-set
/// leak, and no further poll.
#[test]
fn cancel_join_forgets_in_flight_join_leak_free() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");
  // One step dispatches the seed and binds the exchange IN FLIGHT (its ignore token
  // recorded, not yet terminal).
  link.step(now);
  let sid = {
    let pj = &link.a.pending_joins[&handle];
    assert!(
      !pj.pending.is_empty(),
      "its push/pull exchange is bound and in flight"
    );
    *pj.started.iter().next().expect("one started stream")
  };
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token is recorded while the exchange is in flight"
  );

  // The caller gives up: cancel_join forgets the reply but RETAINS the ignore token
  // (the exchange can still merge).
  link.a.cancel_join(handle);
  assert!(
    link.a.poll_join(handle).is_none(),
    "a cancelled join yields no outcome to the caller"
  );
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "cancel_join must NOT clear the ignore token while the exchange can still merge"
  );

  // Driving the exchange to its terminal clears the ignore token AND reaps the
  // forgotten waiter — no poll, no leak.
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if link.a.pending_join_count() == 0 {
      break;
    }
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "a cancelled join is reaped once its exchange terminates (no pending-join leak)"
  );
  assert!(
    !link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token is cleared on the exchange terminal (no machine leak)"
  );
}

/// A resolved-but-undelivered join result survives a `pump` that runs between its
/// resolution and the caller's `poll_join`. The push/pull succeeds and its
/// completion is FOLDED (via `poll_event`) into a `Ready(Ok(..))` reply without the
/// caller polling; a further `pump` then runs the end-of-pump join sweep, which
/// must RETAIN the undelivered result. Regression for a one-tick / exchange-work-
/// done reap that dropped the result under a slow/async waiter — reverting it makes
/// the poll below observe `None`.
#[test]
fn join_result_survives_pump_after_resolve_until_polled() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  // Drive the push/pull to its terminal and FOLD the Succeeded completion (via
  // poll_event) WITHOUT ever calling poll_join, so the reply resolves to
  // `Ready(Ok(..))` yet is not delivered to the caller.
  let mut resolved = false;
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if matches!(
      link.a.pending_joins.get(&handle).map(|pj| &pj.reply),
      Some(JoinReply::Ready(_))
    ) {
      resolved = true;
      break;
    }
  }
  assert!(
    resolved,
    "the join must resolve Ready off the folded Succeeded push/pull"
  );

  // Pump A ONCE MORE before the caller polls. The end-of-pump join sweep must
  // RETAIN a resolved-but-undelivered result (a one-tick / exchange-work-done reap
  // would drop it here, so the poll below would see None).
  link.a.pump(now, &mut link.a_gossip, &mut link.a_rel);

  // The successful result is STILL retrievable after the extra pump.
  match link.a.poll_join(handle) {
    Some(Ok(reached)) => assert!(
      reached.contains(&link.b_addr),
      "the reached set retained across the extra pump must include B"
    ),
    other => {
      panic!("a resolved join result must survive a pump before the caller polls, got {other:?}")
    }
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the delivered join is reaped once the caller retrieves its result"
  );
}

/// A `join` immediately cancelled BEFORE any pump dispatches no push/pull: its
/// queued seed is removed, so the pump initiates nothing for the handle, and the
/// waiter is reaped at once with zero network side effect. Regression for a
/// `cancel_join` that left un-dispatched seeds queued — reverting it lets the pump
/// dial the cancelled seed (it parks as a PendingDial) and the join reappear.
#[test]
fn cancel_before_pump_dispatches_no_seed_and_reaps() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  // A listener but an empty dial pool: a dispatched seed's Connect parks as
  // PendingDial, so `pending_dial_count` is a precise "a push/pull was started"
  // probe.
  engine.set_listener(9);

  let handle = engine
    .join(&[node_addr(7002)], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");
  assert_eq!(engine.pending_seeds.len(), 1, "the routable seed is queued");
  assert_eq!(
    engine.pending_join_count(),
    1,
    "the in-flight join is tracked"
  );

  // Cancel BEFORE any pump: the queued seed is removed and — no exchange having
  // started — the waiter is reaped immediately.
  engine.cancel_join(handle);
  assert!(
    engine.pending_seeds.is_empty(),
    "cancel_join must drop the never-dispatched seed so the pump initiates no push/pull"
  );
  assert_eq!(
    engine.pending_join_count(),
    0,
    "a cancel-before-pump forgets everything and reaps the waiter immediately"
  );

  // The pump now dispatches nothing for that handle: no push/pull is started, so no
  // exchange parks as PendingDial (a buggy cancel that left the seed queued would
  // dial it here).
  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.pending_dial_count(),
    0,
    "no push/pull may be dispatched for a seed cancelled before the pump"
  );
  assert_eq!(
    engine.pending_join_count(),
    0,
    "no join reappears after the pump"
  );
}

// ── pump-driven join resolution (fold in the pump, not poll_event) ────────────

/// A successful join resolves via the PUMP alone: drive the push/pull to
/// `ExchangeCompleted(Succeeded)` and poll the join after each pump WITHOUT ever
/// calling A's `poll_event`. The pump folds the completion into the join, so
/// `poll_join` returns `Ok(reached)` including B. This is the core of the fix —
/// reverting the pump fold (folding only in `poll_event`) makes `poll_join` return
/// `None` forever here, since A's events are never drained.
#[test]
fn pump_without_poll_event_resolves_successful_join() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut outcome = None;
  for _ in 0..40 {
    // Pump both engines and ferry gossip/fabric. Drain ONLY B's events; A's
    // `poll_event` is NEVER called, so only the pump can fold A's join.
    link.step(now);
    while link.b.poll_event().is_some() {}
    if let Some(res) = link.a.poll_join(handle) {
      outcome = Some(res);
      break;
    }
  }

  match outcome {
    Some(Ok(reached)) => assert!(
      reached.contains(&link.b_addr),
      "the reached set folded by the pump must include B"
    ),
    other => {
      panic!("the pump alone must resolve the join Ok(reached) with no poll_event, got {other:?}")
    }
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the resolved + delivered join is reaped"
  );
}

/// A cancelled in-flight join whose started exchange later reaches
/// `ExchangeCompleted` is reaped — entry AND ignore token — by the PUMP alone, with
/// A's `poll_event` never drained. `cancel_join` forgets the reply while the
/// exchange is in flight; driving it to its terminal via pumps then clears the
/// ignore token and reaps the waiter in-pump. Reverting the pump fold leaks both
/// (the terminal never folds without a `poll_event`).
#[test]
fn cancelled_in_flight_join_reaped_by_pump_without_poll_event() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], /*ignore_old*/ true, now)
    .expect("join announces intent and mints a handle");
  // One step dispatches the seed and binds the exchange IN FLIGHT.
  link.step(now);
  let sid = {
    let pj = &link.a.pending_joins[&handle];
    assert!(
      !pj.pending.is_empty(),
      "its push/pull exchange is bound and in flight"
    );
    *pj.started.iter().next().expect("one started stream")
  };
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token is recorded while the exchange is in flight"
  );

  // The caller gives up mid-flight: cancel forgets the reply but RETAINS the token.
  link.a.cancel_join(handle);
  assert!(
    link.a.endpoint.test_has_ignore_join_stream(sid),
    "cancel_join retains the token while the exchange can still merge"
  );

  // Drive the exchange to its terminal via pumps, NEVER draining A's events. The
  // pump folds the terminal completion, clears the token, and reaps the waiter.
  for _ in 0..40 {
    link.step(now);
    while link.b.poll_event().is_some() {}
    if link.a.pending_join_count() == 0 {
      break;
    }
  }
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the cancelled join is reaped by the pump (no pending-join leak, no poll_event)"
  );
  assert!(
    !link.a.endpoint.test_has_ignore_join_stream(sid),
    "the ignore token is cleared by the pump on the exchange terminal (no machine leak)"
  );
}

/// After the pump folds a join's completion, `poll_event` STILL delivers every
/// event — the push/pull `ExchangeCompleted` and the membership changes — exactly
/// once. Folding in the pump must not consume, drop, or duplicate the app's event
/// stream: the completion is buffered (not swallowed by the fold) and the
/// membership events flow through the same buffer. Regression for a fold that
/// consumed the event or a buffer that dropped/duplicated it.
#[test]
fn poll_event_delivers_all_events_exactly_once_after_pump_fold() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut a_events: std::vec::Vec<Event<SmolStr, SocketAddr>> = std::vec::Vec::new();
  let mut resolved: Option<Result<ReachedSet, JoinFailed>> = None;
  for _ in 0..40 {
    link.step(now);
    while link.b.poll_event().is_some() {}
    // The pump already folded any terminal completion; draining here collects the
    // events the app observes — the completion MUST still be among them.
    while let Some(ev) = link.a.poll_event() {
      a_events.push(ev);
    }
    if resolved.is_none()
      && let Some(res) = link.a.poll_join(handle)
    {
      resolved = Some(res);
    }
    // Stop once the join resolved AND its completion has been delivered to the app.
    if resolved.is_some()
      && a_events
        .iter()
        .any(|ev| matches!(ev, Event::ExchangeCompleted(ec) if ec.kind() == ExchangeKind::PushPull))
    {
      break;
    }
  }

  // The join resolved Ok off the pump-folded state.
  match &resolved {
    Some(Ok(reached)) => assert!(
      reached.contains(&link.b_addr),
      "the join resolved Ok(reached) including B"
    ),
    other => panic!("the join must resolve Ok(reached), got {other:?}"),
  }

  // The push/pull ExchangeCompleted was delivered to the app EXACTLY ONCE, even
  // though the pump folded it (the fold buffers, never consumes).
  let completions = a_events
    .iter()
    .filter(|ev| matches!(ev, Event::ExchangeCompleted(ec) if ec.kind() == ExchangeKind::PushPull))
    .count();
  assert_eq!(
    completions, 1,
    "the push/pull ExchangeCompleted must be delivered via poll_event exactly once"
  );
  // Membership changes flow through the same buffer (A learned B).
  assert!(
    a_events.iter().any(|ev| matches!(ev, Event::Member(_))),
    "membership events must also be delivered through the buffer"
  );
}

// ── bounded app-event backlog (no unbounded growth) ───────────────────────────

/// A driver that pumps and resolves joins but NEVER drains `poll_event` must not
/// grow the app-event backlog without bound: `buffered_events` is capped at
/// [`DEFAULT_EVENT_BUFFER_CAP`], surplus events are shed OLDEST-first and counted in
/// `events_dropped()`, and — because join completions are folded BEFORE buffering —
/// the shed app-events never affect join resolution (`poll_join` still returns
/// `Ok(reached)`). Regression for an unbounded `buffered_events` that OOMs a
/// long-running embedded node under the supported pump-then-poll_join flow; reverting
/// the bound lets the backlog grow to `2 * cap` and leaves `events_dropped()` at 0.
#[test]
fn undrained_poll_event_backlog_is_bounded_and_counts_drops() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  // Drive the push/pull to its Succeeded terminal by pumping BOTH engines, draining
  // ONLY B's events. A's `poll_event` is NEVER called, so the pump alone folds A's
  // join completion and every event A observes piles into `buffered_events`.
  let mut folded = false;
  for _ in 0..40 {
    link.step(now);
    while link.b.poll_event().is_some() {}
    if matches!(
      link.a.pending_joins.get(&handle).map(|pj| &pj.reply),
      Some(JoinReply::Ready(_))
    ) {
      folded = true;
      break;
    }
  }
  assert!(
    folded,
    "the pump must fold the Succeeded push/pull into the join with no poll_event drain"
  );

  // Flood the app-event backlog far past the cap, still WITHOUT draining `poll_event`.
  // This exercises the exact bounded push the pump's `drain_fold_events` uses.
  for _ in 0..(DEFAULT_EVENT_BUFFER_CAP * 2) {
    link.a.push_app_event(Event::LeftCluster);
  }

  // The backlog is BOUNDED — it never exceeds the cap however long the app ignores
  // it — and every shed event is counted, so the loss is observable rather than an
  // unbounded memory leak.
  assert_eq!(
    link.a.buffered_events.len(),
    DEFAULT_EVENT_BUFFER_CAP,
    "the undrained app-event backlog must be bounded at the cap, never growing without limit"
  );
  assert!(
    link.a.events_dropped() >= DEFAULT_EVENT_BUFFER_CAP as u64,
    "every shed event must be counted in events_dropped (got {})",
    link.a.events_dropped()
  );

  // Join accounting is folded before buffering, so the shed app-events never affect
  // resolution: the join still resolves `Ok(reached)` with B.
  match link.a.poll_join(handle) {
    Some(Ok(reached)) => assert!(
      reached.contains(&link.b_addr),
      "the join still resolves Ok(reached) including B despite the app-event drops"
    ),
    other => panic!("the bounded app-event drop must not affect join resolution, got {other:?}"),
  }
}

// ── mandatory control events survive an observation flood (non-lossy path) ────

/// A MANDATORY driver-actioned event survives an observation flood far exceeding the
/// passive backlog cap. Routed through the SAME per-event path the pump uses
/// ([`SerfEngine::route_drained_event`]), a `Event::Shutdown` lands on the non-lossy
/// control queue while `> DEFAULT_EVENT_BUFFER_CAP` passive `Event::LeftCluster`
/// observations flood the bounded buffer. `poll_event` must STILL yield the Shutdown
/// (delivered ahead of observations, never evicted), the passive backlog must be
/// bounded at the cap, and `events_dropped()` must count the shed observations.
///
/// This is the fail-on-revert regression: reverting to a single lossy queue routes
/// the Shutdown through the drop-oldest buffer, where the over-cap flood evicts it
/// before any driver could act on it — so the final assertion fails. `Shutdown` is
/// the only mandatory variant constructible from a downstream crate (`KeyRequest` /
/// `DialRequested` carry `pub(crate)` payloads and, on the stream path, the
/// coordinator sieves the dial into a `poll_action` `Connect`), and all three route
/// through the identical `is_mandatory_event` → non-lossy control path exercised
/// here.
#[test]
fn mandatory_event_survives_observation_flood() {
  let mut engine = make_engine();

  // The mandatory event is enqueued FIRST, then a flood of passive observations far
  // past the cap — so a single lossy queue (drop-oldest) would evict the Shutdown.
  engine.route_drained_event(Event::Shutdown);
  for _ in 0..(DEFAULT_EVENT_BUFFER_CAP * 2) {
    engine.route_drained_event(Event::LeftCluster);
  }

  // The passive backlog is bounded at the cap and every shed observation is counted.
  assert_eq!(
    engine.buffered_events.len(),
    DEFAULT_EVENT_BUFFER_CAP,
    "the passive observation backlog must be bounded at the cap"
  );
  assert!(
    engine.events_dropped() >= 1,
    "the over-cap observation flood must shed and count observations (got {})",
    engine.events_dropped()
  );

  // The mandatory Shutdown is delivered FIRST and was never evicted by the flood.
  assert!(
    matches!(engine.poll_event(), Some(Event::Shutdown)),
    "the mandatory Shutdown must survive the flood and lead the observations"
  );

  // Nothing after it is a Shutdown (only one is ever queued), and exactly the bounded
  // passive backlog remains.
  let mut remaining = 0usize;
  while let Some(ev) = engine.poll_event() {
    remaining += 1;
    assert!(
      !matches!(ev, Event::Shutdown),
      "only one Shutdown is ever queued; it was already delivered first"
    );
  }
  assert_eq!(
    remaining, DEFAULT_EVENT_BUFFER_CAP,
    "after the Shutdown, exactly the bounded passive backlog is delivered"
  );
}

/// The non-lossy control queue dedupes the idempotent-terminal `Event::Shutdown` — a
/// repeated conflict signal queues at most one — and delivers control events ahead of
/// passive observations regardless of arrival order. Together these keep the
/// unbounded control queue from being grown by a duplicated terminal signal and
/// guarantee a mandatory action is never delayed behind queued observations.
#[test]
fn control_queue_dedupes_shutdown_and_leads_observations() {
  let mut engine = make_engine();

  // Observations arrive first, then several Shutdowns.
  engine.route_drained_event(Event::LeftCluster);
  engine.route_drained_event(Event::LeftCluster);
  engine.route_drained_event(Event::Shutdown);
  engine.route_drained_event(Event::Shutdown);
  engine.route_drained_event(Event::Shutdown);

  assert_eq!(
    engine.control_events.len(),
    1,
    "repeated Shutdowns dedupe to a single queued terminal signal"
  );

  // Control-first: the Shutdown leads despite the earlier-enqueued observations, then
  // the two observations follow in arrival order.
  assert!(
    matches!(engine.poll_event(), Some(Event::Shutdown)),
    "the mandatory Shutdown is delivered ahead of the earlier observations"
  );
  assert!(
    matches!(engine.poll_event(), Some(Event::LeftCluster)),
    "the buffered observations follow the control event, in arrival order"
  );
  assert!(matches!(engine.poll_event(), Some(Event::LeftCluster)));
  assert!(
    engine.poll_event().is_none(),
    "both queues are drained after delivering the control event and observations"
  );
}

// ── mandatory control queue bounded by KeyRequest liveness (encrypted flood) ──

/// Build an `Event::KeyRequest` with a distinct `id` and an explicit response
/// `deadline`, carrying real key material so a prune demonstrably releases it. This
/// mirrors the endpoint's own emission — one `Event::KeyRequest` whose `deadline`
/// equals its registered `received_queries` entry — via the serf-proto
/// `test-support` constructor (the wire fields are `pub(crate)`, so a downstream
/// test cannot build one directly).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn key_request(id: u32, deadline: Instant) -> Event<SmolStr, SocketAddr> {
  let from = memberlist_proto::Node::new(SmolStr::new("flooder"), node_addr(6000));
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([0x11u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([0x11u8; 32]);
  Event::KeyRequest(KeyRequest::test_with_deadline(
    KeyRequestOperation::Install,
    id,
    from,
    Some(key),
    deadline,
  ))
}

/// An encrypted peer flooding distinct `KeyRequest`s across successive deadline
/// windows cannot grow the non-lossy control queue without bound, even when the
/// driver pumps and resolves joins but NEVER drains `poll_event`. Each window's
/// batch is routed to `control_events`, then `now` advances past its deadline and a
/// `pump` prunes it (dead + unanswerable) — so the queue holds at most one live
/// window's worth, never `windows * batch`.
///
/// This is the fail-on-revert regression: without the pump's deadline-prune the
/// queue accumulates every window's batch, so the `<= BATCH` bound fails on the
/// second window (and the heap grows without limit under a sustained flood).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_request_flood_across_deadline_windows_is_bounded_without_poll_event() {
  let mut engine = make_engine();
  let base = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(base);

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);
  // Settle the construction self-join; its events are passive (buffered_events),
  // never mandatory, so they cannot enter control_events.
  engine.pump(base, &mut gossip, &mut stream);

  const BATCH: usize = 64;
  const WINDOWS: usize = 8;
  const WINDOW: Duration = Duration::from_secs(10);

  let mut id = 0u32;
  let mut now = base;
  let mut peak = 0usize;
  for w in 0..WINDOWS {
    // This window's KeyRequests are live until `now + WINDOW`.
    let deadline = now + WINDOW;
    for _ in 0..BATCH {
      engine.route_drained_event(key_request(id, deadline));
      id += 1;
    }
    peak = peak.max(engine.control_events.len());

    // Advance PAST this window's deadline and pump — WITHOUT draining poll_event —
    // so the pump's prune sheds this now-dead batch.
    now = deadline + Duration::from_secs(1);
    engine.pump(now, &mut gossip, &mut stream);

    assert!(
      engine.control_events.len() <= BATCH,
      "control queue must stay bounded to one live window, got {} at window {w}",
      engine.control_events.len()
    );
  }

  // The queue never accumulated across windows: its peak is one batch, not
  // WINDOWS * BATCH, and every window's expired batch is gone.
  assert!(
    peak <= BATCH,
    "control queue peaked at {peak}, exceeding a single {BATCH}-request window"
  );
  assert!(
    engine.control_events.is_empty(),
    "after every window's deadline passed, no KeyRequest remains queued"
  );
}

/// A LIVE (future-deadline) `KeyRequest` is never pruned: a pump at a `now` before
/// its deadline retains it, and `poll_event` still delivers it (mandatory events
/// lead the observation stream). Guards the prune boundary — `now < deadline` keeps
/// it — so the liveness bound never sheds an answerable request.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn live_key_request_is_never_pruned_and_is_delivered() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);

  // Deadline well in the future; the pump runs at `now` (still in-window).
  engine.route_drained_event(key_request(7, now + Duration::from_secs(30)));
  engine.pump(now, &mut gossip, &mut stream);

  assert!(
    matches!(engine.poll_event(), Some(Event::KeyRequest(_))),
    "a live (in-deadline) KeyRequest must survive the pump and be delivered by poll_event"
  );
}

/// A past-deadline `KeyRequest` is pruned by the pump — releasing the raw key
/// material pinned in its payload — and is never surfaced to the driver. Reverting
/// the prune leaves it queued (payload retained), failing the assertions below.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn past_deadline_key_request_is_pruned_and_payload_released() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);

  let deadline = now + Duration::from_secs(5);
  engine.route_drained_event(key_request(9, deadline));
  assert_eq!(
    engine.control_events.len(),
    1,
    "the KeyRequest (holding its key payload) is queued on the control path"
  );

  // Advance PAST its deadline and pump: the prune drops the dead request, dropping
  // the queue's sole reference to its key material.
  let later = deadline + Duration::from_secs(1);
  engine.pump(later, &mut gossip, &mut stream);

  assert!(
    !engine
      .control_events
      .iter()
      .any(|ev| matches!(ev, Event::KeyRequest(_))),
    "a past-deadline KeyRequest must be pruned from the control queue (its payload released)"
  );
  // It is dropped, not deferred: poll_event never surfaces the pruned request.
  while let Some(ev) = engine.poll_event() {
    assert!(
      !matches!(ev, Event::KeyRequest(_)),
      "a pruned past-deadline KeyRequest must never surface via poll_event"
    );
  }
}

/// A `KeyRequest` at EXACTLY its response `deadline` is still answerable —
/// `respond_key` rejects only `now > deadline`, so a `respond_key` at the exact
/// instant `now == deadline` succeeds. The pump's prune must therefore RETAIN it
/// at `now == deadline`, and `poll_event` must still deliver it. Guards the
/// inclusive prune boundary: dropping it at `now == deadline` would shed a live
/// mandatory key request out from under a response the originator could still
/// send. Reverting the predicate to `now >= deadline` prunes it here, so
/// `poll_event` no longer delivers the request and this fails.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_request_at_exact_deadline_survives_prune_and_is_delivered() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);

  // Deadline equal to the pump instant: the request is answerable at exactly D
  // (respond_key rejects only now > D), so the prune must keep it.
  let deadline = now + Duration::from_secs(5);
  engine.route_drained_event(key_request(11, deadline));
  engine.pump(deadline, &mut gossip, &mut stream);

  assert!(
    matches!(engine.poll_event(), Some(Event::KeyRequest(_))),
    "a KeyRequest at exactly its deadline is still answerable, so the prune must \
     retain it and poll_event must deliver it"
  );
}

// ── key-management applied to the LIVE wire keyring ───────────────────────────

/// A fixed AEAD key filled with `fill`, in whichever backend is compiled.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  {
    SecretKey::Aes256([fill; 32])
  }
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  {
    SecretKey::ChaCha20Poly1305([fill; 32])
  }
}

/// A `SerfEngine` whose gossip and reliable planes encrypt under `keyring`.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn make_encrypted_engine(id: &str, port: u16, keyring: Keyring) -> SerfEngine<SmolStr, u32> {
  let cfg = Options::new()
    .with_port(port)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new(id), node_addr(port));
  let transform =
    TransformOptions::default().with_encryption(EncryptionOptions::new().with_keyring(keyring));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  SerfEngine::try_new_at(cfg, transform, ep_cfg, SerfOptions::new(), now, test_rng())
    .expect("valid encrypted configuration must construct")
}

/// A `KeyRequest` carrying `op` and `key` from a synthetic originator, with a
/// future deadline (the mutation path ignores the deadline; `respond_key` uses it).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn key_op(op: KeyRequestOperation, key: Option<SecretKey>) -> KeyRequest<SmolStr, SocketAddr> {
  let from = memberlist_proto::Node::new(SmolStr::new("op"), node_addr(6000));
  let deadline = Instant::from_origin(Duration::from_secs(86_400)) + Duration::from_secs(30);
  KeyRequest::test_with_deadline(op, 1, from, key, deadline)
}

/// `apply_key_request` mutates the engine's LIVE wire keyring (the coordinator's,
/// not a driver-held shadow): install adds a secondary leaving the primary intact,
/// use promotes it, remove drops a secondary, removing the current primary is
/// refused with the ring unchanged, and list snapshots the live post-op state.
/// Every op's effect is visible through the `keyring()` accessor — the single
/// source of truth. `handle_key_request` then applies-then-responds, so a valid op
/// lands on the live keyring even when the response cannot be routed.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_ops_mutate_the_live_keyring() {
  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);
  let mut engine = make_encrypted_engine("a", 7946, Keyring::new(k1));

  // Baseline: primary K1, no secondaries.
  assert_eq!(engine.keyring().expect("encrypted").primary_ref(), &k1);
  assert!(engine.keyring().unwrap().secondaries().is_empty());

  // install K2 -> K2 becomes a secondary; the primary is untouched.
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Install, Some(k2)));
  assert!(resp.result, "install must succeed");
  {
    let kr = engine.keyring().expect("encrypted");
    assert_eq!(kr.primary_ref(), &k1, "install must not move the primary");
    assert!(
      kr.secondaries().contains(&k2),
      "install must add K2 as a secondary"
    );
  }

  // use K2 -> K2 is promoted to primary.
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Use, Some(k2)));
  assert!(resp.result, "use must succeed");
  assert_eq!(
    engine.keyring().unwrap().primary_ref(),
    &k2,
    "use must promote K2 to primary"
  );

  // remove K1 (now a secondary) -> gone from the ring.
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Remove, Some(k1)));
  assert!(resp.result, "remove of a secondary must succeed");
  {
    let kr = engine.keyring().expect("encrypted");
    assert_eq!(kr.primary_ref(), &k2);
    assert!(
      !kr.secondaries().contains(&k1),
      "remove must drop K1 from the ring"
    );
  }

  // remove the CURRENT primary (K2) -> refused; the ring is unchanged.
  let before_primary = *engine.keyring().unwrap().primary_ref();
  let before_secondaries = engine.keyring().unwrap().secondaries().to_vec();
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Remove, Some(k2)));
  assert!(!resp.result, "removing the current primary must be refused");
  {
    let kr = engine.keyring().expect("encrypted");
    assert_eq!(
      *kr.primary_ref(),
      before_primary,
      "a refused remove must not move the primary"
    );
    assert_eq!(
      kr.secondaries(),
      before_secondaries.as_slice(),
      "a refused remove must not change the ring"
    );
  }

  // list -> reports the live primary and every installed key; no wire change.
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::List, None));
  assert!(resp.result, "list must succeed");
  assert_eq!(resp.primary_key, Some(k2), "list reports the live primary");
  assert!(
    resp.keys.contains(&k2),
    "list reports the primary among the keys"
  );

  // handle_key_request composes apply-then-respond: the op lands on the live
  // keyring even though the synthetic originator is unroutable (respond is
  // best-effort, so its Result is not asserted here).
  let k3 = secret_key(0x33);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let _ = engine.handle_key_request(&key_op(KeyRequestOperation::Install, Some(k3)), now);
  assert!(
    engine.keyring().unwrap().secondaries().contains(&k3),
    "handle_key_request must apply the op to the live keyring regardless of respond routing"
  );
}

/// The wire proof: after a rotation applied through the live-keyring path, the
/// engine's gossip crypto runs under the NEW primary and REJECTS a frame under the
/// removed key. A rotates to K2 and drops K1; a peer B holding K2 decrypts A's
/// post-rotation frame (rotated traffic flows), while a frame captured under K1 no
/// longer decrypts on A (old-key traffic rejected). Driven through the real
/// `encrypt_gossip` / `decrypt_gossip` paths, not keyring-state asserts alone.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_rotation_reencrypts_gossip_and_rejects_removed_key() {
  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);
  // A starts {primary K1, secondary K2}; B holds only K2.
  let mut a = make_encrypted_engine("a", 7946, Keyring::with_secondaries(k1, [k2]));
  let b = make_encrypted_engine("b", 7947, Keyring::new(k2));

  let plaintext = b"gossip-probe-payload-0123456789";

  // A frame A emits under its CURRENT primary (K1), captured before the rotation.
  let under_k1 = a
    .endpoint
    .encrypt_gossip(plaintext)
    .expect("encrypt under K1");
  assert_eq!(
    a.endpoint
      .decrypt_gossip(&under_k1)
      .expect("A decrypts its own K1 frame pre-rotation"),
    plaintext
  );

  // Rotate A to K2 and drop K1 through the single live-keyring chokepoint.
  assert!(
    a.apply_key_request(&key_op(KeyRequestOperation::Use, Some(k2)))
      .result
  );
  assert!(
    a.apply_key_request(&key_op(KeyRequestOperation::Remove, Some(k1)))
      .result
  );

  // Rotated traffic flows: A now encrypts under K2, and B (holding K2) decrypts it.
  let under_k2 = a
    .endpoint
    .encrypt_gossip(plaintext)
    .expect("encrypt under K2");
  assert_eq!(
    b.endpoint
      .decrypt_gossip(&under_k2)
      .expect("a peer holding the new key decrypts A's rotated gossip"),
    plaintext,
    "post-rotation gossip must decrypt under the promoted key"
  );

  // Old-key traffic rejected: A dropped K1, so a frame under K1 no longer decrypts.
  assert!(
    a.endpoint.decrypt_gossip(&under_k1).is_err(),
    "a frame under the removed key K1 must be rejected by the rotated engine"
  );
}

// ── Cross-cipher (dual-backend) ambiguity regressions ────────────────────────
//
// These need BOTH AEAD backends so an AES-256 key and a ChaCha20-Poly1305 key can
// share the same 32 raw bytes yet be DISTINCT keys — the twin the byte-keyed
// coordinator ops (`promote`/`remove_secondary`) cannot tell apart. Gated on both
// features accordingly.

/// A fixed AES-256-GCM key filled with `fill`.
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn aes256(fill: u8) -> SecretKey {
  SecretKey::Aes256([fill; 32])
}

/// A fixed ChaCha20-Poly1305 key filled with `fill` — the cross-cipher byte twin of
/// `aes256(fill)` (identical bytes, different variant, so `!=` under `SecretKey`'s
/// variant-inclusive equality).
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn chacha(fill: u8) -> SecretKey {
  SecretKey::ChaCha20Poly1305([fill; 32])
}

/// Fallible sibling of [`make_encrypted_engine`] that surfaces the construction
/// `InitError` instead of panicking, for the construction-preflight assertion.
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn try_make_encrypted_engine(
  id: &str,
  port: u16,
  keyring: Keyring,
) -> Result<SerfEngine<SmolStr, u32>, InitError> {
  let cfg = Options::new()
    .with_port(port)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new(id), node_addr(port));
  let transform =
    TransformOptions::default().with_encryption(EncryptionOptions::new().with_keyring(keyring));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  SerfEngine::try_new_at(cfg, transform, ep_cfg, SerfOptions::new(), now, test_rng())
}

/// The chokepoint is variant-exact: a key op naming one cipher can never touch a
/// byte-twin of another cipher, and installing a twin is refused — while exact
/// (variant + bytes) ops still apply. Every wrong-variant assertion fails on the
/// byte-keyed revert (which would promote/remove/install the wrong cipher's key
/// and report success).
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn cross_cipher_key_ops_are_variant_exact() {
  let k1 = aes256(0x11); // primary
  let x_aes = aes256(0x22); // the AES twin, installed as a secondary
  let x_chacha = chacha(0x22); // its cross-cipher byte twin (same bytes, ChaCha)
  let mut engine = make_encrypted_engine("a", 7946, Keyring::with_secondaries(k1, [x_aes]));
  let before = engine.keyring().unwrap().secondaries().to_vec();

  // use(ChaCha(X)) must NOT promote the byte-twin AES256(X): the exact ChaCha key is
  // absent. Refused, ring unchanged. (Revert: promotes AES256(X), reports success.)
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Use, Some(x_chacha)));
  assert!(
    !resp.result,
    "use of an absent cross-cipher twin must be refused"
  );
  assert_eq!(
    engine.keyring().unwrap().primary_ref(),
    &k1,
    "a refused use must not move the primary"
  );
  assert_eq!(
    engine.keyring().unwrap().secondaries(),
    before.as_slice(),
    "a refused use must not change the ring"
  );

  // remove(ChaCha(X)) must NOT drop the byte-twin AES256(X). (Revert: removes it.)
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Remove, Some(x_chacha)));
  assert!(
    !resp.result,
    "remove of an absent cross-cipher twin must be refused"
  );
  assert!(
    engine.keyring().unwrap().secondaries().contains(&x_aes),
    "the AES twin must remain installed after a refused cross-cipher remove"
  );

  // install(ChaCha(X)) while AES256(X) is present is a cross-cipher collision:
  // refused, ring unchanged. (Revert: inserts ChaCha(X), producing an ambiguous
  // twin ring and reporting success.)
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::Install, Some(x_chacha)));
  assert!(
    !resp.result,
    "installing a cross-cipher byte twin must be refused"
  );
  assert!(
    !engine.keyring().unwrap().secondaries().contains(&x_chacha),
    "a refused cross-cipher install must not add the twin"
  );
  assert_eq!(
    engine.keyring().unwrap().secondaries(),
    before.as_slice(),
    "a refused cross-cipher install must not change the ring"
  );

  // Exact-variant ops still apply. Install a genuinely new ChaCha key (no byte twin
  // present), promote it by its exact variant, and remove the AES twin by ITS true
  // variant — every one succeeds.
  let z_chacha = chacha(0x33);
  assert!(
    engine
      .apply_key_request(&key_op(KeyRequestOperation::Install, Some(z_chacha)))
      .result,
    "installing a non-colliding ChaCha key must succeed"
  );
  assert!(engine.keyring().unwrap().secondaries().contains(&z_chacha));
  assert!(
    engine
      .apply_key_request(&key_op(KeyRequestOperation::Use, Some(z_chacha)))
      .result,
    "promoting the exact ChaCha key must succeed"
  );
  assert_eq!(
    engine.keyring().unwrap().primary_ref(),
    &z_chacha,
    "the exact ChaCha key must become the primary"
  );
  assert!(
    engine
      .apply_key_request(&key_op(KeyRequestOperation::Remove, Some(x_aes)))
      .result,
    "removing the AES key by its true variant must succeed"
  );
  assert!(
    !engine.keyring().unwrap().secondaries().contains(&x_aes),
    "the AES key must be gone after an exact-variant remove"
  );
}

/// Construction refuses a seed keyring that already carries a cross-cipher byte
/// twin, so an ambiguous ring can never reach the running chokepoint. Fails on the
/// revert: without the preflight the ring is individually usable and constructs.
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn construction_rejects_a_keyring_with_cross_cipher_byte_twins() {
  // AES256(X) primary + ChaCha20-Poly1305(X) secondary: identical bytes, different
  // ciphers — an ambiguous ring for the byte-keyed rotation ops.
  let keyring = Keyring::with_secondaries(aes256(0x11), [chacha(0x11)]);
  match try_make_encrypted_engine("a", 7946, keyring) {
    Err(InitError::Memberlist(crate::MemberlistInitError::Encryption(_))) => {}
    Err(other) => {
      panic!("expected InitError::Encryption for a cross-cipher twin keyring, got {other:?}")
    }
    Ok(_) => panic!("construction must reject a keyring carrying cross-cipher byte twins"),
  }
}

// ── panicking convenience constructors ───────────────────────────────────────

/// The first query id a running engine issues — a `u32` drawn straight from serf's
/// core RNG, so with every other input held fixed it is a pure function of that
/// RNG's seed.
fn first_query_id_of(engine: &mut SerfEngine<SmolStr, u32>, now: Instant) -> u32 {
  engine
    .query(
      "probe",
      Bytes::from_static(b"payload"),
      QueryParams::default(),
      now,
    )
    .expect("a query is issued while running")
    .id
}

/// `new_at_with_rng` — the panicking form of `try_new_at_with_rng` — builds on a
/// valid configuration AND injects serf's core RNG: with the gossip RNG held fixed,
/// distinct serf seeds yield distinct first query ids and the same seed reproduces
/// one. So the production constructor's entropy actually reaches query-id
/// generation through the panicking wrapper too.
#[test]
fn new_at_with_rng_builds_and_injects_the_serf_rng() {
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let build = |serf_seed: u64| -> SerfEngine<SmolStr, u32> {
    let cfg = Options::new()
      .with_port(7946)
      .with_close_timeout(Duration::from_secs(10));
    let ep_cfg = EndpointOptions::new(SmolStr::new("q"), node_addr(7946));
    let mut engine = SerfEngine::new_at_with_rng(
      cfg,
      TransformOptions::default(),
      ep_cfg,
      SerfOptions::new(),
      now,
      SmallRng::seed_from_u64(1),
      SmallRng::seed_from_u64(serf_seed),
    );
    engine.start(now);
    engine
  };

  let mut engine = build(100);
  assert_eq!(
    engine.port(),
    7946,
    "port() reports the configured bind port"
  );
  assert_eq!(
    engine.state(),
    SerfState::Alive,
    "an engine built by the panicking constructor is running"
  );
  assert_eq!(engine.num_members(), 1, "a fresh engine tracks only itself");
  assert_eq!(engine.local_id(), &SmolStr::new("q"));

  let seeded_100 = first_query_id_of(&mut engine, now);
  assert_ne!(
    seeded_100,
    first_query_id_of(&mut build(200), now),
    "distinct serf RNG seeds must produce distinct first query ids"
  );
  assert_eq!(
    seeded_100,
    first_query_id_of(&mut build(100), now),
    "the same serf RNG seed must reproduce the same first query id"
  );
}

/// `new_at` — the panicking form of `try_new_at` — builds on a valid configuration
/// with serf's core RNG ZERO-SEEDED. That is the documented determinism caveat: two
/// fresh engines built this way emit the SAME first query id, which is exactly why a
/// production driver must construct via `try_new_at_with_rng` instead.
#[test]
fn new_at_builds_with_a_zero_seeded_serf_rng() {
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let build = || -> SerfEngine<SmolStr, u32> {
    let cfg = Options::new()
      .with_port(7946)
      .with_close_timeout(Duration::from_secs(10));
    let ep_cfg = EndpointOptions::new(SmolStr::new("z"), node_addr(7946));
    let mut engine = SerfEngine::new_at(
      cfg,
      TransformOptions::default(),
      ep_cfg,
      SerfOptions::new(),
      now,
      test_rng(),
    );
    engine.start(now);
    engine
  };

  let mut one = build();
  assert_eq!(one.port(), 7946, "port() reports the configured bind port");
  assert_eq!(
    one.state(),
    SerfState::Alive,
    "an engine built by the panicking constructor is running"
  );
  assert_eq!(
    first_query_id_of(&mut one, now),
    first_query_id_of(&mut build(), now),
    "a zero-seeded serf RNG makes two fresh engines share one query-id sequence"
  );
}

/// `new_at_with_rng` panics on an invalid configuration rather than building a node
/// past the construction-time checks — the documented contract steering a fallible
/// caller to `try_new_at_with_rng`.
#[test]
#[should_panic(expected = "invalid configuration")]
fn new_at_with_rng_panics_on_an_invalid_configuration() {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("bad"), node_addr(7946));
  let serf_opts =
    SerfOptions::new().with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let _engine: SerfEngine<SmolStr, u32> = SerfEngine::new_at_with_rng(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    serf_opts,
    now,
    test_rng(),
    SmallRng::seed_from_u64(7),
  );
}

/// `new_at` panics on an invalid configuration rather than building a node past the
/// construction-time checks — the documented contract steering a fallible caller to
/// `try_new_at`.
#[test]
#[should_panic(expected = "invalid configuration")]
fn new_at_panics_on_an_invalid_configuration() {
  // A non-routable advertise address: a node must advertise an address its peers can
  // route a reply to.
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let bad = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946);
  let ep_cfg = EndpointOptions::new(SmolStr::new("bad"), bad);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let _engine: SerfEngine<SmolStr, u32> = SerfEngine::new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    SerfOptions::new(),
    now,
    test_rng(),
  );
}

// ── running-state gate ───────────────────────────────────────────────────────

/// `ensure_running` — the gate every operation that would queue work no peer could
/// observe consults — passes while the node is `Alive` and, once it has left, fails
/// with `BadJoinState` CARRYING the current serf state (so a caller can report why
/// it was refused rather than guessing).
#[test]
fn ensure_running_rejects_once_the_node_has_left() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  // Let the construction self-join sieve settle before leaving.
  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(2);
  engine.pump(now, &mut gossip, &mut stream);
  assert!(
    engine.ensure_running().is_ok(),
    "a running node must pass the gate"
  );

  engine
    .leave(now)
    .expect("leave from a running node succeeds");
  match engine.ensure_running() {
    Err(SerfError::BadJoinState(state)) => assert_eq!(
      state,
      engine.state(),
      "the rejection must carry the node's current serf state"
    ),
    other => panic!("expected Err(BadJoinState) once the node has left, got {other:?}"),
  }
}

// ── join-handle allocation and per-join exchange binding ─────────────────────

/// Two concurrent joins mint DISTINCT handles from a monotonic sequence, and each
/// dispatched `Connect` binds its exchange to the join that actually started its
/// stream — never to the other in-flight join. Once a slot frees, each parked dial is
/// serviced and each join terminalizes on its OWN exchange, so their outcomes never
/// cross-resolve. A binding that credited the first waiter it found (rather than
/// matching the START `StreamId`) would leave both exchanges in one join's pending set.
#[test]
fn concurrent_joins_mint_distinct_handles_and_bind_their_own_exchanges() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  // A listener but an EMPTY dial pool: each `Connect` parks as `PendingDial`, so both
  // exchanges stay in flight while their bindings are inspected.
  engine.set_listener(9);

  let first = engine
    .join(&[node_addr(7002)], false, now)
    .expect("join announces intent and mints a handle");
  let second = engine
    .join(&[node_addr(7003)], false, now)
    .expect("a second concurrent join mints its own handle");
  assert_ne!(
    first, second,
    "two concurrent joins must mint distinct handles"
  );
  assert_eq!(
    second.get(),
    first.get() + 1,
    "handles are minted from a monotonic sequence"
  );

  let mut gossip = NoGossip;
  let mut stream = NoStream::with_pool(0);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.pending_dial_count(),
    2,
    "both seeds dispatched a push/pull that parked on the exhausted pool"
  );

  {
    let one = &engine.pending_joins[&first];
    let two = &engine.pending_joins[&second];
    assert_eq!(
      one.started.len(),
      1,
      "each join records only its own stream"
    );
    assert_eq!(
      two.started.len(),
      1,
      "each join records only its own stream"
    );
    assert!(
      one.started.is_disjoint(&two.started),
      "two joins never share a start stream"
    );
    assert_eq!(
      one.pending.len(),
      1,
      "exactly the exchange this join started is bound to it"
    );
    assert_eq!(
      two.pending.len(),
      1,
      "exactly the exchange this join started is bound to it"
    );
    assert!(
      one.pending.is_disjoint(&two.pending),
      "a Connect binds only the join whose start stream it carries"
    );
  }

  // Free two reuse-ready slots: the rebalance assigns them to the parked dials, which
  // `NoStream::connect` rejects — so each join terminalizes on its OWN exchange.
  engine.plane_mut().pool.push(5);
  engine.plane_mut().pool.push(6);

  let mut outcomes = (None, None);
  for _ in 0..8 {
    engine.pump(now, &mut gossip, &mut stream);
    if outcomes.0.is_none() {
      outcomes.0 = engine.poll_join(first);
    }
    if outcomes.1.is_none() {
      outcomes.1 = engine.poll_join(second);
    }
    if outcomes.0.is_some() && outcomes.1.is_some() {
      break;
    }
  }
  assert_eq!(
    engine.pending_dial_count(),
    0,
    "each deferred dial must be assigned a freed slot and leave PendingDial"
  );

  for outcome in [outcomes.0, outcomes.1] {
    match outcome {
      Some(Err(jf)) => {
        assert_eq!(
          jf.requested(),
          1,
          "each join dispatched exactly its own seed"
        );
        assert_eq!(jf.contacted(), 0, "its failed dial contacted no seed");
      }
      other => panic!("each join must resolve independently as Err(JoinFailed), got {other:?}"),
    }
  }
  assert_eq!(
    engine.pending_join_count(),
    0,
    "both resolved joins are reaped once delivered"
  );
}

// ── programmable single-engine reliable mock ─────────────────────────────────
//
// The reliable-plane lifecycle (dial → promote → flush → half-close → teardown →
// reap) is link-layer-independent engine code: the machine emits the `StreamAction`s
// and the engine pumps them over `StreamIo`. `ProgRel` stands in for a driver's
// socket pool so a single engine can be walked through those paths — a test flips a
// slot's state between pumps and reads back exactly what reached the wire.

/// The simulated TCP state of one mock reliable socket, as the engine observes it
/// through [`StreamIo`].
#[derive(Clone)]
struct SockState {
  /// The handshake is modelled complete: `may_send` is true and writes are accepted.
  /// A test flips this to promote a dial.
  established: bool,
  /// The socket has not reached `Closed`/`TimeWait` — `is_open` is true. `connect`
  /// opens it, a `close` leaves it open (our FIN in flight), and an `abort` drops it.
  open: bool,
  /// Cap on the bytes one `send` accepts, modelling partial-write backpressure: the
  /// remainder stays parked in the connection's `out` queue. `usize::MAX` accepts
  /// the whole buffer.
  send_cap: usize,
}

impl SockState {
  fn idle() -> Self {
    Self {
      established: false,
      open: false,
      send_cap: usize::MAX,
    }
  }
}

/// A directly-programmable single-engine reliable mock: a test mutates each slot's
/// [`SockState`] between pumps and asserts on the recorded sends / closes / aborts.
struct ProgRel {
  free: std::vec::Vec<u32>,
  socks: BTreeMap<u32, SockState>,
  /// Every `(handle, bytes)` a `send` accepted, in order.
  sent: std::vec::Vec<(u32, std::vec::Vec<u8>)>,
  /// Handles `close` (a graceful FIN) was called on.
  closed: std::vec::Vec<u32>,
  /// Handles `abort` (an RST) was called on.
  aborted: std::vec::Vec<u32>,
}

impl ProgRel {
  /// A mock realizing sockets for `handles`. The engine's own `plane_mut().pool` is
  /// the authority its reliable handlers consult, so a test pushes the same handles
  /// there (or installs one via `set_listener`).
  fn new(handles: &[u32]) -> Self {
    let mut socks = BTreeMap::new();
    for &h in handles {
      socks.insert(h, SockState::idle());
    }
    Self {
      free: handles.to_vec(),
      socks,
      sent: std::vec::Vec::new(),
      closed: std::vec::Vec::new(),
      aborted: std::vec::Vec::new(),
    }
  }

  fn sock(&self, c: u32) -> &SockState {
    self.socks.get(&c).expect("handle exists")
  }

  fn sock_mut(&mut self, c: u32) -> &mut SockState {
    self.socks.get_mut(&c).expect("handle exists")
  }
}

impl StreamIo for ProgRel {
  type Conn = u32;

  fn take_free(&mut self) -> Option<u32> {
    self.free.pop()
  }

  fn give(&mut self, c: u32) {
    self.free.push(c);
  }

  fn free_count(&self) -> usize {
    self.free.len()
  }

  fn listen(&mut self, c: u32, _port: u16) -> Result<(), crate::StreamIoError> {
    // A listening socket is open and awaiting a passive open; clear any per-slot
    // residue so a reclaimed-then-relistened handle starts clean.
    *self.sock_mut(c) = SockState::idle();
    self.sock_mut(c).open = true;
    Ok(())
  }

  fn accepted_peer(&self, _c: u32) -> Option<SocketAddr> {
    None
  }

  fn connect(
    &mut self,
    c: u32,
    _remote: SocketAddr,
    _local_port: u16,
  ) -> Result<(), crate::StreamIoError> {
    // A dial opens the socket; the test flips `established` to model the handshake
    // completing on a later tick.
    self.sock_mut(c).open = true;
    Ok(())
  }

  fn may_send(&self, c: u32) -> bool {
    let s = self.sock(c);
    s.established && s.open
  }

  fn may_recv(&self, _c: u32) -> bool {
    false
  }

  fn is_open(&self, c: u32) -> bool {
    self.sock(c).open
  }

  fn is_established(&self, c: u32) -> bool {
    self.sock(c).established
  }

  fn recv(&mut self, _c: u32, _buf: &mut [u8]) -> Option<usize> {
    None
  }

  fn recv_finished(&self, _c: u32) -> bool {
    false
  }

  fn send(&mut self, c: u32, bytes: &[u8]) -> usize {
    let n = bytes.len().min(self.sock(c).send_cap);
    self.sent.push((c, bytes[..n].to_vec()));
    n
  }

  fn send_queue(&self, _c: u32) -> usize {
    0
  }

  fn close(&mut self, c: u32) {
    // A graceful close leaves the socket open (our FIN in flight) until the peer
    // FINs back or the reap backstop forces it closed.
    self.closed.push(c);
  }

  fn abort(&mut self, c: u32) {
    self.aborted.push(c);
    let s = self.sock_mut(c);
    s.open = false;
    s.established = false;
  }
}

/// A running single-node engine with the given reliable-exchange (`stream_timeout`)
/// deadline, plus the clock it was started at.
fn engine_with_stream_timeout(stream_timeout: Duration) -> (SerfEngine<SmolStr, u32>, Instant) {
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg =
    EndpointOptions::new(SmolStr::new("test"), node_addr(7946)).with_stream_timeout(stream_timeout);
  let mut engine: SerfEngine<SmolStr, u32> = SerfEngine::try_new_at(
    cfg,
    TransformOptions::default(),
    ep_cfg,
    SerfOptions::new(),
    now,
    test_rng(),
  )
  .expect("valid configuration must construct without error");
  engine.start(now);
  (engine, now)
}

/// The sole reliable exchange currently mapped on the engine's plane.
fn sole_exchange(engine: &mut SerfEngine<SmolStr, u32>) -> ExchangeId {
  let mut ids = engine.plane_mut().connections.keys().copied();
  let eid = ids.next().expect("exactly one exchange is mapped");
  assert!(ids.next().is_none(), "exactly one exchange is mapped");
  eid
}

// ── reliable-plane reap / teardown by socket state ───────────────────────────

/// The reap pass reclaims a gracefully-closing handle the moment its socket reaches a
/// reusable (`!is_open`) state, and FORCE-ABORTS one whose close has outlived
/// `close_timeout` — so a peer that vanished mid-FIN can never permanently shrink the
/// pool. Without the force-abort backstop the stalled handle stays parked forever and
/// the pool loses a slot for good.
#[test]
fn reap_closing_reclaims_a_finished_close_and_force_aborts_a_vanished_peer() {
  let (mut engine, now) = engine_with_stream_timeout(Duration::from_secs(30));
  let mut stream = ProgRel::new(&[0, 1]);
  // Slot 0: a clean close that already reached `Closed`. Slot 1: a peer that vanished
  // mid-FIN — still open, and past its close deadline.
  stream.sock_mut(0).open = false;
  stream.sock_mut(1).open = true;
  engine
    .plane_mut()
    .closing
    .insert(0, now + Duration::from_secs(10));
  engine.plane_mut().closing.insert(1, now);
  assert_eq!(
    engine.closing_count(),
    2,
    "two handles are parked mid-close"
  );

  let mut gossip = NoGossip;
  engine.pump(now, &mut gossip, &mut stream);

  assert_eq!(
    engine.closing_count(),
    0,
    "both parked handles must be reaped — one cleanly, one force-aborted"
  );
  assert!(
    stream.aborted.contains(&1),
    "the handle past its close deadline must be force-aborted so its slot is reclaimable"
  );
  assert!(
    !stream.aborted.contains(&0),
    "an already-closed handle must be reclaimed without an abort"
  );
  assert_eq!(
    engine.pool_free_count() + engine.listener_present() as usize,
    2,
    "both reaped handles must return to the pool, never leak"
  );
}

/// Tearing down a `PendingDial` — an exchange whose dial was deferred and which
/// therefore holds NO socket — removes it outright, so the deferred dial is never
/// later issued for a retired exchange, and reclaims nothing (there is no slot to
/// reclaim). Tearing down an exchange that is no longer mapped is inert: it must not
/// panic, reclaim a phantom slot, or touch the link.
#[test]
fn teardown_of_a_socketless_pending_dial_removes_it_and_reclaims_nothing() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);
  // A listener but an empty dial pool: the join's Connect parks as PendingDial.
  engine.set_listener(9);
  let mut stream = ProgRel::new(&[9]);

  engine
    .join(&[node_addr(7002)], false, now)
    .expect("join announces intent and queues the routable seed");
  let mut gossip = NoGossip;
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.pending_dial_count(),
    1,
    "the exhausted-pool seed parked as a socketless PendingDial"
  );

  let eid = sole_exchange(&mut engine);
  let free_before = engine.pool_free_count();
  engine.teardown(eid, now, &mut stream);

  assert_eq!(
    engine.pending_dial_count(),
    0,
    "the retired exchange must be removed so a later rebalance never dials it"
  );
  assert_eq!(
    engine.pool_free_count(),
    free_before,
    "a socketless exchange reclaims no slot"
  );
  assert_eq!(engine.closing_count(), 0, "nothing is parked mid-close");
  assert!(
    stream.aborted.is_empty() && stream.closed.is_empty(),
    "a socketless teardown must not touch the link"
  );

  // The same exchange is now unknown: a repeat teardown is a no-op.
  engine.teardown(eid, now, &mut stream);
  assert_eq!(
    engine.pool_free_count(),
    free_before,
    "tearing down an unknown exchange must reclaim no slot"
  );
  assert!(
    stream.aborted.is_empty() && stream.closed.is_empty(),
    "tearing down an unknown exchange must not touch the link"
  );
}

/// Tearing down a HALF-CLOSED connection — our FIN already emitted, the peer's not yet
/// received — parks its handle for the reap backstop instead of returning it to the
/// pool: the socket is still open, so reusing the slot now would clobber a connection
/// that is still finishing its close. The handle comes back only once the reap sees it
/// closed (or its close deadline elapses).
#[test]
fn teardown_of_a_half_closed_connection_parks_it_for_the_reap() {
  let (mut engine, now) = engine_with_stream_timeout(Duration::from_secs(30));
  engine.plane_mut().pool.push(0);
  engine.set_listener(1);
  let mut stream = ProgRel::new(&[0, 1]);

  engine
    .join(&[node_addr(7002)], false, now)
    .expect("join announces intent and queues the routable seed");
  let mut gossip = NoGossip;
  // Tick 1: the Connect dials slot 0; the mock leaves the handshake incomplete.
  engine.pump(now, &mut gossip, &mut stream);
  // Complete the handshake: the request flushes and the machine's deferred FIN is
  // emitted once the write queue has drained, half-closing the connection.
  stream.sock_mut(0).established = true;
  let mut half_closed = false;
  for _ in 0..8 {
    engine.pump(now, &mut gossip, &mut stream);
    if engine.half_closed_count() == 1 {
      half_closed = true;
      break;
    }
  }
  assert!(
    half_closed,
    "the push/pull initiator half-closes its write half once the request is written"
  );
  assert!(
    stream.closed.contains(&0),
    "the deferred FIN reached the socket"
  );

  let eid = sole_exchange(&mut engine);
  let free_before = engine.pool_free_count();
  engine.teardown(eid, now, &mut stream);

  assert_eq!(
    engine.closing_count(),
    1,
    "a half-closed connection's handle must be parked for the reap backstop"
  );
  assert_eq!(
    engine.pool_free_count(),
    free_before,
    "its handle must NOT return to the pool while the socket is still closing"
  );
  assert!(
    !stream.aborted.contains(&0),
    "a half-closed teardown must not RST a socket whose FIN is already in flight"
  );
}

/// Tearing down a dial the peer NEVER established — the socket is open but not
/// send-capable — RST-aborts it and returns the slot straight to the pool. FIN-ing a
/// connection the peer never opened would strand the slot in the close backstop for a
/// close that can never complete.
#[test]
fn teardown_of_a_never_established_dial_aborts_and_reclaims_the_slot() {
  let (mut engine, now) = engine_with_stream_timeout(Duration::from_secs(30));
  engine.plane_mut().pool.push(0);
  engine.set_listener(1);
  let mut stream = ProgRel::new(&[0, 1]);

  engine
    .join(&[node_addr(7002)], false, now)
    .expect("join announces intent and queues the routable seed");
  let mut gossip = NoGossip;
  engine.pump(now, &mut gossip, &mut stream);
  assert!(
    StreamIo::is_open(&stream, 0),
    "the dial issued connect on the pooled slot"
  );
  assert!(
    !StreamIo::may_send(&stream, 0),
    "the mock leaves the handshake incomplete, so the socket is not send-capable"
  );

  let eid = sole_exchange(&mut engine);
  let free_before = engine.pool_free_count();
  engine.teardown(eid, now, &mut stream);

  assert!(
    stream.aborted.contains(&0),
    "a socket the peer never established must be RST, not FIN'd"
  );
  assert_eq!(
    engine.pool_free_count(),
    free_before + 1,
    "its slot must return straight to the pool"
  );
  assert_eq!(
    engine.closing_count(),
    0,
    "nothing is parked mid-close for a connection that never opened"
  );
  assert!(
    engine.plane_mut().connections.is_empty(),
    "the torn-down exchange is unmapped"
  );
}

// ── reliable egress under partial-write backpressure ─────────────────────────

/// Flush a join push/pull's request bytes over a link whose `send` accepts at most
/// `send_cap` bytes at a time, returning the byte stream that reached the socket plus
/// the length of each accepted write.
fn push_pull_bytes_written(send_cap: usize) -> (std::vec::Vec<u8>, std::vec::Vec<usize>) {
  let (mut engine, now) = engine_with_stream_timeout(Duration::from_secs(30));
  engine.plane_mut().pool.push(0);
  engine.set_listener(1);
  let mut stream = ProgRel::new(&[0, 1]);
  stream.sock_mut(0).send_cap = send_cap;

  engine
    .join(&[node_addr(7004)], false, now)
    .expect("join announces intent and queues the routable seed");

  let mut gossip = NoGossip;
  // Tick 1: the Connect dials slot 0. The mock leaves it un-established, so the egress
  // pump skips the `!may_send` socket and nothing flushes yet.
  engine.pump(now, &mut gossip, &mut stream);
  assert!(
    stream.sent.is_empty(),
    "a still-handshaking socket must not be written to"
  );
  // Establish it and pump until the queue has drained. The clock is held, so no
  // deadline elapses and only the capped writes limit progress.
  stream.sock_mut(0).established = true;
  for _ in 0..400 {
    engine.pump(now, &mut gossip, &mut stream);
  }

  let writes: std::vec::Vec<usize> = stream
    .sent
    .iter()
    .filter(|(c, _)| *c == 0)
    .map(|(_, b)| b.len())
    .collect();
  let bytes: std::vec::Vec<u8> = stream
    .sent
    .iter()
    .filter(|(c, _)| *c == 0)
    .flat_map(|(_, b)| b.iter().copied())
    .collect();
  (bytes, writes)
}

/// A `send` that accepts fewer bytes than offered leaves the UNSENT TAIL at the front
/// of the connection's out queue, so a later tick delivers exactly the remainder: the
/// reassembled stream is byte-identical to the one an uncapped link receives — nothing
/// dropped, duplicated, or reordered. Popping the front on a partial write would
/// truncate the request; re-sending the whole front would duplicate its prefix. Either
/// corrupts the push/pull framing.
#[test]
fn partial_writes_park_the_remainder_and_preserve_byte_order() {
  let (whole, _) = push_pull_bytes_written(usize::MAX);
  assert!(
    !whole.is_empty(),
    "the join push/pull must write its request to the dialed socket"
  );

  let (chunked, writes) = push_pull_bytes_written(4);
  assert!(
    writes.len() >= 2,
    "the capped link must force the request across multiple writes, got {writes:?}"
  );
  assert!(
    writes.iter().all(|&n| n <= 4),
    "no write may exceed the link's per-send cap, got {writes:?}"
  );
  assert_eq!(
    chunked, whole,
    "the partial-write remainder must reassemble to exactly the bytes an uncapped link \
     receives — no byte dropped, duplicated, or reordered"
  );
}

// ── gossip ingress screens and the egress destination screen ─────────────────

/// A [`GossipIo`] with a programmable inbound queue and a capture of every emitted
/// datagram, so a test can feed one exact datagram from one exact source and assert
/// precisely what — if anything — went back on the wire.
struct QueueGossip {
  inbound: std::vec::Vec<(SocketAddr, std::vec::Vec<u8>)>,
  outbound: std::vec::Vec<(std::vec::Vec<u8>, SocketAddr)>,
}

impl QueueGossip {
  fn new() -> Self {
    Self {
      inbound: std::vec::Vec::new(),
      outbound: std::vec::Vec::new(),
    }
  }

  fn push(&mut self, src: SocketAddr, bytes: std::vec::Vec<u8>) {
    self.inbound.push((src, bytes));
  }
}

impl GossipIo for QueueGossip {
  fn recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
    if self.inbound.is_empty() {
      return None;
    }
    let (src, bytes) = self.inbound.remove(0);
    let n = bytes.len().min(buf.len());
    buf[..n].copy_from_slice(&bytes[..n]);
    Some((src, n))
  }

  fn send(&mut self, bytes: &[u8], dest: SocketAddr) {
    self.outbound.push((bytes.to_vec(), dest));
  }
}

/// A peer address on the test subnet.
fn peer_addr(host: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)), port)
}

/// A plaintext gossip datagram carrying an `Alive` for `ghost`, encoded with `label`.
/// Incarnation 1 passes SWIM's freshness check for a node the receiver has never seen.
fn alive_datagram(ghost: &Node<SmolStr, SocketAddr>, label: Option<Bytes>) -> Bytes {
  encode_outgoing::<SmolStr, SocketAddr>(
    &Message::Alive(Alive::new(1, ghost.clone())),
    &EncodeOptions::new(label),
  )
  .expect("a well-formed Alive encodes")
}

/// A malformed inbound gossip datagram is dropped at the decode step: bad network
/// input must never panic the node or mutate membership. The SAME source then lands a
/// well-formed `Alive` and IS admitted, so the drop is the codec rejecting the garbage
/// rather than the ingress path being inert.
#[test]
fn malformed_gossip_datagram_is_dropped_without_membership_change() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let src = peer_addr(3, 7946);
  let mut gossip = QueueGossip::new();
  let mut stream = NoStream::with_pool(2);
  // No byte of this is a valid message frame.
  gossip.push(src, std::vec![0xffu8; 32]);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    1,
    "a malformed datagram must be dropped — no member may be learned from it"
  );

  let ghost = Node::new(SmolStr::new("ghost"), peer_addr(2, 7946));
  gossip.push(src, alive_datagram(&ghost, None).to_vec());
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    2,
    "a well-formed Alive from the same source must still be admitted"
  );
}

/// A node with a cluster label REJECTS an inbound gossip datagram that does not carry
/// it — the label check drops the frame before the machine sees it, so a neighbouring
/// cluster's chatter can never inject a member. The same `Alive` stamped with the
/// node's own label is admitted.
#[test]
fn a_labeled_node_rejects_gossip_without_its_cluster_label() {
  let cfg = Options::new()
    .with_port(7946)
    .with_close_timeout(Duration::from_secs(10));
  let ep_cfg = EndpointOptions::new(SmolStr::new("alpha"), node_addr(7946));
  let transform = TransformOptions::default()
    .with_label(Some(b"alpha".to_vec()))
    .expect("a valid cluster label");
  let now = Instant::from_origin(Duration::from_secs(86_400));
  let mut engine: SerfEngine<SmolStr, u32> =
    SerfEngine::try_new_at(cfg, transform, ep_cfg, SerfOptions::new(), now, test_rng())
      .expect("valid configuration must construct without error");
  engine.start(now);

  let ghost = Node::new(SmolStr::new("ghost"), peer_addr(2, 7946));
  let src = peer_addr(3, 7946);
  let mut gossip = QueueGossip::new();
  let mut stream = NoStream::with_pool(2);

  gossip.push(src, alive_datagram(&ghost, None).to_vec());
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    1,
    "an unlabeled datagram must be rejected by a labeled node"
  );

  gossip.push(
    src,
    alive_datagram(&ghost, Some(Bytes::from_static(b"beta"))).to_vec(),
  );
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    1,
    "a wrong-label datagram must be rejected by a labeled node"
  );

  gossip.push(
    src,
    alive_datagram(&ghost, Some(Bytes::from_static(b"alpha"))).to_vec(),
  );
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    2,
    "the same Alive stamped with the node's own label must be admitted"
  );
}

/// The last-line egress screen: the engine never writes a gossip datagram to a
/// destination no packet could reach. A Ping addressed to this node but arriving from
/// a NON-ROUTABLE source would have its ack reflected straight back to that source, so
/// the ack is screened at egress and nothing goes on the wire. The identical Ping from
/// a routable source IS acked, so the drop is the destination screen and not a rejected
/// Ping.
#[test]
fn no_gossip_datagram_is_emitted_to_a_non_routable_destination() {
  let mut engine = make_engine();
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  // A Ping must name this node as its target to be answered at all.
  let me = Node::new(SmolStr::new("test"), node_addr(7946));
  let prober = Node::new(SmolStr::new("prober"), peer_addr(9, 7002));
  let ping = encode_outgoing::<SmolStr, SocketAddr>(
    &Message::Ping(Ping::new(7, prober, me)),
    &EncodeOptions::new(None),
  )
  .expect("a well-formed Ping encodes");

  let mut gossip = QueueGossip::new();
  let mut stream = NoStream::with_pool(2);

  // Port 0 is non-routable: an ack sent there could never arrive.
  gossip.push(peer_addr(9, 0), ping.to_vec());
  engine.pump(now, &mut gossip, &mut stream);
  assert!(
    gossip.outbound.is_empty(),
    "an ack to a non-routable source must be screened at egress, not written to the wire"
  );

  let routable = peer_addr(9, 7002);
  gossip.push(routable, ping.to_vec());
  engine.pump(now, &mut gossip, &mut stream);
  assert!(
    gossip.outbound.iter().any(|(_, dest)| *dest == routable),
    "the identical Ping from a routable source must be acked"
  );
}

/// An encrypted node DROPS an inbound plaintext gossip datagram at the decrypt step: a
/// node on an encrypted cluster must never admit an unauthenticated frame. The same
/// `Alive`, sealed under the node's live keyring, IS admitted — so the drop is the
/// decrypt guard rejecting an unauthenticated frame, not a malformed one.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn an_encrypted_node_drops_plaintext_inbound_gossip() {
  let mut engine = make_encrypted_engine("enc", 7946, Keyring::new(secret_key(0x11)));
  let now = Instant::from_origin(Duration::from_secs(86_400));
  engine.start(now);

  let ghost = Node::new(SmolStr::new("ghost"), peer_addr(2, 7946));
  let plaintext = alive_datagram(&ghost, None);
  let src = peer_addr(3, 7946);
  let mut gossip = QueueGossip::new();
  let mut stream = NoStream::with_pool(2);

  gossip.push(src, plaintext.to_vec());
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    1,
    "an unauthenticated plaintext datagram must be dropped by an encrypted node"
  );

  let sealed = engine
    .endpoint
    .encrypt_gossip(&plaintext)
    .expect("the live keyring seals the frame");
  gossip.push(src, sealed);
  engine.pump(now, &mut gossip, &mut stream);
  assert_eq!(
    engine.num_members(),
    2,
    "the same Alive, sealed under the node's keyring, must be admitted"
  );
}

// ── key-management requests missing their key ────────────────────────────────

/// A keyed key-management op (`install` / `use` / `remove`) that arrives WITHOUT its
/// key is refused with a message naming the omission, and the live keyring is left
/// exactly as it was — a malformed request can neither mutate the wire keyring nor be
/// reported as a success. `list`, which needs no key, is unaffected: it still reports
/// the live state.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn a_keyed_op_without_its_key_is_refused_and_leaves_the_ring_untouched() {
  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);
  let mut engine = make_encrypted_engine("a", 7946, Keyring::with_secondaries(k1, [k2]));
  let primary_before = *engine.keyring().expect("encrypted").primary_ref();
  let secondaries_before = engine.keyring().expect("encrypted").secondaries().to_vec();

  for op in [
    KeyRequestOperation::Install,
    KeyRequestOperation::Use,
    KeyRequestOperation::Remove,
  ] {
    let resp = engine.apply_key_request(&key_op(op, None));
    assert!(
      !resp.result,
      "a keyed op carrying no key must be refused, never reported as applied"
    );
    assert!(
      resp.message.contains("missing"),
      "the refusal must name the omitted key, got {:?}",
      resp.message
    );
    let kr = engine.keyring().expect("encrypted");
    assert_eq!(
      *kr.primary_ref(),
      primary_before,
      "a refused op must not move the primary"
    );
    assert_eq!(
      kr.secondaries(),
      secondaries_before.as_slice(),
      "a refused op must not change the ring"
    );
  }

  // `list` needs no key, so the keyless refusal must not swallow it.
  let resp = engine.apply_key_request(&key_op(KeyRequestOperation::List, None));
  assert!(resp.result, "list carries no key and must still succeed");
  assert_eq!(
    resp.primary_key,
    Some(primary_before),
    "list reports the live primary"
  );
}

// ── the Closing drain: progress re-arms, a stall force-aborts ────────────────

/// Whether the engine holds a reliable connection in the `Closing` drain state.
/// `closing_count` counts the DETACHED handles, not the still-mapped draining
/// connections, so this scans the live connections for the drain state.
fn has_closing_connection(engine: &mut SerfEngine<SmolStr, u32>) -> bool {
  engine
    .plane_mut()
    .connections
    .values()
    .any(|c| c.state == ConnState::Closing)
}

/// The undelivered-byte mark of the engine's sole draining (`Closing`) connection.
fn closing_drain_mark(engine: &mut SerfEngine<SmolStr, u32>) -> Option<usize> {
  engine
    .plane_mut()
    .connections
    .values()
    .find(|c| c.state == ConnState::Closing)
    .map(|c| c.close_drain_mark)
}

/// A graceful close whose reply is still unacknowledged must NOT truncate it: the
/// connection parks in `Closing` (keeping its slot) while the egress pump keeps
/// draining. Each acknowledged byte is progress, which re-arms the drain rather than
/// letting the close deadline fire — `close_timeout` bounds a STALL, not the total
/// drain — and once the reply is fully acked the terminal FIN goes out and the slot is
/// reclaimed.
#[test]
fn closing_drain_re_arms_on_progress_then_fins_once_the_reply_is_acked() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  // B withholds its acknowledgements, so its push/pull reply is still unacked when its
  // bridge gracefully closes.
  link.hold_b_tx();

  link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut parked = false;
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if has_closing_connection(&mut link.b) {
      parked = true;
      break;
    }
  }
  assert!(
    parked,
    "a graceful close with an unacked reply must park in Closing, not FIN at once"
  );
  assert!(
    (link.b.pool_free_count() + link.b.listener_present() as usize) < 2,
    "the draining connection still pins its slot until the reply is acked"
  );

  // Acknowledge part of the reply: the undelivered count shrinks, so the drain re-arms
  // and the connection stays mapped rather than being force-closed.
  let mark_before = closing_drain_mark(&mut link.b).expect("the draining connection is mapped");
  link.ack_b(1);
  link.step(now);
  while link.b.poll_event().is_some() {}
  let mark_after =
    closing_drain_mark(&mut link.b).expect("progress keeps the draining connection mapped");
  assert!(
    mark_after < mark_before,
    "an acknowledged byte is progress: the drain mark must shrink ({mark_before} -> {mark_after})"
  );

  // Acknowledge the remainder: the drain completes, the terminal FIN goes out, and the
  // reap returns the slot.
  link.ack_b(usize::MAX);
  let mut t = now;
  for _ in 0..20 {
    link.step(t);
    while link.b.poll_event().is_some() {}
    if link.b.pool_free_count() + link.b.listener_present() as usize == 2 {
      break;
    }
    t += Duration::from_millis(200);
  }
  assert!(
    !has_closing_connection(&mut link.b),
    "the fully-drained connection must FIN and leave the Closing state"
  );
  assert_eq!(
    link.b.pool_free_count() + link.b.listener_present() as usize,
    2,
    "once the reply is fully acked, the drained connection's slot is reclaimed"
  );
}

/// The `Closing` drain's force-abort backstop: a peer that never acknowledges the reply
/// makes no progress, so at the close deadline the engine gives up on the remainder,
/// RSTs the socket, and reclaims the slot — a stalled peer can never permanently wedge
/// a pooled slot mid-drain.
#[test]
fn closing_drain_force_aborts_a_stalled_peer_at_the_deadline() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));
  // B's reply is never acknowledged, so its drain makes no progress at all.
  link.hold_b_tx();

  link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut parked = false;
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if has_closing_connection(&mut link.b) {
      parked = true;
      break;
    }
  }
  assert!(parked, "the unacked reply parked the connection in Closing");

  // Never acknowledge. Advance past the 10 s close timeout: with no progress, the
  // no-progress bound elapses and the drain is force-aborted.
  let mut t = now + Duration::from_secs(15);
  for _ in 0..10 {
    link.step(t);
    while link.b.poll_event().is_some() {}
    if !has_closing_connection(&mut link.b)
      && link.b.pool_free_count() + link.b.listener_present() as usize == 2
    {
      break;
    }
    t += Duration::from_secs(15);
  }
  assert!(
    !has_closing_connection(&mut link.b),
    "a stalled drain must be force-aborted off the connection map at its deadline"
  );
  assert_eq!(
    link.b.pool_free_count() + link.b.listener_present() as usize,
    2,
    "the force-aborted slot must be reclaimed so the pool cannot wedge"
  );
}

// ── push/pull completions the engine did not start for a join ────────────────

/// An anti-entropy push/pull — one the engine started for a state REFRESH, not for an
/// await-result join — is never credited to a join: its terminal `ExchangeCompleted`
/// folds into no waiter, so an already-delivered join is not re-resolved and no waiter
/// is resurrected. A fold keyed on the peer ADDRESS rather than the exchange id would
/// re-resolve the join that reached the very same peer.
#[test]
fn a_refresh_push_pull_completion_is_credited_to_no_join() {
  let mut link = LinkPair::new(&[10, 11], &[20, 21]);
  let now = Instant::from_origin(Duration::from_secs(86_400));

  let handle = link
    .a
    .join(&[link.b_addr], false, now)
    .expect("join announces intent and mints a handle");

  let mut outcome = None;
  for _ in 0..40 {
    link.step(now);
    while link.a.poll_event().is_some() {}
    while link.b.poll_event().is_some() {}
    if let Some(res) = link.a.poll_join(handle) {
      outcome = Some(res);
      break;
    }
  }
  assert!(
    matches!(outcome, Some(Ok(_))),
    "the join must resolve Ok before the refresh, got {outcome:?}"
  );
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "the delivered join is reaped"
  );

  // A push/pull started for anti-entropy, owned by no join.
  link
    .a
    .endpoint
    .start_push_pull(link.b_addr, PushPullKind::Refresh, now);

  let mut completed = false;
  for _ in 0..40 {
    link.step(now);
    while link.b.poll_event().is_some() {}
    while let Some(ev) = link.a.poll_event() {
      if matches!(&ev, Event::ExchangeCompleted(ec) if ec.kind() == ExchangeKind::PushPull) {
        completed = true;
      }
    }
    if completed {
      break;
    }
  }
  assert!(
    completed,
    "the refresh push/pull must reach its terminal ExchangeCompleted"
  );
  assert_eq!(
    link.a.pending_join_count(),
    0,
    "a completion owned by no join must not resurrect a waiter"
  );
  assert!(
    link.a.poll_join(handle).is_none(),
    "the already-delivered join must not be re-resolved by an unrelated completion"
  );
}
