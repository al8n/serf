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
    StreamTransportOptions::new(),
    None,
    #[cfg(encryption)]
    Arc::new(crate::VoidKeyringDelegate),
  );
  (driver, obs_rx, shared)
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

/// Regression: at a DUE await-result-join deadline, the pull `ExchangeCompleted`
/// that resolves the join `Ok` is queued one bridge-inbound item behind the
/// per-poll `iter_drain_cap`. The single `handle_timeout` site and the join
/// deadline reap must wait for that pre-deadline completion to drain — a premature
/// reap would surface a spurious `JoinAllFailed` against a seed that was in fact
/// reached.
///
/// Pre-fix, the first poll (bridge-inbound cap hit → `more`) still runs the
/// past-due reap and replies `JoinAllFailed`. Post-fix, the timer + reap are gated
/// on quiescence, so the completion drains first and the join resolves `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_deadline_waits_for_join_completion_behind_iter_drain_cap() {
  let now = Instant::now();
  // iter_drain_cap = 1: the bridge-inbound loop processes at most one item per
  // poll, so the completion staged behind the response chunk(s) is at least one
  // poll behind at the due deadline.
  let (mut driver, _obs_rx, _shared) = build_driver(1).await;

  let seed_addr = sa("127.0.0.1:7000");

  // Seed reached: drive its push/pull to a Succeeded whose pull response + peer-FIN
  // we queue on inbound_rx (NOT consumed), so the resolving ExchangeCompleted sits
  // behind the per-poll cap.
  let (eid, response) = drive_push_to_queued_response(&mut driver, seed_addr, now);
  let inbound_tx = driver.inbound_tx.as_ref().expect("template alive").clone();
  for bytes in response {
    inbound_tx
      .try_send(BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at: now,
      }))
      .expect("queue inbound response");
  }
  inbound_tx
    .try_send(BridgeInbound::Eof(BridgeEof {
      eid,
      received_at: now,
    }))
    .expect("queue inbound EOF");

  // Park an await-result join awaiting `eid` with a deadline ALREADY in the past,
  // so the deadline reap is due on the very first poll — while the completion that
  // resolves it Ok is still queued behind the cap.
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

  // Drive the pump by hand. The `more` self-wake re-polls, so a bounded loop drains
  // the staged completion. Pre-fix, the first poll reaps the past-due deadline
  // (JoinAllFailed); post-fix, the reap waits until the completion resolves Ok.
  let mut resolved = None;
  for _ in 0..256 {
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
      "the ready ExchangeCompleted resolved the join before the past-due deadline reap fired; \
       a premature reap would surface a spurious JoinAllFailed",
    );
  assert!(
    reached.contains(&seed_addr),
    "the resolved join reached the seed whose completion was queued behind the cap: {reached:?}"
  );
}
