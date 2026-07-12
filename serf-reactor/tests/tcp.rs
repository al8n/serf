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
use serf_proto::{
  event::{Event, MemberEventKind},
  members::SerfState,
  options::Options as SerfOptions,
};
#[cfg(encryption)]
use serf_reactor::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TcpTransportOptions,
  VoidDelegate,
};
use smol_str::SmolStr;

/// The reusable multi-node fault-injection fixture, shared by the tokio and smol
/// cells below.
mod cluster;

/// A reactor TCP node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// A [`ReconnectDelegate`](serf_proto::ReconnectDelegate) that forces an immediate
/// reap (zero timeout) for one target member id and passes every other member
/// through the configured base timeout unchanged.
struct ReapImmediately {
  target: SmolStr,
}

impl serf_proto::ReconnectDelegate<SmolStr, SocketAddr> for ReapImmediately {
  fn reconnect_timeout(
    &self,
    member: &serf_proto::members::Member<SmolStr, SocketAddr>,
    base: Duration,
  ) -> Duration {
    if member.node().id_ref() == &self.target {
      Duration::ZERO
    } else {
      base
    }
  }
}

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
    None,
    None,
    None,
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

  // The operator aggregate reflects the converged view and the live endpoint
  // readings: full member count, nothing failed or left, a healthy awareness
  // score, and no keyring on these plaintext nodes.
  let stats = a.stats();
  assert_eq!(stats.members(), 2);
  assert_eq!(stats.failed(), 0);
  assert_eq!(stats.left(), 0);
  assert_eq!(stats.health_score(), 0, "a healthy node scores 0");
  assert!(!a.encryption_enabled(), "no keyring is configured");

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

/// The construction-time `reconnect_delegate` is installed into the endpoint and
/// consulted by the reaper: node A carries a delegate that zeroes node B's LEFT
/// tombstone while A's flat `tombstone_timeout` stays at 24h. After B leaves
/// gracefully, A drops back to a single member — which can only happen if the
/// driver installed the delegate AND the reaper consulted it (the flat 24h
/// timeout would otherwise hold B for the whole test). Proves the reactor
/// constructor wiring end-to-end.
async fn reconnect_delegate_reaps_left_member<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("rd-b").await;
  let b_addr = b.advertise_address();

  // A: a long flat tombstone (so a default reap never fires within the test) with
  // fast reap ticks, plus a delegate overriding ONLY B's tombstone to zero.
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("rd-a"))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  let serf_opts = SerfOptions::new()
    .with_reap_interval(Duration::from_millis(100))
    .with_tombstone_timeout(Duration::from_secs(86_400));
  let a = Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    serf_opts,
    Some(Box::new(ReapImmediately {
      target: SmolStr::new("rd-b"),
    })),
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf tcp node A with a reconnect delegate");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  // B leaves gracefully: A moves B to its LEFT tombstone list. With the delegate
  // zeroing B's tombstone, A's next reap tick drops B — the 2-member cluster
  // returns to 1. Without the delegate consult, A would hold B for the flat 24h.
  b.leave().await.expect("B leaves the cluster");

  R::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 1 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A reaps the left member B early via the reconnect-delegate override");

  a.shutdown().await.expect("rd-a shuts down");
  b.shutdown().await.expect("rd-b shuts down");
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

/// Two nodes on loopback: node A joins node B, then B is abruptly killed. Node A
/// must observe the member-event sequence Join → Failed → Reap about B: a Failed
/// (not a Leave), proving the kill discards the graceful-leave datagram, followed
/// by the reaper removing the failed member under the shortened reconnect timeout.
///
/// Mirrors Go serf `serf_events_failed`
/// (`legacy/serf-core/src/serf/base/tests/serf/event.rs`), whose `test_events`
/// asserts the exact ordered member-event sequence about the shut-down node.
async fn serf_events_failed<R>()
where
  R: Runtime,
{
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["events-failed-a", "events-failed-b"],
    cluster::ClusterTiming::fast(),
  )
  .await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  // A drops back to a single member once B is detected Failed and then reaped.
  cluster.await_num_members(0, 1).await;

  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Reap,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Two nodes on loopback: node A joins node B, then B leaves gracefully. Node A
/// must observe the member-event sequence Join → Leave about B: a Leave (not a
/// Failed), proving the farewell — the leave intent packed with the dead-self
/// notice — reached A before B tore down, and B lands in A's Left tombstone
/// view. The absence of a Failed is the discriminator against the abrupt-kill
/// path (`serf_events_failed`), which the two tests together pin.
async fn serf_events_leave<R>()
where
  R: Runtime,
{
  // Raise A's tombstone timeout past the test window so the reaper holds B's
  // Left tombstone rather than appending a trailing Reap to the observed
  // sequence (the fast profile otherwise reaps a left member sub-second).
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["events-leave-a", "events-leave-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);

  // The graceful leave().await must complete well inside the driver's 5s leave
  // timeout — a canary against anyone reintroducing a broadcast-flush wait.
  let elapsed = cluster.leave_graceful(1).await;
  assert!(
    elapsed < Duration::from_secs(3),
    "graceful leave().await took {elapsed:?}, expected well under the 5s driver leave timeout"
  );

  // A observes exactly [Join, Leave] about B — never a Failed.
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[MemberEventKind::Join, MemberEventKind::Leave],
    )
    .await;

  // B lands in A's Left tombstone view.
  cluster.await_left_tombstone(0, subject.as_str()).await;

  cluster.shutdown_all().await;
}

/// Two nodes on loopback: node A joins node B, then B's `leave()` and
/// `shutdown()` race — issued concurrently, they typically land in the same
/// driver command batch, so the teardown itself owns egressing the queued
/// farewell before it releases the gossip socket. Node A must still observe
/// Join → Leave (never a Failed) and B's leave must still resolve `Ok`,
/// pinning that an immediate shutdown cannot discard the departure fan-out
/// while it is still queued inside the endpoint.
async fn serf_events_leave_with_racing_shutdown<R>()
where
  R: Runtime,
{
  // Hold B's Left tombstone past the test window, as in `serf_events_leave`.
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["leave-race-a", "leave-race-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.leave_with_racing_shutdown(1).await;

  // A observes exactly [Join, Leave] about B — never a Failed.
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[MemberEventKind::Join, MemberEventKind::Leave],
    )
    .await;

  // B lands in A's Left tombstone view.
  cluster.await_left_tombstone(0, subject.as_str()).await;

  cluster.shutdown_all().await;
}

