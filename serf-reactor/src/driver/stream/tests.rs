//! Stream-driver pump-ordering regression tests.
//!
//! The single `handle_timeout` site and the join / leave deadline reaps must fire
//! only once the pump is quiescent, so a due deadline never times out a probe,
//! await-result join, or graceful leave whose resolving Ack / `ExchangeCompleted`
//! / `LeftCluster` is still queued behind the per-poll `iter_drain_cap`.

use core::num::NonZeroU8;
use std::task::Waker;

use agnostic::{RuntimeLite, tokio::TokioRuntime};
use memberlist_proto::{
  Endpoint, EndpointOptions, Node, PushPullKind, RawRecords, SmallRng,
  streams::{LabelOptions, StreamEndpoint as Coordinator},
};
use serf_proto::{
  members::{Member, MemberStatus},
  options::Options as SerfOptions,
  typed::Tags,
};
use smol_str::SmolStr;

use super::*;

/// Shared loopback cluster label so both coordinators' record-layer handshakes
/// settle.
const CLUSTER: &[u8] = b"serf-reactor-loopback";

/// The driver endpoint's advertise identity (distinct from the seed peer).
const DRIVER_ADDR: &str = "127.0.0.1:7946";

type TestDriver = StreamDriver<SmolStr, TokioRuntime, RawRecords, SmallRng, SmallRng>;

/// The driver's serf endpoint, pinning the reactor's shared drop-counter storage
/// the [`StreamDriver`] requires.
type DrvEndpoint =
  StreamEndpoint<SmolStr, SocketAddr, RawRecords, SmallRng, SmallRng, ReactorDropCounter>;

fn sa(s: &str) -> SocketAddr {
  s.parse().expect("loopback addr")
}

/// Drive the driver through exactly one `Future::poll`. The manual poll loop
/// re-polls unconditionally, so the no-op waker is a valid, harmless sink.
fn poll_once(driver: &mut TestDriver) -> Poll<()> {
  let mut cx = Context::from_waker(Waker::noop());
  Pin::new(driver).poll(&mut cx)
}

/// The reliable exchange timeout every non-clamp test uses: large enough that no
/// exchange deadline fires during a fast unit test, so the memberlist default
/// (`10s`) behavior is preserved for the existing pump regressions.
const DEFAULT_TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a serf `StreamEndpoint<SmolStr, SocketAddr, RawRecords>` rooted at `id` /
/// `advertise`, mirroring the production construction (memberlist inner endpoint →
/// reliable coordinator → serf super-machine). Seeded deterministically; the
/// initial local `NodeJoined` self-event is drained so a caller starts clean.
fn build_endpoint(id: &str, advertise: SocketAddr) -> DrvEndpoint {
  build_endpoint_with_stream_timeout(id, advertise, DEFAULT_TEST_STREAM_TIMEOUT)
}

/// As [`build_endpoint`], but with an explicit reliable `stream_timeout` so a test
/// can make the coordinator stamp a short exchange deadline on its push/pulls.
fn build_endpoint_with_stream_timeout(
  id: &str,
  advertise: SocketAddr,
  stream_timeout: Duration,
) -> DrvEndpoint {
  let inner_opts = EndpointOptions::new(SmolStr::new(id), advertise)
    .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"))
    .with_stream_timeout(stream_timeout);
  let inner = Endpoint::new(inner_opts, SmallRng::seed_from_u64(0));
  let coord = Coordinator::<_, _, RawRecords>::new(
    inner,
    LabelOptions::new_in(Some(CLUSTER.to_vec()), ()),
    Box::new(|_addr: &SocketAddr| None),
    Box::new(|addr: &SocketAddr| *addr),
  );
  // These pump-ordering tests do not assert coalescer shed counts, so the write
  // halves suffice; the read halves are unused here.
  let (user_drop, _) = crate::drop_counter::drop_channel();
  let (member_drop, _) = crate::drop_counter::drop_channel();
  let mut e = StreamEndpoint::new_with_rng_in(
    coord,
    SerfOptions::new(),
    SmallRng::seed_from_u64(0),
    user_drop,
    member_drop,
  );
  while e.poll_event().is_some() {}
  e
}

/// The initial published snapshot: the local node, `Alive`, zeroed clocks
/// (a test-local copy of `crate::serf::initial_snapshot`).
fn initial_snapshot(id: &str, advertise: SocketAddr) -> SerfSnapshot<SmolStr, SocketAddr> {
  let member = Member::new(
    Node::new(SmolStr::new(id), advertise),
    Tags::new(),
    MemberStatus::Alive,
  );
  SerfSnapshot::new(
    vec![Arc::new(member)],
    &SmolStr::new(id),
    SerfState::Alive,
    LamportTime::from(0u64),
    LamportTime::from(0u64),
    LamportTime::from(0u64),
  )
}

/// Build a real `StreamDriver` over a bound gossip socket + a live accept task,
/// with the scheduling deliberately OFF so no stray coordinator deadline can
/// supply a timer the test means to attribute to the parked join. Returns the
/// driver, the obs receiver (held so the obs channel stays connected), and the
/// shared state.
async fn build_driver(
  iter_drain_cap: usize,
  bridge_inbound_cap: usize,
) -> (
  TestDriver,
  Receiver<Event<SmolStr, SocketAddr>>,
  Arc<Shared<SmolStr>>,
) {
  build_driver_with_stream_timeout(
    iter_drain_cap,
    bridge_inbound_cap,
    DEFAULT_TEST_STREAM_TIMEOUT,
  )
  .await
}

