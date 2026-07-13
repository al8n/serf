//! The `Serf` handle's construction, cloning, read surface, and membership
//! commands over two real embassy-net stacks.
//!
//! The behavioral gates here: the default-RNG constructor stands a node up from
//! platform entropy (no injected seeds), a cloned handle is the SAME node rather
//! than a detached copy, every read accessor reports the engine's live view (not
//! a stale snapshot), and `set_tags` / `force_leave` actually reach the peer.

// nested `if let X = ev { if cond }` kept for readability, as in the crate roots.
#![allow(clippy::collapsible_if)]

mod support;

use core::{net::SocketAddr, time::Duration as CoreDuration};

use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{
  Bytes, EndpointOptions, Member, MemberStatus, Options, QueryParams, ReconnectDelegate, Serf,
  SerfOptions, SerfState, SocketAddrResolver, Tags, TransformOptions, now,
};
use smol_str::SmolStr;

use support::cluster::{
  NodeBufs, POOL, addr, build_node, build_sockets, build_stack, devices, drive, join_and_converge,
};

/// A reconnect policy that pins every member to a fixed timeout, so installing it
/// is observable as "the delegate the engine now consults".
struct FixedReconnect(CoreDuration);

impl ReconnectDelegate<SmolStr, SocketAddr> for FixedReconnect {
  fn reconnect_timeout(
    &self,
    _member: &Member<SmolStr, SocketAddr>,
    _timeout: CoreDuration,
  ) -> CoreDuration {
    self.0
  }
}

/// The default-RNG constructor stands up a live node from platform entropy alone:
/// no injected seeds, and the resulting node is `Alive`, a member of its own
/// cluster, and advertising the address it was configured with.
#[test]
fn the_default_rng_constructor_stands_up_a_live_node() {
  let (dev_a, _dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);

  let mut bufs_a = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);

  let (serf, _runner) = block_on(Serf::<SmolStr, SocketAddr>::new::<_, POOL>(
    Options::new(),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("solo"), addr(1, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    udp_a,
    tcp_a,
    now(),
  ))
  .expect("entropy-seeded construction over a routable address");

  assert_eq!(serf.state(), SerfState::Alive);
  assert_eq!(serf.local_id(), SmolStr::new("solo"));
  assert_eq!(serf.advertise_address(), addr(1, 7946));
  assert_eq!(
    serf.num_members(),
    1,
    "a fresh node is the only member of its own view"
  );
  assert!(!serf.is_shutdown());

  // Silence the unused stack runner without driving it: no I/O is required for
  // construction, which is the property under test.
  let _ = &mut net_a;
}

/// A cloned handle is the SAME node, not a detached copy: both handles observe one
/// membership view, and a shutdown latched through the clone is visible on the
/// original.
#[test]
fn a_cloned_handle_shares_one_node() {
  let (dev_a, _dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);

  let mut bufs_a = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (serf, _runner) = build_node(udp_a, tcp_a, "a", 1, now(), TransformOptions::default());

  let clone = serf.clone();
  assert_eq!(clone.local_id(), serf.local_id());
  assert_eq!(clone.advertise_address(), serf.advertise_address());
  assert_eq!(clone.num_members(), serf.num_members());

  // The latch is shared state, not per-handle state.
  assert!(!serf.is_shutdown() && !clone.is_shutdown());
  clone.shutdown();
  assert!(
    serf.is_shutdown(),
    "a shutdown through the clone must stop the original handle's node"
  );

  let _ = &mut net_a;
}

