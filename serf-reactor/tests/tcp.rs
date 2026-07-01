//! Real-node TCP serf tests on tokio: two loopback nodes exercising the reactor
//! stream driver end-to-end. Each test spins up ephemeral `127.0.0.1:0` nodes via
//! the ergonomic [`Serf::tcp`] constructor and drives the full pump — join
//! push/pull, coordinator merge, gossip, user events, queries, and graceful
//! leave/shutdown — end-to-end proof the reactor stream driver works over a
//! concrete runtime.
//!
//! Mirrors serf-compio's serf behavior tests and memberlist-reactor's real-node
//! harness (bind loopback, join, poll-until-converged with a timeout, assert
//! membership / events), adapted to the reactor's `Send`/`agnostic` model.

#![cfg(all(feature = "tcp", feature = "tokio"))]

use core::time::Duration;
use std::net::SocketAddr;

use agnostic::tokio::TokioRuntime;
use bytes::Bytes;
use futures_util::{StreamExt, future};
use serf_proto::{event::Event, members::SerfState, options::Options as SerfOptions};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TcpTransportOptions,
  VoidDelegate,
};
use smol_str::SmolStr;

/// A tokio-backed reactor TCP node handle.
type Node = Serf<SmolStr, SocketAddr, TokioRuntime>;

/// Build and spawn a reactor TCP node on an ephemeral loopback port through the
/// ergonomic `Serf::tcp` constructor.
async fn spawn_node(id: &str) -> Node {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  Serf::<SmolStr, SocketAddr, TokioRuntime>::tcp(
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
async fn converge(a: &Node, b: &Node) {
  tokio::time::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Two nodes on loopback: A joins B (await-result), then BOTH converge to a
/// two-member cluster and shut down cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_node_join_converges() {
  let b = spawn_node("conv-b").await;
  let a = spawn_node("conv-a").await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_event_delivered() {
  let b = spawn_node("ue-b").await;
  let a = spawn_node("ue-a").await;
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

  let got = tokio::time::timeout(Duration::from_secs(20), async {
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_round_trip() {
  let b = spawn_node("q-b").await;
  let a = spawn_node("q-a").await;
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

  let got = tokio::time::timeout(Duration::from_secs(20), async {
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_emits_left_cluster() {
  let b = spawn_node("lv-b").await;
  let a = spawn_node("lv-a").await;
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
  let saw = tokio::time::timeout(Duration::from_secs(20), async {
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
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_forwarders_reflect_joined_cluster() {
  let b = spawn_node("snap-b").await;
  let a = spawn_node("snap-a").await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_many_returns_only_reached_seeds() {
  let b = spawn_node("jm-b").await;
  let a = spawn_node("jm-a").await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_failed_node_alias_succeeds() {
  let b = spawn_node("rfn-b").await;
  let a = spawn_node("rfn-a").await;
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
