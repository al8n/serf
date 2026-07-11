use super::*;

// ── determinism regression ────────────────────────────────────────────────────

/// Flood two identically-initialised `Members` stores with more than
/// `MAX_RECENT_INTENTS` leave intents for distinct unknown node ids, all
/// stamped with the SAME `Instant`.  Without a sequence-number tie-break the
/// eviction is decided by `HashMap` iteration order (randomised per process),
/// so the two stores retain *different* subsets.  With the fix the composite
/// `(wall_time, sequence)` key makes eviction purely a function of insertion
/// order — identical input sequence → identical retained set on both stores.
///
/// A subsequent `recent_intent` lookup for a specific node then returns the
/// same answer on both stores, and a downstream `handle_node_join` for that
/// node would produce the same `MemberStatus` on both machines.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn upsert_intent_cap_eviction_is_deterministic() {
  use memberlist_proto::Instant;

  type TestMembers = Members<u32, core::net::SocketAddr>;

  // Use cap + a surplus so we trigger exactly one eviction per surplus insert.
  let n = MAX_RECENT_INTENTS + 16;

  // Both stores receive the SAME sequence of intents in the SAME order.
  // All share the same wall_time to maximise tie collisions.
  let shared_now = Instant::ORIGIN;

  let mut m1 = TestMembers::default();
  let mut m2 = TestMembers::default();

  for node_id in 0u32..(n as u32) {
    let ltime = LamportTime::new(node_id as u64 + 1);
    m1.upsert_intent(&node_id, IntentKind::Leave, ltime, shared_now);
    m2.upsert_intent(&node_id, IntentKind::Leave, ltime, shared_now);
  }

  // Both stores must be at exactly MAX_RECENT_INTENTS.
  assert_eq!(m1.recent_intents.len(), MAX_RECENT_INTENTS);
  assert_eq!(m2.recent_intents.len(), MAX_RECENT_INTENTS);

  // The retained key sets must be identical.
  let keys1: std::collections::BTreeSet<u32> = m1.recent_intents.keys().cloned().collect();
  let keys2: std::collections::BTreeSet<u32> = m2.recent_intents.keys().cloned().collect();
  assert_eq!(
    keys1, keys2,
    "two identically-seeded stores retained different intent subsets after cap eviction"
  );

  // The first `n - MAX_RECENT_INTENTS` ids (oldest-inserted) must have been
  // evicted; the most recently inserted ones must be retained.
  let evicted_count = n - MAX_RECENT_INTENTS;
  for evicted_id in 0u32..(evicted_count as u32) {
    assert!(
      !m1.recent_intents.contains_key(&evicted_id),
      "node {evicted_id} should have been evicted (oldest-inserted) but was retained"
    );
  }
  for retained_id in (evicted_count as u32)..(n as u32) {
    assert!(
      m1.recent_intents.contains_key(&retained_id),
      "node {retained_id} should be retained but was evicted"
    );
  }

  // Verify that a `recent_intent` lookup for a retained node returns the same
  // answer on both stores — confirming downstream FSM decisions are identical.
  let probe_retained = evicted_count as u32;
  assert_eq!(
    m1.recent_intent(&probe_retained, IntentKind::Leave),
    m2.recent_intent(&probe_retained, IntentKind::Leave),
    "retained node lookup diverged between the two stores"
  );

  // And for an evicted node both stores agree it is absent.
  let probe_evicted = 0u32;
  assert_eq!(
    m1.recent_intent(&probe_evicted, IntentKind::Leave),
    m2.recent_intent(&probe_evicted, IntentKind::Leave),
  );
  assert_eq!(m1.recent_intent(&probe_evicted, IntentKind::Leave), None);
}

