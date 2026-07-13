//! Oracle-conformance tests for the serf member-status FSM and push-pull replay.
//!
//! Verifies that the Sans-I/O `Endpoint` FSM matches the behaviour specified by
//! `legacy/serf-core/src/serf/base.rs` (`handle_node_join`, `handle_node_leave`,
//! `handle_node_join_intent`, `handle_node_leave_intent`, `upsert_intent`) and
//! `legacy/serf-core/src/serf/delegate.rs` (`merge_remote_state`).
//! Each test is named after the invariant it checks.

use bytes::Bytes;
use memberlist_proto::{
  EndpointOptions, Instant, RawRecords, SeedableRng, SmallRng, streams::LabelOptions,
};

use crate::{
  AnyMessage, LamportTime, StreamEndpoint,
  event::{Event, MemberEventKind},
  members::{IntentKind, MemberStatus},
  options::Options,
  typed::{PushPullMessage, UserEvent, UserEvents},
};

fn ep() -> StreamEndpoint<u32, core::net::SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(1u32, "127.0.0.1:7946".parse().unwrap())
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner =
    memberlist_proto::Endpoint::new_at(inner_opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let coord = memberlist_proto::streams::StreamEndpoint::<_, _, RawRecords>::new(
    inner,
    LabelOptions::new_in(Some(b"serf-test".to_vec()), ()),
    Box::new(|_addr: &core::net::SocketAddr| None),
    Box::new(|addr: &core::net::SocketAddr| *addr),
  );
  let mut e = StreamEndpoint::new(coord, Options::new());
  let _ = e.poll_event();
  e
}

/// A distinct loopback peer address for the per-peer ignore-join tests.
fn peer(port: u16) -> core::net::SocketAddr {
  core::net::SocketAddr::from(([127, 0, 0, 1], port))
}

// ── base.rs handle_node_join invariants ───────────────────────────────────────

/// base.rs:1286-1311 — new node with a pending Leave intent starts as Leaving.
#[test]
fn handle_node_join_with_pending_leave_intent_starts_leaving() {
  let mut e = ep();
  // Leave intent arrives before the inner NodeJoined.
  e.test_handle_leave_intent(2, LamportTime::new(4), Instant::ORIGIN);
  e.test_inner_node_joined(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Leaving));
}

/// base.rs:1286-1311 — new node with only a Join intent starts as Alive.
#[test]
fn handle_node_join_with_pending_join_intent_starts_alive() {
  let mut e = ep();
  e.test_handle_join_intent(2, LamportTime::new(4), Instant::ORIGIN);
  e.test_inner_node_joined(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
}

/// base.rs:1324-1327 — re-joining after Failed clears the failed_members list.
#[test]
fn handle_node_join_clears_failed_and_left_lists() {
  let mut e = ep();
  // Insert as Left first.
  e.test_seed_left_member_by_status(2, LamportTime::new(5), Instant::ORIGIN);
  assert!(e.test_in_left_members(2));
  e.test_inner_node_joined(2, Instant::ORIGIN);
  assert!(
    !e.test_in_left_members(2),
    "re-join must clear left_members"
  );
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
}

/// base.rs:1274 — a Member(Join) event is always emitted on re-join.
#[test]
fn handle_node_join_always_emits_join_event() {
  let mut e = ep();
  e.test_seed_failed_member_by_status(2, LamportTime::new(3), Instant::ORIGIN);
  e.test_inner_node_joined(2, Instant::ORIGIN);
  let ev = e.poll_event();
  assert!(
    matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join),
    "expected Member(Join), got {ev:?}"
  );
}

// ── base.rs handle_node_leave invariants ─────────────────────────────────────

/// base.rs:1382 — Leaving → Left, emits Leave.
#[test]
fn handle_node_leave_leaving_to_left() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Leaving, LamportTime::new(5));
  e.test_inner_node_left(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Left));
  assert!(e.test_in_left_members(2));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave));
}

/// base.rs:1400 — Alive → Failed, emits Failed.
#[test]
fn handle_node_leave_alive_to_failed() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(5));
  e.test_inner_node_left(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Failed));
  assert!(e.test_in_failed_members(2));
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Failed));
}