/// The read accessors report the engine's LIVE view: after a converged join the
/// clocks have advanced, the reliable plane holds a listener and has consumed pool
/// slots, and the member list agrees with the counts.
#[test]
fn the_read_surface_reports_the_live_engine_view() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now_ = now();
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now_, TransformOptions::default());
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now_, TransformOptions::default());

  // Before the run loop turns, nothing has been shed and no clock has moved.
  assert_eq!(a.events_dropped(), 0);
  assert_eq!(a.coalesced_user_events_dropped(), 0);
  assert_eq!(a.coalesced_member_events_dropped(), 0);
  assert_eq!(a.event_time(), 0);
  assert_eq!(a.query_time(), 0);

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      // A user event and a query advance the two application clocks; the member
      // clock advanced with the join itself.
      a.user_event("evt", Bytes::from_static(b"p"), false)
        .expect("user_event from a running node");
      a.query(
        "q",
        Bytes::from_static(b"p"),
        QueryParams {
          timeout: CoreDuration::from_secs(1),
          ..Default::default()
        },
      )
      .expect("query from a running node");

      // The member view is the source of truth for the counts.
      let members = a.members();
      assert_eq!(members.len(), a.num_members());
      assert_eq!(members.len(), 2);
      let mut ids: std::vec::Vec<SmolStr> =
        members.iter().map(|m| m.node().id_ref().clone()).collect();
      ids.sort();
      assert_eq!(ids, ["a", "b"].map(SmolStr::new));

      assert!(
        a.member_time() > 0,
        "the join advanced the membership clock"
      );
      assert!(
        a.event_time() > 0,
        "the user event advanced the event clock"
      );
      assert!(a.query_time() > 0, "the query advanced the query clock");

      // The reliable plane: a dedicated listener, and the join consumed slots from
      // the pool it was seeded with.
      assert!(a.listener_present(), "the listener slot must be armed");
      assert!(
        a.pool_free_count() < POOL,
        "the listener plus the exchange must hold pooled slots"
      );
      assert!(a.closing_count() <= POOL);
      assert!(a.half_closed_count() <= POOL);
      assert_eq!(
        a.pending_join_count(),
        0,
        "the converged join left no pending seed"
      );
      assert_eq!(a.pending_dial_count(), 0);
      // A accepted B's inbound push/pull exchange.
      assert!(
        a.accepted_inbound_count() >= 1,
        "A must have accepted B's join exchange"
      );
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}

/// New tags set on A reach B's membership view: `set_tags` re-advertises the local
/// member rather than only mutating a local copy.
#[test]
fn set_tags_reaches_the_peers_member_view() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now_ = now();
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now_, TransformOptions::default());
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now_, TransformOptions::default());

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      let tags: Tags = [("role", "leader"), ("dc", "eu-west")]
        .into_iter()
        .collect();
      a.set_tags(tags).expect("set_tags from a running node");

      // B's view of A must carry the new tags.
      loop {
        let seen = b.members().into_iter().find(|m| m.node().id_ref() == "a");
        if let Some(m) = seen {
          if m.tags().0.get("role").map(SmolStr::as_str) == Some("leader") {
            assert_eq!(
              m.tags().0.get("dc").map(SmolStr::as_str),
              Some("eu-west"),
              "every tag in the set must propagate, not just the first"
            );
            return;
          }
        }
        Timer::after(Duration::from_millis(10)).await;
      }
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}

/// `force_leave` on A removes B from A's own view: the operator-driven removal
/// marks the target as no longer alive rather than being a silent no-op.
#[test]
fn force_leave_removes_the_target_from_the_local_view() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now_ = now();
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now_, TransformOptions::default());
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now_, TransformOptions::default());

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      // Installing a reconnect policy is accepted on a running node and does not
      // disturb the converged view.
      a.set_reconnect_delegate(Some(std::boxed::Box::new(FixedReconnect(
        CoreDuration::from_secs(30),
      ))));
      assert_eq!(a.num_members(), 2);

      a.force_leave(SmolStr::new("b"), false)
        .expect("force_leave from a running node");

      // A's view of B must stop being Alive.
      loop {
        let seen = a.members().into_iter().find(|m| m.node().id_ref() == "b");
        match seen {
          None => return,
          Some(m) if m.status() != MemberStatus::Alive => return,
          _ => Timer::after(Duration::from_millis(10)).await,
        }
      }
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });

  // And clearing the override afterwards is equally accepted.
  a.set_reconnect_delegate(None);
}
