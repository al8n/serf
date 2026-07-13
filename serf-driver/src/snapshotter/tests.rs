use std::net::SocketAddr;

use memberlist_proto::Node;
use serf_proto::snapshot::ReplayResult;
use smol_str::SmolStr;

use super::*;

/// A fresh snapshot path plus the compaction threshold under test.
struct TestSnapshot {
  path: PathBuf,
  compact_threshold: u64,
}

impl TestSnapshot {
  fn path(&self) -> &std::path::Path {
    &self.path
  }

  fn with_compact_threshold(mut self, threshold: u64) -> Self {
    self.compact_threshold = threshold;
    self
  }

  fn open<I>(&self) -> Result<OpenedSnapshot<I>, SnapshotOpenError>
  where
    I: memberlist_proto::Data + Clone + Eq + core::hash::Hash,
  {
    Snapshotter::open(&self.path, self.compact_threshold)
  }
}

fn opts(name: &str) -> TestSnapshot {
  let mut p = std::env::temp_dir();
  p.push(format!("serf-snapshotter-{name}-{}", std::process::id()));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = fs::remove_file(&p);
  TestSnapshot {
    path: p,
    compact_threshold: crate::DEFAULT_SNAPSHOT_COMPACT_THRESHOLD,
  }
}

fn node(id: &str, port: u16) -> Node<SmolStr, SocketAddr> {
  Node::new(
    SmolStr::new(id),
    format!("127.0.0.1:{port}").parse().unwrap(),
  )
}

fn cleanup(o: &TestSnapshot) {
  // Ignoring Err: best-effort test-file cleanup.
  let _ = fs::remove_file(o.path());
}

/// Appended membership and clock records replay across a reopen: the alive
/// set reflects joins minus removals, and the clock floors are the high-water
/// marks.
#[test]
fn appends_replay_across_reopen() {
  let o = opts("roundtrip");
  {
    let (mut snap, records) = o.open::<SmolStr>().expect("first open of a fresh path");
    assert!(records.is_empty(), "a fresh path replays to nothing");
    snap.append_member(true, &node("a", 7001));
    snap.append_member(true, &node("b", 7002));
    snap.append_clocks(
      LamportTime::new(5),
      LamportTime::new(3),
      LamportTime::new(2),
    );
    snap.append_member(false, &node("b", 7002));
    snap.flush_and_maybe_compact(Vec::new);
  }

  let (_snap, records) = o.open::<SmolStr>().expect("reopen parses");
  let replay = ReplayResult::replay(records, false);
  assert_eq!(replay.alive_nodes, vec![node("a", 7001)]);
  assert_eq!(replay.last_clock, LamportTime::new(5));
  assert_eq!(replay.last_event_clock, LamportTime::new(3));
  assert_eq!(replay.last_query_clock, LamportTime::new(2));
  cleanup(&o);
}

/// The clean-leave marker clears the recovered state on replay unless
/// `rejoin_after_leave` ignores it.
#[test]
fn leave_marker_gates_the_replay() {
  let o = opts("leave-gate");
  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("a", 7001));
    snap.append_clocks(LamportTime::new(9), LamportTime::ZERO, LamportTime::ZERO);
    snap.append_leave();
    snap.flush_and_maybe_compact(Vec::new);
  }

  let (_s, records) = o.open::<SmolStr>().expect("reopen");
  let fresh = ReplayResult::replay(records.clone(), false);
  assert!(fresh.alive_nodes.is_empty(), "a clean leave starts fresh");
  assert_eq!(fresh.last_clock, LamportTime::ZERO);

  let rejoin = ReplayResult::replay(records, true);
  assert_eq!(
    rejoin.alive_nodes,
    vec![node("a", 7001)],
    "rejoin_after_leave preserves the recovered membership"
  );
  assert_eq!(rejoin.last_clock, LamportTime::new(9));
  cleanup(&o);
}