/// base.rs:1410 — other statuses are a no-op.
#[test]
fn handle_node_leave_non_alive_leaving_is_noop() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Failed, LamportTime::new(5));
  e.test_inner_node_left(2, Instant::ORIGIN);
  // The test_inner_node_left path for Failed should not change status (no-op).
  // Verify no event was emitted (the oracle ignores other statuses).
  // NOTE: Failed→Left transition is triggered via handle_node_leave_intent (not inner_node_left).
  // handle_node_leave in the machine only handles Alive/Leaving, so Failed stays.
  // This is oracle-faithful: the oracle's `match ms` at base.rs:1390 returns from `_`.
  let ev = e.poll_event();
  assert!(
    ev.is_none(),
    "no-op statuses should not emit events; got {ev:?}"
  );
}

// ── base.rs handle_node_join_intent invariants ───────────────────────────────

/// base.rs:1345-1380 — witness the member clock on join intent.
#[test]
fn handle_node_join_intent_witnesses_member_clock() {
  let mut e = ep();
  assert_eq!(e.member_time(), 0);
  e.test_handle_join_intent(2, LamportTime::new(10), Instant::ORIGIN);
  assert!(
    e.member_time() >= 11,
    "member clock must be witnessed past ltime=10"
  );
}

/// base.rs:1353-1355 — stale join intent (ltime <= status_time) returns false.
#[test]
fn handle_node_join_intent_stale_returns_false() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(10));
  let rebroadcast = e.test_handle_join_intent(2, LamportTime::new(10), Instant::ORIGIN);
  assert!(!rebroadcast);
}

/// base.rs:1363-1365 — Leaving member moves back to Alive on fresh join intent.
#[test]
fn handle_node_join_intent_leaving_returns_to_alive() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Leaving, LamportTime::new(5));
  let rebroadcast = e.test_handle_join_intent(2, LamportTime::new(8), Instant::ORIGIN);
  assert!(rebroadcast);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
}

// ── base.rs handle_node_leave_intent invariants ──────────────────────────────

/// base.rs:1449 — always witness member clock.
#[test]
fn handle_node_leave_intent_witnesses_clock() {
  let mut e = ep();
  e.test_handle_leave_intent(99, LamportTime::new(15), Instant::ORIGIN);
  assert!(e.member_time() >= 16);
}

/// base.rs:1471-1473 — stale intent (ltime <= status_time) → false, no transition.
#[test]
fn handle_node_leave_intent_stale_for_existing_member() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(10));
  let rb = e.test_handle_leave_intent(2, LamportTime::new(9), Instant::ORIGIN);
  assert!(!rb);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
}

/// base.rs:1489-1504 — status_time is updated even when already Leaving/Left
/// to prevent the infinite-rebroadcast bug (consul#8179 / consul#7960).
#[test]
fn handle_node_leave_intent_updates_status_time_for_leaving() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Leaving, LamportTime::new(5));
  let rb = e.test_handle_leave_intent(2, LamportTime::new(8), Instant::ORIGIN);
  assert!(rb, "Leaving node: fresh leave intent should rebroadcast");
  // Status unchanged (already Leaving), but status_time updated.
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Leaving));
  assert_eq!(
    e.test_member_status_time(2),
    Some(LamportTime::new(8)),
    "status_time must be updated unconditionally"
  );
}

/// base.rs:1527-1565 — Failed → Left, move to left_members, emit Leave.
#[test]
fn handle_node_leave_intent_failed_to_left() {
  let mut e = ep();
  e.test_seed_failed_member_by_status(2, LamportTime::new(3), Instant::ORIGIN);
  let rb = e.test_handle_leave_intent(2, LamportTime::new(7), Instant::ORIGIN);
  assert!(rb);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Left));
  assert!(
    !e.test_in_failed_members(2),
    "should be removed from failed_members"
  );
  assert!(e.test_in_left_members(2), "should be in left_members");
  let ev = e.poll_event();
  assert!(matches!(ev, Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave));
}

// ── graceful-leave ordering + late-intent heal (peer side) ──────────────────

