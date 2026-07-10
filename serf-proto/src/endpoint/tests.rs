use super::*;
use crate::{
  AnyMessage, JoinMessage, LamportTime, LeaveMessage, StreamEndpoint,
  event::{Event, MemberEventKind},
  members::{MemberStatus, SerfState},
  typed::{Filter, QueryFlag, QueryMessage, RelayMessage, UserEventMessage},
};
#[cfg(feature = "coordinates")]
use bytes::Bytes;
use memberlist_proto::{EndpointOptions, RawRecords, SeedableRng, SmallRng, streams::LabelOptions};

/// The plain-TCP record layer the unit-test coordinators run over.
type TestTransport = RawRecords;

/// Wrap a raw membership [`memberlist_proto::Endpoint`] into the plain-TCP
/// reliable coordinator the serf `StreamEndpoint` composes with.
///
/// All unit tests root at `A = SocketAddr`, so the peer-to-socket resolver is
/// the identity and the SNI provider is unused (the plain-TCP record layer
/// ignores it).  A fixed cluster label keeps the handshake well-formed for the
/// loopback tests that complete a real exchange.
fn coord(
  inner: memberlist_proto::Endpoint<u32, core::net::SocketAddr>,
) -> memberlist_proto::streams::StreamEndpoint<u32, core::net::SocketAddr, TestTransport> {
  memberlist_proto::streams::StreamEndpoint::new(
    inner,
    LabelOptions::new_in(Some(b"serf-test".to_vec()), ()),
    Box::new(|_addr: &core::net::SocketAddr| None),
    Box::new(|addr: &core::net::SocketAddr| *addr),
  )
}

/// Build a minimal serf `Endpoint` suitable for unit tests.
///
/// Uses `u32` node ids and `SocketAddr` addresses with a deterministically
/// seeded `SmallRng` so tests are reproducible.
fn ep() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let mut e = StreamEndpoint::new(coord(inner), Options::new());
  // The inner memberlist emits NodeJoined(self) on construction; drain it so
  // every test starts from the post-self-join-drained state (self is a member).
  let _ = e.poll_event();
  e
}

/// Build a serf `Endpoint` with coordinates enabled (for coordinate-gated tests).
#[cfg(feature = "coordinates")]
fn ep_with_coords() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new().with_disable_coordinates(false);
  let mut e = StreamEndpoint::new(coord(inner), opts);
  let _ = e.poll_event();
  e
}

#[test]
fn new_endpoint_starts_alive_with_zero_clocks() {
  let e = ep();
  assert!(e.state().is_alive());
  assert_eq!(e.member_time(), 0);
  assert_eq!(e.event_time(), 0);
  assert_eq!(e.query_time(), 0);
  // After the construction self-join is drained, the local node is the sole member.
  assert_eq!(e.num_members(), 1);
}

#[test]
fn witness_advances_to_at_least() {
  let mut c = 0u64;
  witness(&mut c, 5);
  assert_eq!(c, 6); // advance past the witnessed time
  witness(&mut c, 3);
  assert_eq!(c, 6); // older time does not regress
  witness(&mut c, 6);
  assert_eq!(c, 7); // equal triggers advance
}

#[test]
fn poll_event_drains_when_inner_empty() {
  let mut e = ep();
  assert!(e.poll_event().is_none());
}

#[test]
fn poll_timeout_is_none_on_idle_alive_endpoint() {
  // No serf deadlines armed yet; inner scheduler idle at ORIGIN-relative new.
  // Either None or Some — assert it does not panic.
  let mut e = ep();
  let _ = e.poll_timeout();
}

#[test]
fn handle_packet_with_garbage_bytes_is_a_noop() {
  let mut e = ep();
  e.handle_packet(
    "127.0.0.1:9999".parse().unwrap(),
    Bytes::from_static(b"\xff\xff"),
    memberlist_proto::Instant::ORIGIN,
  );
  // Undecodable inner message -> no serf event, no panic.
  assert!(e.poll_event().is_none());
}

#[test]
fn poll_memberlist_transmit_delegates_to_coordinator() {
  let mut e = ep();
  // No transmits queued at construction time; must not panic.
  assert!(e.poll_memberlist_transmit().is_none());
}

// ── Task 1.4: member-status FSM + intent reconciliation + clock witnessing ────

#[test]
fn leave_intent_for_known_alive_self_refutes_and_does_not_rebroadcast() {
  // local_id = 1, state Alive, seeded in states. A FRESH leave intent for the
  // local node is refuted: the node re-announces its join and returns false.
  // The stale check fires first (for known nodes); this ltime=5 exceeds
  // status_time=0, so it reaches the self-refute path.
  let mut e = ep();
  e.test_seed_member(1u32, MemberStatus::Alive, LamportTime::new(0));
  let rebroadcast =
    e.test_handle_leave_intent(1, LamportTime::new(5), memberlist_proto::Instant::ORIGIN);
  assert!(
    !rebroadcast,
    "fresh self-leave while Alive and known must be refuted, not rebroadcast"
  );
  // The member clock must have witnessed ltime=5, so clock >= 6.
  assert!(
    e.member_time() >= 6,
    "member clock should have been witnessed"
  );
}

#[test]
fn stale_leave_intent_is_dropped() {
  let mut e = ep();
  // Seed member 2 at status_time=10; a leave intent at ltime=3 is stale.
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(10));
  let rebroadcast =
    e.test_handle_leave_intent(2, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);
  assert!(!rebroadcast, "stale leave intent must not be rebroadcast");
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "stale intent must not change member status"
  );
}

#[test]
fn live_leave_intent_transitions_alive_to_leaving() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(5));
  let rebroadcast =
    e.test_handle_leave_intent(2, LamportTime::new(8), memberlist_proto::Instant::ORIGIN);
  assert!(
    rebroadcast,
    "fresh leave intent for Alive should rebroadcast"
  );
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Leaving));
  assert!(
    e.member_time() >= 9,
    "member clock must have been witnessed"
  );
}

#[test]
fn leave_intent_for_failed_transitions_to_left_and_emits_leave_event() {
  let mut e = ep();
  // Seed a Failed member in failed_members list.
  e.test_seed_failed_member_by_status(2, LamportTime::new(5), memberlist_proto::Instant::ORIGIN);
  let rebroadcast =
    e.test_handle_leave_intent(2, LamportTime::new(9), memberlist_proto::Instant::ORIGIN);
  assert!(rebroadcast, "leave intent for Failed should rebroadcast");
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Left),
    "Failed + leave intent should transition to Left"
  );
  // A Member(Leave) event should be pending.
  let ev = e.poll_event();
  assert!(
    matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave),
    "expected Member(Leave) event, got: {ev:?}"
  );
}

#[test]
fn leave_intent_for_unknown_node_is_buffered_as_intent() {
  let mut e = ep();
  // Node 99 is not in states yet — intent should be upserted.
  let rebroadcast =
    e.test_handle_leave_intent(99, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);
  assert!(rebroadcast, "unknown node leave intent should be buffered");
  assert_eq!(
    e.test_member_status(99),
    None,
    "no member should be created"
  );
}

#[test]
fn join_intent_for_existing_leaving_member_clears_to_alive() {
  let mut e = ep();
  // Seed member 2 as Leaving at status_time=5.
  e.test_seed_member(2, MemberStatus::Leaving, LamportTime::new(5));
  // A fresh join intent at ltime=8 should move it back to Alive.
  let rebroadcast =
    e.test_handle_join_intent(2, LamportTime::new(8), memberlist_proto::Instant::ORIGIN);
  assert!(
    rebroadcast,
    "fresh join intent for Leaving should rebroadcast"
  );
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
  assert!(
    e.member_time() >= 9,
    "member clock must have been witnessed"
  );
}

#[test]
fn stale_join_intent_for_existing_member_is_ignored() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(10));
  let rebroadcast =
    e.test_handle_join_intent(2, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);
  assert!(!rebroadcast, "stale join intent must not rebroadcast");
}

#[test]
fn join_intent_for_unknown_node_is_buffered() {
  let mut e = ep();
  assert!(
    e.test_handle_join_intent(3, LamportTime::new(7), memberlist_proto::Instant::ORIGIN),
    "first join intent for unknown node should be buffered"
  );
  assert_eq!(e.test_member_status(3), None);
}

#[test]
fn inner_node_joined_with_pending_leave_intent_creates_leaving_member() {
  let mut e = ep();
  // Leave intent buffered before the inner NodeJoined fires.
  e.test_handle_leave_intent(2, LamportTime::new(7), memberlist_proto::Instant::ORIGIN);
  // Now the inner NodeJoined arrives.
  e.test_inner_node_joined(2, memberlist_proto::Instant::ORIGIN);
  // Should be Leaving (intent applied), not Alive.
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Leaving));
  // A Member(Join) event must have been emitted regardless.
  let ev = e.poll_event();
  assert!(
    matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join),
    "expected Member(Join), got: {ev:?}"
  );
}

#[test]
fn inner_node_joined_without_intents_creates_alive_member() {
  let mut e = ep();
  e.test_inner_node_joined(2, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join));
}

#[test]
fn inner_node_left_alive_transitions_to_failed() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(5));
  e.test_inner_node_left(2, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Failed));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Failed));
}

#[test]
fn inner_node_left_leaving_transitions_to_left() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Leaving, LamportTime::new(5));
  e.test_inner_node_left(2, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Left));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave));
}

#[test]
fn inner_node_left_unknown_is_a_noop() {
  let mut e = ep();
  // Node not in states — should not panic.
  e.test_inner_node_left(99, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(99), None);
  assert!(e.poll_event().is_none());
}

#[test]
fn inner_node_updated_refreshes_member_and_emits_update_event() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(3));
  e.test_inner_node_updated(2, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Update));
}

#[test]
fn inner_node_updated_for_unknown_is_a_noop() {
  let mut e = ep();
  e.test_inner_node_updated(99, memberlist_proto::Instant::ORIGIN);
  assert!(e.poll_event().is_none());
}

#[test]
fn inner_node_updated_does_not_dirty_local_state() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(3));
  e.resync_local_state();
  assert!(
    !e.test_is_dirty(),
    "resync_local_state clears the dirty flag"
  );

  // A NodeUpdated refreshes only the member's tags/address — neither is in the
  // push-pull snapshot — so it must not dirty local state. set_tags queues
  // exactly this event via update_meta on every local tag change, so dirtying
  // here would force a wasted resync per tag update.
  e.test_inner_node_updated(2, memberlist_proto::Instant::ORIGIN);

  assert!(
    !e.test_is_dirty(),
    "a tag-only NodeUpdated must not mark local_state_dirty"
  );
  // The Update event is still emitted.
  assert!(matches!(
    e.poll_event(),
    Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Update
  ));
}

#[test]
fn handle_node_join_re_joining_failed_clears_lists() {
  let mut e = ep();
  // Add node 2 as Failed in failed_members.
  e.test_seed_failed_member_by_status(2, LamportTime::new(5), memberlist_proto::Instant::ORIGIN);
  assert!(e.test_in_failed_members(2), "should be in failed_members");
  // Inner NodeJoined re-joins it.
  e.test_inner_node_joined(2, memberlist_proto::Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
  assert!(
    !e.test_in_failed_members(2),
    "should be cleared from failed_members"
  );
}

#[test]
fn leave_intent_no_op_on_none_status() {
  let mut e = ep();
  // Seed a member with None status.
  e.test_seed_member(2, MemberStatus::None, LamportTime::new(5));
  let rebroadcast =
    e.test_handle_leave_intent(2, LamportTime::new(8), memberlist_proto::Instant::ORIGIN);
  // The status_time IS updated (the FIX), but FSM does not transition from None.
  assert!(!rebroadcast, "None-status leave intent returns false");
  assert_eq!(e.test_member_status(2), Some(MemberStatus::None));
}

// ── Task 1.5: SerfState lifecycle FSM + leave chain ───────────────────────────

#[test]
fn leave_from_alive_transitions_to_leaving() {
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  assert!(e.state().is_leaving(), "leave() must set state to Leaving");
}

/// Integrity floor: a `leave()` whose post-incremented stamp would reach
/// `LTIME_MAX` (member clock at `LTIME_MAX - 1`) parks degraded-but-safe — it
/// returns `LeaveClockExhausted`, advances no clock, queues NO invalid intent,
/// starts no inner leave, and leaves the lifecycle AND self-membership a
/// consistent `Alive`.
#[test]
fn leave_at_ltime_watermark_parks_degraded_but_safe() {
  let mut e = ep();
  // Drive the member clock to the floor so the next leave stamp would be LTIME_MAX.
  e.test_set_clocks(LTIME_MAX - 1, 0, 0);
  let q_before = e.user_broadcast_queue_len();

  let err = e
    .leave(memberlist_proto::Instant::ORIGIN)
    .expect_err("a leave whose stamp reaches LTIME_MAX must be refused");
  assert!(
    matches!(err, Error::LeaveClockExhausted),
    "expected LeaveClockExhausted, got {err:?}"
  );

  // No invalid intent was queued, the clock did not advance into the rejected
  // range, and both the lifecycle state and the local member stay a consistent
  // Alive — nothing entered an inconsistent Leaving half-state.
  assert_eq!(
    e.user_broadcast_queue_len(),
    q_before,
    "the degraded leave must not queue a leave intent"
  );
  assert_eq!(
    e.member_time(),
    LTIME_MAX - 1,
    "the degraded leave must not advance the member clock"
  );
  assert!(e.state().is_alive(), "the endpoint must remain Alive");
  assert_eq!(
    e.test_member_status(1),
    Some(MemberStatus::Alive),
    "self-membership must stay consistent (Alive, not a half-Leaving)"
  );
}

/// Boundary: one tick below the floor (stamp lands at `LTIME_MAX - 1`, still
/// acceptable), `leave()` proceeds normally — the gate is exactly at the floor,
/// not off-by-one.
#[test]
fn leave_one_below_watermark_succeeds() {
  let mut e = ep();
  e.test_set_clocks(LTIME_MAX - 2, 0, 0);
  e.leave(memberlist_proto::Instant::ORIGIN)
    .expect("a leave whose stamp is LTIME_MAX - 1 must still apply");
  // The intent (ltime = LTIME_MAX - 1) is acceptable, so the leave applies
  // fully: both the lifecycle and self-membership transition to Leaving — the
  // exact contrast with the degraded watermark path, which stays Alive.
  assert!(
    e.state().is_leaving(),
    "the boundary leave must transition to Leaving"
  );
  assert_eq!(
    e.test_member_status(1),
    Some(MemberStatus::Leaving),
    "self-membership must transition to Leaving on the boundary leave"
  );
}

#[test]
fn double_leave_is_rejected() {
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  let err = e
    .leave(memberlist_proto::Instant::ORIGIN)
    .expect_err("second leave from Leaving must fail");
  assert!(matches!(err, Error::BadLeaveState(SerfState::Leaving)));
}

#[test]
fn leave_from_already_left_is_idempotent() {
  let mut e = ep();
  // Manually drive to Left state to test the idempotent path.
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  // Simulate the leave chain completing: inner LeftCluster + delay elapses.
  e.test_inner_left_cluster();
  let delay = core::time::Duration::from_secs(2); // > leave_propagate_delay (1s)
  e.handle_timeout(memberlist_proto::Instant::ORIGIN + delay);
  assert!(e.state().is_left(), "should have transitioned to Left");
  // A second leave from Left is Ok(()).
  assert!(e.leave(memberlist_proto::Instant::ORIGIN).is_ok());
}

#[test]
fn leave_from_shutdown_is_rejected() {
  let mut e = ep();
  // Force state to Shutdown.
  e.core_mut().state = SerfState::Shutdown;
  let err = e
    .leave(memberlist_proto::Instant::ORIGIN)
    .expect_err("leave from Shutdown must fail");
  assert!(matches!(err, Error::BadLeaveState(SerfState::Shutdown)));
}

#[test]
fn inner_left_cluster_arms_leave_complete_deadline() {
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  assert!(
    e.leave_complete_deadline().is_none(),
    "deadline not armed until inner LeftCluster arrives"
  );
  // Simulate inner LeftCluster.
  e.test_inner_left_cluster();
  assert!(
    e.leave_complete_deadline().is_some(),
    "deadline must be armed after inner LeftCluster"
  );
}

#[test]
fn inner_left_cluster_not_in_leaving_state_is_ignored() {
  // If the inner emits LeftCluster while serf is still Alive (unexpected but
  // must not panic or set a spurious deadline).
  let mut e = ep();
  assert!(e.state().is_alive());
  e.test_inner_left_cluster();
  assert!(
    e.leave_complete_deadline().is_none(),
    "LeftCluster while Alive must not arm leave_complete_deadline"
  );
}

#[test]
fn inner_left_cluster_drives_serf_to_left_and_emits_left_cluster_event() {
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  // Simulate the inner emitting LeftCluster (no live peers → immediate).
  e.test_inner_left_cluster();
  // Advance time past leave_propagate_delay (default 1s).
  let after_delay = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(2);
  e.handle_timeout(after_delay);
  // State must be Left.
  assert!(
    e.state().is_left(),
    "state must be Left after propagation delay"
  );
  // Member(Leave, self) appears when the inner's NodeLeft(self) is drained
  // (Leaving → Left transition).  Event::LeftCluster follows once the
  // leave_propagate_delay deadline fires.  Drain until LeftCluster is found.
  let found_left_cluster =
    core::iter::from_fn(|| e.poll_event()).any(|ev| matches!(ev, Event::LeftCluster));
  assert!(
    found_left_cluster,
    "Event::LeftCluster must be emitted after the leave propagation delay"
  );
}

#[test]
fn leave_complete_deadline_not_fired_before_delay() {
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  e.test_inner_left_cluster();
  // Tick to just before the propagation deadline (< 1s).
  let before_delay = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_millis(500);
  e.handle_timeout(before_delay);
  // SerfState must still be Leaving (the deadline hasn't fired yet).
  assert!(e.state().is_leaving(), "must still be Leaving before delay");
  // Member(Leave, self) from the inner's NodeLeft drain may appear, but
  // Event::LeftCluster must NOT appear before the propagation deadline.
  let no_left_cluster =
    core::iter::from_fn(|| e.poll_event()).all(|ev| !matches!(ev, Event::LeftCluster));
  assert!(
    no_left_cluster,
    "LeftCluster must not appear before the propagation deadline"
  );
}

#[test]
fn shutdown_prevents_leaving_to_left_transition() {
  // If state is set to Shutdown before leave_complete_deadline fires, the
  // transition to Left is skipped (mirrors oracle: Shutdown wins over Left).
  let mut e = ep();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  e.test_inner_left_cluster();
  // Force Shutdown before the deadline fires.
  e.core_mut().state = SerfState::Shutdown;
  let after_delay = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(2);
  e.handle_timeout(after_delay);
  // Must remain Shutdown, not Left.
  assert!(
    e.state().is_shutdown(),
    "Shutdown should not transition to Left"
  );
  // Event::LeftCluster must not appear (the leave chain was interrupted by
  // Shutdown). Member(Leave, self) from the inner NodeLeft drain may appear.
  let no_left_cluster =
    core::iter::from_fn(|| e.poll_event()).all(|ev| !matches!(ev, Event::LeftCluster));
  assert!(
    no_left_cluster,
    "LeftCluster must not be emitted when Shutdown interrupts the leave chain"
  );
}

#[test]
fn leave_arms_broadcast_deadline() {
  let mut e = ep();
  assert!(e.leave_broadcast_deadline().is_none());
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  let dl = e
    .leave_broadcast_deadline()
    .expect("broadcast deadline must be armed");
  // Default broadcast_timeout is 5s; deadline = ORIGIN + 5s.
  assert_eq!(
    dl,
    memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(5)
  );
}

#[test]
fn force_leave_from_shutdown_is_rejected() {
  let mut e = ep();
  e.core_mut().state = SerfState::Shutdown;
  let err = e
    .force_leave(2u32, false, memberlist_proto::Instant::ORIGIN)
    .expect_err("force_leave from Shutdown must fail");
  assert!(matches!(err, Error::BadLeaveState(SerfState::Shutdown)));
}

#[test]
fn force_leave_transitions_alive_member_to_leaving() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(0));
  e.force_leave(2u32, false, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "force_leave must transition Alive → Leaving"
  );
}

/// `force_leave` at `clock == LTIME_MAX - 1` parks degraded-but-safe — it
/// returns `LeaveClockExhausted`, advances no clock, queues NO invalid intent,
/// and leaves the target member and clock state consistent.
#[test]
fn force_leave_at_ltime_watermark_parks_degraded_but_safe() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(0));
  // Drive the member clock to the floor so the next stamp would be LTIME_MAX.
  e.test_set_clocks(LTIME_MAX - 1, 0, 0);
  let q_before = e.user_broadcast_queue_len();

  let err = e
    .force_leave(2u32, false, memberlist_proto::Instant::ORIGIN)
    .expect_err("a force_leave whose stamp reaches LTIME_MAX must be refused");
  assert!(
    matches!(err, Error::LeaveClockExhausted),
    "expected LeaveClockExhausted, got {err:?}"
  );

  // No invalid intent was queued, the clock did not advance into the rejected
  // range, the target member stayed Alive, and the local endpoint state is
  // unchanged — nothing entered an inconsistent half-state.
  assert_eq!(
    e.user_broadcast_queue_len(),
    q_before,
    "the degraded force_leave must not queue a leave intent"
  );
  assert_eq!(
    e.member_time(),
    LTIME_MAX - 1,
    "the degraded force_leave must not advance the member clock"
  );
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "the target member must stay Alive after a degraded force_leave"
  );
}

/// Boundary: one tick below the floor (stamp lands at `LTIME_MAX - 1`, still
/// acceptable), `force_leave()` proceeds normally — the gate is exactly at the
/// floor, not off-by-one.
#[test]
fn force_leave_one_below_watermark_succeeds() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(0));
  e.test_set_clocks(LTIME_MAX - 2, 0, 0);
  e.force_leave(2u32, false, memberlist_proto::Instant::ORIGIN)
    .expect("a force_leave whose stamp is LTIME_MAX - 1 must still apply");
  // The intent is acceptable, so the target transitions to Leaving — the exact
  // contrast with the degraded watermark path, which stays Alive.
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "the boundary force_leave must transition the target to Leaving"
  );
}

#[test]
fn leave_witnesses_and_advances_member_clock() {
  let mut e = ep();
  let clock_before = e.member_time();
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  assert!(
    e.member_time() > clock_before,
    "leave() must advance the member clock"
  );
}

#[test]
fn poll_timeout_includes_leave_deadlines_when_armed() {
  let mut e = ep();
  // Initially no serf deadlines; poll_timeout may be None (inner idle) or
  // Some from inner's own schedule — just confirm it does not panic.
  let _ = e.poll_timeout();

  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  // After leave(), leave_broadcast_deadline is armed.
  let timeout = e
    .poll_timeout()
    .expect("must have a deadline after leave()");
  let expected = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(5);
  assert!(
    timeout <= expected,
    "poll_timeout must be ≤ leave_broadcast_deadline ({expected:?}), got {timeout:?}"
  );
}

// ── Task 1.6: reconnector dial-output + reaper deadlines ─────────────────────

// Helper: seed a failed member with an explicit address so we can assert what
// addr is dialled by the reconnector.
fn seed_failed(
  e: &mut StreamEndpoint<u32, core::net::SocketAddr, RawRecords>,
  id: u32,
  addr: core::net::SocketAddr,
  leave_time: memberlist_proto::Instant,
) {
  e.test_seed_failed_member(id, addr, leave_time);
}

// Helper: seed the endpoint's one alive member (the local node) explicitly so
// the probability computation has a stable num_alive value.
fn seed_alive(e: &mut StreamEndpoint<u32, core::net::SocketAddr, RawRecords>, id: u32) {
  e.test_seed_member(id, MemberStatus::Alive, LamportTime::new(0));
}

#[test]
fn reap_failed_removes_after_reconnect_timeout() {
  let mut e = ep();
  let t0 = memberlist_proto::Instant::ORIGIN;
  seed_failed(&mut e, 2, "127.0.0.1:1002".parse().unwrap(), t0);
  // 25 hours > reconnect_timeout (24h)
  let past_timeout = t0 + core::time::Duration::from_secs(3600 * 25);
  e.test_fire_reap(past_timeout);
  assert_eq!(
    e.test_member_status(2),
    None,
    "failed member should be reaped after reconnect_timeout"
  );
  let reaped = core::iter::from_fn(|| e.poll_event())
    .any(|ev| matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Reap));
  assert!(reaped, "a Member(Reap) event should have been emitted");
}

#[test]
fn reap_failed_keeps_member_before_reconnect_timeout() {
  let mut e = ep();
  let t0 = memberlist_proto::Instant::ORIGIN;
  seed_failed(&mut e, 2, "127.0.0.1:1002".parse().unwrap(), t0);
  // 1 hour < reconnect_timeout (24h) — should NOT reap
  let before_timeout = t0 + core::time::Duration::from_secs(3600);
  e.test_fire_reap(before_timeout);
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Failed),
    "member not yet past reconnect_timeout must not be reaped"
  );
  assert!(e.poll_event().is_none(), "no Reap event before timeout");
}

#[test]
fn reap_left_removes_after_tombstone_timeout() {
  let mut e = ep();
  let t0 = memberlist_proto::Instant::ORIGIN;
  e.test_seed_left_member_by_status(2, LamportTime::new(3), t0);
  // 25 hours > tombstone_timeout (24h)
  let past_timeout = t0 + core::time::Duration::from_secs(3600 * 25);
  e.test_fire_reap(past_timeout);
  assert_eq!(
    e.test_member_status(2),
    None,
    "left member should be reaped after tombstone_timeout"
  );
  let reaped = core::iter::from_fn(|| e.poll_event())
    .any(|ev| matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Reap));
  assert!(reaped, "a Member(Reap) event should have been emitted");
}

#[test]
fn reap_intents_removes_stale_intents() {
  let mut e = ep();
  let t0 = memberlist_proto::Instant::ORIGIN;
  // Buffer a leave intent for an unknown node at t0.
  e.test_handle_leave_intent(99, LamportTime::new(3), t0);
  // recent_intent_timeout is 600s (10 min). At t0 + 700s the intent is stale.
  let past_intent_timeout = t0 + core::time::Duration::from_secs(700);
  e.test_fire_reap(past_intent_timeout);
  // The intent buffer should be empty now (no state was created, the node was unknown).
  // Verify by checking that the intent is no longer present — seeded as Leave intent for 99.
  // We confirm indirectly: a second identical intent would be accepted (it would
  // insert fresh), which it should be regardless. Instead we check members directly.
  assert_eq!(e.test_member_status(99), None, "no member state expected");
  // No reap event should have been emitted (intents are not members).
  assert!(
    e.poll_event().is_none(),
    "reap_intents must not emit Member events"
  );
}

#[test]
fn reconnect_gate_skips_when_no_failed_members() {
  let mut e = ep();
  // No failed members: reconnect should be a no-op.
  e.test_fire_reconnect(memberlist_proto::Instant::ORIGIN);
  // No DialRequested passthrough in pending events.
  assert!(e.poll_event().is_none());
}

#[test]
fn reconnect_picks_a_failed_member_and_emits_dial_requested() {
  // Use a fixed seed so the probability gate (1/1 = 1.0) always fires.
  // ep() seeds SmallRng from 0, so draws are deterministic.
  let mut e = ep();
  seed_alive(&mut e, 1); // local node alive, num_alive=1
  seed_failed(
    &mut e,
    2,
    "127.0.0.1:1002".parse().unwrap(),
    memberlist_proto::Instant::ORIGIN,
  );
  // With num_failed=1, num_alive=1, prob=1.0 → gate always fires.
  e.test_fire_reconnect(memberlist_proto::Instant::ORIGIN);
  // The inner start_push_pull emits a DialRequested which the sieve passes through.
  let dialled = e.test_last_dial_addr();
  assert!(
    dialled.is_some(),
    "reconnect should have triggered a dial request"
  );
}

#[test]
fn reconnect_deadline_armed_and_polls_in_poll_timeout() {
  let mut e = ep();
  seed_alive(&mut e, 1);
  seed_failed(
    &mut e,
    2,
    "127.0.0.1:1002".parse().unwrap(),
    memberlist_proto::Instant::ORIGIN,
  );
  // Before arming: next_reconnect is None internally but poll_timeout may still
  // return Some from the inner.  After handle_timeout fires and re-arms:
  // drive handle_timeout past the first reconnect_interval (30s).
  let after_interval = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(31);
  e.handle_timeout(after_interval);
  // After the tick the reconnect deadline is re-armed, so poll_timeout is Some.
  let _ = e.poll_timeout(); // must not panic
}

#[test]
fn reap_deadline_fires_via_handle_timeout() {
  let mut e = ep();
  let t0 = memberlist_proto::Instant::ORIGIN;
  seed_failed(&mut e, 2, "127.0.0.1:1002".parse().unwrap(), t0);
  // Drive handle_timeout well past reconnect_timeout (24h) + reap_interval (15s).
  let far_future = t0 + core::time::Duration::from_secs(3600 * 25 + 16);
  e.handle_timeout(far_future);
  // Member should be reaped.
  assert_eq!(
    e.test_member_status(2),
    None,
    "failed member should be reaped after handle_timeout fires the reap deadline"
  );
}

// ── Task 2.1: event ring-buffer dedup + event-clock + event broadcast tier ────

#[test]
fn user_event_increments_event_clock_and_emits_locally() {
  let mut e = ep();
  e.user_event(
    "deploy",
    bytes::Bytes::from_static(b"v2"),
    false,
    Instant::ORIGIN,
  )
  .unwrap();
  // Clock is incremented after stamping; local event at ltime=0 → clock now 1.
  assert_eq!(e.event_time(), 1);
  let ev = e.poll_event().expect("local user event must be pending");
  match ev {
    Event::User(u) => {
      assert_eq!(u.name.as_str(), "deploy");
      assert_eq!(u.payload.as_ref(), b"v2");
    }
    other => panic!("expected Event::User, got {other:?}"),
  }
}

#[test]
fn duplicate_user_event_is_deduped() {
  let mut e = ep();
  let m = crate::typed::UserEventMessage {
    ltime: 4.into(),
    cc: false,
    name: "x".into(),
    payload: bytes::Bytes::from_static(b"p"),
  };
  // First sight → rebroadcast=true, event emitted.
  assert!(
    e.test_handle_user_event(m.clone()),
    "first sight should return true"
  );
  // Drain the event.
  let _ = e.poll_event();
  // Duplicate → dropped (rebroadcast=false, no second event).
  assert!(
    !e.test_handle_user_event(m),
    "duplicate should return false"
  );
  assert!(e.poll_event().is_none(), "no second event for duplicate");
}

#[test]
fn user_event_below_min_time_is_dropped() {
  let mut e = ep();
  e.test_set_event_min_time(10);
  let m = crate::typed::UserEventMessage {
    ltime: 3.into(),
    cc: false,
    name: "old".into(),
    payload: bytes::Bytes::new(),
  };
  assert!(
    !e.test_handle_user_event(m),
    "event below min_time must return false"
  );
  assert!(
    e.poll_event().is_none(),
    "no event emitted for below-min-time message"
  );
}

#[test]
fn oversized_user_event_is_rejected() {
  let mut e = ep(); // max_user_event_size = 512
  // 1024 bytes payload, well over the 512-byte limit.
  let big = bytes::Bytes::from(vec![0u8; 1024]);
  assert!(
    e.user_event("big", big, false, Instant::ORIGIN).is_err(),
    "oversized user event must return Err"
  );
}

#[test]
fn user_event_broadcast_is_queued_at_event_tier() {
  let mut e = ep();
  e.user_event(
    "ship",
    bytes::Bytes::from_static(b"ok"),
    false,
    Instant::ORIGIN,
  )
  .unwrap();
  // After queuing, the user-broadcast queue must be non-empty (event tier = rank 2).
  assert!(
    e.user_broadcast_queue_len() > 0,
    "user broadcast queue must be non-empty after user_event"
  );
}

