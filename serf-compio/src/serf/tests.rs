//! End-to-end smoke test: two TCP serf nodes on the loopback interface, one
//! joining the other, asserting the membership event propagates through the full
//! pump (Join command → push-pull dial → coordinator merge → serf `Member` event
//! → `EventStream`).

use core::time::Duration;
use std::net::SocketAddr;

use bytes::Bytes;
use futures_util::StreamExt;
use memberlist_proto::MaybeResolved;
use serf_proto::{
  event::{Event, MemberEventKind},
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

use crate::{
  Channel, FirstAddrResolver, RuntimeOptions, Serf, SerfError, SocketAddrResolver, TcpTransport,
  TcpTransportOptions, VoidDelegate, gossip_rng,
};

#[cfg(encryption)]
use crate::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};

/// Build and spawn a TCP serf node bound to an ephemeral loopback port.
async fn spawn_node(id: &str) -> Serf<SmolStr> {
  try_spawn_node_at(id, "127.0.0.1:0".parse().expect("loopback addr"))
    .await
    .expect("spawn serf node")
}

/// Build a TCP serf node bound to a specific advertise address, returning the
/// construction result so the same-address rebind regression can assert a freed
/// port accepts an immediate rebind.
async fn try_spawn_node_at(id: &str, bind: SocketAddr) -> Result<Serf<SmolStr>, SerfError> {
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
}

/// `shutdown().await` must release the bound TCP listener and UDP gossip socket
/// before it resolves: a second node binding the SAME advertise address the
/// instant the first shuts down must construct successfully, not fail with
/// `AddrInUse`. A plain drop of the compio listener does not guarantee its fd is
/// closed synchronously, so the driver awaits an explicit `close()` on both
/// sockets before acking the shutdown caller.
#[compio::test]
async fn tcp_shutdown_releases_bound_address_for_rebind() {
  let first = spawn_node("rebind-first").await;
  let addr = first.advertise_address();
  first.shutdown().await.expect("first node shuts down");

  let second = try_spawn_node_at("rebind-second", addr)
    .await
    .expect("rebinding the freed address must succeed, not AddrInUse");
  assert_eq!(
    second.advertise_address(),
    addr,
    "the second node rebinds the exact freed address"
  );
  second.shutdown().await.expect("second node shuts down");
}

/// All `Serf` handles dropping under a continuous gossip flood must still shut the
/// driver down. Under the flood the higher-priority recv arm starves the main
/// select's command arm, so the command-channel disconnect is observable ONLY by
/// the iter-top command drain; a dropped handle must therefore free the bound TCP
/// listener and UDP gossip socket for an immediate same-address rebind rather than
/// spinning forever and leaking them.
#[compio::test]
async fn tcp_command_disconnect_under_flood_releases_bound_ports() {
  let node = spawn_node("flood-drop").await;
  let addr = node.advertise_address();

  // Flood the driver's gossip UDP socket so the biased select's recv arm stays
  // ready — the load under which the command-channel disconnect must still tear
  // the driver down. A detached task keyed off a stop flag so it ends with the
  // test.
  let stop = std::rc::Rc::new(std::cell::Cell::new(false));
  let flood_stop = stop.clone();
  compio::runtime::spawn(async move {
    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
    let peer = compio::net::UdpSocket::bind(bind)
      .await
      .expect("bind flood peer");
    while !flood_stop.get() {
      // Ignoring Err: best-effort flood; a transient send error is non-fatal.
      let _ = peer.send_to(vec![1u8, 0, 0, 0], addr).await.0;
      // Yield via a short sleep so the flood cannot monopolize the single-threaded
      // runtime — a loopback send can complete inline, which would starve the
      // driver and the rebind poll. The gossip socket's kernel queue keeps the
      // recv arm ready across the gap.
      compio::time::sleep(Duration::from_millis(1)).await;
    }
    // Ignoring Err: test cleanup of the flood socket.
    let _ = peer.close().await;
  })
  .detach();

  // Drop every handle: the command channel disconnects. Under the flood the main
  // select's command arm is starved, so the iter-top drain's Disconnect branch is
  // the only path that can observe it and tear down.
  drop(node);

  // The driver must terminate and release BOTH bound ports; poll for the rebind
  // under a generous timeout so a regression (driver never exits) fails as a
  // timeout, not a hang.
  let rebound = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      if let Ok(listener) = compio::net::TcpListener::bind(addr).await {
        if let Ok(gossip) = compio::net::UdpSocket::bind(addr).await {
          break (listener, gossip);
        }
        // Teardown closes the listener before the UDP socket; release the
        // just-bound listener and retry until the UDP port frees too.
        // Ignoring Err: discarding the probe listener.
        let _ = listener.close().await;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await;

  stop.set(true);
  let (listener, gossip) = rebound.expect(
    "the driver must release its bound ports after a command-channel disconnect under flood",
  );
  // Ignoring Err: test cleanup of the rebind probe sockets.
  let _ = listener.close().await;
  let _ = gossip.close().await;
}

/// Build VALID TCP transport options paired with a deliberately invalid
/// `runtime`, and assert `Serf::new` rejects it with [`SerfError::InvalidOption`]
/// — before binding a socket or spawning the detached driver — rather than
/// returning `Ok` and later panicking the driver task on a zero-capacity channel.
async fn assert_tcp_new_rejects(runtime: RuntimeOptions) {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  let res =
    Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      runtime,
      SerfOptions::new(),
      gossip_rng().expect("seed gossip rng"),
      #[cfg(encryption)]
      std::rc::Rc::new(VoidKeyringDelegate),
    )
    .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero-capacity channel option must be rejected at construction"),
  }
}