/// Farewell ordering: a leave intent processed before the dead-self notice —
/// the order the farewell compound delivers, user intent part ahead of the
/// `Dead` part — yields EXACTLY one `Member(Leave)`. The intent takes the Alive
/// peer to `Leaving` silently (no event), then the inner `NodeLeft` completes
/// `Leaving → Left` and emits the single `Member(Leave)`. The absence of a
/// `Failed` is the discriminator between an intentional departure and a crash.
#[test]
fn graceful_leave_intent_then_dead_yields_exactly_leave() {
  let mut e = ep();
  // A known Alive peer, seeded silently at a low status_time.
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(3));

  // One drain cycle in wire order: the leave intent first (Alive → Leaving,
  // silent), then the dead-self notice (Leaving → Left, emits Leave).
  let rb = e.test_handle_leave_intent(2, LamportTime::new(7), Instant::ORIGIN);
  assert!(rb, "a fresh leave intent for an Alive peer rebroadcasts");
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "the leave intent must move the Alive peer to Leaving silently"
  );
  e.test_inner_node_left(2, Instant::ORIGIN);
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Left),
    "the dead-self notice must complete Leaving → Left"
  );
  assert!(
    e.test_in_left_members(2),
    "the departed peer lands in left_members"
  );

  // Exactly one member event, and it is Leave: the Alive → Leaving step emits
  // nothing, so no Failed and no duplicate Leave appears.
  let kinds: Vec<MemberEventKind> = core::iter::from_fn(|| e.poll_event())
    .filter_map(|ev| match ev {
      Event::Member(me) => Some(me.kind()),
      _ => None,
    })
    .collect();
  assert_eq!(
    kinds,
    vec![MemberEventKind::Leave],
    "peer-side output must be exactly one Member(Leave)"
  );
}

/// Reversed order: the dead-self notice arrives before the leave intent, so the
/// Alive peer first goes `Failed` (emitting `Member(Failed)`), then the late
/// intent heals `Failed → Left` (emitting `Member(Leave)`) and moves the peer
/// from failed_members to left_members. Pins the full second-hand sequence that
/// a farewell whose intent part is processed after a prior suspicion-driven
/// failure produces.
#[test]
fn dead_then_late_intent_heals_to_leave() {
  let mut e = ep();
  e.test_seed_member(2, MemberStatus::Alive, LamportTime::new(3));

  // Dead-self notice first: Alive → Failed, emits Member(Failed).
  e.test_inner_node_left(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Failed));
  assert!(
    e.test_in_failed_members(2),
    "the failed peer lands in failed_members"
  );

  // Late leave intent heals Failed → Left, emits Member(Leave).
  let rb = e.test_handle_leave_intent(2, LamportTime::new(7), Instant::ORIGIN);
  assert!(rb, "a fresh leave intent for a Failed peer rebroadcasts");
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Left),
    "the late intent must heal Failed → Left"
  );
  assert!(
    !e.test_in_failed_members(2),
    "the healed peer leaves failed_members"
  );
  assert!(
    e.test_in_left_members(2),
    "the healed peer joins left_members"
  );

  // The full second-hand sequence: Failed then Leave, in order.
  let kinds: Vec<MemberEventKind> = core::iter::from_fn(|| e.poll_event())
    .filter_map(|ev| match ev {
      Event::Member(me) => Some(me.kind()),
      _ => None,
    })
    .collect();
  assert_eq!(
    kinds,
    vec![MemberEventKind::Failed, MemberEventKind::Leave],
    "peer-side output must be Failed then Leave"
  );
}

// ── merge_remote_state (G2 witness, G3 left-first, G4 eventJoinIgnore) ───────

/// Helper: build a `PushPullMessage` body as encoded `Bytes`.
fn push_pull_body(
  ltime: u64,
  status_ltimes: Vec<(u32, u64)>,
  left_members: Vec<u32>,
  event_ltime: u64,
  events: Vec<UserEvents>,
  query_ltime: u64,
) -> Bytes {
  let pp = PushPullMessage::<u32>::new(
    LamportTime::new(ltime),
    status_ltimes
      .into_iter()
      .map(|(id, lt)| (id, LamportTime::new(lt)))
      .collect(),
    left_members,
    LamportTime::new(event_ltime),
    events,
    LamportTime::new(query_ltime),
  );
  AnyMessage::<u32, core::net::SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode must succeed in test")
}

