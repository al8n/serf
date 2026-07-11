//! Handle-level unit tests for the reactor `Serf` on tokio: the ergonomic
//! `Serf::tcp` constructor's option validation, single-node shutdown/rebind, and
//! the fast join-failure paths. The multi-node convergence / event / query /
//! leave behavior is covered by the real-node suite in `tests/tcp.rs`.

use core::{future::Future, time::Duration};
use std::net::SocketAddr;

use agnostic::tokio::TokioRuntime;

use crate::{
  Channel, FirstAddrResolver, MaybeResolved, Resolver, RuntimeOptions, Serf, SerfError,
  SocketAddrResolver, TcpTransportOptions, VoidDelegate,
  command::{Command, UserEventCmd},
};
use futures_channel::oneshot;
use serf_proto::options::Options as SerfOptions;
use smol_str::SmolStr;

/// A tokio-backed reactor TCP node handle.
type Node = Serf<SmolStr, SocketAddr, TokioRuntime>;

/// A loopback address with a port nothing listens on — `connect()` returns
/// `ECONNREFUSED` immediately, so its push/pull exchange fails fast. The port is
/// below the OS ephemeral range, so a `:0` test bind never collides with it.
fn blackhole_addr() -> SocketAddr {
  "127.0.0.1:7217".parse().expect("loopback addr")
}

/// Resolver that always resolves to an empty address list — models a
/// service-discovery resolver that finds no live endpoints under a service key.
struct EmptyResolver;

impl Resolver for EmptyResolver {
  type Address = String;
  type Error = std::io::Error;

  // The trait's `resolve` future is bound `+ '_` to the `&self` lifetime; an
  // `async fn` would also capture the (unused) `&addr` lifetime and fail to
  // satisfy it, so the future is written explicitly, borrowing neither argument.
  #[allow(clippy::manual_async_fn)]
  fn resolve(
    &self,
    _addr: &String,
  ) -> impl Future<Output = Result<Vec<SocketAddr>, std::io::Error>> + Send + '_ {
    async move { Ok(Vec::new()) }
  }
}

/// Build a reactor TCP node bound to a specific advertise address, returning the
/// construction result so the same-address rebind regression can assert a freed
/// port accepts an immediate rebind.
async fn try_spawn_node_at(id: &str, bind: SocketAddr) -> Result<Node, SerfError> {
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
    std::sync::Arc::new(crate::VoidKeyringDelegate),
  )
  .await
}

/// Build and spawn a reactor TCP node bound to an ephemeral loopback port.
async fn spawn_node(id: &str) -> Node {
  try_spawn_node_at(id, "127.0.0.1:0".parse().expect("loopback addr"))
    .await
    .expect("spawn serf node")
}

/// Build and spawn a reactor TCP node with a custom `SerfOptions` (runtime options
/// at defaults).
async fn spawn_node_with_serf_options(id: &str, serf_options: SerfOptions) -> Node {
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
    serf_options,
    #[cfg(encryption)]
    std::sync::Arc::new(crate::VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// A single node with user coalescing enabled and a small buffered-volume cap sheds
/// every distinct-named coalescing user event issued past the cap through the public
/// `user_event` command path, and the cumulative drop count surfaces on the public
/// `coalesced_user_events_dropped` accessor — the endpoint counter is otherwise
/// unreachable once the driver moves the endpoint into the detached pump.
#[tokio::test]
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
    a.user_event(format!("evt-{i}"), bytes::Bytes::new(), true)
      .await
      .expect("user event dispatched");
  }

  // The pump republishes the endpoint's cumulative counter each poll; give it a beat
  // to run one past the final feed before reading.
  tokio::time::sleep(Duration::from_millis(200)).await;

  let dropped = a.coalesced_user_events_dropped();
  a.shutdown().await.expect("coalesce-a shuts down");

  assert_eq!(
    dropped,
    u64::from(n) - cap.get() as u64,
    "every distinct-named cc event past the cap is counted on the public handle (got {dropped})"
  );
}