/// As [`build_driver`], but with an explicit reliable `stream_timeout` applied to
/// BOTH the driver's endpoint (so its push/pull exchanges carry a short deadline)
/// AND the driver's clamp field (so [`clamp_join_deadline`] reconciles against the
/// same value the coordinator will stamp).
async fn build_driver_with_stream_timeout(
  iter_drain_cap: usize,
  bridge_inbound_cap: usize,
  stream_timeout: Duration,
) -> (
  TestDriver,
  Receiver<Event<SmolStr, SocketAddr>>,
  Arc<Shared<SmolStr>>,
) {
  let socket = <<TokioRuntime as Runtime>::Net as Net>::UdpSocket::bind("127.0.0.1:0")
    .await
    .expect("bind gossip socket");
  let endpoint = build_endpoint_with_stream_timeout("drv", sa(DRIVER_ADDR), stream_timeout);
  let (_, user_drop_reader) = crate::drop_counter::drop_channel();
  let (_, member_drop_reader) = crate::drop_counter::drop_channel();
  let shared = Arc::new(Shared::new(
    initial_snapshot("drv", sa(DRIVER_ADDR)),
    user_drop_reader,
    member_drop_reader,
  ));
  let obs_payload_bytes = Arc::new(AtomicU64::new(0));
  let (obs_tx, obs_rx) = flume::unbounded();
  let (accepted_tx, accepted_rx) = flume::bounded(ACCEPT_CAP);
  let (accept_shutdown_tx, accept_shutdown_rx) = flume::bounded(1);
  let listener = <<TokioRuntime as Runtime>::Net as Net>::TcpListener::bind("127.0.0.1:0")
    .await
    .expect("bind accept listener");
  let accept_join = TokioRuntime::spawn(accept_task::<SmolStr, _>(
    listener,
    accepted_tx,
    accept_shutdown_rx,
    shared.clone(),
  ));
  let driver = StreamDriver::<SmolStr, TokioRuntime, RawRecords, SmallRng, SmallRng>::new(
    endpoint,
    socket,
    shared.clone(),
    obs_tx,
    obs_payload_bytes,
    None,
    accepted_rx,
    accept_shutdown_tx,
    accept_join,
    RuntimeOptions::new().with_iter_drain_cap(iter_drain_cap),
    StreamTransportOptions::new().with_bridge_inbound_cap(bridge_inbound_cap),
    None,
    stream_timeout,
    None,
    #[cfg(encryption)]
    Arc::new(crate::VoidKeyringDelegate),
  );
  (driver, obs_rx, shared)
}

/// Queue one bridge-inbound item toward the pump AS A REAL BRIDGE WOULD: bump the
/// in-flight reservation BEFORE the hand-off, then enqueue. Test-injected frames
/// bypass `bridge_task`, so without this the reap watermark would not account for
/// them and could reap a join whose completion is still buffered.
fn queue_inbound(driver: &TestDriver, tx: &Sender<BridgeInbound>, item: BridgeInbound) {
  driver
    .bridge_inbound_inflight
    .fetch_add(1, Ordering::Release);
  tx.try_send(item).expect("queue inbound item");
}

/// Drive one outbound Join push/pull on `driver.endpoint` toward `seed_addr` to a
/// real `Succeeded`, returning the dialer exchange id and the peer's pull response
/// frames (which the caller queues on `inbound_rx` rather than feeding here). A
/// second `StreamEndpoint` stands in for the seed; the dialer sends push + FIN up
/// front, so the exchange completes purely from feeding that response + a peer-FIN
/// EOF back — no live socket needed.
fn drive_push_to_queued_response(
  driver: &mut TestDriver,
  seed_addr: SocketAddr,
  now: Instant,
) -> (ExchangeId, Vec<Vec<u8>>) {
  let mut peer = build_endpoint("seed", seed_addr);

  // Dialer: start the exchange and capture its Connect id + push frames.
  driver
    .endpoint
    .start_push_pull(seed_addr, PushPullKind::Join, now);
  let mut eid = None;
  let mut push: Vec<Vec<u8>> = Vec::new();
  for _ in 0..256 {
    let mut progressed = false;
    while let Some(action) = driver.endpoint.poll_action() {
      progressed = true;
      if let StreamAction::Connect(info) = action {
        eid = Some(info.id());
      }
    }
    while let Some((id, _peer, bytes)) = driver.endpoint.poll_transport_transmit() {
      progressed = true;
      if Some(id) == eid {
        push.push(bytes.to_vec());
      }
    }
    if !progressed {
      break;
    }
  }
  let eid = eid.expect("the dialer's start_push_pull produced a Connect exchange id");
  assert!(
    !push.is_empty(),
    "the dialer emitted its push frames up front"
  );

  // Peer: admit the inbound connection, replay push + FIN, collect the pull
  // response (ticking the peer so its serf push-pull snapshot resyncs).
  let server_eid = peer
    .accept_connection(sa(DRIVER_ADDR), now)
    .expect("the peer admits the inbound exchange");
  for chunk in &push {
    peer.handle_transport_data(server_eid, chunk, false, now);
  }
  peer.handle_transport_data(server_eid, &[], true, now); // the dialer's FIN
  let mut response: Vec<Vec<u8>> = Vec::new();
  for _ in 0..256 {
    let mut progressed = false;
    peer.handle_timeout(now);
    while peer.poll_action().is_some() {
      progressed = true;
    }
    while let Some((id, _peer, bytes)) = peer.poll_transport_transmit() {
      progressed = true;
      if id == server_eid {
        response.push(bytes.to_vec());
      }
    }
    while peer.poll_event().is_some() {
      progressed = true;
    }
    if !progressed {
      break;
    }
  }
  assert!(
    !response.is_empty(),
    "the peer produced a pull response to the dialer's push"
  );
  (eid, response)
}