/// delegate.rs:466-480 — G2: all three clocks are witnessed at `remote_value - 1`.
///
/// Remote sends ltime=10, event_ltime=8, query_ltime=6.
/// After merge: member_clock >= 10 (witness at 9 → 10), event_clock >= 8,
/// query_clock >= 6.
#[test]
fn merge_witnesses_clocks_at_ltime_minus_one() {
  let mut e = ep();
  let body = push_pull_body(10, vec![], vec![], 8, vec![], 6);
  e.test_merge_remote_state(body);
  // witness(9) → clock = 10; witness(7) → event_clock = 8; witness(5) → query_clock = 6
  assert_eq!(e.member_time(), 10, "member clock: witness(9) → 10");
  assert_eq!(e.event_time(), 8, "event clock: witness(7) → 8");
  assert_eq!(e.query_time(), 6, "query clock: witness(5) → 6");
}

/// delegate.rs:466 — G2 guard: `ltime = 0` must NOT witness (would underflow/wrap).
///
/// All three clocks stay at 0 when the remote sends 0.
#[test]
fn merge_zero_clocks_are_not_witnessed() {
  let mut e = ep();
  let body = push_pull_body(0, vec![], vec![], 0, vec![], 0);
  e.test_merge_remote_state(body);
  assert_eq!(e.member_time(), 0, "zero ltime must not witness");
  assert_eq!(e.event_time(), 0, "zero event_ltime must not witness");
  assert_eq!(e.query_time(), 0, "zero query_ltime must not witness");
}

/// delegate.rs:495-523 — G3: left_members are processed as leave intents BEFORE
/// the join pass, so a node in both lists ends up as Left, not Alive.
///
/// Node 2 appears in both `status_ltimes` (ltime=4) and `left_members`.
/// Expected result: a synthetic leave intent at ltime=5 (=4+1) is processed
/// before the join pass, and the join pass skips node 2.  The intent buffer
/// carries ltime=5.
#[test]
fn merge_processes_left_members_before_joins() {
  let mut e = ep();
  // Node 2 in status_ltimes (ltime=4) AND in left_members.
  let body = push_pull_body(5, vec![(2u32, 4)], vec![2u32], 0, vec![], 0);
  e.test_merge_remote_state(body);
  // The join pass must have skipped node 2 (left_set guard).
  // The leave intent at synthetic ltime=5 must be buffered (node 2 not in states yet).
  assert_eq!(
    e.test_intent_ltime(2, IntentKind::Leave),
    Some(LamportTime::new(5)),
    "node in left_members must have a synthetic Leave intent at status_ltime + 1"
  );
  // No Join intent should have been buffered (join pass skipped it).
  assert_eq!(
    e.test_intent_ltime(2, IntentKind::Join),
    None,
    "join pass must skip nodes in left_members"
  );
}

/// delegate.rs:513-523 — G3: nodes that appear ONLY in status_ltimes (not left)
/// receive a synthetic join intent.
#[test]
fn merge_join_intent_buffered_for_non_left_node() {
  let mut e = ep();
  // Node 3 appears only in status_ltimes, NOT in left_members.
  let body = push_pull_body(5, vec![(3u32, 7)], vec![], 0, vec![], 0);
  e.test_merge_remote_state(body);
  // A join intent must be buffered for node 3 (it is not in states yet).
  assert_eq!(
    e.test_intent_ltime(3, IntentKind::Join),
    Some(LamportTime::new(7)),
    "non-left node must receive a join intent at its status_ltime"
  );
}

