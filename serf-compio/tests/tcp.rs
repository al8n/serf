//! Real-node TCP serf tests: loopback nodes exercising the compio stream driver
//! end-to-end. Each test spins up ephemeral `127.0.0.1:0` nodes and drives the
//! full pump — join push/pull, coordinator merge, gossip, user events, queries,
//! graceful leave, force-leave, snapshot persistence, and teardown — on the
//! thread-per-core `!Send` compio runtime.
//!
//! The multi-node fault-injection scenarios run through the shared
//! [`cluster`] fixture; the single-purpose driver/handle contracts build their
//! nodes directly so they can vary one knob at a time.

#![cfg(feature = "tcp")]

use core::{future::Future, pin::Pin, time::Duration};
use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use bytes::Bytes;
use futures_util::{StreamExt, future};
use memberlist_proto::MaybeResolved;
use serf_compio::{
  AdvertiseAddrResolver, AdvertiseResolutionError, Channel, Delegate, FirstAddrResolver,
  Ipv4PreferringResolver, Ipv6PreferringResolver, MemberDelegate, OsResolver, QueryDelegate,
  Resolver, RuntimeOptions, Serf, SerfError, SnapshotOptions, SocketAddrResolver,
  StreamTransportOptions, TcpTransport, TcpTransportOptions, Transport, UserEventDelegate,
  VoidDelegate, gossip_rng,
};
use serf_proto::{
  Tags, UserEventMessage,
  event::{Event, MemberEventKind, QueryEvent},
  members::{MemberStatus, SerfState},
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

mod cluster;

/// Bound on every convergence / delivery poll in this file, so a regression
/// surfaces as a timeout rather than a hang.
const WINDOW: Duration = Duration::from_secs(45);

/// A loopback address with a port nothing listens on — `connect()` returns
/// `ECONNREFUSED` immediately. The port is below the OS ephemeral range, so a
/// `:0` test bind never collides with it.
fn blackhole_addr() -> SocketAddr {
  "127.0.0.1:7215".parse().expect("loopback addr")
}

/// A resolver whose every resolution fails — models a service-discovery backend
/// that is down.
struct FailingResolver;

impl Resolver for FailingResolver {
  type Address = String;
  type Error = std::io::Error;

  async fn resolve(&self, _addr: &String) -> Result<Vec<SocketAddr>, std::io::Error> {
    Err(std::io::Error::other("discovery backend unavailable"))
  }
}

/// A resolver that finds no live endpoints under the key it is given — models a
/// service-discovery backend whose service record is empty.
struct EmptyResolver;

impl Resolver for EmptyResolver {
  type Address = String;
  type Error = std::io::Error;

  async fn resolve(&self, _addr: &String) -> Result<Vec<SocketAddr>, std::io::Error> {
    Ok(Vec::new())
  }
}

/// A resolver that answers with a dual-stack candidate set (IPv6 first, then
/// IPv4) on the port it was asked for — enough to drive the
/// `MaybeResolved::Unresolved` advertise path AND the advertise picker's
/// narrowing, without depending on the host's name resolution.
struct DualStackResolver;

impl Resolver for DualStackResolver {
  type Address = SocketAddr;
  type Error = std::io::Error;

  async fn resolve(&self, addr: &SocketAddr) -> Result<Vec<SocketAddr>, std::io::Error> {
    Ok(vec![
      SocketAddr::new("::1".parse().expect("v6 loopback"), addr.port()),
      SocketAddr::new("127.0.0.1".parse().expect("v4 loopback"), addr.port()),
    ])
  }
}

/// Recorded observation-hook fan-out, shared between a [`RecordingDelegate`]
/// handed to the driver and the test that asserts on it.
#[derive(Default)]
struct Observed {
  failed: RefCell<Vec<SmolStr>>,
  updated: RefCell<Vec<SmolStr>>,
  queries: RefCell<Vec<SmolStr>>,
  user_events: RefCell<Vec<SmolStr>>,
}

impl Observed {
  /// Poll `recorded` until it contains `id`, bounded by `window`; returns
  /// whether it landed in time. Each observation hook is delivered on a path
  /// separate from the membership snapshot, so a hook can land a moment after
  /// the snapshot a test has already awaited — poll for it rather than sampling
  /// the hook once.
  async fn recorded_within(recorded: &RefCell<Vec<SmolStr>>, id: &str, window: Duration) -> bool {
    compio::time::timeout(window, async {
      loop {
        let present = recorded.borrow().iter().any(|got| got.as_str() == id);
        if present {
          break;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
      }
    })
    .await
    .is_ok()
  }
}

/// A [`Delegate`] that records which observation hooks the driver fired.
struct RecordingDelegate(Rc<Observed>);

impl MemberDelegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;

  async fn notify_failed(
    &self,
    member: std::sync::Arc<serf_proto::members::Member<SmolStr, SocketAddr>>,
  ) {
    self
      .0
      .failed
      .borrow_mut()
      .push(member.node().id_ref().clone());
  }

  async fn notify_update(
    &self,
    member: std::sync::Arc<serf_proto::members::Member<SmolStr, SocketAddr>>,
  ) {
    self
      .0
      .updated
      .borrow_mut()
      .push(member.node().id_ref().clone());
  }
}

impl UserEventDelegate for RecordingDelegate {
  async fn notify_user_event(&self, event: &UserEventMessage) {
    self.0.user_events.borrow_mut().push(event.name.clone());
  }
}

impl QueryDelegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;

  async fn notify_query(&self, event: &QueryEvent<SmolStr, SocketAddr>) {
    self.0.queries.borrow_mut().push(SmolStr::new(event.name()));
  }
}

impl Delegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

/// A [`Delegate`] whose user-event hook parks for `stall`, so the driver's
/// observation task cannot drain its queue while the pump keeps enqueueing.
struct StallingDelegate {
  stall: Duration,
}

impl MemberDelegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

impl UserEventDelegate for StallingDelegate {
  async fn notify_user_event(&self, _event: &UserEventMessage) {
    compio::time::sleep(self.stall).await;
  }
}

impl QueryDelegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

impl Delegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

/// Every knob a test node may vary, defaulted to the plain loopback node the
/// majority of scenarios want.
struct NodeSpec<D>
where
  D: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
{
  delegate: D,
  runtime: RuntimeOptions,
  serf: SerfOptions,
  snapshot: Option<SnapshotOptions>,
}

impl NodeSpec<VoidDelegate<SmolStr, SocketAddr>> {
  fn new() -> Self {
    Self {
      delegate: VoidDelegate::new(),
      runtime: RuntimeOptions::new(),
      serf: SerfOptions::new(),
      snapshot: None,
    }
  }
}