/// Regression (Gate 1, non-premature AT DEPTH): at a DUE await-result-join
/// deadline, the pull `ExchangeCompleted` that resolves the join `Ok` is buffered
/// on `inbound_rx` behind a DEEP backlog (far more than the old fixed `8`-poll
/// deferral bound), drained one item per poll at `iter_drain_cap == 1`. The exact
/// inbound-depth watermark must defer the reap the WHOLE way — until the completion
/// drains — so the join resolves `Ok`, not a spurious `JoinAllFailed`.
///
/// This is the case the old fixed-count deferral got wrong: at depth `> 8` (or a
/// low `iter_drain_cap` / a large exchange) the count elapsed while the resolving
/// completion was still buffered, force-firing a premature reap. The watermark is a
/// DEPTH, not a count, so it is immune.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_deadline_waits_for_join_completion_behind_deep_backlog() {
  let now = Instant::now();
  // iter_drain_cap = 1 (one inbound item per poll); a bridge cap large enough to
  // hold the whole deep backlog at once.
  let (mut driver, _obs_rx, _shared) = build_driver(1, 4096).await;

  let seed_addr = sa("127.0.0.1:7000");

  // Seed reached: drive its push/pull to a Succeeded whose pull response + peer-FIN
  // we queue on inbound_rx (NOT consumed), so the resolving ExchangeCompleted sits
  // behind the per-poll cap.
  let (eid, response) = drive_push_to_queued_response(&mut driver, seed_addr, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();

  // A dummy 'connecting' exchange — its Connect is captured but never dialed, so no
  // bridge is minted and the machine has no conn for it: transport data keyed by its
  // id is ignored. That gives a benign filler id without touching the real join.
  driver
    .endpoint
    .start_push_pull(sa("127.0.0.1:7001"), PushPullKind::Join, now);
  let mut dummy = None;
  while let Some(action) = driver.endpoint.poll_action() {
    if let StreamAction::Connect(info) = action {
      dummy.get_or_insert(info.id());
    }
  }
  let dummy = dummy.expect("the dummy start_push_pull emitted a Connect exchange id");

  // A DEEP pre-completion backlog — benign `Data` for that dummy exchange, far past
  // the old fixed-8 bound — drained one-per-poll AHEAD of the real completion, each
  // counted in-flight exactly as a bridge would.
  const DEPTH: usize = 40;
  for _ in 0..DEPTH {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: dummy,
        bytes: vec![0u8; 1],
        received_at: now,
      }),
    );
  }
  // The real pull response + peer-FIN EOF (the resolving `ExchangeCompleted` rides
  // the EOF), buffered BEHIND the deep backlog.
  for bytes in response {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid,
      received_at: now,
    }),
  );

  // Park an await-result join awaiting `eid` with a deadline ALREADY in the past,
  // so the deadline reap is due on the very first poll — while the completion that
  // resolves it Ok is far behind the deep backlog.
  let (tx, mut rx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: now - Duration::from_secs(1),
    reply: Some(tx),
  });

  // The `more` self-wake re-polls; a bounded loop generous enough to drain the whole
  // depth (DEPTH + response + EOF at one item per poll). The old fixed-8 deferral
  // would have force-reaped `JoinAllFailed` long before this depth drained.
  let mut resolved = None;
  for _ in 0..(DEPTH + 64) {
    let _ = poll_once(&mut driver);
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("join reply sender dropped without resolving"),
    }
  }

  let reached = resolved
    .expect("the join resolved within the bounded poll budget")
    .expect(
      "the deep-buffered ExchangeCompleted resolved the join Ok before the past-due reap; \
       the old fixed-8 deferral would have force-reaped a spurious JoinAllFailed at depth > 8",
    );
  assert!(
    reached.contains(&seed_addr),
    "the resolved join reached the seed whose completion was behind the deep backlog: {reached:?}"
  );
}

/// Regression (Gate 1 flood-liveness, sub-case i): under a SUSTAINED UDP gossip
/// flood that pins `udp_backlog` every poll, a past-due await-result join whose
/// exchange never completes (nothing ever rides `inbound_rx` for it) must STILL be
/// reaped `JoinAllFailed` PROMPTLY. The reap gates on the reliable `inbound_rx`
/// watermark — clear here, since the flood is UDP and deposits nothing on the
/// reliable plane — NOT on the flood, so `reap_due` fires the very first poll.
///
/// The old fixed-count deferral instead deferred `8` polls under the flood's `more`
/// before firing; the watermark fires immediately because the reliable backlog is
/// already clear (`inflight == 0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_flood_does_not_suppress_stuck_join_reap() {
  let now = Instant::now();
  let cap = 4usize;
  let (mut driver, _obs_rx, _shared) = build_driver(cap, 256).await;

  // A live outbound exchange with NO completion coming (no peer feeds it) and no
  // bridge on `inbound_rx`: the reliable in-flight count stays 0, so the watermark
  // is clear and only the past-due deadline reap can resolve it — to JoinAllFailed.
  driver
    .endpoint
    .start_push_pull(sa("127.0.0.1:7300"), PushPullKind::Join, now);
  let mut eid = None;
  while let Some(action) = driver.endpoint.poll_action() {
    if let StreamAction::Connect(info) = action {
      eid = Some(info.id());
    }
  }
  let eid = eid.expect("start_push_pull emitted a Connect exchange id");

  let (tx, mut rx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: now - Duration::from_secs(1),
    reply: Some(tx),
  });

  let flood_src = sa("127.0.0.1:7301");
  let mut resolved = None;
  let mut polls = 0usize;
  // A tight budget: the watermark-gated reap fires the FIRST poll (reliable backlog
  // clear), far inside a budget the old fixed-8 deferral could not meet.
  for _ in 0..3 {
    for _ in 0..cap {
      driver
        .endpoint
        .handle_gossip(flood_src, &[0xff, 0x00, 0xff], now);
    }
    let _ = poll_once(&mut driver);
    polls += 1;
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("join reply sender dropped without resolving"),
    }
  }

  let reply = resolved.expect(
    "the past-due join reap fired despite the UDP flood; it gates on the reliable \
     watermark, not the UDP flood, so the flood cannot suppress it",
  );
  assert!(
    polls <= 2,
    "the reap fired at once (reliable backlog clear), not after a deferral: {polls} polls",
  );
  match reply {
    Err((ref reached, SerfError::JoinAllFailed(_))) => {
      assert!(reached.is_empty(), "no seed contacted: {reached:?}");
    }
    other => panic!("expected JoinAllFailed from the deadline reap, got {other:?}"),
  }
}

