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

fn sa(s: &str) -> SocketAddr {
  s.parse().expect("loopback addr")
}

/// Drive the driver through exactly one `Future::poll`. The manual poll loop
/// re-polls unconditionally, so the no-op waker is a valid, harmless sink.
fn poll_once(driver: &mut TestDriver) -> Poll<()> {
  let mut cx = Context::from_waker(Waker::noop());
  Pin::new(driver).poll(&mut cx)
}

/// Build a serf `StreamEndpoint<SmolStr, SocketAddr, RawRecords>` rooted at `id` /
/// `advertise`, mirroring the production construction (memberlist inner endpoint →
/// reliable coordinator → serf super-machine). Seeded deterministically; the
/// initial local `NodeJoined` self-event is drained so a caller starts clean.
fn build_endpoint(
  id: &str,
  advertise: SocketAddr,
) -> StreamEndpoint<SmolStr, SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(SmolStr::new(id), advertise)
    .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
  let inner = Endpoint::new(inner_opts, SmallRng::seed_from_u64(0));
  let coord = Coordinator::<_, _, RawRecords>::new(
    inner,
    LabelOptions::new_in(Some(CLUSTER.to_vec()), ()),
    Box::new(|_addr: &SocketAddr| None),
    Box::new(|addr: &SocketAddr| *addr),
  );
  let mut e = StreamEndpoint::new(coord, SerfOptions::new());
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
  let socket = <<TokioRuntime as Runtime>::Net as Net>::UdpSocket::bind("127.0.0.1:0")
    .await
    .expect("bind gossip socket");
  let endpoint = build_endpoint("drv", sa(DRIVER_ADDR));
  let shared = Arc::new(Shared::new(initial_snapshot("drv", sa(DRIVER_ADDR))));
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
