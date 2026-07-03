use super::*;

use core::{
  net::{IpAddr, Ipv4Addr},
  time::Duration,
};

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
    .join(&[node_addr(7002)])
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
  engine.join(&[dead]).expect("join succeeds");

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
      engine.join(&[node_addr(7003)]),
      Err(SerfError::BadJoinState(_))
    ),
    "a join after leave must be rejected with BadJoinState"
  );
}
