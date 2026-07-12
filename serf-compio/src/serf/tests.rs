//! End-to-end smoke test: two TCP serf nodes on the loopback interface, one
//! joining the other, asserting the membership event propagates through the full
//! pump (Join command → push-pull dial → coordinator merge → serf `Member` event
//! → `EventStream`).

use core::time::Duration;
use std::net::SocketAddr;

use bytes::Bytes;
use futures_util::{StreamExt, future};
use memberlist_proto::MaybeResolved;
use serf_proto::{
  event::{Event, MemberEventKind},
  members::SerfState,
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

use crate::{
  Channel, FirstAddrResolver, Resolver, RuntimeOptions, Serf, SerfError, SocketAddrResolver,
  TcpTransport, TcpTransportOptions, VoidDelegate, gossip_rng,
};

/// A loopback address with a port nothing listens on — `connect()` returns
/// `ECONNREFUSED` immediately, so its push/pull exchange fails fast. The port is
/// below the OS ephemeral range, so a `:0` test bind never collides with it.
fn blackhole_addr() -> SocketAddr {
  "127.0.0.1:7213".parse().expect("loopback addr")
}

/// Resolver that always resolves to an empty address list — models a
/// service-discovery resolver that finds no live endpoints under a configured
/// service key.
struct EmptyResolver;

impl Resolver for EmptyResolver {
  type Address = String;
  type Error = std::io::Error;

  async fn resolve(&self, _addr: &String) -> Result<Vec<SocketAddr>, std::io::Error> {
    Ok(Vec::new())
  }
}

#[cfg(encryption)]
use crate::{EncryptionOptions, Keyring, KeyringDelegate, SecretKey, VoidKeyringDelegate};

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
    None,
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
      None,
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

/// An over-ceiling `max_user_event_size` in the serf options is rejected by
/// `Serf::new` at construction — before binding a socket or spawning the driver —
/// rather than returning `Ok` and later dropping oversize user events.
#[compio::test]
async fn tcp_new_rejects_over_ceiling_user_event_size() {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-serf-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  let serf =
    SerfOptions::new().with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1);
  let res =
    Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      serf,
      gossip_rng().expect("seed gossip rng"),
      None,
      #[cfg(encryption)]
      std::rc::Rc::new(VoidKeyringDelegate),
    )
    .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("an over-ceiling max_user_event_size must be rejected at construction"),
  }
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

  // Node A joins node B (await-result): the call returns the reached address
  // once the push-pull to B's seed completes.
  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

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
    None,
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

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted reliable plane");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

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

  // Fire-and-forget dispatch: the dial is queued regardless of whether the
  // reliable exchange can authenticate. An await-result `join` would instead
  // fail here (the mismatched-key push-pull never completes); the absence probe
  // below is what proves membership never merges.
  let dispatched = a
    .dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(b_addr)])
    .await
    .expect("join dispatched");
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
    None,
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

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");

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

/// Build a TCP serf node with a custom `SerfOptions` (runtime options at defaults).
async fn spawn_node_with_serf_options(id: &str, serf_options: SerfOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    serf_options,
    gossip_rng().expect("seed gossip rng"),
    None,
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// A single node with user coalescing enabled and a small buffered-volume cap sheds
/// every distinct-named coalescing user event issued past the cap through the public
/// `user_event` command path, and the cumulative drop count surfaces on the public
/// `coalesced_user_events_dropped` accessor — the endpoint counter is otherwise
/// unreachable once the driver moves the endpoint into the detached pump.
#[compio::test]
async fn tcp_coalesced_user_events_dropped_observable() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let a = spawn_node_with_serf_options("coalesce-a", serf_opts).await;

  assert_eq!(
    a.coalesced_user_events_dropped(),
    0,
    "no drops before any user event is issued"
  );

  // Issue distinct-named coalescing user events past the cap. Each is buffered by
  // name in the open window; every name past the cap is shed and counted.
  let n: u32 = 20;
  for i in 0..n {
    a.user_event(format!("evt-{i}"), Bytes::new(), true)
      .await
      .expect("user event dispatched");
  }

  // Each `user_event().await` returned only after the pump processed that command
  // and incremented the shared shed cell, so the handle read is already current on
  // the same executor thread with no publish step.
  let dropped = a.coalesced_user_events_dropped();
  a.shutdown().await.expect("coalesce-a shuts down");

  assert_eq!(
    dropped,
    u64::from(n) - cap.get() as u64,
    "every distinct-named cc event past the cap is counted on the public handle (got {dropped})"
  );
}