/// Regression (Gate 1 flood-liveness, sub-case ii): under the SAME sustained UDP
/// flood, an await-result join whose resolving `ExchangeCompleted` is buffered on
/// `inbound_rx` (a FUTURE deadline, so ONLY the completion — never a deadline reap —
/// can resolve it) must resolve `Ok`. The flood pins the UDP path but the reliable
/// inbound drain (step 6) runs every poll regardless, so it is never starved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_flood_does_not_starve_inbound_join_completion() {
  let now = Instant::now();
  let cap = 4usize;
  let (mut driver, _obs_rx, _shared) = build_driver(cap, 256).await;

  let seed_addr = sa("127.0.0.1:7002");
  let (eid, response) = drive_push_to_queued_response(&mut driver, seed_addr, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();
  for bytes in response {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid,
      received_at: now,
    }),
  );

  // FUTURE deadline: no deadline reap can fire, so a starved TCP drain would hang
  // the join forever under the flood. Only the completion can resolve it.
  let (tx, mut rx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: now + Duration::from_secs(30),
    reply: Some(tx),
  });

  let flood_src = sa("127.0.0.1:7003");
  let mut resolved = None;
  for _ in 0..256 {
    for _ in 0..cap {
      driver
        .endpoint
        .handle_gossip(flood_src, &[0xff, 0x00, 0xff], now);
    }
    let _ = poll_once(&mut driver);
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("join reply sender dropped without resolving"),
    }
  }

  let reached = resolved
    .expect("the buffered completion resolved despite the UDP flood")
    .expect("the completion resolved the join Ok; the flood did not starve the TCP drain");
  assert!(
    reached.contains(&seed_addr),
    "the resolved join reached the seed whose completion drained under the flood: {reached:?}"
  );
}

/// Drive an INBOUND (server-side) Join push/pull on `driver.endpoint` until its pull
/// response is queued in the coordinator's transmit surface and the terminal
/// `StreamAction::Close` is WITHHELD behind those bytes (the coordinator self-orders
/// a teardown after the exchange's last transmit). A separate dialer endpoint
/// supplies the push frames. The response transmit is deliberately NOT drained here,
/// so the `Close` stays withheld until the pump's own drain runs. Returns the server
/// exchange id.
fn drive_server_to_withheld_close(
  driver: &mut TestDriver,
  dialer_addr: SocketAddr,
  now: Instant,
) -> ExchangeId {
  let mut dialer = build_endpoint("dialer", dialer_addr);
  dialer.start_push_pull(sa(DRIVER_ADDR), PushPullKind::Join, now);
  let mut dialer_eid = None;
  let mut push: Vec<Vec<u8>> = Vec::new();
  for _ in 0..256 {
    let mut progressed = false;
    while let Some(action) = dialer.poll_action() {
      progressed = true;
      if let StreamAction::Connect(info) = action {
        dialer_eid = Some(info.id());
      }
    }
    while let Some((id, _peer, bytes)) = dialer.poll_transport_transmit() {
      progressed = true;
      if Some(id) == dialer_eid {
        push.push(bytes.to_vec());
      }
    }
    if !progressed {
      break;
    }
  }
  assert!(
    !push.is_empty(),
    "the dialer emitted its push frames up front"
  );

  let server_eid = driver
    .endpoint
    .accept_connection(dialer_addr, now)
    .expect("the driver admits the inbound exchange");
  for chunk in &push {
    driver
      .endpoint
      .handle_transport_data(server_eid, chunk, false, now);
  }
  driver
    .endpoint
    .handle_transport_data(server_eid, &[], true, now); // the dialer's FIN

  // Tick the server so it generates the pull response and reaps the exchange
  // cleanly. Do NOT drain actions or transport transmits: the response stays queued
  // and the terminal `Close` stays withheld behind it.
  for _ in 0..32 {
    driver.endpoint.handle_timeout(now);
  }
  server_eid
}

/// Regression (fixed-point drain): a terminal `StreamAction::Close` the coordinator
/// withholds behind an exchange's final transport transmit must be released AND
/// processed within the SAME poll — the transport surface pops the response
/// (releasing the `Close`), and the fixed-point re-pass drains the `Close`, closing
/// the bridge. A single ordered pass would leave the `Close` buffered, so the pump
/// would report false quiescence and return `Pending` with the TCP bridge still open
/// until an unrelated wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_point_drain_releases_withheld_close_same_poll() {
  let now = Instant::now();
  // The default (large) cap: no surface hits its cap, so a false quiescence could
  // come ONLY from the single-pass ordering — isolating the fixed-point fix.
  let cap = RuntimeOptions::new().iter_drain_cap();
  let (mut driver, _obs_rx, _shared) = build_driver(cap, 256).await;

  let dialer_addr = sa("127.0.0.1:7400");
  let server_eid = drive_server_to_withheld_close(&mut driver, dialer_addr, now);

  // Register the pump's bridge handle for the server exchange as the real accept
  // path would: the withheld `Close` targets this handle, and the response transmit
  // routes to `out_rx`.
  let (out_tx, out_rx) = flume::unbounded::<BridgeOut>();
  let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
  driver
    .bridges
    .insert(server_eid, BridgeHandle { out_tx, cancel_tx });
  assert!(
    driver.bridges.contains_key(&server_eid),
    "precondition: the server bridge is registered"
  );

  // ONE poll. The fixed-point drain must pop the response transmit (releasing the
  // withheld `Close`) and then drain that `Close`, removing the bridge — all here.
  let _ = poll_once(&mut driver);

  assert!(
    !driver.bridges.contains_key(&server_eid),
    "the fixed-point surface drain released and processed the withheld Close in the \
     same poll; a single-pass drain would leave the TCP bridge open past this poll"
  );
  assert!(
    matches!(out_rx.try_recv(), Ok(BridgeOut::Data(_))),
    "the pull response transmit routed to the bridge before its Close"
  );
}

