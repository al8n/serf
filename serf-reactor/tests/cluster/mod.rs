//! Reusable multi-node fault-injection fixture for the reactor's real-node serf
//! driver, shared by the runtime-generic scenario bodies in the sibling test
//! binaries.
//!
//! A [`Cluster`] spins up N ephemeral loopback nodes through the ergonomic
//! `Serf::tcp` constructor with fast SWIM failure-detection timing (the transport
//! probe / gossip / suspicion overrides), so an abruptly-killed peer is detected
//! as Failed in well under a second. Each node runs a detached collector that
//! drains its event stream into a shared log; the log is a separate `Arc`, so it
//! survives an abrupt [`kill_abrupt`](Cluster::kill_abrupt) (which drops the
//! node's last handle) and later assertions read an ordered per-node member-event
//! history.
//!
//! Mirrors the legacy Go-parity cluster helpers — `wait_until_num_nodes` /
//! `test_events` in `legacy/serf-core/src/serf/base/tests.rs` — adapted to the
//! reactor's `Send`/`agnostic` model.

use core::time::Duration;
use std::{
  net::SocketAddr,
  sync::{Arc, Mutex},
};

use agnostic::Runtime;
use futures_util::StreamExt;
use serf_proto::{
  event::{Event, MemberEventKind},
  members::MemberStatus,
  options::Options as SerfOptions,
};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TcpTransportOptions,
  VoidDelegate,
};
use smol_str::SmolStr;

/// A reactor TCP node handle over the agnostic runtime `R`.
pub type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// Wall-clock ceiling for every fixture poll loop, so a convergence or detection
/// regression surfaces as a bounded timeout rather than a hang.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll granularity for the fixture's await loops.
const POLL_STEP: Duration = Duration::from_millis(20);

/// Failure-detection and reap timing shared by every node in a fault-injection
/// cluster.
///
/// The probe / gossip / suspicion knobs tune the memberlist SWIM layer (carried
/// on the transport options); the reap / reconnect knobs tune the serf reaper
/// (carried on the serf `Options`). [`fast`](Self::fast) yields CI-speed values
/// that detect an abrupt kill in sub-second time and reap the failed member
/// shortly after (short reconnect timeout). A test that intends to reconnect a
/// killed node raises the reconnect timeout via
/// [`with_reconnect_timeout`](Self::with_reconnect_timeout) so the failed member
/// is held — not reaped — until it rejoins.
#[derive(Clone)]
pub struct ClusterTiming {
  probe_interval: Duration,
  probe_timeout: Duration,
  gossip_interval: Duration,
  suspicion_mult: u32,
  reap_interval: Duration,
  reconnect_interval: Duration,
  reconnect_timeout: Duration,
  tombstone_timeout: Duration,
  /// Dead-node reclaim window (`None` keeps revival-at-a-new-address a
  /// conflict, the coordinator default).
  pub dead_node_reclaim: Option<Duration>,
  /// Periodic anti-entropy push/pull override (`None` keeps the coordinator
  /// default; `Duration::ZERO` disables it, leaving gossiped intents as the
  /// only dissemination path — the exclusivity a causal clock fence needs).
  pub push_pull_interval: Option<Duration>,
  leave_propagate_delay: Duration,
}

impl ClusterTiming {
  /// CI-speed timing: sub-second SWIM failure detection on loopback with the
  /// failed member reaped shortly after. The probe timeout stays generous
  /// relative to a loopback round-trip so a live peer is not falsely suspected
  /// under CI scheduling jitter, while the suspicion timeout (fixed at the minimum
  /// for a small cluster: `suspicion_mult * probe_interval`) stays short.
  ///
  /// The probe timeout sits BELOW the probe interval so an unanswered probe still
  /// has an indirect/fallback window inside its own cycle, and the suspicion
  /// multiplier keeps the small-cluster suspicion floor at ~300 ms — a live peer
  /// survives a couple hundred milliseconds of executor starvation on an
  /// oversubscribed CI runner without being falsely declared Failed, while
  /// detection of a real kill stays comfortably sub-second.
  pub fn fast() -> Self {
    Self {
      probe_interval: Duration::from_millis(100),
      probe_timeout: Duration::from_millis(50),
      gossip_interval: Duration::from_millis(20),
      suspicion_mult: 3,
      reap_interval: Duration::from_millis(100),
      reconnect_interval: Duration::from_millis(100),
      reconnect_timeout: Duration::from_millis(1),
      tombstone_timeout: Duration::from_millis(1),
      dead_node_reclaim: None,
      push_pull_interval: None,
      // Short enough to keep a graceful leave sub-second, long enough to give
      // in-flight probes a gossip cycle to observe the leave intent.
      leave_propagate_delay: Duration::from_millis(100),
    }
  }

