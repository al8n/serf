//! Reusable multi-node fixture for the compio real-node serf driver, shared by
//! the scenario bodies in the sibling test binaries.
//!
//! A [`Cluster`] spins up N ephemeral loopback TCP nodes, joins every non-seed
//! node to the first (a star), and waits for the whole cluster to converge. Each
//! node runs a detached collector that drains its event stream into a shared log;
//! the log is a separate `Rc`, so it survives an abrupt
//! [`kill_abrupt`](Cluster::kill_abrupt) (which drops the node's last handle) and
//! later assertions read an ordered per-node member-event history.
//!
//! compio is thread-per-core and `!Send`, so the fixture is `Rc`/`RefCell`-based
//! and every node, collector, and assertion runs on the one runtime thread.
//!
//! The nodes run fast SWIM failure-detection timing (the transport probe / gossip
//! / suspicion overrides), so an abruptly-killed peer is detected as Failed in
//! well under a second. [`ClusterTiming`] carries both those memberlist knobs and
//! the serf-level reaper windows.

use core::time::Duration;
use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use futures_util::StreamExt;
use memberlist_proto::MaybeResolved;
use serf_compio::{
  FirstAddrResolver, RuntimeOptions, Serf, SocketAddrResolver, TcpTransport, TcpTransportOptions,
  VoidDelegate, gossip_rng,
};
use serf_proto::{
  event::{Event, MemberEventKind},
  members::MemberStatus,
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

/// A compio TCP node handle.
pub type Node = Serf<SmolStr>;

/// Wall-clock ceiling for every fixture poll loop, so a convergence or detection
/// regression surfaces as a bounded timeout rather than a hang.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll granularity for the fixture's await loops.
const POLL_STEP: Duration = Duration::from_millis(20);

/// An ephemeral loopback bind address (`127.0.0.1:0`).
pub fn loopback_ephemeral() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

/// Failure-detection and reap timing shared by every node in a fixture cluster.
///
/// The probe / gossip / suspicion knobs tune the memberlist SWIM layer (carried on
/// the transport options); the reap / reconnect knobs tune the serf reaper (carried
/// on the serf `Options`). [`fast`](Self::fast) yields CI-speed values that detect
/// an abrupt kill in sub-second time and reap the failed member shortly after
/// (short reconnect timeout), and drop a gracefully-left member shortly after its
/// Leave (short tombstone timeout). A test that wants to OBSERVE a member sitting
/// in a Failed or Left state raises the matching window with
/// [`with_reconnect_timeout`](Self::with_reconnect_timeout) /
/// [`with_tombstone_timeout`](Self::with_tombstone_timeout).
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
  leave_propagate_delay: Duration,
}

impl ClusterTiming {
  /// CI-speed timing: sub-second SWIM failure detection on loopback with the
  /// failed member reaped shortly after.
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
      // Short enough to keep a graceful leave sub-second, long enough to give
      // in-flight probes a gossip cycle to observe the leave intent.
      leave_propagate_delay: Duration::from_millis(100),
    }
  }

  /// Override the failed-member retention window — the age at which the reaper
  /// removes a Failed member. Raise it beyond the test window to HOLD a failed
  /// peer in the Failed view instead of reaping it.
  #[must_use]
  pub fn with_reconnect_timeout(mut self, v: Duration) -> Self {
    self.reconnect_timeout = v;
    self
  }

  /// Override the tombstone timeout — the age at which the reaper removes a
  /// gracefully-Left member. Raise it beyond the test window to HOLD a left peer
  /// in the tombstone view.
  #[must_use]
  pub fn with_tombstone_timeout(mut self, v: Duration) -> Self {
    self.tombstone_timeout = v;
    self
  }

  /// The serf `Options` every fixture node is built with.
  pub fn serf_opts(&self) -> SerfOptions {
    SerfOptions::new()
      .with_reap_interval(self.reap_interval)
      .with_reconnect_interval(self.reconnect_interval)
      .with_reconnect_timeout(self.reconnect_timeout)
      .with_tombstone_timeout(self.tombstone_timeout)
      .with_leave_propagate_delay(self.leave_propagate_delay)
  }

  /// Apply the memberlist SWIM knobs to a fixture node's transport options.
  pub fn apply(
    &self,
    opts: TcpTransportOptions<SmolStr, SocketAddr>,
  ) -> TcpTransportOptions<SmolStr, SocketAddr> {
    opts
      .with_probe_interval(self.probe_interval)
      .with_probe_timeout(self.probe_timeout)
      .with_gossip_interval(self.gossip_interval)
      .with_suspicion_mult(self.suspicion_mult)
  }
}

/// One observed member event: its kind and the member ids it names.
struct MemberRec {
  kind: MemberEventKind,
  ids: Vec<SmolStr>,
}

/// The shared, handle-independent event log a node's collector appends to.
type EventLog = Rc<RefCell<Vec<MemberRec>>>;

/// One cluster node: its stable id, the live handle (taken while killed), and the
/// event log the collector drains into.
struct NodeSlot {
  id: SmolStr,
  serf: Option<Node>,
  log: EventLog,
}

/// A live multi-node loopback cluster with per-node member-event logs.
pub struct Cluster {
  slots: Vec<NodeSlot>,
}

