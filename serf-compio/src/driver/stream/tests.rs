//! Unit tests for the stream pump's past-due gossip decode.
//!
//! The bounded past-due UDP drain itself (reading every queued datagram, not
//! just the first) is shared with the QUIC pump and proven in the driver's
//! `shared::tests`. This file covers the stream-specific per-datagram step: the
//! past-due drain hands each datagram to `dispatch_gossip`, which decodes it
//! inline (unlike QUIC, which buffers and decodes in `drain_ingress`), so a
//! drained Ack resolves its probe before the deadline recheck fires
//! `handle_timeout`.

use super::*;

use core::num::NonZeroU8;

use memberlist_proto::{
  Endpoint, EndpointOptions, RawRecords,
  streams::{LabelOptions, StreamEndpoint as Coordinator},
};
use rand::rngs::StdRng;
use serf_proto::options::Options as SerfOptions;
use smol_str::SmolStr;

/// Build a standalone plain-TCP serf `StreamEndpoint` over a memberlist stream
/// coordinator — no bound socket, no driver loop; just the composed machine, for
/// driving the gossip ingress surface directly. Mirrors the coordinator the TCP
/// transport's `run` builds.
fn build_endpoint() -> StreamEndpoint<SmolStr, SocketAddr, RawRecords, StdRng, StdRng> {
  let local: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let inner_opts = EndpointOptions::new(SmolStr::new("node"), local)
    .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
  let inner = Endpoint::new(inner_opts, StdRng::seed_from_u64(1));
  let coord = Coordinator::<_, _, RawRecords, StdRng>::new(
    inner,
    LabelOptions::new_in(None::<Vec<u8>>, ()),
    Box::new(|_: &SocketAddr| None),
    Box::new(|addr: &SocketAddr| *addr),
  );
  StreamEndpoint::<SmolStr, SocketAddr, RawRecords, StdRng, StdRng>::new_with_rng(
    coord,
    SerfOptions::new(),
    StdRng::seed_from_u64(2),
  )
}

/// `dispatch_gossip` must fully drain the coordinator's memberlist ingress queue
/// inline, so the stream pump's past-due drain leaves nothing buffered for a
/// subsequent `handle_timeout` to race: a drained Ack is decoded and fed back
/// through `handle_message` before the suspicion sweep. A first byte of 1 (the
/// `Compound` message tag) is buffered by `handle_gossip` and consumed by the
/// inline ingress drain inside `dispatch_gossip`.
#[test]
fn dispatch_gossip_drains_ingress_before_timeout() {
  let mut endpoint = build_endpoint();
  let peer: SocketAddr = "127.0.0.1:65000".parse().expect("peer addr");
  let now = Instant::now();

  dispatch_gossip::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    peer,
    &[1u8, 0, 0, 0],
    now,
    None,
  );

  assert!(
    endpoint.poll_memberlist_ingress().is_none(),
    "dispatch_gossip must drain memberlist ingress inline so no buffered frame is left for \
     handle_timeout to race"
  );
}

