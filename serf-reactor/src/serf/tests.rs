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
};
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
    None,
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
    None,
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

  // Each `user_event().await` returned only after the pump processed that command
  // and incremented the shared shed counter, so the handle read is already current
  // with no publish step or extra wake.
  let dropped = a.coalesced_user_events_dropped();
  a.shutdown().await.expect("coalesce-a shuts down");

  assert_eq!(
    dropped,
    u64::from(n) - cap.get() as u64,
    "every distinct-named cc event past the cap is counted on the public handle (got {dropped})"
  );
}

/// On a multi-threaded runtime a `Serf` clone read on a DIFFERENT worker than the
/// pump observes a coalescer shed the instant the `user_event` reply resolves: the
/// handle's reader and the endpoint's writer share one atomic, so there is no
/// publish step for the read to lag behind. Repeated so a regression that
/// reintroduced a copied-out mirror would be reliably caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_drop_visible_cross_thread_after_reply() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let n: u32 = 20;
  for trial in 0..24 {
    let serf_opts = SerfOptions::new()
      .with_user_coalesce_period(Duration::from_secs(10))
      .with_user_quiescent_period(Duration::from_secs(2))
      .with_max_coalesced_user_events(Some(cap));
    let a = spawn_node_with_serf_options(&format!("coalesce-xthread-{trial}"), serf_opts).await;

    // Issue distinct-named coalescing events past the cap; await each reply so the
    // pump has processed and shed it.
    for i in 0..n {
      a.user_event(format!("evt-{i}"), bytes::Bytes::new(), true)
        .await
        .expect("user event dispatched");
    }

    // Read the shed count from a CLONE on a spawned task — a different worker than
    // the pump — immediately after the replies, with no sleep and no publish.
    let b = a.clone();
    let observed = tokio::spawn(async move { b.coalesced_user_events_dropped() })
      .await
      .expect("read task joins");

    a.shutdown().await.expect("coalesce-xthread shuts down");
    assert_eq!(
      observed,
      u64::from(n) - cap.get() as u64,
      "a clone on another worker observes the shed count with no publish step"
    );
  }
}

/// After exactly one shed through the public `user_event` path the handle getter
/// returns at least one. A construction typo that wired the handle's reader to a
/// different atomic than the endpoint's writer would leave this a permanent zero,
/// so the aliasing bug fails loudly here rather than silently reporting no drops.
#[tokio::test]
async fn coalesced_drop_aliasing_guard() {
  // A cap of one: the second distinct-named coalescing event is shed.
  let cap = core::num::NonZeroUsize::new(1).unwrap();
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let a = spawn_node_with_serf_options("coalesce-alias", serf_opts).await;

  a.user_event("first".to_string(), bytes::Bytes::new(), true)
    .await
    .expect("first user event dispatched");
  a.user_event("second".to_string(), bytes::Bytes::new(), true)
    .await
    .expect("second user event dispatched");

  assert!(
    a.coalesced_user_events_dropped() >= 1,
    "the handle observes the endpoint's shed; a mis-wired reader would read a permanent 0"
  );
  a.shutdown().await.expect("coalesce-alias shuts down");
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
    None,
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

/// An over-ceiling `max_user_event_size` in the serf options is rejected by
/// `Serf::tcp` at construction — before binding a socket or spawning the detached
/// driver — rather than returning `Ok` and later dropping oversize user events.
#[tokio::test]
async fn tcp_new_rejects_over_ceiling_user_event_size() {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-serf-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  let serf = SerfOptions::new().with_max_user_event_size(SerfOptions::USER_EVENT_SIZE_LIMIT + 1);
  let res = Serf::<SmolStr, SocketAddr, TokioRuntime>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    serf,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(crate::VoidKeyringDelegate),
  )
  .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("an over-ceiling max_user_event_size must be rejected at construction"),
  }
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