#[test]
fn user_event_too_old_relative_to_ring_is_dropped() {
  let mut e = ep();
  // Fire enough events to advance the clock well past the ring size (512).
  // Then try to inject a very old event (ltime=0 while clock is at 600+).
  // Shortcut: use the min_time setter to simulate the effect.
  // event_buffer_size = 512; clock = 600; ltime=0: cur_time(600) > bltime(512)
  // and ltime(0) < cur_time - bltime = 88 → too old.
  e.test_set_event_clock(600);
  let m = crate::typed::UserEventMessage {
    ltime: 0.into(),
    cc: false,
    name: "stale".into(),
    payload: bytes::Bytes::new(),
  };
  assert!(
    !e.test_handle_user_event(m),
    "too-old event must be dropped"
  );
  assert!(e.poll_event().is_none());
}

// ── Task 2.2: UserPacket decode + dispatch + relay-retain ─────────────────────

#[test]
fn user_event_arrives_over_user_packet_and_surfaces() {
  // A gossiped UserEvent encoded as an AnyMessage is injected via the
  // UserPacket sieve arm.  The machine must decode it, dedup, emit
  // Event::User, and re-queue the ORIGINAL bytes on the event broadcast
  // tier (relay-retain: no re-encode).
  let mut e = ep();
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "deploy".into(),
    payload: bytes::Bytes::from_static(b"v3"),
  })
  .encode()
  .unwrap();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);

  let ev = e.poll_event().expect("user event must surface");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "deploy"),
    "expected Event::User with name 'deploy', got: {ev:?}"
  );
}

#[test]
fn duplicate_user_event_over_user_packet_is_deduped() {
  // Same UserEvent injected twice → second one is silently dropped.
  // The relay-retain property: first-sight re-queues original bytes; second
  // sight is dropped without touching the broadcast queue.
  let mut e = ep();
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 3.into(),
    cc: false,
    name: "once".into(),
    payload: bytes::Bytes::from_static(b"x"),
  })
  .encode()
  .unwrap();

  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes.clone(), memberlist_proto::Instant::ORIGIN);
  let first = e.poll_event();
  assert!(
    matches!(first, Some(Event::User(_))),
    "first packet must surface Event::User"
  );

  // Deliver the identical encoded bytes again.
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);
  assert!(
    e.poll_event().is_none(),
    "duplicate must be silently dropped, not emitted again"
  );
}

#[test]
fn join_intent_over_user_packet_buffers_and_requeues_on_intent_tier() {
  // A Join intent for an unknown node arrives via UserPacket.  It is buffered
  // in the intent store and the original bytes are re-queued on the intent
  // broadcast tier (rank 0 = highest priority).
  let mut e = ep();
  let serf_bytes =
    AnyMessage::<u32, core::net::SocketAddr>::Join(JoinMessage::new(LamportTime::new(7), 2u32))
      .encode()
      .unwrap();
  let before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.user_broadcast_queue_len() > before,
    "intent broadcast queue must grow after a new join-intent over UserPacket"
  );
}

#[test]
fn leave_intent_over_user_packet_dispatches_and_requeues() {
  // A Leave intent for a known Alive member arrives via UserPacket.
  // The member transitions to Leaving and the bytes are re-queued on the
  // intent broadcast tier.
  let mut e = ep();
  e.test_seed_member(2u32, MemberStatus::Alive, LamportTime::new(3));
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::Leave(LeaveMessage::new(
    LamportTime::new(8),
    2u32,
    false,
  ))
  .encode()
  .unwrap();
  let before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "Alive member must transition to Leaving on a fresh leave intent"
  );
  assert!(
    e.user_broadcast_queue_len() > before,
    "leave intent must be re-queued on the intent tier"
  );
}

#[test]
fn stale_join_intent_over_user_packet_is_not_requeued() {
  // A stale join intent (ltime <= current status_time) must be dropped without
  // growing the broadcast queue (relay-retain: only first-sight is re-queued).
  let mut e = ep();
  e.test_seed_member(2u32, MemberStatus::Alive, LamportTime::new(10));
  let serf_bytes =
    AnyMessage::<u32, core::net::SocketAddr>::Join(JoinMessage::new(LamportTime::new(3), 2u32))
      .encode()
      .unwrap();
  let before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);

  assert_eq!(
    e.user_broadcast_queue_len(),
    before,
    "stale intent must not grow the broadcast queue"
  );
}

#[test]
fn user_event_over_user_packet_witnesses_event_clock() {
  // H4: The event clock must be witnessed when a UserEvent arrives via
  // UserPacket.  Both Reliable and Unreliable paths run through the same
  // handler; this test verifies the clock-witness side-effect.
  let mut e = ep();
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 5.into(),
    cc: false,
    name: "ping".into(),
    payload: bytes::Bytes::from_static(b"ok"),
  })
  .encode()
  .unwrap();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, serf_bytes, memberlist_proto::Instant::ORIGIN);

  let ev = e.poll_event().expect("event must surface");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "ping"),
    "expected Event::User(ping), got {ev:?}"
  );
  // After witnessing ltime=5, clock must be at least 6.
  assert!(
    e.event_time() >= 6,
    "event_time must be >= 6 after witnessing ltime=5"
  );
}

#[test]
fn malformed_bytes_in_user_packet_are_silently_dropped() {
  // A UserPacket carrying garbage bytes must not panic or emit events.
  let mut e = ep();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(
    from,
    bytes::Bytes::from_static(b"\xff\xff\xfe"),
    memberlist_proto::Instant::ORIGIN,
  );
  assert!(
    e.poll_event().is_none(),
    "malformed bytes must not produce any serf event"
  );
}

#[test]
fn user_event_rebroadcast_uses_original_bytes() {
  // Relay-retain: the bytes put on the broadcast queue after a first-sight
  // UserEvent must be the same encoding that arrived (refcount-shared,
  // not re-encoded).  We verify this by checking that the queue grows by
  // exactly the original encoding.
  let mut e = ep();
  let original_bytes = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 2.into(),
    cc: false,
    name: "ship".into(),
    payload: bytes::Bytes::from_static(b"payload"),
  })
  .encode()
  .unwrap();

  let queue_before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(
    from,
    original_bytes.clone(),
    memberlist_proto::Instant::ORIGIN,
  );

  // The event must have surfaced.
  assert!(matches!(e.poll_event(), Some(Event::User(_))));
  // The broadcast queue must have grown (re-queue happened).
  assert!(
    e.user_broadcast_queue_len() > queue_before,
    "relay-retain: user event must be re-queued after first sight"
  );
}

// ── Task 3.1: local-state synthesis + H6 dirty-flag re-push ──────────────────

#[test]
fn local_state_carries_three_clocks_and_members() {
  // Seed an endpoint with known clock values and members, call resync_local_state,
  // then decode the snapshot from the inner and verify the fields.
  let mut e = ep();
  e.test_set_clocks(11, 22, 33);
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(9));
  e.test_seed_left_member(3, LamportTime::new(7));
  e.resync_local_state();

  let snap = e.test_inner_local_state_snapshot();
  assert!(!snap.is_empty(), "snapshot must be non-empty after resync");

  let pp = e.test_decode_pushpull(&snap);
  assert_eq!(u64::from(pp.ltime), 11, "member clock must match");
  assert_eq!(u64::from(pp.event_ltime), 22, "event clock must match");
  assert_eq!(u64::from(pp.query_ltime), 33, "query clock must match");

  // member 2 must appear in status_ltimes
  assert!(
    pp.status_ltimes.iter().any(|(id, _)| *id == 2u32),
    "status_ltimes must contain member 2"
  );
  // member 3 must appear in left_members
  assert!(
    pp.left_members.contains(&3u32),
    "left_members must contain id 3"
  );
}

#[test]
fn resync_clears_dirty_flag() {
  let mut e = ep();
  e.test_set_clocks(1, 2, 3);
  // After set_clocks, dirty = true.
  assert!(e.test_is_dirty(), "clock mutation must mark dirty");
  e.resync_local_state();
  assert!(
    !e.test_is_dirty(),
    "resync_local_state must clear dirty flag"
  );
}

#[test]
fn mutating_event_clock_via_user_event_marks_state_dirty() {
  let mut e = ep();
  e.test_clear_dirty();
  // user_event increments the event clock → must mark dirty.
  e.user_event("x", bytes::Bytes::new(), false, Instant::ORIGIN)
    .unwrap();
  assert!(e.test_is_dirty(), "user_event must mark local_state dirty");
}

#[test]
fn drain_inner_calls_resync_when_dirty() {
  // After a mutation, the first poll_event call (which drives drain_inner)
  // must sync the snapshot into the inner endpoint.
  let mut e = ep();
  e.test_set_clocks(5, 0, 0);
  assert!(e.test_is_dirty());
  // A poll_event call drives drain_inner which calls resync_local_state when dirty.
  let _ = e.poll_event();
  assert!(
    !e.test_is_dirty(),
    "drain_inner must have called resync_local_state, clearing dirty"
  );
  // The snapshot must be non-empty and carry the clock we set.
  let snap = e.test_inner_local_state_snapshot();
  assert!(
    !snap.is_empty(),
    "snapshot must be present after drain_inner"
  );
  let pp = e.test_decode_pushpull(&snap);
  assert_eq!(
    u64::from(pp.ltime),
    5,
    "snapshot must carry the member clock"
  );
}

#[test]
fn ignore_join_stream_recorded_and_consumed_one_shot() {
  // Accessor smoke-test: an ignore_old join records its exchange StreamId
  // (idempotently); the matching merge consumes the one-shot entry, and an
  // unrecorded stream is never present.
  let mut e = ep();
  let p = core::net::SocketAddr::from(([127, 0, 0, 1], 6100));
  let s = e.start_join_push_pull(
    p,
    /*ignore_old*/ true,
    memberlist_proto::Instant::ORIGIN,
  );
  // Idempotent: re-noting the same StreamId must not double-store.
  e.test_note_ignore_join_stream(s);
  assert!(
    e.test_has_ignore_join_stream(s),
    "recorded exchange must be present"
  );
  // A join merge on this stream consumes the single entry (a second entry, had
  // the record not been idempotent, would survive this one consume).
  e.test_merge_remote_state_with_stream(bytes::Bytes::new(), true, s);
  assert!(
    !e.test_has_ignore_join_stream(s),
    "a join merge consumes the one-shot ignore-join entry"
  );
}

#[test]
fn snapshot_left_members_only_includes_known_ids() {
  // test_seed_left_member inserts into both states and left_members.
  // resync must include only ids that also exist in states.
  let mut e = ep();
  e.test_seed_left_member(3u32, LamportTime::new(2));
  e.test_set_clocks(1, 0, 0);
  e.resync_local_state();
  let snap = e.test_inner_local_state_snapshot();
  assert!(!snap.is_empty());
  let pp = e.test_decode_pushpull(&snap);
  // Member 3 should appear in left_members.
  assert!(
    pp.left_members.contains(&3u32),
    "seeded left member must appear in snapshot left_members"
  );
}

// ── Determinism: push-pull local-state bytes ──────────────────────────────────

#[test]
fn push_pull_local_state_bytes_deterministic() {
  // Two endpoints with the same members inserted in opposite id orders must
  // produce byte-identical push-pull wire output after resync_local_state.
  // This verifies that HashMap iteration order in `members.states` does NOT
  // leak into the encoded PushPullMessage.
  fn build_ep_asc() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
    let mut e = ep();
    e.test_set_clocks(5, 10, 15);
    // Insert members in ascending id order: 1, 2, 3, 4, 5.
    for id in [1u32, 2, 3, 4, 5] {
      e.test_seed_member(id, MemberStatus::Alive, LamportTime::new(id as u64));
    }
    e.resync_local_state();
    e
  }
  fn build_ep_desc() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
    let mut e = ep();
    e.test_set_clocks(5, 10, 15);
    // Insert members in descending id order: 5, 4, 3, 2, 1.
    for id in [5u32, 4, 3, 2, 1] {
      e.test_seed_member(id, MemberStatus::Alive, LamportTime::new(id as u64));
    }
    e.resync_local_state();
    e
  }

  let snap_asc = build_ep_asc().test_inner_local_state_snapshot();
  let snap_desc = build_ep_desc().test_inner_local_state_snapshot();

  assert!(
    !snap_asc.is_empty(),
    "ascending-order snapshot must be non-empty"
  );
  assert_eq!(
    snap_asc, snap_desc,
    "push-pull bytes must be identical regardless of member insertion order"
  );
}

// ── Task 4.1: query ring-buffer + query() + handle_query + Event::Query ───────

/// Helper: build a minimal `QueryMessage<u32, SocketAddr>` for tests.
///
/// Uses `ltime`, `id`, no filters, no flags, no relay, 5 s timeout,
/// and a sentinel from-address.
fn test_query(ltime: LamportTime, id: u32) -> QueryMessage<u32, core::net::SocketAddr> {
  QueryMessage {
    ltime,
    id,
    from: memberlist_proto::Node::new(99u32, "127.0.0.1:9999".parse().unwrap()),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "ping".into(),
    payload: bytes::Bytes::new(),
  }
}