  /// Override the reconnect timeout — the age at which the reaper removes a Failed
  /// member. Raise it well beyond the test's kill-to-restart window to hold a
  /// failed peer for reconnection instead of reaping it.
  #[must_use]
  pub fn with_reconnect_timeout(mut self, v: Duration) -> Self {
    self.reconnect_timeout = v;
    self
  }

  /// Override the tombstone timeout — the age at which the reaper removes a
  /// gracefully-Left member. Raise it beyond the test window to HOLD a left peer
  /// in the tombstone view instead of reaping it, so a graceful-leave assertion
  /// observes `[Join, Leave]` without a trailing `Reap`.
  #[must_use]
  pub fn with_dead_node_reclaim(mut self, v: Duration) -> Self {
    self.dead_node_reclaim = Some(v);
    self
  }

  /// Override (or, at zero, disable) the periodic anti-entropy push/pull.
  pub fn with_push_pull_interval(mut self, v: Duration) -> Self {
    self.push_pull_interval = Some(v);
    self
  }

  /// Override the tombstone retention window.
  pub fn with_tombstone_timeout(mut self, v: Duration) -> Self {
    self.tombstone_timeout = v;
    self
  }

  /// The transport options for one node: the fast SWIM overrides plus this node's
  /// id and advertise address.
  fn transport_opts(
    &self,
    id: &str,
    advertise: SocketAddr,
  ) -> TcpTransportOptions<SmolStr, SocketAddr> {
    let mut opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(advertise))
      .with_probe_interval(self.probe_interval)
      .with_probe_timeout(self.probe_timeout)
      .with_gossip_interval(self.gossip_interval)
      .with_suspicion_mult(self.suspicion_mult);
    if let Some(v) = self.dead_node_reclaim {
      opts = opts.with_dead_node_reclaim_time(v);
    }
    if let Some(v) = self.push_pull_interval {
      opts = opts.with_push_pull_interval(v);
    }
    opts
  }

  /// The serf `Options` for every node: the fast reap / reconnect timing.
  fn serf_opts(&self) -> SerfOptions {
    SerfOptions::new()
      .with_reap_interval(self.reap_interval)
      .with_reconnect_interval(self.reconnect_interval)
      .with_reconnect_timeout(self.reconnect_timeout)
      .with_tombstone_timeout(self.tombstone_timeout)
      .with_leave_propagate_delay(self.leave_propagate_delay)
  }
}

/// One observed member event: its kind and the member ids it names.
#[derive(Clone)]
struct MemberRec {
  kind: MemberEventKind,
  ids: Vec<SmolStr>,
}

/// One cluster node: its stable id and advertise address, the live handle (taken
/// while killed), and the event log the collector drains into.
struct NodeSlot<R>
where
  R: Runtime,
{
  id: SmolStr,
  addr: SocketAddr,
  serf: Option<Node<R>>,
  log: Arc<Mutex<Vec<MemberRec>>>,
}

/// A live multi-node loopback cluster with per-node member-event logs.
pub struct Cluster<R>
where
  R: Runtime,
{
  timing: ClusterTiming,
  slots: Vec<NodeSlot<R>>,
}

