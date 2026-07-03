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
