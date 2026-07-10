use core::{net::SocketAddr, time::Duration};

use std::{collections::VecDeque, vec, vec::Vec};

use memberlist_proto::{Instant, Node};

use crate::{
  LamportTime, UserEventMessage,
  event::{Event, MemberEventKind},
  members::{Member, MemberStatus},
  typed::Tags,
};

use super::{MemberEventCoalescer, UserEventCoalescer};

// ── helpers ────────────────────────────────────────────────────────────────────

fn secs(n: u64) -> Duration {
  Duration::from_secs(n)
}

fn addr() -> SocketAddr {
  "127.0.0.1:8080".parse().unwrap()
}

fn member(id: u32) -> Member<u32, SocketAddr> {
  Member::new(Node::new(id, addr()), Tags::new(), MemberStatus::None)
}

fn member_tagged(id: u32, role: &str) -> Member<u32, SocketAddr> {
  Member::new(
    Node::new(id, addr()),
    Tags::from_iter([("role", role)]),
    MemberStatus::None,
  )
}

fn uev(name: &str, ltime: u64, payload: &str) -> UserEventMessage {
  UserEventMessage {
    ltime: LamportTime::new(ltime),
    cc: true,
    name: name.into(),
    payload: bytes::Bytes::copy_from_slice(payload.as_bytes()),
  }
}

/// Drain a flushed queue into `(kind, member-ids)` groups for order-independent
/// assertions (the flush groups by kind but does not sort).
fn member_groups(out: VecDeque<Event<u32, SocketAddr>>) -> Vec<(MemberEventKind, Vec<u32>)> {
  out
    .into_iter()
    .map(|ev| match ev {
      Event::Member(me) => {
        let mut ids: Vec<u32> = me.members().iter().map(|m| *m.node().id_ref()).collect();
        ids.sort_unstable();
        (me.kind(), ids)
      }
      other => panic!("expected Event::Member, got {other:?}"),
    })
    .collect()
}

fn find_group(
  groups: &[(MemberEventKind, Vec<u32>)],
  kind: MemberEventKind,
) -> Option<&(MemberEventKind, Vec<u32>)> {
  groups.iter().find(|(k, _)| *k == kind)
}

fn user_events(out: VecDeque<Event<u32, SocketAddr>>) -> Vec<UserEventMessage> {
  out
    .into_iter()
    .map(|ev| match ev {
      Event::User(u) => u,
      other => panic!("expected Event::User, got {other:?}"),
    })
    .collect()
}

// ── window timing ───────────────────────────────────────────────────────────────

#[test]
fn empty_member_window_has_no_deadline() {
  let c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  assert_eq!(c.flush_deadline(), None);
  assert!(!c.due(Instant::ORIGIN));
  assert!(!c.due(Instant::ORIGIN + secs(1000)));
}

#[test]
fn empty_user_window_has_no_deadline() {
  let c = UserEventCoalescer::new(secs(10), secs(2));
  assert_eq!(c.flush_deadline(), None);
  assert!(!c.due(Instant::ORIGIN));
}

#[test]
fn quiescent_binds_before_coalesce_on_a_single_event() {
  // quiescent (2s) < coalesce (10s): a lone event flushes at first + quiescent.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN + secs(1);
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  assert_eq!(c.flush_deadline(), Some(t0 + secs(2)));
  assert!(!c.due(t0 + secs(1)));
  assert!(c.due(t0 + secs(2)));
}

#[test]
fn coalesce_caps_the_maximum_batch_delay() {
  // A steady stream keeps re-arming the quiescent timer, but the coalesce
  // max-window is armed once on the first event and never advances, so the flush
  // deadline can never exceed first_event + coalesce_period.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(8));
  let t0 = Instant::ORIGIN;
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  // First arm: min(t0 + 10, t0 + 8) = t0 + 8.
  assert_eq!(c.flush_deadline(), Some(t0 + secs(8)));
  // A later event re-arms quiescent to t0 + 13, but coalesce stays t0 + 10.
  c.feed(MemberEventKind::Join, vec![member(2)], t0 + secs(5));
  assert_eq!(c.flush_deadline(), Some(t0 + secs(10)));
}

#[test]
fn flush_disarms_the_window() {
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  assert!(c.flush_deadline().is_some());
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(c.flush_deadline(), None, "window disarms after a flush");
  assert!(!c.due(t0 + secs(1000)));
}

// ── member dedup (ports serf-core coalesce/member.rs) ────────────────────────────

#[test]
fn member_flush_collapses_to_latest_status_per_node() {
  // Ports `test_member_event_coealesce_basic`: rapid transitions per node
  // collapse to the final status; the flush groups survivors by kind.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  // node 1: Join then Leave -> Leave wins.
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  c.feed(MemberEventKind::Leave, vec![member(1)], t0);
  // node 2: Leave.
  c.feed(MemberEventKind::Leave, vec![member(2)], t0);
  // node 3: Update then Update -> the latter (role=bar) wins.
  c.feed(MemberEventKind::Update, vec![member_tagged(3, "foo")], t0);
  c.feed(MemberEventKind::Update, vec![member_tagged(3, "bar")], t0);
  // node 4: Reap.
  c.feed(MemberEventKind::Reap, vec![member(4)], t0);

  let mut out = VecDeque::new();
  c.flush(&mut out);
  let groups = member_groups(out);

  assert_eq!(groups.len(), 3, "Leave, Update, Reap (no Join): {groups:?}");
  assert_eq!(
    find_group(&groups, MemberEventKind::Leave).map(|(_, ids)| ids.as_slice()),
    Some([1u32, 2].as_slice()),
    "node 1 (join->leave) and node 2 collapse into one Leave batch"
  );
  assert_eq!(
    find_group(&groups, MemberEventKind::Reap).map(|(_, ids)| ids.as_slice()),
    Some([4u32].as_slice())
  );
  assert!(
    find_group(&groups, MemberEventKind::Join).is_none(),
    "node 1's Join is superseded by its Leave"
  );
}