/// A `Bounded(0)` observation channel (direct builder) is rejected by the stream
/// driver's `Serf::new` instead of panicking the detached driver task.
#[compio::test]
async fn tcp_new_rejects_zero_observation_channel() {
  assert_tcp_new_rejects(RuntimeOptions::new().with_observation_channel(Channel::Bounded(0))).await;
}

/// A zero `event_queue_cap` (direct builder) is rejected at construction.
#[compio::test]
async fn tcp_new_rejects_zero_event_queue_cap() {
  assert_tcp_new_rejects(RuntimeOptions::new().with_event_queue_cap(0)).await;
}

/// A zero `cmd_fairness_budget` (direct builder) starves the command drain under
/// an inbound flood, so `Serf::new` rejects it at construction rather than
/// spawning a driver whose commands could never make progress.
#[compio::test]
async fn tcp_new_rejects_zero_cmd_fairness_budget() {
  assert_tcp_new_rejects(RuntimeOptions::new().with_cmd_fairness_budget(0)).await;
}

/// A `Bounded(0)` observation channel sourced from a serde config is rejected.
#[cfg(feature = "serde")]
#[compio::test]
async fn tcp_new_rejects_zero_observation_channel_from_serde() {
  let runtime: RuntimeOptions =
    serde_json::from_str(r#"{"observation_channel":{"bounded":0}}"#).expect("deserialize");
  assert_tcp_new_rejects(runtime).await;
}

/// A `bounded:0` observation channel parsed from a clap flag is rejected.
#[cfg(feature = "clap")]
#[compio::test]
async fn tcp_new_rejects_zero_observation_channel_from_clap() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  let cli = Cli::try_parse_from(["app", "--runtime-observation-channel", "bounded:0"])
    .expect("clap parses bounded:0");
  assert_tcp_new_rejects(cli.runtime).await;
}