impl<D> NodeSpec<D>
where
  D: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
{
  fn with_delegate<E>(self, delegate: E) -> NodeSpec<E>
  where
    E: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
  {
    NodeSpec {
      delegate,
      runtime: self.runtime,
      serf: self.serf,
      snapshot: self.snapshot,
    }
  }

  fn with_runtime(mut self, runtime: RuntimeOptions) -> Self {
    self.runtime = runtime;
    self
  }

  fn with_serf(mut self, serf: SerfOptions) -> Self {
    self.serf = serf;
    self
  }

  fn with_snapshot(mut self, snapshot: SnapshotOptions) -> Self {
    self.snapshot = Some(snapshot);
    self
  }

  /// Build and spawn the node on an ephemeral loopback port.
  async fn spawn(self, id: &str) -> Serf<SmolStr, SocketAddr> {
    let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(cluster::loopback_ephemeral()));
    Serf::tcp(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      self.delegate,
      self.runtime,
      self.serf,
      None,
      None,
      self.snapshot,
      #[cfg(encryption)]
      Rc::new(serf_compio::VoidKeyringDelegate),
    )
    .await
    .expect("spawn serf tcp node")
  }
}

/// Spawn a plain loopback node.
async fn spawn_node(id: &str) -> Serf<SmolStr, SocketAddr> {
  NodeSpec::new().spawn(id).await
}

/// Poll both nodes until each reports the full two-member cluster.
async fn converge(a: &Serf<SmolStr, SocketAddr>, b: &Serf<SmolStr, SocketAddr>) {
  compio::time::timeout(WINDOW, async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Join `joiner` to `seed` and wait for both to converge.
async fn join_and_converge(joiner: &Serf<SmolStr, SocketAddr>, seed: &Serf<SmolStr, SocketAddr>) {
  joiner
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed.advertise_address()),
      false,
    )
    .await
    .expect("join reaches the seed");
  converge(joiner, seed).await;
}

/// Drive `events` until the next inbound `Event::Query` named `name` arrives,
/// returning its response token.
async fn next_query_token<S>(events: &mut S, name: &str) -> QueryEvent<SmolStr, SocketAddr>
where
  S: futures_util::Stream<Item = Event<SmolStr, SocketAddr>> + Unpin,
{
  compio::time::timeout(WINDOW, async {
    loop {
      match events.next().await {
        Some(Event::Query(qe)) if qe.name() == name => break qe,
        Some(_) => {}
        None => panic!("the event stream closed before the query arrived"),
      }
    }
  })
  .await
  .expect("the query reaches the responder within the window")
}

/// A unique temp path for a snapshot file.
fn snapshot_path(name: &str) -> std::path::PathBuf {
  let mut p = std::env::temp_dir();
  p.push(format!("serf-compio-it-{name}-{}", std::process::id()));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&p);
  p
}

/// A query issued by A round-trips: B surfaces the inbound `Event::Query` (and
/// fires its `notify_query` hook), answers through `Serf::respond`, and A
/// surfaces the matching `Event::QueryResponse` carrying B's payload.
#[compio::test]
async fn query_round_trips_through_respond() {
  let seen = Rc::new(Observed::default());
  let b = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("q-b")
    .await;
  let a = spawn_node("q-a").await;

  let mut b_events = b.events();
  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  let want = Bytes::from_static(b"pong");
  a.query(
    "ping",
    Bytes::from_static(b"ping-payload"),
    a.default_query_param(),
  )
  .await
  .expect("query issued");

  let responder = async {
    let token = next_query_token(&mut b_events, "ping").await;
    assert_eq!(
      token.payload(),
      &Bytes::from_static(b"ping-payload"),
      "the inbound query carries the originator's payload"
    );
    b.respond(token, want.clone())
      .await
      .expect("B responds to the query");
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

  let got = compio::time::timeout(WINDOW, async {
    let (_, got) = future::join(responder, collector).await;
    got
  })
  .await
  .expect("the query round-trip completes within the window");
  assert!(got, "A must receive B's query response");

  assert_eq!(
    seen.queries.borrow().as_slice(),
    &[SmolStr::new("ping")],
    "the driver fired B's notify_query hook exactly once for the inbound query"
  );

  a.shutdown().await.expect("q-a shuts down");
  b.shutdown().await.expect("q-b shuts down");
}

/// A user event broadcast by B reaches A's event stream with the original name
/// and payload, and fires A's `notify_user_event` observation hook.
#[compio::test]
async fn user_event_reaches_the_peer_stream_and_delegate() {
  let seen = Rc::new(Observed::default());
  let a = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("ue-a")
    .await;
  let b = spawn_node("ue-b").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  let payload = Bytes::from_static(b"deploy-42");
  b.user_event("deploy", payload.clone(), false)
    .await
    .expect("B broadcasts a user event");

  let got = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "deploy" => break u.payload.clone(),
        Some(_) => {}
        None => panic!("A's event stream closed before the user event arrived"),
      }
    }
  })
  .await
  .expect("A receives B's user event within the window");
  assert_eq!(got, payload, "the payload survives the broadcast");

  assert!(
    Observed::recorded_within(&seen.user_events, "deploy", WINDOW).await,
    "the driver fired A's notify_user_event hook for the broadcast"
  );

  a.shutdown().await.expect("ue-a shuts down");
  b.shutdown().await.expect("ue-b shuts down");
}

