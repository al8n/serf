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
  members::{MemberStatus, SerfState},
  options::Options as SerfOptions,
};
#[cfg(encryption)]
use serf_reactor::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SerfError, SocketAddrResolver,
  TcpTransportOptions, VoidDelegate,
};
use smol_str::SmolStr;

/// The reusable multi-node fault-injection fixture, shared by the tokio and smol
/// cells below.
mod cluster;

/// A reactor TCP node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// The fixture's plain-TCP backend: the fast-SWIM timing mapped onto a
/// [`TcpTransportOptions`] block.
struct Tcp;

impl<R> cluster::Backend<R> for Tcp
where
  R: Runtime,
{
  async fn build(
    id: &str,
    bind: SocketAddr,
    timing: &cluster::ClusterTiming,
  ) -> serf_reactor::Result<cluster::Node<R>> {
    let mut opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(bind))
      .with_probe_interval(timing.probe_interval())
      .with_probe_timeout(timing.probe_timeout())
      .with_gossip_interval(timing.gossip_interval())
      .with_suspicion_mult(timing.suspicion_mult());
    if let Some(v) = timing.dead_node_reclaim {
      opts = opts.with_dead_node_reclaim_time(v);
    }
    if let Some(v) = timing.push_pull_interval {
      opts = opts.with_push_pull_interval(v);
    }
    Serf::<SmolStr, SocketAddr, R>::tcp(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      timing.serf_opts(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
    )
    .await
  }
}

/// The plain-TCP fault-injection cluster.
type TcpCluster<R> = cluster::Cluster<R, Tcp>;

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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
    TcpCluster::<R>::spawn(&["coord-a", "coord-b"], cluster::ClusterTiming::fast()).await;
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
  bind: SocketAddr,
  snapshot: serf_reactor::SnapshotOptions,
  rejoin_after_leave: bool,
) -> Result<Node<R>, serf_reactor::SerfError>
where
  R: Runtime,
{
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
}