/// The drain-first timeout chokepoint must read the gossip UDP socket BEFORE
/// deciding on `handle_timeout`: on a completion backend a freshly-submitted recv
/// is pending on first poll, so the main select's timer arm winning is NOT proof
/// of a would-block — a near-deadline Ack can be queued. `fire_timeout_with_drain`
/// is the single `handle_timeout` site, reached from both the past-due branch and
/// the main timer arm; this proves it actually invokes the folded-in UDP drain on
/// the real socket. The socket-level proof that the drain reads EVERY queued
/// datagram is in the driver's `shared::tests`.
#[compio::test]
async fn fire_timeout_with_drain_drains_socket_before_handle_timeout() {
  let mut endpoint = build_endpoint();
  let any: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let driver = UdpSocket::bind(any).await.expect("bind driver socket");
  let driver_addr = driver.local_addr().expect("driver local_addr");
  let peer = UdpSocket::bind(any).await.expect("bind peer socket");

  // A single gossip compound-tagged datagram queued in the driver's socket — the
  // near-deadline "Ack" the chokepoint must consume before any suspicion.
  peer
    .send_to(vec![1u8, 0, 0, 0], driver_addr)
    .await
    .0
    .expect("queue a datagram");
  // Let loopback delivery settle so the drain's eager recv reads the datagram on
  // its first poll (production's past-due Ack is already buffered).
  compio::time::sleep(Duration::from_millis(100)).await;

  // The bridge plumbing the chokepoint also drains is empty here; this test
  // targets the folded-in UDP drain.
  let mut bridges: HashMap<ExchangeId, BridgeHandle> = HashMap::new();
  let (bridge_inbound_tx, mut bridge_inbound_rx) = mpsc::bounded::<BridgeInbound>(16);
  let (_bridge_ready_tx, bridge_ready_rx) = flume::unbounded::<BridgeReady>();

  let opts = RuntimeOptions::new();
  let dirty = fire_timeout_with_drain::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    &mut bridges,
    &bridge_inbound_tx,
    &mut bridge_inbound_rx,
    &bridge_ready_rx,
    &driver,
    64,
    &None,
    opts,
    StreamTransportOptions::new(),
  )
  .await;
  assert!(
    dirty,
    "the chokepoint consumed the queued datagram (and/or fired handle_timeout)"
  );

  // The datagram was drained off the socket before handle_timeout: a fresh
  // bounded recv now BLOCKS (the timer wins) rather than returning the
  // still-queued datagram immediately — proving the chokepoint read the socket.
  // Scope the recv future so it drops (releasing its borrow of `driver`) before
  // the socket is closed below.
  let socket_drained = {
    let buf = vec![0u8; 64];
    let recv = driver.recv_from(buf).fuse();
    let timer = compio::time::sleep(Duration::from_millis(200)).fuse();
    pin_mut!(recv, timer);
    select_biased! {
      _ = recv => false,
      _ = timer => true,
    }
  };
  assert!(
    socket_drained,
    "fire_timeout_with_drain must drain the queued datagram off the socket before handle_timeout"
  );

  // Ignoring Err: test cleanup of the probe sockets.
  let _ = driver.close().await;
  let _ = peer.close().await;
}

/// Park one `ignore_old` await-result `PendingJoin` whose single exchange is
/// still live, with its `StreamId` recorded in the machine's ignore set and a
/// `deadline` already in the past. Returns the join's `(StreamId, ExchangeId)`
/// and the oneshot receiver the caller awaits.
fn park_ignore_old_join(
  endpoint: &mut StreamEndpoint<SmolStr, SocketAddr, RawRecords, StdRng, StdRng>,
  pending_joins: &mut Vec<PendingJoin>,
  deadline: Instant,
) -> (
  StreamId,
  ExchangeId,
  futures_channel::oneshot::Receiver<JoinReply>,
) {
  let seed: SocketAddr = "127.0.0.1:7946".parse().expect("seed addr");
  // `ignore_old = true` records the returned `StreamId` in the machine's
  // per-exchange ignore set — the entry whose premature clear is the bug.
  let sid = endpoint.start_join_push_pull(seed, true, Instant::now());
  let eid = ExchangeId::from(sid);
  let (tx, rx) = futures_channel::oneshot::channel::<JoinReply>();
  pending_joins.push(PendingJoin {
    pending: core::iter::once(eid).collect(),
    contacted: SmallVec::new(),
    ignore_streams: core::iter::once(sid).collect(),
    requested: 1,
    deadline,
    reply: Some(tx),
  });
  (sid, eid, rx)
}