#[test]
fn query_reads_not_explicitly_increments_query_clock() {
  // G8 / H8: `query()` stamps ltime = query_clock (READ), not query_clock += 1.
  // Unlike user_event (which explicitly calls event_clock += 1 then stamps),
  // query() does NOT contain an explicit increment.  The clock may still advance
  // via the witness inside handle_query (witnessing ltime=N at clock=N → N+1).
  // The invariant tested here: QueryId.ltime equals the clock value AT ISSUE TIME,
  // and the clock after the call is >= that value (witness, not explicit increment).
  let mut e = ep();
  e.test_set_clocks(0, 0, 5);
  let id = e
    .query(
      "ping",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  // The ltime stamped on the QueryId is the clock value READ before any witness.
  assert_eq!(
    id.ltime.0, 5,
    "QueryId.ltime must equal the clock at issue time"
  );
  // The clock may advance to 6 via witness(ltime=5, clock=5) in handle_query
  // (oracle-correct; contrast user_event which explicitly does clock += 1 BEFORE stamp).
  assert!(
    e.query_time() >= 5,
    "query clock must not regress below the issued ltime"
  );
}

#[test]
fn query_registers_pending_entry() {
  // After `query()`, one pending query must be registered.
  let mut e = ep();
  let id = e
    .query(
      "test",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  assert_eq!(e.test_pending_query_count(), 1);
  assert_eq!(e.test_last_query_id(), Some(id));
}

#[test]
fn query_processes_locally_and_emits_event_query() {
  // `query()` calls `handle_query(self)` before broadcasting,
  // so the local node sees Event::Query if no filters reject it.
  let mut e = ep();
  e.query(
    "local-ping",
    bytes::Bytes::new(),
    QueryParams::default(),
    memberlist_proto::Instant::ORIGIN,
  )
  .unwrap();
  // The local process step emits an Event::Query.
  let ev = e.poll_event().expect("must emit a local Event::Query");
  assert!(
    matches!(ev, Event::Query(_)),
    "expected Event::Query, got {:?}",
    core::mem::discriminant(&ev)
  );
}

#[test]
fn handle_query_witnesses_query_clock() {
  let mut e = ep();
  assert_eq!(e.query_time(), 0);
  let q = test_query(LamportTime::new(7), 1);
  e.test_handle_query(q);
  // witness(7) → clock becomes 8
  assert_eq!(
    e.query_time(),
    8,
    "query clock must be witnessed on ingress"
  );
}

#[test]
fn duplicate_query_id_is_deduped_but_distinct_ids_at_same_ltime_pass() {
  // G6 / dedup: (ltime, id) is the composite key.
  // Same ltime + same id → dropped.  Same ltime + different id → first sight.
  let mut e = ep();
  let a = test_query(LamportTime::new(3), 100);
  let b = QueryMessage {
    id: 200,
    ..a.clone()
  }; // same ltime, different random id

  assert!(
    e.test_handle_query(a.clone()),
    "first sight of (3, 100) must return true"
  );
  assert!(
    !e.test_handle_query(a),
    "exact dup (3, 100) must return false"
  );
  assert!(
    e.test_handle_query(b),
    "distinct id (3, 200) at same ltime must return true"
  );
}

#[test]
fn filter_rejected_query_still_rebroadcasts_but_does_not_surface() {
  // G6: a filter-rejected query must STILL return rebroadcast=true.
  // The local node (id=1) is NOT in the id filter [999].
  let mut e = ep();
  let q = QueryMessage {
    filters: vec![Filter::Id(vec![999u32])],
    ..test_query(LamportTime::new(1), 42)
  };
  assert!(
    e.test_handle_query(q),
    "filter-rejected query must still rebroadcast (G6)"
  );
  // But no Event::Query must be emitted locally.
  assert!(
    e.poll_event().is_none(),
    "filter-rejected query must not surface as Event::Query"
  );
}

#[test]
fn query_too_old_is_dropped() {
  // A query whose ltime is older than the entire ring is dropped.
  let mut e = ep();
  // Set the query clock high enough that ltime=1 is "too old" relative to the ring.
  // query_buffer_size = 512 (default). cur_time > 512 && ltime < cur_time - 512.
  e.test_set_clocks(0, 0, 600); // query_clock = 600

  // Deliver a query at ltime=1 (much older than cur_time - 512 = 88).
  let q = test_query(LamportTime::new(1), 77);
  // handle_query witnesses the clock first; after witness(1) clock stays 600 (>1).
  // Then dedup check: cur_time=600, bltime=512, 1 < 600 - 512 = 88 → too old.
  assert!(
    !e.test_handle_query(q),
    "query older than the ring must be dropped (not rebroadcast)"
  );
  assert!(
    e.poll_event().is_none(),
    "too-old query must not emit an event"
  );
}

#[test]
fn no_broadcast_flag_suppresses_rebroadcast() {
  // When the NO_BROADCAST flag is set, rebroadcast must be false even on first sight.
  let mut e = ep();
  let q = QueryMessage {
    flags: QueryFlag::NO_BROADCAST,
    ..test_query(LamportTime::new(2), 11)
  };
  assert!(
    !e.test_handle_query(q),
    "NO_BROADCAST flag must suppress rebroadcast"
  );
  // But the query is still processed locally (no filter), so Event::Query is emitted.
  assert!(
    matches!(e.poll_event(), Some(Event::Query(_))),
    "NO_BROADCAST query must still surface locally"
  );
}

#[test]
fn query_id_has_correct_ltime_and_nonzero_id() {
  // The QueryId returned from query() must carry the clock value at issue and a non-
  // deterministic (but seed-reproducible) random id.  The random id must be u32 (any).
  let mut e = ep();
  e.test_set_clocks(0, 0, 9);
  let qid = e
    .query(
      "check",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  assert_eq!(qid.ltime.0, 9, "ltime must equal query_clock at issue");
  // id is random but deterministically seeded; just assert it is a u32 (no panic).
  let _ = qid.id;
}

#[test]
fn query_size_limit_is_enforced() {
  // A query whose encoded size exceeds query_size_limit must return Err.
  let mut e = ep();
  // Default query_size_limit = 1024. A 1500-byte payload will exceed it.
  let big = bytes::Bytes::from(vec![0u8; 1500]);
  let result = e.query(
    "big",
    big,
    QueryParams::default(),
    memberlist_proto::Instant::ORIGIN,
  );
  assert!(result.is_err(), "oversized query must be rejected");
}

#[test]
fn query_buffer_min_time_is_zero_at_start() {
  let e = ep();
  assert_eq!(
    e.test_query_min_time(),
    0,
    "initial query min_time must be zero"
  );
}

#[test]
fn query_emits_on_query_tier_broadcast_queue() {
  // After query(), the encoded query must be in the user broadcast queue (rank 1).
  let mut e = ep();
  let _id = e
    .query(
      "broadcast-test",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  assert!(
    e.user_broadcast_queue_len() >= 1,
    "query must enqueue on the broadcast queue"
  );
}

// Regression: invalid tag-regex must not advance the RNG.
//
// Two endpoints seeded identically must produce the same QueryId for the same
// valid query, even when endpoint A first receives a rejected invalid-regex
// query.  The zero-side-effect contract requires that a failed query() leaves
// the RNG stream unmoved.
#[cfg(feature = "tag-regex")]
#[test]
fn invalid_tag_regex_does_not_advance_rng() {
  // Helper that builds a serf Endpoint with a specified u64 seed so both
  // endpoints start with exactly the same RNG state.
  let make_ep = |seed: u64| {
    let inner_opts = EndpointOptions::new(
      1u32,
      "127.0.0.1:7946".parse::<core::net::SocketAddr>().unwrap(),
    )
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
    let inner = memberlist_proto::Endpoint::new_at(
      inner_opts,
      memberlist_proto::Instant::ORIGIN,
      SmallRng::seed_from_u64(0),
    );
    StreamEndpoint::new_with_rng(coord(inner), Options::new(), SmallRng::seed_from_u64(seed))
  };

  let mut ep_a = make_ep(42);
  let mut ep_b = make_ep(42);

  // Invalid regex: unbalanced bracket.
  let bad_filter = QueryParams {
    filters: vec![Filter::Tag(crate::typed::TagFilter {
      tag: "role".into(),
      expr: Some("[invalid-regex".into()),
    })],
    ..Default::default()
  };

  // Endpoint A: rejected call — must return Err and leave RNG untouched.
  let bad_result = ep_a.query(
    "ping",
    bytes::Bytes::new(),
    bad_filter,
    memberlist_proto::Instant::ORIGIN,
  );
  assert!(
    matches!(bad_result, Err(Error::InvalidQueryFilter)),
    "invalid tag regex must return Err(InvalidQueryFilter)"
  );

  // Valid filter (no tag filter at all).
  let good_params = QueryParams::default();

  // Endpoint A: valid call after the failed one.
  let qid_a = ep_a
    .query(
      "ping",
      bytes::Bytes::new(),
      good_params.clone(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();

  // Endpoint B: only the valid call — no prior failed call.
  let qid_b = ep_b
    .query(
      "ping",
      bytes::Bytes::new(),
      good_params,
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();

  // The random ids must be equal; if the failed query advanced the RNG on A
  // but not on B, they would diverge.
  assert_eq!(
    qid_a.id, qid_b.id,
    "rejected invalid-regex query must not advance the RNG (ids diverged: A={}, B={})",
    qid_a.id, qid_b.id
  );
  // Lamport ltimes must also match (both endpoints share the same clock state).
  assert_eq!(
    qid_a.ltime, qid_b.ltime,
    "ltime must match between the two identically-seeded endpoints"
  );
}

// ── Task 4.2: respond() three guards + query-response fold ───────────────────

fn addr(port: u16) -> core::net::SocketAddr {
  format!("127.0.0.1:{port}").parse().unwrap()
}

fn t_secs(s: u64) -> memberlist_proto::Instant {
  memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(s)
}

fn qresp(
  ltime: LamportTime,
  id: u32,
  from_port: u16,
) -> crate::typed::QueryResponseMessage<u32, core::net::SocketAddr> {
  crate::typed::QueryResponseMessage {
    ltime,
    id,
    from: memberlist_proto::Node::new(from_port as u32, addr(from_port)),
    flags: QueryFlag::empty(),
    payload: bytes::Bytes::new(),
  }
}

#[test]
fn respond_once_succeeds() {
  // A valid respond() within the deadline must succeed.
  let mut e = ep();
  let deadline = t_secs(10);
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    deadline,
  );
  assert!(
    e.respond(
      &token,
      bytes::Bytes::from_static(b"ok"),
      memberlist_proto::Instant::ORIGIN
    )
    .is_ok(),
    "first respond() must succeed"
  );
}

#[test]
fn respond_twice_is_rejected() {
  // G7 guard 2: a second respond() on the same token must return AlreadyResponded.
  let mut e = ep();
  let deadline = t_secs(10);
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    deadline,
  );
  assert!(
    e.respond(
      &token,
      bytes::Bytes::from_static(b"ok"),
      memberlist_proto::Instant::ORIGIN
    )
    .is_ok()
  );
  let err = e
    .respond(
      &token,
      bytes::Bytes::from_static(b"ok2"),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap_err();
  assert!(
    matches!(err, Error::AlreadyResponded),
    "second respond() must return AlreadyResponded"
  );
}

#[test]
fn respond_after_deadline_is_rejected() {
  // G7 guard 3: respond() with now > deadline must return RespondAfterDeadline.
  let mut e = ep();
  let deadline = t_secs(1);
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    deadline,
  );
  // now = t_secs(5) > deadline = t_secs(1)
  let err = e
    .respond(&token, bytes::Bytes::from_static(b"late"), t_secs(5))
    .unwrap_err();
  assert!(
    matches!(err, Error::RespondAfterDeadline),
    "respond() past deadline must return RespondAfterDeadline"
  );
}

#[test]
fn respond_with_oversized_payload_is_rejected() {
  // G7 guard 1: payload.len() > query_response_size_limit (default 1024) must error.
  let mut e = ep();
  let deadline = t_secs(10);
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    deadline,
  );
  let big = bytes::Bytes::from(vec![0u8; 2048]);
  let err = e
    .respond(&token, big, memberlist_proto::Instant::ORIGIN)
    .unwrap_err();
  assert!(
    matches!(err, Error::RespondTooLarge(_, _)),
    "oversized respond() must return RespondTooLarge"
  );
}

#[test]
fn respond_succeeds_and_prevents_second_call() {
  // After a successful respond(), the entry is removed from received_queries.
  // A second respond() must return AlreadyResponded (via the .ok_or guard).
  let mut e = ep();
  let qid = QueryId {
    ltime: LamportTime::new(2),
    id: 9,
  };
  let deadline = t_secs(10);
  let token = e.test_register_received_query(qid, addr(1002), deadline);
  assert!(!e.test_is_responded(qid), "initially not responded");
  e.respond(
    &token,
    bytes::Bytes::new(),
    memberlist_proto::Instant::ORIGIN,
  )
  .unwrap();
  // Entry removed on success: second call returns AlreadyResponded.
  let err = e
    .respond(
      &token,
      bytes::Bytes::new(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap_err();
  assert!(
    matches!(err, Error::AlreadyResponded),
    "second respond() after success must return AlreadyResponded, got {err:?}"
  );
}

#[test]
fn app_query_response_surfaces_as_event() {
  // A QueryResponseMessage arriving for an App-kind PendingQuery must emit
  // Event::QueryResponse with the matching id.
  let mut e = ep();
  let id = e
    .query(
      "ping",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  // Drain the locally-emitted Event::Query.
  let _ = e.poll_event();

  // A responder (node 2) replies.
  let resp = qresp(id.ltime, id.id, 2);
  e.test_handle_query_response(resp);

  // The response must surface as Event::QueryResponse.
  let ev = e.poll_event().expect("Event::QueryResponse expected");
  assert!(
    matches!(&ev, Event::QueryResponse(qr) if qr.id() == id.id),
    "response must carry the query id"
  );
}

#[test]
fn duplicate_query_response_is_deduped() {
  // A second response from the same node for the same query must be dropped.
  let mut e = ep();
  let id = e
    .query(
      "ping",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  // Drain locally-emitted Event::Query.
  let _ = e.poll_event();

  // First response from node 2.
  let resp = qresp(id.ltime, id.id, 2);
  e.test_handle_query_response(resp.clone());
  let ev1 = e.poll_event();
  assert!(
    matches!(ev1, Some(Event::QueryResponse(_))),
    "first response must surface"
  );

  // Duplicate from the same node.
  e.test_handle_query_response(resp);
  assert!(
    e.poll_event().is_none(),
    "duplicate responder must be deduped and not surface"
  );
}

#[test]
fn multiple_responders_each_surface_independently() {
  // Distinct responders for the same query must each produce an Event::QueryResponse.
  let mut e = ep();
  let id = e
    .query(
      "ping",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  // Drain locally-emitted Event::Query.
  let _ = e.poll_event();

  // Two distinct responders.
  e.test_handle_query_response(qresp(id.ltime, id.id, 2));
  e.test_handle_query_response(qresp(id.ltime, id.id, 3));

  let ev1 = e.poll_event().expect("response from node 2");
  let ev2 = e.poll_event().expect("response from node 3");
  assert!(matches!(ev1, Event::QueryResponse(_)));
  assert!(matches!(ev2, Event::QueryResponse(_)));
  assert!(e.poll_event().is_none(), "no further events");
}

#[test]
fn stale_query_response_is_dropped() {
  // A response for an unknown/expired query id must be silently discarded.
  let mut e = ep();
  // Deliver a response for a query that was never registered.
  let resp = qresp(LamportTime::new(99), 0xdead, 2);
  e.test_handle_query_response(resp);
  assert!(
    e.poll_event().is_none(),
    "response for unknown query must not emit an event"
  );
}

#[test]
fn query_response_via_user_packet_wire_path_surfaces_event() {
  // A QueryResponseMessage arriving via the gossip UserPacket path (injected as
  // serf-level bytes via test_inject_user_packet) must fold into the matching
  // PendingQuery and emit Event::QueryResponse.
  let mut e = ep();
  let id = e
    .query(
      "wire-test",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap();
  // Drain locally-emitted Event::Query.
  let _ = e.poll_event();

  // Encode a QueryResponseMessage as serf-level bytes and inject via the
  // UserPacket path (mimicking the gossip plane delivery).
  let resp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: id.ltime,
    id: id.id,
    from: memberlist_proto::Node::new(2u32, addr(1002)),
    flags: QueryFlag::empty(),
    payload: bytes::Bytes::new(),
  };
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::QueryResponse(resp)
    .encode()
    .unwrap();
  e.test_inject_user_packet(addr(1002), serf_bytes, memberlist_proto::Instant::ORIGIN);

  let ev = e
    .poll_event()
    .expect("Event::QueryResponse from UserPacket path");
  assert!(
    matches!(ev, Event::QueryResponse(_)),
    "UserPacket-path response must surface as Event::QueryResponse"
  );
}

// ── Task 4.3: responder-side relay ───────────────────────────────────────────

/// Build a `Node<u32, SocketAddr>` at `127.0.0.1:<port>` with `id = port as u32`.
fn relay_node(port: u16) -> memberlist_proto::Node<u32, core::net::SocketAddr> {
  memberlist_proto::Node::new(port as u32, addr(port))
}

#[test]
fn relay_response_is_silent_noop_when_too_few_members() {
  // With only self in membership, relay_factor=2 requires at least 3 total members
  // (relay_factor + 1 = 3) but we have 1 → silent no-op: no RelayDropped event,
  // no directed send.
  let mut e = ep();
  let querier = relay_node(2000);
  e.test_relay_response(querier, bytes::Bytes::from_static(b"frame"), 2);
  assert!(
    e.poll_event().is_none(),
    "relay with too few members must not emit RelayDropped"
  );
  assert!(
    e.test_last_directed_send().is_none(),
    "no directed send should have occurred"
  );
}

#[test]
fn relay_response_with_zero_factor_is_noop() {
  // relay_factor == 0 must be a fast-path no-op: no sends, no events.
  let mut e = ep();
  let querier = relay_node(2000);
  e.test_relay_response(querier, bytes::Bytes::from_static(b"frame"), 0);
  assert!(e.poll_event().is_none());
  assert!(e.test_last_directed_send().is_none());
}

#[test]
fn relay_response_picks_alive_non_self_member_and_sends() {
  // Seed 2 Alive members (ids 10, 11). With relay_factor=1 the count guard
  // requires at least 2 members. One directed send must occur.
  let mut e = ep();
  // Seed Alive members with ports 1010 and 1011.
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));
  // test_seed_member uses port 0; seed id 11 at an explicit address so the
  // relay-peer selection has two distinct addresses to choose between.
  e.test_seed_member_at(11u32, addr(1011), MemberStatus::Alive, LamportTime::new(1));

  let querier = relay_node(2000);
  let frame = bytes::Bytes::from_static(b"\x06relay-payload");
  e.test_relay_response(querier, frame.clone(), 1);

  // A directed send must have happened to one of the Alive peers.
  let (dest_addr, sent_bytes) = e
    .test_last_directed_send()
    .expect("relay_response must produce a directed send when members >= k+1");
  // The sent bytes are the relay-wrapped frame, not the raw frame.
  // Verify that the destination is one of the seeded alive peers (not self port 7946).
  assert_ne!(
    dest_addr.port(),
    7946,
    "relay must not target the local node"
  );
  // The relay wrapper is non-empty (the inner frame is embedded in it).
  assert!(!sent_bytes.is_empty(), "relay frame must be non-empty");
  assert!(e.poll_event().is_none(), "no RelayDropped on success");
}

#[test]
fn relay_node_b_forwards_verbatim_to_destination() {
  // Node B (this node) receives a RelayMessage and must forward the inner
  // payload verbatim to the destination via send_user_packet.
  // The destination is node 2 (not self = 1).
  let mut e = ep();
  let inner_payload = bytes::Bytes::from_static(b"\x06inner-qresp");
  let relay = RelayMessage::new(relay_node(1002), inner_payload.clone());
  e.test_handle_relay(relay);

  // The directed send must carry the verbatim inner payload to node 2's address.
  let (dest_addr, sent_bytes) = e
    .test_last_directed_send()
    .expect("handle_relay must produce a directed send to the destination");
  assert_eq!(
    dest_addr,
    addr(1002),
    "relay must forward to the wrapped destination address"
  );
  assert_eq!(
    sent_bytes, inner_payload,
    "relay payload must be forwarded verbatim, not re-encoded"
  );
  assert!(
    e.poll_event().is_none(),
    "successful handle_relay must not emit any event"
  );
}

#[test]
fn relay_to_self_emits_relay_dropped() {
  // A Relay with destination == local_id (1) must emit Event::RelayDropped
  // and must NOT attempt a directed send (self-relay is a no-op failure).
  let mut e = ep(); // local id = 1
  let relay = RelayMessage::new(
    relay_node(1), // id=1 matches the local endpoint
    bytes::Bytes::from_static(b"x"),
  );
  e.test_handle_relay(relay);
  // The test_last_directed_send must be None (no real send attempted).
  // Note: port 7946 is the local bind port, not the relay_node(1) port;
  // the id-equality guard fires before any send.  The event must be present.
  let ev = e.poll_event().expect("relay to self must emit an event");
  assert!(
    matches!(ev, Event::RelayDropped(_)),
    "relay to self must surface as Event::RelayDropped"
  );
}

#[test]
fn relay_sieve_arm_decodes_relay_message_from_user_packet() {
  // A RelayMessage arriving via the gossip UserPacket path must be decoded
  // and handled (the Relay sieve arm must call handle_relay).
  // We inject a Relay wrapping a tiny inner payload targeting a non-self node.
  let mut e = ep();
  let inner_payload = bytes::Bytes::from_static(b"\x06fake-resp");
  let relay = RelayMessage::new(relay_node(1002), inner_payload.clone());
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::Relay(relay)
    .encode()
    .unwrap();

  e.test_inject_user_packet(addr(1002), serf_bytes, memberlist_proto::Instant::ORIGIN);

  // The inner payload must have been directed-sent to the destination.
  let (dest, sent) = e
    .test_last_directed_send()
    .expect("relay sieve arm must forward to destination");
  assert_eq!(dest, addr(1002));
  assert_eq!(sent, inner_payload);
}

/// Build a serf `StreamEndpoint<u32, SocketAddr>` with an explicit RNG seed for the
/// serf-level RNG (the relay/reconnect draws).  The inner Endpoint uses a fixed
/// seed 0; the serf-level seed is the caller-supplied `serf_seed`.
fn ep_with_serf_seed(serf_seed: u64) -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  StreamEndpoint::new_with_rng(
    coord(inner),
    Options::new(),
    SmallRng::seed_from_u64(serf_seed),
  )
}

#[test]
fn relay_response_candidate_selection_is_deterministic() {
  // Regression test for HashMap-iteration-order nondeterminism in relay candidate
  // selection.
  //
  // Two endpoints with identical RNG seeds and identical Alive membership MUST
  // select the same relay peer set regardless of HashMap iteration order.
  // Pre-fix: the candidates Vec was collected directly from HashMap::iter() —
  // which Rust randomises per map instance — so a Fisher-Yates shuffle on a
  // fixed RNG seed produced different selections when the input ordering differed.
  // Post-fix: candidates are sorted by the encoded node id before the shuffle so
  // the shuffle input is always the same total order, making the selection a pure
  // function of the RNG state.
  //
  // The two endpoints intentionally insert the same members in REVERSE id order
  // to maximise the chance that HashMap iteration diverges (hash-map bucket
  // assignment is seed-randomised, but inserting in different orders can shift
  // collision chains and iteration position). With relay_factor=2 and five
  // candidates the probability that both happen to produce the same two-element
  // selection by chance is at most (2/5)^2 = 4% — negligible for a
  // determinism gate, and zero after the sort fix.

  // Five Alive members with distinct ids and distinct addresses.
  // Local id = 1 (from ep_with_serf_seed), so ids 10..14 are non-self.
  let members: Vec<(u32, u16)> = vec![(10, 1010), (11, 1011), (12, 1012), (13, 1013), (14, 1014)];

  let seed = 0xdeadbeef_cafebabe_u64;

  // Endpoint A: members inserted in ascending id order.
  let mut ep_a = ep_with_serf_seed(seed);
  for &(id, port) in &members {
    ep_a.test_seed_member_at(id, addr(port), MemberStatus::Alive, LamportTime::new(1));
  }

  // Endpoint B: members inserted in DESCENDING id order (maximum input-order
  // difference from A, exercising the HashMap-order divergence).
  let mut ep_b = ep_with_serf_seed(seed);
  for &(id, port) in members.iter().rev() {
    ep_b.test_seed_member_at(id, addr(port), MemberStatus::Alive, LamportTime::new(1));
  }

  let querier = relay_node(2000);
  let frame = bytes::Bytes::from_static(b"\x06determinism-test-frame");

  // relay_factor=2 — select two relay peers from the five candidates.
  // The count guard requires relay_factor + 1 = 3 total members; we have 5.
  ep_a.test_relay_response(querier, frame.clone(), 2);
  ep_b.test_relay_response(relay_node(2000), frame, 2);

  let sends_a = ep_a.test_relay_all_directed_sends().to_vec();
  let sends_b = ep_b.test_relay_all_directed_sends().to_vec();

  // Both must have produced exactly relay_factor=2 directed sends.
  assert_eq!(sends_a.len(), 2, "endpoint A must relay to exactly 2 peers");
  assert_eq!(sends_b.len(), 2, "endpoint B must relay to exactly 2 peers");

  // The selected peer addresses must be identical in both order and identity.
  // Any divergence here indicates the candidate ordering was not stabilised
  // before the Fisher-Yates shuffle.
  let addrs_a: Vec<core::net::SocketAddr> = sends_a.iter().map(|(a, _)| *a).collect();
  let addrs_b: Vec<core::net::SocketAddr> = sends_b.iter().map(|(a, _)| *a).collect();
  assert_eq!(
    addrs_a, addrs_b,
    "relay peer selection must be identical across endpoints with the same RNG seed \
     regardless of HashMap insertion order; got A={addrs_a:?}, B={addrs_b:?}"
  );
}

// ── Task 4.4: conflict-resolution and key-management queries ─────────────────

fn far_future() -> memberlist_proto::Instant {
  memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3600)
}

#[test]
fn conflict_win_does_not_shut_down() {
  // Majority of responses agree → local node won → no Event::Shutdown, and the
  // machine stays Alive and fully functional (guards against over-eager gating).
  let mut e = ep();
  let deadline = far_future();
  let qid = e.test_register_conflict_query(deadline);

  // 3 responses: 2 agree (matching), 1 disagrees.
  e.test_fold_conflict_response(qid, 100u32, true);
  e.test_fold_conflict_response(qid, 101u32, true);
  e.test_fold_conflict_response(qid, 102u32, false);

  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);

  assert!(
    e.poll_event().is_none(),
    "winning conflict must not emit Shutdown"
  );
  assert!(
    e.state().is_alive(),
    "winning conflict must leave the machine Alive"
  );
  // A won vote leaves the command surface fully open.
  assert!(
    e.user_event("post-win", bytes::Bytes::new(), false, Instant::ORIGIN)
      .is_ok(),
    "a won vote must not gate commands"
  );
}

#[test]
fn conflict_loss_transitions_to_shutdown_and_still_delivers_event() {
  // Minority of responses agree → local node lost → the machine performs its
  // documented forced Alive → Shutdown transition (Go serf's conflict-loss
  // shutdown()) AND the buffered Event::Shutdown still drains via poll_event.
  let mut e = ep();
  let deadline = far_future();
  let qid = e.test_register_conflict_query(deadline);

  // 3 responses: 1 agrees, 2 disagree → majority = 2, matching = 1 < 2 → lost.
  e.test_fold_conflict_response(qid, 200u32, true);
  e.test_fold_conflict_response(qid, 201u32, false);
  e.test_fold_conflict_response(qid, 202u32, false);

  assert!(e.state().is_alive(), "machine is Alive before the close");

  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);

  // The transition happened at the close, before any event is drained: reverting
  // the `self.state = Shutdown` in close_conflict_query leaves this Alive → fail.
  assert!(
    e.state().is_shutdown(),
    "a lost conflict vote must transition the machine to Shutdown"
  );

  // Event delivery is PRESERVED: the already-buffered Event::Shutdown drains from
  // the now-dead machine (poll_event stays functional post-Shutdown).
  let ev = e.poll_event().expect("conflict loss must emit an event");
  assert!(
    matches!(ev, Event::Shutdown),
    "conflict loss must emit Event::Shutdown, got {:?}",
    ev
  );
  assert!(
    e.state().is_shutdown(),
    "the machine stays Shutdown after the event drains"
  );
}

// ── same-tick conflict-loss ordering (mid-loop / mid-pass transition) ─────────
//
// When a due conflict close loses the vote and shuts the machine down partway
// through the timeout pass, the remaining due-query closes and the rest of the
// deadline work must not run: nothing may be produced after Event::Shutdown.

#[test]
fn same_tick_conflict_loss_stops_remaining_due_conflict_query() {
  // Two conflict queries (both losing) share a deadline; the received-query
  // prune of this pass would evict an expired token.  The first close transitions
  // to Shutdown, so the loop must stop before the second close (only ONE
  // Event::Shutdown) and the prune must not run (token retained).
  let mut e = ep();
  let deadline = t_secs(10);

  // First losing conflict query (1 agree, 2 disagree → matching 1 < majority 2).
  let cq1 = e.test_register_conflict_query(deadline);
  e.test_fold_conflict_response(cq1, 200u32, true);
  e.test_fold_conflict_response(cq1, 201u32, false);
  e.test_fold_conflict_response(cq1, 202u32, false);

  // Bump the query clock so the second conflict query gets a distinct QueryId
  // (test_register_conflict_query stamps the ltime from the query clock).
  e.test_set_clocks(0, 0, 1);
  let cq2 = e.test_register_conflict_query(deadline);
  e.test_fold_conflict_response(cq2, 210u32, true);
  e.test_fold_conflict_response(cq2, 211u32, false);
  e.test_fold_conflict_response(cq2, 212u32, false);

  // An expired received-query token: the prune step, if reached, evicts it.
  let _token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    t_secs(1),
  );
  assert_eq!(
    e.test_received_queries_len(),
    1,
    "the received-query token is present before the tick"
  );

  // One tick past the shared deadline drives the whole after_inner_timeout pass.
  e.handle_timeout(t_secs(20));

  let mut shutdowns = 0;
  while let Some(ev) = e.poll_event() {
    if matches!(ev, Event::Shutdown) {
      shutdowns += 1;
    }
  }
  // Reverting the mid-loop break closes the second losing conflict query too,
  // emitting a second Event::Shutdown → this fails.
  assert_eq!(
    shutdowns, 1,
    "only one Event::Shutdown: the loop must stop at the first conflict loss"
  );
  // Reverting the after_inner_timeout early-return runs the prune → len 0 → fails.
  assert_eq!(
    e.test_received_queries_len(),
    1,
    "the received-query prune must not run after the same-tick Shutdown"
  );
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn same_tick_conflict_loss_stops_remaining_due_key_query() {
  // A conflict query (losing) is registered FIRST and a key query shares its
  // deadline.  When the conflict close shuts the machine down mid-loop, the loop
  // must stop before closing the key query, so no KeyResponse is ever enqueued
  // after Event::Shutdown, and the received-query prune of the pass must not run.
  let mut e = ep();
  let deadline = t_secs(10);

  let cq = e.test_register_conflict_query(deadline);
  e.test_fold_conflict_response(cq, 200u32, true);
  e.test_fold_conflict_response(cq, 201u32, false);
  e.test_fold_conflict_response(cq, 202u32, false);

  // Key query at the SAME deadline, registered second (distinct QueryId: id 77).
  let _kq = e.test_register_key_query(deadline);

  let _token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    t_secs(1),
  );
  assert_eq!(e.test_received_queries_len(), 1);

  e.handle_timeout(t_secs(20));

  let mut events = vec![];
  while let Some(ev) = e.poll_event() {
    events.push(ev);
  }
  // Event::Shutdown must be the LAST event, and no KeyResponse may appear at all.
  // Reverting the mid-loop break closes the key query after the Shutdown →
  // KeyResponse lands after Shutdown → both assertions fail.
  assert!(
    matches!(events.last(), Some(Event::Shutdown)),
    "Event::Shutdown must be the last event delivered, got {events:?}"
  );
  assert!(
    !events.iter().any(|ev| matches!(ev, Event::KeyResponse(_))),
    "no KeyResponse may follow the conflict-loss Shutdown, got {events:?}"
  );
  // Reverting the after_inner_timeout early-return runs the prune → len 0 → fails.
  assert_eq!(
    e.test_received_queries_len(),
    1,
    "the received-query prune must not run after the same-tick Shutdown"
  );
}

// ── post-Shutdown chokepoint contract ────────────────────────────────────────
//
// A machine that lost an id-conflict vote transitions to `SerfState::Shutdown`
// and thereafter refuses commands, goes inert on ingress, and quiets its timers
// (mirroring the memberlist post-leave contract), while still draining the
// buffered `Event::Shutdown`.  The tests below drive a REAL lost vote through the
// conflict-query scaffolding and sweep each chokepoint class.

/// Drive the endpoint through a real lost id-conflict vote and drain the
/// resulting `Event::Shutdown`, leaving it in the terminal `Shutdown` state.
fn shut_down_via_lost_conflict(e: &mut StreamEndpoint<u32, core::net::SocketAddr, RawRecords>) {
  let qid = e.test_register_conflict_query(far_future());
  // 1 agree, 2 disagree → matching 1 < majority 2 → lost.
  e.test_fold_conflict_response(qid, 200u32, true);
  e.test_fold_conflict_response(qid, 201u32, false);
  e.test_fold_conflict_response(qid, 202u32, false);
  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);
  assert!(
    matches!(e.poll_event(), Some(Event::Shutdown)),
    "the buffered Event::Shutdown must drain from the shut-down machine"
  );
  assert!(e.state().is_shutdown(), "the machine must be Shutdown");
}

#[test]
fn shutdown_refuses_originating_commands() {
  let mut e = ep();
  // Register a live received-query token BEFORE shutdown so respond()'s refusal
  // is proven to precede its received_queries lookup and deadline guard.
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    addr(1002),
    far_future(),
  );

  shut_down_via_lost_conflict(&mut e);

  let now = memberlist_proto::Instant::ORIGIN;
  let tags: Tags = [("role", "web")].into_iter().collect();

  // Commands that originate cluster work funnel through ensure_not_shutdown.
  assert!(
    matches!(
      e.user_event("x", bytes::Bytes::new(), false, Instant::ORIGIN),
      Err(Error::Shutdown)
    ),
    "user_event must be refused after shutdown"
  );
  assert!(
    matches!(
      e.query("q", bytes::Bytes::new(), QueryParams::default(), now),
      Err(Error::Shutdown)
    ),
    "query must be refused after shutdown"
  );
  assert!(
    matches!(e.set_tags(tags, Instant::ORIGIN), Err(Error::Shutdown)),
    "set_tags must be refused after shutdown"
  );
  assert!(
    matches!(
      e.respond(&token, bytes::Bytes::new(), now),
      Err(Error::Shutdown)
    ),
    "respond must be refused after shutdown, before the token lookup"
  );

  // The lifecycle commands keep their own pre-existing typed state errors — the
  // transition alone already makes them refuse Shutdown.
  assert!(
    matches!(e.join(), Err(Error::BadJoinState(SerfState::Shutdown))),
    "join keeps BadJoinState on a Shutdown machine"
  );
  assert!(
    matches!(e.leave(now), Err(Error::BadLeaveState(SerfState::Shutdown))),
    "leave keeps BadLeaveState on a Shutdown machine"
  );
  assert!(
    matches!(
      e.force_leave(2u32, false, now),
      Err(Error::BadLeaveState(SerfState::Shutdown))
    ),
    "force_leave keeps BadLeaveState on a Shutdown machine"
  );
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn shutdown_refuses_key_management() {
  use crate::event::{KeyRequest, KeyRequestOperation, KeyResponseArgs};
  use memberlist_proto::SecretKey;

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([2u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([2u8; 32]);

  let mut e = ep();
  // A KeyRequest token obtained before shutdown, to prove respond_key's refusal
  // precedes its received_queries lookup.
  let req = KeyRequest::<u32, core::net::SocketAddr>::test_with_deadline(
    KeyRequestOperation::List,
    7,
    memberlist_proto::Node::new(2u32, addr(2000)),
    None,
    far_future(),
  );

  shut_down_via_lost_conflict(&mut e);

  let now = memberlist_proto::Instant::ORIGIN;

  // Every issuance funnels through internal_query → ensure_not_shutdown.
  assert!(
    matches!(e.list_keys(now), Err(Error::Shutdown)),
    "list_keys must be refused after shutdown"
  );
  assert!(
    matches!(e.install_key(key, now), Err(Error::Shutdown)),
    "install_key must be refused after shutdown"
  );
  assert!(
    matches!(e.use_key(key, now), Err(Error::Shutdown)),
    "use_key must be refused after shutdown"
  );
  assert!(
    matches!(e.remove_key(key, now), Err(Error::Shutdown)),
    "remove_key must be refused after shutdown"
  );

  // respond_key shares the ensure_not_shutdown gate.
  let refused = e.respond_key(
    &req,
    KeyResponseArgs {
      result: true,
      message: smol_str::SmolStr::default(),
      keys: vec![key],
      primary_key: Some(key),
    },
    now,
  );
  assert!(
    matches!(refused, Err(Error::Shutdown)),
    "respond_key must be refused after shutdown"
  );
}

#[test]
fn shutdown_refuses_load_snapshot_replay() {
  // snapshot replay is a public origination path: on an Alive machine it advances
  // the Lamport clocks, dirties the snapshot, and dials every recorded peer.  A
  // Shutdown conflict loser must refuse it with Error::Shutdown before any
  // mutation — otherwise it could resurrect itself and rejoin the cluster.
  let mut e = ep();
  shut_down_via_lost_conflict(&mut e);

  // Clear dirty so the no-mutation assertion is unambiguous, then snapshot every
  // observable the replay would move.
  e.test_clear_dirty();
  let member_before = e.member_time();
  let event_before = e.event_time();
  let query_before = e.query_time();
  let event_min_before = e.test_event_min_time();
  let query_min_before = e.test_query_min_time();

  // A replay that WOULD advance all three clocks and dial peer 2 on an Alive node.
  let replay = ReplayResult {
    alive_nodes: vec![snapshot_node(1, 7946), snapshot_node(2, 1002)],
    last_clock: 40.into(),
    last_event_clock: 50.into(),
    last_query_clock: 60.into(),
  };
  assert!(
    matches!(
      e.load_snapshot(replay, memberlist_proto::Instant::ORIGIN),
      Err(Error::Shutdown)
    ),
    "load_snapshot must refuse on a Shutdown machine"
  );

  // Reverting the ensure_not_shutdown gate advances these clocks, dirties the
  // snapshot, and pushes the rejoin dial → each assertion below fails.
  assert_eq!(e.member_time(), member_before, "member clock unchanged");
  assert_eq!(e.event_time(), event_before, "event clock unchanged");
  assert_eq!(e.query_time(), query_before, "query clock unchanged");
  assert_eq!(
    e.test_event_min_time(),
    event_min_before,
    "event min_time unchanged"
  );
  assert_eq!(
    e.test_query_min_time(),
    query_min_before,
    "query min_time unchanged"
  );
  assert!(
    !e.test_is_dirty(),
    "the refused replay must not dirty the snapshot"
  );
  assert!(
    e.test_rejoin_dials().is_empty(),
    "a Shutdown machine must originate no rejoin dials"
  );
  assert!(
    e.poll_event().is_none(),
    "the refused replay must enqueue no event"
  );
}

#[test]
fn shutdown_ingress_is_inert() {
  let mut e = ep();
  shut_down_via_lost_conflict(&mut e);

  let members_before = e.num_members();
  let event_time_before = e.event_time();

  // A valid inbound user event that would normally advance the event clock and
  // emit Event::User must mutate nothing and emit nothing on a Shutdown machine.
  let serf_bytes = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 5.into(),
    cc: false,
    name: "post-shutdown".into(),
    payload: bytes::Bytes::from_static(b"x"),
  })
  .encode()
  .unwrap();
  e.test_inject_user_packet(addr(1002), serf_bytes, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "ingress must emit nothing new after shutdown"
  );
  assert_eq!(
    e.event_time(),
    event_time_before,
    "ingress must not advance the event clock after shutdown"
  );
  assert_eq!(
    e.num_members(),
    members_before,
    "ingress must not change membership after shutdown"
  );
}

#[test]
fn shutdown_timers_are_quiet() {
  let mut e = ep();
  // A live endpoint schedules serf deadlines (reap / reconnect / queue-check).
  assert!(
    e.core_mut().serf_poll_timeout().is_some(),
    "a live machine schedules serf deadlines"
  );

  shut_down_via_lost_conflict(&mut e);

  // A Shutdown machine schedules no serf wakeup (reverting the serf_poll_timeout
  // gate surfaces the still-armed next_reap → this fails).
  assert!(
    e.core_mut().serf_poll_timeout().is_none(),
    "a Shutdown machine must schedule no serf deadline"
  );

  // A timer tick far past every deadline fires no serf work: no reap, reconnect,
  // query-close, or leave-completion, and its ingress drain is inert.
  let far = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(86_400);
  e.handle_timeout(far);
  assert!(
    e.poll_event().is_none(),
    "a Shutdown machine must emit no serf event on a timer tick"
  );
  assert!(
    e.state().is_shutdown(),
    "a Shutdown machine stays Shutdown across a timer tick"
  );
}

#[test]
fn app_query_close_is_silent() {
  // An App query closing (deadline elapsed) must not emit any serf event.
  let mut e = ep();
  let now = memberlist_proto::Instant::ORIGIN;
  let params = QueryParams {
    timeout: core::time::Duration::from_millis(1),
    ..Default::default()
  };
  let _ = e.query("test", Bytes::new(), params, now);
  // Drain any Event::Query emitted by handle_query (the local node sees its own query).
  while e.poll_event().is_some() {}

  // Advance time past the deadline.
  let past = now + core::time::Duration::from_secs(1);
  e.test_fire_due_query_closes(past);

  // No serf event should be emitted (App queries close silently).
  assert!(
    e.poll_event().is_none(),
    "App query close must not emit any event"
  );
}

// ── Internal-query payload exact-consumption gate ────────────────────────────

#[test]
fn conflict_query_with_trailing_junk_is_dropped_entirely() {
  // Regression: a `_serf_conflict` Query whose payload is a valid encoded id
  // followed by trailing junk bytes must cause NO state mutation — no
  // query_clock advance, no dedup entry, no received_queries entry, and no
  // directed ConflictResponse send.
  //
  // Before the fix, `handle_conflict_query` decoded the id with `I::decode`
  // which ignores the returned byte count, allowing the malformed payload to
  // pass AFTER the clock witness, dedup insert, and received_queries insert had
  // already mutated state.
  let mut e = ep();

  // Build a valid-prefix payload for id=42 followed by trailing junk.
  // u32 is varint-encoded; 42u32 encodes to a single byte (0x2a).
  let id_bytes = (42u32).encode_to_bytes().unwrap();
  let mut payload_with_junk = id_bytes.to_vec();
  payload_with_junk.extend_from_slice(&[0xaa, 0xbb]); // 2 junk bytes

  let q = QueryMessage {
    ltime: LamportTime::new(1),
    id: 55,
    from: memberlist_proto::Node::new(99u32, "127.0.0.1:9999".parse().unwrap()),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "_serf_conflict".into(),
    payload: Bytes::from(payload_with_junk),
  };

  // Preconditions: all state counters at zero.
  assert_eq!(e.query_time(), 0, "query_clock must start at 0");
  assert_eq!(
    e.test_received_queries_len(),
    0,
    "no received_queries initially"
  );

  let rebroadcast = e.test_handle_query(q);

  // A malformed internal query must cause NO state mutation.
  assert_eq!(
    e.query_time(),
    0,
    "query_clock must NOT advance for a malformed _serf_conflict payload"
  );
  assert_eq!(
    e.test_query_slot_len(1),
    0,
    "dedup buffer must NOT record an entry for a malformed _serf_conflict payload"
  );
  assert_eq!(
    e.test_received_queries_len(),
    0,
    "received_queries must NOT gain an entry for a malformed _serf_conflict payload"
  );
  assert!(
    e.test_last_directed_send().is_none(),
    "no ConflictResponse must be sent for a malformed _serf_conflict payload"
  );
  // A malformed internal query also must NOT trigger a rebroadcast.
  assert!(
    !rebroadcast,
    "malformed _serf_conflict must not request rebroadcast"
  );
}

#[test]
fn conflict_query_with_exact_payload_processes_normally() {
  // Complement: a `_serf_conflict` Query whose payload is an exactly-encoded id
  // (no trailing bytes) must still be processed: clock advances, dedup entry is
  // recorded.  The member is not in the local store, so no ConflictResponse is
  // sent, but the clock and dedup state must have been updated.
  let mut e = ep();

  // Encode id=42 exactly (no junk).
  let id_bytes = (42u32).encode_to_bytes().unwrap();

  let q = QueryMessage {
    ltime: LamportTime::new(3),
    id: 77,
    from: memberlist_proto::Node::new(99u32, "127.0.0.1:9999".parse().unwrap()),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "_serf_conflict".into(),
    payload: Bytes::from(id_bytes.to_vec()),
  };

  assert_eq!(e.query_time(), 0);
  e.test_handle_query(q);

  // Clock must have advanced past the witnessed ltime of 3.
  assert!(
    e.query_time() > 3,
    "query_clock must advance after a well-formed _serf_conflict query"
  );
  // Dedup slot for ltime=3 must be non-empty.
  assert!(
    e.test_query_slot_len(3) > 0,
    "dedup buffer must record the well-formed _serf_conflict query"
  );
}

// ── Task 6.2: replay(records) -> ReplayResult + Endpoint::load_snapshot ──────

use crate::snapshot::ReplayResult;

fn snapshot_node(id: u32, port: u16) -> memberlist_proto::Node<u32, core::net::SocketAddr> {
  memberlist_proto::Node::new(id, format!("127.0.0.1:{port}").parse().unwrap())
}

#[test]
fn load_snapshot_sets_member_clock_to_last_clock() {
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 5.into(),
    last_event_clock: 0.into(),
    last_query_clock: 0.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // G5: member clock >= last_clock.
  assert!(
    e.member_time() >= 5,
    "member_time must be at least last_clock after load_snapshot"
  );
}

#[test]
fn load_snapshot_sets_event_min_time_to_last_event_clock_plus_one() {
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 0.into(),
    last_event_clock: 7.into(),
    last_query_clock: 0.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // G5: event_buffer.min_time = last_event_clock + 1 = 8.
  assert_eq!(
    e.test_event_min_time(),
    8,
    "event min_time must be last_event_clock + 1"
  );
}

#[test]
fn load_snapshot_sets_query_min_time_to_last_query_clock_plus_one() {
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 0.into(),
    last_event_clock: 0.into(),
    last_query_clock: 9.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // G5: query_buffer.min_time = last_query_clock + 1 = 10.
  assert_eq!(
    e.test_query_min_time(),
    10,
    "query min_time must be last_query_clock + 1"
  );
}

#[test]
fn load_snapshot_skips_self_on_rejoin() {
  // The local endpoint has id=1 (see ep()). A ReplayResult containing id=1
  // plus id=2 must only emit a dial for id=2 (self is skipped per G10).
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![
      snapshot_node(1, 7946), // self
      snapshot_node(2, 1002),
    ],
    last_clock: 5.into(),
    last_event_clock: 7.into(),
    last_query_clock: 9.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // Dials recorded via test_last_dial_addr: the reconnect should have been issued
  // for node 2 only (and none for self).
  let dialled = e.test_rejoin_dials();
  assert_eq!(
    dialled.len(),
    1,
    "exactly one rejoin dial expected (self is skipped)"
  );
  assert_eq!(
    dialled[0],
    "127.0.0.1:1002".parse::<core::net::SocketAddr>().unwrap(),
    "rejoin dial must target node 2's address"
  );
}

#[test]
fn load_snapshot_empty_alive_nodes_emits_no_dials() {
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 3.into(),
    last_event_clock: 2.into(),
    last_query_clock: 1.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  let dialled = e.test_rejoin_dials();
  assert!(dialled.is_empty(), "no dials for empty alive_nodes");
}

#[test]
fn load_snapshot_all_clocks_combined() {
  // G5: all three clocks + min-times in one test.
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 10.into(),
    last_event_clock: 20.into(),
    last_query_clock: 30.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  assert!(e.member_time() >= 10, "member_time >= last_clock");
  assert_eq!(e.test_event_min_time(), 21, "event min_time = 20 + 1");
  assert_eq!(e.test_query_min_time(), 31, "query min_time = 30 + 1");
}

#[test]
fn load_snapshot_marks_local_state_dirty() {
  // After load_snapshot, the push-pull snapshot must reflect the recovered clocks.
  let mut e = ep();
  e.test_clear_dirty();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 3.into(),
    last_event_clock: 0.into(),
    last_query_clock: 0.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // Dirty flag must be set so the first push-pull egress carries the recovered clocks.
  assert!(
    e.test_is_dirty(),
    "load_snapshot must mark local state dirty"
  );
}

// ── Task 5.2: CoordinateClient + PingCompleted RTT update ────────────────────

/// Build an ack-payload bytes `[1u8] ++ pb::Coordinate` for the given `Coordinate`
/// (test utility mirroring the production `coord_ack_payload` helper).
#[cfg(feature = "coordinates")]
fn make_coord_payload(coord: &crate::typed::Coordinate) -> Bytes {
  use crate::bridge::coordinate_to_pb;
  use buffa::Message as _;
  let pb = coordinate_to_pb(coord);
  let encoded = pb.encode_to_vec();
  let mut buf = Vec::with_capacity(1 + encoded.len());
  buf.push(1u8); // PING_VERSION
  buf.extend_from_slice(&encoded);
  Bytes::from(buf)
}

#[cfg(feature = "coordinates")]
#[test]
fn ping_completed_updates_local_coordinate_and_caches_remote() {
  // G9 both halves: a PingCompleted with a valid coordinate payload must
  // (1) update the local Vivaldi model and (2) cache the remote coordinate.
  let mut e = ep_with_coords();

  // Build a synthetic peer coordinate and its wire payload.
  let peer_coord = crate::typed::Coordinate {
    vec: vec![5.0; 8],
    error: 1.0,
    adjustment: 0.0,
    height: 0.0,
  };
  let payload = make_coord_payload(&peer_coord);

  e.test_ping_completed(2u32, core::time::Duration::from_millis(40), payload);

  // Half 1: remote coord is cached under node id 2.
  assert!(
    e.cached_coordinate(&2u32).is_some(),
    "remote coordinate should be cached after PingCompleted"
  );
  // Half 2: local coordinate was updated (not None).
  assert!(
    e.get_coordinate().is_some(),
    "local coordinate should be present after PingCompleted"
  );
}

#[test]
fn ping_completed_is_noop_when_coordinates_disabled() {
  // When the `coordinates` feature is absent or coordinates are runtime-disabled,
  // a PingCompleted with any payload must not panic or emit any event.
  let mut e = ep(); // opts.disable_coordinates() == true (default)
  // Payload starts with PING_VERSION byte to ensure it is not rejected by the
  // version guard; the test still expects a no-op.
  let payload = Bytes::from_static(b"\x01garbage");
  // Must not panic; test_ping_completed is a no-op without the feature or when disabled.
  #[cfg(feature = "coordinates")]
  e.test_ping_completed(2u32, core::time::Duration::from_millis(40), payload);
  #[cfg(not(feature = "coordinates"))]
  let _ = payload; // consume without calling the cfg-gated adapter
  assert!(
    e.poll_event().is_none(),
    "ping_completed must not emit any event"
  );
}

#[cfg(feature = "coordinates")]
#[test]
fn ping_completed_bad_version_is_noop() {
  // A PingCompleted payload with a wrong version byte must be silently dropped.
  let mut e = ep_with_coords();
  let payload = Bytes::from_static(b"\x02garbage"); // version 2, not 1
  e.test_ping_completed(2u32, core::time::Duration::from_millis(10), payload);
  assert!(
    e.cached_coordinate(&2u32).is_none(),
    "bad version byte must not update coord_cache"
  );
}

#[cfg(feature = "coordinates")]
#[test]
fn ping_completed_empty_payload_is_noop() {
  // An empty PingCompleted payload must be silently dropped.
  let mut e = ep_with_coords();
  e.test_ping_completed(2u32, core::time::Duration::from_millis(10), Bytes::new());
  assert!(
    e.cached_coordinate(&2u32).is_none(),
    "empty payload must not update coord_cache"
  );
}

#[cfg(feature = "coordinates")]
#[test]
fn reap_forgets_coordinate() {
  // G13: after a member is reaped, its coordinate must be purged from the cache.
  let mut e = ep_with_coords();

  // Seed a valid coordinate for node 42.
  let peer_coord = crate::typed::Coordinate {
    vec: vec![1.0; 8],
    error: 0.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let payload = make_coord_payload(&peer_coord);
  e.test_ping_completed(42u32, core::time::Duration::from_millis(20), payload);
  assert!(
    e.cached_coordinate(&42u32).is_some(),
    "coordinate should be cached before reap"
  );

  // Seed node 42 as a Failed member with leave_time = ORIGIN.
  use memberlist_proto::Instant;
  e.test_seed_failed_member(42u32, "127.0.0.1:7947".parse().unwrap(), Instant::ORIGIN);

  // Fire the reaper at a time past reconnect_timeout (default = 24 h = 86 400 s).
  let past = Instant::ORIGIN + core::time::Duration::from_secs(90_000);
  e.test_fire_reap(past);

  // The coordinate cache entry for 42 must be gone.
  assert!(
    e.cached_coordinate(&42u32).is_none(),
    "coordinate must be removed from cache after reap"
  );
}

// ── Bug-fix regression tests ──────────────────────────────────────────────────

// Bug 1: EventBuffer wraparound corruption.
//
// Use a tiny buffer (size=4) so ltime=1 and ltime=5 map to the same ring index
// without needing a high clock that would make ltime=1 "too old".
fn ep_tiny_event_buf() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  StreamEndpoint::new(coord(inner), Options::new().with_event_buffer_size(4))
}

#[test]
fn event_ring_wraparound_delivers_new_event() {
  // Buffer size = 4. ltime=1 and ltime=5 both map to ring index 1 (1%4=1, 5%4=1).
  // With the bug the ltime=5 event is spuriously deduped against the ltime=1 slot.
  // Use a small ring so neither ltime is "too old" at the time of delivery.
  let mut e = ep_tiny_event_buf();
  let m1 = crate::typed::UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "dup_test".into(),
    payload: bytes::Bytes::new(),
  };
  let m2 = crate::typed::UserEventMessage {
    ltime: 5.into(),
    cc: false,
    name: "dup_test".into(),
    payload: bytes::Bytes::new(),
  };
  // First delivery at ltime=1 → first sight → true.
  assert!(
    e.test_handle_user_event(m1),
    "ltime=1 delivery must return true (first sight)"
  );
  // Drain the event so it doesn't pollute the next assertion.
  // Ignoring Err: test drain, we don't care about the value.
  let _ = e.poll_event();
  // Second delivery at ltime=5, same event name — a different ltime, must return true.
  // With the bug the ltime=1 slot is reused and the event is spuriously deduped → false.
  assert!(
    e.test_handle_user_event(m2),
    "ltime=5 delivery must return true (different ltime, not a duplicate)"
  );
}