/// After exactly one shed through the public `user_event` path the handle getter
/// returns at least one. A construction typo that wired the handle's reader to a
/// different cell than the endpoint's writer would leave this a permanent zero, so
/// the aliasing bug fails loudly here rather than silently reporting no drops.
#[compio::test]
async fn coalesced_drop_aliasing_guard() {
  // A cap of one: the second distinct-named coalescing event is shed.
  let cap = core::num::NonZeroUsize::new(1).unwrap();
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let a = spawn_node_with_serf_options("coalesce-alias", serf_opts).await;

  a.user_event("first".to_string(), Bytes::new(), true)
    .await
    .expect("first user event dispatched");
  a.user_event("second".to_string(), Bytes::new(), true)
    .await
    .expect("second user event dispatched");

  assert!(
    a.coalesced_user_events_dropped() >= 1,
    "the handle observes the endpoint's shed; a mis-wired reader would read a permanent 0"
  );
  a.shutdown().await.expect("coalesce-alias shuts down");
}

/// `join_many` over two seeds — one reachable (node B), one a blackhole port —
/// returns only the reached seed's address. The reachable exchange succeeds and
/// the blackhole exchange fails fast; once both terminate the call resolves
/// `Ok([b_addr])`.
#[compio::test]
async fn tcp_join_many_returns_only_reached_seeds() {
  let b = spawn_node("jm-b").await;
  let a = spawn_node("jm-a").await;
  let b_addr = b.advertise_address();
  let blackhole = blackhole_addr();

  let reached = a
    .join_many(
      &SocketAddrResolver,
      [
        MaybeResolved::Resolved(b_addr),
        MaybeResolved::Resolved(blackhole),
      ]
      .into_iter(),
      false,
    )
    .await
    .expect("join_many reaches the reachable seed");

  assert_eq!(reached.len(), 1, "only the reachable seed is contacted");
  assert_eq!(
    reached[0], b_addr,
    "the reached set carries node B's address"
  );

  a.shutdown().await.expect("jm-a shuts down");
  b.shutdown().await.expect("jm-b shuts down");
}

/// An await-result `join` against an unreachable blackhole seed surfaces
/// `SerfError::JoinAllFailed { requested: 1, contacted: 0 }` once the dial fails
/// fast — well before the join deadline.
#[compio::test]
async fn tcp_join_unreachable_seed_surfaces_join_all_failed() {
  let a = spawn_node("blackhole-joiner").await;

  let err = a
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(blackhole_addr()),
      false,
    )
    .await
    .expect_err("join against a blackhole must fail");

  match err {
    SerfError::JoinAllFailed(payload) => {
      assert_eq!(payload.requested(), 1, "one seed requested");
      assert_eq!(payload.contacted(), 0, "no seed contacted");
    }
    other => panic!("expected JoinAllFailed, got {other:?}"),
  }

  a.shutdown().await.expect("joiner shuts down");
}

/// A non-empty `join` whose resolver returns zero addresses surfaces
/// `JoinAllFailed`, NOT a silent success — a service-discovery resolver that
/// finds no endpoints must never be reported as a healthy zero-contact join.
#[compio::test]
async fn tcp_join_zero_resolution_surfaces_join_all_failed() {
  let a = spawn_node("empty-resolve-joiner").await;

  let err = a
    .join(
      &EmptyResolver,
      MaybeResolved::Unresolved("svc-a".into()),
      false,
    )
    .await
    .expect_err("a seed resolving to zero addresses must fail");

  match err {
    SerfError::JoinAllFailed(payload) => {
      assert_eq!(payload.requested(), 1, "one input seed requested");
      assert_eq!(payload.contacted(), 0);
    }
    other => panic!("expected JoinAllFailed, got {other:?}"),
  }

  // An empty `join_many` input is a trivial `Ok(empty)` — no command is sent.
  let empty: Vec<MaybeResolved<String, SocketAddr>> = Vec::new();
  let reached = a
    .join_many(&EmptyResolver, empty.into_iter(), false)
    .await
    .expect("empty input is a trivial success");
  assert!(reached.is_empty(), "empty input contacts nothing");

  a.shutdown().await.expect("joiner shuts down");
}