/// Regression (leave-before-reap ordering): a same-poll graceful leave whose
/// `LeftCluster` is emitted BY `handle_timeout` inside the fire path must resolve
/// `Ok`, not `LeaveTimeout`. The fire path folds the uncapped `poll_event` surface
/// BETWEEN `handle_timeout` and `reap_pending_leave`, so the fresh `LeftCluster`
/// resolves the parked leave before the reap sees a still-parked leave at a past
/// deadline. Without that intervening fold the reap would fire a false
/// `LeaveTimeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leave_left_cluster_from_handle_timeout_folds_before_reap() {
  let real_now = Instant::now();
  // A base well in the past so the (default 1s) leave-propagate deadline, armed
  // relative to `base`, is already elapsed at the real-time driver poll — while the
  // pump instants below stay UNDER it, so the Leaving→Left transition is withheld
  // until the driver poll's `handle_timeout`.
  let base = real_now - Duration::from_secs(5);
  let (mut driver, _obs_rx, _shared) = build_driver(64, 256).await;

  driver.endpoint.leave(base).expect("leave from Alive");
  // On a lone node the inner memberlist emits `LeftCluster` immediately; the first
  // `handle_timeout` sieves it and ARMS the serf leave-complete deadline. Pump at
  // instants held below that deadline so serf stays `Leaving` (deadline armed but
  // not yet fired), draining the pre-`LeftCluster` serf events as we go.
  let mut armed = false;
  for i in 0..64u32 {
    let t = base + Duration::from_millis(i as u64);
    driver.endpoint.handle_timeout(t);
    while driver.endpoint.poll_action().is_some() {}
    while driver.endpoint.poll_transport_transmit().is_some() {}
    while driver.endpoint.poll_memberlist_transmit().is_some() {}
    while driver.endpoint.poll_event().is_some() {}
    if driver.endpoint.leave_complete_deadline().is_some() {
      armed = true;
      break;
    }
  }
  assert!(
    armed,
    "the inner leave armed the serf leave-complete deadline while still Leaving"
  );

  // Park a leave waiter with an ALREADY-PAST deadline: at the driver poll both the
  // endpoint deadline (the armed, now-elapsed leave-complete deadline) and this
  // leave deadline are due, so the fire path runs handle_timeout (emitting
  // LeftCluster) THEN would reap the leave — the ordering under test.
  let (tx, mut rx) = oneshot::channel();
  driver.pending_leave = Some(PendingLeave {
    repliers: vec![tx],
    deadline: base,
  });

  let mut resolved = None;
  for _ in 0..64 {
    let _ = poll_once(&mut driver);
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("leave reply sender dropped without resolving"),
    }
  }

  resolved
    .expect("the leave resolved within the bounded poll budget")
    .expect(
      "the LeftCluster emitted by handle_timeout was folded before reap_pending_leave, \
       resolving the leave Ok; without the intervening fold the reap fires LeaveTimeout",
    );
}

/// Regression (parked saturated hand-off): a resolving completion the bridge has
/// READ (`received_at < deadline`) but that is still PARKED on a SATURATED
/// `inbound_rx` hand-off lives OUTSIDE `inbound_rx.len()`. It is accounted in the
/// watermark via the in-flight reservation, so a past-due join deadline does NOT
/// prematurely reap `JoinAllFailed` — the parked completion drains and resolves the
/// join `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_saturated_handoff_completion_is_accounted_not_reaped() {
  let now = Instant::now();
  // bridge_inbound_cap = 1 forces every hand-off after the first to PARK; one inbound
  // item per poll makes the parked completion strictly lag the past-due deadline.
  let (mut driver, _obs_rx, _shared) = build_driver(1, 1).await;

  let seed_addr = sa("127.0.0.1:7005");
  let (eid, response) = drive_push_to_queued_response(&mut driver, seed_addr, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();

  // The frames the parked bridge would deliver: the pull response + the peer-FIN EOF
  // that completes the exchange. They stay OUT of inbound_rx (the parked residence),
  // delivered one-at-a-time as the pump frees the cap-1 slot below.
  let mut parked_frames: std::collections::VecDeque<BridgeInbound> = response
    .into_iter()
    .map(|bytes| {
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      })
    })
    .collect();
  parked_frames.push_back(BridgeInbound::Eof(BridgeEof {
    eid,
    received_at: now,
  }));

  // RESERVE the in-flight count for EVERY held frame up front — exactly the
  // reservation a real `bridge_task` accrues via its pre-send bump — while the
  // frames themselves are NOT yet in inbound_rx. That is the residence a raw
  // `inbound_rx.len()` watermark misses: a completion read (`received_at < deadline`)
  // but still parked on a saturated `send_async`, so `inflight > len`.
  for _ in 0..parked_frames.len() {
    driver
      .bridge_inbound_inflight
      .fetch_add(1, Ordering::Release);
  }

  // Past deadline: only the watermark accounting for the parked completion stands
  // between the pump and a premature JoinAllFailed.
  let (tx, mut rx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: now - Duration::from_secs(1),
    reply: Some(tx),
  });

  let mut resolved = None;
  for _ in 0..512 {
    // Land the next parked frame into the freed cap-1 slot — a deterministic,
    // race-free stand-in for a bridge whose `send_async` unparks as the pump drains.
    // The watermark sees the identical state a real parked hand-off produces
    // (`inflight` reserved for the un-landed frames, `len` only the one in flight).
    if !inbound_tx.is_full()
      && let Some(frame) = parked_frames.pop_front()
    {
      inbound_tx.try_send(frame).expect("land parked frame");
    }
    let _ = poll_once(&mut driver);
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("join reply sender dropped without resolving"),
    }
  }

  let reached = resolved
    .expect("the join resolved within the bounded poll budget")
    .expect(
      "the parked completion was accounted in the watermark and folded before the past-due reap; \
       a raw inbound_rx.len() watermark would miss it and reap a spurious JoinAllFailed",
    );
  assert!(
    reached.contains(&seed_addr),
    "the resolved join reached the seed whose completion was parked on the saturated hand-off: {reached:?}"
  );
}