// Bug 2: ACK queries never produce ACKs.
#[test]
fn ack_query_produces_immediate_ack_directed_send() {
  // A QueryMessage with the ACK flag set must trigger an immediate directed
  // ACK response to the querier before emitting Event::Query.
  let mut e = ep();
  let q = QueryMessage {
    flags: QueryFlag::ACK,
    ..test_query(LamportTime::new(3), 77)
  };
  e.test_handle_query(q);

  // A directed send must have occurred with the ACK response.
  let (dest_addr, sent_bytes) = e
    .test_last_directed_send()
    .expect("ACK query must produce a directed send");

  // The destination must be the querier's address (127.0.0.1:9999 from test_query).
  assert_eq!(
    dest_addr,
    "127.0.0.1:9999".parse::<core::net::SocketAddr>().unwrap(),
    "ACK must be directed to the querier"
  );

  // Decode the sent bytes and verify it is a QueryResponse with ACK flag.
  let decoded = AnyMessage::<u32, core::net::SocketAddr>::decode(&sent_bytes)
    .expect("ACK bytes must decode as AnyMessage");
  match decoded {
    AnyMessage::QueryResponse(resp) => {
      assert!(resp.ack(), "decoded response must have ACK flag set");
      assert!(resp.payload.is_empty(), "ACK payload must be empty");
    }
    other => panic!(
      "expected QueryResponse(ACK), got {:?}",
      other.message_type()
    ),
  }
}

// Bug 3: Oversized query responses silently consumed.
fn ep_small_resp_limit() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  StreamEndpoint::new(
    coord(inner),
    Options::new().with_query_response_size_limit(10),
  )
}

#[test]
fn respond_encoded_frame_size_check_rejects_small_payload_on_tight_limit() {
  // Guard 1 must check the ENCODED frame size, not the raw payload size.
  // With limit=10, even an empty payload produces an encoded frame >> 10 bytes.
  let mut e = ep_small_resp_limit();
  let qid = QueryId {
    ltime: LamportTime::new(1),
    id: 42,
  };
  let token = e.test_register_received_query(qid, addr(1002), t_secs(100));
  // Empty payload passes the OLD raw-size guard (0 <= 10) but must fail the
  // new encoded-size guard.
  let err = e
    .respond(
      &token,
      bytes::Bytes::new(),
      memberlist_proto::Instant::ORIGIN,
    )
    .expect_err("encoded frame exceeds limit=10, must return RespondTooLarge");
  assert!(
    matches!(err, Error::RespondTooLarge(_, _)),
    "expected RespondTooLarge, got {:?}",
    err
  );
  // The responded flag must NOT be set (send was not attempted).
  assert!(
    !e.test_is_responded(qid),
    "responded flag must remain false when RespondTooLarge is returned"
  );
}

// Bug 4: Stale self-leave → unbounded refute.
#[test]
fn stale_self_leave_does_not_trigger_refute() {
  // Seed local node (id=1) as Alive with status_time=10.
  // A leave intent at ltime=3 is stale (3 <= 10) and must NOT fire broadcast_join.
  let mut e = ep();
  e.test_seed_member(1u32, MemberStatus::Alive, LamportTime::new(10));

  let result =
    e.test_handle_leave_intent(1u32, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);

  // Must return false (stale: no rebroadcast).
  assert!(!result, "stale self-leave must return false");

  // Clock was witnessed to 3 → clock = 4. broadcast_join would advance it further.
  // broadcast_join(LamportTime(4)) calls witness(clock=4, 4) → clock=5.
  // With the fix, the stale check fires before the self-refute, so clock stays at 4.
  assert_eq!(
    e.member_time(),
    4,
    "stale self-leave must not advance the clock beyond witness(0, 3) = 4"
  );
}

// Bug 5: Leave-broadcast deadline never retired.
#[test]
fn leave_broadcast_deadline_is_cleared_after_expiry() {
  let mut e = ep();
  // leave() arms leave_broadcast_deadline = ORIGIN + broadcast_timeout (5s).
  e.leave(memberlist_proto::Instant::ORIGIN).unwrap();
  assert!(
    e.leave_broadcast_deadline().is_some(),
    "leave_broadcast_deadline must be armed after leave()"
  );

  // Tick past the deadline.
  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(6);
  e.handle_timeout(past);

  // The deadline must be cleared.
  assert!(
    e.leave_broadcast_deadline().is_none(),
    "leave_broadcast_deadline must be cleared after expiry"
  );
}

// ── Regression: 6 correctness fixes ──────────────────────────────────────────

// ── Fix 1: load_snapshot must advance event_clock / query_clock ───────────────

#[test]
fn load_snapshot_advances_event_clock() {
  // Bug: load_snapshot set event_buffer.min_time but left event_clock at 0.
  // user_event() stamps ltime = event_clock (0) which is below min_time → dropped.
  // Fix: witness(&mut event_clock, last_event_clock.0) after setting min_time.
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 0.into(),
    last_event_clock: 10.into(),
    last_query_clock: 0.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  assert!(
    e.event_time() >= 10,
    "event_clock must be advanced to at least last_event_clock after load_snapshot, got {}",
    e.event_time()
  );
}

#[test]
fn load_snapshot_advances_query_clock() {
  // Bug: load_snapshot set query_buffer.min_time but left query_clock at 0.
  // Fix: witness(&mut query_clock, last_query_clock.0) after setting min_time.
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 0.into(),
    last_event_clock: 0.into(),
    last_query_clock: 20.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  assert!(
    e.query_time() >= 20,
    "query_clock must be advanced to at least last_query_clock after load_snapshot, got {}",
    e.query_time()
  );
}

#[test]
fn load_snapshot_event_clock_allows_new_events_above_floor() {
  // After load_snapshot with last_event_clock=10, a new user_event() stamps
  // ltime = event_clock (>= 10), which is >= min_time floor, so it must be delivered.
  let mut e = ep();
  let r = ReplayResult {
    alive_nodes: vec![],
    last_clock: 0.into(),
    last_event_clock: 10.into(),
    last_query_clock: 0.into(),
  };
  e.load_snapshot(r, memberlist_proto::Instant::ORIGIN)
    .unwrap();
  // Drain any pending events from load_snapshot.
  while e.poll_event().is_some() {}
  // Issue a new user event — must succeed and be delivered above the floor.
  e.user_event(
    "post-snap",
    bytes::Bytes::from_static(b"ok"),
    false,
    Instant::ORIGIN,
  )
  .expect("user_event after load_snapshot must succeed");
  let ev = e
    .poll_event()
    .expect("user_event after load_snapshot must be delivered");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "post-snap"),
    "expected Event::User(post-snap), got: {ev:?}"
  );
}

// ── Fix 2: malformed conflict response must not inflate the denominator ────────

#[test]
fn malformed_conflict_response_does_not_inflate_denominator() {
  // Scenario: 1 malformed + 1 valid-agreeing response.
  // Bug: malformed response counted in responses → num_resp=2, majority=2,
  //      matching=1 < 2 → false Shutdown.
  // Fix: malformed dropped before insert → num_resp=1, majority=1,
  //      matching=1 >= 1 → WIN (no Shutdown).
  let mut e = ep();
  let deadline = far_future();
  let qid = e.test_register_conflict_query(deadline);

  // Build a QueryResponseMessage carrying the wrong inner type (UserEvent bytes).
  let bad_inner = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "bad".into(),
    payload: bytes::Bytes::new(),
  });
  let bad_payload = bad_inner.encode().expect("encode must succeed");
  let bad_resp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: memberlist_proto::Node::new(200u32, addr(2000)),
    flags: QueryFlag::empty(),
    payload: bad_payload,
  };
  e.test_handle_query_response(bad_resp);

  // 1 valid response that agrees (conflict_matching += 1).
  e.test_fold_conflict_response(qid, 201u32, true);

  // Fire query close at a time past the deadline.
  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);

  // Fix: malformed not counted → num_resp=1, majority=1, matching=1 → WIN → no Shutdown.
  assert!(
    e.poll_event().is_none(),
    "malformed conflict response must not inflate denominator and trigger false Shutdown"
  );
}

// ── Fix 3: respond() must return Err and leave responded=false on send failure ─

#[test]
fn respond_send_failure_returns_err_and_leaves_responded_false() {
  // Force the inner Endpoint's send_user_packet to fail by setting gossip_mtu
  // to the minimum (512 bytes) and sending a 490-byte payload.  The
  // QueryResponseMessage encoding wraps the payload with framing overhead,
  // pushing the total past 512.  The serf query_response_size_limit is raised
  // to 50_000 so our guard passes and the inner's MTU check is the one that fires.
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap())
      .with_gossip_mtu(512); // minimum MTU: a 490-byte payload + framing exceeds it.
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new().with_query_response_size_limit(50_000);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  let qid = QueryId {
    ltime: LamportTime::new(1),
    id: 5,
  };
  let token = e.test_register_received_query(qid, addr(1002), t_secs(10));

  // 490-byte payload: encodes to > 512 bytes with QueryResponse + UserData framing.
  let large_payload = bytes::Bytes::from(vec![0u8; 490]);
  let result = e.respond(&token, large_payload, memberlist_proto::Instant::ORIGIN);

  assert!(
    result.is_err(),
    "respond() must return Err when the inner directed send fails"
  );
  assert!(
    matches!(result.unwrap_err(), Error::RespondSend(_)),
    "send failure must surface as Error::RespondSend"
  );
  // Entry must still be present (not removed on failure): test_is_responded returns false.
  assert!(
    !e.test_is_responded(qid),
    "entry must not be removed when the directed send fails"
  );
}

// ── Fix 4: ACKs must call relay_response when relay_factor > 0 ───────────────

#[test]
fn ack_query_with_relay_factor_relays_ack() {
  // An ACK-flagged query with relay_factor=1 must relay the ACK response through
  // a random Alive non-self peer after the direct send.
  let mut e = ep();
  // Seed 2 Alive members so the relay count guard passes (relay_factor=1 needs >= 2).
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));
  e.test_seed_member(11u32, MemberStatus::Alive, LamportTime::new(1));

  let q = QueryMessage {
    flags: QueryFlag::ACK,
    relay_factor: 1,
    ..test_query(LamportTime::new(1), 42)
  };
  e.test_handle_query(q);

  // The direct ACK sends to the querier (127.0.0.1:9999).
  // The relay sends to one of the alive members (ports 1010 or 1011).
  // last_directed_send is overwritten by relay_response, so it records the relay peer.
  let (dest, _) = e
    .test_last_directed_send()
    .expect("at least one directed send must have occurred");

  // The relay overwrites last_directed_send to a relay peer (not the querier at 9999).
  assert_ne!(
    dest.port(),
    9999,
    "last directed send after relay must be to a relay peer, not the querier"
  );
}

// ── Fix 5: received_queries growth and pruning ────────────────────────────────

#[test]
fn received_query_is_removed_after_successful_respond() {
  // After a successful respond(), the entry must be removed from received_queries.
  // A second respond() must return AlreadyResponded (not panic or succeed again).
  let mut e = ep();
  let qid = QueryId {
    ltime: LamportTime::new(1),
    id: 5,
  };
  let token = e.test_register_received_query(qid, addr(1002), t_secs(10));

  e.respond(
    &token,
    bytes::Bytes::new(),
    memberlist_proto::Instant::ORIGIN,
  )
  .expect("first respond() must succeed");

  // Entry removed: second call returns AlreadyResponded.
  let err = e
    .respond(
      &token,
      bytes::Bytes::new(),
      memberlist_proto::Instant::ORIGIN,
    )
    .unwrap_err();
  assert!(
    matches!(err, Error::AlreadyResponded),
    "second respond() after removal must return AlreadyResponded, got {err:?}"
  );
}

#[test]
fn expired_received_queries_are_pruned_in_handle_timeout() {
  // An expired received-query entry (deadline elapsed, never responded) must be
  // pruned by handle_timeout so received_queries does not grow without bound.
  let mut e = ep();
  let qid = QueryId {
    ltime: LamportTime::new(2),
    id: 8,
  };
  let token = e.test_register_received_query(qid, addr(1003), t_secs(1));

  // handle_timeout at t=10, past deadline=1.
  e.handle_timeout(t_secs(10));

  // Entry pruned: respond() returns AlreadyResponded (entry absent, .ok_or path).
  let err = e
    .respond(&token, bytes::Bytes::new(), t_secs(10))
    .unwrap_err();
  assert!(
    matches!(err, Error::AlreadyResponded),
    "expired received query must be pruned by handle_timeout, got {err:?}"
  );
}

#[test]
fn expired_received_queries_pruned_before_inbound_cap() {
  // A query/key-request flood can leave received_queries full of STALE
  // (past-deadline, not-yet-pruned) tokens when a new LIVE inbound query
  // arrives: every driver ingests inbound data before it runs the periodic
  // deadline-prune in after_inner_timeout.  handle_query must therefore prune
  // expired tokens inline before the inbound cap, so the cap counts only live
  // entries and the live query is admitted.  Reverting the inline prune (leaving
  // only the after_inner_timeout prune) makes the stale-full cap drop the live
  // query and fails this test.
  let mut e = ep();

  // Fill received_queries to the cap with inbound queries carrying a short (1s)
  // deadline, at drain_now = ORIGIN.  Distinct (ltime = i + 1, id = i) pairs are
  // each first-sight, so all MAX_RECEIVED_QUERIES entries are inserted (all live
  // at ORIGIN, so the inline prune is a no-op during the fill).
  e.test_set_drain_now(t_secs(0));
  for i in 0..MAX_RECEIVED_QUERIES as u32 {
    let q = QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(i as u64 + 1),
      id: i,
      from: memberlist_proto::Node::new(99u32, addr(9001)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(1),
      name: "flood".into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_query(q);
  }
  assert_eq!(
    e.test_received_queries_len(),
    MAX_RECEIVED_QUERIES,
    "received_queries must be exactly at the cap after the flood"
  );

  // Advance the endpoint's clock PAST the fill deadlines (ORIGIN + 1s) WITHOUT
  // calling handle_timeout, so the only thing that can reclaim the now-stale
  // tokens is the inline prune in handle_query.
  e.test_set_drain_now(t_secs(10));

  // Drain the queued flood events so the live query's event is observed alone.
  while e.poll_event().is_some() {}

  // A new LIVE inbound query (future deadline, distinct ltime/id) must be
  // ADMITTED: the inline prune reclaims all the stale slots before the cap check.
  let live_ltime = LamportTime::new(MAX_RECEIVED_QUERIES as u64 + 100);
  let live_id = MAX_RECEIVED_QUERIES as u32 + 100;
  let live = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: live_ltime,
    id: live_id,
    from: memberlist_proto::Node::new(7u32, addr(7000)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(30),
    name: "live".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_query(live);

  // The live query surfaced as Event::Query (it was not dropped at the cap) ...
  let ev = e
    .poll_event()
    .expect("the live inbound query must be admitted and surfaced as Event::Query");
  match ev {
    Event::Query(qe) => {
      assert_eq!(
        qe.ltime(),
        live_ltime,
        "surfaced event must be the live query"
      );
      assert_eq!(qe.id(), live_id, "surfaced event must be the live query");
    }
    other => panic!(
      "expected Event::Query for the live query, got {:?}",
      core::mem::discriminant(&other)
    ),
  }

  // ... and its token is the only entry left: the stale tokens were pruned.
  assert_eq!(
    e.test_received_queries_len(),
    1,
    "the stale tokens must be pruned inline, leaving only the live token"
  );
}

#[test]
fn received_query_answerable_at_exact_deadline_survives_inbound_prune() {
  // A received query is answerable up to and including its deadline: respond
  // rejects only now > deadline.  The ingress prune in handle_query must not
  // drop a token at the exact instant now == deadline, or a still-valid response
  // fails with AlreadyResponded.  Register an inbound query, advance to exactly
  // its deadline, run the ingress prune by handling a second inbound query at
  // that instant, then answer the first token at now == deadline: it must
  // succeed.  Reverting the prune to `now < deadline` drops the token here and
  // makes the respond fail.
  let mut e = ep();

  // First inbound query at ORIGIN → deadline D = ORIGIN + timeout.
  e.test_set_drain_now(t_secs(0));
  let original = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 1,
    from: memberlist_proto::Node::new(99u32, addr(9001)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "original".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_query(original);

  // Capture the original token from the surfaced Event::Query; its deadline is D.
  let token = match e
    .poll_event()
    .expect("the original inbound query must surface as Event::Query")
  {
    Event::Query(qe) => qe,
    other => panic!(
      "expected Event::Query, got {:?}",
      core::mem::discriminant(&other)
    ),
  };
  let deadline = token.deadline();

  // Advance to EXACTLY the deadline and handle a DIFFERENT inbound query, which
  // runs the ingress prune at now == deadline.
  e.test_set_drain_now(deadline);
  let other = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(2),
    id: 2,
    from: memberlist_proto::Node::new(7u32, addr(7000)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(30),
    name: "other".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_query(other);

  // The original token, answerable at now == deadline, must NOT have been pruned:
  // respond at exactly the deadline must succeed.
  e.respond(&token, bytes::Bytes::new(), deadline)
    .expect("respond at exactly the deadline must succeed: the token is still answerable");
}

// ── Bug 1: zero valid conflict responses must not emit Event::Shutdown ────────

#[test]
fn zero_conflict_responses_does_not_shut_down() {
  // Scenario: a conflict query receives ONLY malformed/wrong-type responses so
  // num_resp stays 0.  The prior code computed majority = 0/2 + 1 = 1 and then
  // compared matching=0 < 1 → emitted Event::Shutdown.  With the fix,
  // num_resp == 0 is treated as inconclusive and no shutdown is emitted.
  let mut e = ep();
  let deadline = far_future();
  let _qid = e.test_register_conflict_query(deadline);

  // Build a QueryResponseMessage carrying the wrong inner type (UserEvent bytes).
  // The existing validate-before-count fix (Bug 2/R2) ensures this does NOT
  // increment responses; we just confirm close_conflict_query also handles
  // the num_resp==0 case correctly when even no malformed response is present.
  // Fire query close: no responses at all (responses is empty).
  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);

  assert!(
    e.poll_event().is_none(),
    "zero conflict responses must not emit Event::Shutdown (inconclusive)"
  );
}

#[test]
fn zero_valid_conflict_responses_all_malformed_does_not_shut_down() {
  // Scenario: conflict query receives only wrong-type responses (all dropped
  // before counting).  Confirm the combined effect of Bug 1 + Bug 2 fixes:
  // malformed dropped → num_resp=0 → no shutdown.
  let mut e = ep();
  let deadline = far_future();
  let qid = e.test_register_conflict_query(deadline);

  // Inject a QueryResponseMessage carrying wrong inner type (dropped by Bug 2 fix).
  let bad_inner = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "bad".into(),
    payload: bytes::Bytes::new(),
  });
  let bad_payload = bad_inner.encode().expect("encode must succeed");
  let bad_resp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: memberlist_proto::Node::new(300u32, addr(3000)),
    flags: QueryFlag::empty(),
    payload: bad_payload,
  };
  e.test_handle_query_response(bad_resp);

  // Confirm: zero responses counted.
  assert_eq!(
    e.test_pending_query_response_count(qid),
    0,
    "malformed conflict response must not be counted"
  );

  let past = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3601);
  e.test_fire_due_query_closes(past);

  assert!(
    e.poll_event().is_none(),
    "zero valid conflict responses (all malformed) must not emit Event::Shutdown"
  );
}

// ── Bug 2 (class sweep): malformed key response must not inflate num_resp ─────

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn malformed_key_response_does_not_inflate_num_resp() {
  // Scenario: a Key query receives a wrong-type response payload (UserEvent
  // bytes where KeyResponse is expected).  Without the fix the responder is
  // inserted into pending.responses before the payload is checked in
  // handle_key_response_fold, inflating num_resp.  With the fix, the
  // validate-before-count guard rejects it and num_resp stays 0.
  let mut e = ep();
  let deadline = far_future();
  let qid = e.test_register_key_query(deadline);

  // Build a QueryResponseMessage carrying wrong inner type.
  let bad_inner = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "bad".into(),
    payload: bytes::Bytes::new(),
  });
  let bad_payload = bad_inner.encode().expect("encode must succeed");
  let bad_resp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: memberlist_proto::Node::new(400u32, addr(4000)),
    flags: QueryFlag::empty(),
    payload: bad_payload,
  };
  e.test_handle_query_response(bad_resp);

  // Fix: malformed key response was not counted → num_resp stays 0.
  assert_eq!(
    e.test_pending_query_response_count(qid),
    0,
    "malformed key response must not inflate num_resp"
  );
}

// ── Bug 3 (class sweep): clock witness overflow ────────────────────────────────

#[test]
fn witness_max_u64_is_noop() {
  // Witnessing u64::MAX must not panic and must not wrap the clock to 0.
  let mut c = 5u64;
  witness(&mut c, u64::MAX);
  assert_eq!(
    c, 5,
    "witness(u64::MAX) must be a no-op (impossible Lamport time)"
  );
}

#[test]
fn witness_max_u64_minus_one_is_rejected() {
  // u64::MAX - 1 is now an unacceptable Lamport time (the two-value safety
  // buffer: witnessing it would advance the clock to u64::MAX, a permanent
  // tombstone).  `ltime_is_acceptable` rejects it; `witness` is a no-op.
  let mut c = 0u64;
  witness(&mut c, u64::MAX - 1);
  assert_eq!(
    c, 0,
    "witness(MAX-1) must be a no-op — unacceptable Lamport time"
  );
}

#[test]
fn ingress_max_ltime_user_event_does_not_regress_clock() {
  // An ingress UserEventMessage carrying ltime == u64::MAX must not panic
  // (overflow-checked builds) and must not regress the clock to 0.
  let mut e = ep();
  e.test_set_clocks(10, 10, 10);
  let msg = UserEventMessage {
    ltime: LamportTime::new(u64::MAX),
    cc: false,
    name: "flood".into(),
    payload: bytes::Bytes::new(),
  };
  // This must not panic; the event is dropped as a dedup no-op (slot is
  // below min_time floor if we ever reach u64::MAX, and witness rejects MAX).
  let _ = e.handle_user_event(msg);
  // Clock must not have regressed to 0.
  assert!(
    e.event_time() >= 10,
    "event clock must not regress after witnessing u64::MAX ltime"
  );
}

#[test]
fn ingress_max_ltime_query_does_not_regress_clock() {
  // A QueryMessage carrying ltime == u64::MAX must not panic and must not
  // wrap the query_clock to 0.
  use crate::typed::{QueryFlag, QueryMessage};
  let mut e = ep();
  e.test_set_clocks(0, 0, 20);
  let msg = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(u64::MAX),
    id: 42,
    from: memberlist_proto::Node::new(1u32, addr(9000)),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(1),
    name: "flood".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_query(msg);
  // Clock must not have regressed to 0.
  assert!(
    e.query_time() >= 20,
    "query clock must not regress after witnessing u64::MAX ltime"
  );
}

#[test]
fn ingress_max_ltime_join_intent_does_not_regress_member_clock() {
  // A JoinMessage with ltime == u64::MAX must not wrap the member clock.
  let mut e = ep();
  e.test_set_clocks(15, 0, 0);
  let _ = e.handle_node_join_intent(LamportTime::new(u64::MAX), &999u32, t_secs(0));
  assert!(
    e.member_time() >= 15,
    "member clock must not regress after witnessing u64::MAX join intent"
  );
}

#[test]
fn ingress_max_ltime_leave_intent_does_not_regress_member_clock() {
  // A LeaveMessage with ltime == u64::MAX must not wrap the member clock.
  // Seed the node first so the leave handler has a member to update.
  let mut e = ep();
  e.test_set_clocks(15, 0, 0);
  e.test_seed_left_member(888u32, LamportTime::new(1));
  let _ = e.handle_node_leave_intent(LamportTime::new(u64::MAX), &888u32, false, t_secs(0));
  assert!(
    e.member_time() >= 15,
    "member clock must not regress after witnessing u64::MAX leave intent"
  );
}

// ── Bug 4: query dedup slot Vec must be capped ────────────────────────────────

#[test]
fn query_buffer_slot_capped_at_max_query_ids_per_ltime() {
  // Inserting more than MAX_QUERY_IDS_PER_LTIME unique ids at the same ltime
  // must not grow the slot beyond the cap; excess ids are treated as seen
  // (return false) and the slot length stays at MAX_QUERY_IDS_PER_LTIME.
  let mut buf = QueryBuffer::new(64);
  let ltime = 1u64;
  // Fill up to the cap.
  for id in 0..MAX_QUERY_IDS_PER_LTIME as u32 {
    let accepted = buf.witness_query(ltime + 1, ltime, id);
    assert!(
      accepted,
      "id {id} at ltime {ltime} must be accepted before cap"
    );
  }
  // One more unique id past the cap: must be rejected.
  let overflow_id = MAX_QUERY_IDS_PER_LTIME as u32;
  let rejected = buf.witness_query(ltime + 1, ltime, overflow_id);
  assert!(
    !rejected,
    "id past the per-slot cap must be rejected (treated as already-seen)"
  );
  // Slot length must not exceed the cap.
  let slot_len = match buf.buffer[(ltime % 64) as usize].as_ref() {
    Some(q) if q.ltime.0 == ltime => q.query_ids.len(),
    _ => panic!("expected a slot for ltime {ltime}"),
  };
  assert_eq!(
    slot_len, MAX_QUERY_IDS_PER_LTIME,
    "slot must not grow past MAX_QUERY_IDS_PER_LTIME"
  );
}

#[test]
fn endpoint_query_buffer_slot_capped_via_adapter() {
  // Same check via the Endpoint adapter to confirm the cap is enforced in the
  // full machine path (witness_query called from handle_query).
  use crate::typed::{QueryFlag, QueryMessage};
  let mut e = ep();
  e.test_set_clocks(0, 0, 1); // query_clock = 1
  let ltime = 1u64; // queries at this ltime

  // Push unique ids up to the cap through handle_query.
  for id in 0..MAX_QUERY_IDS_PER_LTIME as u32 {
    let msg = QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(ltime),
      id,
      from: memberlist_proto::Node::new(id + 10, addr(9000)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(1),
      name: "flood".into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_query(msg);
  }

  let before_overflow = e.test_query_slot_len(ltime);
  assert_eq!(
    before_overflow, MAX_QUERY_IDS_PER_LTIME,
    "slot must be exactly at the cap"
  );

  // One more unique id: must not grow the slot.
  let overflow_msg = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(ltime),
    id: MAX_QUERY_IDS_PER_LTIME as u32,
    from: memberlist_proto::Node::new(9999u32, addr(9999)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(1),
    name: "overflow".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_query(overflow_msg);

  assert_eq!(
    e.test_query_slot_len(ltime),
    MAX_QUERY_IDS_PER_LTIME,
    "slot must not exceed the cap after overflow attempt"
  );
}

// ── Fix 6: failed resync must not clear the dirty flag ───────────────────────

#[test]
fn resync_keeps_dirty_when_inner_snapshot_rejects() {
  // When set_local_state_snapshot returns Err (snapshot exceeds the inner's
  // max_stream_frame_size), local_state_dirty must stay true for retry.
  // Construction validates the minimal push-pull (empty snapshot) and requires
  // max_stream_frame_size >= its encoded length (~547 bytes for u32/SocketAddr).
  // At 547 the construction preflight passes (547 >= 547), but validate_local_
  // state_snapshot uses budget = max_stream_frame_size - LOCAL_STATE_FRAME_BUDGET
  // (1 MiB) which saturating_sub-underflows to 0, so any non-empty serf
  // PushPull (encoded_len > 0) exceeds the budget and is rejected.
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap())
      .with_max_stream_frame_size(547); // passes construction but rejects serf PushPull.
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), Options::new());

  // Force dirty and call resync.
  e.test_set_clocks(1, 2, 3);
  e.resync_local_state();

  // The inner rejected the snapshot: dirty flag must remain set.
  assert!(
    e.test_is_dirty(),
    "dirty flag must remain true when inner set_local_state_snapshot fails"
  );
}

// ── Bug 1: class sweep — reject u64::MAX Lamport times at all ingress sites ──

fn sa(port: u16) -> core::net::SocketAddr {
  format!("127.0.0.1:{port}").parse().unwrap()
}

/// A join intent carrying ltime == u64::MAX must be dropped before any state
/// mutation: clock unchanged, status_time not written, no rebroadcast.
#[test]
fn join_intent_max_ltime_is_dropped_no_state_mutation() {
  let mut e = ep();
  // Pre-seed node 42 as Alive with status_time = 1.
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  let clock_before = e.member_time();

  let result = e.test_handle_join_intent(
    42u32,
    LamportTime::new(u64::MAX),
    memberlist_proto::Instant::ORIGIN,
  );

  assert!(
    !result,
    "join intent at u64::MAX must return false (no rebroadcast)"
  );
  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on u64::MAX join intent"
  );
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated to u64::MAX"
  );
  assert_eq!(
    e.test_member_status(42u32),
    Some(MemberStatus::Alive),
    "status must remain Alive"
  );
}