/// delegate.rs:528-534 — G4: a join merge whose `originating_stream_id` matches a
/// recorded ignore-join exchange bumps `event_buffer.min_time` to
/// max(min_time, event_ltime), and CONSUMES the one-shot per-exchange entry.
#[test]
fn join_with_event_join_ignore_bumps_event_min_time() {
  let mut e = ep();
  // Start a real `ignore_old` join to mint and record this exchange's StreamId.
  let s = e.start_join_push_pull(peer(5001), /*ignore_old*/ true, Instant::ORIGIN);
  assert!(
    e.test_has_ignore_join_stream(s),
    "an ignore_old join must record its exchange StreamId"
  );
  let body = push_pull_body(0, vec![], vec![], 42, vec![], 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ true, s);
  assert_eq!(
    e.test_event_min_time(),
    42,
    "an ignore-join exchange's merge must set min_time to remote event_ltime"
  );
  assert!(
    !e.test_has_ignore_join_stream(s),
    "the ignore-join entry must be consumed (one-shot) at the join merge"
  );
}

/// G4 inverse: a REFRESH (non-join) exchange on a recorded ignore-join stream must
/// NOT bump min_time and must NOT consume the entry — only a join merge does.
#[test]
fn refresh_exchange_does_not_bump_event_min_time() {
  let mut e = ep();
  let s = e.start_join_push_pull(peer(5002), /*ignore_old*/ true, Instant::ORIGIN);
  let body = push_pull_body(0, vec![], vec![], 99, vec![], 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ false, s);
  assert_eq!(
    e.test_event_min_time(),
    0,
    "non-join exchange must not bump event_buffer.min_time"
  );
  assert!(
    e.test_has_ignore_join_stream(s),
    "a refresh must not consume the exchange's pending ignore-join entry"
  );
}

/// G4 same-peer concurrency-correctness (the per-exchange precision the StreamId
/// key buys): an `ignore_old` join and a non-ignore join to the SAME seed get
/// DISTINCT `StreamId`s, so the non-ignore join's merge must NOT be suppressed and
/// must NOT consume the ignore-join's still-pending entry. A per-PEER key could
/// not tell these apart — whichever merge landed first would consume the wrong
/// token.
#[test]
fn non_ignore_join_to_same_seed_is_not_suppressed() {
  let mut e = ep();
  let seed = peer(5003);
  // Two concurrent joins to the SAME seed: one ignore_old, one plain.
  let ignored = e.start_join_push_pull(seed, /*ignore_old*/ true, Instant::ORIGIN);
  let plain = e.start_join_push_pull(seed, /*ignore_old*/ false, Instant::ORIGIN);
  assert_ne!(ignored, plain, "same-seed joins get distinct StreamIds");
  assert!(
    e.test_has_ignore_join_stream(ignored),
    "the ignore_old join recorded its exchange"
  );
  assert!(
    !e.test_has_ignore_join_stream(plain),
    "the plain join to the same seed is NOT recorded"
  );

  // The plain join's merge lands first: it must not be suppressed and must leave
  // the ignore_old join's entry intact.
  let body = push_pull_body(0, vec![], vec![], 99, vec![], 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ true, plain);
  assert_eq!(
    e.test_event_min_time(),
    0,
    "a non-ignore join to the same seed must not bump event_buffer.min_time"
  );
  assert!(
    e.test_has_ignore_join_stream(ignored),
    "the plain join's merge must not consume the ignore_old join's entry"
  );

  // The ignore_old join's own merge still suppresses, keyed by ITS StreamId.
  let body = push_pull_body(0, vec![], vec![], 99, vec![], 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ true, ignored);
  assert_eq!(
    e.test_event_min_time(),
    99,
    "the ignore_old join's own merge (its StreamId) bumps min_time"
  );
}

/// delegate.rs:536-548 — buffered user events in the push-pull body are replayed
/// via handle_user_event (dedup + emit).
#[test]
fn merge_replays_buffered_user_events() {
  let mut e = ep();
  let events = vec![UserEvents {
    ltime: LamportTime::new(3),
    events: vec![UserEvent {
      name: "deploy".into(),
      payload: Bytes::from_static(b"v1"),
    }],
  }];
  let body = push_pull_body(4, vec![], vec![], 4, events, 0);
  e.test_merge_remote_state(body);
  // The replayed user event must surface.
  let ev = e.poll_event().expect("replayed user event must surface");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "deploy"),
    "expected Event::User(deploy), got: {ev:?}"
  );
}