/// The deadline reaper must NOT clear an `ignore_old` join's ignore `StreamId`
/// while its push/pull exchange is still live: it replies to the caller (deadline
/// path) but the waiter LINGERS with its `StreamId` recorded, so a late merge for
/// that still-live exchange still suppresses the peer's pre-join user events. Only
/// when the exchange finally completes is the waiter reaped and its ignore stream
/// cleared. This is the premature-clear / cancellation-safety regression.
#[compio::test]
async fn deadline_reap_keeps_ignore_stream_until_exchange_completes() {
  let mut endpoint = build_endpoint();
  let mut joins: Vec<PendingJoin> = Vec::new();
  // Deadline already elapsed; the exchange is still pending.
  let past = Instant::now() - Duration::from_secs(1);
  let (sid, eid, rx) = park_ignore_old_join(&mut endpoint, &mut joins, past);

  // Reap on the elapsed deadline. Reply resolution is decoupled from ignore-stream
  // cleanup: the caller is answered, but the waiter must linger.
  reap_pending_joins::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    &mut joins,
    Instant::now(),
  )
  .await;

  // The caller got the deadline reply (zero contacts -> JoinAllFailed) ...
  match rx.await {
    Ok(Err((_set, SerfError::JoinAllFailed(_)))) => {}
    other => panic!("deadline reap must reply JoinAllFailed, got {other:?}"),
  }
  // ... but the waiter LINGERED rather than being removed, and its ignore
  // `StreamId` was NOT cleared — it stays recorded for the still-live exchange so
  // a late merge for that exchange still suppresses the peer's pre-join user
  // events. Removing the waiter and clearing the stream here would let that late
  // merge replay them.
  assert_eq!(
    joins.len(),
    1,
    "the waiter must linger past its reply while the exchange is still live"
  );
  assert!(
    joins[0].reply.is_none(),
    "the deadline reply must have resolved (reply taken)"
  );
  assert_eq!(
    joins[0].ignore_streams.as_slice(),
    &[sid],
    "the ignore StreamId must stay recorded for the still-live exchange"
  );
  assert!(
    joins[0].pending.contains(&eid),
    "the live exchange is still pending"
  );

  // The delayed terminal `ExchangeCompleted` (a failure / timeout / decode error
  // all surface as `Failed`) finally arrives: NOW the waiter is reaped and its
  // ignore stream cleared. No second reply is sent (the deadline already replied).
  let peer: SocketAddr = "127.0.0.1:7946".parse().expect("peer addr");
  complete_join_exchange::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    &mut joins,
    eid,
    peer,
    false,
  );
  assert!(
    joins.is_empty(),
    "the waiter must be reaped once its last exchange completes"
  );
}

/// A deadline-reaped join lingers with `reply == None` while its exchange is
/// still live. `min_pending_join_deadline` must exclude it: the lingering waiter
/// has already replied to its caller and is cleaned up by `complete_join_exchange`
/// when its `ExchangeCompleted` arrives (I/O-driven), NOT by a timer. Contributing
/// a past deadline here causes a CPU busy-spin for the remaining stream-timeout
/// window.
#[compio::test]
async fn resolved_lingering_join_excluded_from_min_deadline() {
  let mut endpoint = build_endpoint();
  let mut joins: Vec<PendingJoin> = Vec::new();
  // Past deadline — the join will be reaped by the timer, leaving a lingering
  // waiter (reply == None, exchange still pending).
  let past = Instant::now() - Duration::from_secs(1);
  let (_sid, _eid, _rx) = park_ignore_old_join(&mut endpoint, &mut joins, past);

  // Reap: the reply resolves (reply taken -> None), waiter lingers.
  reap_pending_joins::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    &mut joins,
    Instant::now(),
  )
  .await;

  assert_eq!(joins.len(), 1, "the lingering waiter must still be present");
  assert!(
    joins[0].reply.is_none(),
    "the deadline reply must have resolved (reply taken)"
  );

  // The lingering waiter (reply == None) must NOT contribute a timer deadline —
  // returning None here is what prevents the busy-spin.
  assert_eq!(
    min_pending_join_deadline(&joins),
    None,
    "a resolved-lingering waiter must not contribute a timer deadline"
  );
}

/// A success-path merge consumes the ignore `StreamId` BEFORE the exchange's
/// `ExchangeCompleted`, so the count->0 clear in `complete_join_exchange` is a
/// no-op for it: the all-exchanges-done path resolves the reply with the contact
/// and reaps the waiter. This guards the non-deadline terminal that shares the
/// reap path with the deadline-linger case.
#[compio::test]
async fn all_exchanges_done_resolves_and_reaps() {
  let mut endpoint = build_endpoint();
  let mut joins: Vec<PendingJoin> = Vec::new();
  // A far-future deadline: this resolution is driven by exchange completion, not
  // the timer.
  let future = Instant::now() + Duration::from_secs(60);
  let (_sid, eid, rx) = park_ignore_old_join(&mut endpoint, &mut joins, future);

  // The exchange completes Succeeded; its peer enters `contacted` and, with no
  // exchanges left pending, the reply resolves and the waiter is reaped.
  let peer: SocketAddr = "127.0.0.1:7946".parse().expect("peer addr");
  complete_join_exchange::<SmolStr, RawRecords, StdRng, StdRng>(
    &mut endpoint,
    &mut joins,
    eid,
    peer,
    true,
  );
  assert!(
    joins.is_empty(),
    "an all-exchanges-done waiter is reaped on completion, not left for the timer"
  );
  match rx.await {
    Ok(Ok(contacted)) => assert_eq!(contacted.as_slice(), &[peer]),
    other => panic!("a successful exchange must reply Ok(contacted), got {other:?}"),
  }
}