#[test]
fn member_update_carries_the_latest_tags() {
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  c.feed(MemberEventKind::Update, vec![member_tagged(3, "foo")], t0);
  c.feed(MemberEventKind::Update, vec![member_tagged(3, "bar")], t0);

  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(out.len(), 1);
  match out.pop_front().unwrap() {
    Event::Member(me) => {
      assert_eq!(me.kind(), MemberEventKind::Update);
      assert_eq!(me.members().len(), 1);
      assert_eq!(
        me.members()[0].tags().0.get("role").map(|s| s.as_str()),
        Some("bar")
      );
    }
    other => panic!("expected Update, got {other:?}"),
  }
}

#[test]
fn member_update_always_re_emits_across_flushes() {
  // Ports `test_member_event_coalesce_tag_update`: a second Update for a node is
  // NOT suppressed even though the last emitted kind was already Update, because
  // its tags may have changed.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;

  c.feed(MemberEventKind::Update, vec![member_tagged(1, "foo")], t0);
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(out.len(), 1, "first Update delivered");

  c.feed(
    MemberEventKind::Update,
    vec![member_tagged(1, "bar")],
    t0 + secs(1),
  );
  let mut out2 = VecDeque::new();
  c.flush(&mut out2);
  assert_eq!(out2.len(), 1, "second Update re-emitted, not suppressed");
  match out2.pop_front().unwrap() {
    Event::Member(me) => {
      assert_eq!(me.kind(), MemberEventKind::Update);
      assert_eq!(
        me.members()[0].tags().0.get("role").map(|s| s.as_str()),
        Some("bar")
      );
    }
    other => panic!("expected Update, got {other:?}"),
  }
}

#[test]
fn member_repeated_same_status_is_suppressed() {
  // A non-Update kind unchanged since the last flush is suppressed (the node is
  // not re-announced).  Only Update is exempt from this suppression.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;

  c.feed(MemberEventKind::Failed, vec![member(1)], t0);
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(out.len(), 1, "first Failed delivered");

  c.feed(MemberEventKind::Failed, vec![member(1)], t0 + secs(1));
  let mut out2 = VecDeque::new();
  c.flush(&mut out2);
  assert!(
    out2.is_empty(),
    "repeated Failed for the same node is suppressed"
  );
}

#[test]
fn member_reset_drops_buffer_without_emitting() {
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  assert!(c.flush_deadline().is_some());

  c.reset();
  assert_eq!(c.flush_deadline(), None, "reset disarms the window");

  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert!(out.is_empty(), "a reset coalescer flushes nothing");
}

// ── user dedup (ports serf-core coalesce/user.rs) ────────────────────────────────

#[test]
fn user_flush_keeps_newest_generation_per_name() {
  // Ports `test_user_event_coalesce_basic`: foo@1 then foo@2 keeps only foo@2;
  // bar@2(test1) then bar@2(test2) keeps both (same generation).
  let mut c = UserEventCoalescer::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  c.feed(uev("foo", 1, ""), t0);
  c.feed(uev("foo", 2, ""), t0);
  c.feed(uev("bar", 2, "test1"), t0);
  c.feed(uev("bar", 2, "test2"), t0);

  let mut out = VecDeque::new();
  c.flush::<u32, SocketAddr>(&mut out);
  let events = user_events(out);

  let foo: Vec<_> = events.iter().filter(|e| e.name == "foo").collect();
  assert_eq!(foo.len(), 1, "only the newest foo generation survives");
  assert_eq!(foo[0].ltime, LamportTime::new(2));

  let mut bar_payloads: Vec<&[u8]> = events
    .iter()
    .filter(|e| e.name == "bar")
    .map(|e| e.payload.as_ref())
    .collect();
  bar_payloads.sort_unstable();
  assert_eq!(
    bar_payloads,
    vec![b"test1".as_slice(), b"test2".as_slice()],
    "both same-generation bar payloads survive"
  );
}

#[test]
fn user_older_generation_is_dropped() {
  let mut c = UserEventCoalescer::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;
  c.feed(uev("foo", 5, "new"), t0);
  c.feed(uev("foo", 3, "old"), t0);

  let mut out = VecDeque::new();
  c.flush::<u32, SocketAddr>(&mut out);
  let events = user_events(out);
  assert_eq!(events.len(), 1);
  assert_eq!(events[0].ltime, LamportTime::new(5));
  assert_eq!(events[0].payload.as_ref(), b"new");
}

#[test]
fn user_reset_drops_buffer_without_emitting() {
  let mut c = UserEventCoalescer::new(secs(10), secs(2));
  c.feed(uev("foo", 1, "x"), Instant::ORIGIN);
  assert!(c.flush_deadline().is_some());

  c.reset();
  assert_eq!(c.flush_deadline(), None);
  let mut out = VecDeque::new();
  c.flush::<u32, SocketAddr>(&mut out);
  assert!(out.is_empty());
}