/// Two nodes on loopback: A joins B; A must observe B joining the cluster
/// through its event stream, then both shut down cleanly.
#[compio::test]
async fn two_node_tcp_join_observes_membership() {
  let b = spawn_node("node-b").await;
  let a = spawn_node("node-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  // Subscribe BEFORE the join so a `Member` event cannot race ahead of the
  // subscription (the channel buffers either way, but this is the clean order).
  let mut a_events = a.events();

  // Node A dials node B as its seed.
  let dispatched = a.join(vec![b_addr]).await.expect("join dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  // Node A should observe node B joining via a `Member(Join)` event.
  let observed = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    matches!(observed, Ok(true)),
    "node A should observe node B joining the cluster within the timeout"
  );

  // Exercise the graceful shutdown command path on both nodes.
  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled so the encrypted tests work under either backend.
#[cfg(encryption)]
fn test_secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// Build and spawn a TCP serf node on an ephemeral loopback port with `encryption`
/// installed as its gossip-and-reliable keyring policy.
#[cfg(encryption)]
async fn spawn_encrypted_node(id: &str, encryption: EncryptionOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_encryption(encryption);
  Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// Two nodes sharing one keyring: A joins B over an AEAD-sealed plain-TCP
/// reliable push-pull (and encrypted gossip), and A must still observe B joining
/// through its event stream. Proves the keyring reaches the coordinator and that
/// `encrypt_gossip`/`decrypt_gossip` round-trip end-to-end rather than running
/// as identity transforms when no keyring is wired.
#[cfg(encryption)]
#[compio::test]
async fn two_node_tcp_join_observes_membership_encrypted() {
  let enc = EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42)));
  let b = spawn_encrypted_node("node-b", enc.clone()).await;
  let a = spawn_encrypted_node("node-a", enc).await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  let mut a_events = a.events();

  let dispatched = a.join(vec![b_addr]).await.expect("join dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  let observed = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    matches!(observed, Ok(true)),
    "node A should observe node B joining the encrypted cluster within the timeout"
  );

  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// A node holding one keyring and a node holding a DIFFERENT keyring must NOT
/// exchange membership: the plain-TCP reliable push-pull units and the gossip
/// datagrams are both AEAD-sealed under disjoint keys, so neither side can
/// authenticate the other and the join never merges. Proves the encryption is
/// real enforcement, not an identity pass-through.
#[cfg(encryption)]
#[compio::test]
async fn mismatched_keyring_nodes_do_not_exchange_membership() {
  let b = spawn_encrypted_node(
    "node-b",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42))),
  )
  .await;
  let a = spawn_encrypted_node(
    "node-a",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x43))),
  )
  .await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  let mut a_events = a.events();

  let dispatched = a.join(vec![b_addr]).await.expect("join dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  // Absence probe: A must never surface a Join carrying node-b. A short window
  // covers several gossip / probe / push-pull rounds on loopback — the positive
  // test forms its cluster within ~1-2s, so a clean 3s window is decisive.
  let observed = compio::time::timeout(Duration::from_secs(3), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    !matches!(observed, Ok(true)),
    "node A must NOT observe node B across a mismatched keyring"
  );

  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// Build a TCP serf node with a custom `RuntimeOptions`.
async fn spawn_node_with_runtime(id: &str, runtime_options: RuntimeOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    runtime_options,
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// With a cap-1 event queue, the `events_dropped` counter on the `Serf`
/// handle becomes non-zero once more than one event arrives while no consumer
/// is draining the channel. Both the stream (TCP) and QUIC transports feed
/// the same shared counter via the same `Rc<Cell<u64>>` that `Serf::new`
/// retains on the handle, so a TCP test covers both code paths.
///
/// The test drains A's event stream only until the initial Member(Join) event
/// confirms the push-pull completed, then stops polling. User events gossiped
/// from B subsequently fill the cap=1 slot and overflow it, incrementing
/// `events_dropped`.
#[compio::test]
async fn tcp_events_dropped_counter_observable_under_backpressure() {
  // Node B provides the seed. Node A uses event_queue_cap=1 so the bounded
  // channel fills after a single unread event and every subsequent delivery
  // is dropped and counted.
  let b = spawn_node("drop-b").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("drop-b");
  let a = spawn_node_with_runtime("drop-a", RuntimeOptions::new().with_event_queue_cap(1)).await;

  // Subscribe before the join so we can drain until completion is confirmed.
  let mut events = a.events();

  a.join(vec![b_addr]).await.expect("join dispatched");

  // Drain A's event stream only until Member(Join, [B]) confirms the
  // push-pull completed. After breaking, `events` is alive but never polled
  // again: subsequent events fill the cap=1 slot and overflow it.
  compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break;
          }
        }
        Some(_) => {}
        None => panic!("event stream closed before join was observed"),
      }
    }
  })
  .await
  .expect("A must observe B joining within timeout");

  // Flood A with user events gossiped from B. With cap=1 and nobody draining,
  // the slot fills after one delivery and each subsequent try_send returns
  // Full, incrementing events_dropped on the Serf handle.
  for i in 0u32..10 {
    b.user_event(format!("drop-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  // Give gossip time to propagate across the loopback.
  compio::time::sleep(Duration::from_secs(3)).await;

  let dropped = a.events_dropped();

  a.shutdown().await.expect("drop-a shuts down");
  b.shutdown().await.expect("drop-b shuts down");

  assert!(
    dropped > 0,
    "events_dropped must be > 0 when event_queue_cap=1 and events are not drained (got {dropped})"
  );
}