/// Regression (overlapping deadlines, Gate 1): a SECOND await-result join whose
/// resolving completion is buffered on `inbound_rx` AFTER the FIRST join's
/// (earlier) deadline crossing snapshotted the reap watermark, but BEFORE the
/// second join's own (later) deadline elapses. A single sticky watermark taken at
/// the first crossing (`W0`) covers only the first join's backlog, so once the
/// later deadline also elapses the shared fire path reaps BOTH joins against `W0`
/// — spuriously failing the later join whose `Ok` completion is still queued
/// behind `W0`. Re-snapshotting the watermark when the latest due deadline
/// advances widens the target to cover the later join's backlog, so both resolve
/// `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_deadlines_later_join_waits_for_its_own_backlog() {
  let now = Instant::now();
  // iter_drain_cap = 1: one inbound item per poll, so W0 drains slowly enough that
  // the later deadline elapses mid-drain (the fire poll then sees both due).
  let (mut driver, _obs_rx, _shared) = build_driver(1, 4096).await;

  // Two reached seeds; each drives an outbound push/pull to a Succeeded whose pull
  // response + peer-FIN we queue on inbound_rx.
  let seed_a = sa("127.0.0.1:7010");
  let seed_b = sa("127.0.0.1:7011");
  let (eid_a, resp_a) = drive_push_to_queued_response(&mut driver, seed_a, now);
  let (eid_b, resp_b) = drive_push_to_queued_response(&mut driver, seed_b, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();

  // A dummy 'connecting' exchange for benign filler (its transport data is dropped
  // by the machine — no conn), deepening the first join's backlog.
  driver
    .endpoint
    .start_push_pull(sa("127.0.0.1:7012"), PushPullKind::Join, now);
  let mut dummy = None;
  while let Some(action) = driver.endpoint.poll_action() {
    if let StreamAction::Connect(info) = action {
      dummy.get_or_insert(info.id());
    }
  }
  let dummy = dummy.expect("the dummy start_push_pull emitted a Connect exchange id");

  // Backlog + join A's completion queued FIRST: this is the depth W0 will cover.
  // Join B's completion is deliberately NOT queued yet.
  const DEPTH: usize = 24;
  for _ in 0..DEPTH {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: dummy,
        bytes: vec![0u8; 1],
        received_at: now,
      }),
    );
  }
  for bytes in resp_a {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: eid_a,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid: eid_a,
      received_at: now,
    }),
  );

  // Join A: deadline ALREADY past → due on the first poll, snapshotting W0 to the
  // depth queued above. Join B: deadline in the near FUTURE (captured fresh so a
  // slow setup cannot backdate it), so on that first poll B does NOT yet widen the
  // watermark.
  let t_ref = Instant::now();
  let future = Duration::from_millis(100);
  let (tx_a, mut rx_a) = oneshot::channel::<JoinReply>();
  let mut pending_a = HashSet::new();
  pending_a.insert(eid_a);
  driver.pending_joins.push(PendingJoin {
    pending: pending_a,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: t_ref - Duration::from_secs(1),
    reply: Some(tx_a),
  });
  let (tx_b, mut rx_b) = oneshot::channel::<JoinReply>();
  let mut pending_b = HashSet::new();
  pending_b.insert(eid_b);
  driver.pending_joins.push(PendingJoin {
    pending: pending_b,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: t_ref + future,
    reply: Some(tx_b),
  });

  // One poll snapshots W0 at join A's crossing (join B still future), covering only
  // the depth queued so far — NOT join B's completion.
  let _ = poll_once(&mut driver);

  // NOW queue join B's completion, strictly BEHIND W0 and BEFORE its own deadline
  // `t_ref + future` (which has not yet elapsed).
  for bytes in resp_b {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: eid_b,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid: eid_b,
      received_at: now,
    }),
  );

  // Let join B's deadline elapse while W0 is still draining: the next fire poll now
  // sees BOTH deadlines due. A single sticky W0 reaps join B before its buffered
  // completion drains; the re-snapshotted watermark defers until it does.
  tokio::time::sleep(future + Duration::from_millis(50)).await;

  let mut reached_a = None;
  let mut reached_b = None;
  for _ in 0..(DEPTH + 128) {
    let _ = poll_once(&mut driver);
    if reached_a.is_none()
      && let Ok(Some(reply)) = rx_a.try_recv()
    {
      reached_a = Some(reply);
    }
    if reached_b.is_none()
      && let Ok(Some(reply)) = rx_b.try_recv()
    {
      reached_b = Some(reply);
    }
    if reached_a.is_some() && reached_b.is_some() {
      break;
    }
  }

  let reached_a = reached_a
    .expect("join A resolved within the poll budget")
    .expect("join A (earlier, past deadline) resolved Ok from its buffered completion");
  assert!(
    reached_a.contains(&seed_a),
    "join A reached seed A: {reached_a:?}"
  );
  let reached_b = reached_b
    .expect("join B resolved within the poll budget")
    .expect(
      "join B (later deadline) resolved Ok — its completion, queued behind W0 but before its own \
       deadline, was covered by the re-snapshotted watermark; a single sticky W0 reaps a spurious \
       JoinAllFailed here",
    );
  assert!(
    reached_b.contains(&seed_b),
    "join B reached seed B: {reached_b:?}"
  );
}

