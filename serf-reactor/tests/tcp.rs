//! Real-node TCP serf tests: two loopback nodes exercising the reactor stream
//! driver end-to-end. Each test spins up ephemeral `127.0.0.1:0` nodes via the
//! ergonomic [`Serf::tcp`] constructor and drives the full pump — join push/pull,
//! coordinator merge, gossip, user events, queries, and graceful leave/shutdown —
//! end-to-end proof the reactor stream driver works over a concrete runtime.
//!
//! The scenario bodies are runtime-generic `async fn <R: Runtime>` helpers, so the
//! SAME scenario runs as a `#[tokio::test]` cell over `TokioRuntime` and as a
//! `_smol` cell driven by `SmolRuntime::block_on` — mirroring memberlist-reactor's
//! runtime-parameterized suite. The `Serf<I, A, R>` handle is already runtime-
//! generic; the helpers reach for timers through the runtime (`R::timeout` /
//! `R::sleep`) rather than a concrete runtime's clock.
//!
//! Mirrors serf-compio's serf behavior tests and memberlist-reactor's real-node
//! harness (bind loopback, join, poll-until-converged with a timeout, assert
//! membership / events), adapted to the reactor's `Send`/`agnostic` model.

#![cfg(feature = "tcp")]

use core::time::Duration;
use std::net::SocketAddr;

use agnostic::Runtime;
use bytes::Bytes;
use futures_util::{StreamExt, future};
#[cfg(encryption)]
use serf_proto::event::MemberEventKind;
use serf_proto::{event::Event, members::SerfState, options::Options as SerfOptions};
#[cfg(encryption)]
use serf_reactor::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TcpTransportOptions,
  VoidDelegate,
};
use smol_str::SmolStr;

/// A reactor TCP node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// Build and spawn a reactor TCP node on an ephemeral loopback port through the
/// ergonomic `Serf::tcp` constructor.
async fn spawn_node<R>(id: &str) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf tcp node")
}

/// Poll both nodes until each reports the full two-member cluster, or fail on a
/// generous timeout so a convergence regression surfaces as a timeout, not a hang.
async fn converge<R>(a: &Node<R>, b: &Node<R>)
where
  R: Runtime,
{
  R::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Two nodes on loopback: A joins B (await-result), then BOTH converge to a
/// two-member cluster and shut down cleanly.
async fn two_node_join_converges<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("conv-b").await;
  let a = spawn_node::<R>("conv-a").await;
  let b_addr = b.advertise_address();

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  converge(&a, &b).await;
  assert_eq!(a.num_members(), 2, "A sees the 2-member cluster");
  assert_eq!(b.num_members(), 2, "B sees the 2-member cluster");

  a.shutdown().await.expect("conv-a shuts down");
  b.shutdown().await.expect("conv-b shuts down");
}

/// After a two-node join, a user event broadcast by B is delivered to A's event
/// stream carrying the original name and payload.
async fn user_event_delivered<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("ue-b").await;
  let a = spawn_node::<R>("ue-a").await;
  let b_addr = b.advertise_address();

  // Subscribe before joining so the user event cannot race the subscription.
  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  b.user_event("greet", Bytes::from_static(b"hello"), false)
    .await
    .expect("user event dispatched");

  let got = R::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "greet" => break Some(u.payload.clone()),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's user event within the timeout");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"hello")),
    "A receives B's user-event payload"
  );

  a.shutdown().await.expect("ue-a shuts down");
  b.shutdown().await.expect("ue-b shuts down");
}

/// After a two-node join, a query issued by A round-trips: B receives the
/// `Event::Query`, responds, and A surfaces the matching `Event::QueryResponse`.
async fn query_round_trip<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-b").await;
  let a = spawn_node::<R>("q-a").await;
  let b_addr = b.advertise_address();

  // Subscribe both before the join so neither the query nor its response races
  // ahead of a subscription.
  let mut b_events = b.events();
  let mut a_events = a.events();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let want = Bytes::from_static(b"pong");
  a.query(
    "ping",
    Bytes::from_static(b"ping-payload"),
    a.default_query_param(),
  )
  .await
  .expect("query issued");

  // B answers the first "ping" query it sees; A collects the matching response.
  let responder = async {
    loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "ping" => {
          b.respond(qe, want.clone())
            .await
            .expect("B responds to the query");
          break;
        }
        Some(_) => {}
        None => panic!("B's event stream closed before the query arrived"),
      }
    }
  };
  let collector = async {
    loop {
      match a_events.next().await {
        Some(Event::QueryResponse(qr)) if qr.payload() == &want => break true,
        Some(_) => {}
        None => break false,
      }
    }
  };

  let got = R::timeout(Duration::from_secs(20), async {
    let (_, got) = future::join(responder, collector).await;
    got
  })
  .await
  .expect("query round-trip completes within the timeout");
  assert!(got, "A must receive B's query response");

  a.shutdown().await.expect("q-a shuts down");
  b.shutdown().await.expect("q-b shuts down");
}