/// A truncated tail (a crash mid-append) is tolerated: whole records replay,
/// the partial tail is dropped, and new appends land on a whole-record
/// boundary.
#[test]
fn truncated_tail_is_tolerated_and_repaired() {
  let o = opts("torn-tail");
  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("a", 7001));
    snap.flush_and_maybe_compact(Vec::new);
  }
  // Simulate a torn append: a record tag with a length that promises more
  // bytes than exist.
  {
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new()
      .append(true)
      .open(o.path())
      .expect("append to test file");
    f.write_all(&[0x00, 0xff, 0xff]).expect("write torn tail");
  }

  let (mut snap, records) = o.open::<SmolStr>().expect("torn tail tolerated");
  let replay = ReplayResult::replay(records, false);
  assert_eq!(replay.alive_nodes, vec![node("a", 7001)]);

  // Appends continue cleanly on the repaired boundary.
  snap.append_member(true, &node("b", 7002));
  snap.flush_and_maybe_compact(Vec::new);
  drop(snap);
  let (_s, records) = o.open::<SmolStr>().expect("reopen after repair");
  let replay = ReplayResult::replay(records, false);
  assert_eq!(replay.alive_nodes, vec![node("a", 7001), node("b", 7002)]);
  cleanup(&o);
}

/// A malformed record BEFORE the tail is a hard open error, never a silent
/// partial replay.
#[test]
fn corrupt_middle_record_refuses_to_open() {
  let o = opts("corrupt");
  {
    use std::io::Write as _;
    let mut f = fs::File::create(o.path()).expect("create test file");
    // An unknown tag followed by a whole valid record's worth of bytes.
    f.write_all(&[0xEE]).expect("write bogus tag");
    let rec = SnapshotRecord::<SmolStr, SocketAddr>::Comment
      .encode()
      .expect("encode comment");
    f.write_all(&rec).expect("write trailing record");
  }
  assert!(
    matches!(o.open::<SmolStr>(), Err(SnapshotOpenError::Corrupt(_))),
    "an unknown tag before the tail must refuse the open"
  );
  cleanup(&o);
}

/// Past the compaction threshold, the file is rewritten to just the clock
/// floors and the caller-supplied live set — and replays identically.
#[test]
fn compaction_rewrites_to_the_live_state() {
  let o = opts("compact").with_compact_threshold(64);
  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    // Churn well past 64 bytes: many joins and removals of a transient peer.
    for i in 0..32u16 {
      snap.append_member(true, &node("transient", 8000 + i));
      snap.append_member(false, &node("transient", 8000 + i));
    }
    snap.append_clocks(
      LamportTime::new(7),
      LamportTime::new(6),
      LamportTime::new(5),
    );
    snap.flush_and_maybe_compact(|| vec![node("kept", 7001)]);
  }
  let size = fs::metadata(o.path()).expect("stat").len();
  assert!(
    size < 128,
    "compaction must shrink the churned file, got {size} bytes"
  );
  let (_s, records) = o.open::<SmolStr>().expect("reopen compacted");
  let replay = ReplayResult::replay(records, false);
  assert_eq!(replay.alive_nodes, vec![node("kept", 7001)]);
  assert_eq!(replay.last_clock, LamportTime::new(7));
  assert_eq!(replay.last_event_clock, LamportTime::new(6));
  assert_eq!(replay.last_query_clock, LamportTime::new(5));
  cleanup(&o);
}

/// The clean-leave gate survives compaction: with a tiny threshold forcing a
/// rewrite on the very batch that carried the Leave marker, the compacted
/// file still replays to a gated fresh start under the default posture and
/// to the preserved membership under `rejoin_after_leave = true`.
#[test]
fn compaction_preserves_the_clean_leave_gate() {
  let o = opts("compact-leave").with_compact_threshold(1);
  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("peer", 7001));
    snap.append_clocks(LamportTime::new(4), LamportTime::ZERO, LamportTime::ZERO);
    snap.append_leave();
    // Threshold 1: this flush compacts, rewriting the file.
    snap.flush_and_maybe_compact(|| vec![node("peer", 7001)]);
  }

  let (_s, records) = o.open::<SmolStr>().expect("reopen compacted");
  let fresh = ReplayResult::replay(records.clone(), false);
  assert!(
    fresh.alive_nodes.is_empty(),
    "the compacted file must still gate a clean leave on the default posture"
  );
  assert_eq!(fresh.last_clock, LamportTime::ZERO);

  let rejoin = ReplayResult::replay(records, true);
  assert_eq!(
    rejoin.alive_nodes,
    vec![node("peer", 7001)],
    "the opt-in posture must still recover the pre-leave membership"
  );
  assert_eq!(rejoin.last_clock, LamportTime::new(4));
  cleanup(&o);
}