/// An ephemeral loopback bind (`127.0.0.1:0`).
fn ephemeral_bind() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
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
  let b = spawn_node_with_snapshot::<R>(
    "snap-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
  let a_addr = a.advertise_address();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Abrupt kill: no leave marker lands in the snapshot.
  b.shutdown().await.expect("snap-b shuts down");

  // A fresh B from the same snapshot auto-rejoins A (no join call).
  let b2 = spawn_node_with_snapshot::<R>(
    "snap-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
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
  let b = spawn_node_with_snapshot::<R>(
    "gate-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
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
  let b2 = spawn_node_with_snapshot::<R>(
    "gate-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
  R::sleep(Duration::from_millis(1500)).await;
  assert_eq!(
    b2.num_members(),
    1,
    "a cleanly-left node must not auto-rejoin unless opted in"
  );
  b2.shutdown().await.expect("gate-b2 shuts down");

  // Opt-in posture: the Leave marker is ignored and the membership recovers.
  let b3 = spawn_node_with_snapshot::<R>(
    "gate-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    true,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
  let mut cluster = TcpCluster::<R>::spawn(
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
    TcpCluster::<R>::spawn(&["tags-a", "tags-b"], cluster::ClusterTiming::fast()).await;
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
  let mut cluster = TcpCluster::<R>::spawn(
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
async fn await_version_tag<R>(cluster: &TcpCluster<R>, observer: usize, subject: &str, want: &str)
where
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
  let mut cluster = TcpCluster::<R>::spawn(
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

  // The completion fence on the VIEW: a refutation-lifted Alive delivers v2.
  await_version_tag(&cluster, 0, subject.as_str(), "v2").await;

  // Fence the COLLECTOR too — the view publishes independently of the event
  // stream — by waiting for the logged Update that carries v2, then assert
  // the exact prefix through that event: one admission followed by nothing
  // but Updates. No Failed (probing parked), no second Join (the observer
  // never saw the subject leave), both retags surfaced, and an early
  // empty-meta refutation may add one benign extra Update (see the doc), so
  // the shape is pinned rather than an exact count.
  let prefix = cluster
    .await_member_event_with_tag(
      0,
      subject.as_str(),
      MemberEventKind::Update,
      "version",
      "v2",
    )
    .await;
  assert!(
    prefix.len() >= 3
      && prefix[0] == MemberEventKind::Join
      && prefix[1..].iter().all(|k| *k == MemberEventKind::Update),
    "the restart cycle through the v2 Update must surface as one Join then only Updates (got {prefix:?})"
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

/// Port of legacy `serf_join_leave` (Go `TestSerf_JoinLeave`): after a peer
/// leaves gracefully, the departure settles on BOTH sides under the DEFAULT
/// tombstone timeout. The other leave e2es raise the tombstone timeout to HOLD
/// the tombstone and pin the event sequence; this one exercises the plain
/// default-tombstone reap that none of them cover, and — like the legacy body —
/// checks the leaver's own side, not just the observer's.
///
/// The leaver is kept running rather than shut down so its own convergence is
/// observable. The local node is exempt from reaping (a running node always
/// knows itself), so the leaver HOLDS its self `Left` tombstone rather than
/// reaping it: its own event log is exactly `Join → Leave` — no self `Reap` —
/// and its live membership view still shows itself `Left` beside the `Alive`
/// peer. This is the al8n/serf#88 fix: before the exemption the machine reaped
/// self and `refresh_snapshot` then froze the view (it will not publish a
/// snapshot missing the local id). The ABSENCE of the self `Reap` is what
/// distinguishes the held (correct) case from the reaped-then-frozen (buggy)
/// one — the frozen view carried the same member values.
async fn serf_join_leave<R>()
where
  R: Runtime,
{
  let mut cluster = TcpCluster::<R>::spawn(&["jl-a", "jl-b"], cluster::ClusterTiming::fast()).await;
  let peer = cluster.id(0);
  let leaver = cluster.id(1);

  // Leave in place — keep B's handle live so its own convergence is observable.
  cluster.leave_in_place(1).await;

  // A (observer) reaps the departed B under the fast profile's 1ms tombstone +
  // 100ms reap ticks, returning to just itself. This also fences past the reap
  // window, so any (regressed) self-reap on B would have surfaced by now.
  cluster.await_num_members(0, 1).await;

  // B (the leaver) holds its self tombstone: its own event log is exactly
  // Join → Leave, with NO self Reap.
  assert_eq!(
    cluster.member_event_kinds(1, leaver.as_str()),
    vec![MemberEventKind::Join, MemberEventKind::Leave],
    "the leaver holds its self tombstone: Join then Leave, never a self Reap"
  );
  // Its live view still shows itself Left beside the still-Alive peer — never
  // frozen, never dropping the live peer.
  cluster
    .await_member_status(1, leaver.as_str(), MemberStatus::Left)
    .await;
  cluster
    .await_member_status(1, peer.as_str(), MemberStatus::Alive)
    .await;

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_join_leave_join` (Go `TestSerf_JoinLeaveJoin`): a peer
/// leaves — the observer holds it as a Left tombstone — then the same node
/// restarts and rejoins, and the observer transitions it Left → Alive. The
/// tombstone timeout is raised so the Left state is observable before the
/// rejoin rather than reaped away first.
async fn serf_join_leave_join<R>()
where
  R: Runtime,
{
  let mut cluster = TcpCluster::<R>::spawn(
    &["jlj-a", "jlj-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);
  let seed = cluster.node(0).advertise_address();

  cluster.leave_graceful(1).await;
  cluster.await_left_tombstone(0, subject.as_str()).await;

  cluster.restart(1).await;
  cluster
    .node(1)
    .join(&SocketAddrResolver, MaybeResolved::Resolved(seed), false)
    .await
    .expect("the restarted node rejoins the seed");

  // A transitions B from its Left tombstone back to Alive.
  cluster
    .await_member_status(0, subject.as_str(), MemberStatus::Alive)
    .await;

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_leave_rejoin_different_role` (Go
/// `TestSerf_LeaveRejoin_DifferentRole`): a node leaves, then a fresh node
/// rejoins at the SAME id and address carrying a DIFFERENT role tag, and the
/// observer's view reflects the new role. The legacy test sets the role at
/// construction; the Sans-I/O stack has no start-time tag surface, so the
/// restarted node applies it via `set_tags` before the rejoin — the revival
/// folds the tag into its Join (matching the reference `handleNodeJoin`), and
/// the observer ends up seeing the new value.
async fn serf_leave_rejoin_different_role<R>()
where
  R: Runtime,
{
  let mut cluster = TcpCluster::<R>::spawn(
    &["lrr-a", "lrr-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);
  let seed = cluster.node(0).advertise_address();

  cluster.leave_graceful(1).await;
  cluster.await_left_tombstone(0, subject.as_str()).await;

  cluster.restart(1).await;
  let mut tags = serf_proto::Tags::new();
  tags.0.insert(SmolStr::new("role"), SmolStr::new("bar"));
  cluster
    .node(1)
    .set_tags(tags)
    .await
    .expect("the restarted node adopts the new role before rejoining");
  cluster
    .node(1)
    .join(&SocketAddrResolver, MaybeResolved::Resolved(seed), false)
    .await
    .expect("the re-roled node rejoins the seed");

  // A must see B back as Alive AND carrying the new role — the two ride the
  // same revival Join, so poll them together to avoid reading the view between
  // the status flip and the tag landing.
  R::timeout(Duration::from_secs(20), async {
    loop {
      let seen = cluster.node(0).members().iter().any(|m| {
        m.node().id_ref().as_str() == subject.as_str()
          && m.status() == MemberStatus::Alive
          && m.tags().0.get("role").map(SmolStr::as_str) == Some("bar")
      });
      if seen {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A never sees the rejoined B as Alive carrying role=bar");

  cluster.shutdown_all().await;
}

/// Port of legacy `serf_per_node_reconnect_timeout` (Go
/// `TestSerf_PerNodeReconnectTimeout`): a per-member `ReconnectDelegate`
/// override drives the FAILED-member reap timeout, not just the graceful-LEFT
/// tombstone. Node A carries a delegate that zeroes node B's reconnect timeout
/// while A's flat `reconnect_timeout` stays at 24h; after B is abruptly killed
/// and A detects it Failed, A reaps B early — which can only happen if the
/// reaper consulted the delegate on the FAILED path (the flat 24h timeout would
/// otherwise hold B for the whole test). The companion
/// `reconnect_delegate_reaps_left_member` covers the LEFT/tombstone path; this
/// covers the FAILED/reconnect path.
async fn serf_per_node_reconnect_timeout<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("prt-b").await;
  let b_addr = b.advertise_address();

  // A: fast SWIM so it detects B's kill sub-second, a long flat reconnect
  // timeout so a default reap never fires within the test, and a delegate
  // zeroing ONLY B's reconnect timeout.
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("prt-a"))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_probe_interval(Duration::from_millis(100))
    .with_probe_timeout(Duration::from_millis(50))
    .with_gossip_interval(Duration::from_millis(20))
    .with_suspicion_mult(3);
  let serf_opts = SerfOptions::new()
    .with_reap_interval(Duration::from_millis(100))
    .with_reconnect_timeout(Duration::from_secs(86_400));
  let a = Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    serf_opts,
    Some(Box::new(ReapImmediately {
      target: SmolStr::new("prt-b"),
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

  // Abruptly kill B (no farewell) so A detects a probe-timeout Failed rather
  // than a graceful Leave.
  b.shutdown().await.expect("prt-b shuts down abruptly");

  // A detects B Failed, then the delegate zeroes B's reconnect timeout so A's
  // next reap tick drops B — back to a single member. Without the delegate
  // consult, A would hold the failed B for the flat 24h.
  R::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 1 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A reaps the failed member B early via the reconnect-delegate override");

  a.shutdown().await.expect("prt-a shuts down");
}

/// Port of legacy `serf_snapshot_recovery` (Go `TestSerf_SnapshotRecovery`): a
/// snapshot-backed node that fails, is force-removed by the survivor, then
/// restarts from its snapshot and rejoins — WITHOUT replaying the pre-failure
/// user event onto its fresh event channel. Distinguishes itself from
/// `snapshot_restart_rejoins_the_cluster` by the explicit `remove_failed_node`
/// before the restart (the survivor tombstones B as Left, not merely Failed)
/// and by asserting the recovered node surfaces zero user/query events.
async fn serf_snapshot_recovery<R>()
where
  R: Runtime,
{
  let path = snapshot_path::<R>("recovery");
  let a = spawn_node::<R>("sr-a").await;
  let b = spawn_node_with_snapshot::<R>(
    "sr-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
  let a_addr = a.advertise_address();
  let b_id = b.local_id();
  // Capture B's address: the restart must rebind it EXACTLY, because A holds B
  // as a tombstone at this address (no dead-node reclaim window is configured),
  // so a restart at a fresh port would be a same-id-new-address name conflict —
  // exercising conflict resolution instead of snapshot recovery.
  let b_addr = b.advertise_address();

  let mut b_events = b.events();
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // A fires a user event; fence on the pre-failure B receiving it, so the
  // recovery below is genuinely tested for NOT replaying it.
  a.user_event("event!", Bytes::from_static(b"test"), false)
    .await
    .expect("user event dispatched");
  R::timeout(Duration::from_secs(20), async {
    loop {
      match b_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "event!" => break,
        Some(_) => {}
        None => break,
      }
    }
  })
  .await
  .expect("pre-failure B observes A's user event");

  // Abruptly kill B; A detects it Failed.
  b.shutdown().await.expect("sr-b shuts down");
  R::timeout(Duration::from_secs(20), async {
    loop {
      let failed = a
        .members()
        .iter()
        .any(|m| m.node().id_ref() == &b_id && m.status() == MemberStatus::Failed);
      if failed {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A detects B failed");

  // Force-remove the failed B: A tombstones it Left (not merely Failed), so the
  // restart below revives against a Left tombstone.
  a.remove_failed_node(b_id.clone())
    .await
    .expect("A force-removes the failed B");
  R::timeout(Duration::from_secs(20), async {
    loop {
      let left = a
        .members()
        .iter()
        .any(|m| m.node().id_ref() == &b_id && m.status() == MemberStatus::Left);
      if left {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A holds B as a Left tombstone after the force-remove");

  // Restart B from the snapshot with a FRESH event channel, at its ORIGINAL
  // address so the revival is a clean recovery, not an address conflict. The
  // freed port may not be instantly rebindable, so retry a bounded number of
  // times.
  let b2 = {
    let mut attempt = 0usize;
    loop {
      match spawn_node_with_snapshot::<R>(
        "sr-b",
        b_addr,
        serf_reactor::SnapshotOptions::new(&path),
        false,
      )
      .await
      {
        Ok(node) => break node,
        // Ignoring Err: a transient rebind race on the just-freed port is
        // retried; only the final attempt's failure is fatal.
        Err(_) if attempt + 1 < 25 => {
          attempt += 1;
          R::sleep(Duration::from_millis(20)).await;
        }
        Err(e) => panic!("restart rebind for sr-b at {b_addr} failed: {e}"),
      }
    }
  };
  let mut b2_events = b2.events();
  converge(&a, &b2).await;

  // The recovery must NOT replay the pre-failure user event (nor any query) onto
  // the restarted node's channel — only membership events are permitted. Drain a
  // settle window and fail on any surfaced user/query event.
  let quiet = R::timeout(Duration::from_secs(2), async {
    loop {
      match b2_events.next().await {
        Some(Event::User(u)) => break Some(format!("user:{}", u.name)),
        Some(Event::Query(q)) => break Some(format!("query:{}", q.name())),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await;
  assert!(
    quiet.is_err(),
    "the recovered node must replay no user/query events, saw {quiet:?}"
  );

  a.shutdown().await.expect("sr-a shuts down");
  b2.shutdown().await.expect("sr-b2 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Spawn a standalone reactor TCP node with fast SWIM timing, so failure
/// detection and query resolution run sub-second (the plain [`spawn_node`] uses
/// default, slower timing).
async fn spawn_fast_node<R>(id: &str) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_probe_interval(Duration::from_millis(100))
    .with_probe_timeout(Duration::from_millis(50))
    .with_gossip_interval(Duration::from_millis(20))
    .with_suspicion_mult(3);
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
  .expect("spawn fast serf tcp node")
}

/// Port of legacy `serf_name_resolution` (Go `TestSerf_NameResolution`): a
/// third node claiming an id already held in the cluster loses the conflict
/// vote and shuts down, while the incumbent survives. `nr-dup` is spawned with
/// the SAME id as the incumbent `nr-1`; after both are joined, the incumbent
/// (which the rest of the cluster already knows) wins the name-resolution query
/// and the newcomer transitions itself to Shutdown.
async fn serf_name_resolution<R>()
where
  R: Runtime,
{
  let s1 = spawn_fast_node::<R>("nr-1").await;
  let s2 = spawn_fast_node::<R>("nr-2").await;
  let s3 = spawn_fast_node::<R>("nr-1").await; // duplicate of s1's id
  let s2_addr = s2.advertise_address();
  let s3_addr = s3.advertise_address();

  // Join the incumbent to s2 first, so the cluster knows nr-1 at s1's address
  // and will vote for it in the conflict.
  s1.join(&SocketAddrResolver, MaybeResolved::Resolved(s2_addr), false)
    .await
    .expect("s1 joins s2");
  converge(&s1, &s2).await;

  // Introduce the duplicate: joining nr-1@s3 into a cluster that already holds
  // nr-1@s1 triggers the name-resolution conflict.
  // Ignoring Err: the join may itself surface the conflict as an error; the
  // resolution below is what the test asserts.
  let _ = s1
    .join(&SocketAddrResolver, MaybeResolved::Resolved(s3_addr), false)
    .await;

  // The newcomer loses the vote and shuts itself down; the incumbent survives.
  R::timeout(Duration::from_secs(30), async {
    loop {
      if s3.state() == SerfState::Shutdown {
        break;
      }
      R::sleep(Duration::from_millis(50)).await;
    }
  })
  .await
  .expect("the duplicate-id newcomer loses the conflict and shuts down");
  assert_eq!(
    s1.state(),
    SerfState::Alive,
    "the incumbent survives the conflict"
  );

  s1.shutdown().await.expect("nr-1 shuts down");
  s2.shutdown().await.expect("nr-2 shuts down");
  // s3 already shut itself down on the conflict loss.
}

/// A test-only [`MessageDropper`](serf_proto::MessageDropper) that drops inbound
/// joins and push/pulls while its shared flag is set — the reactor analog of
/// the legacy `DropJoins`.
#[cfg(feature = "test")]
#[derive(Clone)]
struct DropJoins {
  drop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "test")]
impl serf_proto::MessageDropper for DropJoins {
  fn should_drop(&self, kind: serf_proto::DropKind) -> bool {
    matches!(
      kind,
      serf_proto::DropKind::Join | serf_proto::DropKind::PushPull
    ) && self.drop.load(std::sync::atomic::Ordering::SeqCst)
  }
}

/// Spawn a fast-SWIM node bound to `bind`, holding Left tombstones for the whole
/// test, and — when `drop_flag` is `Some` — dropping inbound joins/push-pulls
/// while that flag is set.
#[cfg(feature = "test")]
async fn spawn_air_node<R>(
  id: &str,
  bind: SocketAddr,
  drop_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Node<R>, serf_reactor::SerfError>
where
  R: Runtime,
{
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_probe_interval(Duration::from_millis(100))
    .with_probe_timeout(Duration::from_millis(50))
    .with_gossip_interval(Duration::from_millis(20))
    .with_suspicion_mult(3);
  let delegate = match drop_flag {
    Some(flag) => VoidDelegate::<SmolStr, SocketAddr>::new()
      .with_message_dropper(std::sync::Arc::new(DropJoins { drop: flag })),
    None => VoidDelegate::<SmolStr, SocketAddr>::new(),
  };
  Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    delegate,
    RuntimeOptions::new(),
    // Hold Left tombstones so every node's post-leave view is observable, and
    // never auto-reap during the double-leave.
    SerfOptions::new().with_tombstone_timeout(Duration::from_secs(30)),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
}

/// Poll until every node in `nodes` reports `expect` members, or panic on the
/// bound.
#[cfg(feature = "test")]
async fn await_all_num_members<R>(nodes: &[&Node<R>], expect: usize)
where
  R: Runtime,
{
  R::timeout(Duration::from_secs(30), async {
    loop {
      if nodes.iter().all(|n| n.num_members() == expect) {
        break;
      }
      R::sleep(Duration::from_millis(25)).await;
    }
  })
  .await
  .expect("all nodes reach the expected member count");
}

/// Exercises the test-only [`MessageDropper`](serf_proto::MessageDropper)
/// infrastructure end to end: a node whose delegate drops every inbound join /
/// push-pull never learns its peer, while the peer — dropping nothing — learns
/// it. This proves the reactor installs the delegate's dropper on the machine
/// and the machine consults it at ingress. (It is NOT the legacy
/// avoid-infinite-rebroadcast scenario, whose anti-rebroadcast property is
/// pinned deterministically at the machine layer by
/// `handle_node_leave_intent_updates_status_time_for_leaving` +
/// `stale_leave_intent_is_dropped`; the reactor cannot observe that property
/// non-vacuously.)
#[cfg(feature = "test")]
async fn message_dropper_drops_inbound_joins<R>()
where
  R: Runtime,
{
  // B drops every inbound join/push-pull from the start.
  let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
  let a = spawn_air_node::<R>("md-a", ephemeral_bind(), None)
    .await
    .expect("spawn md-a");
  let b = spawn_air_node::<R>("md-b", ephemeral_bind(), Some(flag.clone()))
    .await
    .expect("spawn md-b");
  let b_addr = b.advertise_address();

  // A joins B: A dials B and learns it from the exchange. B drops A's inbound
  // push-pull AND every gossip-induced NodeJoined, so B never learns A.
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("md-a joins md-b");

  // A converges to the 2-member cluster (it sees B).
  await_all_num_members(&[&a], 2).await;

  // B stays at a single member across a settle window spanning many gossip
  // rounds — it drops every inbound join, so it never learns A. Without the
  // dropper B would reach 2.
  R::sleep(Duration::from_secs(2)).await;
  assert_eq!(
    b.num_members(),
    1,
    "the dropper node never learns its peer — every inbound join is dropped"
  );

  a.shutdown().await.expect("md-a shuts down");
  b.shutdown().await.expect("md-b shuts down");
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

/// Every mutating operation is REFUSED with `NotRunning` once the node has left
/// the cluster — `leave()` stops the periodic schedulers, so a post-leave mutation
/// would be issued by a node no longer participating. The read-only coordinate
/// probe is the deliberate exception: post-leave introspection stays valid.
///
/// `respond` is included by capturing a live query token BEFORE the leave, which
/// is the only way its post-leave arm can be reached at all.
async fn post_leave_operations_report_not_running<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("nr-a").await;
  let b = spawn_node::<R>("nr-b").await;
  let a_addr = a.advertise_address();

  let mut b_events = b.events();
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Capture a live query token on B, so the post-leave `respond` below has
  // something real to answer.
  a.query("probe", Bytes::from_static(b"q"), a.default_query_param())
    .await
    .expect("query issued");
  let token = R::timeout(Duration::from_secs(20), async {
    loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "probe" => break Some(qe),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("B surfaces the query within the timeout")
  .expect("B's event stream stays open");

  b.leave().await.expect("B leaves the cluster");

  let not_running = |e: &serf_reactor::SerfError| matches!(e, serf_reactor::SerfError::NotRunning);

  let err = b
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect_err("a left node cannot rejoin in place");
  assert!(not_running(&err), "join after leave: got {err:?}");

  for (what, res) in [
    (
      "user_event",
      b.user_event("nope", Bytes::from_static(b"x"), false).await,
    ),
    (
      "query",
      b.query("nope", Bytes::from_static(b"x"), b.default_query_param())
        .await
        .map(|_| ()),
    ),
    ("respond", b.respond(token, Bytes::from_static(b"x")).await),
    ("set_tags", b.set_tags(serf_proto::Tags::new()).await),
    (
      "force_leave",
      b.force_leave(SmolStr::new("nr-a"), false).await,
    ),
  ] {
    let err = res.expect_err("a left node refuses to mutate");
    assert!(
      not_running(&err),
      "{what} after leave must report NotRunning, got {err:?}"
    );
  }

  // The read-only coordinate probe still answers after the leave.
  #[cfg(feature = "coordinates")]
  b.cached_coordinate(SmolStr::new("nr-a"))
    .await
    .expect("a read-only coordinate probe stays answerable after leave");

  a.shutdown().await.expect("nr-a shuts down");
  b.shutdown().await.expect("nr-b shuts down");
}

/// The key-management operations refuse to run on a node that has left — the same
/// `NotRunning` gate the membership mutations take, so a departed node can never
/// originate a cluster-wide rotation.
#[cfg(encryption)]
async fn post_leave_key_operations_report_not_running<R>()
where
  R: Runtime,
{
  let node = spawn_encrypted_node::<R>(
    "nrkey",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x61))),
  )
  .await;
  node.leave().await.expect("the node leaves the cluster");

  let k = test_secret_key(0x62);
  for (what, res) in [
    ("install_key", node.install_key(k).await.map(|_| ())),
    ("use_key", node.use_key(k).await.map(|_| ())),
    ("remove_key", node.remove_key(k).await.map(|_| ())),
    ("list_keys", node.list_keys().await.map(|_| ())),
  ] {
    let err = res.expect_err("a left node refuses a key rotation");
    assert!(
      matches!(err, serf_reactor::SerfError::NotRunning),
      "{what} after leave must report NotRunning, got {err:?}"
    );
  }

  node.shutdown().await.expect("nrkey shuts down");
}

/// Leave is a SHARED in-flight operation: two concurrent `leave()` calls JOIN one
/// leave — the machine's `leave()` is invoked once and its single `LeftCluster`
/// resolves BOTH callers `Ok`. (Re-invoking it would be a terminal no-op emitting
/// no second `LeftCluster`, so the second caller would hang to its timeout.) A
/// THIRD leave issued after the chain completed is an accepted no-op, not an
/// error.
async fn concurrent_leaves_share_one_in_flight_leave<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("dl-b").await;
  let a = spawn_node::<R>("dl-a").await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let (first, second) = future::join(a.leave(), a.leave()).await;
  first.expect("the initiating leave resolves Ok");
  second.expect("the leave that JOINED the in-flight one resolves Ok too");

  // The shared leave still drove the endpoint to Left (poll to absorb the
  // snapshot-refresh race after the leave chain completes).
  R::timeout(Duration::from_secs(5), async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the shared leave drives the endpoint to Left");

  // A leave on an already-left node is a terminal no-op, replied to immediately.
  a.leave()
    .await
    .expect("leaving an already-left node is an accepted no-op");

  a.shutdown().await.expect("dl-a shuts down");
  b.shutdown().await.expect("dl-b shuts down");
}

/// An `ignore_old` join records each seed's exchange as a one-shot replay-suppress
/// target, and the driver clears the recording when the exchange terminates: the
/// join still merges membership, and the joiner does NOT replay the seed's
/// pre-join user event onto its fresh event stream. A plain (non-ignoring) join is
/// the control — it DOES surface the buffered event — so the suppression is
/// proven, not merely asserted as an absence.
async fn ignore_old_join_suppresses_the_replay<R>()
where
  R: Runtime,
{
  let seed = spawn_node::<R>("io-seed").await;
  let seed_addr = seed.advertise_address();

  // The seed buffers a user event BEFORE anyone joins.
  seed
    .user_event("old-news", Bytes::from_static(b"stale"), false)
    .await
    .expect("the seed buffers a pre-join user event");

  // The control: a plain join replays the buffered event to the joiner.
  let plain = spawn_node::<R>("io-plain").await;
  let mut plain_events = plain.events();
  plain
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      false,
    )
    .await
    .expect("the plain join reaches the seed");
  let replayed = R::timeout(Duration::from_secs(20), async {
    loop {
      match plain_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "old-news" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("a plain join replays the seed's buffered user event");
  assert!(
    replayed,
    "the control join must surface the buffered event, or the suppression below is vacuous"
  );

  // The subject: an `ignore_old` join merges membership but suppresses the replay.
  let quiet = spawn_node::<R>("io-quiet").await;
  let mut quiet_events = quiet.events();
  let reached = quiet
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      true,
    )
    .await
    .expect("the ignore_old join reaches the seed");
  assert_eq!(reached, seed_addr, "the ignore_old join still merges");
  R::timeout(Duration::from_secs(20), async {
    loop {
      if quiet.num_members() >= 2 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the ignore_old join still converges the membership");

  // No replay reaches the ignoring joiner across a window the control proved is
  // ample.
  let saw = R::timeout(Duration::from_secs(2), async {
    loop {
      match quiet_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "old-news" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .unwrap_or(false);
  assert!(
    !saw,
    "an ignore_old join must not replay the seed's pre-join user events"
  );

  quiet.shutdown().await.expect("io-quiet shuts down");
  plain.shutdown().await.expect("io-plain shuts down");
  seed.shutdown().await.expect("io-seed shuts down");
}

/// A datagram the gossip plane cannot parse is DROPPED and the node keeps serving:
/// the ingress decode failure must not poison the pump. Garbage is injected from a
/// raw UDP socket (no serf peer involved), then the node is proven still live by a
/// fresh peer joining and converging afterwards.
async fn malformed_gossip_datagram_is_ignored<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("junk-a").await;
  let a_addr = a.advertise_address();

  // Raw garbage straight at the gossip socket: not a label frame, not a message.
  let raw = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a raw sender");
  for payload in [
    &b"\xff\xff\xff\xff\xff\xff\xff\xff"[..],
    &b""[..],
    &[0x7fu8; 400][..],
  ] {
    raw
      .send_to(payload, a_addr)
      .expect("the garbage datagram is sent");
  }

  // The node survived the junk: a fresh peer still joins and converges.
  let b = spawn_node::<R>("junk-b").await;
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the node still serves joins after the malformed datagrams");
  converge(&a, &b).await;

  a.shutdown().await.expect("junk-a shuts down");
  b.shutdown().await.expect("junk-b shuts down");
}

/// An encrypted node DROPS a datagram it cannot authenticate — the AEAD open
/// fails, the pump moves on, and the node keeps serving. The garbage is
/// indistinguishable from a forged datagram, so this is the gossip plane's
/// unauthenticated-input gate.
#[cfg(encryption)]
async fn unauthenticatable_gossip_datagram_is_ignored<R>()
where
  R: Runtime,
{
  let key = EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x51)));
  let a = spawn_encrypted_node::<R>("forge-a", key.clone()).await;
  let a_addr = a.advertise_address();

  let raw = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a raw sender");
  for payload in [&[0x00u8; 64][..], &[0xabu8; 300][..]] {
    raw
      .send_to(payload, a_addr)
      .expect("the forged datagram is sent");
  }

  // The node survived the forgeries: a keyring-sharing peer still joins.
  let b = spawn_encrypted_node::<R>("forge-b", key).await;
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the node still serves joins after the unauthenticatable datagrams");
  converge(&a, &b).await;

  a.shutdown().await.expect("forge-a shuts down");
  b.shutdown().await.expect("forge-b shuts down");
}

/// A node that has LEFT admits no further inbound reliable exchange: the accept
/// still happens (the listener is bound until shutdown), but the machine refuses
/// the connection and the driver drops the stream rather than bridging an exchange
/// it will never feed — so the raw peer sees an immediate EOF.
async fn left_node_refuses_new_inbound_exchanges<R>()
where
  R: Runtime,
{
  use std::io::Read;

  let a = spawn_node::<R>("closed-a").await;
  let a_addr = a.advertise_address();
  a.leave().await.expect("A leaves the cluster");

  // A raw reliable dial after the leave: the connection is accepted and then
  // dropped, so the read reaches EOF without a byte of protocol.
  let eof = R::spawn_blocking(move || {
    let mut sock = std::net::TcpStream::connect(a_addr).expect("the listener is still bound");
    sock
      .set_read_timeout(Some(Duration::from_secs(10)))
      .expect("set read timeout");
    // A push/pull request the machine would answer if it were still running.
    let mut buf = [0u8; 64];
    sock.read(&mut buf)
  })
  .await
  .expect("the blocking dial completes");

  match eof {
    Ok(0) => {}
    other => panic!("a left node must drop the accepted stream (EOF), got {other:?}"),
  }

  a.shutdown().await.expect("closed-a shuts down");
}

/// A peer that RESETS mid-exchange must FAIL the join, not complete it: a reset is
/// a transport error, and a one-way frame maps a clean EOF to a SUCCESSFUL
/// completion — so routing the reset down the benign-EOF path would report the
/// reliable exchange as having succeeded against a peer that never answered.
///
/// The evil peer accepts the connection and drops it WITHOUT reading the joiner's
/// already-sent request, so the unread bytes in its receive queue make the close a
/// RST rather than a FIN.
async fn peer_reset_mid_exchange_fails_the_join<R>()
where
  R: Runtime,
{
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the evil peer");
  let peer_addr = listener.local_addr().expect("the evil peer's address");

  let evil = std::thread::spawn(move || {
    if let Ok((stream, _)) = listener.accept() {
      // Let the joiner's push/pull request land in the receive queue unread — the
      // close then resets the connection instead of half-closing it.
      std::thread::sleep(Duration::from_millis(250));
      drop(stream);
    }
  });

  let a = spawn_node::<R>("rst-a").await;
  let outcome = a
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(peer_addr),
      false,
    )
    .await;
  assert!(
    outcome.is_err(),
    "a peer that never answers must fail the join, never complete it (got {outcome:?})"
  );
  assert_eq!(a.num_members(), 1, "nothing was merged from the evil peer");

  // The driver survived the reset: a real peer still joins.
  let b = spawn_node::<R>("rst-b").await;
  let a_addr = a.advertise_address();
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the node still serves joins after the reset exchange");
  converge(&a, &b).await;

  a.shutdown().await.expect("rst-a shuts down");
  b.shutdown().await.expect("rst-b shuts down");
  evil.join().expect("the evil peer thread exits");
}

/// A subscriber that stops draining its `EventStream` must NOT stall the driver:
/// the fan-out sheds the events it cannot deliver and COUNTS them, and the node
/// keeps serving. The shed count is the observable — a driver that instead blocked
/// on the full queue would never reach it.
async fn slow_subscriber_sheds_events_and_counts_them<R>()
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let node = Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("shed-a"))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    // A one-slot event queue: the second undelivered event already overflows.
    RuntimeOptions::new().with_event_queue_cap(1),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn shed-a");

  // Subscribe and NEVER poll the stream: the fan-out queue fills at once.
  let _stalled = node.events();

  for i in 0..64u32 {
    node
      .user_event("flood", Bytes::from(i.to_be_bytes().to_vec()), false)
      .await
      .expect("the driver keeps accepting commands while the subscriber stalls");
  }

  R::timeout(Duration::from_secs(20), async {
    loop {
      if node.events_dropped() > 0 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the fan-out sheds events a stalled subscriber cannot take, and counts them");

  // The node is still live and answering.
  assert_eq!(node.num_members(), 1);
  node.shutdown().await.expect("shed-a shuts down");
}

/// A delegate hook that PARKS must not wedge the driver: the observation channel
/// backs up, the pump retains what application data it can (a bounded overflow),
/// and once THAT is full it sheds the excess and COUNTS the loss — while the node
/// keeps accepting commands throughout. The shed count is the observable; a pump
/// that blocked on the delegate would never reach it.
async fn stalling_delegate_sheds_observations_and_counts_them<R>()
where
  R: Runtime,
{
  /// Enough payload events to fill the two-slot channel, then the pump's bounded
  /// retry overflow (1024 entries), and still leave a surplus that must be shed.
  const FLOOD: u32 = 1200;

  /// A delegate whose user-event hook parks until `release` is dropped. It does
  /// NOT override the test-only message-dropper hook, so the composite's default
  /// (drop nothing) applies.
  struct StallingDelegate {
    gate: flume::Receiver<()>,
  }

  impl serf_reactor::MemberDelegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }
  impl serf_reactor::QueryDelegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }
  impl serf_reactor::UserEventDelegate for StallingDelegate {
    fn notify_user_event(
      &self,
      _event: &serf_proto::typed::UserEventMessage,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
      let gate = self.gate.clone();
      async move {
        // Ignoring Err: a disconnected gate means the test released the hook.
        let _ = gate.recv_async().await;
      }
    }
  }
  impl serf_reactor::Delegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }

  // Hold the sender: while it lives, every `notify_user_event` parks.
  let (release, gate) = flume::bounded::<()>(0);

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let node = Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("stall-a"))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    StallingDelegate { gate },
    // A two-slot observation channel: the parked hook fills it immediately, so the
    // pump must shed rather than block.
    RuntimeOptions::new().with_observation_channel(serf_reactor::Channel::Bounded(2)),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn stall-a");

  // Flood payload events past the pump's bounded retry overflow while the hook parks.
  let payload = Bytes::from(vec![0x5au8; 32]);
  for _ in 0..FLOOD {
    node
      .user_event("flood", payload.clone(), false)
      .await
      .expect("the pump keeps accepting commands while the delegate parks");
  }

  R::timeout(Duration::from_secs(20), async {
    loop {
      if node.observation_dropped() > 0 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the pump sheds observations a parked delegate cannot take, and counts them");

  // The node is still live and answering commands.
  assert_eq!(node.num_members(), 1);

  // Release the hook so the observation task can drain, then shut down.
  drop(release);
  node.shutdown().await.expect("stall-a shuts down");
}

/// An UNBOUNDED observation channel never sheds: with no cap there is no byte
/// backstop and no overflow, so a node that surfaces many payload events reports a
/// zero observation-drop count while still delivering them.
async fn unbounded_observation_channel_never_sheds<R>()
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let node = Serf::<SmolStr, SocketAddr, R>::tcp(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("unb-a"))
      .with_advertise_addr(MaybeResolved::Resolved(bind)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new().with_observation_channel(serf_reactor::Channel::Unbounded),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn unb-a");

  let mut events = node.events();
  let payload = Bytes::from(vec![0x11u8; 480]);
  for _ in 0..128u32 {
    node
      .user_event("flood", payload.clone(), false)
      .await
      .expect("user event dispatched");
  }

  // Every event still arrives, and nothing was shed at the observation channel.
  let mut seen = 0usize;
  R::timeout(Duration::from_secs(20), async {
    loop {
      match events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "flood" => {
          seen += 1;
          if seen == 128 {
            break;
          }
        }
        Some(_) => {}
        None => break,
      }
    }
  })
  .await
  .expect("an unbounded observation channel delivers every event");
  assert_eq!(seen, 128, "every flooded event reached the subscriber");
  assert_eq!(
    node.observation_dropped(),
    0,
    "an unbounded observation channel has no backstop to shed against"
  );

  node.shutdown().await.expect("unb-a shuts down");
}

/// A shutdown racing an IN-FLIGHT await-result join resolves that join
/// `Err(Shutdown)` — never leaves it parked forever. The seed accepts the TCP
/// connection but never answers, so the exchange is still pending inside the
/// driver when the teardown reaps the waiter.
async fn shutdown_racing_an_inflight_join_resolves_it<R>()
where
  R: Runtime,
{
  // A seed that accepts and then goes silent: the join's push/pull never completes.
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the silent seed");
  let seed_addr = listener.local_addr().expect("the silent seed's address");
  let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
  let silent = std::thread::spawn(move || {
    let held = listener.accept();
    // Hold the accepted connection open (no reply) until the test releases us.
    // Ignoring Err: a disconnected sender means the test finished.
    let _ = stop_rx.recv();
    drop(held);
  });

  let node = spawn_node::<R>("race-join").await;

  // The join parks on the silent seed; the shutdown races it.
  let (join, shutdown) = future::join(
    node.join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      false,
    ),
    async {
      // Let the dial connect and the request go out before the teardown begins, so
      // the waiter is genuinely in flight rather than never dispatched.
      R::sleep(Duration::from_millis(200)).await;
      node.shutdown().await
    },
  )
  .await;

  shutdown.expect("the node shuts down");
  let err = join.expect_err("a join racing a shutdown cannot succeed against a silent seed");
  assert!(
    matches!(
      err,
      serf_reactor::SerfError::Shutdown | serf_reactor::SerfError::JoinAllFailed(_)
    ),
    "the in-flight join must be resolved by the teardown, not stranded (got {err:?})"
  );

  // Ignoring Err: the silent-seed thread may already have exited.
  let _ = stop_tx.send(());
  silent.join().expect("the silent seed thread exits");
}

/// A second `shutdown()` — issued once the driver has already exited and closed
/// its command queue — still resolves `Ok`, and only AFTER the bind address is
/// actually free: the late caller parks on the teardown-completion latch instead
/// of returning into a still-bound port. The freed port is proven rebindable
/// immediately after.
async fn second_shutdown_awaits_teardown_completion<R>()
where
  R: Runtime,
{
  let node = spawn_node::<R>("twice-a").await;
  let addr = node.advertise_address();

  node.shutdown().await.expect("the first shutdown resolves");
  node
    .shutdown()
    .await
    .expect("a second shutdown after teardown still resolves Ok");

  // Every command path fails fast once the queue is closed, rather than hanging.
  let err = node
    .user_event("post", Bytes::from_static(b"x"), false)
    .await
    .expect_err("a shut-down node accepts no commands");
  assert!(
    matches!(err, serf_reactor::SerfError::Shutdown),
    "a post-shutdown command reports Shutdown, got {err:?}"
  );

  // The latch fired only once the bind address was free: rebinding it succeeds.
  let reborn = spawn_node_with_snapshot::<R>(
    "twice-b",
    addr,
    serf_reactor::SnapshotOptions::new(snapshot_path::<R>("twice")),
    false,
  )
  .await
  .expect("the freed address rebinds after the awaited teardown");
  assert_eq!(reborn.advertise_address(), addr);
  reborn.shutdown().await.expect("twice-b shuts down");
}

/// A key-management request on a node with NO keyring is REFUSED, not silently
/// applied: the responder answers `result = false` with an explanatory message and
/// leaves the wire untouched, so the originator's collected response carries the
/// error rather than a false success.
#[cfg(encryption)]
async fn key_op_without_a_keyring_is_refused<R>()
where
  R: Runtime,
{
  // Two PLAINTEXT nodes — neither carries a keyring.
  let b = spawn_node::<R>("nokey-b").await;
  let a = spawn_node::<R>("nokey-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  a.install_key(test_secret_key(0x77))
    .await
    .expect("the request itself dispatches");

  let kr = R::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("the event stream ended before the key response"),
      }
    }
  })
  .await
  .expect("A collects the key response within the query window");

  assert!(
    kr.num_err > 0,
    "a node with no keyring must REFUSE the key op, not report success (num_err={}, num_resp={})",
    kr.num_err,
    kr.num_resp
  );
  assert!(
    !kr.messages.is_empty(),
    "the refusal carries an explanatory message"
  );
  assert!(
    !a.encryption_enabled() && !b.encryption_enabled(),
    "the refused op left both nodes plaintext"
  );

  a.shutdown().await.expect("nokey-a shuts down");
  b.shutdown().await.expect("nokey-b shuts down");
}

/// Past its threshold the snapshot file is COMPACTED to the live state rather than
/// growing without bound: after a churn of membership events the file stays small
/// and still replays the live cluster — a restarted node recovers its peer from the
/// compacted file.
async fn snapshot_compaction_rewrites_the_live_state<R>()
where
  R: Runtime,
{
  let path = snapshot_path::<R>("compact");
  let a = spawn_node::<R>("cmp-a").await;
  let a_addr = a.advertise_address();

  // A tiny threshold: any membership churn crosses it and forces a rewrite.
  let b = spawn_node_with_snapshot::<R>(
    "cmp-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path).with_compact_threshold(1),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Churn: repeated tag updates append member records the compaction must fold.
  for i in 0..8u32 {
    let mut tags = serf_proto::Tags::new();
    tags
      .0
      .insert(SmolStr::new("gen"), SmolStr::new(i.to_string()));
    a.set_tags(tags).await.expect("A re-tags itself");
    R::sleep(Duration::from_millis(50)).await;
  }
  R::sleep(Duration::from_millis(300)).await;

  // The compacted file holds the LIVE alive-set, not the whole append history: a
  // fold of two members plus their clock floors stays far below the churn's
  // un-compacted footprint.
  let len = std::fs::metadata(&path).expect("the snapshot exists").len();
  assert!(
    len < 4096,
    "the compaction must rewrite the file to the live state, but it grew to {len} bytes"
  );

  b.shutdown().await.expect("cmp-b shuts down");

  // The compacted file still replays: a restarted node recovers its peer from it.
  let b2 = spawn_node_with_snapshot::<R>(
    "cmp-b",
    ephemeral_bind(),
    serf_reactor::SnapshotOptions::new(&path).with_compact_threshold(1),
    false,
  )
  .await
  .expect("spawn snapshot-backed serf tcp node");
  converge(&a, &b2).await;
  assert_eq!(
    b2.num_members(),
    2,
    "the compacted snapshot still recovers the live membership"
  );

  a.shutdown().await.expect("cmp-a shuts down");
  b2.shutdown().await.expect("cmp-b2 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Build a plain-TCP node from `opts` through the ergonomic constructor.
async fn build_tcp<R>(opts: TcpTransportOptions<SmolStr, SocketAddr>) -> Result<Node<R>, SerfError>
where
  R: Runtime,
{
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
}

/// The TCP transport's construction gate: each field `TcpTransport::new` requires
/// is refused when absent, so a half-built options block can never bind a socket.
async fn construction_requires_id_and_advertise_addr<R>()
where
  R: Runtime,
{
  let err = build_tcp::<R>(
    TcpTransportOptions::new().with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind())),
  )
  .await
  .err()
  .expect("a node with no id cannot be built");
  assert!(
    matches!(err, SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "a missing local_id is an InvalidInput, got {err:?}"
  );

  let err = build_tcp::<R>(TcpTransportOptions::new().with_local_id(SmolStr::new("no-addr")))
    .await
    .err()
    .expect("a node with no advertise address cannot be built");
  assert!(
    matches!(err, SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "a missing advertise_addr is an InvalidInput, got {err:?}"
  );
}

/// A wildcard advertise address is refused AFTER the bind: the readback keeps the
/// unspecified IP, which peers could not route back to, so construction fails and
/// BOTH bound sockets are released rather than the node joining as an undialable
/// member. The released port is proven free by an immediate successful rebind of
/// the very port the failed attempt had claimed.
async fn wildcard_advertise_is_refused_and_releases_the_bind<R>()
where
  R: Runtime,
{
  // Claim a concrete port through a successful node, then free it, so the wildcard
  // attempt below binds a KNOWN port we can prove was released.
  let probe = spawn_node::<R>("wild-probe").await;
  let port = probe.advertise_address().port();
  probe.shutdown().await.expect("probe shuts down");

  let wildcard: SocketAddr = format!("0.0.0.0:{port}").parse().expect("wildcard addr");
  let err = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("wild"))
      .with_advertise_addr(MaybeResolved::Resolved(wildcard)),
  )
  .await
  .err()
  .expect("a wildcard advertise address is not a routable contact");
  assert!(
    matches!(err, SerfError::InvalidAdvertiseAddr(_)),
    "a wildcard bind must be refused as an invalid advertise address, got {err:?}"
  );

  // The refused construction released BOTH bound sockets: the same port rebinds.
  let after = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("wild-after"))
      .with_advertise_addr(MaybeResolved::Resolved(
        format!("127.0.0.1:{port}").parse().expect("loopback addr"),
      )),
  )
  .await
  .expect("the refused construction released the ports it had bound");
  assert_eq!(after.advertise_address().port(), port);
  after.shutdown().await.expect("wild-after shuts down");
}

/// An UNRESOLVED advertise address is resolved at construction through the
/// caller's resolvers, and the node comes up on the resolved contact.
async fn unresolved_advertise_addr_is_resolved_at_construction<R>()
where
  R: Runtime,
{
  let node = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("resolve-me"))
      .with_advertise_addr(MaybeResolved::Unresolved(ephemeral_bind())),
  )
  .await
  .expect("the unresolved advertise address resolves through the supplied resolver");

  let bound = node.advertise_address();
  assert!(bound.ip().is_loopback(), "the resolved contact is loopback");
  assert_ne!(
    bound.port(),
    0,
    "the ephemeral bind resolved to a concrete port"
  );
  node.shutdown().await.expect("resolve-me shuts down");
}

/// A resolver that FAILS, and one that resolves to NO candidate, both fail
/// construction rather than booting an addressless node: an advertise outage must
/// be loud, never a node that gossips a contact nobody can dial.
async fn advertise_resolution_failure_fails_construction<R>()
where
  R: Runtime,
{
  /// Resolves nothing — a bootstrap outage the advertise picker must refuse.
  struct EmptyResolver;
  impl serf_reactor::Resolver for EmptyResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;
    fn resolve(
      &self,
      _addr: &SocketAddr,
    ) -> impl core::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + '_ {
      // The candidate set is empty whatever the input, so the future borrows nothing.
      let out = Vec::new();
      async move { Ok(out) }
    }
  }

  /// Fails outright — the DNS-down shape.
  struct FailingResolver;
  impl serf_reactor::Resolver for FailingResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;
    fn resolve(
      &self,
      _addr: &SocketAddr,
    ) -> impl core::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + '_ {
      // The failure is unconditional, so the future borrows nothing.
      let err = std::io::Error::other("resolver is down");
      async move { Err(err) }
    }
  }

  async fn build_with<R, RES>(resolver: &RES) -> Result<Node<R>, SerfError>
  where
    R: Runtime,
    RES: serf_reactor::Resolver<Address = SocketAddr>,
  {
    Serf::<SmolStr, SocketAddr, R>::tcp(
      TcpTransportOptions::new()
        .with_local_id(SmolStr::new("unresolvable"))
        .with_advertise_addr(MaybeResolved::Unresolved(ephemeral_bind())),
      resolver,
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
  }

  let err = build_with::<R, _>(&EmptyResolver)
    .await
    .err()
    .expect("an advertise address that resolves to nothing cannot boot a node");
  assert!(
    matches!(err, SerfError::Resolve(_)),
    "an empty candidate set is a resolution failure, got {err:?}"
  );

  let err = build_with::<R, _>(&FailingResolver)
    .await
    .err()
    .expect("a failing resolver cannot boot a node");
  assert!(
    matches!(err, SerfError::Resolve(_)),
    "a resolver error is a resolution failure, got {err:?}"
  );
}

/// A node needs BOTH planes: the reliable TCP listener and the gossip UDP socket
/// on the same port. When the gossip port is already taken, construction FAILS
/// rather than coming up with a reliable plane and no gossip — a node that could
/// merge membership but never probe, gossip, or be detected as failed.
async fn taken_gossip_port_fails_construction<R>()
where
  R: Runtime,
{
  // Take a concrete UDP port, leaving the same TCP port free.
  let squatter = std::net::UdpSocket::bind("127.0.0.1:0").expect("squat a UDP port");
  let taken = squatter.local_addr().expect("the squatted address");

  let err = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("squatted"))
      .with_advertise_addr(MaybeResolved::Resolved(taken)),
  )
  .await
  .err()
  .expect("a node cannot come up without its gossip plane");
  assert!(
    matches!(err, SerfError::Io(_)),
    "a taken gossip port is an I/O failure, got {err:?}"
  );

  // Freeing the port makes the very same construction succeed — the failure was
  // the squatter, not the address.
  drop(squatter);
  let node = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("unsquatted"))
      .with_advertise_addr(MaybeResolved::Resolved(taken)),
  )
  .await
  .expect("the released gossip port lets the node bind");
  assert_eq!(node.advertise_address(), taken);
  node.shutdown().await.expect("unsquatted shuts down");
}

/// The handle's operator readouts are all live and consistent on a healthy joined
/// node: nothing has been shed at any of the four drop counters, the awareness
/// score is healthy, and the QUIC datagram counter stays at zero on a stream
/// transport (it counts only datagram-plane gossip, which plain TCP never sends).
async fn handle_readouts_are_quiet_on_a_healthy_node<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("obs-b").await;
  let a = spawn_node::<R>("obs-a").await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  // A subscriber that keeps up, a delegate that never parks, and no coalescing
  // pressure: every shed counter must be zero.
  assert_eq!(a.events_dropped(), 0, "no subscriber was starved");
  assert_eq!(a.observation_dropped(), 0, "no delegate stalled the pump");
  assert_eq!(
    a.coalesced_user_events_dropped(),
    0,
    "no user event was shed by the coalescer"
  );
  assert_eq!(
    a.coalesced_member_events_dropped(),
    0,
    "no member change was shed by the coalescer"
  );
  assert_eq!(
    a.datagrams_sent(),
    0,
    "a stream transport never rides the QUIC datagram plane"
  );
  assert_eq!(a.health_score(), 0, "a healthy node scores 0");
  assert_eq!(
    a.health_score(),
    a.stats().health_score(),
    "the standalone readout and the aggregate agree"
  );

  a.shutdown().await.expect("obs-a shuts down");
  b.shutdown().await.expect("obs-b shuts down");
}

/// The seed-resolution failure paths are LOUD on every join entry point: a
/// resolver that fails surfaces the error from `join_many` and `dispatch_join`
/// rather than reporting a healthy zero-contact join, and a join issued after
/// shutdown reports `Shutdown` rather than parking on a reply that can never come.
async fn join_entry_points_surface_their_failures<R>()
where
  R: Runtime,
{
  /// Fails outright — the DNS-down shape.
  struct FailingResolver;
  impl serf_reactor::Resolver for FailingResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;
    fn resolve(
      &self,
      _addr: &SocketAddr,
    ) -> impl core::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + '_ {
      // The failure is unconditional, so the future borrows nothing.
      let err = std::io::Error::other("resolver is down");
      async move { Err(err) }
    }
  }

  let node = spawn_node::<R>("seed-fail").await;
  // An UNRESOLVED seed is the one the resolver is actually consulted for (an
  // already-resolved seed passes straight through).
  let unresolved = || {
    MaybeResolved::Unresolved(
      "127.0.0.1:7219"
        .parse::<SocketAddr>()
        .expect("loopback addr"),
    )
  };

  // `join_many` propagates the resolver failure with an empty reached set.
  let (reached, err) = node
    .join_many(&FailingResolver, [unresolved()].into_iter(), false)
    .await
    .expect_err("an unresolvable seed set cannot report a healthy join");
  assert!(reached.is_empty(), "no seed was reached");
  assert!(
    matches!(err, SerfError::Resolve(_)),
    "the resolver failure reaches the caller, got {err:?}"
  );

  // `dispatch_join` propagates it too — a fire-and-forget join must not swallow it.
  let err = node
    .dispatch_join(&FailingResolver, &[unresolved()])
    .await
    .expect_err("a fire-and-forget join still surfaces the resolver failure");
  assert!(
    matches!(err, SerfError::Resolve(_)),
    "the resolver failure reaches the caller, got {err:?}"
  );

  // A dispatch_join on a LEFT node is refused, not silently dispatched.
  node.leave().await.expect("the node leaves the cluster");
  let err = node
    .dispatch_join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(
        "127.0.0.1:7219".parse().expect("loopback addr"),
      )],
    )
    .await
    .expect_err("a left node cannot dispatch a join");
  assert!(
    matches!(err, SerfError::NotRunning),
    "a post-leave dispatch_join reports NotRunning, got {err:?}"
  );

  // After shutdown the command never even reaches a driver: the send fails fast.
  node.shutdown().await.expect("seed-fail shuts down");
  let err = node
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved("127.0.0.1:7219".parse().expect("loopback addr")),
      false,
    )
    .await
    .expect_err("a shut-down node cannot join");
  assert!(
    matches!(err, SerfError::Shutdown),
    "a post-shutdown join reports Shutdown, got {err:?}"
  );
}

/// A burst of concurrent operations racing a shutdown must all RESOLVE — with a
/// success, a `NotRunning`, or a `Shutdown` — and never strand a caller on a reply
/// that can never come. Whichever commands the teardown finds still queued are
/// failed by it rather than dropped, and the node still frees its bind address.
async fn concurrent_commands_racing_shutdown_all_resolve<R>()
where
  R: Runtime,
{
  let node = spawn_node::<R>("storm-a").await;
  let addr = node.advertise_address();

  // 64 command-issuing tasks against one shutdown: whichever land in the queue as
  // the teardown closes it must be failed by the teardown, not stranded.
  let mut tasks = Vec::new();
  for i in 0..64u32 {
    let node = node.clone();
    tasks.push(R::spawn(async move {
      let mut outcomes = Vec::new();
      for _ in 0..8 {
        outcomes.push(
          node
            .user_event("storm", Bytes::from(i.to_be_bytes().to_vec()), false)
            .await,
        );
        outcomes.push(node.set_tags(serf_proto::Tags::new()).await);
      }
      outcomes
    }));
  }

  let shutdown = node.shutdown().await;
  shutdown.expect("the node shuts down under the command storm");

  for t in tasks {
    for outcome in t.await.expect("no command task panics or is stranded") {
      if let Err(e) = outcome {
        assert!(
          matches!(
            e,
            SerfError::Shutdown | SerfError::NotRunning | SerfError::CommandSend
          ),
          "a command racing the shutdown must resolve with a terminal reason, got {e:?}"
        );
      }
    }
  }

  // The teardown still completed: the bind address is free.
  let reborn = build_tcp::<R>(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("storm-b"))
      .with_advertise_addr(MaybeResolved::Resolved(addr)),
  )
  .await
  .expect("the storm did not wedge the teardown; the address is free");
  reborn.shutdown().await.expect("storm-b shuts down");
}

/// Every SWIM override the transport options carry is threaded into the
/// coordinator the driver builds. Two nodes configured with the FULL override set
/// — including the reclaim window and the suspicion ceiling that no other scenario
/// sets — still join, converge, and detect an abrupt kill, so no override is
/// dropped or mis-wired on the way through `Transport::run`.
async fn full_swim_override_set_is_threaded_into_the_coordinator<R>()
where
  R: Runtime,
{
  async fn spawn_tuned<R>(id: &str) -> Node<R>
  where
    R: Runtime,
  {
    build_tcp::<R>(
      TcpTransportOptions::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
        .with_probe_interval(Duration::from_millis(100))
        .with_probe_timeout(Duration::from_millis(50))
        .with_gossip_interval(Duration::from_millis(20))
        .with_suspicion_mult(3)
        .with_suspicion_max_timeout_mult(4)
        .with_dead_node_reclaim_time(Duration::from_millis(1))
        .with_push_pull_interval(Duration::from_millis(500))
        .with_stream(
          serf_reactor::StreamTransportOptions::new()
            .with_dial_timeout(Duration::from_secs(5))
            .with_close_timeout(Duration::from_secs(5)),
        ),
    )
    .await
    .expect("spawn a fully-tuned serf tcp node")
  }

  let b = spawn_tuned::<R>("tuned-b").await;
  let a = spawn_tuned::<R>("tuned-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("tuned-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("the fully-tuned nodes still join");
  converge(&a, &b).await;

  // The tuned failure detection still fires: an abrupt kill is detected Failed.
  b.shutdown().await.expect("tuned-b shuts down abruptly");
  R::timeout(Duration::from_secs(20), async {
    loop {
      let failed = a
        .members()
        .iter()
        .any(|m| m.node().id_ref() == &b_id && m.status() != MemberStatus::Alive);
      if failed {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the tuned SWIM overrides still detect the killed peer");

  a.shutdown().await.expect("tuned-a shuts down");
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
  async fn construction_requires_id_and_advertise_addr() {
    super::construction_requires_id_and_advertise_addr::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn wildcard_advertise_is_refused_and_releases_the_bind() {
    super::wildcard_advertise_is_refused_and_releases_the_bind::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn unresolved_advertise_addr_is_resolved_at_construction() {
    super::unresolved_advertise_addr_is_resolved_at_construction::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn advertise_resolution_failure_fails_construction() {
    super::advertise_resolution_failure_fails_construction::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn taken_gossip_port_fails_construction() {
    super::taken_gossip_port_fails_construction::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn full_swim_override_set_is_threaded_into_the_coordinator() {
    super::full_swim_override_set_is_threaded_into_the_coordinator::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn handle_readouts_are_quiet_on_a_healthy_node() {
    super::handle_readouts_are_quiet_on_a_healthy_node::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn join_entry_points_surface_their_failures() {
    super::join_entry_points_surface_their_failures::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn concurrent_commands_racing_shutdown_all_resolve() {
    super::concurrent_commands_racing_shutdown_all_resolve::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn post_leave_operations_report_not_running() {
    super::post_leave_operations_report_not_running::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn post_leave_key_operations_report_not_running() {
    super::post_leave_key_operations_report_not_running::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn concurrent_leaves_share_one_in_flight_leave() {
    super::concurrent_leaves_share_one_in_flight_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn ignore_old_join_suppresses_the_replay() {
    super::ignore_old_join_suppresses_the_replay::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn malformed_gossip_datagram_is_ignored() {
    super::malformed_gossip_datagram_is_ignored::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn unauthenticatable_gossip_datagram_is_ignored() {
    super::unauthenticatable_gossip_datagram_is_ignored::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn left_node_refuses_new_inbound_exchanges() {
    super::left_node_refuses_new_inbound_exchanges::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn peer_reset_mid_exchange_fails_the_join() {
    super::peer_reset_mid_exchange_fails_the_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn slow_subscriber_sheds_events_and_counts_them() {
    super::slow_subscriber_sheds_events_and_counts_them::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn stalling_delegate_sheds_observations_and_counts_them() {
    super::stalling_delegate_sheds_observations_and_counts_them::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn unbounded_observation_channel_never_sheds() {
    super::unbounded_observation_channel_never_sheds::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn shutdown_racing_an_inflight_join_resolves_it() {
    super::shutdown_racing_an_inflight_join_resolves_it::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn second_shutdown_awaits_teardown_completion() {
    super::second_shutdown_awaits_teardown_completion::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn key_op_without_a_keyring_is_refused() {
    super::key_op_without_a_keyring_is_refused::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_compaction_rewrites_the_live_state() {
    super::snapshot_compaction_rewrites_the_live_state::<TokioRuntime>().await;
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

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_join_leave() {
    super::serf_join_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_join_leave_join() {
    super::serf_join_leave_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_leave_rejoin_different_role() {
    super::serf_leave_rejoin_different_role::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_per_node_reconnect_timeout() {
    super::serf_per_node_reconnect_timeout::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_snapshot_recovery() {
    super::serf_snapshot_recovery::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_name_resolution() {
    super::serf_name_resolution::<TokioRuntime>().await;
  }

  #[cfg(feature = "test")]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn message_dropper_drops_inbound_joins() {
    super::message_dropper_drops_inbound_joins::<TokioRuntime>().await;
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

  #[test]
  fn serf_join_leave_smol() {
    SmolRuntime::block_on(super::serf_join_leave::<SmolRuntime>());
  }

  #[test]
  fn serf_join_leave_join_smol() {
    SmolRuntime::block_on(super::serf_join_leave_join::<SmolRuntime>());
  }

  #[test]
  fn serf_leave_rejoin_different_role_smol() {
    SmolRuntime::block_on(super::serf_leave_rejoin_different_role::<SmolRuntime>());
  }

  #[test]
  fn serf_per_node_reconnect_timeout_smol() {
    SmolRuntime::block_on(super::serf_per_node_reconnect_timeout::<SmolRuntime>());
  }

  #[test]
  fn serf_snapshot_recovery_smol() {
    SmolRuntime::block_on(super::serf_snapshot_recovery::<SmolRuntime>());
  }

  #[test]
  fn serf_name_resolution_smol() {
    SmolRuntime::block_on(super::serf_name_resolution::<SmolRuntime>());
  }

  #[cfg(feature = "test")]
  #[test]
  fn message_dropper_drops_inbound_joins_smol() {
    SmolRuntime::block_on(super::message_dropper_drops_inbound_joins::<SmolRuntime>());
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

  #[test]
  fn post_leave_operations_report_not_running_smol() {
    SmolRuntime::block_on(super::post_leave_operations_report_not_running::<SmolRuntime>());
  }

  #[cfg(encryption)]
  #[test]
  fn post_leave_key_operations_report_not_running_smol() {
    SmolRuntime::block_on(super::post_leave_key_operations_report_not_running::<
      SmolRuntime,
    >());
  }

  #[test]
  fn concurrent_leaves_share_one_in_flight_leave_smol() {
    SmolRuntime::block_on(super::concurrent_leaves_share_one_in_flight_leave::<
      SmolRuntime,
    >());
  }

  #[test]
  fn ignore_old_join_suppresses_the_replay_smol() {
    SmolRuntime::block_on(super::ignore_old_join_suppresses_the_replay::<SmolRuntime>());
  }

  #[test]
  fn malformed_gossip_datagram_is_ignored_smol() {
    SmolRuntime::block_on(super::malformed_gossip_datagram_is_ignored::<SmolRuntime>());
  }

  #[cfg(encryption)]
  #[test]
  fn unauthenticatable_gossip_datagram_is_ignored_smol() {
    SmolRuntime::block_on(super::unauthenticatable_gossip_datagram_is_ignored::<
      SmolRuntime,
    >());
  }

  #[test]
  fn left_node_refuses_new_inbound_exchanges_smol() {
    SmolRuntime::block_on(super::left_node_refuses_new_inbound_exchanges::<SmolRuntime>());
  }

  #[test]
  fn peer_reset_mid_exchange_fails_the_join_smol() {
    SmolRuntime::block_on(super::peer_reset_mid_exchange_fails_the_join::<SmolRuntime>());
  }

  #[test]
  fn slow_subscriber_sheds_events_and_counts_them_smol() {
    SmolRuntime::block_on(super::slow_subscriber_sheds_events_and_counts_them::<
      SmolRuntime,
    >());
  }

  #[test]
  fn stalling_delegate_sheds_observations_and_counts_them_smol() {
    SmolRuntime::block_on(
      super::stalling_delegate_sheds_observations_and_counts_them::<SmolRuntime>(),
    );
  }

  #[test]
  fn unbounded_observation_channel_never_sheds_smol() {
    SmolRuntime::block_on(super::unbounded_observation_channel_never_sheds::<
      SmolRuntime,
    >());
  }

  #[test]
  fn shutdown_racing_an_inflight_join_resolves_it_smol() {
    SmolRuntime::block_on(super::shutdown_racing_an_inflight_join_resolves_it::<
      SmolRuntime,
    >());
  }

  #[test]
  fn second_shutdown_awaits_teardown_completion_smol() {
    SmolRuntime::block_on(super::second_shutdown_awaits_teardown_completion::<
      SmolRuntime,
    >());
  }

  #[cfg(encryption)]
  #[test]
  fn key_op_without_a_keyring_is_refused_smol() {
    SmolRuntime::block_on(super::key_op_without_a_keyring_is_refused::<SmolRuntime>());
  }

  #[test]
  fn snapshot_compaction_rewrites_the_live_state_smol() {
    SmolRuntime::block_on(super::snapshot_compaction_rewrites_the_live_state::<
      SmolRuntime,
    >());
  }

  #[test]
  fn construction_requires_id_and_advertise_addr_smol() {
    SmolRuntime::block_on(super::construction_requires_id_and_advertise_addr::<
      SmolRuntime,
    >());
  }

  #[test]
  fn wildcard_advertise_is_refused_and_releases_the_bind_smol() {
    SmolRuntime::block_on(
      super::wildcard_advertise_is_refused_and_releases_the_bind::<SmolRuntime>(),
    );
  }

  #[test]
  fn unresolved_advertise_addr_is_resolved_at_construction_smol() {
    SmolRuntime::block_on(
      super::unresolved_advertise_addr_is_resolved_at_construction::<SmolRuntime>(),
    );
  }

  #[test]
  fn advertise_resolution_failure_fails_construction_smol() {
    SmolRuntime::block_on(super::advertise_resolution_failure_fails_construction::<
      SmolRuntime,
    >());
  }

  #[test]
  fn taken_gossip_port_fails_construction_smol() {
    SmolRuntime::block_on(super::taken_gossip_port_fails_construction::<SmolRuntime>());
  }

  #[test]
  fn full_swim_override_set_is_threaded_into_the_coordinator_smol() {
    SmolRuntime::block_on(
      super::full_swim_override_set_is_threaded_into_the_coordinator::<SmolRuntime>(),
    );
  }

  #[test]
  fn handle_readouts_are_quiet_on_a_healthy_node_smol() {
    SmolRuntime::block_on(super::handle_readouts_are_quiet_on_a_healthy_node::<
      SmolRuntime,
    >());
  }

  #[test]
  fn join_entry_points_surface_their_failures_smol() {
    SmolRuntime::block_on(super::join_entry_points_surface_their_failures::<SmolRuntime>());
  }

  #[test]
  fn concurrent_commands_racing_shutdown_all_resolve_smol() {
    SmolRuntime::block_on(super::concurrent_commands_racing_shutdown_all_resolve::<
      SmolRuntime,
    >());
  }
}