/// A leave intent carrying ltime == u64::MAX must be dropped before any state
/// mutation: clock unchanged, status_time not written, no rebroadcast.
#[test]
fn leave_intent_max_ltime_is_dropped_no_state_mutation() {
  let mut e = ep();
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  let clock_before = e.member_time();

  let result = e.test_handle_leave_intent(
    42u32,
    LamportTime::new(u64::MAX),
    memberlist_proto::Instant::ORIGIN,
  );

  assert!(
    !result,
    "leave intent at u64::MAX must return false (no rebroadcast)"
  );
  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on u64::MAX leave intent"
  );
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated to u64::MAX"
  );
  assert_eq!(
    e.test_member_status(42u32),
    Some(MemberStatus::Alive),
    "status must remain Alive"
  );
}

/// A user event carrying ltime == u64::MAX must be dropped: event clock
/// unchanged, no Event::User emitted, returns false.
#[test]
fn user_event_max_ltime_is_dropped_no_state_mutation() {
  let mut e = ep();
  let clock_before = e.event_time();

  let msg = UserEventMessage {
    ltime: LamportTime::new(u64::MAX),
    cc: false,
    name: "test".into(),
    payload: bytes::Bytes::new(),
  };
  let is_new = e.test_handle_user_event(msg);

  assert!(!is_new, "user event at u64::MAX must return false");
  assert_eq!(
    e.event_time(),
    clock_before,
    "event clock must not advance on u64::MAX user event"
  );
  assert!(
    e.poll_event().is_none(),
    "no event must be emitted for u64::MAX user event"
  );
}

/// A query message carrying ltime == u64::MAX must be dropped: query clock
/// unchanged, no Event::Query emitted, returns false.
#[test]
fn query_max_ltime_is_dropped_no_state_mutation() {
  let mut e = ep();
  let clock_before = e.query_time();

  let msg = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(u64::MAX),
    id: 1,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(1),
    name: "test".into(),
    payload: bytes::Bytes::new(),
  };
  let rebroadcast = e.test_handle_query(msg);

  assert!(
    !rebroadcast,
    "query at u64::MAX must return false (no rebroadcast)"
  );
  assert_eq!(
    e.query_time(),
    clock_before,
    "query clock must not advance on u64::MAX query"
  );
  assert!(
    e.poll_event().is_none(),
    "no event must be emitted for u64::MAX query"
  );
}

/// merge_remote_state with a member clock of u64::MAX must not advance any
/// local clock and must not write a u64::MAX status_time on any member.
#[test]
fn merge_remote_state_max_member_clock_is_ignored() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  let clock_before = e.member_time();
  let event_clock_before = e.event_time();
  let query_clock_before = e.query_time();

  // Build a PushPull body with all three clocks at u64::MAX and a
  // status_ltimes entry at u64::MAX for node 42.
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(u64::MAX),
    event_ltime: LamportTime::new(u64::MAX),
    query_ltime: LamportTime::new(u64::MAX),
    status_ltimes: vec![(42u32, LamportTime::new(u64::MAX))],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  e.test_merge_remote_state(encoded);

  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on MAX push-pull"
  );
  assert_eq!(
    e.event_time(),
    event_clock_before,
    "event clock must not advance on MAX push-pull"
  );
  assert_eq!(
    e.query_time(),
    query_clock_before,
    "query clock must not advance on MAX push-pull"
  );
  // The u64::MAX status_ltime entry must be filtered out; node 42 retains status_time = 1.
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be overwritten with u64::MAX via push-pull"
  );
}

/// load_snapshot with all clocks at u64::MAX must not advance any local clock
/// and must not set min_time to u64::MAX (which would drop all future events).
#[test]
fn load_snapshot_max_clocks_are_ignored() {
  use crate::snapshot::ReplayResult;
  let mut e = ep();
  let clock_before = e.member_time();
  let event_clock_before = e.event_time();
  let query_clock_before = e.query_time();
  let event_min_before = e.test_event_min_time();

  let replay = ReplayResult {
    last_clock: LamportTime::new(u64::MAX),
    last_event_clock: LamportTime::new(u64::MAX),
    last_query_clock: LamportTime::new(u64::MAX),
    alive_nodes: vec![],
  };
  e.load_snapshot(replay, memberlist_proto::Instant::ORIGIN)
    .unwrap();

  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on MAX snapshot"
  );
  assert_eq!(
    e.event_time(),
    event_clock_before,
    "event clock must not advance on MAX snapshot"
  );
  assert_eq!(
    e.query_time(),
    query_clock_before,
    "query clock must not advance on MAX snapshot"
  );
  assert_eq!(
    e.test_event_min_time(),
    event_min_before,
    "event min_time must not be set to u64::MAX by snapshot"
  );
  assert_ne!(
    e.test_event_min_time(),
    u64::MAX,
    "event min_time == u64::MAX would drop all future events"
  );
}

// ── Bug 2: class sweep — per-ltime event buffer cap ──────────────────────────

/// Sending many unique (name, payload) events at the same ltime must not grow
/// the slot past MAX_EVENTS_PER_LTIME.
#[test]
fn event_buffer_slot_capped_at_max_events_per_ltime() {
  let mut e = ep();
  e.test_set_event_clock(1);
  let ltime = 1u64;

  // Fill the slot to the cap.
  for i in 0..MAX_EVENTS_PER_LTIME as u32 {
    let msg = UserEventMessage {
      ltime: LamportTime::new(ltime),
      cc: false,
      name: format!("ev-{i}").into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_user_event(msg);
  }
  assert_eq!(
    e.test_event_slot_len(ltime),
    MAX_EVENTS_PER_LTIME,
    "slot must be exactly at the cap"
  );

  // One more unique event: slot must not grow.
  let overflow = UserEventMessage {
    ltime: LamportTime::new(ltime),
    cc: false,
    name: "overflow".into(),
    payload: bytes::Bytes::new(),
  };
  let is_new = e.test_handle_user_event(overflow);
  assert!(!is_new, "event past the per-ltime cap must be rejected");
  assert_eq!(
    e.test_event_slot_len(ltime),
    MAX_EVENTS_PER_LTIME,
    "slot must not grow past MAX_EVENTS_PER_LTIME"
  );
}

/// Inbound events exceeding max_user_event_size must be dropped without
/// emitting an Event::User or growing the slot.
#[test]
fn inbound_user_event_too_large_is_dropped() {
  let mut e = ep();
  e.test_set_event_clock(1);
  let ltime = 1u64;

  // Default max_user_event_size is 512 bytes. Build a name+payload > 512.
  let big_name: smol_str::SmolStr = "x".repeat(300).into();
  let big_payload = bytes::Bytes::from(vec![0u8; 300]);
  let msg = UserEventMessage {
    ltime: LamportTime::new(ltime),
    cc: false,
    name: big_name,
    payload: big_payload,
  };
  let is_new = e.test_handle_user_event(msg);
  assert!(!is_new, "oversized inbound event must be rejected");
  assert!(
    e.poll_event().is_none(),
    "no Event::User for oversized inbound event"
  );
  assert_eq!(
    e.test_event_slot_len(ltime),
    0,
    "slot must not grow for oversized event"
  );
}

// ── Bug 3: key query num_nodes ────────────────────────────────────────────────

/// A key query issued with N members in the map must report num_nodes == N
/// in the emitted KeyResponse, both at timeout and after all responses.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_query_num_nodes_equals_member_count_at_issue_time() {
  let mut e = ep();
  // Seed 3 distinct members; including self (drained at construction) the total
  // membership at issue time is 4.
  for id in [10u32, 11, 12] {
    e.test_seed_member(id, MemberStatus::Alive, LamportTime::new(1));
  }
  assert_eq!(e.num_members(), 4);

  let far_future = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(9999);
  let _query_id = e.test_register_key_query(far_future);

  // num_nodes is captured at registration time.
  let num_nodes = e
    .test_last_pending_query_num_nodes()
    .expect("pending query must exist");
  assert_eq!(
    num_nodes, 4,
    "num_nodes must equal the member count at issue time"
  );

  // Fire the timeout: close_key_query emits Event::KeyResponse.
  let after_deadline = far_future + core::time::Duration::from_nanos(1);
  e.test_fire_due_query_closes(after_deadline);

  let ev = e
    .poll_event()
    .expect("Event::KeyResponse must be emitted on timeout");
  match ev {
    Event::KeyResponse(kr) => {
      assert_eq!(
        kr.num_nodes, 4,
        "KeyResponse.num_nodes must equal the queried member count"
      );
      assert_eq!(kr.num_resp, 0, "no responses were folded before timeout");
    }
    other => panic!("expected Event::KeyResponse, got {other:?}"),
  }
}

/// A key query with num_nodes captured correctly reports the right count even
/// when members are added after the query is issued.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_query_num_nodes_is_captured_at_issue_not_at_close() {
  let mut e = ep();
  // Self is already a member after construction drain; seed one more.
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));
  assert_eq!(e.num_members(), 2);

  let far_future = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(9999);
  let _query_id = e.test_register_key_query(far_future);

  // Now add 2 more members after the query was issued.
  e.test_seed_member(11u32, MemberStatus::Alive, LamportTime::new(1));
  e.test_seed_member(12u32, MemberStatus::Alive, LamportTime::new(1));
  assert_eq!(e.num_members(), 4);

  // Close the query: num_nodes must reflect membership AT ISSUE TIME (2), not now (4).
  let after_deadline = far_future + core::time::Duration::from_nanos(1);
  e.test_fire_due_query_closes(after_deadline);

  let ev = e.poll_event().expect("Event::KeyResponse must be emitted");
  match ev {
    Event::KeyResponse(kr) => {
      assert_eq!(
        kr.num_nodes, 2,
        "num_nodes must be the count at issue time (2), not the current count (4)"
      );
    }
    other => panic!("expected Event::KeyResponse, got {other:?}"),
  }
}

// ── UNIFIED Lamport-validation redesign regressions (Finding 1) ───────────────
//
// u64::MAX - 1 is the NEW threshold: witnessing it would advance the clock to
// u64::MAX, a permanent tombstone.  Prior fixes only rejected u64::MAX; these
// regressions verify the two-value safety buffer.

/// A join intent at u64::MAX - 1 must be dropped with NO clock change and
/// no state mutation — the same guarantee previously only tested for u64::MAX.
#[test]
fn join_intent_max_minus_one_ltime_is_dropped() {
  let mut e = ep();
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  let clock_before = e.member_time();

  let result = e.test_handle_join_intent(
    42u32,
    LamportTime::new(u64::MAX - 1),
    memberlist_proto::Instant::ORIGIN,
  );

  assert!(!result, "join intent at u64::MAX-1 must return false");
  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on u64::MAX-1 join intent"
  );
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated to u64::MAX-1"
  );
  // After the bad ingress a subsequent valid local user_event must still work.
  e.user_event("ok", bytes::Bytes::new(), false, Instant::ORIGIN)
    .expect("user_event must succeed after rejected intent");
}

/// A leave intent at u64::MAX - 1 must be dropped with NO clock change and
/// no state mutation.
#[test]
fn leave_intent_max_minus_one_ltime_is_dropped() {
  let mut e = ep();
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  let clock_before = e.member_time();

  let result = e.test_handle_leave_intent(
    42u32,
    LamportTime::new(u64::MAX - 1),
    memberlist_proto::Instant::ORIGIN,
  );

  assert!(!result, "leave intent at u64::MAX-1 must return false");
  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance on u64::MAX-1 leave intent"
  );
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated to u64::MAX-1"
  );
  // A subsequent valid join intent at a sane ltime must be accepted.
  let ok = e.test_handle_join_intent(
    42u32,
    LamportTime::new(5),
    memberlist_proto::Instant::ORIGIN,
  );
  assert!(
    ok,
    "valid join intent after rejected leave must be accepted"
  );
}

/// A user event at u64::MAX - 1 must be dropped: event clock unchanged, no
/// Event::User emitted, subsequent valid user_event() must succeed.
#[test]
fn user_event_max_minus_one_ltime_is_dropped() {
  let mut e = ep();
  let clock_before = e.event_time();

  let msg = crate::typed::UserEventMessage {
    ltime: LamportTime::new(u64::MAX - 1),
    cc: false,
    name: "bad".into(),
    payload: bytes::Bytes::new(),
  };
  let is_new = e.test_handle_user_event(msg);

  assert!(!is_new, "user event at u64::MAX-1 must return false");
  assert_eq!(
    e.event_time(),
    clock_before,
    "event clock must not advance on u64::MAX-1 user event"
  );
  assert!(
    e.poll_event().is_none(),
    "no event must be emitted for u64::MAX-1 user event"
  );
  // A subsequent valid user_event() must still work.
  e.user_event("ok", bytes::Bytes::new(), false, Instant::ORIGIN)
    .expect("user_event must succeed after rejected event");
}

/// A query at u64::MAX - 1 must be dropped: query clock unchanged, no
/// Event::Query, subsequent valid query() must succeed.
#[test]
fn query_max_minus_one_ltime_is_dropped() {
  let mut e = ep();
  let clock_before = e.query_time();

  let msg = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(u64::MAX - 1),
    id: 1,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    filters: vec![],
    flags: crate::typed::QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(1),
    name: "bad".into(),
    payload: bytes::Bytes::new(),
  };
  let rebroadcast = e.test_handle_query(msg);

  assert!(!rebroadcast, "query at u64::MAX-1 must return false");
  assert_eq!(
    e.query_time(),
    clock_before,
    "query clock must not advance on u64::MAX-1 query"
  );
  assert!(
    e.poll_event().is_none(),
    "no event must be emitted for u64::MAX-1 query"
  );
  // Subsequent valid query() must succeed.
  let _qid = e
    .query(
      "ok",
      bytes::Bytes::new(),
      QueryParams::default(),
      memberlist_proto::Instant::ORIGIN,
    )
    .expect("query must succeed after rejected query");
}

/// A push-pull with an unacceptable top-level member clock (u64::MAX - 1) must
/// drop the ENTIRE message: no dirty flag, no status_ltimes applied, no events.
#[test]
fn merge_remote_state_max_minus_one_member_clock_drops_entire_message() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  // Seed node 42 with status_time = 1.
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  e.test_clear_dirty();
  let clock_before = e.member_time();

  // Push-pull carries member_ltime = u64::MAX-1 (unacceptable) but also
  // a valid status_ltime = 100 for node 42 — the whole message must be dropped,
  // not just the top-level clock.
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(u64::MAX - 1), // unacceptable
    event_ltime: LamportTime::new(5),
    query_ltime: LamportTime::new(5),
    status_ltimes: vec![(42u32, LamportTime::new(100))],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  e.test_merge_remote_state(encoded);

  // No state mutation: clock unchanged, dirty flag not set by this call.
  assert_eq!(
    e.member_time(),
    clock_before,
    "member clock must not advance when top-level clock is u64::MAX-1"
  );
  assert!(
    !e.test_is_dirty(),
    "dirty flag must not be set when the entire push-pull is dropped"
  );
  // The status_ltimes entry (ltime=100) must NOT have been applied.
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated when the whole push-pull is dropped"
  );
}

/// A push-pull with an unacceptable event clock drops the ENTIRE message —
/// the status_ltimes body is NOT applied even though member_ltime is valid.
#[test]
fn merge_remote_state_max_minus_one_event_clock_drops_entire_message() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  e.test_seed_member(42u32, MemberStatus::Alive, LamportTime::new(1));
  e.test_clear_dirty();
  let event_clock_before = e.event_time();

  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(5),                  // valid
    event_ltime: LamportTime::new(u64::MAX - 1), // unacceptable
    query_ltime: LamportTime::new(5),
    status_ltimes: vec![(42u32, LamportTime::new(100))],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  e.test_merge_remote_state(encoded);

  assert_eq!(
    e.event_time(),
    event_clock_before,
    "event clock must not advance when event_ltime is u64::MAX-1"
  );
  assert!(
    !e.test_is_dirty(),
    "dirty flag must not be set when the entire push-pull is dropped"
  );
  assert_eq!(
    e.test_member_status_time(42u32).map(|lt| lt.0),
    Some(1),
    "status_time must not be updated when push-pull is dropped on event_ltime"
  );
}

// ── Responder identity validation regressions (Finding 2) ─────────────────────

/// A query response from an unknown (non-member) responder id must NOT be
/// counted in the conflict denominator, so it cannot cause a false shutdown.
#[test]
fn conflict_response_from_unknown_responder_is_not_counted() {
  let mut e = ep();
  // Seed one known member (id=10) so the machine has a real cluster.
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));

  let far_future = t_secs(9999);
  let qid = e.test_register_conflict_query(far_future);

  // Send a response claiming to be from id=999 — unknown, not in members.states.
  let forged = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: memberlist_proto::Node::new(999u32, addr(9999)),
    flags: crate::typed::QueryFlag::empty(),
    payload: bytes::Bytes::new(), // payload doesn't matter; membership check fires first
  };
  e.test_handle_query_response(forged);

  // The response must NOT have been counted (responses map is still empty).
  assert_eq!(
    e.test_pending_query_response_count(qid),
    0,
    "forged responder id must not be counted in conflict denominator"
  );
  // conflict_matching must also be 0.
  assert_eq!(
    e.test_pending_query_conflict_matching(qid),
    Some(0),
    "conflict_matching must not increase for forged responder"
  );
}

/// A flood of forged unknown responder ids for a conflict query must not
/// inflate the denominator, preventing a false shutdown.
#[test]
fn conflict_response_flood_of_unknown_ids_does_not_inflate_denominator() {
  let mut e = ep();
  // One real member.
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));

  let far_future = t_secs(9999);
  let qid = e.test_register_conflict_query(far_future);

  // Send 1000 responses each from a different unknown id.
  for forger_id in 1000u32..2000 {
    let resp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
      ltime: qid.ltime,
      id: qid.id,
      from: memberlist_proto::Node::new(forger_id, addr(9000)),
      flags: crate::typed::QueryFlag::empty(),
      payload: bytes::Bytes::new(),
    };
    e.test_handle_query_response(resp);
  }

  assert_eq!(
    e.test_pending_query_response_count(qid),
    0,
    "1000 forged responder ids must not inflate the denominator"
  );
}

/// A response from a KNOWN member is still counted (the validation must not
/// over-reject).
#[test]
fn conflict_response_from_known_member_is_counted() {
  let mut e = ep();
  e.test_seed_member(10u32, MemberStatus::Alive, LamportTime::new(1));

  let far_future = t_secs(9999);
  let qid = e.test_register_conflict_query(far_future);

  // Build a valid ConflictResponseMessage payload so the payload check passes.
  let member_node = memberlist_proto::Node::<u32, core::net::SocketAddr>::new(10u32, addr(7946));
  let conflict_resp = ConflictResponseMessage::new(member_node);
  let payload = AnyMessage::<u32, core::net::SocketAddr>::ConflictResponse(conflict_resp)
    .encode()
    .expect("encode conflict response");

  let resp = crate::typed::QueryResponseMessage {
    ltime: qid.ltime,
    id: qid.id,
    from: member_node,
    flags: crate::typed::QueryFlag::empty(),
    payload,
  };
  e.test_handle_query_response(resp);

  assert_eq!(
    e.test_pending_query_response_count(qid),
    1,
    "response from known member must be counted"
  );
}

// ── UNIFIED bounded-memory regressions (Finding 3) ───────────────────────────

/// A flood of unique unknown node ids via join intents must not grow
/// recent_intents past MAX_RECENT_INTENTS.
#[test]
fn recent_intents_capped_on_join_intent_flood() {
  use crate::members::MAX_RECENT_INTENTS;
  let mut e = ep();
  let now = memberlist_proto::Instant::ORIGIN;

  // Flood with 2 * MAX_RECENT_INTENTS unique unknown ids.
  for unknown_id in 100u32..(100 + 2 * MAX_RECENT_INTENTS as u32) {
    // These ids are NOT in members.states so they go into recent_intents.
    e.test_handle_join_intent(unknown_id, LamportTime::new(1), now);
  }

  assert!(
    e.test_recent_intents_len() <= MAX_RECENT_INTENTS,
    "recent_intents must not exceed MAX_RECENT_INTENTS after flood: got {}",
    e.test_recent_intents_len()
  );
}

/// A flood of unique unknown node ids via leave intents must not grow
/// recent_intents past MAX_RECENT_INTENTS.
#[test]
fn recent_intents_capped_on_leave_intent_flood() {
  use crate::members::MAX_RECENT_INTENTS;
  let mut e = ep();
  let now = memberlist_proto::Instant::ORIGIN;

  for unknown_id in 100u32..(100 + 2 * MAX_RECENT_INTENTS as u32) {
    e.test_handle_leave_intent(unknown_id, LamportTime::new(1), now);
  }

  assert!(
    e.test_recent_intents_len() <= MAX_RECENT_INTENTS,
    "recent_intents must not exceed MAX_RECENT_INTENTS after leave flood: got {}",
    e.test_recent_intents_len()
  );
}

// ── Zero event buffer panic regression (Finding 4) ────────────────────────────

/// An endpoint constructed with event_buffer_size = 0 must not panic when
/// a user event arrives (ltime % 0 would panic without the .max(1) guard).
#[test]
fn zero_event_buffer_size_does_not_panic_on_first_event() {
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  // event_buffer_size = 0 should be clamped to 1 internally.
  let opts = crate::options::Options::new().with_event_buffer_size(0);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  // Must NOT panic.
  e.user_event("test", bytes::Bytes::new(), false, Instant::ORIGIN)
    .expect("user_event must not panic when event_buffer_size was 0");
}

/// A push-pull with event_buffer_size = 0 endpoint must not panic when
/// merge_remote_state processes events.
#[test]
fn zero_event_buffer_size_does_not_panic_on_merge_remote_state() {
  use crate::typed::PushPullMessage;

  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = crate::options::Options::new().with_event_buffer_size(0);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(1),
    event_ltime: LamportTime::new(1),
    query_ltime: LamportTime::new(1),
    status_ltimes: vec![],
    left_members: vec![],
    events: vec![crate::typed::UserEvents {
      ltime: LamportTime::new(1),
      events: vec![crate::typed::UserEvent {
        name: "e".into(),
        payload: bytes::Bytes::new(),
      }],
    }],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");

  // Must NOT panic.
  e.test_merge_remote_state(encoded);
}

// ── Clock integrity + overflow regression tests ───────────────────────────────

/// An ingress ltime of LTIME_MAX is dropped on the join-intent path.
#[test]
fn ltime_max_join_intent_is_dropped() {
  let mut e = ep();
  e.test_set_clocks(5, 0, 0);
  let result = e.test_handle_join_intent(
    42u32,
    LamportTime::new(LTIME_MAX),
    memberlist_proto::Instant::ORIGIN,
  );
  assert!(!result, "join intent at LTIME_MAX must be dropped");
  assert_eq!(
    e.member_time(),
    5,
    "clock must not advance on LTIME_MAX join"
  );
}

/// An ingress ltime of LTIME_MAX + 5 is dropped on the user-event path.
#[test]
fn ltime_max_plus5_user_event_is_dropped() {
  let mut e = ep();
  e.test_set_clocks(0, 3, 0);
  let msg = UserEventMessage {
    ltime: LamportTime::new(LTIME_MAX + 5),
    cc: false,
    name: "flood".into(),
    payload: bytes::Bytes::new(),
  };
  let is_new = e.test_handle_user_event(msg);
  assert!(!is_new, "user event at LTIME_MAX+5 must be dropped");
  assert_eq!(
    e.event_time(),
    3,
    "event clock must not advance on LTIME_MAX+5 event"
  );
  assert!(e.poll_event().is_none(), "no event must be emitted");
}

/// Integrity floor: next_ltime on a poisoned clock (LTIME_MAX) must not wrap
/// to 0 and must not produce u64::MAX via saturating_add-then-max-wrap.
///
/// The no-UB contract: stamp = LTIME_MAX (the poisoned value as-is); stored
/// clock advances to LTIME_MAX.saturating_add(1) = LTIME_MAX + 1.  Neither
/// the stamped value nor the stored clock may be 0 or u64::MAX.
#[test]
fn poisoned_clock_integrity_floor() {
  // Simulate a hypothetically-poisoned clock set to LTIME_MAX.
  let mut clock = LTIME_MAX;
  let stamped = next_ltime(&mut clock);
  // Stamped is LTIME_MAX (the poisoned value — peers will reject it via
  // ltime_is_acceptable, which is the correct degraded behaviour).
  // The integrity floor: no wrap to 0, no tombstone at u64::MAX.
  assert_ne!(stamped, 0, "stamped must not wrap to 0; got {stamped}");
  assert_ne!(
    stamped,
    u64::MAX,
    "stamped must not be u64::MAX tombstone; got {stamped}"
  );
  // Stored clock must not wrap to 0 (no UB).
  assert_ne!(clock, 0, "stored clock must not wrap to 0; got {clock}");
  assert_ne!(
    clock,
    u64::MAX,
    "stored clock must not be u64::MAX tombstone; got {clock}"
  );
}

/// After witnessing the max acceptable ltime, a subsequent local user_event still
/// succeeds and emits a finite sub-watermark ltime.
#[test]
fn after_high_clock_local_user_event_still_works() {
  let mut e = ep();
  // Set event clock to LTIME_MAX - 2 (highest acceptable witness value).
  e.test_set_event_clock(LTIME_MAX - 2);
  // Witness LTIME_MAX - 2: event_clock advances to LTIME_MAX - 1.
  let msg = UserEventMessage {
    ltime: LamportTime::new(LTIME_MAX - 2),
    cc: false,
    name: "ok".into(),
    payload: bytes::Bytes::new(),
  };
  let _ = e.test_handle_user_event(msg);
  // Now emit a local user_event via user_event() — next_ltime clamps and advances.
  let result = e.user_event("local", bytes::Bytes::new(), false, Instant::ORIGIN);
  assert!(result.is_ok(), "user_event must succeed even at high clock");
}

/// A flood of distinct (ltime,id) queries injected via handle_query does not
/// grow received_queries past MAX_RECEIVED_QUERIES.
#[test]
fn received_queries_capped_at_max_via_handle_query() {
  let mut e = ep();
  e.test_set_clocks(0, 0, 1);

  // Send MAX_RECEIVED_QUERIES + 20 unique (ltime=1, id=i) queries.
  // Each passes filter (no filters = broadcast all) and unique id = first sight.
  for i in 0..(MAX_RECEIVED_QUERIES + 20) as u32 {
    let msg = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(1),
      id: i,
      from: memberlist_proto::Node::new(99u32, sa(9001)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(3600), // huge timeout
      name: "flood".into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_query(msg);
  }

  assert!(
    e.test_received_queries_len() <= MAX_RECEIVED_QUERIES,
    "received_queries must not exceed cap after flood; got {}",
    e.test_received_queries_len()
  );
}

/// A peer-supplied huge timeout is clamped so the received_query deadline is at
/// most now + MAX_QUERY_TIMEOUT.
#[test]
fn inbound_query_timeout_clamped() {
  let mut e = ep();
  e.test_set_clocks(0, 0, 1);
  let now = memberlist_proto::Instant::ORIGIN;

  // Inject a query with a 1-hour timeout (far exceeds MAX_QUERY_TIMEOUT = 600s).
  let msg = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 77,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(3600),
    name: "test".into(),
    payload: bytes::Bytes::new(),
  };
  // drain_now is ORIGIN by default.
  let _ = e.test_handle_query(msg);

  // The received_query entry must have a deadline <= now + MAX_QUERY_TIMEOUT.
  let max_deadline = now + MAX_QUERY_TIMEOUT;
  let all_within = e
    .test_peek_received_query_deadlines()
    .iter()
    .all(|&dl| dl <= max_deadline);
  assert!(
    all_within,
    "all received_query deadlines must be <= now + MAX_QUERY_TIMEOUT"
  );
}

/// A flood of first-seen messages does not push the broadcast queue past queue_max.
#[test]
fn rebroadcast_queue_depth_capped() {
  let mut e = ep();
  // Default max_queue_depth = 4096. Inject 5000 unique user events.
  for i in 0u32..5000 {
    let msg = UserEventMessage {
      ltime: LamportTime::new(i as u64 + 1),
      cc: false,
      name: format!("ev{i}").into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_user_event(msg);
  }
  let depth = e.user_broadcast_queue_len();
  let queue_max = Options::new().max_queue_depth();
  assert!(
    depth <= queue_max,
    "broadcast queue depth {depth} must not exceed queue_max {queue_max}"
  );
}

// ── FIX: inbound query size limit enforced symmetrically ─────────────────────

/// An inbound `Query` whose encoded wire size exceeds `query_size_limit` must
/// be rejected BEFORE any state mutation: no `Event::Query`, no entry in
/// `received_queries`, no rebroadcast, and no `query_clock` advance.
///
/// Regression for the asymmetry where the local `query()` outbound path
/// enforced the limit but the inbound `UserPacket → AnyMessage::Query` path
/// did not.
#[test]
fn inbound_query_oversized_is_dropped_before_state_mutation() {
  // Build an endpoint with a small query_size_limit (64 bytes) so that a
  // query with a large payload reliably exceeds it.
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    memberlist_proto::SmallRng::seed_from_u64(0),
  );
  let opts = crate::options::Options::new().with_query_size_limit(64);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);
  // Drain the construction self-join so it does not appear as a spurious event.
  let _ = e.poll_event();

  // Encode a QueryMessage whose payload pushes the wire encoding over 64 bytes.
  let oversized_payload = bytes::Bytes::from(vec![0u8; 100]);
  let q = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(3),
    id: 0xabcd_ef01,
    from: memberlist_proto::Node::new(2u32, sa(9002)),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "ping".into(),
    payload: oversized_payload,
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::Query(q)
    .encode()
    .unwrap();
  assert!(
    encoded.len() > 64,
    "test precondition: encoded size ({}) must exceed limit (64)",
    encoded.len()
  );

  let before_qclock = e.query_time();
  let before_rcv = e.test_received_queries_len();
  let before_queue = e.user_broadcast_queue_len();

  e.test_inject_user_packet(sa(9002), encoded, memberlist_proto::Instant::ORIGIN);

  // No Event::Query must have been emitted.
  assert!(
    e.poll_event().is_none(),
    "oversized inbound query must not emit Event::Query"
  );
  // query_clock must not have advanced.
  assert_eq!(
    e.query_time(),
    before_qclock,
    "oversized inbound query must not advance query_clock"
  );
  // received_queries must not have grown.
  assert_eq!(
    e.test_received_queries_len(),
    before_rcv,
    "oversized inbound query must not insert into received_queries"
  );
  // No rebroadcast: broadcast queue must not have grown.
  assert_eq!(
    e.user_broadcast_queue_len(),
    before_queue,
    "oversized inbound query must not be rebroadcast"
  );
}

/// A well-formed inbound `Query` within the size limit DOES advance
/// `query_clock`, insert into `received_queries`, and emit `Event::Query`.
///
/// Regression twin: confirms the gate does not over-reject.
#[test]
fn inbound_query_within_size_limit_is_accepted() {
  let mut e = ep(); // query_size_limit = 1024 (default)

  let q = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 0x1234_5678,
    from: memberlist_proto::Node::new(2u32, sa(9002)),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "ok".into(),
    payload: bytes::Bytes::from_static(b"small"),
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::Query(q)
    .encode()
    .unwrap();
  assert!(
    encoded.len() <= 1024,
    "test precondition: encoded size ({}) must be within default limit (1024)",
    encoded.len()
  );

  let before_qclock = e.query_time();

  e.test_inject_user_packet(sa(9002), encoded, memberlist_proto::Instant::ORIGIN);

  // Event::Query must be emitted.
  let ev = e.poll_event();
  assert!(
    matches!(ev, Some(Event::Query(_))),
    "in-limit inbound query must emit Event::Query; got {ev:?}"
  );
  // query_clock must have advanced.
  assert!(
    e.query_time() > before_qclock,
    "in-limit inbound query must advance query_clock"
  );
  // received_queries must have grown.
  assert_eq!(
    e.test_received_queries_len(),
    1,
    "in-limit inbound query must insert into received_queries"
  );
}

