use super::*;

use core::{
  net::{IpAddr, Ipv4Addr},
  time::Duration,
};

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use memberlist_proto::{SeedableRng, SmallRng};
use smol_str::SmolStr;

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
    matches!(result, Err(InitError::NonRoutableAdvertiseAddr(_))),
    "a non-routable advertise address must fail construction"
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
    .user_event("deploy", Bytes::from_static(b"v2"), false)
    .expect("a user event is accepted while the node is running");
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
// ferry both ways over the matched pipe. The link acks instantly (`send_queue`
// is always 0), so a graceful close FINs without a `Closing` drain.

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
    let mut fab = self.fabric.borrow_mut();
    let Some(p) = fab.pipes.get_mut(&pipe) else {
      return 0;
    };
    if p.reset || !p.established {
      return 0;
    }
    // Deliver to the peer's rx immediately (the FSM sees the bytes); the link acks
    // instantly so nothing lingers as unacked tx.
    match end {
      End::Dialer => p.d2a.extend(bytes.iter().copied()),
      End::Acceptor => p.a2d.extend(bytes.iter().copied()),
    }
    bytes.len()
  }

  fn send_queue(&self, _c: u32) -> usize {
    0
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