/// A graceful leave completes the machine's leave chain: `leave()` resolves only
/// once `LeftCluster` fires (the reactor gates the reply on it), that event
/// surfaces on the leaver's own stream, and the local endpoint settles at `Left`.
async fn leave_emits_left_cluster<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("lv-b").await;
  let a = spawn_node::<R>("lv-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  // The reactor resolves `leave()` only once the machine's `LeftCluster` fires, so
  // a successful return already proves the graceful-leave chain completed.
  a.leave().await.expect("A leaves the cluster");

  // `LeftCluster` is also forwarded to A's own subscribers.
  let saw = R::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::LeftCluster) => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("A observes LeftCluster within the timeout");
  assert!(saw, "A must surface Event::LeftCluster after leave()");

  // The local endpoint state settles at `Left` (poll to absorb the snapshot-refresh
  // race after the leave chain completes).
  R::timeout(Duration::from_secs(5), async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A's endpoint state becomes Left");

  a.shutdown().await.expect("lv-a shuts down");
  b.shutdown().await.expect("lv-b shuts down");
}

/// After a two-node join, the snapshot read-forwarders on the joined node reflect
/// the two-member cluster: `members` returns both nodes, `local_member` / `local_id`
/// return this node, `state` is `Alive`, `advertise_node` composes id + advertise,
/// and `default_query_*` produce a positive, filter-free query default.
async fn snapshot_forwarders_reflect_joined_cluster<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("snap-b").await;
  let a = spawn_node::<R>("snap-a").await;
  let b_addr = b.advertise_address();
  let a_id = SmolStr::new("snap-a");
  let b_id = SmolStr::new("snap-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let members = a.members();
  assert_eq!(members.len(), 2, "members() returns the 2-member cluster");
  let ids: Vec<&SmolStr> = members.iter().map(|m| m.node().id_ref()).collect();
  assert!(ids.contains(&&a_id), "members() includes local node A");
  assert!(ids.contains(&&b_id), "members() includes peer node B");

  assert_eq!(
    a.local_member().node().id_ref(),
    &a_id,
    "local_member() returns local node A"
  );
  assert_eq!(a.local_id(), a_id, "local_id() returns local node A");
  assert_eq!(a.state(), SerfState::Alive, "state() is Alive after join");

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

  let qt = a.default_query_timeout();
  assert!(qt > Duration::ZERO, "default_query_timeout() is positive");
  let qp = a.default_query_param();
  assert_eq!(qp.timeout, qt, "default_query_param() timeout matches");
  assert!(
    qp.filters.is_empty(),
    "default_query_param() has no filters"
  );
  assert!(!qp.request_ack, "default_query_param() has no ack");
  assert_eq!(qp.relay_factor, 0, "default_query_param() has no relay");

  a.shutdown().await.expect("snap-a shuts down");
  b.shutdown().await.expect("snap-b shuts down");
}

/// `join_many` over two seeds — one reachable (node B), one an unroutable
/// blackhole port — returns only the reached seed's address once both exchanges
/// terminate.
async fn join_many_returns_only_reached_seeds<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("jm-b").await;
  let a = spawn_node::<R>("jm-a").await;
  let b_addr = b.advertise_address();
  let blackhole: SocketAddr = "127.0.0.1:7219".parse().expect("loopback addr");

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

/// `remove_failed_node` / `remove_failed_node_prune` are thin `force_leave`
/// aliases; calling both on a valid joined node-id completes without error.
async fn remove_failed_node_alias_succeeds<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("rfn-b").await;
  let a = spawn_node::<R>("rfn-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("rfn-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  a.remove_failed_node(b_id.clone())
    .await
    .expect("remove_failed_node must not error");
  a.remove_failed_node_prune(b_id)
    .await
    .expect("remove_failed_node_prune must not error");

  a.shutdown().await.expect("rfn-a shuts down");
  b.shutdown().await.expect("rfn-b shuts down");
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

/// Build and spawn a reactor TCP node on an ephemeral loopback port with
/// `encryption` installed as its gossip-and-reliable keyring policy.
#[cfg(encryption)]
async fn spawn_encrypted_node<R>(id: &str, encryption: EncryptionOptions) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_encryption(encryption);
  Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn encrypted serf tcp node")
}