/// A clean-leave tail replays identically before and after compaction, under
/// both rejoin postures. The pumps append the clocks BEFORE the leave marker
/// — the compacted terminal shape — so compaction can never change what a
/// restart recovers: were the order reversed, the original file would replay
/// clock floors the no-rejoin posture is supposed to zero, while its
/// compacted replacement zeroed them.
#[test]
fn leave_tail_replays_identically_across_compaction() {
  // Identical production-ordered appends (member, clocks, leave — the
  // pumps' account_event order); only the threshold differs, so one flush
  // compacts and the other keeps the original records.
  let plain = opts("leave-order-plain");
  let compacted = opts("leave-order-compacted").with_compact_threshold(1);
  for o in [&plain, &compacted] {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("peer", 7001));
    snap.append_clocks(
      LamportTime::new(8),
      LamportTime::new(2),
      LamportTime::new(1),
    );
    snap.append_leave();
    snap.flush_and_maybe_compact(|| vec![node("peer", 7001)]);
  }

  let (_p, plain_records) = plain.open::<SmolStr>().expect("reopen the original");
  let (_c, compacted_records) = compacted.open::<SmolStr>().expect("reopen the compacted");
  for rejoin in [false, true] {
    let original = ReplayResult::replay(plain_records.clone(), rejoin);
    let rewritten = ReplayResult::replay(compacted_records.clone(), rejoin);
    assert_eq!(
      original.alive_nodes, rewritten.alive_nodes,
      "membership must replay identically across compaction (rejoin: {rejoin})"
    );
    assert_eq!(
      (
        original.last_clock,
        original.last_event_clock,
        original.last_query_clock
      ),
      (
        rewritten.last_clock,
        rewritten.last_event_clock,
        rewritten.last_query_clock
      ),
      "clock floors must replay identically across compaction (rejoin: {rejoin})"
    );
  }
  // And both match the reference semantics: a clean leave zeroes the clocks
  // and empties the membership unless the rejoin posture ignores it.
  let fresh = ReplayResult::replay(plain_records, false);
  assert!(fresh.alive_nodes.is_empty());
  assert_eq!(fresh.last_clock, LamportTime::ZERO);
  let rejoined = ReplayResult::replay(compacted_records, true);
  assert_eq!(rejoined.alive_nodes, vec![node("peer", 7001)]);
  assert_eq!(rejoined.last_clock, LamportTime::new(8));
  cleanup(&plain);
  cleanup(&compacted);
}