impl<R> Cluster<R>
where
  R: Runtime,
{
  /// Spawn `ids.len()` ephemeral loopback nodes with `timing`, attach a per-node
  /// event collector, join every non-seed node to the first (a star), and wait
  /// for the whole cluster to converge.
  pub async fn spawn(ids: &[&str], timing: ClusterTiming) -> Self {
    let mut slots = Vec::with_capacity(ids.len());
    for id in ids {
      let serf = build_node::<R>(id, loopback_ephemeral(), &timing)
        .await
        .expect("spawn serf tcp node");
      let addr = serf.advertise_address();
      let log = Arc::new(Mutex::new(Vec::new()));
      // Attach the collector before the handle moves into the slot, so no member
      // event can slip past between construction and the first join.
      attach_collector::<R>(&serf, log.clone());
      slots.push(NodeSlot {
        id: SmolStr::new(*id),
        addr,
        serf: Some(serf),
        log,
      });
    }
    let cluster = Self { timing, slots };
    let seed = cluster.slots[0].addr;
    for i in 1..cluster.slots.len() {
      cluster
        .node(i)
        .join(&SocketAddrResolver, MaybeResolved::Resolved(seed), false)
        .await
        .expect("join reaches the seed node");
    }
    cluster.converge(cluster.slots.len()).await;
    cluster
  }

  /// The live handle for node `i` (panics if the node is currently killed).
  pub fn node(&self, i: usize) -> &Node<R> {
    self.slots[i].serf.as_ref().expect("node slot is live")
  }

  /// The stable id of node `i`.
  pub fn id(&self, i: usize) -> SmolStr {
    self.slots[i].id.clone()
  }

  /// Abruptly kill node `i`: shut its handle down without a leave. A shutdown
  /// sends no farewell (abrupt by design, mirroring the reference
  /// implementation's Shutdown), so peers detect a probe-timeout Failed rather
  /// than a graceful Leave. The slot's id, addr, and event log are retained for
  /// a later restart or assertion; the freed port is released before `shutdown`
  /// resolves.
  pub async fn kill_abrupt(&mut self, i: usize) {
    let serf = self.slots[i].serf.take().expect("node slot is live");
    serf.shutdown().await.expect("node shuts down");
  }

  /// Gracefully leave node `i`, then release its slot. Calls `leave()` — which
  /// the reactor resolves only once the machine's `LeftCluster` fires, so a
  /// successful return proves the graceful-leave chain completed — and returns
  /// the wall-clock the `leave().await` took (a latency canary against any
  /// reintroduced flush wait). Unlike [`kill_abrupt`](Self::kill_abrupt), the
  /// farewell (leave intent packed with the dead-self notice) reaches peers
  /// before teardown, so peers observe an intentional Leave rather than a
  /// probe-timeout Failed. `shutdown()` follows the resolved `leave()` with no
  /// intervening delay, deliberately pinning the leave-then-immediate-shutdown
  /// ordering: a resolved leave means every farewell datagram was accepted by
  /// the socket, so an immediate teardown cannot discard one. The slot's id,
  /// addr, and event log are retained for later assertions.
  pub async fn leave_graceful(&mut self, i: usize) -> Duration {
    let serf = self.slots[i].serf.take().expect("node slot is live");
    let start = std::time::Instant::now();
    serf.leave().await.expect("node leaves gracefully");
    let elapsed = start.elapsed();
    serf.shutdown().await.expect("node shuts down");
    elapsed
  }

  /// Gracefully leave node `i` with a shutdown racing the leave: both commands
  /// are issued concurrently, so they typically land in the same driver command
  /// batch and the teardown itself must egress the still-queued farewell before
  /// releasing the gossip socket. Unlike
  /// [`leave_graceful`](Self::leave_graceful) there is no resolved-leave fence
  /// ahead of the shutdown — the leave must still resolve `Ok`, proving the
  /// farewell reached the transport under the race.
  pub async fn leave_with_racing_shutdown(&mut self, i: usize) {
    let serf = self.slots[i].serf.take().expect("node slot is live");
    let (leave, shutdown) = futures_util::future::join(serf.leave(), serf.shutdown()).await;
    leave.expect("node leaves gracefully despite the racing shutdown");
    shutdown.expect("node shuts down");
  }

  /// Restart a previously-killed node `i` at the SAME id and advertise address,
  /// re-attaching a collector to the slot's existing log. The freed port is
  /// rebound with a bounded retry to absorb a transient rebind race.
  pub async fn restart(&mut self, i: usize) {
    assert!(
      self.slots[i].serf.is_none(),
      "node {i} must be killed before restart"
    );
    let id = self.slots[i].id.clone();
    let addr = self.slots[i].addr;
    const REBIND_RETRIES: usize = 25;
    let mut attempt = 0usize;
    let serf = loop {
      match build_node::<R>(id.as_str(), addr, &self.timing).await {
        Ok(serf) => break serf,
        // Ignoring Err: a transient rebind race (the freed port not yet reusable)
        // is retried; only the final attempt's error is fatal.
        Err(_) if attempt + 1 < REBIND_RETRIES => {
          attempt += 1;
          R::sleep(POLL_STEP).await;
        }
        Err(e) => panic!("restart rebind for {id:?} at {addr} failed: {e}"),
      }
    };
    attach_collector::<R>(&serf, self.slots[i].log.clone());
    self.slots[i].serf = Some(serf);
  }

  /// Restart a previously-killed node `i` at the SAME id but a FRESH ephemeral
  /// port on the same interface, re-attaching a collector to the slot's
  /// existing log and updating the recorded address. Unlike
  /// [`restart`](Self::restart), the serf reconnect re-dial cannot reach the
  /// node (it targets the old address), so the caller re-joins explicitly —
  /// exercising the same-name-new-address revival path.
  pub async fn restart_at_ephemeral(&mut self, i: usize) {
    assert!(
      self.slots[i].serf.is_none(),
      "node {i} must be killed before restart"
    );
    let id = self.slots[i].id.clone();
    let old_addr = self.slots[i].addr;
    // An ephemeral bind guarantees an AVAILABLE port, not a DIFFERENT one:
    // the OS can hand the just-released port straight back, which would
    // silently degrade a new-address scenario into a same-address one.
    // Rebind until the address genuinely differs.
    const DISTINCT_PORT_RETRIES: usize = 25;
    let mut attempt = 0usize;
    let serf = loop {
      let serf = build_node::<R>(
        id.as_str(),
        "127.0.0.1:0".parse().expect("loopback addr"),
        &self.timing,
      )
      .await
      .expect("an ephemeral rebind cannot collide");
      if serf.advertise_address() != old_addr {
        break serf;
      }
      assert!(
        attempt + 1 < DISTINCT_PORT_RETRIES,
        "the OS kept re-issuing the released port {old_addr}"
      );
      attempt += 1;
      serf.shutdown().await.expect("same-port rebind shuts down");
    };
    self.slots[i].addr = serf.advertise_address();
    attach_collector::<R>(&serf, self.slots[i].log.clone());
    self.slots[i].serf = Some(serf);
  }

  /// Poll every live node until each reports exactly `expect` members, or fail on
  /// the poll timeout.
  pub async fn converge(&self, expect: usize) {
    R::timeout(POLL_TIMEOUT, async {
      loop {
        if self
          .slots
          .iter()
          .filter_map(|s| s.serf.as_ref())
          .all(|n| n.num_members() == expect)
        {
          break;
        }
        R::sleep(POLL_STEP).await;
      }
    })
    .await
    .expect("cluster converges to the expected member count");
  }

  /// Poll until node `observer` reports exactly `expect` members.
  pub async fn await_num_members(&self, observer: usize, expect: usize) {
    R::timeout(POLL_TIMEOUT, async {
      loop {
        if self.node(observer).num_members() == expect {
          break;
        }
        R::sleep(POLL_STEP).await;
      }
    })
    .await
    .expect("observer reaches the expected member count");
  }

  /// Poll until `observer`'s membership view holds `subject` as a `Left`
  /// tombstone — the graceful-leave end state — or fail on the poll timeout.
  pub async fn await_left_tombstone(&self, observer: usize, subject: &str) {
    R::timeout(POLL_TIMEOUT, async {
      loop {
        if self
          .node(observer)
          .members()
          .iter()
          .any(|m| m.node().id_ref().as_str() == subject && m.status() == MemberStatus::Left)
        {
          break;
        }
        R::sleep(POLL_STEP).await;
      }
    })
    .await
    .unwrap_or_else(|_| panic!("node {observer} never holds {subject:?} as a Left tombstone"));
  }

  /// Poll until `observer`'s log records a member event of `kind` naming
  /// `subject`.
  pub async fn await_member_event(&self, observer: usize, subject: &str, kind: MemberEventKind) {
    R::timeout(POLL_TIMEOUT, async {
      loop {
        if self.member_event_kinds(observer, subject).contains(&kind) {
          break;
        }
        R::sleep(POLL_STEP).await;
      }
    })
    .await
    .unwrap_or_else(|_| {
      panic!(
        "node {observer} never records {kind:?} for {subject:?} (saw {:?})",
        self.member_event_kinds(observer, subject)
      )
    });
  }

  /// Poll until the ordered member-event kinds `observer` recorded about `subject`
  /// are at least as long as `expected`, then assert exact equality. Polling first
  /// lets a still-in-flight event land; the final assert then catches a wrong,
  /// missing, or extra event.
  pub async fn assert_member_events(
    &self,
    observer: usize,
    subject: &str,
    expected: &[MemberEventKind],
  ) {
    // Ignoring Err: a poll timeout here just means fewer events than expected
    // arrived; the assert_eq below reports the precise sequence mismatch.
    let _ = R::timeout(POLL_TIMEOUT, async {
      loop {
        if self.member_event_kinds(observer, subject).len() >= expected.len() {
          break;
        }
        R::sleep(POLL_STEP).await;
      }
    })
    .await;
    let actual = self.member_event_kinds(observer, subject);
    assert_eq!(
      actual, expected,
      "member events for {subject:?} observed by node {observer}"
    );
  }

  /// Shut down every live node, releasing all bound ports.
  pub async fn shutdown_all(&mut self) {
    for slot in &mut self.slots {
      if let Some(serf) = slot.serf.take() {
        serf.shutdown().await.expect("node shuts down");
      }
    }
  }

  /// The ordered member-event kinds `observer` recorded about `subject`.
  pub fn member_event_kinds(&self, observer: usize, subject: &str) -> Vec<MemberEventKind> {
    self.slots[observer]
      .log
      .lock()
      .expect("event log lock")
      .iter()
      .filter(|rec| rec.ids.iter().any(|id| id.as_str() == subject))
      .map(|rec| rec.kind)
      .collect()
  }
}