/// Two nodes sharing one keyring join and converge over an AEAD-sealed gossip
/// plane: A joins B (await-result over the encrypted reliable push/pull), both
/// reach the two-member cluster, and A surfaces B's membership through its event
/// stream. Proves the keyring reaches the coordinator and that
/// `encrypt_gossip`/`decrypt_gossip` round-trip end-to-end rather than running as
/// identity transforms.
#[cfg(encryption)]
async fn two_node_join_converges_encrypted<R>()
where
  R: Runtime,
{
  let key = EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42)));
  let b = spawn_encrypted_node::<R>("enc-b", key.clone()).await;
  let a = spawn_encrypted_node::<R>("enc-a", key).await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("enc-b");

  // Subscribe before joining so the membership event cannot race the subscription.
  let mut a_events = a.events();
  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted reliable plane");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  converge(&a, &b).await;

  let observed = R::timeout(Duration::from_secs(20), async {
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

  a.shutdown().await.expect("enc-a shuts down");
  b.shutdown().await.expect("enc-b shuts down");
}

/// A node holding one keyring and a node holding a DIFFERENT keyring must NOT
/// exchange membership: the reliable push/pull units and the gossip datagrams are
/// both AEAD-sealed under disjoint keys, so neither side can authenticate the
/// other and the join never merges. Proves the encryption is real enforcement,
/// not an identity pass-through — without this negative case a passing encrypted
/// convergence test could not distinguish real AEAD from an identity transform.
#[cfg(encryption)]
async fn mismatched_keyring_nodes_do_not_exchange_membership<R>()
where
  R: Runtime,
{
  let b = spawn_encrypted_node::<R>(
    "mis-b",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42))),
  )
  .await;
  let a = spawn_encrypted_node::<R>(
    "mis-a",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x43))),
  )
  .await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("mis-b");

  let mut a_events = a.events();

  // Fire-and-forget dispatch: an await-result `join` would instead fail here (the
  // mismatched-key push/pull never authenticates); the absence probe below is what
  // proves membership never merges.
  let dispatched = a
    .dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(b_addr)])
    .await
    .expect("join dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  // Absence probe: A must never surface a Join carrying node-b. A short window
  // covers several gossip / probe / push-pull rounds on loopback — the positive
  // test forms its cluster within ~1-2s, so a clean 3s window is decisive.
  let observed = R::timeout(Duration::from_secs(3), async {
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

  a.shutdown().await.expect("mis-a shuts down");
  b.shutdown().await.expect("mis-b shuts down");
}

// The tokio cells: the runtime-generic scenarios driven on tokio's multi-thread
// runtime. Gated on the `tokio` feature so the `--test tcp -- smol` build (which
// enables only `smol`) can drop the `agnostic/tokio` code path.
#[cfg(feature = "tokio")]
mod tokio_cells {
  use agnostic::tokio::TokioRuntime;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn two_node_join_converges() {
    super::two_node_join_converges::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn user_event_delivered() {
    super::user_event_delivered::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn query_round_trip() {
    super::query_round_trip::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn leave_emits_left_cluster() {
    super::leave_emits_left_cluster::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_forwarders_reflect_joined_cluster() {
    super::snapshot_forwarders_reflect_joined_cluster::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn join_many_returns_only_reached_seeds() {
    super::join_many_returns_only_reached_seeds::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn remove_failed_node_alias_succeeds() {
    super::remove_failed_node_alias_succeeds::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn two_node_join_converges_encrypted() {
    super::two_node_join_converges_encrypted::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn mismatched_keyring_nodes_do_not_exchange_membership() {
    super::mismatched_keyring_nodes_do_not_exchange_membership::<TokioRuntime>().await;
  }
}

// The smol cells: the identical scenarios instantiated over `SmolRuntime` and
// driven by smol's `block_on`. The reactor poll task runs on smol's global
// executor, so the same scenario bodies verify the driver under a second runtime
// with no per-runtime scenario code. `cargo test --test tcp -- smol` selects
// exactly these.
#[cfg(feature = "smol")]
mod smol_cells {
  use agnostic::{RuntimeLite, smol::SmolRuntime};

  #[test]
  fn two_node_join_converges_smol() {
    SmolRuntime::block_on(super::two_node_join_converges::<SmolRuntime>());
  }

  #[test]
  fn user_event_delivered_smol() {
    SmolRuntime::block_on(super::user_event_delivered::<SmolRuntime>());
  }

  #[test]
  fn query_round_trip_smol() {
    SmolRuntime::block_on(super::query_round_trip::<SmolRuntime>());
  }

  #[test]
  fn leave_emits_left_cluster_smol() {
    SmolRuntime::block_on(super::leave_emits_left_cluster::<SmolRuntime>());
  }

  #[test]
  fn snapshot_forwarders_reflect_joined_cluster_smol() {
    SmolRuntime::block_on(super::snapshot_forwarders_reflect_joined_cluster::<
      SmolRuntime,
    >());
  }

  #[test]
  fn join_many_returns_only_reached_seeds_smol() {
    SmolRuntime::block_on(super::join_many_returns_only_reached_seeds::<SmolRuntime>());
  }

  #[test]
  fn remove_failed_node_alias_succeeds_smol() {
    SmolRuntime::block_on(super::remove_failed_node_alias_succeeds::<SmolRuntime>());
  }

  #[cfg(encryption)]
  #[test]
  fn two_node_join_converges_encrypted_smol() {
    SmolRuntime::block_on(super::two_node_join_converges_encrypted::<SmolRuntime>());
  }

  #[cfg(encryption)]
  #[test]
  fn mismatched_keyring_nodes_do_not_exchange_membership_smol() {
    SmolRuntime::block_on(
      super::mismatched_keyring_nodes_do_not_exchange_membership::<SmolRuntime>(),
    );
  }
}
