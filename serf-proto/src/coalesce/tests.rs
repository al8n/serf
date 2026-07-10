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
fn reap_then_rejoin_in_one_window_still_emits_the_join() {
  // A prior window records `last[id] = Join`. Within a single later window the
  // node is Reaped and then rejoins (Join) before the window closes. The Reap is
  // forgotten from `last` at feed time, so the stale `last[id] == Join` can no
  // longer suppress the rejoin — the Join must still be delivered.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;

  // Window 1: a plain Join records `last[1] = Join`.
  c.feed(MemberEventKind::Join, vec![member(1)], t0);
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(member_groups(out), vec![(MemberEventKind::Join, vec![1])]);

  // Window 2: Reap then a rejoin Join for the same id, within one window.
  let t1 = t0 + secs(5);
  c.feed(MemberEventKind::Reap, vec![member(1)], t1);
  c.feed(MemberEventKind::Join, vec![member(1)], t1);
  let mut out2 = VecDeque::new();
  c.flush(&mut out2);
  let groups = member_groups(out2);

  assert_eq!(
    find_group(&groups, MemberEventKind::Join).map(|(_, ids)| ids.as_slice()),
    Some([1u32].as_slice()),
    "the rejoin Join must not be suppressed by the pre-Reap last[id]: {groups:?}"
  );
}

#[test]
fn member_reap_evicts_suppression_entry_bounding_last_to_live_membership() {
  // The cross-flush `last` map must track only LIVE membership: a node whose
  // flushed terminal event is `Reap` is gone from membership for good, so its id
  // is evicted.  Churning many DISTINCT ids through join → reap must therefore
  // NOT grow `last` — reverting the eviction makes it grow to the total ids ever
  // seen and this assertion fails.
  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));

  // Three long-lived members that join once and never leave: they stay in `last`.
  for id in 0..3u32 {
    c.feed(MemberEventKind::Join, vec![member(id)], Instant::ORIGIN);
  }
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(c.last_len(), 3, "three live members are tracked");

  // Churn 1000 distinct transient ids through join (flush) → reap (flush).
  let mut t = Instant::ORIGIN;
  for id in 100..1100u32 {
    c.feed(MemberEventKind::Join, vec![member(id)], t);
    let mut j = VecDeque::new();
    c.flush(&mut j);
    c.feed(MemberEventKind::Reap, vec![member(id)], t);
    let mut r = VecDeque::new();
    c.flush(&mut r);
    t += secs(1);
  }

  assert_eq!(
    c.last_len(),
    3,
    "after 1000 join→reap cycles `last` still tracks only the 3 live members, \
     not the 1000 reaped ids (reverting Reap-eviction grows it to 1003)"
  );
}

#[test]
fn address_change_rejoin_is_not_suppressed() {
  // A prior window records `last[id] = (Join, addr_a)`. When the node then leaves
  // addr_a and rejoins at addr_b within a later window, `latest[id]` collapses to
  // Join(addr_b). Suppression keys on (kind, address), so even though the last
  // emitted kind was also Join the changed address defeats it and the move must
  // be delivered — consumers would otherwise retain the stale addr_a.
  let addr_a: SocketAddr = "127.0.0.1:5000".parse().unwrap();
  let addr_b: SocketAddr = "127.0.0.1:6000".parse().unwrap();
  let at = |a: SocketAddr| Member::new(Node::new(7u32, a), Tags::new(), MemberStatus::None);

  let mut c = MemberEventCoalescer::<u32, SocketAddr>::new(secs(10), secs(2));
  let t0 = Instant::ORIGIN;

  // Window 1: Join at addr_a records `last[7] = (Join, addr_a)`.
  c.feed(MemberEventKind::Join, vec![at(addr_a)], t0);
  let mut out = VecDeque::new();
  c.flush(&mut out);
  assert_eq!(member_groups(out), vec![(MemberEventKind::Join, vec![7])]);

  // Window 2: Leave(addr_a) then a rejoin Join(addr_b), collapsing to Join(addr_b).
  let t1 = t0 + secs(5);
  c.feed(MemberEventKind::Leave, vec![at(addr_a)], t1);
  c.feed(MemberEventKind::Join, vec![at(addr_b)], t1);
  let mut out2 = VecDeque::new();
  c.flush(&mut out2);

  let joined: Vec<(u32, SocketAddr)> = out2
    .iter()
    .filter_map(|ev| match ev {
      Event::Member(me) if me.kind() == MemberEventKind::Join => Some(
        me.members()
          .iter()
          .map(|m| (*m.node().id_ref(), *m.node().addr_ref())),
      ),
      _ => None,
    })
    .flatten()
    .collect();
  assert_eq!(
    joined,
    vec![(7u32, addr_b)],
    "the rejoin at a new address must re-emit, carrying addr_b"
  );

  // Window 3: a plain same-address repeat is STILL suppressed — the dedup is
  // intact and only a changed address defeats it.
  let t2 = t1 + secs(5);
  c.feed(MemberEventKind::Join, vec![at(addr_b)], t2);
  let mut out3 = VecDeque::new();
  c.flush(&mut out3);
  assert!(
    out3.is_empty(),
    "a repeat Join at the unchanged address is still suppressed"
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