/// Regression (overlapping deadlines, leave analog): the EARLIER due deadline is a
/// graceful-leave deadline and the LATER one an await-result join whose completion
/// is buffered behind the watermark the leave's crossing snapshotted. The max due
/// deadline the re-snapshot tracks must span both planes (join AND leave), so the
/// leave's earlier crossing does not pin the watermark and starve the later join
/// into a spurious `JoinAllFailed`. The leave itself times out on its own past
/// deadline (not under test); the join must resolve `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_deadlines_leave_then_join_waits_for_join_backlog() {
  let now = Instant::now();
  let (mut driver, _obs_rx, _shared) = build_driver(1, 4096).await;

  let seed = sa("127.0.0.1:7020");
  let (eid, resp) = drive_push_to_queued_response(&mut driver, seed, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();

  driver
    .endpoint
    .start_push_pull(sa("127.0.0.1:7021"), PushPullKind::Join, now);
  let mut dummy = None;
  while let Some(action) = driver.endpoint.poll_action() {
    if let StreamAction::Connect(info) = action {
      dummy.get_or_insert(info.id());
    }
  }
  let dummy = dummy.expect("the dummy start_push_pull emitted a Connect exchange id");

  // Backlog queued FIRST (the depth the leave's crossing snapshots). The join's
  // completion is NOT queued yet.
  const DEPTH: usize = 24;
  for _ in 0..DEPTH {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: dummy,
        bytes: vec![0u8; 1],
        received_at: now,
      }),
    );
  }

  // A leave with a PAST deadline (the earlier due deadline that snapshots the
  // watermark) and a join with a FUTURE deadline (so its completion, queued after
  // the snapshot, must be covered when its own deadline later elapses).
  let t_ref = Instant::now();
  let future = Duration::from_millis(100);
  let (ltx, _lrx) = oneshot::channel::<Result<()>>();
  driver.pending_leave = Some(PendingLeave {
    repliers: vec![ltx],
    deadline: t_ref - Duration::from_secs(1),
  });
  let (jtx, mut jrx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: t_ref + future,
    reply: Some(jtx),
  });

  // One poll snapshots the watermark at the leave's past-deadline crossing (the
  // join is still future), covering only the filler backlog — not the join.
  let _ = poll_once(&mut driver);

  // Queue the join's completion BEHIND the watermark and BEFORE its own deadline.
  for bytes in resp {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid,
      received_at: now,
    }),
  );

  // Let the join deadline elapse while the watermark is still draining.
  tokio::time::sleep(future + Duration::from_millis(50)).await;

  let mut resolved = None;
  for _ in 0..(DEPTH + 128) {
    let _ = poll_once(&mut driver);
    if let Ok(Some(reply)) = jrx.try_recv() {
      resolved = Some(reply);
      break;
    }
  }

  let reached = resolved
    .expect("the join resolved within the poll budget")
    .expect(
      "the join (later deadline) resolved Ok — the leave's earlier crossing did not pin the \
       watermark; the max-due re-snapshot covered the join's backlog before the reap",
    );
  assert!(
    reached.contains(&seed),
    "the join reached its seed despite the overlapping earlier leave deadline: {reached:?}"
  );
}