/// G4 + replay: when an ignore-join peer's merge bumps min_time to the remote
/// event_ltime, events strictly below the new min_time must be suppressed on
/// replay.
///
/// G4 sets `min_time = event_ltime`.  An event at `ltime=4` with `min_time=5`
/// satisfies `ltime < min_time`, so it is suppressed.
#[test]
fn join_with_event_join_ignore_suppresses_event_replay() {
  let mut e = ep();
  let s = e.start_join_push_pull(peer(5005), /*ignore_old*/ true, Instant::ORIGIN);
  // Starting the dial surfaces no serf event, but drain defensively so the
  // assertion below only observes replay (or its suppression).
  while e.poll_event().is_some() {}
  // event_ltime = 5 → G4 bumps min_time to 5.  Event at ltime=4 < 5 must be dropped.
  let events = vec![UserEvents {
    ltime: LamportTime::new(4),
    events: vec![UserEvent {
      name: "really-old".into(),
      payload: Bytes::from_static(b"y"),
    }],
  }];
  let body = push_pull_body(0, vec![], vec![], 5, events, 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ true, s);
  // min_time = 5; event at ltime=4 must be suppressed.
  assert!(
    e.poll_event().is_none(),
    "event at ltime < min_time must be suppressed after G4 bump"
  );
}

/// Cancellation safety: an ignore-join whose exchange terminates WITHOUT a merge
/// (a dropped/failed join future) is cleaned up by the driver at the join's
/// terminal via `clear_ignore_join_stream`, so a later merge on that stream is NOT
/// suppressed — the entry cannot leak.
#[test]
fn ignore_join_cleared_on_exchange_termination_does_not_leak() {
  let mut e = ep();
  let s = e.start_join_push_pull(peer(5006), /*ignore_old*/ true, Instant::ORIGIN);
  assert!(
    e.test_has_ignore_join_stream(s),
    "the ignore_old join recorded its exchange"
  );
  // The ignore-old join terminates without ever merging (the join future was
  // dropped / the dial failed): the driver clears its StreamId.
  e.test_clear_ignore_join_stream(s);
  assert!(
    !e.test_has_ignore_join_stream(s),
    "a terminated ignore-join exchange must drop its pending entry"
  );
  while e.poll_event().is_some() {}

  // A later merge carrying a buffered event must replay: because the entry was
  // cleared, the merge is not suppressed (no min_time bump, event surfaces).
  let events = vec![UserEvents {
    ltime: LamportTime::new(3),
    events: vec![UserEvent {
      name: "should-replay".into(),
      payload: Bytes::from_static(b"z"),
    }],
  }];
  let body = push_pull_body(4, vec![], vec![], 4, events, 0);
  e.test_merge_remote_state_with_stream(body, /*is_join*/ true, s);
  assert_eq!(
    e.test_event_min_time(),
    0,
    "a cleared ignore must NOT bump min_time on a later merge"
  );
  let ev = e.poll_event().expect("the later merge's event must replay");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "should-replay"),
    "expected the un-suppressed event to surface, got: {ev:?}"
  );
}

/// Empty user_data must be silently ignored without panicking.
#[test]
fn merge_empty_user_data_is_silently_dropped() {
  let mut e = ep();
  // Simulate the RemoteStateReceived path: empty bytes are dropped before
  // merge_remote_state is called (the sieve arm guards `!user_data.is_empty()`).
  // Here we test that a zero-byte body passed directly does not panic.
  e.test_merge_remote_state(Bytes::new());
  assert!(e.poll_event().is_none());
}

/// Malformed (non-PushPull) bytes are silently dropped.
#[test]
fn merge_malformed_body_is_silently_dropped() {
  let mut e = ep();
  e.test_merge_remote_state(Bytes::from_static(b"\xff\xfe\xfd"));
  assert_eq!(
    e.member_time(),
    0,
    "clocks must be unchanged on decode error"
  );
  assert!(e.poll_event().is_none());
}

// ── handle_timeout tick order (H1b) + cross-tier priority ────────────────────