/// An ephemeral loopback bind address (`127.0.0.1:0`).
fn loopback_ephemeral() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

/// Spawn a fixture node at `bind` (an ephemeral `:0` for a fresh node, or a
/// concrete addr for a restart) with `timing`.
async fn build_node<R>(
  id: &str,
  bind: SocketAddr,
  timing: &ClusterTiming,
) -> serf_reactor::Result<Node<R>>
where
  R: Runtime,
{
  Serf::<SmolStr, SocketAddr, R>::tcp(
    timing.transport_opts(id, bind),
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

/// Attach a detached collector that drains `serf`'s event stream into `log`,
/// recording every member event (kind + named ids). The collector holds only the
/// stream, so a later kill (last-handle drop) still tears the node down while the
/// log persists.
fn attach_collector<R>(serf: &Node<R>, log: Arc<Mutex<Vec<MemberRec>>>)
where
  R: Runtime,
{
  let mut stream = serf.events();
  R::spawn_detach(async move {
    while let Some(ev) = stream.next().await {
      if let Event::Member(me) = ev {
        let ids = me
          .members()
          .iter()
          .map(|m| m.node().id_ref().clone())
          .collect();
        log.lock().expect("event log lock").push(MemberRec {
          kind: me.kind(),
          ids,
        });
      }
    }
  });
}