/// Regression (two-clock reconciliation): an await-result join configured with a
/// `join_deadline` GREATER than the reliable `stream_timeout` must not resolve a
/// premature `JoinAllFailed` when the join's own exchange deadline elapses ahead of
/// the (public) caller deadline.
///
/// A join carries two clocks: the driver [`PendingJoin::deadline`] (from the caller)
/// and the coordinator's per-exchange deadline (`now + stream_timeout`). The exchange
/// deadline is HIDDEN behind an earlier endpoint deadline in `poll_timeout`'s min
/// (here a second push/pull started earlier), so it never widens the reap watermark
/// on its own — only a join whose OWN deadline goes due re-snapshots it. With the raw
/// (far-future) caller deadline, that earlier endpoint deadline snapshots `W0`, the
/// shared `handle_timeout` then fires the elapsed exchange deadline as
/// `ExchangeCompleted(Failed)` once `W0` drains, and the join reaps a spurious
/// `JoinAllFailed` while its `Succeeded` completion — queued behind `W0`, arrived
/// before the exchange deadline — is still buffered. [`clamp_join_deadline`] caps the
/// driver deadline at the exchange deadline, so the join's deadline goes due WITH the
/// exchange and re-snapshots the watermark to cover that completion; the join then
/// resolves `Ok`. Reverting the clamp to the raw caller deadline reproduces the
/// premature failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_deadline_above_stream_timeout_does_not_reap_premature_failure() {
  let now = Instant::now();
  // A SHORT stream_timeout so the coordinator stamps a near exchange deadline the
  // test can let elapse; applied to BOTH the endpoint and the driver's clamp.
  let stream_timeout = Duration::from_millis(200);
  // The offset between the earlier (dummy) exchange deadline and the join's, wide
  // enough that the `W0`-snapshot poll lands between them under CI jitter.
  let gap = Duration::from_millis(300);
  // iter_drain_cap = 1: `W0` drains one item per poll, so the join's exchange
  // deadline elapses mid-drain (the fire poll then sees it due behind `W0`).
  let (mut driver, _obs_rx, _shared) =
    build_driver_with_stream_timeout(1, 4096, stream_timeout).await;

  // An EARLIER endpoint deadline: a dummy 'connecting' push/pull started at `now`
  // (its Connect captured, never dialed, so its transport data is dropped). Its
  // exchange deadline `T_early = now + stream_timeout` is the min `poll_timeout`
  // exposes, hiding the later join exchange deadline behind it.
  driver
    .endpoint
    .start_push_pull(sa("127.0.0.1:7031"), PushPullKind::Join, now);
  let mut dummy = None;
  while let Some(action) = driver.endpoint.poll_action() {
    if let StreamAction::Connect(info) = action {
      dummy.get_or_insert(info.id());
    }
  }
  let dummy = dummy.expect("the dummy start_push_pull emitted a Connect exchange id");

  // The real join push/pull, started `gap` LATER so its exchange deadline
  // `T_exch = now + gap + stream_timeout` sits strictly AFTER the dummy's — hidden
  // behind it in `poll_timeout`'s min.
  let seed = sa("127.0.0.1:7030");
  let (eid, response) = drive_push_to_queued_response(&mut driver, seed, now + gap);
  let response_len = response.len();
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();

  // Backlog for the dummy exchange, queued FIRST: the depth `W0` covers.
  const DEPTH: usize = 24;
  for _ in 0..DEPTH {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid: dummy,
        bytes: vec![0u8; 1],
        received_at: now,
      }),
    );
  }

  // Park the await-result join with a caller deadline FAR in the future
  // (`join_deadline` = 20s > stream_timeout), reconciled by `clamp_join_deadline` to
  // the join's exchange deadline (`now + gap + stream_timeout`). The dispatch path
  // applies this same clamp; reverting its body to the raw caller deadline
  // reproduces the premature `JoinAllFailed`.
  let (tx, mut rx) = oneshot::channel::<JoinReply>();
  let mut pending = HashSet::new();
  pending.insert(eid);
  driver.pending_joins.push(PendingJoin {
    pending,
    contacted: SmallVec::new(),
    ignore_streams: SmallVec::new(),
    requested: 1,
    deadline: clamp_join_deadline(now + Duration::from_secs(20), now + gap, stream_timeout),
    reply: Some(tx),
  });

  // Advance past the dummy's exchange deadline (`T_early`) but BEFORE the join's
  // (`T_exch`): one poll snapshots `W0` to the dummy backlog only. cap = 1 keeps
  // `handle_timeout` from firing (`W0` not yet drained), so `T_early` stays armed.
  tokio::time::sleep(stream_timeout + Duration::from_millis(50)).await;
  let _ = poll_once(&mut driver);

  // Queue the join's `Succeeded` completion NOW — strictly BEHIND `W0`, and by
  // `received_at` before the join's own exchange deadline.
  for bytes in response {
    queue_inbound(
      &driver,
      &inbound_tx,
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      }),
    );
  }
  queue_inbound(
    &driver,
    &inbound_tx,
    BridgeInbound::Eof(BridgeEof {
      eid,
      received_at: now,
    }),
  );

  // Let the join's exchange deadline (`T_exch`) elapse while `W0` is still draining:
  // `handle_timeout` would now fire it as `ExchangeCompleted(Failed)`. The clamped
  // driver deadline is due WITH it, re-snapshotting the watermark to cover the
  // completion queued above; the raw caller deadline would not.
  tokio::time::sleep(gap + Duration::from_millis(50)).await;

  let mut resolved = None;
  for _ in 0..(DEPTH + response_len + 128) {
    let _ = poll_once(&mut driver);
    match rx.try_recv() {
      Ok(Some(reply)) => {
        resolved = Some(reply);
        break;
      }
      Ok(None) => {}
      Err(_) => panic!("join reply sender dropped without resolving"),
    }
  }

  let reached = resolved
    .expect("the join resolved within the bounded poll budget")
    .expect(
      "a join_deadline above the reliable stream_timeout must not reap a premature JoinAllFailed: \
       the clamped driver deadline goes due WITH the exchange deadline, re-snapshotting the \
       watermark to cover the Succeeded completion queued behind W0; the raw caller deadline \
       reaps a spurious failure",
    );
  assert!(
    reached.contains(&seed),
    "the join reached its seed once the clamped deadline widened the watermark: {reached:?}"
  );
}

/// The retained-farewell retry is epoch-gated: while `retry_after` lies in the
/// future, ANY number of pump polls — including back-to-back self-wake
/// re-polls — leaves a retained datagram untouched (a re-poll is NOT a
/// temporally distinct sample of the socket's error slot, so it must not burn
/// the bounded ICMP allowance); once the epoch elapses, the next poll's single
/// hoisted retry sends it and the drain empties.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn farewell_retry_waits_for_its_epoch_across_polls() {
  let (mut driver, _obs_rx, _shared) = build_driver(8, 8).await;
  // Address the retained farewell to the driver's own bound socket, so an
  // ELIGIBLE retry deterministically completes `Ready(Ok)` on loopback.
  let dest = driver
    .socket
    .as_ref()
    .expect("gossip socket held")
    .local_addr()
    .expect("gossip socket local addr");
  driver.leave_initiated = true;
  driver
    .leave_drain
    .push_back(crate::driver::shared::LeaveDatagram::for_tests(
      dest,
      b"farewell".to_vec(),
      1,
    ));
  driver.farewell.retry_after = Some(Instant::now() + Duration::from_secs(3600));

  for i in 0..8 {
    let _ = poll_once(&mut driver);
    assert_eq!(
      driver.leave_drain.len(),
      1,
      "poll {i}: the epoch gate must hold the retained farewell across re-polls"
    );
  }

  // The epoch elapses: an eligible retry hands the datagram to the socket.
  // The send can legitimately return `Pending` while the fresh socket's
  // writable readiness has not yet reached the reactor (production advances
  // on the registered writable wake), so drive bounded wake iterations
  // rather than demanding completion on one poll.
  driver.farewell.retry_after = Some(Instant::now());
  let mut sent = false;
  for _ in 0..64 {
    let _ = poll_once(&mut driver);
    if driver.leave_drain.is_empty() {
      sent = true;
      break;
    }
    TokioRuntime::sleep(Duration::from_millis(5)).await;
  }
  assert!(
    sent,
    "an eligible retry must hand the retained farewell to the socket"
  );
  assert!(
    !driver.farewell.send_failed,
    "a completed loopback send must not fail the leave"
  );
}
