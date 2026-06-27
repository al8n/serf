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