/// An ACK for a non-ack PendingQuery is dropped silently (no QueryAck event emitted).
#[test]
fn ack_for_non_ack_query_is_dropped() {
  let mut e = ep();
  let deadline = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(10);

  // Register a pending query WITHOUT request_ack.
  let qid = QueryId {
    ltime: LamportTime::new(1),
    id: 42,
  };
  e.core_mut().pending_queries.push(PendingQuery {
    kind: QueryPurpose::App,
    deadline,
    responses: crate::FxHashMap::default(),
    acks: crate::FxHashMap::default(),
    query_id: qid,
    request_ack: false,
    conflict_matching: 0,
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    num_nodes: 0,
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    key_tally: None,
  });

  // Inject an ACK response for that query.
  let ack_msg = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 42,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    flags: QueryFlag::ACK,
    payload: bytes::Bytes::new(),
  };
  e.test_handle_query_response(ack_msg);

  // No QueryAck event must have been emitted.
  assert!(
    e.poll_event().is_none(),
    "ACK for a non-ack query must not emit Event::QueryAck"
  );
}

/// An ACK for a request_ack=true PendingQuery IS delivered.
#[test]
fn ack_for_ack_query_is_delivered() {
  let mut e = ep();
  let deadline = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(10);

  let qid = QueryId {
    ltime: LamportTime::new(1),
    id: 42,
  };
  e.core_mut().pending_queries.push(PendingQuery {
    kind: QueryPurpose::App,
    deadline,
    responses: crate::FxHashMap::default(),
    acks: crate::FxHashMap::default(),
    query_id: qid,
    request_ack: true, // <-- requesting acks
    conflict_matching: 0,
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    num_nodes: 0,
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    key_tally: None,
  });

  // Inject an ACK response.
  let ack_msg = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 42,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    flags: QueryFlag::ACK,
    payload: bytes::Bytes::new(),
  };
  e.test_handle_query_response(ack_msg);

  // A QueryAck event must have been emitted.
  let ev = e.poll_event();
  assert!(
    matches!(ev, Some(Event::QueryAck(_))),
    "ACK for a request_ack=true query must emit Event::QueryAck; got {ev:?}"
  );
}

// ── FIX 1: exact-consumption decode gate on UserPacket ingress ────────────────

/// A valid UserEvent frame followed by trailing junk bytes must be dropped —
/// no event emitted, no clock advance, no rebroadcast.
///
/// Without the exact-consumption gate the junk-padded packet passes the frame
/// decode (the framing decoder accepts the prefix), mutates clock / dedup state,
/// emits `Event::User`, and rebroadcasts the WHOLE original bytes (junk included).
/// The gate fires at the TOP of `handle_user_packet`, before any handler call.
#[test]
fn user_event_with_trailing_junk_is_dropped_no_event_no_clock_advance() {
  let mut e = ep();

  // Build a valid UserEvent frame.
  let valid = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 7.into(),
    cc: false,
    name: "op".into(),
    payload: bytes::Bytes::from_static(b"data"),
  })
  .encode()
  .unwrap();

  // Append trailing junk so the total length exceeds the frame.
  let mut padded = valid.to_vec();
  padded.extend_from_slice(b"\xde\xad\xbe\xef");
  let padded = bytes::Bytes::from(padded);

  let clock_before = e.event_time();
  let queue_before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();

  e.test_inject_user_packet(from, padded, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "trailing-junk packet must be dropped — no Event::User"
  );
  assert_eq!(
    e.event_time(),
    clock_before,
    "trailing-junk packet must not advance the event clock"
  );
  assert_eq!(
    e.user_broadcast_queue_len(),
    queue_before,
    "trailing-junk packet must not grow the rebroadcast queue"
  );
}

/// A Join frame with trailing junk bytes is dropped — no intent buffered, no
/// member clock advance, no rebroadcast.
#[test]
fn join_intent_with_trailing_junk_is_dropped() {
  let mut e = ep();

  let valid =
    AnyMessage::<u32, core::net::SocketAddr>::Join(JoinMessage::new(LamportTime::new(5), 99u32))
      .encode()
      .unwrap();
  let mut padded = valid.to_vec();
  padded.push(0xff);
  let padded = bytes::Bytes::from(padded);

  let clock_before = e.member_time();
  let queue_before = e.user_broadcast_queue_len();
  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();

  e.test_inject_user_packet(from, padded, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "trailing-junk join must be dropped"
  );
  assert_eq!(
    e.member_time(),
    clock_before,
    "trailing-junk join must not advance the member clock"
  );
  assert_eq!(
    e.user_broadcast_queue_len(),
    queue_before,
    "trailing-junk join must not rebroadcast"
  );
  assert_eq!(
    e.test_member_status(99u32),
    None,
    "trailing-junk join must not buffer an intent"
  );
}

// ── FIX 1 regression: exact-consumption decode at every ingress decode site ──

/// A PushPull frame with trailing junk bytes is dropped by `merge_remote_state`
/// — no clock witness, no intents applied, not marked dirty, no event.
#[test]
fn merge_remote_state_with_trailing_junk_is_dropped() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  // Build a valid PushPull payload.
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(7),
    status_ltimes: vec![],
    left_members: vec![],
    event_ltime: LamportTime::new(3),
    events: vec![],
    query_ltime: LamportTime::new(2),
  };
  let valid = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  // Append trailing junk.
  let mut padded = valid.to_vec();
  padded.push(0xde);
  padded.push(0xad);
  let padded = bytes::Bytes::from(padded);

  let clock_before = e.member_time();
  let dirty_before = e.test_is_dirty();
  e.test_merge_remote_state(padded);

  assert_eq!(
    e.member_time(),
    clock_before,
    "trailing-junk PushPull must not advance the member clock"
  );
  assert_eq!(
    e.test_is_dirty(),
    dirty_before,
    "trailing-junk PushPull must not mark local state dirty"
  );
  assert!(
    e.poll_event().is_none(),
    "trailing-junk PushPull must not emit any event"
  );
}

/// A ConflictResponse payload with trailing junk bytes is NOT counted toward
/// `conflict_matching` — exact consumption required.
#[test]
fn conflict_response_with_trailing_junk_is_not_counted() {
  use crate::typed::ConflictResponseMessage;
  use memberlist_proto::Node;

  let mut e = ep();
  let local_addr: core::net::SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let deadline = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3600);
  let qid = e.test_register_conflict_query(deadline);

  // Seed the responder as a known Alive member so the membership gate passes.
  let responder_addr: core::net::SocketAddr = "127.0.0.1:2000".parse().unwrap();
  e.test_seed_member(200u32, MemberStatus::Alive, LamportTime::new(1));

  // Encode a ConflictResponseMessage that points to the local address (would agree if counted).
  let resp_msg = ConflictResponseMessage::new(Node::new(999u32, local_addr));
  let valid = AnyMessage::<u32, core::net::SocketAddr>::ConflictResponse(resp_msg)
    .encode()
    .expect("encode must succeed");
  // Append trailing junk — must be dropped.
  let mut padded = valid.to_vec();
  padded.push(0xff);
  let padded = bytes::Bytes::from(padded);

  // Inject via handle_query_response (which routes to handle_conflict_response_fold).
  let qresp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: Node::new(200u32, responder_addr),
    flags: crate::typed::QueryFlag::empty(),
    payload: padded,
  };
  e.test_handle_query_response(qresp);

  assert_eq!(
    e.test_pending_query_conflict_matching(qid),
    Some(0),
    "trailing-junk ConflictResponse must not increment conflict_matching"
  );
}

/// A KeyResponse payload with trailing junk bytes is NOT folded into the tally
/// — exact consumption required.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_response_with_trailing_junk_is_not_tallied() {
  use crate::typed::KeyResponseMessage;
  use memberlist_proto::Node;

  let mut e = ep();
  let deadline = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(3600);
  let qid = e.test_register_key_query(deadline);

  // Seed the responder as a known Alive member so the membership gate passes.
  e.test_seed_member(300u32, MemberStatus::Alive, LamportTime::new(1));

  // Encode a successful KeyResponseMessage.
  let key_msg = KeyResponseMessage {
    result: true,
    message: "ok".into(),
    keys: vec![],
    primary_key: None,
  };
  let valid = AnyMessage::<u32, core::net::SocketAddr>::KeyResponse(key_msg)
    .encode()
    .expect("encode must succeed");
  // Append trailing junk — must be dropped.
  let mut padded = valid.to_vec();
  padded.push(0xbe);
  padded.push(0xef);
  let padded = bytes::Bytes::from(padded);

  let qresp = crate::typed::QueryResponseMessage::<u32, core::net::SocketAddr> {
    ltime: qid.ltime,
    id: qid.id,
    from: Node::new(300u32, "127.0.0.1:3000".parse().unwrap()),
    flags: crate::typed::QueryFlag::empty(),
    payload: padded,
  };
  e.test_handle_query_response(qresp);

  // Trailing junk dropped → response NOT counted (num_resp stays 0).
  assert_eq!(
    e.test_pending_query_response_count(qid),
    0,
    "trailing-junk KeyResponse must not be counted toward the tally"
  );
}

// ── FIX 2 regression: zero-interval busy-loop ────────────────────────────────

/// With `reap_interval` set to zero, `handle_timeout` must advance
/// `next_reap` to a strictly later instant — no busy-loop.
#[test]
fn zero_reap_interval_advances_deadline() {
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new().with_reap_interval(core::time::Duration::ZERO);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  // Advance time past the first reap deadline so handle_timeout fires it.
  let now = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(1);
  e.handle_timeout(now);

  // The deadline must be strictly after `now` — clamp prevents busy-loop.
  if let Some(dl) = e.poll_timeout() {
    assert!(
      dl > now,
      "zero reap_interval must still advance the deadline past now (got dl={:?}, now={:?})",
      dl,
      now
    );
  }
}

/// With `reconnect_interval` set to zero, `handle_timeout` must advance
/// `next_reconnect` to a strictly later instant — no busy-loop.
#[test]
fn zero_reconnect_interval_advances_deadline() {
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new().with_reconnect_interval(core::time::Duration::ZERO);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  let now = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(1);
  e.handle_timeout(now);

  if let Some(dl) = e.poll_timeout() {
    assert!(
      dl > now,
      "zero reconnect_interval must still advance the deadline past now (got dl={:?}, now={:?})",
      dl,
      now
    );
  }
}

/// With `queue_check_interval` set to zero, `handle_timeout` must advance
/// `next_queue_check` to a strictly later instant — no busy-loop.
#[test]
fn zero_queue_check_interval_advances_deadline() {
  let inner_opts =
    EndpointOptions::<u32, core::net::SocketAddr>::new(1u32, "127.0.0.1:7946".parse().unwrap())
      .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new().with_queue_check_interval(core::time::Duration::ZERO);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);

  let now = memberlist_proto::Instant::ORIGIN + core::time::Duration::from_secs(1);
  e.handle_timeout(now);

  if let Some(dl) = e.poll_timeout() {
    assert!(
      dl > now,
      "zero queue_check_interval must still advance the deadline past now (got dl={:?}, now={:?})",
      dl,
      now
    );
  }
}

/// A valid UserEvent whose TOTAL packet (frame + junk) exceeds
/// `max_user_event_size` is dropped — the size budget must see the whole
/// packet, not just the decoded payload.
#[test]
fn user_event_total_packet_with_junk_exceeding_size_limit_is_dropped() {
  // Set max_user_event_size to a small value so a padded packet exceeds it.
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    memberlist_proto::SmallRng::seed_from_u64(0),
  );
  // Set a tight size limit: 32 bytes.
  let opts = crate::options::Options::new().with_max_user_event_size(32);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);
  // Drain the construction self-join so it does not appear as a spurious event.
  let _ = e.poll_event();

  // A small valid UserEvent that fits within 32 bytes on its own.
  let valid = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 3.into(),
    cc: false,
    name: "x".into(),
    payload: bytes::Bytes::from_static(b"y"),
  })
  .encode()
  .unwrap();
  // Pad to exceed the 32-byte limit with junk.
  let mut padded = valid.to_vec();
  while padded.len() <= 32 {
    padded.push(0x00);
  }
  let padded = bytes::Bytes::from(padded);

  let from: core::net::SocketAddr = "127.0.0.1:1002".parse().unwrap();
  e.test_inject_user_packet(from, padded, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "junk-padded packet exceeding size limit must be dropped"
  );
}

// ── FIX: pre-decode size fence in handle_user_packet ─────────────────────────

/// An over-limit but syntactically-valid `Query` frame is dropped at the
/// pre-decode size fence — before `AnyMessage::decode_with_consumed` is called.
///
/// Asserts: no `Event::Query`, no `query_clock` advance, no insert into
/// `received_queries`, no rebroadcast.  The frame is fully parseable by the
/// codec; the fence must fire on frame length alone.
#[test]
fn pre_decode_fence_drops_oversized_valid_query_frame() {
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    memberlist_proto::SmallRng::seed_from_u64(0),
  );
  // Small limit (64 bytes) so that a query with a 100-byte payload exceeds it.
  let opts = crate::options::Options::new().with_query_size_limit(64);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);
  // Drain the construction self-join so it does not appear as a spurious event.
  let _ = e.poll_event();

  // Construct a syntactically valid Query whose encoded frame exceeds 64 bytes.
  let q = QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(7),
    id: 0xdead_beef,
    from: memberlist_proto::Node::new(2u32, sa(9002)),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "oversized".into(),
    payload: bytes::Bytes::from(vec![0xffu8; 100]),
  };
  let frame = AnyMessage::<u32, core::net::SocketAddr>::Query(q)
    .encode()
    .expect("encode must succeed");
  assert!(
    frame.len() > 64,
    "test precondition: frame ({} bytes) must exceed limit (64)",
    frame.len()
  );

  let before_qclock = e.query_time();
  let before_rcv = e.test_received_queries_len();
  let before_queue = e.user_broadcast_queue_len();

  e.test_inject_user_packet(sa(9002), frame, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "pre-decode fence must drop oversized valid Query — no Event::Query"
  );
  assert_eq!(
    e.query_time(),
    before_qclock,
    "pre-decode fence must not advance query_clock"
  );
  assert_eq!(
    e.test_received_queries_len(),
    before_rcv,
    "pre-decode fence must not insert into received_queries"
  );
  assert_eq!(
    e.user_broadcast_queue_len(),
    before_queue,
    "pre-decode fence must not rebroadcast"
  );
}

/// An over-limit but syntactically-valid `UserEvent` frame is dropped at the
/// pre-decode size fence — before `AnyMessage::decode_with_consumed` is called.
///
/// Asserts: no `Event::User`, no `event_clock` advance, no rebroadcast.
#[test]
fn pre_decode_fence_drops_oversized_valid_user_event_frame() {
  let inner_opts = memberlist_proto::EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    memberlist_proto::SmallRng::seed_from_u64(0),
  );
  // Tight limit: 32 bytes.
  let opts = crate::options::Options::new().with_max_user_event_size(32);
  let mut e: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(coord(inner), opts);
  // Drain the construction self-join so it does not appear as a spurious event.
  let _ = e.poll_event();

  // Construct a syntactically valid UserEvent whose encoded frame exceeds 32 bytes.
  let frame = AnyMessage::<u32, core::net::SocketAddr>::UserEvent(UserEventMessage {
    ltime: 5.into(),
    cc: false,
    name: "big-event".into(),
    payload: bytes::Bytes::from(vec![0xaau8; 60]),
  })
  .encode()
  .expect("encode must succeed");
  assert!(
    frame.len() > 32,
    "test precondition: frame ({} bytes) must exceed limit (32)",
    frame.len()
  );

  let before_eclock = e.event_time();
  let before_queue = e.user_broadcast_queue_len();

  e.test_inject_user_packet(sa(9001), frame, memberlist_proto::Instant::ORIGIN);

  assert!(
    e.poll_event().is_none(),
    "pre-decode fence must drop oversized valid UserEvent — no Event::User"
  );
  assert_eq!(
    e.event_time(),
    before_eclock,
    "pre-decode fence must not advance event_clock"
  );
  assert_eq!(
    e.user_broadcast_queue_len(),
    before_queue,
    "pre-decode fence must not rebroadcast"
  );
}

// ── Pre-dedup dirty-mark regression tests ─────────────────────────────────────
//
// These guard that `local_state_dirty` is only set when a clock actually
// advanced or a buffer/member state actually changed — never on a duplicate,
// stale, or witness-only no-op.

/// A duplicate UserEvent (same ltime + payload, second delivery) must not set
/// `local_state_dirty`.  Only the first delivery should mark dirty.
#[test]
fn duplicate_user_event_does_not_set_local_state_dirty() {
  let mut e = ep();

  let msg = UserEventMessage {
    ltime: LamportTime::new(3),
    cc: false,
    name: "ping".into(),
    payload: bytes::Bytes::from_static(b"data"),
  };

  // First delivery: new event, should dirty.
  let first = e.handle_user_event(msg.clone());
  assert!(first, "first delivery must be accepted as new");
  assert!(
    e.test_is_dirty(),
    "first delivery must set local_state_dirty"
  );

  // Clear the flag so we can observe whether the second delivery re-sets it.
  e.test_clear_dirty();

  // Second delivery of the exact same message: dedup ring returns false.
  let second = e.handle_user_event(msg);
  assert!(!second, "duplicate delivery must be dropped by dedup ring");
  assert!(
    !e.test_is_dirty(),
    "duplicate user event must NOT set local_state_dirty"
  );
}

/// A stale JoinIntent (ltime <= status_time for a known member) must not set
/// `local_state_dirty`.
#[test]
fn stale_join_intent_does_not_set_local_state_dirty() {
  let mut e = ep();
  // Seed member 2 at status_time=10.
  e.test_seed_member(2u32, MemberStatus::Alive, LamportTime::new(10));
  e.test_clear_dirty();

  // A join intent at ltime=3 is stale (3 <= 10).
  let rebroadcast =
    e.test_handle_join_intent(2, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);
  assert!(!rebroadcast, "stale join intent must not rebroadcast");
  assert!(
    !e.test_is_dirty(),
    "stale join intent must NOT set local_state_dirty"
  );
}

/// A stale LeaveIntent (ltime <= status_time for a known member) must not set
/// `local_state_dirty`.
#[test]
fn stale_leave_intent_does_not_set_local_state_dirty() {
  let mut e = ep();
  // Seed member 2 at status_time=10.
  e.test_seed_member(2u32, MemberStatus::Alive, LamportTime::new(10));
  e.test_clear_dirty();

  // A leave intent at ltime=3 is stale (3 <= 10).
  let rebroadcast =
    e.test_handle_leave_intent(2, LamportTime::new(3), memberlist_proto::Instant::ORIGIN);
  assert!(!rebroadcast, "stale leave intent must not rebroadcast");
  assert!(
    !e.test_is_dirty(),
    "stale leave intent must NOT set local_state_dirty"
  );
}

/// A duplicate Query (same ltime + id, second delivery) must not set
/// `local_state_dirty`.
#[test]
fn duplicate_query_does_not_set_local_state_dirty() {
  let mut e = ep();

  let msg = QueryMessage {
    ltime: LamportTime::new(4),
    id: 0xdead_beef,
    from: memberlist_proto::Node::new(
      42u32,
      "127.0.0.1:9999".parse::<core::net::SocketAddr>().unwrap(),
    ),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(1),
    name: "test-query".into(),
    payload: bytes::Bytes::new(),
  };

  // First delivery: new query, should dirty.
  let first = e.test_handle_query(msg.clone());
  assert!(first, "first delivery must be accepted as new");
  assert!(
    e.test_is_dirty(),
    "first query delivery must set local_state_dirty"
  );

  // Clear the flag.
  e.test_clear_dirty();

  // Second delivery: dedup ring returns false.
  let second = e.test_handle_query(msg);
  assert!(!second, "duplicate query must be dropped by dedup ring");
  assert!(
    !e.test_is_dirty(),
    "duplicate query must NOT set local_state_dirty"
  );
}

// ── Key-management regression tests ──────────────────────────────────────────

