use std::{net::SocketAddr, sync::Arc};

use memberlist_proto::Node;
use serf_proto::{
  LamportTime,
  members::{Member, MemberStatus, SerfState},
};

use super::SerfSnapshot;

fn make_member(id: u32, addr: &str, status: MemberStatus) -> Arc<Member<u32, SocketAddr>> {
  let node = Node::new(id, addr.parse::<SocketAddr>().unwrap());
  Arc::new(Member::new(node, Default::default(), status))
}

#[test]
fn snapshot_counts() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);
  let carol = make_member(3, "127.0.0.1:7948", MemberStatus::Left);

  let snap = SerfSnapshot::new(
    vec![alice, bob, carol],
    &1u32,
    SerfState::Alive,
    LamportTime::new(10),
    LamportTime::new(20),
    LamportTime::new(30),
  );

  // Counts are derived from the member view, so they always agree with it.
  assert_eq!(snap.member_count(), 3);
  assert_eq!(snap.num_members(), 3);
  assert_eq!(snap.alive_count(), 2);
  assert_eq!(snap.alive_count(), snap.online_members().count());
  assert_eq!(snap.member_count(), snap.members().len());
}

#[test]
fn snapshot_by_id() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);
  let carol = make_member(3, "127.0.0.1:7948", MemberStatus::Left);

  let snap = SerfSnapshot::new(
    vec![alice, bob, carol],
    &1u32,
    SerfState::Alive,
    LamportTime::new(1),
    LamportTime::new(2),
    LamportTime::new(3),
  );

  assert!(snap.by_id(&1).is_some());
  assert!(snap.by_id(&2).is_some());
  assert!(snap.by_id(&3).is_some());
  assert!(snap.by_id(&99).is_none());
}

#[test]
fn snapshot_online_members() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);
  let carol = make_member(3, "127.0.0.1:7948", MemberStatus::Left);

  let snap = SerfSnapshot::new(
    vec![alice, bob, carol],
    &1u32,
    SerfState::Alive,
    LamportTime::new(5),
    LamportTime::new(6),
    LamportTime::new(7),
  );

  let online: Vec<_> = snap.online_members().collect();
  assert_eq!(online.len(), 2);
}

#[test]
fn snapshot_clock_accessors() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);

  let member_clock = LamportTime::new(100);
  let event_clock = LamportTime::new(200);
  let query_clock = LamportTime::new(300);

  let snap = SerfSnapshot::new(
    vec![alice],
    &1u32,
    SerfState::Leaving,
    member_clock,
    event_clock,
    query_clock,
  );

  assert_eq!(snap.member_clock(), member_clock);
  assert_eq!(snap.event_clock(), event_clock);
  assert_eq!(snap.query_clock(), query_clock);
  assert_eq!(snap.state(), SerfState::Leaving);
}

#[test]
fn snapshot_members_by_and_num_members_by() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);
  let carol = make_member(3, "127.0.0.1:7948", MemberStatus::Left);

  let snap = SerfSnapshot::new(
    vec![alice, bob, carol],
    &1u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );

  let alive_count = snap.num_members_by(|m| m.status() == MemberStatus::Alive);
  assert_eq!(alive_count, 2);

  let alive_members: Vec<_> = snap
    .members_by(|m| m.status() == MemberStatus::Alive)
    .collect();
  assert_eq!(alive_members.len(), 2);
}

#[test]
fn snapshot_local_and_members_slice() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);

  let snap = SerfSnapshot::new(
    vec![alice],
    &1u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );

  assert_eq!(snap.local().node().id_ref(), &1u32);
  assert_eq!(snap.local_ref().node().id_ref(), &1u32);
  assert_eq!(snap.members().len(), 1);
  assert_eq!(snap.members_slice().len(), 1);
}

#[test]
fn snapshot_members_map_by() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Failed);

  let snap = SerfSnapshot::new(
    vec![alice, bob],
    &1u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );

  let ids = snap.members_map_by(|m| Some(*m.node().id_ref()));
  assert_eq!(ids.len(), 2);
}

/// `local()` / `local_ref()` must return the exact same `Arc` that lives at
/// `local_index` in `members()` — no independent copy that could diverge.
#[test]
fn local_is_indexed_member() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);

  // local is alice (id 1), found at index 0.
  let snap = SerfSnapshot::new(
    vec![alice, bob],
    &1u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );

  // `local()` is a clone of the same Arc, so ptr_eq holds.
  assert!(Arc::ptr_eq(&snap.local(), &snap.members()[0]));
  assert_eq!(snap.local().node().id_ref(), &1u32);

  // local is bob (id 2), found at index 1.
  let snap2 = SerfSnapshot::new(
    vec![
      make_member(1, "127.0.0.1:7946", MemberStatus::Alive),
      make_member(2, "127.0.0.1:7947", MemberStatus::Alive),
    ],
    &2u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );
  assert!(Arc::ptr_eq(&snap2.local(), &snap2.members()[1]));
  assert_eq!(snap2.local().node().id_ref(), &2u32);
}

/// `new` must panic at construction time when `local_id` is not present in `members`,
/// making the driver invariant violation visible immediately rather than returning a
/// snapshot that would panic (or silently misbehave) later on `local()`.
#[test]
#[should_panic(expected = "local node must be present in members")]
fn new_panics_when_local_id_absent() {
  let alice = make_member(1, "127.0.0.1:7946", MemberStatus::Alive);
  let bob = make_member(2, "127.0.0.1:7947", MemberStatus::Alive);
  // id 99 is not present — must panic.
  let _ = SerfSnapshot::new(
    vec![alice, bob],
    &99u32,
    SerfState::Alive,
    LamportTime::ZERO,
    LamportTime::ZERO,
    LamportTime::ZERO,
  );
}

/// `stats()` assembles the aggregate from the member view plus the
/// driver-attached live readings: failed/left counts come from the statuses,
/// and the ops-stats builder carries health score, queue depth, and the
/// encryption flag; the defaults are the zero posture.
#[test]
fn stats_assembles_counts_and_ops_readings() {
  let members = vec![
    make_member(1, "127.0.0.1:7946", MemberStatus::Alive),
    make_member(2, "127.0.0.1:7947", MemberStatus::Failed),
    make_member(3, "127.0.0.1:7948", MemberStatus::Failed),
    make_member(4, "127.0.0.1:7949", MemberStatus::Left),
    make_member(5, "127.0.0.1:7950", MemberStatus::Leaving),
  ];
  let snap = SerfSnapshot::new(
    members,
    &1u32,
    SerfState::Alive,
    LamportTime::new(7),
    LamportTime::new(8),
    LamportTime::new(9),
  );

  // Defaults before the driver attaches its live readings.
  let zero = snap.stats();
  assert_eq!(zero.health_score(), 0);
  assert_eq!(zero.broadcast_queue_depth(), 0);
  assert!(!zero.encrypted());

  let snap = snap.with_ops_stats(2, 5, true);
  let stats = snap.stats();
  assert_eq!(stats.members(), 5);
  assert_eq!(stats.failed(), 2);
  assert_eq!(stats.left(), 1, "Leaving is not Left");
  assert_eq!(stats.health_score(), 2);
  assert_eq!(stats.broadcast_queue_depth(), 5);
  assert!(stats.encrypted());
  assert_eq!(stats.member_clock(), LamportTime::new(7));
  assert_eq!(stats.event_clock(), LamportTime::new(8));
  assert_eq!(stats.query_clock(), LamportTime::new(9));
}