#[test]
fn member_status_as_str_round_trips() {
  assert_eq!(MemberStatus::None.as_str(), "none");
  assert_eq!(MemberStatus::Alive.as_str(), "alive");
  assert_eq!(MemberStatus::Leaving.as_str(), "leaving");
  assert_eq!(MemberStatus::Left.as_str(), "left");
  assert_eq!(MemberStatus::Failed.as_str(), "failed");
  assert!(MemberStatus::default().is_none());
}

#[test]
fn serf_state_as_str() {
  assert_eq!(SerfState::Alive.as_str(), "alive");
  assert_eq!(SerfState::Shutdown.as_str(), "shutdown");
}

#[test]
fn member_status_display_matches_as_str() {
  for status in [
    MemberStatus::None,
    MemberStatus::Alive,
    MemberStatus::Leaving,
    MemberStatus::Left,
    MemberStatus::Failed,
  ] {
    assert_eq!(status.to_string(), status.as_str());
  }
}

#[test]
fn serf_state_display_matches_as_str() {
  for state in [
    SerfState::Alive,
    SerfState::Leaving,
    SerfState::Left,
    SerfState::Shutdown,
  ] {
    assert_eq!(state.to_string(), state.as_str());
  }
}

#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn upsert_intent_newest_ltime_wins() {
  use memberlist_proto::Instant;

  type TestMembers = Members<u32, core::net::SocketAddr>;
  let mut m = TestMembers::default();
  let t0 = Instant::ORIGIN;

  // First insert
  assert!(m.upsert_intent(&1u32, IntentKind::Join, LamportTime::new(5), t0));
  assert_eq!(
    m.recent_intent(&1u32, IntentKind::Join),
    Some(LamportTime::new(5))
  );

  // Same ltime — not updated
  assert!(!m.upsert_intent(&1u32, IntentKind::Join, LamportTime::new(5), t0));

  // Older ltime — not updated
  assert!(!m.upsert_intent(&1u32, IntentKind::Join, LamportTime::new(3), t0));
  assert_eq!(
    m.recent_intent(&1u32, IntentKind::Join),
    Some(LamportTime::new(5))
  );

  // Newer ltime — updated
  assert!(m.upsert_intent(&1u32, IntentKind::Join, LamportTime::new(7), t0));
  assert_eq!(
    m.recent_intent(&1u32, IntentKind::Join),
    Some(LamportTime::new(7))
  );
}

#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn recent_intent_kind_mismatch_returns_none() {
  use memberlist_proto::Instant;

  type TestMembers = Members<u32, core::net::SocketAddr>;
  let mut m = TestMembers::default();

  m.upsert_intent(
    &2u32,
    IntentKind::Leave,
    LamportTime::new(3),
    Instant::ORIGIN,
  );
  // Looking for Join when only Leave is stored
  assert_eq!(m.recent_intent(&2u32, IntentKind::Join), None);
  // Leave matches
  assert_eq!(
    m.recent_intent(&2u32, IntentKind::Leave),
    Some(LamportTime::new(3))
  );
}

/// Mirrors Go serf `test_remove_old_member`
/// (legacy/serf-core/src/serf/base/tests/serf/remove.rs): removing a named node
/// from a reaper index list drops only that entry and retains the others.
///
/// The legacy helper retained `MemberState`s by node id; the Sans-I/O machine
/// tracks the index lists as plain id `Vec`s, so this asserts the same invariant
/// against the id-list form used by `handle_node_join` reconcile and
/// `prune_member`.
#[test]
fn remove_old_member_drops_only_the_named_id() {
  use smol_str::SmolStr;

  let mut old: Vec<SmolStr> = vec!["foo".into(), "bar".into(), "baz".into()];
  remove_old_member(&mut old, &SmolStr::from("bar"));
  assert_eq!(old.len(), 2);
  assert!(
    !old.contains(&SmolStr::from("bar")),
    "named id must be removed"
  );
  assert!(
    old.contains(&SmolStr::from("foo")),
    "other ids must be retained"
  );
  assert!(
    old.contains(&SmolStr::from("baz")),
    "other ids must be retained"
  );
}