/// The pump republishes the cumulative coalescer drop counters at the end of every
/// poll pass, but the shutdown branch returns `Poll::Ready` before that publish, so
/// the drops shed in the same command drain as the `Shutdown` would be lost from the
/// public total. The teardown publish — run as the driver future completes — makes
/// the total exact even for drops shed on the way out; without it the handle keeps
/// the stale value the last normal poll left.
///
/// The flood is enqueued synchronously — no await between the sends — so on the
/// single-threaded cooperative runtime the driver cannot run, and cannot publish,
/// until the test parks on the shutdown reply; the whole flood and the trailing
/// shutdown then drain in one poll, since `drain_commands` takes the entire queue.
#[tokio::test]
async fn coalesced_drops_are_published_on_shutdown_teardown() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let a = spawn_node_with_serf_options("coalesce-teardown", serf_opts).await;

  // Enqueue distinct-named coalescing user events past the cap directly onto the
  // command queue without awaiting each reply: the reply receivers drop immediately,
  // so the driver's acks are discarded, but every event is still fed to the coalescer
  // and the overflow counted. Firing them synchronously guarantees they queue ahead
  // of the shutdown with no intervening driver publish.
  let n: u32 = 20;
  for i in 0..n {
    let (tx, _) = oneshot::channel();
    a.send(Command::UserEvent(UserEventCmd::new(
      SmolStr::new(format!("evt-{i}")),
      bytes::Bytes::new(),
      true,
      tx,
    )))
    .expect("enqueue user event");
  }

  // Shut down with no intervening sleep: the shutdown lands in the same poll's drain
  // as the flood, so the coalescer sheds `n - cap` events and the driver returns
  // `Poll::Ready` from the shutdown branch with those drops unpublished by the normal
  // per-poll store. Awaiting completion runs the teardown publish.
  a.shutdown().await.expect("coalesce-teardown shuts down");

  assert_eq!(
    a.coalesced_user_events_dropped(),
    u64::from(n) - cap.get() as u64,
    "the cumulative drop total shed in the shutdown drain is published on teardown"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_drops_are_visible_immediately_after_shutdown_on_a_multi_thread_runtime() {
  // On a multi-threaded runtime the `shutdown().await` caller can resume on a
  // different worker than the pump. The final counter publish must therefore
  // happen-before the shutdown reply is sent, so an accessor read immediately
  // after `shutdown().await` observes the final total rather than a stale
  // pre-drain value. Repeat to make the ordering violation reliably observable if
  // the publish is ever moved back after the reply.
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let n: u32 = 20;
  for round in 0..24 {
    let serf_opts = SerfOptions::new()
      .with_user_coalesce_period(Duration::from_secs(10))
      .with_user_quiescent_period(Duration::from_secs(2))
      .with_max_coalesced_user_events(Some(cap));
    let a = spawn_node_with_serf_options(&format!("coalesce-mt-{round}"), serf_opts).await;

    for i in 0..n {
      let (tx, _) = oneshot::channel();
      a.send(Command::UserEvent(UserEventCmd::new(
        SmolStr::new(format!("evt-{i}")),
        bytes::Bytes::new(),
        true,
        tx,
      )))
      .expect("enqueue user event");
    }

    a.shutdown().await.expect("coalesce-mt shuts down");

    assert_eq!(
      a.coalesced_user_events_dropped(),
      u64::from(n) - cap.get() as u64,
      "the final drop total is visible to a caller resumed after shutdown completes"
    );
  }
}

/// Build VALID TCP transport options paired with a deliberately invalid
/// `runtime`, and assert `Serf::tcp` rejects it with [`SerfError::InvalidOption`]
/// — before binding a socket or spawning the detached driver — rather than
/// returning `Ok` and later panicking the driver task on a zero-capacity channel.
async fn assert_tcp_new_rejects(runtime: RuntimeOptions) {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  let res = Serf::<SmolStr, SocketAddr, TokioRuntime>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    runtime,
    SerfOptions::new(),
    #[cfg(encryption)]
    std::sync::Arc::new(crate::VoidKeyringDelegate),
  )
  .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero-capacity channel option must be rejected at construction"),
  }
}

/// A `Bounded(0)` observation channel is rejected by `Serf::tcp` instead of
/// panicking the detached driver task.
#[tokio::test]
async fn tcp_new_rejects_zero_observation_channel() {
  assert_tcp_new_rejects(RuntimeOptions::new().with_observation_channel(Channel::Bounded(0))).await;
}

/// A zero `event_queue_cap` is rejected at construction.
#[tokio::test]
async fn tcp_new_rejects_zero_event_queue_cap() {
  assert_tcp_new_rejects(RuntimeOptions::new().with_event_queue_cap(0)).await;
}

/// A `Bounded(0)` observation channel sourced from a serde config is rejected.
#[cfg(feature = "serde")]
#[tokio::test]
async fn tcp_new_rejects_zero_observation_channel_from_serde() {
  let runtime: RuntimeOptions =
    serde_json::from_str(r#"{"observation_channel":{"bounded":0}}"#).expect("deserialize");
  assert_tcp_new_rejects(runtime).await;
}

/// A `bounded:0` observation channel parsed from a clap flag is rejected.
#[cfg(feature = "clap")]
#[tokio::test]
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

/// `shutdown().await` must release the bound TCP listener and UDP gossip socket
/// before it resolves: a second node binding the SAME advertise address the
/// instant the first shuts down must construct successfully, not fail with
/// `AddrInUse`. The driver awaits an explicit release of both sockets before
/// acking the shutdown caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

/// An await-result `join` against an unreachable blackhole seed surfaces
/// `SerfError::JoinAllFailed { requested: 1, contacted: 0 }` once the dial fails
/// fast — well before the join deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
/// `JoinAllFailed`, NOT a silent success. An empty `join_many` input is instead a
/// trivial `Ok(empty)` (no command is sent).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

  let empty: Vec<MaybeResolved<String, SocketAddr>> = Vec::new();
  let reached = a
    .join_many(&EmptyResolver, empty.into_iter(), false)
    .await
    .expect("empty input is a trivial success");
  assert!(reached.is_empty(), "empty input contacts nothing");

  a.shutdown().await.expect("joiner shuts down");
}

/// `default_query_timeout` on a fresh single-member node is a positive duration,
/// and `default_query_param` carries that timeout with no filters / relay / ack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_default_query_param_defaults() {
  let a = spawn_node("dqp-node").await;

  let qt = a.default_query_timeout();
  assert!(
    qt > Duration::ZERO,
    "default_query_timeout must be positive"
  );

  let qp = a.default_query_param();
  assert_eq!(qp.timeout, qt, "default_query_param timeout matches");
  assert!(qp.filters.is_empty(), "no filters");
  assert!(!qp.request_ack, "no ack");
  assert_eq!(qp.relay_factor, 0, "no relay");

  a.shutdown().await.expect("node shuts down");
}