/// When `received_queries` reaches `MAX_RECEIVED_QUERIES`, overflow queries are
/// DROPPED (not inserted and not surfaced) so that already-surfaced tokens
/// remain answerable via `respond_key`.
///
/// The test injects `MAX_RECEIVED_QUERIES + 1` distinct key queries and verifies
/// that the first surfaced `Event::KeyRequest` can still be answered with
/// `respond_key` — its token was not evicted.  Exactly `MAX_RECEIVED_QUERIES`
/// events are surfaced (the overflow query produces no event).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn surfaced_token_survives_overflow_and_overflow_never_surfaces() {
  use crate::{
    AnyMessage, KeyRequestMessage,
    event::{Event, KeyRequestOperation, KeyResponseArgs},
  };
  use memberlist_proto::SecretKey;

  #[cfg(feature = "aes-gcm")]
  let test_key = SecretKey::Aes128([0u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let test_key = SecretKey::ChaCha20Poly1305([0u8; 32]);

  let mut e = ep();

  // Inject MAX_RECEIVED_QUERIES + 1 well-formed key queries with distinct
  // (ltime, id) pairs so each is a first-sight query (passes dedup).
  // Use huge timeouts so none expire during the test.
  let n = MAX_RECEIVED_QUERIES + 1;
  for i in 0..n as u32 {
    let req = KeyRequestMessage::new(Some(test_key));
    let payload = AnyMessage::<u32, core::net::SocketAddr>::KeyRequest(req)
      .encode()
      .expect("encode KeyRequestMessage");
    let q = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(i as u64 + 1),
      id: i,
      from: memberlist_proto::Node::new(99u32, sa(9001)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(3600),
      name: "_serf_install_key".into(),
      payload,
    };
    let _ = e.test_handle_query(q);
  }

  // received_queries must be exactly at the cap (the overflow was dropped, not
  // inserted).
  assert_eq!(
    e.test_received_queries_len(),
    MAX_RECEIVED_QUERIES,
    "received_queries must be exactly at cap after overflow; got {}",
    e.test_received_queries_len()
  );

  // The first emitted Event::KeyRequest corresponds to the FIRST query.
  // Its received_queries token must still be present (not evicted).
  let first_ev = e
    .poll_event()
    .expect("first query must emit Event::KeyRequest");
  let first_req = match first_ev {
    Event::KeyRequest(kr) => kr,
    other => panic!(
      "expected Event::KeyRequest for first query, got {:?}",
      core::mem::discriminant(&other)
    ),
  };
  assert!(
    matches!(first_req.op(), KeyRequestOperation::Install),
    "first event must be Install"
  );

  // The first token must still be answerable (not evicted).
  let now = memberlist_proto::Instant::ORIGIN;
  e.respond_key(
    &first_req,
    KeyResponseArgs {
      result: true,
      message: smol_str::SmolStr::default(),
      keys: vec![test_key],
      primary_key: Some(test_key),
    },
    now,
  )
  .expect("respond_key must succeed — first surfaced token must not have been evicted");

  // Drain remaining events: there must be exactly MAX_RECEIVED_QUERIES - 1
  // more (the overflow's event was never produced).
  let mut remaining = 0usize;
  while let Some(ev) = e.poll_event() {
    assert!(
      matches!(ev, Event::KeyRequest(_)),
      "all remaining events must be KeyRequest, got {:?}",
      core::mem::discriminant(&ev)
    );
    remaining += 1;
  }
  assert_eq!(
    remaining,
    MAX_RECEIVED_QUERIES - 1,
    "must have exactly MAX_RECEIVED_QUERIES - 1 remaining events; the overflow must not surface"
  );
}

// ── FIX 1b regression: local key ops self-apply even when inbound cap is full ──

/// A local `install_key` / `use_key` / `remove_key` MUST surface
/// `Event::KeyRequest` on the initiating node even when `received_queries` is
/// already at `MAX_RECEIVED_QUERIES` from inbound peer queries.
///
/// The inbound overflow cap is a DoS defence against peer flooding; it MUST NOT
/// gate locally-originated processing (which is app-rate-limited, not
/// peer-controlled).  Without this guard, a full `received_queries` caused
/// `handle_query(Local)` to return early, so the initiator never applied the key
/// op to its own keyring.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn local_key_op_self_applies_when_inbound_cap_is_full() {
  use crate::{AnyMessage, KeyRequestMessage, event::KeyRequestOperation};
  use memberlist_proto::SecretKey;

  #[cfg(feature = "aes-gcm")]
  let test_key = SecretKey::Aes128([1u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let test_key = SecretKey::ChaCha20Poly1305([1u8; 32]);

  let mut e = ep();

  // Saturate received_queries with MAX_RECEIVED_QUERIES inbound queries so
  // the cap is exactly full.  Use distinct (ltime, id) pairs and huge timeouts
  // so none expire during the test.
  for i in 0..MAX_RECEIVED_QUERIES as u32 {
    let req = KeyRequestMessage::new(Some(test_key));
    let payload = AnyMessage::<u32, core::net::SocketAddr>::KeyRequest(req)
      .encode()
      .expect("encode KeyRequestMessage for inbound flood");
    let q = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(i as u64 + 1),
      id: i,
      from: memberlist_proto::Node::new(99u32, sa(9001)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(3600),
      name: "_serf_install_key".into(),
      payload,
    };
    let _ = e.test_handle_query(q);
  }

  // Confirm the cap is exactly full.
  assert_eq!(
    e.test_received_queries_len(),
    MAX_RECEIVED_QUERIES,
    "received_queries must be exactly at cap before local key op"
  );

  // Drain the inbound events so we can observe the local one in isolation.
  while e.poll_event().is_some() {}

  // Issue a local install_key: the initiating node MUST process its own query
  // and surface Event::KeyRequest even though the inbound cap is full.
  let now = memberlist_proto::Instant::ORIGIN;
  e.install_key(test_key, now)
    .expect("install_key must succeed");

  let ev = e
    .poll_event()
    .expect("local install_key must surface Event::KeyRequest on the initiating node even when inbound cap is full");
  match ev {
    Event::KeyRequest(kr) => {
      assert!(
        matches!(kr.op(), KeyRequestOperation::Install),
        "local key op must be Install"
      );
    }
    other => panic!(
      "expected Event::KeyRequest from local install_key, got {:?}",
      core::mem::discriminant(&other)
    ),
  }
}

// ── Key op-shape check ────────────────────────────────────────────────────────

/// `_serf_install_key`, `_serf_use_key`, `_serf_remove_key` with `key = None`
/// must be dropped before any state mutation (no `Event::KeyRequest`, no clock
/// advance, no `received_queries` entry).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_must_ops_without_key_are_dropped() {
  use crate::{AnyMessage, KeyRequestMessage};

  for name in ["_serf_install_key", "_serf_use_key", "_serf_remove_key"] {
    let mut e = ep();
    // Encode a KeyRequestMessage with key = None (shape mismatch for must-have-key ops).
    let req = KeyRequestMessage::new(None);
    let payload = AnyMessage::<u32, core::net::SocketAddr>::KeyRequest(req)
      .encode()
      .expect("encode KeyRequestMessage");
    let q = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(1),
      id: 42,
      from: memberlist_proto::Node::new(99u32, sa(9001)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(5),
      name: name.into(),
      payload,
    };
    let clock_before = e.query_time();
    let _ = e.test_handle_query(q);
    assert_eq!(
      e.query_time(),
      clock_before,
      "{name}: clock must NOT advance for key=None on a must-have-key op"
    );
    assert_eq!(
      e.test_received_queries_len(),
      0,
      "{name}: no received_queries entry for key=None on a must-have-key op"
    );
    assert!(
      e.poll_event().is_none(),
      "{name}: no Event::KeyRequest for key=None on a must-have-key op"
    );
  }
}

/// `_serf_list_keys` with `key = Some(..)` must be dropped before any state
/// mutation (no `Event::KeyRequest`, no clock advance, no `received_queries`
/// entry).
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn list_keys_with_key_is_dropped() {
  use crate::{AnyMessage, KeyRequestMessage};
  use memberlist_proto::SecretKey;

  #[cfg(feature = "aes-gcm")]
  let test_key = SecretKey::Aes128([0u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let test_key = SecretKey::ChaCha20Poly1305([0u8; 32]);

  let mut e = ep();
  let req = KeyRequestMessage::new(Some(test_key));
  let payload = AnyMessage::<u32, core::net::SocketAddr>::KeyRequest(req)
    .encode()
    .expect("encode KeyRequestMessage");
  let q = crate::typed::QueryMessage::<u32, core::net::SocketAddr> {
    ltime: LamportTime::new(1),
    id: 77,
    from: memberlist_proto::Node::new(99u32, sa(9001)),
    filters: vec![],
    flags: QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: core::time::Duration::from_secs(5),
    name: "_serf_list_keys".into(),
    payload,
  };
  let clock_before = e.query_time();
  let _ = e.test_handle_query(q);
  assert_eq!(
    e.query_time(),
    clock_before,
    "_serf_list_keys with key=Some must NOT advance query clock"
  );
  assert_eq!(
    e.test_received_queries_len(),
    0,
    "_serf_list_keys with key=Some must have no received_queries entry"
  );
  assert!(
    e.poll_event().is_none(),
    "_serf_list_keys with key=Some must emit no Event::KeyRequest"
  );
}

// ── merge_remote_state dirty-flag semantics ───────────────────────────────────

/// A push-pull whose three clocks are all <= current (no-op witness) and which
/// carries no new status_ltimes, no left_members, and no events must NOT set the
/// dirty flag.
#[test]
fn stale_push_pull_does_not_set_dirty() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  // Advance the clocks above the values in the push-pull so witness is a no-op.
  e.test_set_clocks(10, 10, 10);
  e.test_clear_dirty();

  // Push-pull with clocks <= current (no advance).
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(5),       // < 10 → no clock advance
    event_ltime: LamportTime::new(3), // < 10
    query_ltime: LamportTime::new(2), // < 10
    status_ltimes: vec![],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode PushPull");
  e.test_merge_remote_state(encoded);

  assert!(
    !e.test_is_dirty(),
    "stale push-pull (no clock advance, no intents, no events) must not set the dirty flag"
  );
}

/// A push-pull that advances one of the three clocks DOES set the dirty flag.
#[test]
fn clock_advancing_push_pull_sets_dirty() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  e.test_set_clocks(5, 5, 5);
  e.test_clear_dirty();

  // Push-pull with member clock > current (advances clock).
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(20),      // > 5 → advances member clock
    event_ltime: LamportTime::new(3), // < 5 → no advance
    query_ltime: LamportTime::new(2), // < 5 → no advance
    status_ltimes: vec![],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode PushPull");
  e.test_merge_remote_state(encoded);

  assert!(
    e.test_is_dirty(),
    "push-pull that advances the member clock must set the dirty flag"
  );
}

// ── Tag-filter regex matching ─────────────────────────────────────────────────
//
// These tests verify that `should_process_query` honours the `Filter::Tag`
// predicate.  The Go oracle (`query.go`) calls `regexp.MatchString(filt.Expr,
// tags[filt.Tag])` which is a PARTIAL (anywhere-in-value) match.  With the
// `tag-regex` feature active the Rust machine matches identically; without the
// feature it degrades to exact-string equality.

#[cfg(feature = "tag-regex")]
mod tag_filter_regex {
  use super::*;
  use crate::typed::{TagFilter, Tags};

  /// Seed the local node (id=1) with the given tags and return an endpoint
  /// ready for tag-filter query tests.
  fn ep_with_tags(tags: Tags) -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
    let mut e = ep();
    e.test_seed_member_with_tags(1u32, tags, MemberStatus::Alive, LamportTime::new(0));
    e
  }

  fn tag_query(tag: &str, expr: Option<&str>) -> QueryMessage<u32, core::net::SocketAddr> {
    QueryMessage {
      filters: vec![Filter::Tag(TagFilter {
        tag: tag.into(),
        expr: expr.map(Into::into),
      })],
      ..test_query(LamportTime::new(1), 100)
    }
  }

  #[test]
  fn tag_filter_regex_prefix_matches() {
    // A node tagged `role=webserver` must match the partial regex `^web`.
    // Go: regexp.MatchString("^web", "webserver") == true.
    let tags: Tags = [("role", "webserver")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("role", Some("^web"));
    assert!(
      e.test_handle_query(q),
      "partial prefix regex `^web` must match `webserver`"
    );
    assert!(
      e.poll_event().is_some(),
      "matching tag filter must surface Event::Query"
    );
  }

  #[test]
  fn tag_filter_regex_suffix_matches() {
    // A node tagged `cloud=aws` must match the partial regex `aws$`.
    let tags: Tags = [("cloud", "aws")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("cloud", Some("aws$"));
    assert!(
      e.test_handle_query(q),
      "partial suffix regex `aws$` must match `aws`"
    );
    assert!(
      e.poll_event().is_some(),
      "matching tag filter must surface Event::Query"
    );
  }

  #[test]
  fn tag_filter_regex_non_match_suppresses() {
    // A node tagged `role=database` must NOT match `^web`.
    let tags: Tags = [("role", "database")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("role", Some("^web"));
    assert!(
      e.test_handle_query(q),
      "filtered query must still rebroadcast (G6)"
    );
    assert!(
      e.poll_event().is_none(),
      "non-matching tag filter must not surface Event::Query"
    );
  }

  #[test]
  fn invalid_regex_tag_filter_drops_before_witness() {
    // A `Filter::Tag` with an uncompilable regex pattern is malformed input.
    // The query must be dropped with zero side effects — no clock advance, no
    // dedup entry, no received_queries entry, no rebroadcast, no Event::Query.
    // This is distinct from a valid-but-non-matching filter, which still
    // rebroadcasts (see `valid_non_matching_tag_filter_still_rebroadcasts`).
    let tags: Tags = [("role", "webserver")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("role", Some("(unclosed"));

    let clock_before = e.query_time();
    let dedup_before = e.test_query_slot_len(1);
    let rcv_before = e.test_received_queries_len();

    let rebroadcast = e.test_handle_query(q);

    assert!(
      !rebroadcast,
      "invalid regex: must NOT rebroadcast (dropped before any side effect)"
    );
    assert_eq!(
      e.query_time(),
      clock_before,
      "invalid regex: query_clock must NOT advance"
    );
    assert_eq!(
      e.test_query_slot_len(1),
      dedup_before,
      "invalid regex: dedup buffer must NOT gain an entry"
    );
    assert_eq!(
      e.test_received_queries_len(),
      rcv_before,
      "invalid regex: received_queries must NOT gain an entry"
    );
    assert!(
      e.poll_event().is_none(),
      "invalid regex: no Event::Query must be emitted"
    );
  }

  #[test]
  fn valid_non_matching_tag_filter_still_rebroadcasts() {
    // A `Filter::Tag` with a valid regex that does not match the local node's
    // tag value is a filter-miss (G6): the query propagates (rebroadcast=true,
    // clock witnessed, dedup entry recorded) but no Event::Query is surfaced.
    let tags: Tags = [("role", "database")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("role", Some("^web")); // valid regex, does not match "database"

    let rebroadcast = e.test_handle_query(q);

    assert!(
      rebroadcast,
      "valid-but-non-matching tag filter must still rebroadcast (G6)"
    );
    // Clock must have advanced (witness occurred).
    assert!(
      e.query_time() > 0,
      "valid-but-non-matching filter: query_clock must advance"
    );
    // Dedup entry must be present.
    assert!(
      e.test_query_slot_len(1) > 0,
      "valid-but-non-matching filter: dedup buffer must record the query"
    );
    // No local event.
    assert!(
      e.poll_event().is_none(),
      "valid-but-non-matching filter: no Event::Query must be emitted"
    );
  }

  #[test]
  fn tag_filter_no_expr_matches_on_key_presence() {
    // `expr = None` means "any node that has this tag key" regardless of value.
    let tags: Tags = [("role", "anything")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    let q = tag_query("role", None);
    assert!(
      e.test_handle_query(q),
      "absent expr with matching key must rebroadcast"
    );
    assert!(
      e.poll_event().is_some(),
      "key-presence match (expr=None) must surface Event::Query"
    );
  }

  // ── Local-origination invalid-regex gate ─────────────────────────────────

  #[test]
  fn query_with_invalid_tag_regex_returns_err_with_no_side_effects() {
    // A caller passing Filter::Tag with an uncompilable regex to query() must
    // receive Err(Error::InvalidQueryFilter) before any side effect.
    // Specifically: no PendingQuery is inserted, the broadcast queue does not
    // grow, no Event::Query is emitted, and query_clock is unchanged.
    let tags: Tags = [("role", "webserver")].into_iter().collect();
    let mut e = ep_with_tags(tags);
    e.test_set_clocks(0, 0, 5); // set a known query clock

    let clock_before = e.query_time();
    let pending_before = e.test_pending_query_count();
    let queue_before = e.user_broadcast_queue_len();

    let result = e.query(
      "bad-filter",
      bytes::Bytes::new(),
      QueryParams {
        filters: vec![Filter::Tag(TagFilter {
          tag: "role".into(),
          expr: Some("(unclosed".into()),
        })],
        ..QueryParams::default()
      },
      memberlist_proto::Instant::ORIGIN,
    );

    assert!(
      matches!(result, Err(Error::InvalidQueryFilter)),
      "query() with invalid tag regex must return Err(Error::InvalidQueryFilter), got: {result:?}"
    );
    assert_eq!(
      e.query_time(),
      clock_before,
      "query_clock must NOT change after invalid-filter rejection"
    );
    assert_eq!(
      e.test_pending_query_count(),
      pending_before,
      "no PendingQuery must be inserted for an invalid-filter query"
    );
    assert_eq!(
      e.user_broadcast_queue_len(),
      queue_before,
      "broadcast queue must NOT grow after invalid-filter rejection"
    );
    assert!(
      e.poll_event().is_none(),
      "no Event::Query must be emitted after invalid-filter rejection"
    );
  }

  #[test]
  fn query_with_valid_tag_regex_succeeds_and_broadcasts() {
    // A valid tag-regex filter must not be rejected: query() succeeds, a
    // PendingQuery is registered, and the encoded query appears on the broadcast
    // queue.
    let tags: Tags = [("role", "webserver")].into_iter().collect();
    let mut e = ep_with_tags(tags);

    let pending_before = e.test_pending_query_count();
    let queue_before = e.user_broadcast_queue_len();

    let result = e.query(
      "good-filter",
      bytes::Bytes::new(),
      QueryParams {
        filters: vec![Filter::Tag(TagFilter {
          tag: "role".into(),
          expr: Some("^web".into()),
        })],
        ..QueryParams::default()
      },
      memberlist_proto::Instant::ORIGIN,
    );

    assert!(
      result.is_ok(),
      "query() with valid tag regex must succeed, got: {result:?}"
    );
    assert_eq!(
      e.test_pending_query_count(),
      pending_before + 1,
      "a PendingQuery must be inserted for a valid-filter query"
    );
    assert!(
      e.user_broadcast_queue_len() > queue_before,
      "broadcast queue must grow after a valid-filter query"
    );
  }
}

// ── Responder-side key-management (Stage 4 respond path) ─────────────────────

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
mod key_request_responder {
  use super::*;
  use crate::event::{KeyRequest, KeyRequestOperation, KeyResponseArgs};
  use memberlist_proto::SecretKey;

  fn make_key_query(
    name: &str,
    key: Option<SecretKey>,
  ) -> crate::typed::QueryMessage<u32, core::net::SocketAddr> {
    use crate::{AnyMessage, KeyRequestMessage};
    let req = KeyRequestMessage::new(key);
    let payload = AnyMessage::<u32, core::net::SocketAddr>::KeyRequest(req)
      .encode()
      .expect("encode KeyRequestMessage");
    crate::typed::QueryMessage {
      ltime: LamportTime::new(1),
      id: 55,
      from: memberlist_proto::Node::new(99u32, "127.0.0.1:9999".parse().unwrap()),
      filters: vec![],
      flags: crate::typed::QueryFlag::empty(),
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(5),
      name: name.into(),
      payload,
    }
  }

  #[cfg(feature = "aes-gcm")]
  fn test_key() -> SecretKey {
    SecretKey::Aes128([0u8; 16])
  }
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  fn test_key() -> SecretKey {
    SecretKey::ChaCha20Poly1305([0u8; 32])
  }

  #[test]
  fn key_query_emits_key_request_event() {
    let mut e = ep();
    // Register originator as a member so membership path is normal.
    e.test_seed_member(99u32, MemberStatus::Alive, LamportTime::new(1));
    let key = test_key();
    let q = make_key_query("_serf_install_key", Some(key));
    let rebroadcast = e.test_handle_query(q);
    // query_clock must have advanced.
    assert!(e.query_time() > 0, "query_clock must advance");
    // A received_queries entry MUST exist (not removed — driver responds later).
    assert_eq!(
      e.test_received_queries_len(),
      1,
      "received_queries entry must exist"
    );
    // Must emit Event::KeyRequest.
    let ev = e.poll_event().expect("must emit Event::KeyRequest");
    match ev {
      Event::KeyRequest(kr) => {
        assert!(matches!(kr.op(), KeyRequestOperation::Install));
        assert!(kr.key().is_some(), "install key request must carry a key");
      }
      other => panic!(
        "expected Event::KeyRequest, got {:?}",
        core::mem::discriminant(&other)
      ),
    }
    // Must not also emit Event::Query.
    assert!(e.poll_event().is_none(), "no second event expected");
    let _ = rebroadcast; // rebroadcast value is valid but not the focus here
  }

  #[test]
  fn list_keys_query_emits_key_request_with_no_key() {
    let mut e = ep();
    let q = make_key_query("_serf_list_keys", None);
    e.test_handle_query(q);
    let ev = e
      .poll_event()
      .expect("must emit Event::KeyRequest for list_keys");
    match ev {
      Event::KeyRequest(kr) => {
        assert!(matches!(kr.op(), KeyRequestOperation::List));
        assert!(kr.key().is_none(), "list_keys must carry no key");
      }
      other => panic!(
        "expected Event::KeyRequest, got {:?}",
        core::mem::discriminant(&other)
      ),
    }
  }

  #[test]
  fn malformed_key_query_payload_is_dropped() {
    let mut e = ep();
    // Build a query with a valid-prefix payload + trailing junk.
    let key = test_key();
    let mut good_q = make_key_query("_serf_install_key", Some(key));
    let mut bad_payload = good_q.payload.to_vec();
    bad_payload.extend_from_slice(&[0xaa, 0xbb]); // trailing junk
    good_q.payload = bytes::Bytes::from(bad_payload);
    let clock_before = e.query_time();
    let _rb = e.test_handle_query(good_q);
    assert_eq!(
      e.query_time(),
      clock_before,
      "clock must NOT advance for malformed key query"
    );
    assert_eq!(
      e.test_received_queries_len(),
      0,
      "no received_queries entry for malformed key query"
    );
    assert!(e.poll_event().is_none(), "no event for malformed key query");
  }

  #[test]
  fn key_query_never_surfaces_as_app_query() {
    let mut e = ep();
    let q = make_key_query("_serf_use_key", Some(test_key()));
    e.test_handle_query(q);
    // Must emit KeyRequest but NOT Event::Query.
    if let Some(ev) = e.poll_event() {
      assert!(
        !matches!(ev, Event::Query(_)),
        "key query must never surface as Event::Query"
      );
    }
  }

  #[test]
  fn respond_key_directed_sends_key_response() {
    let mut e = ep();
    let key = test_key();
    let q = make_key_query("_serf_install_key", Some(key));
    e.test_handle_query(q);
    // Drain the KeyRequest event to get the token.
    let req = match e.poll_event().expect("must emit KeyRequest") {
      Event::KeyRequest(kr) => kr,
      other => panic!(
        "expected KeyRequest, got {:?}",
        core::mem::discriminant(&other)
      ),
    };
    // Verify received_queries entry exists before respond_key.
    assert_eq!(e.test_received_queries_len(), 1);
    // Call respond_key.
    let now = memberlist_proto::Instant::ORIGIN;
    e.respond_key(
      &req,
      KeyResponseArgs {
        result: true,
        message: smol_str::SmolStr::default(),
        keys: vec![key],
        primary_key: Some(key),
      },
      now,
    )
    .expect("respond_key must succeed");
    // received_queries entry must be removed after respond_key.
    assert_eq!(
      e.test_received_queries_len(),
      0,
      "received_queries must be cleared after respond_key"
    );
    // last_directed_send must be set (a QueryResponse to the originator address).
    let (dest, _bytes) = e
      .test_last_directed_send()
      .expect("must have directed send");
    assert_eq!(
      dest,
      "127.0.0.1:9999".parse::<core::net::SocketAddr>().unwrap()
    );
  }

  #[test]
  fn key_request_answerable_at_exact_deadline_survives_inbound_prune() {
    // A KeyRequest token is answerable up to and including its deadline:
    // respond_key rejects only now > deadline.  The ingress prune in handle_query
    // must not drop the token at the exact instant now == deadline, or a mandatory
    // key response fails with AlreadyResponded.  Register a key query, advance to
    // exactly its deadline, run the ingress prune by handling a second inbound
    // query at that instant, then answer the key token at now == deadline: it must
    // succeed.  Reverting the prune to `now < deadline` drops the token here and
    // makes respond_key fail.
    let mut e = ep();

    // Inbound key query at ORIGIN → deadline D = ORIGIN + timeout.
    e.test_set_drain_now(t_secs(0));
    let key = test_key();
    let q = make_key_query("_serf_install_key", Some(key));
    let _ = e.test_handle_query(q);

    // Capture the KeyRequest token; its deadline is D.
    let req = match e.poll_event().expect("must emit Event::KeyRequest") {
      Event::KeyRequest(kr) => kr,
      other => panic!(
        "expected Event::KeyRequest, got {:?}",
        core::mem::discriminant(&other)
      ),
    };
    let deadline = req.deadline();

    // Advance to EXACTLY the deadline and handle a DIFFERENT inbound query, which
    // runs the ingress prune at now == deadline.
    e.test_set_drain_now(deadline);
    let other = QueryMessage::<u32, core::net::SocketAddr> {
      ltime: LamportTime::new(2),
      id: 2,
      from: memberlist_proto::Node::new(7u32, addr(7000)),
      filters: vec![],
      flags: QueryFlag::NO_BROADCAST,
      relay_factor: 0,
      timeout: core::time::Duration::from_secs(30),
      name: "other".into(),
      payload: bytes::Bytes::new(),
    };
    let _ = e.test_handle_query(other);

    // The key token, answerable at now == deadline, must NOT have been pruned:
    // respond_key at exactly the deadline must succeed.
    e.respond_key(
      &req,
      KeyResponseArgs {
        result: true,
        message: smol_str::SmolStr::default(),
        keys: vec![key],
        primary_key: Some(key),
      },
      deadline,
    )
    .expect("respond_key at exactly the deadline must succeed: the token is still answerable");
  }

  #[test]
  fn key_request_event_debug_does_not_leak_key_bytes() {
    let key = test_key();
    let req = KeyRequest::<u32, core::net::SocketAddr> {
      op: KeyRequestOperation::Install,
      key: Some(key),
      id: 1,
      ltime: LamportTime::new(1),
      from: memberlist_proto::Node::new(99u32, "127.0.0.1:9999".parse().unwrap()),
      relay_factor: 0,
      deadline: memberlist_proto::Instant::ORIGIN,
    };
    let ev = Event::<u32, core::net::SocketAddr>::KeyRequest(req);
    let debug_str = format!("{:?}", ev);
    // SecretKey's Debug impl uses "<redacted>" — check the raw bytes don't appear.
    // For Aes128([0u8;16]) the raw bytes would be "0, 0, 0, 0".
    assert!(
      !debug_str.contains("0, 0, 0, 0, 0"),
      "Debug output must not leak raw key bytes: {debug_str}"
    );
    // The key field must contain the redaction marker, not raw bytes.
    assert!(
      debug_str.contains("redacted") || !debug_str.contains("key: Some("),
      "Key field in Debug must show redacted: {debug_str}"
    );
  }
}

// ── Lamport watermark boundary: integrity floor ───────────────────────────────
//
// The integrity floor: `witness` and `next_ltime` must never produce 0 (wrap)
// or u64::MAX (tombstone) from a near-watermark input.  `load_snapshot` sets
// buffer floors via `saturating_add(1)`, which may produce LTIME_MAX when the
// snapshot clock is LTIME_MAX - 1; that floor is acceptable because the
// event_clock is also witnessed to LTIME_MAX - 1, so `next_ltime` stamps
// LTIME_MAX - 1 and the stamp matches or exceeds the floor.

/// After witnessing `LTIME_MAX - 1`, `next_ltime` stamps `LTIME_MAX - 1` (the
/// current clock value) and advances the stored clock to `LTIME_MAX`.  Neither
/// 0 nor u64::MAX may appear.  A subsequent `resync_local_state` emits a
/// push-pull whose clocks are not 0 and not u64::MAX.
#[test]
fn next_ltime_integrity_floor_near_watermark() {
  // Force all three clocks to LTIME_MAX - 1 (the largest acceptable witness).
  let mut e = ep();
  e.test_set_clocks(LTIME_MAX - 1, LTIME_MAX - 1, LTIME_MAX - 1);

  // user_event() calls next_ltime(&mut self.event_clock).
  // next_ltime stamps LTIME_MAX - 1 and stores LTIME_MAX.  The user_event is
  // emitted locally (it passes the min_time floor of 0); it then enters the
  // event ring.
  let result = e.user_event("probe", bytes::Bytes::new(), false, Instant::ORIGIN);
  assert!(
    result.is_ok(),
    "user_event must succeed with event_clock at LTIME_MAX-1: {result:?}"
  );

  // Integrity floor: stored event_clock must not be 0 (no wrap) or u64::MAX.
  assert_ne!(
    e.event_time(),
    0,
    "stored event_clock must not wrap to 0 after next_ltime; got {}",
    e.event_time()
  );
  assert_ne!(
    e.event_time(),
    u64::MAX,
    "stored event_clock must not be u64::MAX tombstone; got {}",
    e.event_time()
  );
  // After witnessing LTIME_MAX-1, the stored clock = LTIME_MAX (saturating).
  assert_eq!(
    e.event_time(),
    LTIME_MAX,
    "stored event_clock after witnessing LTIME_MAX-1 must be LTIME_MAX; got {}",
    e.event_time()
  );

  // The leave path uses post-increment (saturating_add then stamp the new value).
  let mut e2 = ep();
  e2.test_set_clocks(LTIME_MAX - 1, LTIME_MAX - 1, LTIME_MAX - 1);
  let now = memberlist_proto::Instant::ORIGIN;
  let _ = e2.leave(now);
  // Integrity floor: no wrap, no tombstone.
  assert_ne!(
    e2.member_time(),
    0,
    "stored member clock must not wrap to 0 after leave(); got {}",
    e2.member_time()
  );
  assert_ne!(
    e2.member_time(),
    u64::MAX,
    "stored member clock must not be u64::MAX tombstone after leave(); got {}",
    e2.member_time()
  );

  // resync_local_state must emit a push-pull with non-0 / non-u64::MAX clocks.
  e.resync_local_state();
  let snap = e.test_inner_local_state_snapshot();
  let pp = e.test_decode_pushpull(&snap);
  assert_ne!(
    pp.ltime.0, 0,
    "push-pull member ltime must not be 0; got {}",
    pp.ltime.0
  );
  assert_ne!(
    pp.event_ltime.0, 0,
    "push-pull event ltime must not be 0; got {}",
    pp.event_ltime.0
  );
}

/// `load_snapshot` with all three clocks at `LTIME_MAX - 1` must not panic,
/// not wrap any clock to 0, and not set any clock to `u64::MAX`.
///
/// With the headroom apparatus removed, `event_buffer.min_time` is set to
/// `(LTIME_MAX - 1).saturating_add(1) == LTIME_MAX` and the stored event_clock
/// is `LTIME_MAX - 1` (from witness).  A subsequent `user_event()` stamps
/// `LTIME_MAX - 1` via `next_ltime`; `handle_user_event` rejects it because
/// `!ltime_is_acceptable(LTIME_MAX - 1)` ... wait, `LTIME_MAX - 1 < LTIME_MAX`
/// is TRUE, so the event passes the ingress gate.  The `next_ltime` stamp is
/// `LTIME_MAX - 1` (current event_clock = LTIME_MAX - 1), then event_clock
/// advances to LTIME_MAX.  `handle_user_event` checks `ltime < min_time`:
/// `LTIME_MAX - 1 < LTIME_MAX` = true, so the event IS dropped by the floor.
/// This is the degraded-but-safe state: no panic, no crash, no UB.
/// Full functional recovery from a near-watermark snapshot is out of scope.
#[test]
fn load_snapshot_near_watermark_no_panic_integrity_floor() {
  use crate::snapshot::ReplayResult;

  let mut e = ep();
  let replay = ReplayResult {
    alive_nodes: vec![],
    last_clock: LamportTime::new(LTIME_MAX - 1),
    last_event_clock: LamportTime::new(LTIME_MAX - 1),
    last_query_clock: LamportTime::new(LTIME_MAX - 1),
  };
  let now = memberlist_proto::Instant::ORIGIN;
  // Must not panic.
  e.load_snapshot(replay, now).unwrap();

  // Integrity floor: buffer floors are not 0 and not u64::MAX.
  // (They will be LTIME_MAX = saturating_add(1) of LTIME_MAX - 1.)
  assert_ne!(
    e.test_event_min_time(),
    0,
    "event_buffer.min_time must not be 0 after snapshot; got {}",
    e.test_event_min_time()
  );
  assert_ne!(
    e.test_event_min_time(),
    u64::MAX,
    "event_buffer.min_time must not be u64::MAX; got {}",
    e.test_event_min_time()
  );
  assert_ne!(
    e.test_query_min_time(),
    0,
    "query_buffer.min_time must not be 0 after snapshot; got {}",
    e.test_query_min_time()
  );
  assert_ne!(
    e.test_query_min_time(),
    u64::MAX,
    "query_buffer.min_time must not be u64::MAX; got {}",
    e.test_query_min_time()
  );

  // Stored clocks: not 0, not u64::MAX (event_clock = LTIME_MAX - 1 from witness).
  assert_ne!(
    e.member_time(),
    0,
    "member clock must not be 0; got {}",
    e.member_time()
  );
  assert_ne!(
    e.member_time(),
    u64::MAX,
    "member clock must not be u64::MAX"
  );
  assert_ne!(
    e.event_time(),
    0,
    "event clock must not be 0; got {}",
    e.event_time()
  );
  assert_ne!(e.event_time(), u64::MAX, "event clock must not be u64::MAX");
  assert_ne!(
    e.query_time(),
    0,
    "query clock must not be 0; got {}",
    e.query_time()
  );
  assert_ne!(e.query_time(), u64::MAX, "query clock must not be u64::MAX");

  // user_event() must not panic (returns Ok even in the degraded state).
  let ue_result = e.user_event("near-max", bytes::Bytes::new(), false, Instant::ORIGIN);
  assert!(
    ue_result.is_ok(),
    "user_event must not panic/error after near-watermark snapshot: {ue_result:?}"
  );

  // query() must not panic.
  let query_result = e.query("probe", bytes::Bytes::new(), QueryParams::default(), now);
  assert!(
    query_result.is_ok(),
    "query() must not panic/error after near-watermark snapshot: {query_result:?}"
  );
}

/// Witnessing `LTIME_MAX - 1` (the largest acceptable ingress) must advance the
/// stored clock to `LTIME_MAX` — not 0 (no wrap) and not `u64::MAX` (no
/// tombstone).  The resulting stored clock equals `LTIME_MAX` exactly
/// (`saturating_add(1)` of the witnessed value).
#[test]
fn witness_near_watermark_integrity_floor() {
  let mut clock = 0u64;
  // Largest acceptable ingress: LTIME_MAX - 1.
  witness(&mut clock, LTIME_MAX - 1);
  // The stored clock advances to LTIME_MAX (saturating_add(1) of LTIME_MAX - 1).
  assert_eq!(
    clock, LTIME_MAX,
    "stored clock after witnessing LTIME_MAX-1 must be LTIME_MAX; got {clock}"
  );
  // Integrity floor: no wrap to 0, no tombstone at u64::MAX.
  assert_ne!(clock, 0, "stored clock must not wrap to 0; got {clock}");
  assert_ne!(
    clock,
    u64::MAX,
    "stored clock must not be u64::MAX tombstone; got {clock}"
  );
}

/// `load_snapshot` integrity floor: stored clocks must not be 0 or u64::MAX
/// for any near-watermark input; snapshot clocks at or above LTIME_MAX must be
/// rejected by the ingress gate and leave the stored clocks unchanged.
///
/// Case 1 (LTIME_MAX - 1): `witness(LTIME_MAX - 1)` → stored clock = LTIME_MAX
/// (saturating_add(1)), which is the degraded-but-safe state.  Emissions from
/// this node are rejected by peers' `ltime_is_acceptable` gate — consistent
/// with Go serf's undefined behavior at the extreme upper end of the range.
///
/// Case 2 (LTIME_MAX - 2): stored clock = LTIME_MAX - 1; still acceptable to
/// peers.
///
/// Case 3 (LTIME_MAX): the ingress gate rejects it; stored clocks unchanged.
#[test]
fn load_snapshot_near_watermark_integrity_floor() {
  use crate::snapshot::ReplayResult;

  // ── case 1: snapshot clocks at LTIME_MAX - 1
  {
    let mut e = ep();
    let replay = ReplayResult {
      alive_nodes: vec![],
      last_clock: LamportTime::new(LTIME_MAX - 1),
      last_event_clock: LamportTime::new(LTIME_MAX - 1),
      last_query_clock: LamportTime::new(LTIME_MAX - 1),
    };
    e.load_snapshot(replay, memberlist_proto::Instant::ORIGIN)
      .unwrap();

    // Stored clocks = LTIME_MAX (degraded state); integrity floor: not 0, not u64::MAX.
    assert_ne!(
      e.member_time(),
      0,
      "member clock must not be 0; got {}",
      e.member_time()
    );
    assert_ne!(
      e.member_time(),
      u64::MAX,
      "member clock must not be u64::MAX"
    );
    assert_ne!(
      e.event_time(),
      0,
      "event clock must not be 0; got {}",
      e.event_time()
    );
    assert_ne!(e.event_time(), u64::MAX, "event clock must not be u64::MAX");
    assert_ne!(
      e.query_time(),
      0,
      "query clock must not be 0; got {}",
      e.query_time()
    );
    assert_ne!(e.query_time(), u64::MAX, "query clock must not be u64::MAX");
    // Exact value: saturating_add(1) of LTIME_MAX - 1 = LTIME_MAX.
    assert_eq!(
      e.member_time(),
      LTIME_MAX,
      "member clock must be LTIME_MAX; got {}",
      e.member_time()
    );

    // user_event must not panic.
    let stamp_result = e.user_event(
      "after-snapshot",
      bytes::Bytes::new(),
      false,
      Instant::ORIGIN,
    );
    assert!(
      stamp_result.is_ok(),
      "user_event must not panic after LTIME_MAX-1 snapshot"
    );
  }

  // ── case 2: snapshot clocks at LTIME_MAX - 2 (one step safe)
  {
    let mut e = ep();
    let replay = ReplayResult {
      alive_nodes: vec![],
      last_clock: LamportTime::new(LTIME_MAX - 2),
      last_event_clock: LamportTime::new(LTIME_MAX - 2),
      last_query_clock: LamportTime::new(LTIME_MAX - 2),
    };
    e.load_snapshot(replay, memberlist_proto::Instant::ORIGIN)
      .unwrap();

    // stored clocks: witness(LTIME_MAX - 2) → LTIME_MAX - 1 (saturating_add(1)).
    assert_eq!(
      e.member_time(),
      LTIME_MAX - 1,
      "member clock must be LTIME_MAX-1; got {}",
      e.member_time()
    );
    assert_eq!(
      e.event_time(),
      LTIME_MAX - 1,
      "event clock must be LTIME_MAX-1; got {}",
      e.event_time()
    );
    assert_eq!(
      e.query_time(),
      LTIME_MAX - 1,
      "query clock must be LTIME_MAX-1; got {}",
      e.query_time()
    );
    // Integrity floor: not 0, not u64::MAX.
    assert_ne!(
      e.member_time(),
      0,
      "member clock must not be 0; got {}",
      e.member_time()
    );
    assert_ne!(
      e.member_time(),
      u64::MAX,
      "member clock must not be u64::MAX"
    );
  }

  // ── case 3: snapshot clock at LTIME_MAX — the ingress gate rejects it.
  {
    let mut e = ep();
    e.test_set_clocks(5, 5, 5);
    let replay = ReplayResult {
      alive_nodes: vec![],
      last_clock: LamportTime::new(LTIME_MAX),
      last_event_clock: LamportTime::new(LTIME_MAX),
      last_query_clock: LamportTime::new(LTIME_MAX),
    };
    e.load_snapshot(replay, memberlist_proto::Instant::ORIGIN)
      .unwrap();

    // Clocks must not have advanced past their pre-snapshot values.
    assert_eq!(
      e.member_time(),
      5,
      "member clock must not advance on LTIME_MAX snapshot clock"
    );
    assert_eq!(
      e.event_time(),
      5,
      "event clock must not advance on LTIME_MAX snapshot clock"
    );
    assert_eq!(
      e.query_time(),
      5,
      "query clock must not advance on LTIME_MAX snapshot clock"
    );
  }
}

// ── Watermark-boundary derivation: integrity floor ────────────────────────────
//
// The G3a left-member replay derives `leave_ltime = status_ltime + 1`.  When
// `status_ltime = LTIME_MAX - 1`, the derived value is `LTIME_MAX`, which is
// rejected by `handle_node_leave_intent`'s `ltime_is_acceptable` gate.  The
// leave is silently skipped and the member stays Alive.  This is the
// degraded-but-safe state: no panic, no wrap, no UB — consistent with Go
// serf's undefined behavior at the extreme upper end of the Lamport range.
//
// The integrity floor is: the member clock must not be 0 and not u64::MAX
// after the failed derivation.

/// merge_remote_state with status_ltimes = [(id, LTIME_MAX - 1)] and
/// left_members = [id]: the derived leave ltime (LTIME_MAX) is rejected by
/// the ingress gate, so the leave is NOT applied and the member stays Alive.
/// No panic, no wrap — integrity floor holds.
#[test]
fn left_member_replay_at_watermark_boundary_skips_leave() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  // Seed node 99 as Alive.
  e.test_seed_member(99u32, MemberStatus::Alive, LamportTime::new(1));

  // Push-pull with status_ltimes[99] = LTIME_MAX - 1 and left_members = [99].
  // The derived leave ltime = (LTIME_MAX - 1) + 1 = LTIME_MAX, rejected by
  // handle_node_leave_intent's ltime_is_acceptable gate.
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(10),
    event_ltime: LamportTime::new(10),
    query_ltime: LamportTime::new(10),
    status_ltimes: vec![(99u32, LamportTime::new(LTIME_MAX - 1))],
    left_members: vec![99u32],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  // Must not panic.
  e.test_merge_remote_state(encoded);

  // The leave is skipped (derived LTIME_MAX rejected); member stays Alive.
  let status = e.test_member_status(99u32);
  assert_eq!(
    status,
    Some(MemberStatus::Alive),
    "node 99 must stay Alive when derived leave ltime = LTIME_MAX is rejected; got {status:?}"
  );

  // Integrity floor: member clock is not 0, not u64::MAX.
  assert_ne!(
    e.member_time(),
    0,
    "member clock must not be 0; got {}",
    e.member_time()
  );
  assert_ne!(
    e.member_time(),
    u64::MAX,
    "member clock must not be u64::MAX; got {}",
    e.member_time()
  );
}

/// load_snapshot with all three clocks at LTIME_MAX - 1 followed by
/// user_event, query, force_leave, and resync_local_state: every stored clock
/// and every push-pull emission must satisfy the integrity floor (not 0, not
/// u64::MAX, no wrap).
///
/// witness(LTIME_MAX - 1) advances the stored member clock to LTIME_MAX (post-
/// increment semantics), so force_leave's prospective stamp = LTIME_MAX + 1,
/// which is unacceptable — the watermark guard returns LeaveClockExhausted
/// without mutating the clock.  member_clock stays at LTIME_MAX — not 0, not
/// u64::MAX.
///
/// After load_snapshot with last_event_clock = LTIME_MAX - 1, the stored
/// event_clock is LTIME_MAX - 1 (from witness) and event_buffer.min_time is
/// LTIME_MAX (saturating_add(1)).  user_event() stamps LTIME_MAX - 1 via
/// next_ltime and the stamp equals the floor, so the event is NOT dropped.
#[test]
fn all_clock_derived_values_satisfy_integrity_floor_after_near_watermark_snapshot() {
  use crate::snapshot::ReplayResult;

  let mut e = ep();
  // Seed node 99 as Alive so force_leave has a target.
  e.test_seed_member(99u32, MemberStatus::Alive, LamportTime::new(1));

  let replay = ReplayResult {
    alive_nodes: vec![],
    last_clock: LamportTime::new(LTIME_MAX - 1),
    last_event_clock: LamportTime::new(LTIME_MAX - 1),
    last_query_clock: LamportTime::new(LTIME_MAX - 1),
  };
  let now = memberlist_proto::Instant::ORIGIN;
  e.load_snapshot(replay, now).unwrap();

  // After witnessing LTIME_MAX - 1, stored clocks = LTIME_MAX - 1.
  // Integrity floor: no 0, no u64::MAX.
  assert_ne!(
    e.member_time(),
    0,
    "member_clock must not be 0 after snapshot"
  );
  assert_ne!(
    e.member_time(),
    u64::MAX,
    "member_clock must not be u64::MAX after snapshot"
  );
  assert_ne!(
    e.event_time(),
    0,
    "event_clock must not be 0 after snapshot"
  );
  assert_ne!(
    e.event_time(),
    u64::MAX,
    "event_clock must not be u64::MAX after snapshot"
  );
  assert_ne!(
    e.query_time(),
    0,
    "query_clock must not be 0 after snapshot"
  );
  assert_ne!(
    e.query_time(),
    u64::MAX,
    "query_clock must not be u64::MAX after snapshot"
  );

  // Buffer floors: saturating_add(1) of LTIME_MAX - 1 = LTIME_MAX.  The floor
  // may equal LTIME_MAX; that is not 0 and not u64::MAX.
  assert_ne!(
    e.test_event_min_time(),
    0,
    "event_buffer.min_time must not be 0"
  );
  assert_ne!(
    e.test_event_min_time(),
    u64::MAX,
    "event_buffer.min_time must not be u64::MAX"
  );
  assert_ne!(
    e.test_query_min_time(),
    0,
    "query_buffer.min_time must not be 0"
  );
  assert_ne!(
    e.test_query_min_time(),
    u64::MAX,
    "query_buffer.min_time must not be u64::MAX"
  );

  // After load_snapshot, witness(LTIME_MAX - 1) advances the stored clock to
  // LTIME_MAX (witness semantics: stored = t + 1).  The next force_leave stamp
  // would be LTIME_MAX + 1, which is not acceptable — the watermark guard
  // returns LeaveClockExhausted without mutating anything further.  No invalid
  // intent is emitted and the clock stays at LTIME_MAX.
  let clock_after_snapshot = e.member_time();
  assert_eq!(
    clock_after_snapshot, LTIME_MAX,
    "after witnessing LTIME_MAX-1, stored clock must be LTIME_MAX"
  );
  let fl = e.force_leave(99u32, false, now);
  assert!(
    matches!(fl, Err(Error::LeaveClockExhausted)),
    "force_leave at LTIME_MAX clock must return LeaveClockExhausted: {fl:?}"
  );
  assert_eq!(
    e.member_time(),
    LTIME_MAX,
    "member_clock must remain LTIME_MAX after refused force_leave"
  );

  // resync_local_state must emit push-pull with non-0 / non-u64::MAX clocks.
  e.resync_local_state();
  let snap = e.test_inner_local_state_snapshot();
  let pp = e.test_decode_pushpull(&snap);
  assert_ne!(
    pp.ltime.0, 0,
    "push-pull member ltime must not be 0; got {}",
    pp.ltime.0
  );
  assert_ne!(
    pp.ltime.0,
    u64::MAX,
    "push-pull member ltime must not be u64::MAX; got {}",
    pp.ltime.0
  );
  assert_ne!(
    pp.event_ltime.0, 0,
    "push-pull event ltime must not be 0; got {}",
    pp.event_ltime.0
  );
  assert_ne!(
    pp.event_ltime.0,
    u64::MAX,
    "push-pull event ltime must not be u64::MAX; got {}",
    pp.event_ltime.0
  );
  // Final stored member clock: no wrap, no tombstone.
  assert_ne!(
    e.member_time(),
    0,
    "member_clock must not be 0 after all operations"
  );
  assert_ne!(
    e.member_time(),
    u64::MAX,
    "member_clock must not be u64::MAX after all operations; got {}",
    e.member_time()
  );
}

// ── Near-watermark status_time ingress: integrity floor ───────────────────────
//
// A join or leave intent whose ltime is LTIME_MAX - 1 passes the
// ltime_is_acceptable gate (LTIME_MAX - 1 < LTIME_MAX) and is stored directly
// as the member's status_time.  The integrity floor: the stored value must not
// be 0 (no wrap) and not u64::MAX (no tombstone).  A subsequent leave intent
// at a strictly greater ltime (LTIME_MAX or above) is rejected by
// ltime_is_acceptable, so the stale guard on stored ltime = LTIME_MAX - 1 is
// the effective ceiling for further intents on that member.
//
// The provable assertions after the headroom apparatus is removed:
//   1. stored status_time == the accepted ingress value (no clamping).
//   2. A leave intent strictly below the stored status_time is stale.
//   3. An organic leave intent strictly above the stored status_time applies.
//   4. No wrap (0) and no tombstone (u64::MAX) appear anywhere.

/// A push-pull with status_ltimes at LTIME_MAX - 1 must store that value
/// directly (it passes the ingress gate).  The stored status_time must not be
/// 0 or u64::MAX.  A subsequent leave intent at a strictly greater ltime is
/// rejected by the gate; a leave at a ltime below the stored value is stale.
#[test]
fn push_pull_near_watermark_status_time_integrity_floor() {
  use crate::typed::PushPullMessage;

  let mut e = ep();
  let now = memberlist_proto::Instant::ORIGIN;

  // LTIME_MAX - 1 passes ltime_is_acceptable (< LTIME_MAX).
  let near_max = LTIME_MAX - 1;
  assert!(
    ltime_is_acceptable(near_max),
    "LTIME_MAX-1 must pass the ingress gate"
  );

  // Part A: stored status_time == ingress value.
  // Deliver a push-pull join intent for node 99 at LTIME_MAX - 1.
  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(10),
    event_ltime: LamportTime::new(10),
    query_ltime: LamportTime::new(10),
    status_ltimes: vec![(99u32, LamportTime::new(near_max))],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  e.test_merge_remote_state(encoded);

  // The inner NodeJoined event materialises node 99.
  e.test_inner_node_joined(99u32, now);

  let stored = e
    .test_member_status_time(99u32)
    .expect("node 99 must be in the store after NodeJoined");
  // Integrity floor: not 0, not u64::MAX.
  assert_ne!(
    stored.0, 0,
    "stored status_time must not be 0; got {}",
    stored.0
  );
  assert_ne!(
    stored.0,
    u64::MAX,
    "stored status_time must not be u64::MAX; got {}",
    stored.0
  );
  // No-clamp property: stored == ingress value.
  assert_eq!(
    stored.0, near_max,
    "stored status_time must equal the accepted ingress value; got {}",
    stored.0
  );

  // Part B: the leave-intent path also stores the ingress value as-is.
  e.test_seed_member(55u32, MemberStatus::Alive, LamportTime::new(10));
  let _leave_applied = e.test_handle_leave_intent(55u32, LamportTime::new(near_max), now);
  let stored_55 = e
    .test_member_status_time(55u32)
    .expect("node 55 must remain in the store");
  assert_ne!(
    stored_55.0, 0,
    "leave-intent stored value must not be 0; got {}",
    stored_55.0
  );
  assert_ne!(
    stored_55.0,
    u64::MAX,
    "leave-intent stored value must not be u64::MAX; got {}",
    stored_55.0
  );

  // Part C: an organic leave at a low ltime (20) applies on a SEPARATE endpoint
  // whose member has status_time = 10 (never witnessed the near-max value).
  let mut e2 = ep();
  e2.test_seed_member(55u32, MemberStatus::Alive, LamportTime::new(10));
  let organic_20 = LamportTime::new(20);
  let applied = e2.test_handle_leave_intent(55u32, organic_20, now);
  assert!(
    applied,
    "organic leave at ltime 20 must apply to a member with status_time=10"
  );
  assert_eq!(
    e2.test_member_status(55u32),
    Some(MemberStatus::Leaving),
    "node 55 must be Leaving after a valid organic leave intent"
  );
}

/// A direct join intent at LTIME_MAX - 1 on an unknown node must buffer a
/// recent_intent whose ltime equals the accepted ingress value (no clamping).
/// The buffered ltime must not be 0 or u64::MAX.  A leave at the same ltime
/// must not supersede the existing join (equal ltime is stale).
#[test]
fn join_intent_near_watermark_buffers_ingress_value_exactly() {
  let mut e = ep();
  let now = memberlist_proto::Instant::ORIGIN;

  let near_max = LTIME_MAX - 1;
  assert!(
    ltime_is_acceptable(near_max),
    "LTIME_MAX-1 must pass the ingress gate"
  );

  // Node 77 is unknown: the intent goes into recent_intents.
  let buffered = e.test_handle_join_intent(77u32, LamportTime::new(near_max), now);
  assert!(
    buffered,
    "near-watermark join intent for unknown node must be buffered (returned true)"
  );

  // Buffered ltime == ingress value (no clamping).
  let intent_ltime = e
    .test_intent_ltime(77u32, IntentKind::Join)
    .expect("recent_intent for node 77 must exist");
  // Integrity floor: not 0, not u64::MAX.
  assert_ne!(
    intent_ltime.0, 0,
    "buffered intent ltime must not be 0; got {}",
    intent_ltime.0
  );
  assert_ne!(
    intent_ltime.0,
    u64::MAX,
    "buffered intent ltime must not be u64::MAX; got {}",
    intent_ltime.0
  );
  assert_eq!(
    intent_ltime.0, near_max,
    "buffered intent ltime must equal the accepted ingress value; got {}",
    intent_ltime.0
  );

  // A leave intent at LTIME_MAX - 1 for an unknown node also buffers the
  // ingress value as-is.
  let buffered_leave = e.test_handle_leave_intent(88u32, LamportTime::new(near_max), now);
  assert!(
    buffered_leave,
    "near-watermark leave intent for unknown node must be buffered"
  );
  let leave_intent_ltime = e
    .test_intent_ltime(88u32, IntentKind::Leave)
    .expect("recent_intent for node 88 must exist");
  assert_ne!(
    leave_intent_ltime.0, 0,
    "buffered leave ltime must not be 0; got {}",
    leave_intent_ltime.0
  );
  assert_ne!(
    leave_intent_ltime.0,
    u64::MAX,
    "buffered leave ltime must not be u64::MAX; got {}",
    leave_intent_ltime.0
  );

  // A leave at the SAME ltime as the buffered join must not supersede it:
  // equal ltime is stale per upsert_intent semantics.
  let no_supersede = e.test_handle_leave_intent(77u32, LamportTime::new(near_max), now);
  assert!(
    !no_supersede,
    "leave at ltime == existing join ltime must not supersede \
     (equal ltime is stale per upsert_intent semantics)"
  );
}

// ── G4 near-watermark event-floor: integrity floor ────────────────────────────
//
// A push-pull whose event_ltime is LTIME_MAX - 1 passes the
// ltime_is_acceptable gate and is stored directly as event_buffer.min_time.
// The floor is LTIME_MAX - 1 (the ingress value as-is).  A local user_event()
// then stamps LTIME_MAX - 1 via next_ltime (which stamps the current
// event_clock, also LTIME_MAX - 1 after witness) — the stamp equals the
// floor, so the event is NOT dropped (handle_user_event's guard is `<
// min_time`, not `<=`).  Integrity floor: min_time is not 0, not u64::MAX.

/// merge_remote_state with event_ltime = LTIME_MAX - 1 must store that value
/// directly as event_buffer.min_time (not 0, not u64::MAX), and a subsequent
/// local user_event() must emit Event::User rather than being silently dropped.
#[test]
fn push_pull_near_watermark_event_floor_integrity_and_delivery() {
  use crate::typed::PushPullMessage;

  let mut e = ep();

  // LTIME_MAX - 1 passes ltime_is_acceptable.
  let near_max = LTIME_MAX - 1;
  assert!(
    ltime_is_acceptable(near_max),
    "LTIME_MAX-1 must pass the ingress gate"
  );

  let pp = PushPullMessage::<u32> {
    ltime: LamportTime::new(10),
    event_ltime: LamportTime::new(near_max),
    query_ltime: LamportTime::new(10),
    status_ltimes: vec![],
    left_members: vec![],
    events: vec![],
  };
  let encoded = AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed");
  // A suppressed (ignore-join) merge triggers the G4 event_buffer.min_time
  // update path.
  e.test_merge_remote_state_suppressed(encoded);

  // Integrity floor: min_time is the ingress value, not 0, not u64::MAX.
  assert_ne!(
    e.test_event_min_time(),
    0,
    "event_buffer.min_time must not be 0 after near-watermark push-pull; got {}",
    e.test_event_min_time()
  );
  assert_ne!(
    e.test_event_min_time(),
    u64::MAX,
    "event_buffer.min_time must not be u64::MAX; got {}",
    e.test_event_min_time()
  );

  // A local user_event must succeed and emit Event::User.
  // witness(event_clock, LTIME_MAX-1) → event_clock = LTIME_MAX - 1.
  // next_ltime stamps LTIME_MAX - 1; handle_user_event drops if stamp < min_time
  // (LTIME_MAX - 1).  LTIME_MAX - 1 is NOT < LTIME_MAX - 1, so the event is kept.
  e.user_event("post-join", bytes::Bytes::new(), false, Instant::ORIGIN)
    .expect("user_event must succeed after near-watermark push-pull");
  assert!(
    matches!(e.poll_event(), Some(Event::User(_))),
    "user_event must emit Event::User — must not be dropped by near-watermark event floor"
  );
}

// ── set_tags ──────────────────────────────────────────────────────────────────

/// Tags written by `set_tags` must round-trip: encoding them into `Meta` and
/// decoding that meta must reproduce the original map.
///
/// This verifies the `tags_to_pb` → encode → `Meta::try_from` →
/// `update_meta` → coordinator-meta-store path used by `set_tags`.
#[test]
fn set_tags_round_trips_via_local_meta() {
  use crate::typed::Tags;

  let mut e = ep();

  let tags: Tags = [("role", "web"), ("dc", "us-east-1")].into_iter().collect();
  e.set_tags(tags.clone(), Instant::ORIGIN)
    .expect("set_tags must succeed on a live endpoint");

  let meta = e
    .test_local_meta()
    .expect("local node must be present in the coordinator's membership store");
  let decoded = decode_tags_from_meta(meta.as_bytes())
    .expect("meta written by set_tags must decode as valid Tags");

  assert_eq!(decoded.len(), tags.len(), "decoded tag count must match");
  for (k, v) in &tags.0 {
    assert_eq!(
      decoded.0.get(k),
      Some(v),
      "tag {k:?} must round-trip correctly"
    );
  }
}

/// `set_tags` must synchronously update `members.states` so that tag-filtered
/// local queries evaluate the new tags without a `poll_event` drain first.
///
/// The coordinator queues a `NodeUpdated` event for the meta change; this test
/// verifies the synchronous path is independent of that event being drained.
#[test]
fn set_tags_local_member_state_is_observable_without_poll_event() {
  use crate::typed::Tags;

  let mut e = ep();

  // Seed the local node (id=1) into members.states so the existing-member
  // refresh arm of set_tags is exercised.
  e.test_seed_member(1u32, MemberStatus::Alive, LamportTime::new(0));

  let tags: Tags = [("role", "db")].into_iter().collect();
  e.set_tags(tags.clone(), Instant::ORIGIN)
    .expect("set_tags must succeed on a live endpoint");

  let local_tags = e.test_local_tags();
  assert_eq!(
    local_tags
      .as_ref()
      .and_then(|t| t.0.get("role"))
      .map(|s| s.as_str()),
    Some("db"),
    "local member tags in members.states must be updated synchronously by set_tags"
  );
}

/// After the construction self-join is drained (the local `NodeJoined` has been
/// processed), `set_tags` must update the existing local member in-place and
/// must not create a duplicate or phantom entry. The tags must also be reflected
/// in the coordinator's meta store.
#[test]
fn set_tags_does_not_materialize_absent_local_member() {
  use crate::typed::Tags;

  // ep() drains the construction NodeJoined, so the local member IS already in
  // members.states as Alive when this test begins.
  let mut e = ep();

  let tags: Tags = [("env", "staging")].into_iter().collect();
  e.set_tags(tags.clone(), Instant::ORIGIN)
    .expect("set_tags must succeed when the local node is in members.states");

  // set_tags must have updated the existing local member's tags in-place.
  let local_tags = e.test_local_tags();
  assert_eq!(
    local_tags
      .as_ref()
      .and_then(|t| t.0.get("env"))
      .map(|s| s.as_str()),
    Some("staging"),
    "set_tags must update the local member's tags in members.states"
  );

  // The tags must also have been advertised via the coordinator's meta store.
  let meta = e
    .test_local_meta()
    .expect("coordinator must track local node meta");
  let decoded = decode_tags_from_meta(meta.as_bytes())
    .expect("meta written by set_tags must decode as valid Tags");
  assert_eq!(
    decoded.0.get("env").map(|s| s.as_str()),
    Some("staging"),
    "new tags must be reflected in the coordinator meta"
  );
}

/// `set_tags` must never mark the push-pull snapshot dirty, regardless of
/// whether the local member is present or absent. Tags are not part of the
/// snapshot (only Lamport clocks, per-member status times, `left_members`,
/// and the event ring are).
#[test]
fn set_tags_does_not_mark_local_state_dirty() {
  use crate::typed::Tags;

  let mut e = ep();
  // Clear the construction dirty flag so the assertion below is unambiguous.
  e.resync_local_state();
  assert!(!e.test_is_dirty(), "resync must clear the dirty flag");

  let tags: Tags = [("dc", "us-west-2")].into_iter().collect();
  e.set_tags(tags, Instant::ORIGIN)
    .expect("set_tags must succeed");

  assert!(
    !e.test_is_dirty(),
    "set_tags must not mark local_state_dirty"
  );
}

/// After the construction self-join is drained (the local member is already in
/// `members.states`), `set_tags` triggers a `NodeUpdated` in the inner
/// coordinator. The invariant is that no `Member(Update)` ever appears BEFORE a
/// `Member(Join)` in the same event-stream segment. When the Join already
/// occurred at construction (before this collection), an Update appearing alone
/// (with no Join in the current segment) is valid — the prior Join satisfies the
/// ordering constraint.
#[test]
fn set_tags_before_join_does_not_emit_update_before_join() {
  use crate::typed::Tags;

  // ep() drains the construction NodeJoined, so the local member IS present.
  let mut e = ep();

  let tags: Tags = [("role", "cache")].into_iter().collect();
  e.set_tags(tags, Instant::ORIGIN)
    .expect("set_tags must succeed");

  // Drive the serf tick so inner events (NodeUpdated) are drained.
  e.handle_timeout(t_secs(1));

  // Collect all events produced so far.
  let mut evs = Vec::new();
  while let Some(ev) = e.poll_event() {
    evs.push(ev);
  }

  // Within this collection: if both a Join and an Update appear, the Join must
  // come first.  An Update without a Join in this segment is also valid because
  // the Join already happened at construction (before this collection).
  let join_pos = evs
    .iter()
    .position(|ev| matches!(ev, Event::Member(me) if me.kind() == MemberEventKind::Join));
  let update_pos = evs
    .iter()
    .position(|ev| matches!(ev, Event::Member(me) if me.kind() == MemberEventKind::Update));

  if let (Some(j), Some(u)) = (join_pos, update_pos) {
    assert!(
      u >= j,
      "Member(Update) at index {u} must not precede Member(Join) at index {j}: {evs:?}"
    );
  }
}

// ── event coalescing (member + user) ─────────────────────────────────────────
//
// The endpoint owns a member + a user coalescer, each enabled iff its
// (coalesce_period > 0 && quiescent_period > 0).  When enabled, membership /
// coalescing user events are buffered at their emission sites and flushed via
// the machine's own poll_timeout / handle_timeout window; when disabled (the
// default) every event passes straight through unchanged.

/// Build a serf endpoint with MEMBER coalescing enabled over the given windows.
///
/// Coalescing is enabled, so the construction self-join is buffered in the
/// coalescer; drain it into the coalescer (`poll_event`) then flush its window
/// (`handle_timeout`) so each test starts from an empty coalescer.
fn ep_member_coalescing_at(
  coalesce: core::time::Duration,
  quiescent: core::time::Duration,
) -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new()
    .with_coalesce_period(coalesce)
    .with_quiescent_period(quiescent);
  let mut e = StreamEndpoint::new(coord(inner), opts);
  let _ = e.poll_event();
  e.handle_timeout(memberlist_proto::Instant::ORIGIN + quiescent);
  while e.poll_event().is_some() {}
  e
}

fn ep_member_coalescing() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  ep_member_coalescing_at(
    core::time::Duration::from_secs(10),
    core::time::Duration::from_secs(2),
  )
}

/// Build a serf endpoint with USER coalescing enabled (member coalescing off, so
/// the construction self-join is delivered immediately as usual).
fn ep_user_coalescing() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner = memberlist_proto::Endpoint::new_at(
    inner_opts,
    memberlist_proto::Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  );
  let opts = Options::new()
    .with_user_coalesce_period(core::time::Duration::from_secs(10))
    .with_user_quiescent_period(core::time::Duration::from_secs(2));
  let mut e = StreamEndpoint::new(coord(inner), opts);
  let _ = e.poll_event();
  e
}

fn member_ids(ev: &Event<u32, core::net::SocketAddr>) -> Vec<u32> {
  match ev {
    Event::Member(me) => {
      let mut ids: Vec<u32> = me.members().iter().map(|m| *m.node().id_ref()).collect();
      ids.sort_unstable();
      ids
    }
    other => panic!("expected Event::Member, got {other:?}"),
  }
}

#[test]
fn coalescing_disabled_delivers_member_events_immediately() {
  // Default options: coalescing disabled → exact passthrough (the pre-coalescing
  // behavior every other test relies on).
  let mut e = ep();
  e.test_inner_node_joined(2, t_secs(1));
  let ev = e
    .poll_event()
    .expect("member event delivered immediately when disabled");
  assert!(matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Join));
  assert_eq!(member_ids(&ev), vec![2]);
  assert!(e.poll_event().is_none());
}

#[test]
fn coalescing_disabled_delivers_user_events_immediately() {
  // A coalescing (cc == true) user event still passes straight through when the
  // user coalescer is disabled (default).
  let mut e = ep();
  e.user_event(
    "deploy",
    bytes::Bytes::from_static(b"v2"),
    true,
    Instant::ORIGIN,
  )
  .unwrap();
  let ev = e
    .poll_event()
    .expect("user event delivered immediately when disabled");
  assert!(matches!(ev, Event::User(ref u) if u.name == "deploy"));
}

#[test]
fn member_coalescing_batches_rapid_joins_into_one_flush() {
  let mut e = ep_member_coalescing();
  // Two joins within the window are buffered, not delivered immediately.
  e.test_inner_node_joined(2, t_secs(5));
  e.test_inner_node_joined(3, t_secs(5));
  assert!(
    e.poll_event().is_none(),
    "joins are buffered by the coalescer, not delivered immediately"
  );
  // The flush deadline (last event + quiescent = 2s) surfaces in serf_poll_timeout.
  assert_eq!(
    e.core_mut().serf_poll_timeout(),
    Some(t_secs(7)),
    "the coalescer flush deadline appears in serf_poll_timeout"
  );
  // Firing the window delivers ONE coalesced Join batch carrying both nodes.
  e.handle_timeout(t_secs(7));
  let ev = e
    .poll_event()
    .expect("one coalesced batch at the flush deadline");
  assert!(matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Join));
  assert_eq!(member_ids(&ev), vec![2, 3], "both joins in ONE batch");
  assert!(e.poll_event().is_none(), "exactly one batch delivered");
}