/// After a two-node join, probe round-trips feed the Vivaldi coordinate
/// client on both nodes: the local coordinate surfaces through the published
/// snapshot (`coordinate()`), and the peer's coordinate surfaces through the
/// driver round-trip (`cached_coordinate(id)`). Both accessors must go `Some`
/// within the probe cadence — the discriminator that the driver actually
/// forwards coordinates rather than merely compiling the feature.
#[cfg(feature = "coordinates")]
async fn coordinates_surface_on_the_handle<R>()
where
  R: Runtime,
{
  let mut cluster =
    cluster::Cluster::<R>::spawn(&["coord-a", "coord-b"], cluster::ClusterTiming::fast()).await;
  let b_id = cluster.id(1);

  // Probe RTTs accumulate at the fast profile's 100ms cadence; both surfaces
  // must appear well inside the fixture's poll ceiling.
  let deadline = std::time::Instant::now() + Duration::from_secs(20);
  loop {
    let local = cluster.node(0).coordinate();
    let cached = cluster
      .node(0)
      .cached_coordinate(b_id.clone())
      .await
      .expect("cached_coordinate round-trips through the driver");
    if local.is_some() && cached.is_some() {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "coordinates must surface on the handle: local={local:?} cached={cached:?}"
    );
    R::sleep(Duration::from_millis(50)).await;
  }

  cluster.shutdown_all().await;
}

/// Build a node persisting membership to `snapshot` (fast probe/gossip timing
/// so failure detection inside the scenario window stays sub-second).
async fn spawn_node_with_snapshot<R>(
  id: &str,
  snapshot: serf_reactor::SnapshotOptions,
  rejoin_after_leave: bool,
) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new().with_rejoin_after_leave(rejoin_after_leave),
    None,
    None,
    Some(snapshot),
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn snapshot-backed serf tcp node")
}

/// A unique snapshot path under the system temp dir.
fn snapshot_path<R>(name: &str) -> std::path::PathBuf
where
  R: Runtime,
{
  let mut p = std::env::temp_dir();
  // Keyed by runtime as well as pid: the tokio and smol cells run
  // concurrently in one test binary and must not share a snapshot file.
  p.push(format!(
    "serf-e2e-snap-{name}-{}-{}",
    std::process::id(),
    core::any::type_name::<R>().replace("::", "-"),
  ));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&p);
  p
}

/// Restart-and-rejoin: B persists its membership, is abruptly killed, and a
/// fresh B booted from the SAME snapshot re-dials its recovered peers through
/// the machine's own push/pull machinery — both nodes converge to two members
/// again WITHOUT any explicit join call on the restarted node.
async fn snapshot_restart_rejoins_the_cluster<R>()
where
  R: Runtime,
{
  let path = snapshot_path::<R>("rejoin");
  let a = spawn_node::<R>("snap-a").await;
  let b =
    spawn_node_with_snapshot::<R>("snap-b", serf_reactor::SnapshotOptions::new(&path), false).await;
  let a_addr = a.advertise_address();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Abrupt kill: no leave marker lands in the snapshot.
  b.shutdown().await.expect("snap-b shuts down");

  // A fresh B from the same snapshot auto-rejoins A (no join call).
  let b2 =
    spawn_node_with_snapshot::<R>("snap-b", serf_reactor::SnapshotOptions::new(&path), false).await;
  converge(&a, &b2).await;
  assert_eq!(
    b2.num_members(),
    2,
    "the restarted node recovers its membership from the snapshot"
  );

  a.shutdown().await.expect("snap-a shuts down");
  b2.shutdown().await.expect("snap-b2 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// The clean-leave gate: after a graceful leave, a restart with the default
/// `rejoin_after_leave = false` starts fresh (no auto-rejoin), while a restart
/// opting in with `rejoin_after_leave = true` recovers the pre-leave
/// membership and rejoins.
async fn snapshot_leave_gate_controls_rejoin<R>()
where
  R: Runtime,
{
  let path = snapshot_path::<R>("leave-gate");
  let a = spawn_node::<R>("gate-a").await;
  let b =
    spawn_node_with_snapshot::<R>("gate-b", serf_reactor::SnapshotOptions::new(&path), false).await;
  let a_addr = a.advertise_address();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Graceful leave: the Leave marker lands in the snapshot.
  b.leave().await.expect("gate-b leaves gracefully");
  b.shutdown().await.expect("gate-b shuts down");

  // The pump writes the clock floors BEFORE the leave marker, so a clean
  // shutdown ends the file at the Leave record — the terminal shape replay
  // expects and compaction preserves. A record written after it would
  // resurrect state the default posture is supposed to zero.
  {
    let bytes = std::fs::read(&path).expect("the snapshot survives the leave");
    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
      let (rec, used) =
        serf_proto::snapshot::SnapshotRecord::<SmolStr, SocketAddr>::decode(&bytes[cursor..])
          .expect("a clean-leave snapshot decodes whole");
      records.push(rec);
      cursor += used;
    }
    assert!(
      matches!(
        records.last(),
        Some(serf_proto::snapshot::SnapshotRecord::Leave)
      ),
      "a clean shutdown must end the snapshot at the Leave record"
    );
  }

  // Default posture: the leave clears the recovered state — no auto-rejoin.
  let b2 =
    spawn_node_with_snapshot::<R>("gate-b", serf_reactor::SnapshotOptions::new(&path), false).await;
  R::sleep(Duration::from_millis(1500)).await;
  assert_eq!(
    b2.num_members(),
    1,
    "a cleanly-left node must not auto-rejoin unless opted in"
  );
  b2.shutdown().await.expect("gate-b2 shuts down");

  // Opt-in posture: the Leave marker is ignored and the membership recovers.
  let b3 =
    spawn_node_with_snapshot::<R>("gate-b", serf_reactor::SnapshotOptions::new(&path), true).await;
  converge(&a, &b3).await;

  a.shutdown().await.expect("gate-a shuts down");
  b3.shutdown().await.expect("gate-b3 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// The constructor-supplied merge delegate is the predicate the machine
/// consults: with a recording accept-all delegate installed on B, A's join
/// push-pull drives at least one `notify_merge` on B carrying A's node state.
/// (A veto here gates only the push/pull application — a rejected peer can
/// still be admitted moments later through gossip Alives, exactly as in the
/// reference implementation, so the stable assertion is consultation, not
/// permanent exclusion.)
async fn merge_delegate_is_consulted_on_join<R>()
where
  R: Runtime,
{
  use std::sync::atomic::{AtomicUsize, Ordering};

  struct RecordingMerge {
    hits: std::sync::Arc<AtomicUsize>,
    saw_peer: std::sync::Arc<AtomicUsize>,
  }
  impl serf_reactor::MergeDelegate<SmolStr, SocketAddr> for RecordingMerge {
    fn notify_merge(
      &self,
      peers: memberlist_proto::MaybeOwned<
        '_,
        [memberlist_proto::typed::NodeState<SmolStr, SocketAddr>],
      >,
    ) -> bool {
      self.hits.fetch_add(1, Ordering::Relaxed);
      if peers.iter().any(|p| p.id_ref().as_str() == "merge-a") {
        self.saw_peer.fetch_add(1, Ordering::Relaxed);
      }
      true
    }
  }

  let hits = std::sync::Arc::new(AtomicUsize::new(0));
  let saw_peer = std::sync::Arc::new(AtomicUsize::new(0));

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let b = Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("merge-b"))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    Some(Box::new(RecordingMerge {
      hits: hits.clone(),
      saw_peer: saw_peer.clone(),
    })),
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn merge-b");
  let a = spawn_node::<R>("merge-a").await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  assert!(
    hits.load(Ordering::Relaxed) > 0,
    "the machine must consult the constructor-supplied merge delegate on the join push/pull"
  );
  assert!(
    saw_peer.load(Ordering::Relaxed) > 0,
    "the consulted peer set must carry the joining node's state"
  );

  a.shutdown().await.expect("merge-a shuts down");
  b.shutdown().await.expect("merge-b shuts down");
}

/// A leave configured with a zero timeout racing a shutdown resolves
/// `Err(LeaveTimeout)` — never `Ok` — even though the teardown still delivers
/// the fan-out: the caller's per-leave deadline keeps governing resolution
/// during teardown, and a zero timeout is a loud immediate `LeaveTimeout` by
/// contract.
async fn leave_with_zero_timeout_racing_shutdown_times_out<R>()
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let node = Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("zero-leave"))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new().with_leave_timeout(Duration::ZERO),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn zero-leave-timeout node");

  let (leave, shutdown) = future::join(node.leave(), node.shutdown()).await;
  assert!(
    matches!(leave, Err(serf_reactor::SerfError::LeaveTimeout)),
    "a zero leave timeout must resolve LeaveTimeout even when a shutdown races it, got {leave:?}"
  );
  shutdown.expect("node shuts down");
}