/// `set_tags` re-tags the local node and the change propagates: the peer records
/// a `Member(Update)` event, fires its `notify_update` hook, and its membership
/// view carries the new tag value.
#[compio::test]
async fn set_tags_propagates_as_a_member_update() {
  let seen = Rc::new(Observed::default());
  let a = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("tag-a")
    .await;
  let b = spawn_node("tag-b").await;

  join_and_converge(&a, &b).await;

  let mut tags = Tags::new();
  tags.0.insert(SmolStr::new("role"), SmolStr::new("worker"));
  b.set_tags(tags).await.expect("B re-tags itself");

  let got = compio::time::timeout(WINDOW, async {
    loop {
      let seen_tag = a
        .members()
        .iter()
        .find(|m| m.node().id_ref().as_str() == "tag-b")
        .and_then(|m| m.tags().0.get("role").cloned());
      if let Some(v) = seen_tag {
        break v;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A observes B's new tag within the window");
  assert_eq!(got.as_str(), "worker", "A's view of B carries the new tag");

  assert!(
    Observed::recorded_within(&seen.updated, "tag-b", WINDOW).await,
    "the driver fired A's notify_update hook for the re-tagged peer"
  );

  a.shutdown().await.expect("tag-a shuts down");
  b.shutdown().await.expect("tag-b shuts down");
}

/// A graceful `leave()` resolves only once the machine's `LeftCluster` fires, so
/// the event surfaces on the leaver's own stream and its endpoint settles at
/// `Left` — and the peer records a `Leave`, not a `Failed`.
#[compio::test]
async fn graceful_leave_emits_left_cluster_and_settles_left() {
  let b = spawn_node("lv-b").await;
  let a = spawn_node("lv-a").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  a.leave().await.expect("A leaves the cluster");

  let saw = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::LeftCluster) => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("A observes LeftCluster within the window");
  assert!(saw, "A must surface Event::LeftCluster after leave()");

  compio::time::timeout(WINDOW, async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A's endpoint state becomes Left");

  a.shutdown().await.expect("lv-a shuts down");
  b.shutdown().await.expect("lv-b shuts down");
}

/// Two `leave()` callers racing on the same node share ONE in-flight leave: the
/// second joins the first's waiter rather than re-invoking the machine's
/// terminal `leave()` (which emits no second `LeftCluster` and would hang a fresh
/// waiter). Both must resolve `Ok`.
#[compio::test]
async fn concurrent_leave_calls_share_one_in_flight_operation() {
  let b = spawn_node("cl-b").await;
  let a = spawn_node("cl-a").await;
  join_and_converge(&a, &b).await;

  let (first, second) = compio::time::timeout(WINDOW, future::join(a.leave(), a.leave()))
    .await
    .expect("both racing leaves resolve within the window");
  first.expect("the initiating leave resolves Ok");
  second.expect("the leave that joined the in-flight operation resolves Ok");

  a.shutdown().await.expect("cl-a shuts down");
  b.shutdown().await.expect("cl-b shuts down");
}

/// Once a node has left, every mutating command it is handed reports
/// [`SerfError::NotRunning`] rather than being applied to a non-participating
/// endpoint — while a repeat `leave()` stays idempotent (`Ok`) and the read-only
/// coordinate probe still answers.
#[compio::test]
async fn commands_after_leave_report_not_running() {
  let a = spawn_node("nr-a").await;
  let b = spawn_node("nr-b").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  // A real inbound query token, captured while A is still running, so the
  // post-leave `respond` below carries a valid token and can only be refused by
  // the not-running gate.
  b.query("probe", Bytes::new(), b.default_query_param())
    .await
    .expect("B issues a query");
  let token = next_query_token(&mut a_events, "probe").await;

  a.leave().await.expect("A leaves the cluster");

  assert!(
    matches!(
      a.join(
        &SocketAddrResolver,
        MaybeResolved::Resolved(b.advertise_address()),
        false
      )
      .await,
      Err(SerfError::NotRunning)
    ),
    "join after leave must be refused"
  );
  assert!(
    matches!(
      a.force_leave(SmolStr::new("nr-b"), false).await,
      Err(SerfError::NotRunning)
    ),
    "force_leave after leave must be refused"
  );
  assert!(
    matches!(
      a.user_event("evt", Bytes::new(), false).await,
      Err(SerfError::NotRunning)
    ),
    "user_event after leave must be refused"
  );
  assert!(
    matches!(
      a.query("q", Bytes::new(), a.default_query_param()).await,
      Err(SerfError::NotRunning)
    ),
    "query after leave must be refused"
  );
  assert!(
    matches!(
      a.respond(token, Bytes::new()).await,
      Err(SerfError::NotRunning)
    ),
    "respond after leave must be refused"
  );
  assert!(
    matches!(a.set_tags(Tags::new()).await, Err(SerfError::NotRunning)),
    "set_tags after leave must be refused"
  );
  #[cfg(encryption)]
  {
    let key = test_secret_key(0x5a);
    assert!(
      matches!(a.install_key(key).await, Err(SerfError::NotRunning)),
      "install_key after leave must be refused"
    );
    assert!(
      matches!(a.use_key(key).await, Err(SerfError::NotRunning)),
      "use_key after leave must be refused"
    );
    assert!(
      matches!(a.remove_key(key).await, Err(SerfError::NotRunning)),
      "remove_key after leave must be refused"
    );
    assert!(
      matches!(a.list_keys().await, Err(SerfError::NotRunning)),
      "list_keys after leave must be refused"
    );
  }

  // A repeat leave is a terminal no-op: it resolves immediately rather than
  // parking a waiter for a `LeftCluster` that will never fire again.
  compio::time::timeout(WINDOW, a.leave())
    .await
    .expect("the repeat leave resolves rather than parking")
    .expect("a repeat leave is idempotent");

  // The coordinate cache stays readable after the node has left — it is a
  // read-only probe, not a cluster mutation.
  #[cfg(feature = "coordinates")]
  a.cached_coordinate(SmolStr::new("nr-b"))
    .await
    .expect("the coordinate cache answers after leave");

  a.shutdown().await.expect("nr-a shuts down");
  b.shutdown().await.expect("nr-b shuts down");
}

/// Commands still queued behind a `Shutdown` when the pump breaks are ANSWERED
/// with [`SerfError::Shutdown`] at teardown, never dropped: a caller's reply
/// receiver must not hang forever because the driver exited between its send and
/// its dispatch. Every command variant is queued behind the shutdown in one
/// batch, so each teardown reply arm is exercised.
#[compio::test]
async fn commands_queued_behind_a_shutdown_are_answered_not_dropped() {
  let a = spawn_node("td-a").await;
  let b = spawn_node("td-b").await;

  let mut b_events = b.events();
  join_and_converge(&a, &b).await;

  a.query("probe", Bytes::new(), a.default_query_param())
    .await
    .expect("A issues a query");
  let token = next_query_token(&mut b_events, "probe").await;

  let a_addr = a.advertise_address();
  // Declared ahead of `queued` so it outlives the boxed futures that borrow it.
  #[cfg(encryption)]
  let key = test_secret_key(0x6b);
  let mut queued: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
  // Polled first, so `Shutdown` is the head of the command queue and every
  // command pushed after it lands behind it.
  queued.push(Box::pin(async {
    b.shutdown().await.expect("the shutdown itself is acked");
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.user_event("evt", Bytes::new(), false).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.query("q", Bytes::new(), b.default_query_param()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.respond(token, Bytes::new()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.set_tags(Tags::new()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.force_leave(SmolStr::new("td-a"), false).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.leave().await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(
      b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
        .await,
    );
  }));
  queued.push(Box::pin(async {
    expect_shutdown(
      b.dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(a_addr)])
        .await,
    );
  }));
  #[cfg(encryption)]
  {
    queued.push(Box::pin(async {
      expect_shutdown(b.install_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.use_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.remove_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.list_keys().await);
    }));
  }
  #[cfg(feature = "coordinates")]
  queued.push(Box::pin(async {
    expect_shutdown(b.cached_coordinate(SmolStr::new("td-a")).await);
  }));

  compio::time::timeout(WINDOW, future::join_all(queued))
    .await
    .expect("every command queued behind the shutdown is answered, none hang");

  // A command issued AFTER the driver has torn down fails fast on the handle's
  // shutdown flag rather than queueing into a dead channel.
  assert!(
    matches!(
      b.user_event("late", Bytes::new(), false).await,
      Err(SerfError::Shutdown)
    ),
    "a post-teardown command fails fast with Shutdown"
  );
  assert!(
    matches!(
      b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
        .await,
      Err(SerfError::Shutdown)
    ),
    "a post-teardown join fails fast with Shutdown"
  );

  a.shutdown().await.expect("td-a shuts down");
}

/// Assert a command's reply is the teardown `Shutdown` error.
fn expect_shutdown<T>(res: Result<T, SerfError>)
where
  T: core::fmt::Debug,
{
  match res {
    Err(SerfError::Shutdown) => {}
    Err(other) => panic!("expected Shutdown, got {other:?}"),
    Ok(v) => panic!("expected Shutdown, got Ok({v:?})"),
  }
}

/// With an UNBOUNDED observation channel the driver opts out of shedding
/// entirely: a stalling delegate cannot make the pump drop a single event, so a
/// burst of user events is delivered in full and the drop counters stay at zero.
#[compio::test]
async fn an_unbounded_observation_channel_sheds_nothing() {
  const BURST: u32 = 32;
  let a = NodeSpec::new()
    .with_delegate(StallingDelegate {
      stall: Duration::from_millis(2),
    })
    .with_runtime(RuntimeOptions::new().with_observation_channel(Channel::Unbounded))
    .spawn("unb-a")
    .await;

  let mut events = a.events();
  for i in 0..BURST {
    a.user_event(format!("burst-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  let delivered = compio::time::timeout(WINDOW, async {
    let mut n = 0u32;
    while n < BURST {
      match events.next().await {
        Some(Event::User(_)) => n += 1,
        Some(_) => {}
        None => panic!("the event stream closed mid-burst"),
      }
    }
    n
  })
  .await
  .expect("every event of the burst is delivered under an unbounded observation channel");

  assert_eq!(delivered, BURST, "no event of the burst is shed");
  assert_eq!(
    a.observation_dropped(),
    0,
    "an unbounded observation channel never drops"
  );
  assert_eq!(
    a.events_dropped(),
    0,
    "the drained event stream never drops"
  );

  a.shutdown().await.expect("unb-a shuts down");
}

/// A cap-1 observation channel behind a delegate that parks on every user event
/// makes the pump shed: the enqueue retries once (yielding to the observation
/// task) and then drops and counts, so `observation_dropped` becomes non-zero.
/// A driver that blocked on the full queue instead would stall the FSM.
#[compio::test]
async fn a_stalled_delegate_makes_the_bounded_observation_channel_shed() {
  const BURST: u32 = 32;
  let a = NodeSpec::new()
    .with_delegate(StallingDelegate {
      stall: Duration::from_secs(30),
    })
    .with_runtime(RuntimeOptions::new().with_observation_channel(Channel::Bounded(1)))
    .spawn("shed-a")
    .await;

  for i in 0..BURST {
    a.user_event(format!("shed-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  // Each `user_event().await` returned only after the pump processed that
  // command, and the shed is counted on the same single-threaded executor, so a
  // short settle is enough for the drain to surface every queued event.
  let dropped = compio::time::timeout(WINDOW, async {
    loop {
      let n = a.observation_dropped();
      if n > 0 {
        break n;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("a stalled delegate on a cap-1 observation channel must make the pump shed");

  assert!(
    dropped > 0,
    "the pump drops and counts events the stalled observation task cannot take (got {dropped})"
  );

  a.shutdown().await.expect("shed-a shuts down");
}

/// The user- and member-coalescer shed counters are distinct cells: a node whose
/// user coalescer is shedding must still report zero member-coalescer drops. A
/// construction typo aliasing the two readers onto one cell would leak the user
/// count into the member count and fail here.
#[compio::test]
async fn user_and_member_coalescer_drop_counters_are_distinct() {
  let cap = core::num::NonZeroUsize::new(1).expect("1 is nonzero");
  let a = NodeSpec::new()
    .with_serf(
      SerfOptions::new()
        .with_user_coalesce_period(Duration::from_secs(10))
        .with_user_quiescent_period(Duration::from_secs(2))
        .with_max_coalesced_user_events(Some(cap)),
    )
    .spawn("dc-a")
    .await;

  assert_eq!(
    a.local_id().as_str(),
    "dc-a",
    "the handle carries its local id"
  );

  for i in 0..8u32 {
    a.user_event(format!("cc-{i}"), Bytes::new(), true)
      .await
      .expect("user event dispatched");
  }

  assert!(
    a.coalesced_user_events_dropped() > 0,
    "the user coalescer sheds every distinct-named event past its cap"
  );
  assert_eq!(
    a.coalesced_member_events_dropped(),
    0,
    "the member coalescer is a separate counter and has shed nothing"
  );

  a.shutdown().await.expect("dc-a shuts down");
}

/// An abruptly-killed peer is detected Failed (never Leave — the kill sends no
/// farewell), fires the observer's `notify_failed` hook, and is then reaped out
/// of the membership by the serf reaper.
#[compio::test]
async fn an_abrupt_kill_surfaces_failed_then_reap() {
  let seen = Rc::new(Observed::default());
  let a = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .with_serf(cluster::ClusterTiming::fast().serf_opts())
    .spawn("kill-a")
    .await;
  let b = NodeSpec::new()
    .with_serf(cluster::ClusterTiming::fast().serf_opts())
    .spawn("kill-b")
    .await;

  join_and_converge(&a, &b).await;
  b.shutdown().await.expect("kill-b is torn down abruptly");
  drop(b);

  compio::time::timeout(WINDOW, async {
    loop {
      if a.num_members() == 1 {
        break;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A detects the killed peer Failed and reaps it out of the membership");

  assert!(
    Observed::recorded_within(&seen.failed, "kill-b", WINDOW).await,
    "the driver fired notify_failed for the abruptly-killed peer (saw {:?})",
    seen.failed.borrow()
  );

  a.shutdown().await.expect("kill-a shuts down");
}

/// `force_leave` on a member the cluster has already declared Failed moves it to
/// a Left tombstone on every survivor, and the observer's ordered event log
/// records the full Join -> Failed -> Leave lifecycle.
#[compio::test]
async fn force_leave_moves_a_failed_member_to_a_left_tombstone() {
  let mut cluster = cluster::Cluster::spawn(
    &["fl-a", "fl-b"],
    cluster::ClusterTiming::fast()
      .with_reconnect_timeout(Duration::from_secs(120))
      .with_tombstone_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .force_leave(subject.clone(), false)
    .await
    .expect("force_leave dispatches for a failed member");

  cluster
    .await_member_status(0, subject.as_str(), MemberStatus::Left)
    .await;
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

/// A gracefully-left member is recorded as Leave (never Failed) by its peer and
/// lands in the peer's Left tombstone view.
#[compio::test]
async fn a_graceful_leave_is_recorded_as_leave_not_failed() {
  let mut cluster = cluster::Cluster::spawn(
    &["gl-a", "gl-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.leave_graceful(1).await;
  cluster
    .await_member_status(0, subject.as_str(), MemberStatus::Left)
    .await;
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[MemberEventKind::Join, MemberEventKind::Leave],
    )
    .await;

  cluster.shutdown_all().await;
}

/// A snapshot with a 1-byte compaction threshold is rewritten to the live
/// membership on every flush, so the file stays a compact record of the current
/// cluster: after a restart the node replays it and rejoins the seed WITHOUT any
/// explicit join call.
#[compio::test]
async fn a_compacting_snapshot_replays_the_membership_on_restart() {
  let path = snapshot_path("compact");
  let a = spawn_node("snap-a").await;
  let b = NodeSpec::new()
    .with_snapshot(SnapshotOptions::new(&path).with_compact_threshold(1))
    .spawn("snap-b")
    .await;
  assert_eq!(
    SnapshotOptions::new(&path).with_compact_threshold(1).path(),
    path.as_path(),
    "the snapshot options carry the configured path"
  );

  join_and_converge(&b, &a).await;
  b.shutdown().await.expect("snap-b shuts down");
  drop(b);

  // The compacted file still names the seed, so the restarted node re-dials it
  // from the replayed membership alone.
  let bytes = std::fs::read(&path).expect("the compacted snapshot survives the shutdown");
  assert!(
    !bytes.is_empty(),
    "compaction rewrites the live membership rather than truncating the file"
  );

  let b2 = NodeSpec::new()
    .with_snapshot(SnapshotOptions::new(&path).with_compact_threshold(1))
    .spawn("snap-b")
    .await;
  converge(&a, &b2).await;

  a.shutdown().await.expect("snap-a shuts down");
  b2.shutdown().await.expect("snap-b2 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// A reliable exchange whose peer accepts the connection and then walks away
/// still terminalizes: the driver signals the live bridge closed at teardown and
/// releases both bound ports, rather than leaking the bridge task and the socket.
#[compio::test]
async fn shutdown_closes_a_bridge_whose_peer_never_answers() {
  let stall = compio::net::TcpListener::bind(cluster::loopback_ephemeral())
    .await
    .expect("bind the stalling peer");
  let stall_addr = stall.local_addr().expect("stalling peer address");

  // Accept and hold the connection open without ever answering, so the joiner's
  // bridge is still live when the shutdown lands.
  let held = Rc::new(RefCell::new(None));
  let held_slot = held.clone();
  compio::runtime::spawn(async move {
    if let Ok((stream, _peer)) = stall.accept().await {
      *held_slot.borrow_mut() = Some(stream);
    }
    // Ignoring Err: test cleanup of the stalling listener.
    let _ = stall.close().await;
  })
  .detach();

  let a = spawn_node("stall-a").await;
  let a_addr = a.advertise_address();
  let dispatched = a
    .dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(stall_addr)])
    .await
    .expect("the dial is dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  // Wait until the stalling peer has actually accepted, so the bridge is live.
  compio::time::timeout(WINDOW, async {
    loop {
      if held.borrow().is_some() {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the stalling peer accepts the reliable dial");

  a.shutdown()
    .await
    .expect("shutdown completes with a live bridge outstanding");

  // The shutdown released both bound ports even with the bridge still open.
  let listener = compio::net::TcpListener::bind(a_addr)
    .await
    .expect("the freed TCP port rebinds after a shutdown with a live bridge");
  let gossip = compio::net::UdpSocket::bind(a_addr)
    .await
    .expect("the freed UDP port rebinds after a shutdown with a live bridge");
  // Ignoring Err: test cleanup of the rebind probe sockets.
  let _ = listener.close().await;
  let _ = gossip.close().await;
  // Take the stream OUT of the cell before awaiting: a `RefCell` borrow must not
  // be held across an await point.
  let held_stream = held.borrow_mut().take();
  if let Some(stream) = held_stream {
    // Ignoring Err: test cleanup of the held peer connection.
    let _ = stream.close().await;
  }
}

/// Dropping the last handle with a reliable exchange still in flight tears the
/// driver down through the command-channel disconnect: the live bridge is closed
/// and both bound ports are released, rather than the pump spinning on a bridge
/// that will never complete.
#[compio::test]
async fn dropping_the_last_handle_closes_a_live_bridge() {
  let stall = compio::net::TcpListener::bind(cluster::loopback_ephemeral())
    .await
    .expect("bind the stalling peer");
  let stall_addr = stall.local_addr().expect("stalling peer address");

  let held = Rc::new(RefCell::new(None));
  let held_slot = held.clone();
  compio::runtime::spawn(async move {
    if let Ok((stream, _peer)) = stall.accept().await {
      *held_slot.borrow_mut() = Some(stream);
    }
    // Ignoring Err: test cleanup of the stalling listener.
    let _ = stall.close().await;
  })
  .detach();

  let a = spawn_node("drop-bridge-a").await;
  let a_addr = a.advertise_address();
  a.dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(stall_addr)])
    .await
    .expect("the dial is dispatched");

  compio::time::timeout(WINDOW, async {
    loop {
      if held.borrow().is_some() {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the stalling peer accepts the reliable dial");

  drop(a);

  let (listener, gossip) = compio::time::timeout(WINDOW, async {
    loop {
      if let Ok(listener) = compio::net::TcpListener::bind(a_addr).await {
        if let Ok(gossip) = compio::net::UdpSocket::bind(a_addr).await {
          break (listener, gossip);
        }
        // The teardown closes the listener before the UDP socket; release the
        // probe listener and retry until the UDP port frees too.
        // Ignoring Err: discarding the probe listener.
        let _ = listener.close().await;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the driver releases its bound ports after the handle drop, even with a live bridge");

  // Ignoring Err: test cleanup of the rebind probe sockets.
  let _ = listener.close().await;
  let _ = gossip.close().await;
  // Take the stream OUT of the cell before awaiting: a `RefCell` borrow must not
  // be held across an await point.
  let held_stream = held.borrow_mut().take();
  if let Some(stream) = held_stream {
    // Ignoring Err: test cleanup of the held peer connection.
    let _ = stream.close().await;
  }
}

/// A peer that accepts a reliable dial, never reads the push it is sent, and
/// then closes resets the connection. The bridge must terminalize that exchange
/// rather than leaking it: no member is admitted from the dead exchange, and the
/// pump still serves a subsequent REAL join to convergence — the discriminator
/// against a wedged pump or a leaked bridge.
#[compio::test]
async fn a_resetting_peer_fails_the_exchange_without_wedging_the_pump() {
  let rude = compio::net::TcpListener::bind(cluster::loopback_ephemeral())
    .await
    .expect("bind the resetting peer");
  let rude_addr = rude.local_addr().expect("resetting peer address");

  compio::runtime::spawn(async move {
    if let Ok((stream, _peer)) = rude.accept().await {
      // Let the joiner's push bytes arrive and sit UNREAD in the receive buffer,
      // then close: a close with unread data resets the connection rather than
      // sending a clean FIN.
      compio::time::sleep(Duration::from_millis(200)).await;
      // Ignoring Err: the abrupt close is the point of the fixture.
      let _ = stream.close().await;
    }
    // Ignoring Err: test cleanup of the resetting listener.
    let _ = rude.close().await;
  })
  .detach();

  let a = spawn_node("reset-a").await;
  let dispatched = a
    .dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(rude_addr)])
    .await
    .expect("the dial is dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  // The dead exchange admits nothing, and the pump is still live: a real peer
  // joined afterwards still converges.
  let b = spawn_node("reset-b").await;
  join_and_converge(&a, &b).await;
  assert_eq!(
    a.num_members(),
    2,
    "only the real peer is admitted; the reset exchange contributed no member"
  );

  a.shutdown()
    .await
    .expect("the pump survives the failed exchange and shuts down");
  b.shutdown().await.expect("reset-b shuts down");
}

/// `join_many` whose resolver fails surfaces the resolver error rather than
/// silently reporting a zero-contact success, and an await-result `join` against
/// an unreachable seed reports the requested/contacted tally.
#[compio::test]
async fn join_surfaces_resolver_and_contact_failures() {
  let a = spawn_node("jf-a").await;

  let (reached, err) = a
    .join_many(
      &FailingResolver,
      core::iter::once(MaybeResolved::Unresolved("svc".to_string())),
      false,
    )
    .await
    .expect_err("a failing resolver must not report a healthy join");
  assert!(reached.is_empty(), "a failed resolution contacts nothing");
  assert!(
    matches!(err, SerfError::Resolve(_)),
    "the resolver's error is surfaced, got {err:?}"
  );

  let err = a
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(blackhole_addr()),
      false,
    )
    .await
    .expect_err("a blackhole seed must fail the join");
  match err {
    SerfError::JoinAllFailed(payload) => {
      assert_eq!(payload.requested(), 1, "one seed requested");
      assert_eq!(payload.contacted(), 0, "no seed contacted");
    }
    other => panic!("expected JoinAllFailed, got {other:?}"),
  }

  a.shutdown().await.expect("jf-a shuts down");
}

/// The `TcpTransportOptions` accessors reflect exactly what the builders set,
/// and `Default` is the `new()` state: the required fields the constructor's
/// guards check are unset.
#[test]
fn tcp_transport_options_accessors_reflect_builders() {
  let addr: SocketAddr = "127.0.0.1:7946".parse().expect("loopback addr");

  let empty = TcpTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(empty.local_id().is_none(), "Default leaves local_id unset");
  assert!(
    empty.advertise_addr().is_none(),
    "Default leaves advertise_addr unset"
  );
  assert_eq!(
    empty.stream().dial_timeout(),
    StreamTransportOptions::default().dial_timeout(),
    "Default carries the same stream knobs a fresh StreamTransportOptions does"
  );
  assert_eq!(
    StreamTransportOptions::default().dial_timeout(),
    serf_compio::DEFAULT_DIAL_TIMEOUT,
    "the documented dial-timeout default"
  );

  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("acc-node"))
    .with_advertise_addr(MaybeResolved::Resolved(addr))
    .with_stream(StreamTransportOptions::new().with_dial_timeout(Duration::from_millis(250)));
  assert_eq!(opts.local_id().map(SmolStr::as_str), Some("acc-node"));
  match opts.advertise_addr() {
    Some(MaybeResolved::Resolved(s)) => assert_eq!(*s, addr),
    other => panic!("expected a resolved advertise addr, got {other:?}"),
  }
  assert_eq!(opts.stream().dial_timeout(), Duration::from_millis(250));

  #[cfg(encryption)]
  assert!(
    opts.encryption().keyring().is_none(),
    "the default encryption policy carries no keyring"
  );
}

/// `TcpTransport::new` refuses the two required fields it cannot default:
/// without a local id, and without an advertise address, construction fails with
/// `InvalidInput` naming the missing field — before binding a socket.
#[compio::test]
async fn tcp_new_requires_a_local_id_and_an_advertise_addr() {
  let no_id = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_advertise_addr(MaybeResolved::Resolved(cluster::loopback_ephemeral()));
  assert_missing_field(
    TcpTransport::<SmolStr, SocketAddr>::new(no_id, &SocketAddrResolver, &FirstAddrResolver).await,
    "local_id",
  );

  let no_addr =
    TcpTransportOptions::<SmolStr, SocketAddr>::new().with_local_id(SmolStr::new("no-addr"));
  assert_missing_field(
    TcpTransport::<SmolStr, SocketAddr>::new(no_addr, &SocketAddrResolver, &FirstAddrResolver)
      .await,
    "advertise_addr",
  );
}

/// Assert a transport construction was refused with `InvalidInput` naming the
/// required field the caller left unset.
fn assert_missing_field<T>(res: Result<T, SerfError>, field: &str) {
  match res {
    Err(SerfError::Io(e)) => {
      assert_eq!(
        e.kind(),
        std::io::ErrorKind::InvalidInput,
        "a missing required field is an InvalidInput refusal"
      );
      assert!(
        e.to_string().contains(field),
        "the refusal names the missing field {field:?}, got {e}"
      );
    }
    Err(other) => panic!("expected InvalidInput({field}), got {other:?}"),
    Ok(_) => panic!("a missing {field} must be refused, but construction succeeded"),
  }
}

/// An UNRESOLVED advertise address is resolved through the caller's `Resolver`
/// and NARROWED by the `AdvertiseAddrResolver`: the resolver offers an IPv6 and
/// an IPv4 candidate, the IPv4-preferring picker chooses the IPv4 one, and the
/// transport binds THAT address, reports it as its advertise contact, and still
/// remembers the unresolved input form it was constructed from.
#[compio::test]
async fn tcp_new_resolves_and_narrows_an_unresolved_advertise_addr() {
  let input: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("unres-node"))
    .with_advertise_addr(MaybeResolved::Unresolved(input));
  let transport =
    TcpTransport::<SmolStr, SocketAddr>::new(opts, &DualStackResolver, &Ipv4PreferringResolver)
      .await
      .expect("an unresolved advertise address resolves through the resolver");

  assert_eq!(transport.local_id().as_str(), "unres-node");
  let bound = *transport.advertise_address();
  assert!(
    bound.is_ipv4(),
    "the IPv4-preferring picker narrowed the dual-stack candidate set, got {bound}"
  );
  assert!(bound.ip().is_loopback(), "the picked candidate was bound");
  assert_ne!(
    bound.port(),
    0,
    "the ephemeral port is read back concretely"
  );
  match transport.local_address() {
    MaybeResolved::Unresolved(a) => {
      assert_eq!(*a, input, "the unresolved input form is retained")
    }
    other => panic!("expected the unresolved input form, got {other:?}"),
  }
}

/// The built-in advertise resolvers implement their documented policies over a
/// candidate set, and every one of them reports `Empty` on an empty set rather
/// than silently picking nothing.
#[test]
fn advertise_resolvers_pick_by_their_documented_policy() {
  let v4: SocketAddr = "127.0.0.1:7946".parse().expect("v4 addr");
  let v6: SocketAddr = "[::1]:7946".parse().expect("v6 addr");

  assert_eq!(
    FirstAddrResolver
      .pick(vec![v6, v4])
      .expect("the first candidate"),
    v6,
    "FirstAddrResolver takes the head of the set"
  );
  assert_eq!(
    Ipv4PreferringResolver
      .pick(vec![v6, v4])
      .expect("the v4 candidate"),
    v4,
    "Ipv4PreferringResolver prefers IPv4 over an earlier IPv6"
  );
  assert_eq!(
    Ipv4PreferringResolver
      .pick(vec![v6])
      .expect("the only candidate"),
    v6,
    "Ipv4PreferringResolver falls back to the first when no IPv4 exists"
  );
  assert_eq!(
    Ipv6PreferringResolver
      .pick(vec![v4, v6])
      .expect("the v6 candidate"),
    v6,
    "Ipv6PreferringResolver prefers IPv6 over an earlier IPv4"
  );
  assert_eq!(
    Ipv6PreferringResolver
      .pick(vec![v4])
      .expect("the only candidate"),
    v4,
    "Ipv6PreferringResolver falls back to the first when no IPv6 exists"
  );

  for empty in [
    FirstAddrResolver.pick(Vec::new()),
    Ipv4PreferringResolver.pick(Vec::new()),
    Ipv6PreferringResolver.pick(Vec::new()),
  ] {
    assert!(
      matches!(empty, Err(AdvertiseResolutionError::Empty)),
      "an empty candidate set is an error, not a silent pick"
    );
  }
}

/// `SocketAddrResolver` is the identity pass-through, and `OsResolver` resolves
/// a literal-IP host without a name lookup.
#[compio::test]
async fn the_builtin_resolvers_resolve_concrete_addresses() {
  let addr: SocketAddr = "127.0.0.1:7946".parse().expect("loopback addr");
  assert_eq!(
    SocketAddrResolver
      .resolve(&addr)
      .await
      .expect("identity resolution"),
    vec![addr],
    "SocketAddrResolver passes an already-resolved address straight through"
  );

  let host = hostaddr::HostAddr::<SmolStr>::from(addr);
  assert_eq!(
    OsResolver.resolve(&host).await.expect("os resolution"),
    vec![addr],
    "OsResolver resolves a literal-IP host to that exact address"
  );
}

/// `LocalAddrResolver::default()` is the PRIVATE scope — the LAN-cluster choice
/// the docs promise — and the public / all scopes are distinct from it. Every
/// scope passes a CONCRETE address straight through: the interface enumeration
/// runs only for a wildcard advertise address.
#[cfg(feature = "getifs")]
#[compio::test]
async fn local_addr_resolver_defaults_to_the_private_scope() {
  use serf_compio::LocalAddrResolver;

  let default = format!("{:?}", LocalAddrResolver::default());
  assert_eq!(
    default,
    format!("{:?}", LocalAddrResolver::private()),
    "the default scope is the private scope"
  );
  assert_ne!(
    format!("{:?}", LocalAddrResolver::public()),
    default,
    "the public scope is distinct from the default"
  );
  assert_ne!(
    format!("{:?}", LocalAddrResolver::all()),
    default,
    "the all scope is distinct from the default"
  );

  let concrete: SocketAddr = "127.0.0.1:7946".parse().expect("loopback addr");
  for resolver in [
    LocalAddrResolver::private(),
    LocalAddrResolver::public(),
    LocalAddrResolver::all(),
  ] {
    assert_eq!(
      resolver
        .resolve(&concrete)
        .await
        .expect("a concrete address needs no interface scan"),
      vec![concrete],
      "a concrete advertise address passes through unchanged in every scope"
    );
  }
}

/// An advertise address the caller supplied UNRESOLVED must not silently bind a
/// wrong contact when resolution cannot answer: a resolver outage surfaces as
/// `SerfError::Resolve`, and a resolution that yields ZERO candidates is refused
/// by the advertise picker rather than defaulted.
#[compio::test]
async fn tcp_new_refuses_an_advertise_address_it_cannot_resolve() {
  let opts = TcpTransportOptions::<SmolStr, String>::new()
    .with_local_id(SmolStr::new("res-fail"))
    .with_advertise_addr(MaybeResolved::Unresolved("self".to_string()));
  match TcpTransport::<SmolStr, String>::new(opts, &FailingResolver, &FirstAddrResolver).await {
    Err(SerfError::Resolve(e)) => assert!(
      e.to_string().contains("discovery backend unavailable"),
      "the resolver's own error is surfaced, got {e}"
    ),
    Err(other) => panic!("expected Resolve, got {other:?}"),
    Ok(_) => panic!("a resolver outage must refuse construction"),
  }

  let opts = TcpTransportOptions::<SmolStr, String>::new()
    .with_local_id(SmolStr::new("res-empty"))
    .with_advertise_addr(MaybeResolved::Unresolved("self".to_string()));
  match TcpTransport::<SmolStr, String>::new(opts, &EmptyResolver, &FirstAddrResolver).await {
    Err(SerfError::Resolve(e)) => assert_eq!(
      e.kind(),
      std::io::ErrorKind::AddrNotAvailable,
      "a zero-candidate resolution is an unavailable advertise address"
    ),
    Err(other) => panic!("expected Resolve(AddrNotAvailable), got {other:?}"),
    Ok(_) => panic!("a zero-candidate resolution must refuse construction"),
  }
}

/// A seed keyring already carrying a cross-cipher byte twin (two keys sharing a
/// raw byte value across different cipher variants) makes every later
/// byte-keyed rotation op ambiguous — a `use` or `remove` could promote or drop
/// the WRONG cipher's key — so construction refuses it before binding a socket.
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[compio::test]
async fn tcp_new_refuses_a_cross_cipher_twin_keyring() {
  use serf_compio::{EncryptionOptions, Keyring, SecretKey};

  let twins = Keyring::with_secondaries(
    SecretKey::Aes256([0x21; 32]),
    [SecretKey::ChaCha20Poly1305([0x21; 32])],
  );
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("twin-node"))
    .with_advertise_addr(MaybeResolved::Resolved(cluster::loopback_ephemeral()))
    .with_encryption(EncryptionOptions::new().with_keyring(twins));
  match TcpTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver)
    .await
  {
    Err(SerfError::Io(e)) => {
      assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
      assert!(
        e.to_string().contains("cross-cipher"),
        "the refusal names the collision, got {e}"
      );
    }
    Err(other) => panic!("expected InvalidInput(cross-cipher), got {other:?}"),
    Ok(_) => panic!("a cross-cipher twin keyring must be refused at construction"),
  }
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled.
#[cfg(encryption)]
fn test_secret_key(fill: u8) -> serf_compio::SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = serf_compio::SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = serf_compio::SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// The transport's SWIM knobs actually reach the coordinator's `EndpointOptions`,
/// so the memberlist failure detector is tunable from serf-compio.
///
/// Tuned to a 100 ms probe interval with both suspicion multipliers at 1, an
/// abruptly-killed peer is declared Failed within a few hundred milliseconds. A
/// coordinator left on its OWN defaults needs well over ten seconds for the same
/// kill in a two-node cluster: the probe interval is 1 s, the minimum suspicion
/// timeout is `suspicion_mult(4) * log10(N+1) * probe_interval`, and — with no
/// third node to confirm the Suspect — the timer runs its full
/// `suspicion_max_timeout_mult(6)` multiple of that minimum rather than decaying to
/// it. Asserting detection inside the window below therefore FAILS if
/// `Transport::run` accepted the knobs and dropped them on the floor.
#[compio::test]
async fn swim_knobs_reach_the_coordinator_and_speed_failure_detection() {
  /// Detection must land far inside this bound; a default-timing coordinator
  /// could not.
  const DETECT_WINDOW: Duration = Duration::from_secs(3);

  /// A node whose failure detector is tuned for sub-second detection.
  async fn spawn_tuned(id: &str) -> Serf<SmolStr, SocketAddr> {
    let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(cluster::loopback_ephemeral()))
      .with_probe_interval(Duration::from_millis(100))
      .with_probe_timeout(Duration::from_millis(50))
      .with_gossip_interval(Duration::from_millis(20))
      // A two-node cluster has no third node to confirm a Suspect, so the
      // max-timeout multiple is what the suspicion timer actually runs. Pinning
      // both multipliers to 1 keeps it at the probe-scaled minimum.
      .with_suspicion_mult(1)
      .with_suspicion_max_timeout_mult(1);
    Serf::tcp(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      // Hold the Failed member rather than reaping it, so the status is
      // observable instead of racing the reaper.
      SerfOptions::new().with_reconnect_timeout(Duration::from_secs(3600)),
      None,
      None,
      None,
      #[cfg(encryption)]
      Rc::new(serf_compio::VoidKeyringDelegate),
    )
    .await
    .expect("spawn tuned serf tcp node")
  }

  let a = spawn_tuned("swim-a").await;
  let b = spawn_tuned("swim-b").await;

  b.join(
    &SocketAddrResolver,
    MaybeResolved::Resolved(a.advertise_address()),
    false,
  )
  .await
  .expect("B joins A");
  converge(&a, &b).await;

  let subject = SmolStr::new("swim-b");
  b.shutdown().await.expect("swim-b is killed abruptly");

  compio::time::timeout(DETECT_WINDOW, async {
    loop {
      let failed = a
        .members()
        .iter()
        .any(|m| m.node().id_ref() == &subject && m.status() == MemberStatus::Failed);
      if failed {
        break;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect(
    "the tuned probe/suspicion knobs must reach the coordinator: a default-timing \
     coordinator could not declare the killed peer Failed this fast",
  );

  a.shutdown().await.expect("swim-a shuts down");
}

/// The transport's user-facing address type `A` is usable with the crate's OWN
/// host-address resolvers, so the declared default `A = HostAddr<SmolStr>` is a
/// real, constructible configuration rather than a dead type parameter.
///
/// `OsResolver` and `DnsResolver` both resolve `hostaddr::HostAddr<SmolStr>`, and
/// `HostAddr` carries no wire-codec impl (nor could a downstream crate add one —
/// it is a foreign type). A transport that constrained `A` to the wire-codec trait
/// would therefore reject every one of the crate's host-address resolvers and
/// admit only an already-resolved `SocketAddr`; `A` is only ever resolved at the
/// boundary and never encoded, so no such bound is warranted. This spawns a real
/// node through the default `A`, resolving `localhost:0` with `OsResolver`.
#[compio::test]
async fn an_unresolved_host_advertise_addr_resolves_through_the_os_resolver() {
  let host: hostaddr::HostAddr<SmolStr> = "localhost:0".parse().expect("a host:port address");

  let node = Serf::tcp_with_rng(
    TcpTransportOptions::new()
      .with_local_id(SmolStr::new("hostaddr-node"))
      .with_advertise_addr(MaybeResolved::Unresolved(host)),
    &OsResolver,
    &Ipv4PreferringResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    None,
    None,
    None,
    #[cfg(encryption)]
    Rc::new(serf_compio::VoidKeyringDelegate),
  )
  .await
  .expect("a HostAddr advertise address resolves and binds through OsResolver");

  // The resolver produced a concrete, dialable contact: the OS-assigned port is
  // read back from the bound socket, and the loopback name resolved to a
  // loopback IP.
  let advertise = node.advertise_address();
  assert!(
    advertise.ip().is_loopback(),
    "localhost resolved to a loopback contact, got {advertise}"
  );
  assert_ne!(
    advertise.port(),
    0,
    "the ephemeral :0 must be read back as a concrete bound port"
  );
  assert_eq!(node.local_id(), &SmolStr::new("hostaddr-node"));

  node.shutdown().await.expect("hostaddr-node shuts down");
}