impl Cluster {
  /// Spawn `ids.len()` ephemeral loopback nodes with `timing`, attach a per-node
  /// event collector, join every non-seed node to the first (a star), and wait
  /// for the whole cluster to converge.
  pub async fn spawn(ids: &[&str], timing: ClusterTiming) -> Self {
    let mut slots = Vec::with_capacity(ids.len());
    for id in ids {
      let serf = build_node(id, &timing).await.expect("spawn serf tcp node");
      let log: EventLog = Rc::new(RefCell::new(Vec::new()));
      // Attach the collector before the handle moves into the slot, so no member
      // event can slip past between construction and the first join.
      attach_collector(&serf, log.clone());
      slots.push(NodeSlot {
        id: SmolStr::new(*id),
        serf: Some(serf),
        log,
      });
    }
    let cluster = Self { slots };
    let seed = cluster.node(0).advertise_address();
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
  pub fn node(&self, i: usize) -> &Node {
    self.slots[i].serf.as_ref().expect("node slot is live")
  }

  /// The stable id of node `i`.
  pub fn id(&self, i: usize) -> SmolStr {
    self.slots[i].id.clone()
  }

  /// Abruptly kill node `i`: shut its handle down without a leave. A shutdown
  /// sends no farewell, so peers detect a probe-timeout Failed rather than a
  /// graceful Leave. The slot's id and event log are retained for later
  /// assertions.
  pub async fn kill_abrupt(&mut self, i: usize) {
    let serf = self.slots[i].serf.take().expect("node slot is live");
    serf.shutdown().await.expect("node shuts down");
  }

  /// Gracefully leave node `i`, then release its slot. `leave()` resolves only
  /// once the machine's `LeftCluster` fires, so a successful return proves the
  /// graceful-leave chain completed and the farewell reached the wire before the
  /// teardown that follows.
  pub async fn leave_graceful(&mut self, i: usize) {
    let serf = self.slots[i].serf.take().expect("node slot is live");
    serf.leave().await.expect("node leaves gracefully");
    serf.shutdown().await.expect("node shuts down");
  }

  /// Poll every live node until each reports exactly `expect` members.
  pub async fn converge(&self, expect: usize) {
    compio::time::timeout(POLL_TIMEOUT, async {
      loop {
        if self
          .slots
          .iter()
          .filter_map(|s| s.serf.as_ref())
          .all(|n| n.num_members() == expect)
        {
          break;
        }
        compio::time::sleep(POLL_STEP).await;
      }
    })
    .await
    .expect("cluster converges to the expected member count");
  }

  /// Poll until `observer`'s membership view holds `subject` with `status`.
  pub async fn await_member_status(&self, observer: usize, subject: &str, status: MemberStatus) {
    compio::time::timeout(POLL_TIMEOUT, async {
      loop {
        if self
          .node(observer)
          .members()
          .iter()
          .any(|m| m.node().id_ref().as_str() == subject && m.status() == status)
        {
          break;
        }
        compio::time::sleep(POLL_STEP).await;
      }
    })
    .await
    .unwrap_or_else(|_| panic!("node {observer} never holds {subject:?} as {status:?}"));
  }

  /// Poll until `observer`'s log records a member event of `kind` naming
  /// `subject`.
  pub async fn await_member_event(&self, observer: usize, subject: &str, kind: MemberEventKind) {
    compio::time::timeout(POLL_TIMEOUT, async {
      loop {
        if self.member_event_kinds(observer, subject).contains(&kind) {
          break;
        }
        compio::time::sleep(POLL_STEP).await;
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

  /// Poll until the ordered member-event kinds `observer` recorded about
  /// `subject` are at least as long as `expected`, then assert exact equality.
  /// Polling first lets a still-in-flight event land; the final assert then
  /// catches a wrong, missing, or extra event.
  pub async fn assert_member_events(
    &self,
    observer: usize,
    subject: &str,
    expected: &[MemberEventKind],
  ) {
    // Ignoring Err: a poll timeout here just means fewer events than expected
    // arrived; the assert_eq below reports the precise sequence mismatch.
    let _ = compio::time::timeout(POLL_TIMEOUT, async {
      loop {
        if self.member_event_kinds(observer, subject).len() >= expected.len() {
          break;
        }
        compio::time::sleep(POLL_STEP).await;
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
      .borrow()
      .iter()
      .filter(|rec| rec.ids.iter().any(|id| id.as_str() == subject))
      .map(|rec| rec.kind)
      .collect()
  }
}

/// Spawn a fixture node on an ephemeral loopback port with `timing`'s memberlist
/// SWIM knobs and serf reaper windows.
async fn build_node(id: &str, timing: &ClusterTiming) -> serf_compio::Result<Node> {
  let opts = timing.apply(
    TcpTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral())),
  );
  Serf::new::<TcpTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    timing.serf_opts(),
    gossip_rng().expect("seed gossip rng"),
    None,
    None,
    None,
    #[cfg(encryption)]
    Rc::new(serf_compio::VoidKeyringDelegate),
  )
  .await
}

/// Attach a detached collector that drains `serf`'s event stream into `log`,
/// recording every member event (kind + named ids). The collector holds only the
/// stream, so a later kill (last-handle drop) still tears the node down while the
/// log persists.
fn attach_collector(serf: &Node, log: EventLog) {
  let mut stream = serf.events();
  compio::runtime::spawn(async move {
    while let Some(ev) = stream.next().await {
      if let Event::Member(me) = ev {
        let ids = me
          .members()
          .iter()
          .map(|m| m.node().id_ref().clone())
          .collect();
        log.borrow_mut().push(MemberRec {
          kind: me.kind(),
          ids,
        });
      }
    }
  })
  .detach();
}