/// Two nodes on loopback: node A joins node B, B is abruptly killed and detected
/// Failed, then B is restarted at the same id and advertise address. Node A must
/// observe the sequence Join → Failed → Join about B — the failed member
/// reconnects rather than being reaped, because the reconnect timeout is raised
/// past the kill-to-restart window while the reconnect loop re-dials B.
///
/// Mirrors Go serf `serf_reconnect`
/// (`legacy/serf-core/src/serf/base/tests/serf/reconnect.rs`).
async fn serf_reconnect<R>()
where
  R: Runtime,
{
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["reconnect-a", "reconnect-b"],
    cluster::ClusterTiming::fast().with_reconnect_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  // Wait for A to detect B's failure before B returns, so the Failed event is
  // recorded distinctly from the later rejoin.
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster.restart(1).await;
  // The serf reconnect loop re-dials the restarted B, which rejoins the cluster.
  cluster.await_num_members(0, 2).await;

  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Join,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_force_leave_failed` (Go `TestSerf_ForceLeaveFailed`):
/// an operator force-leaving a FAILED member transitions it to Left on every
/// surviving node, rather than leaving it to linger Failed until the reap.
async fn serf_force_leave_failed<R>()
where
  R: Runtime,
{
  // A long tombstone keeps the force-left member observable as Left for the
  // whole assertion window, and a long reconnect timeout keeps the FAILED
  // member from being reaped out of the views before the intent lands (the
  // reference tests run with the default day-scale reconnect timeout).
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["fleave-a", "fleave-b", "fleave-c"],
    cluster::ClusterTiming::fast()
      .with_tombstone_timeout(Duration::from_secs(120))
      .with_reconnect_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(2);

  cluster.kill_abrupt(2).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .force_leave(subject.clone(), false)
    .await
    .expect("force_leave dispatches for a failed member");

  // The force-leave propagates: BOTH survivors converge the failed member to
  // a Left tombstone, and the observer's event log records the full
  // Join -> Failed -> Leave lifecycle.
  cluster.await_left_tombstone(0, subject.as_str()).await;
  cluster.await_left_tombstone(1, subject.as_str()).await;
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Leave,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_force_leave_left` (Go `TestSerf_ForceLeaveLeft`,
/// folding `TestSerf_ForceLeaveLeaving`): force-leaving a member that already
/// departed gracefully is an accepted no-op — it stays a Left tombstone and
/// the membership view is unchanged. The transient Leaving window itself is
/// machine-covered (`endpoint::force_leave_transitions_alive_member_to_leaving`);
/// a driver cannot deterministically observe it mid-flight.
async fn serf_force_leave_left_is_idempotent<R>()
where
  R: Runtime,
{
  // A long tombstone keeps the departed member observable as Left for the
  // whole assertion window, and push/pull is DISABLED so the gossiped intent
  // is the only path that can advance the survivor's member clock — the
  // exclusivity the causal fence below relies on (anti-entropy also
  // witnesses remote clocks and would replay the Left state, masking the
  // fresh intent).
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["fleft-a", "fleft-b", "fleft-c"],
    cluster::ClusterTiming::fast()
      .with_tombstone_timeout(Duration::from_secs(120))
      .with_push_pull_interval(Duration::ZERO),
  )
  .await;
  let subject = cluster.id(2);

  cluster.leave_graceful(2).await;
  cluster.await_left_tombstone(0, subject.as_str()).await;
  cluster.await_left_tombstone(1, subject.as_str()).await;
  // FENCE the baselines on the COLLECTORS, not the membership snapshot: the
  // tombstone proves the state flipped, while the graceful leave's member
  // event may still be in flight to a detached collector. Awaiting the exact
  // sequence pins each baseline at [Join, Leave].
  let expected = [MemberEventKind::Join, MemberEventKind::Leave];
  cluster
    .assert_member_events(0, subject.as_str(), &expected)
    .await;
  cluster
    .assert_member_events(1, subject.as_str(), &expected)
    .await;

  let clock_before = cluster.node(0).stats().member_clock();
  cluster
    .node(0)
    .force_leave(subject.clone(), false)
    .await
    .expect("force_leave on an already-left member is an accepted no-op");

  // CAUSAL FENCE: the force-leave stamps the member clock and the intent
  // carries that ltime, which every receiver witnesses BEFORE deciding
  // whether the intent applies — so the non-issuing survivor's clock
  // reaching the issuer's post-command value proves THIS intent was
  // received and processed there, not merely that unrelated traffic flowed.
  let stamp = R::timeout(Duration::from_secs(20), async {
    loop {
      let c = cluster.node(0).stats().member_clock();
      if c > clock_before {
        break c;
      }
      R::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the issuer stamps the member clock for the no-op intent");
  R::timeout(Duration::from_secs(20), async {
    loop {
      if cluster.node(1).stats().member_clock() >= stamp {
        break;
      }
      R::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the non-issuing survivor witnesses the no-op intent's clock");

  // A short settle lets the survivors reprocess any rebroadcast echo of the
  // intent (a stale re-receipt must also stay a no-op), then both views must
  // be unchanged — still the Left tombstone, still three members, and NO
  // additional member event.
  R::sleep(Duration::from_millis(500)).await;
  cluster.await_left_tombstone(0, subject.as_str()).await;
  cluster.await_left_tombstone(1, subject.as_str()).await;
  for observer in [0usize, 1] {
    assert_eq!(
      cluster.node(observer).num_members(),
      3,
      "node {observer}: the Left tombstone is retained, not pruned, by a plain force_leave"
    );
  }
  // With propagation causally fenced above, BARRIER the collector drainage:
  // a fresh sentinel node joins and both collectors must record ITS Join
  // before the subject's sequences are compared — each collector's stream is
  // ordered, so any duplicate event the (already-processed) intent emitted
  // is queued ahead of the sentinel and would already be visible.
  let sentinel = spawn_node::<R>("fleft-sentinel").await;
  let a_addr = cluster.node(0).advertise_address();
  sentinel
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the sentinel joins through the issuing survivor");
  cluster
    .await_member_event(0, "fleft-sentinel", MemberEventKind::Join)
    .await;
  cluster
    .await_member_event(1, "fleft-sentinel", MemberEventKind::Join)
    .await;
  assert_eq!(
    cluster.member_event_kinds(0, subject.as_str()),
    expected,
    "the issuing survivor records no additional member event for the no-op"
  );
  assert_eq!(
    cluster.member_event_kinds(1, subject.as_str()),
    expected,
    "the non-issuing survivor records no additional member event for the no-op"
  );

  sentinel.shutdown().await.expect("the sentinel shuts down");
  cluster.shutdown_all().await;
}

/// Port of legacy `serf_remove_failed_node` + `serf_remove_failed_events_leave`
/// (Go `TestSerf_RemoveFailedNode` / `TestSerfRemoveFailedEventsLeave`): after
/// a failure, `remove_failed_node` on one survivor propagates — the OTHER
/// survivor also observes a Leave member event for the failed node and holds
/// it as a Left tombstone.
async fn serf_remove_failed_node_propagates<R>()
where
  R: Runtime,
{
  // A long tombstone keeps the removed member observable as Left for the
  // whole assertion window.
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["remove-a", "remove-b", "remove-c"],
    cluster::ClusterTiming::fast()
      .with_tombstone_timeout(Duration::from_secs(120))
      .with_reconnect_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(2);

  cluster.kill_abrupt(2).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .remove_failed_node(subject.clone())
    .await
    .expect("remove_failed_node dispatches");

  // Propagation: the NON-issuing survivor holds the tombstone too.
  cluster.await_left_tombstone(0, subject.as_str()).await;
  cluster.await_left_tombstone(1, subject.as_str()).await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Leave)
    .await;

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_remove_failed_node_prune`
/// (Go `TestSerf_RemoveFailedNode_prune`): removing with prune erases the
/// failed member outright on every survivor — the membership count drops
/// without waiting out the Left tombstone.
async fn serf_remove_failed_node_prune_erases<R>()
where
  R: Runtime,
{
  // Hold the failed member (no reap, no reconnect eviction) so the prune —
  // not the reaper — is what erases it.
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["prune-a", "prune-b", "prune-c"],
    cluster::ClusterTiming::fast().with_reconnect_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(2);

  cluster.kill_abrupt(2).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .remove_failed_node_prune(subject.clone())
    .await
    .expect("remove_failed_node_prune dispatches");

  cluster.await_num_members(0, 2).await;
  cluster.await_num_members(1, 2).await;

  cluster.shutdown_all().await;
}

/// Port of the legacy `serf_remove_failed_node` absent-member edge
/// (Go `TestSerf_RemoveFailedNode_ourself` shape): removing a name that is
/// not a member reports success as a no-op.
async fn remove_failed_node_absent_is_a_noop<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("absent-a").await;
  a.remove_failed_node(SmolStr::new("no-such-node"))
    .await
    .expect("removing an absent member is an accepted no-op");
  assert_eq!(a.num_members(), 1, "the membership view is unchanged");
  a.shutdown().await.expect("absent-a shuts down");
}

/// Port of legacy `serf_reconnect_same_ip` (Go `TestSerf_Reconnect_SameIP`):
/// the failed node returns at the SAME IP but a DIFFERENT port and re-joins —
/// the same-name member revives at its new address (Join, Failed, Join), and
/// the observer's view carries the updated port.
async fn serf_reconnect_same_ip<R>()
where
  R: Runtime,
{
  // The reclaim window is what allows a SAME-name member to revive at a NEW
  // address at all — without it a different-address Alive is a name conflict,
  // exactly as in the reference implementation's dead-node reclaim.
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["sameip-a", "sameip-b"],
    cluster::ClusterTiming::fast()
      .with_reconnect_timeout(Duration::from_secs(30))
      .with_dead_node_reclaim(Duration::from_millis(1)),
  )
  .await;
  let subject = cluster.id(1);
  let old_addr = cluster.node(1).advertise_address();

  cluster.kill_abrupt(1).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  // The reclaim admits a new-address revival only once the failed state is
  // STRICTLY older than the window; observing the Failed event is not that
  // fence, so wait comfortably past the (1ms) window before rejoining.
  R::sleep(Duration::from_millis(100)).await;

  cluster.restart_at_ephemeral(1).await;
  let new_addr = cluster.node(1).advertise_address();
  assert_ne!(old_addr, new_addr, "the restart rebinds a fresh port");
  let a_addr = cluster.node(0).advertise_address();
  cluster
    .node(1)
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the restarted node rejoins through the survivor");

  // The failed tombstone still counts toward the member view, so the revival
  // signal is the SECOND Join event; the sequence assertion below polls until
  // the third event lands.
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Join,
      ],
    )
    .await;
  // The revived member is tracked at its NEW address.
  let seen = cluster
    .node(0)
    .members()
    .iter()
    .find(|m| m.node().id_ref().as_str() == subject.as_str())
    .map(|m| *m.node().addr_ref())
    .expect("the observer tracks the revived member");
  assert_eq!(
    seen, new_addr,
    "the same-name revival updates the tracked address"
  );

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_join_cancel` (Go `TestSerf_Join_Cancel`): with a
/// vetoing merge predicate on BOTH nodes, a join attempt admits nothing —
/// each side's delegate is consulted with the peer's state and each
/// membership view stays at one. (The machine's veto is a push/pull FILTER:
/// with neither side ever admitting the other, no gossip path exists either,
/// so the exclusion here is total and deterministic.)
async fn serf_join_cancel<R>()
where
  R: Runtime,
{
  use std::sync::atomic::{AtomicUsize, Ordering};

  struct VetoAll {
    hits: std::sync::Arc<AtomicUsize>,
  }
  impl serf_reactor::MergeDelegate<SmolStr, SocketAddr> for VetoAll {
    fn notify_merge(
      &self,
      _peers: memberlist_proto::MaybeOwned<
        '_,
        [memberlist_proto::typed::NodeState<SmolStr, SocketAddr>],
      >,
    ) -> bool {
      self.hits.fetch_add(1, Ordering::Relaxed);
      false
    }
  }

  async fn spawn_vetoing<R>(id: &str, hits: std::sync::Arc<AtomicUsize>) -> Node<R>
  where
    R: Runtime,
  {
    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
    Serf::<SmolStr, SocketAddr, R>::tcp(
      TcpTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(bind)),
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      Some(Box::new(VetoAll { hits })),
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
    )
    .await
    .expect("spawn vetoing serf node")
  }

  let a_hits = std::sync::Arc::new(AtomicUsize::new(0));
  let b_hits = std::sync::Arc::new(AtomicUsize::new(0));
  let b = spawn_vetoing::<R>("cancel-b", b_hits.clone()).await;
  let a = spawn_vetoing::<R>("cancel-a", a_hits.clone()).await;
  let b_addr = b.advertise_address();

  let outcome = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await;

  // The SEED's predicate runs and refuses; its machine then closes the
  // exchange WITHOUT sending its own state — a vetoed filter must not leak
  // the peer's state — so the joiner's predicate never receives anything to
  // judge. (The reference implementation consults both sides because it
  // ships its state before the remote verdict; the machine here deliberately
  // tightens that.)
  R::timeout(Duration::from_secs(20), async {
    loop {
      if b_hits.load(Ordering::Relaxed) >= 1 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the seed's merge predicate is consulted by the join push/pull");
  assert_eq!(
    a_hits.load(Ordering::Relaxed),
    0,
    "no state reaches the joiner's predicate once the seed refused"
  );
  // Nothing admitted anywhere: with no member ever merged there is no gossip
  // path either, so the exclusion holds on both views.
  assert_eq!(a.num_members(), 1, "the joiner admitted nothing");
  assert_eq!(b.num_members(), 1, "the seed admitted nothing");
  assert!(
    outcome.is_err(),
    "a fully vetoed join reports failure to the caller (got {outcome:?})"
  );

  a.shutdown().await.expect("cancel-a shuts down");
  b.shutdown().await.expect("cancel-b shuts down");
}

/// Port of legacy `serf_set_tags` folding `serf_role` (Go `TestSerf_SetTags`
/// / `TestSerf_Role`): a tag change on one node propagates to the peer in
/// BOTH directions — the observer records an Update member event for the
/// setter and its member view carries the new tag value.
async fn serf_set_tags_propagates<R>()
where
  R: Runtime,
{
  let mut cluster =
    cluster::Cluster::<R>::spawn(&["tags-a", "tags-b"], cluster::ClusterTiming::fast()).await;
  let a_id = cluster.id(0);
  let b_id = cluster.id(1);

  let mut tags_b = serf_proto::Tags::new();
  tags_b
    .0
    .insert(SmolStr::new("role"), SmolStr::new("worker"));
  cluster
    .node(1)
    .set_tags(tags_b)
    .await
    .expect("B re-tags itself");
  cluster
    .await_member_event(0, b_id.as_str(), MemberEventKind::Update)
    .await;
  let seen = cluster
    .node(0)
    .members()
    .iter()
    .find(|m| m.node().id_ref().as_str() == b_id.as_str())
    .map(|m| m.tags().0.get("role").cloned())
    .expect("A tracks B");
  assert_eq!(
    seen.as_deref(),
    Some("worker"),
    "A's view of B carries the propagated role tag"
  );

  // And the reverse direction.
  let mut tags_a = serf_proto::Tags::new();
  tags_a.0.insert(SmolStr::new("role"), SmolStr::new("lead"));
  cluster
    .node(0)
    .set_tags(tags_a)
    .await
    .expect("A re-tags itself");
  cluster
    .await_member_event(1, a_id.as_str(), MemberEventKind::Update)
    .await;
  let seen = cluster
    .node(1)
    .members()
    .iter()
    .find(|m| m.node().id_ref().as_str() == a_id.as_str())
    .map(|m| m.tags().0.get("role").cloned())
    .expect("B tracks A");
  assert_eq!(
    seen.as_deref(),
    Some("lead"),
    "B's view of A carries the propagated role tag"
  );

  cluster.shutdown_all().await;
}

/// A rejoin-cycle regression guard: after a fail/restart/rejoin cycle, a tag
/// change still propagates as an Update with the refreshed value visible —
/// pinning the clock-sync and event-fencing subtleties of the revival path.
///
/// This is the retag-AFTER-revival half of the legacy update scenario: a
/// revival folds merged tags into the Join event itself (matching the
/// reference `handleNodeJoin`), so the retag must land after the revival
/// fence to surface a distinct Update. The restart-with-changed-tags half —
/// where the Update arrives from the rejoin exchange while the observer
/// still holds the node Alive — is
/// `serf_update_after_restart_with_changed_tags`.
async fn serf_update_after_rejoin<R>()
where
  R: Runtime,
{
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["upd-a", "upd-b"],
    // The explicit rejoin below is the single revival path: the survivor's
    // own reconnect re-dial is parked out of the window so its push/pull
    // merge cannot race the retag (a revival merge folds the tags into its
    // Join rather than a distinct Update), and the failed member is retained
    // throughout.
    cluster::ClusterTiming::fast()
      .with_reconnect_interval(Duration::from_secs(600))
      .with_reconnect_timeout(Duration::from_secs(600)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster.restart(1).await;
  // The restarted node's serf clock begins fresh; an explicit join runs the
  // push/pull that witnesses the survivor's clocks, so the tag update minted
  // below stamps ABOVE the observer's recorded status time instead of
  // arriving stale (the reference test also rejoins explicitly).
  let a_addr = cluster.node(0).advertise_address();
  cluster
    .node(1)
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the restarted node rejoins through the survivor");
  // Fence the REVIVAL on the observer's event sequence — the member count is
  // vacuous here (the failed tombstone still counts toward it), and a retag
  // racing ahead of the join would ride the join itself, leaving no separate
  // Update to observe.
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Join,
      ],
    )
    .await;

  let mut tags = serf_proto::Tags::new();
  tags.0.insert(SmolStr::new("version"), SmolStr::new("v2"));
  cluster
    .node(1)
    .set_tags(tags)
    .await
    .expect("the rejoined node re-tags itself");
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Update)
    .await;
  let seen = cluster
    .node(0)
    .members()
    .iter()
    .find(|m| m.node().id_ref().as_str() == subject.as_str())
    .map(|m| m.tags().0.get("version").cloned())
    .expect("A tracks the rejoined B");
  assert_eq!(
    seen.as_deref(),
    Some("v2"),
    "the failure/rejoin cycle ends with the refreshed tag visible"
  );

  cluster.shutdown_all().await;
}

/// Poll `observer`'s member view until `subject`'s `version` tag equals
/// `want` — the propagation fence for a retag.
async fn await_version_tag<R>(
  cluster: &cluster::Cluster<R>,
  observer: usize,
  subject: &str,
  want: &str,
) where
  R: Runtime,
{
  R::timeout(Duration::from_secs(20), async {
    loop {
      let seen = cluster
        .node(observer)
        .members()
        .iter()
        .find(|m| m.node().id_ref().as_str() == subject)
        .and_then(|m| m.tags().0.get("version").cloned());
      if seen.as_deref() == Some(want) {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .unwrap_or_else(|_| panic!("the observer never sees {subject}'s version tag reach {want:?}"));
}

/// Port of legacy `serf_update` (Go `TestSerf_Update`): a node restarts with
/// CHANGED tags and rejoins while the observer still holds it Alive, and the
/// observer surfaces the change as a member Update — never a Failed/revival
/// detour (whose Join would fold the tags in without a distinct Update).
///
/// The mechanism underneath is the restart self-refutation: a stale claim
/// about the restarted node (its pre-restart incarnation, the old tags)
/// reaches it, and it refutes by re-broadcasting a bumped Alive carrying its
/// CURRENT tags, which the observer applies as an alive-to-alive metadata
/// change — `NodeUpdated` → `Member(Update)`.
///
/// Staging makes the refutation's incarnation BUMP load-bearing end-to-end.
/// A retag bumps the local incarnation, so a pre-kill retag raises the
/// observer's held incarnation to exactly the value the restarted node
/// reaches after its own post-restart retag (fresh incarnation + one bump).
/// Every Alive the restarted node can originate on its own is therefore
/// equal-incarnation at the observer and stale-dropped; with failure
/// detection parked (the observer holds the subject Alive across the whole
/// cycle, the reference's restart-beats-detection timing) and periodic
/// push/pull disabled, NO schedule delivers the new tags unless a
/// refutation first lifts the restarted node past the observer's held
/// incarnation.
///
/// WHICH message then carries the new tags is timing-dependent at driver
/// level: the refutation may be triggered by the rejoin exchange itself
/// (its bumped Alive carries the tags directly) or slightly earlier by a
/// leftover gossip retransmit of the stale claim (an empty-meta refutation
/// lifts the incarnation and the retag's own broadcast is then admitted
/// above it, surfacing one extra Update). Both routes are the same
/// mechanism and converge on the same view; the log assertion therefore
/// pins the event-kind shape rather than an exact count, and the
/// carrier-level causality — an equal-incarnation different-meta self-claim
/// refutes with a broadcast carrying the CURRENT metadata — is pinned
/// deterministically at the machine layer
/// (`alive_node_refute_equal_incarnation_carries_current_meta` in
/// memberlist-proto's SWIM parity suite), where no scheduler is involved.
async fn serf_update_after_restart_with_changed_tags<R>()
where
  R: Runtime,
{
  let mut cluster = cluster::Cluster::<R>::spawn(
    &["updm-a", "updm-b"],
    cluster::ClusterTiming::fast()
      .with_probe_interval(Duration::from_secs(600))
      .with_push_pull_interval(Duration::ZERO),
  )
  .await;
  let subject = cluster.id(1);

  cluster
    .assert_member_events(0, subject.as_str(), &[MemberEventKind::Join])
    .await;

  // The incarnation-equalizing retag: after this propagates, the observer
  // holds the subject at fresh-incarnation-plus-one — the same value the
  // subject reaches below after restarting and retagging once.
  let mut tags = serf_proto::Tags::new();
  tags.0.insert(SmolStr::new("version"), SmolStr::new("v1"));
  cluster
    .node(1)
    .set_tags(tags)
    .await
    .expect("the subject tags itself before the restart");
  await_version_tag(&cluster, 0, subject.as_str(), "v1").await;

  cluster.kill_abrupt(1).await;
  cluster.restart(1).await;

  // Present the changed tags BEFORE rejoining, mirroring the reference's
  // restart-with-new-tags configuration. This lands the restarted node at
  // the observer's held incarnation, so only the refutation below can carry
  // the new value.
  let mut tags = serf_proto::Tags::new();
  tags.0.insert(SmolStr::new("version"), SmolStr::new("v2"));
  cluster
    .node(1)
    .set_tags(tags)
    .await
    .expect("the restarted node presents changed tags");

  let a_addr = cluster.node(0).advertise_address();
  cluster
    .node(1)
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the restarted node rejoins through the survivor");

  // The completion fence: a refutation-lifted Alive delivers v2.
  await_version_tag(&cluster, 0, subject.as_str(), "v2").await;

  // The whole cycle surfaced as one admission followed by nothing but
  // Updates: no Failed (probing parked), no second Join (the observer never
  // saw the subject leave), and both retags surfaced. An early empty-meta
  // refutation may add one benign extra Update (see the doc), so the shape
  // is pinned rather than an exact count.
  let kinds = cluster.member_event_kinds(0, subject.as_str());
  assert!(
    kinds.len() >= 3
      && kinds[0] == MemberEventKind::Join
      && kinds[1..].iter().all(|k| *k == MemberEventKind::Update),
    "the restart cycle must surface as one Join then only Updates (got {kinds:?})"
  );

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_query_filter` (Go `TestSerf_Query_Filter`): an
/// Id-filtered query surfaces on the FILTERED node only, and the originator
/// collects exactly that node's response. Standalone nodes rather than the
/// cluster fixture: the fixture's collector round-robins the event stream
/// away from scenario subscribers.
///
/// `relay_factor = 1` matches the legacy parameters, but a relayed duplicate
/// is not FORCED to reach the originator here (the responder's relay pick may
/// select the originator itself, whose self-relay is dropped, and relay
/// forwarding is best-effort), so duplicate suppression is NOT this
/// scenario's claim — it is pinned deterministically at the machine layer by
/// serf-proto's `duplicate_query_response_is_deduped`.
async fn serf_query_filter<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("qf-a").await;
  let b = spawn_node::<R>("qf-b").await;
  let c = spawn_node::<R>("qf-c").await;
  let a_addr = a.advertise_address();

  // Subscribe before any join so no event races the subscriptions.
  let mut a_events = a.events();
  let mut b_events = b.events();
  let mut c_events = c.events();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("B joins through A");
  c.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("C joins through A");
  R::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 3 && b.num_members() == 3 && c.num_members() == 3 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the three nodes converge");

  // An explicit timeout makes the query LIFETIME known, so the exclusivity
  // drains below can cover it whole (the default would be computed from the
  // gossip cadence and member count).
  let query_lifetime = Duration::from_secs(3);
  let mut params = a.default_query_param();
  params.filters = vec![serf_proto::typed::Filter::Id(vec![SmolStr::new("qf-b")])];
  params.relay_factor = 1;
  params.timeout = query_lifetime;
  a.query("who", Bytes::from_static(b"filtered"), params)
    .await
    .expect("the filtered query dispatches");

  // B — the filtered target — surfaces the query and answers it.
  let responded = R::timeout(Duration::from_secs(20), async {
    loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "who" => {
          b.respond(qe, Bytes::from_static(b"b-here"))
            .await
            .expect("B responds to the filtered query");
          break true;
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("B surfaces the filtered query within the timeout");
  assert!(responded, "the filtered target answers");

  // Drain the originator across the WHOLE query lifetime plus a margin:
  // exactly one matching response — the single responder inside the filter —
  // and no surfaced `Event::Query`, the originator being outside its own
  // Id filter too.
  let drain_window = query_lifetime + Duration::from_secs(1);
  let mut responses: Vec<(SmolStr, Bytes)> = Vec::new();
  // Ignoring Err: the timeout IS the drain bound; events collected until it
  // elapses are what the assertions below examine.
  let _ = R::timeout(drain_window, async {
    loop {
      match a_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "who" => {
          panic!("the originator is outside the Id filter and must not surface the query");
        }
        Some(Event::QueryResponse(qr)) => {
          responses.push((qr.from().id_ref().clone(), qr.payload().clone()));
        }
        Some(_) => {}
        None => break,
      }
    }
  })
  .await;
  assert_eq!(
    responses.len(),
    1,
    "exactly one responder sits inside the Id filter (got {responses:?})"
  );
  assert_eq!(responses[0].0.as_str(), "qf-b");
  assert_eq!(responses[0].1, Bytes::from_static(b"b-here"));

  // C — filtered out — must stay silent across the same whole lifetime.
  let saw_query = R::timeout(drain_window, async {
    loop {
      match c_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "who" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .unwrap_or(false);
  assert!(
    !saw_query,
    "a node outside the Id filter must not surface the query"
  );

  a.shutdown().await.expect("qf-a shuts down");
  b.shutdown().await.expect("qf-b shuts down");
  c.shutdown().await.expect("qf-c shuts down");
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
    None,
    None,
    None,
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
  async fn reconnect_delegate_reaps_left_member() {
    super::reconnect_delegate_reaps_left_member::<TokioRuntime>().await;
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

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_events_failed() {
    super::serf_events_failed::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_events_leave() {
    super::serf_events_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_events_leave_with_racing_shutdown() {
    super::serf_events_leave_with_racing_shutdown::<TokioRuntime>().await;
  }

  #[cfg(feature = "coordinates")]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn coordinates_surface_on_the_handle() {
    super::coordinates_surface_on_the_handle::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_restart_rejoins_the_cluster() {
    super::snapshot_restart_rejoins_the_cluster::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_leave_gate_controls_rejoin() {
    super::snapshot_leave_gate_controls_rejoin::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn merge_delegate_is_consulted_on_join() {
    super::merge_delegate_is_consulted_on_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn leave_with_zero_timeout_racing_shutdown_times_out() {
    super::leave_with_zero_timeout_racing_shutdown_times_out::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_reconnect() {
    super::serf_reconnect::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_force_leave_failed() {
    super::serf_force_leave_failed::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_force_leave_left_is_idempotent() {
    super::serf_force_leave_left_is_idempotent::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_remove_failed_node_propagates() {
    super::serf_remove_failed_node_propagates::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_remove_failed_node_prune_erases() {
    super::serf_remove_failed_node_prune_erases::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn remove_failed_node_absent_is_a_noop() {
    super::remove_failed_node_absent_is_a_noop::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_reconnect_same_ip() {
    super::serf_reconnect_same_ip::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_join_cancel() {
    super::serf_join_cancel::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_set_tags_propagates() {
    super::serf_set_tags_propagates::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_update_after_rejoin() {
    super::serf_update_after_rejoin::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_update_after_restart_with_changed_tags() {
    super::serf_update_after_restart_with_changed_tags::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_query_filter() {
    super::serf_query_filter::<TokioRuntime>().await;
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
  fn reconnect_delegate_reaps_left_member_smol() {
    SmolRuntime::block_on(super::reconnect_delegate_reaps_left_member::<SmolRuntime>());
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

  #[test]
  fn serf_events_failed_smol() {
    SmolRuntime::block_on(super::serf_events_failed::<SmolRuntime>());
  }

  #[test]
  fn serf_events_leave_smol() {
    SmolRuntime::block_on(super::serf_events_leave::<SmolRuntime>());
  }

  #[test]
  fn serf_events_leave_with_racing_shutdown_smol() {
    SmolRuntime::block_on(super::serf_events_leave_with_racing_shutdown::<SmolRuntime>());
  }

  #[cfg(feature = "coordinates")]
  #[test]
  fn coordinates_surface_on_the_handle_smol() {
    SmolRuntime::block_on(super::coordinates_surface_on_the_handle::<SmolRuntime>());
  }

  #[test]
  fn snapshot_restart_rejoins_the_cluster_smol() {
    SmolRuntime::block_on(super::snapshot_restart_rejoins_the_cluster::<SmolRuntime>());
  }

  #[test]
  fn snapshot_leave_gate_controls_rejoin_smol() {
    SmolRuntime::block_on(super::snapshot_leave_gate_controls_rejoin::<SmolRuntime>());
  }

  #[test]
  fn merge_delegate_is_consulted_on_join_smol() {
    SmolRuntime::block_on(super::merge_delegate_is_consulted_on_join::<SmolRuntime>());
  }

  #[test]
  fn leave_with_zero_timeout_racing_shutdown_times_out_smol() {
    SmolRuntime::block_on(super::leave_with_zero_timeout_racing_shutdown_times_out::<
      SmolRuntime,
    >());
  }

  #[test]
  fn serf_reconnect_smol() {
    SmolRuntime::block_on(super::serf_reconnect::<SmolRuntime>());
  }

  #[test]
  fn serf_force_leave_failed_smol() {
    SmolRuntime::block_on(super::serf_force_leave_failed::<SmolRuntime>());
  }

  #[test]
  fn serf_force_leave_left_is_idempotent_smol() {
    SmolRuntime::block_on(super::serf_force_leave_left_is_idempotent::<SmolRuntime>());
  }

  #[test]
  fn serf_remove_failed_node_propagates_smol() {
    SmolRuntime::block_on(super::serf_remove_failed_node_propagates::<SmolRuntime>());
  }

  #[test]
  fn serf_remove_failed_node_prune_erases_smol() {
    SmolRuntime::block_on(super::serf_remove_failed_node_prune_erases::<SmolRuntime>());
  }

  #[test]
  fn remove_failed_node_absent_is_a_noop_smol() {
    SmolRuntime::block_on(super::remove_failed_node_absent_is_a_noop::<SmolRuntime>());
  }

  #[test]
  fn serf_reconnect_same_ip_smol() {
    SmolRuntime::block_on(super::serf_reconnect_same_ip::<SmolRuntime>());
  }

  #[test]
  fn serf_join_cancel_smol() {
    SmolRuntime::block_on(super::serf_join_cancel::<SmolRuntime>());
  }

  #[test]
  fn serf_set_tags_propagates_smol() {
    SmolRuntime::block_on(super::serf_set_tags_propagates::<SmolRuntime>());
  }

  #[test]
  fn serf_update_after_rejoin_smol() {
    SmolRuntime::block_on(super::serf_update_after_rejoin::<SmolRuntime>());
  }

  #[test]
  fn serf_update_after_restart_with_changed_tags_smol() {
    SmolRuntime::block_on(super::serf_update_after_restart_with_changed_tags::<
      SmolRuntime,
    >());
  }

  #[test]
  fn serf_query_filter_smol() {
    SmolRuntime::block_on(super::serf_query_filter::<SmolRuntime>());
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