/// After a two-node join, the snapshot forwarders on the joined node must reflect
/// the two-member cluster: `members()` returns both nodes, `local_member()` returns
/// this node's own `Member`, and `state()` returns `SerfState::Alive`.
///
/// `broadcast_join` materialises the local node in members.states immediately, so
/// the snapshot update races the join return: we drain until `Member(Join, [B])`
/// confirms the push-pull completed, at which point the snapshot is already
/// current (refresh_snapshot is called before the join reply is delivered).
#[compio::test]
async fn tcp_snapshot_forwarders_reflect_joined_cluster() {
  let b = spawn_node("snap-b").await;
  let a = spawn_node("snap-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("snap-b");
  let a_id = SmolStr::new("snap-a");

  // Subscribe before the join so a Member event does not race the subscription.
  let mut a_events = a.events();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");

  // Drain until B's join event confirms the push-pull completed. The event
  // stream may carry Member(Join, [A]) first (local node materialised in
  // members.states during broadcast_join), so we skip non-B join events.
  compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
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

  // The snapshot is refreshed before the join reply is sent, so num_members()
  // reflects the 2-member cluster immediately. A brief poll guards against any
  // marginal scheduling jitter on slow CI hosts.
  compio::time::timeout(Duration::from_secs(5), async {
    loop {
      if a.num_members() == 2 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("snapshot must show 2 members immediately after B's join event");

  // members() must reflect both nodes.
  let members = a.members();
  assert_eq!(
    members.len(),
    2,
    "two-node cluster: members() must return 2"
  );
  let ids: Vec<&SmolStr> = members.iter().map(|m| m.node().id_ref()).collect();
  assert!(ids.contains(&&a_id), "members() must include local node A");
  assert!(ids.contains(&&b_id), "members() must include peer node B");

  // local_member() must return A's own membership record.
  let local = a.local_member();
  assert_eq!(
    local.node().id_ref(),
    &a_id,
    "local_member() must return local node A"
  );

  // state() must be Alive after a successful join.
  assert_eq!(
    a.state(),
    SerfState::Alive,
    "state() must be Alive after join"
  );

  // advertise_node() must compose the local id and advertise address.
  let anode = a.advertise_node();
  assert_eq!(
    anode.id_ref(),
    &a_id,
    "advertise_node() id matches local_id()"
  );
  assert_eq!(
    anode.addr_ref(),
    &a.advertise_address(),
    "advertise_node() addr matches advertise_address()"
  );

  // default_query_timeout() must be a positive duration for a 2-member cluster.
  let qt = a.default_query_timeout();
  assert!(
    qt > Duration::ZERO,
    "default_query_timeout() must be positive"
  );

  // default_query_param() must carry that timeout with no filters/relay/ack.
  let qp = a.default_query_param();
  assert_eq!(
    qp.timeout, qt,
    "default_query_param().timeout matches default_query_timeout()"
  );
  assert!(
    qp.filters.is_empty(),
    "default_query_param() has no filters"
  );
  assert!(!qp.request_ack, "default_query_param() has no ack");
  assert_eq!(qp.relay_factor, 0, "default_query_param() has no relay");

  a.shutdown().await.expect("snap-a shuts down");
  b.shutdown().await.expect("snap-b shuts down");
}

/// `remove_failed_node` is a thin alias for `force_leave(id, false)`. Calling it
/// on a valid node-id in the cluster must complete without error — the driver
/// processes the forced leave broadcast unconditionally.
#[compio::test]
async fn tcp_remove_failed_node_alias_succeeds() {
  let b = spawn_node("rfn-b").await;
  let a = spawn_node("rfn-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("rfn-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");

  // remove_failed_node is force_leave(id, false); must not error.
  a.remove_failed_node(b_id.clone())
    .await
    .expect("remove_failed_node must not error");

  // remove_failed_node_prune is force_leave(id, true); must not error either.
  a.remove_failed_node_prune(b_id)
    .await
    .expect("remove_failed_node_prune must not error");

  a.shutdown().await.expect("rfn-a shuts down");
  b.shutdown().await.expect("rfn-b shuts down");
}

/// Two concurrently-polled `join` calls with `ignore_old=true` on cloned handles
/// must both complete without panicking or deadlocking.
///
/// The per-exchange ignore mechanism records each join exchange's `StreamId`
/// independently and consumes it one-shot at that exchange's own merge, so
/// concurrent ignore_old joins are safe WITHOUT the former `join_lock` — there is
/// no shared flag to serialize. This is the concurrency-correctness property that
/// replaces the old lock-serialization contract.
#[compio::test]
async fn tcp_concurrent_ignore_old_joins_coexist() {
  let b = spawn_node("cji-b").await;
  let b_addr = b.advertise_address();
  let a1 = spawn_node("cji-a").await;
  let a2 = a1.clone();

  // Run both join futures concurrently on the same compio task; with no lock,
  // they interleave freely and must both still resolve.
  let (r1, r2) = future::join(
    a1.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), true),
    a2.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), true),
  )
  .await;

  let contacted = r1.is_ok() as usize + r2.is_ok() as usize;
  assert!(
    contacted >= 1,
    "at least one concurrent ignore_old join must reach node B (got r1={r1:?}, r2={r2:?})"
  );

  a1.shutdown().await.expect("cji-a shuts down");
  b.shutdown().await.expect("cji-b shuts down");
}

/// The same-seed hole the per-exchange key closes: an `ignore_old` join AND a
/// plain (non-ignore) join to the SAME seed, polled concurrently, must coexist.
/// Each is a distinct exchange with a distinct `StreamId`, so the ignore_old
/// suppression targets ONLY its own merge — the plain join's same-seed merge is
/// never wrongly suppressed, and neither call consumes the other's token. A
/// per-PEER key could mis-route whichever merge landed first. (The deterministic
/// per-exchange suppression precision is proven by the serf-proto
/// `non_ignore_join_to_same_seed_is_not_suppressed` test; here we assert the
/// driver-level coexistence and convergence.)
#[compio::test]
async fn tcp_concurrent_ignore_old_and_plain_join_same_seed_coexist() {
  let b = spawn_node("cssj-b").await;
  let b_addr = b.advertise_address();
  let a = spawn_node("cssj-a").await;

  // Same seed B, two concurrent joins: one ignore_old, one plain.
  let (r_ignore, r_plain) = future::join(
    a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), true),
    a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false),
  )
  .await;

  assert!(
    r_ignore.is_ok() || r_plain.is_ok(),
    "at least one same-seed join must reach B (ignore={r_ignore:?}, plain={r_plain:?})"
  );

  // A converges to a 2-member cluster (self + B): neither same-seed join blocked
  // or dropped the other.
  compio::time::timeout(Duration::from_secs(10), async {
    loop {
      if a.num_members() == 2 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("A must converge to a 2-member cluster (self + B)");

  a.shutdown().await.expect("cssj-a shuts down");
  b.shutdown().await.expect("cssj-b shuts down");
}

/// A `dispatch_join` to one peer running concurrently with a
/// `join_many(ignore_old=true)` to a DIFFERENT peer must not interfere: both
/// resolve and the node converges to a 3-member cluster. The ignore_old join's
/// per-exchange suppression targets only its own `StreamId`, so the concurrent
/// dispatch_join's exchange is never wrongly suppressed (the machine-level
/// `non_ignore_join_to_same_seed_is_not_suppressed` test proves the
/// merge-suppression precision deterministically).
#[compio::test]
async fn tcp_concurrent_dispatch_join_and_ignore_old_join_coexist() {
  let b = spawn_node("cdj-b").await;
  let c = spawn_node("cdj-c").await;
  let b_addr = b.advertise_address();
  let c_addr = c.advertise_address();
  let a = spawn_node("cdj-a").await;

  // Concurrently: an ignore_old await-join to B, and a fire-and-forget
  // dispatch_join to C. Neither shares state with the other.
  let (join_b, dispatch_c) = future::join(
    a.join_many(
      &SocketAddrResolver,
      core::iter::once(MaybeResolved::Resolved(b_addr)),
      true,
    ),
    a.dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(c_addr)]),
  )
  .await;

  join_b.expect("the ignore_old join_many to B must reach B");
  assert_eq!(
    dispatch_c.expect("dispatch_join to C must dispatch"),
    1,
    "dispatch_join reports the single dispatched seed"
  );

  // Both peers must converge into A's membership — the concurrent ignore_old
  // join to B did not block or drop the dispatch_join to C.
  compio::time::timeout(Duration::from_secs(10), async {
    loop {
      if a.num_members() == 3 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("A must converge to a 3-member cluster (self + B + C)");

  let ids: Vec<SmolStr> = a
    .members()
    .iter()
    .map(|m| m.node().id_ref().clone())
    .collect();
  assert!(ids.iter().any(|id| id == "cdj-b"), "members must include B");
  assert!(ids.iter().any(|id| id == "cdj-c"), "members must include C");

  a.shutdown().await.expect("cdj-a shuts down");
  b.shutdown().await.expect("cdj-b shuts down");
  c.shutdown().await.expect("cdj-c shuts down");
}

/// A [`KeyringDelegate`] that records, in order, every live keyring the driver
/// publishes through `keyring_updated`. The driver fires it only after it has
/// pushed the rotated ring to the endpoint via `set_encryption_options`, so the
/// recorded ring is exactly the ring the gossip and reliable planes now encrypt
/// under — the live-wire observable the rotation test asserts on. `!Send` behind an
/// `Rc`, matching the compio driver's single-threaded keyring delegate.
#[cfg(encryption)]
#[derive(Default)]
struct RecordingKeyring {
  rings: core::cell::RefCell<Vec<Keyring>>,
}

#[cfg(encryption)]
impl RecordingKeyring {
  /// The keyrings observed so far, oldest first.
  fn rings(&self) -> Vec<Keyring> {
    self.rings.borrow().clone()
  }
}

#[cfg(encryption)]
impl KeyringDelegate for RecordingKeyring {
  fn keyring_updated(&self, keyring: &Keyring) -> serf_driver::KeyringPersistence {
    self.rings.borrow_mut().push(keyring.clone());
    // The in-memory record is durable the moment it is pushed, so the key
    // response goes out immediately.
    serf_driver::KeyringPersistence::Durable
  }
}

/// Build and spawn a TCP serf node on an ephemeral loopback port with `encryption`
/// as its keyring policy and `keyring` as its rotation observer.
#[cfg(encryption)]
async fn spawn_encrypted_node_with_keyring(
  id: &str,
  encryption: EncryptionOptions,
  keyring: std::rc::Rc<dyn KeyringDelegate>,
) -> Serf<SmolStr> {
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
    None,
    keyring,
  )
  .await
  .expect("spawn serf node")
}

/// Drive `events` until the originator's next `KeyResponse` surfaces (emitted when
/// the key query's deadline fires), draining any interleaved membership / gossip
/// events so a backlog cannot stall the stream.
#[cfg(encryption)]
async fn next_key_response<S>(events: &mut S) -> serf_proto::event::KeyResponse<SmolStr>
where
  S: futures_util::Stream<Item = Event<SmolStr, SocketAddr>> + Unpin,
{
  compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("event stream closed before a KeyResponse"),
      }
    }
  })
  .await
  .expect("a KeyResponse within the timeout")
}