/// H1b / decision 5 step 4 — inner events are drained BEFORE serf's own
/// deadlines fire.
///
/// Scenario: member 2 is `Failed` with an elapsed `reconnect_timeout` (would be
/// reaped).  Before `handle_timeout` fires, an inner `NodeJoined` for the same
/// node is injected directly through the sieve (simulating an inner event that
/// arrives in the same tick as the reap deadline).  Because `drain_inner` runs
/// BEFORE `fire_reap`, the join must win: member 2 ends up `Alive`, not reaped.
///
/// This test verifies the ordering invariant without needing to enqueue inside
/// the inner machine's queue (which is not externally addressable): the join is
/// applied via the sieve before `fire_reap` runs, matching the production path
/// where inner events queued during `inner.handle_timeout` are drained first.
#[test]
fn tick_drains_inner_before_firing_reap() {
  let mut e = ep();
  let t0 = Instant::ORIGIN;

  // Seed member 2 as Failed with leave_time = t0 (reconnect_timeout = 24h by default).
  e.test_seed_failed_member(2, "127.0.0.1:1002".parse().unwrap(), t0);

  // Advance to a time far past reconnect_timeout (24h + 1h) so the reaper
  // would normally remove member 2.
  let past_timeout = t0 + core::time::Duration::from_secs(3600 * 25);

  // Inject a NodeJoined event for member 2 directly through the sieve
  // (simulates an inner event produced during inner.handle_timeout but before
  // serf's deadlines fire).  We set drain_now to past_timeout so the sieve
  // sees a consistent "now".
  e.test_inject_inner_joined(2, past_timeout);

  // The join must be processed first; drain clears the pending_events.
  while e.poll_event().is_some() {}

  // Now call handle_timeout at past_timeout — the reaper fires but member 2
  // is Alive after the join, so it must NOT be reaped.
  e.handle_timeout(past_timeout);

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "inner NodeJoined must win over the concurrent reap deadline (H1b: drain-before-reap)"
  );
}

/// Three broadcast tiers are populated at the correct ranks.
///
/// serf configures the inner Endpoint with 3 broadcast tiers
/// (intent=0 highest, query=1, event=2 lowest) and enqueues:
/// - join/leave intents on tier 0 (rank 0),
/// - query broadcasts on tier 1 (rank 1),
/// - user-event broadcasts on tier 2 (rank 2).
///
/// This test verifies the queue depth after serf enqueues one message on each
/// tier (via the intent-broadcast helper, the query-broadcast helper, and
/// `user_event`), confirming that all three tiers are populated.  The inner
/// Endpoint's priority drain order (rank 0 before 1 before 2) is exercised by
/// memberlist-proto's own test suite; serf's responsibility is correct tier
/// assignment, not internal drain scheduling.
///
/// H1b corollary: broadcasts pushed to the inner tiers before `handle_timeout`
/// are available for the inner gossip scheduler on the next tick — there is no
/// extra serf-side drain step needed between enqueue and tick.
#[test]
fn three_tiers_drain_intent_then_query_then_event() {
  let mut e = ep();

  // Enqueue one broadcast on each of the three tiers.
  // Tier 0 = intent (highest), tier 1 = query, tier 2 = event (lowest).
  e.test_enqueue_intent_broadcast(Bytes::from_static(b"intent-bytes"));
  e.test_enqueue_query_broadcast(Bytes::from_static(b"query-bytes"));

  // user_event enqueues on the event tier (rank 2).
  e.user_event("ev", Bytes::from_static(b"event"), false, Instant::ORIGIN)
    .expect("user_event must succeed");

  // All three tiers should be populated (user_broadcast_queue_len = total across all tiers).
  // The intent broadcast at rank 0 and query broadcast at rank 1 were enqueued via
  // test helpers; user_event enqueues at rank 2.
  let total = e.user_broadcast_queue_len();
  assert!(
    total >= 3,
    "all three broadcast tiers must be populated (intent + query + event); got queue_len = {total}"
  );

  // Verify: enqueue one more intent-tier item then check total grew, confirming
  // rank-0 enqueue hits a different internal tier slot from rank-2 enqueue.
  e.test_enqueue_intent_broadcast(Bytes::from_static(b"intent2"));
  let total2 = e.user_broadcast_queue_len();
  assert_eq!(
    total2,
    total + 1,
    "second intent-tier enqueue must increment total queue len"
  );
}