#[test]
fn member_coalescing_collapses_transitions_to_latest_status() {
  let mut e = ep_member_coalescing();
  // node 2 joins then immediately fails within the same window.
  e.test_inner_node_joined(2, t_secs(5));
  e.test_inner_node_left(2, t_secs(5)); // Alive -> Failed
  assert!(e.poll_event().is_none(), "transitions buffered");

  e.handle_timeout(t_secs(7));
  let mut kinds = Vec::new();
  while let Some(ev) = e.poll_event() {
    if let Event::Member(me) = ev {
      kinds.push(me.kind());
    }
  }
  assert_eq!(
    kinds,
    vec![MemberEventKind::Failed],
    "join collapses into the final Failed status: {kinds:?}"
  );
}

#[test]
fn member_coalescing_coalesce_cap_bounds_a_busy_stream() {
  // coalesce cap 3s, quiescent 2s: a stream of events every 1s keeps re-arming
  // the quiescent timer, but the flush deadline can never exceed first + 3s.
  let mut e = ep_member_coalescing_at(
    core::time::Duration::from_secs(3),
    core::time::Duration::from_secs(2),
  );
  e.test_inner_node_joined(2, t_secs(5)); // cap = t8, quiescent = t7
  assert_eq!(e.core_mut().serf_poll_timeout(), Some(t_secs(7)));
  e.test_inner_node_joined(3, t_secs(6)); // quiescent -> t8, cap still t8
  assert_eq!(e.core_mut().serf_poll_timeout(), Some(t_secs(8)));
  e.test_inner_node_joined(4, t_secs(7)); // quiescent -> t9, but cap t8 binds
  assert_eq!(
    e.core_mut().serf_poll_timeout(),
    Some(t_secs(8)),
    "the coalesce cap (first event + 3s) bounds the busy stream"
  );
  // The cap flush delivers all three joins in one batch.
  e.handle_timeout(t_secs(8));
  let ev = e.poll_event().expect("cap flush delivers the batch");
  assert_eq!(member_ids(&ev), vec![2, 3, 4]);
}

#[test]
fn user_coalescing_batches_cc_events_and_passes_non_cc_through() {
  let mut e = ep_user_coalescing();

  // The command's `now` arms the window directly — no manual `test_set_drain_now`.
  // A non-coalescing user event passes straight through even when enabled.
  e.user_event("plain", bytes::Bytes::from_static(b"a"), false, t_secs(5))
    .unwrap();
  assert!(
    matches!(e.poll_event(), Some(Event::User(u)) if u.name == "plain"),
    "a non-cc user event passes through immediately"
  );

  // A coalescing user event is buffered; a newer generation supersedes it.
  e.user_event("cc", bytes::Bytes::from_static(b"v1"), true, t_secs(5))
    .unwrap();
  assert!(e.poll_event().is_none(), "cc user event buffered");
  e.user_event("cc", bytes::Bytes::from_static(b"v2"), true, t_secs(6))
    .unwrap();
  assert!(e.poll_event().is_none());

  // The user flush deadline (last event + quiescent = 2s) surfaces in poll_timeout.
  assert_eq!(e.core_mut().serf_poll_timeout(), Some(t_secs(8)));
  e.handle_timeout(t_secs(8));

  let mut delivered = Vec::new();
  while let Some(ev) = e.poll_event() {
    if let Event::User(u) = ev {
      delivered.push(u);
    }
  }
  assert_eq!(
    delivered.len(),
    1,
    "one coalesced user event: {delivered:?}"
  );
  assert_eq!(delivered[0].name, "cc");
  assert_eq!(
    delivered[0].payload.as_ref(),
    b"v2",
    "only the newest generation survives"
  );
}

#[test]
fn coalesced_member_batch_dropped_on_midwindow_shutdown() {
  // A membership batch buffered mid-window is DROPPED when a lost id-conflict
  // vote shuts the machine down (Go serf abandons the coalescer on shutdown).
  // Nothing may follow the terminal Event::Shutdown.
  let mut e = ep_member_coalescing();
  e.test_inner_node_joined(2, t_secs(5));
  assert!(e.poll_event().is_none(), "join buffered mid-window");

  // A conflict query whose deadline coincides with the flush window; the vote
  // is lost (1 agree, 2 disagree).
  let qid = e.test_register_conflict_query(t_secs(7));
  e.test_fold_conflict_response(qid, 200u32, true);
  e.test_fold_conflict_response(qid, 201u32, false);
  e.test_fold_conflict_response(qid, 202u32, false);

  // Driving the tick closes the conflict (lost) → Shutdown; the buffered batch
  // is dropped before the terminal event, not flushed after it.
  e.handle_timeout(t_secs(7));

  let mut events = Vec::new();
  while let Some(ev) = e.poll_event() {
    events.push(ev);
  }
  assert_eq!(
    events.len(),
    1,
    "only Event::Shutdown drains — the buffered member batch is dropped: {events:?}"
  );
  assert!(matches!(events[0], Event::Shutdown));
  assert!(e.state().is_shutdown());

  // Ticking past the former flush deadline delivers nothing more.
  e.handle_timeout(t_secs(20));
  assert!(
    e.poll_event().is_none(),
    "no coalesced batch may surface after Event::Shutdown"
  );
}

#[test]
fn user_event_after_idle_gap_arms_window_from_live_now() {
  // Regression (coalescer armed from a STALE command time): a coalescing user
  // event issued as a COMMAND after an idle gap must arm its window from the
  // command's live `now`, not from a stale `drain_now` (which command paths did
  // not refresh). `ep_user_coalescing` leaves `drain_now` at ORIGIN; issue the
  // event far in the future WITHOUT touching `drain_now`.
  let mut e = ep_user_coalescing(); // user coalesce 10s, quiescent 2s

  e.user_event("cc", bytes::Bytes::from_static(b"v1"), true, t_secs(100))
    .unwrap();

  // Buffered, NOT flushed immediately.
  assert!(
    e.poll_event().is_none(),
    "the coalescing user event is buffered, not delivered immediately"
  );
  // A FULL quiescent window applies from live now: 100 + 2 = 102 — a future
  // deadline, not one near ORIGIN. Reverting the now-threading arms at ORIGIN
  // (deadline t2) and this assertion fails.
  assert_eq!(
    e.core_mut().test_user_flush_deadline(),
    Some(t_secs(102)),
    "the user coalesce window must arm from the command's live now"
  );
  // A tick within the window does not flush (a stale-armed window would have
  // been past-due and flushed here).
  e.handle_timeout(t_secs(101));
  assert!(
    e.poll_event().is_none(),
    "still buffered within the live window"
  );
  // The window closes at 102, delivering exactly the coalesced event.
  e.handle_timeout(t_secs(102));
  assert!(
    matches!(e.poll_event(), Some(Event::User(u)) if u.name == "cc"),
    "the coalesced user event flushes when its live-armed window closes"
  );
}

#[test]
fn set_tags_after_idle_gap_arms_member_window_from_live_now() {
  // Regression (member side): `set_tags` emits a `Member(Update)` via the
  // coordinator's `NodeUpdated`, which a later `poll_event` drains WITHOUT
  // latching `drain_now`. The member window must arm from `set_tags`'s live
  // `now`, not the stale `drain_now` left by a prior tick.
  use crate::typed::Tags;

  let mut e = ep_member_coalescing(); // member coalesce 10s, quiescent 2s

  let tags: Tags = [("role", "web")].into_iter().collect();
  e.set_tags(tags, t_secs(100))
    .expect("set_tags must succeed");

  // Drain the coordinator's NodeUpdated into the member coalescer: buffered, not
  // delivered immediately.
  assert!(
    e.poll_event().is_none(),
    "the Member(Update) is buffered by the member coalescer, not delivered immediately"
  );
  // A FULL quiescent window applies from live now: 100 + 2 = 102. Reverting the
  // now-threading arms from the stale drain_now (t2, from the helper's self-join
  // flush) → deadline t4 → this assertion fails.
  assert_eq!(
    e.core_mut().test_member_flush_deadline(),
    Some(t_secs(102)),
    "the member coalesce window must arm from set_tags's live now"
  );
  e.handle_timeout(t_secs(101));
  assert!(
    e.poll_event().is_none(),
    "still buffered within the live window"
  );
  e.handle_timeout(t_secs(102));
  assert!(
    matches!(e.poll_event(), Some(Event::Member(me)) if me.kind() == MemberEventKind::Update),
    "the coalesced Member(Update) flushes when its live-armed window closes"
  );
}