/// Two encrypted nodes share primary K1, then A rotates the cluster to K2 via
/// `install_key` -> `use_key` -> `remove_key` over the compio single-threaded
/// stream driver. Each op propagates and every node applies it to its LIVE wire
/// keyring. The fail-on-revert check reads both nodes' recorded live rings (and
/// cross-checks cluster-wide via `list_keys`) and requires primary == K2 with K1
/// gone — unreachable under the pre-fix shadow model; the observer must have fired
/// exactly once per mutation and not for the read-only `list` or the refused final
/// remove; finally a user event still propagates A -> B, proving the wire runs
/// under K2 on both planes.
#[cfg(encryption)]
#[compio::test]
async fn two_node_tcp_key_rotation_rotates_both_live_keyrings() {
  use std::rc::Rc;

  let k1 = test_secret_key(0x11);
  let k2 = test_secret_key(0x22);

  let rec_a = Rc::new(RecordingKeyring::default());
  let rec_b = Rc::new(RecordingKeyring::default());
  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));

  let b = spawn_encrypted_node_with_keyring("rot-b", enc(), rec_b.clone()).await;
  let a = spawn_encrypted_node_with_keyring("rot-a", enc(), rec_a.clone()).await;
  let b_addr = b.advertise_address();

  // Converge on a 2-member view under K1 (a real encrypted push/pull join).
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted reliable plane");
  compio::time::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");

  // Subscribe before issuing any key op so no KeyResponse races the subscription.
  let mut a_events = a.events();
  let mut b_events = b.events();

  // install K2: both nodes gain it as a secondary in their live ring.
  a.install_key(k2).await.expect("install_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert!(
    kr.num_resp >= 2,
    "install_key must collect a response from BOTH nodes (num_resp={})",
    kr.num_resp
  );
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");

  // use K2: both nodes promote it to primary.
  a.use_key(k2).await.expect("use_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "use_key must succeed on every node");

  // remove K1: both nodes drop the old key.
  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "remove_key must succeed on every node");

  // FAIL-ON-REVERT: each node's observer captured exactly the ring the driver
  // published to its endpoint at each mutation, in order — install adds K2 as a
  // secondary under K1, use promotes K2, remove drops K1. Under the pre-fix shadow
  // model the coordinator keyring never rotated, so this sequence is unreachable.
  for (name, rec) in [("rot-a", &rec_a), ("rot-b", &rec_b)] {
    let rings = rec.rings();
    assert_eq!(
      rings.len(),
      3,
      "{name}: the observer fires exactly once per successful mutation"
    );
    assert_eq!(
      rings[0].primary_ref(),
      &k1,
      "{name}: install leaves K1 as primary"
    );
    assert!(
      rings[0].secondaries().contains(&k2),
      "{name}: install adds K2 as a secondary"
    );
    assert_eq!(rings[1].primary_ref(), &k2, "{name}: use promotes K2");
    assert_eq!(
      rings[2].primary_ref(),
      &k2,
      "{name}: K2 stays primary after the remove"
    );
    assert!(
      !rings[2].secondaries().contains(&k1),
      "{name}: the removed K1 is absent from the live keyring"
    );
  }

  // Independent, endpoint-direct cluster cross-check: `list_keys` tallies every
  // node's LIVE ring — K2 primary on BOTH nodes and K1 installed on none.
  a.list_keys().await.expect("list_keys dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(
    kr.primary_keys.get(&k2).copied(),
    Some(2),
    "both nodes report K2 as their live primary"
  );
  assert_eq!(
    kr.keys.get(&k2).copied(),
    Some(2),
    "both nodes still hold K2 in their live ring"
  );
  assert_eq!(
    kr.keys.get(&k1),
    None,
    "the removed K1 is installed on no node"
  );

  // `list_keys` is read-only: it must NOT have fired the observer.
  assert_eq!(
    rec_a.rings().len(),
    3,
    "list_keys does not fire the keyring observer"
  );
  assert_eq!(
    rec_b.rings().len(),
    3,
    "list_keys does not fire the keyring observer"
  );

  // A refused op must not fire the observer either: removing the already-gone K1 is
  // refused on every node, leaving both live rings — and the observer — untouched.
  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert!(
    kr.num_err >= 1,
    "removing an absent key is refused (num_err={})",
    kr.num_err
  );
  assert_eq!(
    rec_a.rings().len(),
    3,
    "a refused op does not fire the keyring observer"
  );
  assert_eq!(
    rec_b.rings().len(),
    3,
    "a refused op does not fire the keyring observer"
  );

  // Post-rotation traffic proof: a user event still crosses the wire, which now runs
  // under K2 on both nodes — the reliable and gossip planes rotated with the keyring.
  a.user_event("after-rotation", Bytes::from_static(b"payload"), false)
    .await
    .expect("user_event from a running node");
  let saw = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match b_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "after-rotation" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("B observes the post-rotation user event within the timeout");
  assert!(
    saw,
    "a user event must still propagate A -> B after the rotation (wire under K2)"
  );

  a.shutdown().await.expect("rot-a shuts down");
  b.shutdown().await.expect("rot-b shuts down");
}
