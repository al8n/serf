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