/// A clock record trailing the leave marker does not mask the clean leave: the
/// clean-left state a reopen re-derives is set by the last MEMBERSHIP record,
/// looking past the clock high-water marks the pump appends after it. Were a
/// trailing clock read as membership activity, the next compaction would drop
/// the marker and a restart would silently rejoin a cluster the operator left.
#[test]
fn a_trailing_clock_record_does_not_mask_the_clean_leave() {
  let o = opts("clock-after-leave");
  {
    // The default threshold: this flush leaves the original records on disk.
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("peer", 7001));
    snap.append_leave();
    // A clock high-water mark advancing after the leave marker.
    snap.append_clocks(LamportTime::new(4), LamportTime::ZERO, LamportTime::ZERO);
    snap.flush_and_maybe_compact(Vec::new);
  }

  // Reopening re-derives the clean-left state from the records on disk.
  let (mut snap, records) =
    Snapshotter::<SmolStr>::open(o.path(), 1).expect("reopen with a compacting threshold");
  assert!(
    matches!(records.last(), Some(SnapshotRecord::Clock(_))),
    "the seeded file must end with the clock record"
  );
  // Threshold 1: this flush compacts, and the rewrite must re-emit the marker.
  snap.flush_and_maybe_compact(|| vec![node("peer", 7001)]);
  drop(snap);

  let (_s, records) = o.open::<SmolStr>().expect("reopen compacted");
  let fresh = ReplayResult::replay(records.clone(), false);
  assert!(
    fresh.alive_nodes.is_empty(),
    "the compacted file must still gate the clean leave on the default posture"
  );
  let rejoin = ReplayResult::replay(records, true);
  assert_eq!(
    rejoin.alive_nodes,
    vec![node("peer", 7001)],
    "the opt-in posture must still recover the pre-leave membership"
  );
  cleanup(&o);
}

/// A snapshot path that cannot be read at all fails the open loudly: a
/// directory standing in the file's place is an I/O error, never an empty
/// replay that would silently discard the recovered membership.
#[test]
fn an_unreadable_snapshot_path_is_an_io_error() {
  let o = opts("unreadable");
  // Ignoring Err: a leftover directory from a previous run is fine to reuse.
  let _ = fs::create_dir(o.path());
  assert!(
    matches!(o.open::<SmolStr>(), Err(SnapshotOpenError::Io(_))),
    "an unreadable snapshot path must not open as an empty replay"
  );
  // Ignoring Err: best-effort test-tree cleanup.
  let _ = fs::remove_dir_all(o.path());
}

/// A compaction that cannot write its replacement leaves the GROWN file
/// authoritative: the records already appended survive untouched, so a failed
/// rewrite can never lose the membership the wire is carrying.
#[test]
fn a_failed_compaction_keeps_the_grown_file_authoritative() {
  let o = opts("compact-blocked").with_compact_threshold(1);
  // Occupy the compaction's temp path with a directory, so its create fails.
  let blocked = o.path().with_extension("compact");
  // Ignoring Err: a leftover from a previous run is fine to reuse.
  let _ = fs::remove_dir_all(&blocked);
  fs::create_dir(&blocked).expect("block the compaction temp path");

  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_member(true, &node("peer", 7001));
    snap.append_clocks(LamportTime::new(6), LamportTime::ZERO, LamportTime::ZERO);
    // A compaction that COMPLETED here would rewrite the file to this empty
    // live set and drop the appended member; the blocked one must not.
    snap.flush_and_maybe_compact(Vec::new);
  }

  let (_s, records) = o
    .open::<SmolStr>()
    .expect("reopen after the failed compaction");
  let replay = ReplayResult::replay(records, false);
  assert_eq!(
    replay.alive_nodes,
    vec![node("peer", 7001)],
    "a failed compaction must leave the grown file's records intact"
  );
  assert_eq!(replay.last_clock, LamportTime::new(6));

  // Ignoring Err: best-effort test-tree cleanup.
  let _ = fs::remove_dir_all(&blocked);
  cleanup(&o);
}

/// Membership activity after a leave clears the clean-left state: the next
/// compaction does not re-emit a stale Leave marker over live members.
#[test]
fn membership_after_a_leave_clears_the_compacted_gate() {
  let o = opts("compact-rejoined").with_compact_threshold(1);
  {
    let (mut snap, _) = o.open::<SmolStr>().expect("open");
    snap.append_leave();
    snap.append_member(true, &node("peer", 7001));
    snap.flush_and_maybe_compact(|| vec![node("peer", 7001)]);
  }
  let (_s, records) = o.open::<SmolStr>().expect("reopen");
  let fresh = ReplayResult::replay(records, false);
  assert_eq!(
    fresh.alive_nodes,
    vec![node("peer", 7001)],
    "post-leave membership must survive the default-posture replay"
  );
  cleanup(&o);
}
