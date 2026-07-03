use core::net::SocketAddr;

use memberlist_proto::Node;

use super::*;

fn make_records_fwd() -> Vec<SnapshotRecord<u32, SocketAddr>> {
  vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::Alive(test_node(3, 1003)),
    SnapshotRecord::Clock(LamportTime::new(10)),
    SnapshotRecord::EventClock(LamportTime::new(20)),
    SnapshotRecord::QueryClock(LamportTime::new(30)),
  ]
}

fn make_records_rev() -> Vec<SnapshotRecord<u32, SocketAddr>> {
  vec![
    SnapshotRecord::Alive(test_node(3, 1003)),
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Clock(LamportTime::new(10)),
    SnapshotRecord::EventClock(LamportTime::new(20)),
    SnapshotRecord::QueryClock(LamportTime::new(30)),
  ]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn test_node(id: u32, port: u16) -> Node<u32, SocketAddr> {
  Node::new(id, format!("127.0.0.1:{port}").parse().unwrap())
}

// ── Round-trip tests ──────────────────────────────────────────────────────────

#[test]
fn clock_record_round_trips() {
  let r = SnapshotRecord::<u32, SocketAddr>::Clock(42.into());
  let bytes = r.encode().unwrap();
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert_eq!(back, r);
  assert_eq!(n, bytes.len(), "bytes_consumed == total encoded length");
}

#[test]
fn event_clock_record_round_trips() {
  let r = SnapshotRecord::<u32, SocketAddr>::EventClock(LamportTime::new(u64::MAX));
  let bytes = r.encode().unwrap();
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert_eq!(back, r);
  assert_eq!(n, bytes.len());
}

#[test]
fn query_clock_record_round_trips() {
  let r = SnapshotRecord::<u32, SocketAddr>::QueryClock(0.into());
  let bytes = r.encode().unwrap();
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert_eq!(back, r);
  assert_eq!(n, bytes.len());
}

#[test]
fn alive_record_round_trips() {
  let node = test_node(7, 7000);
  let r = SnapshotRecord::Alive(node);
  let bytes = r.encode().unwrap();
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert_eq!(n, bytes.len());
  match back {
    SnapshotRecord::Alive(ref got) => assert_eq!(*got, node),
    other => panic!("expected Alive, got {other:?}"),
  }
}

#[test]
fn not_alive_record_round_trips() {
  let node = test_node(99, 9999);
  let r = SnapshotRecord::NotAlive(node);
  let bytes = r.encode().unwrap();
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert_eq!(n, bytes.len());
  match back {
    SnapshotRecord::NotAlive(ref got) => assert_eq!(*got, node),
    other => panic!("expected NotAlive, got {other:?}"),
  }
}

#[test]
fn leave_record_is_single_byte() {
  let r = SnapshotRecord::<u32, SocketAddr>::Leave;
  let bytes = r.encode().unwrap();
  assert_eq!(bytes.as_ref(), &[6u8]);
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert!(matches!(back, SnapshotRecord::Leave));
  assert_eq!(n, 1);
}

#[test]
fn comment_record_is_single_byte() {
  let r = SnapshotRecord::<u32, SocketAddr>::Comment;
  let bytes = r.encode().unwrap();
  assert_eq!(bytes.as_ref(), &[7u8]);
  let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
  assert!(matches!(back, SnapshotRecord::Comment));
  assert_eq!(n, 1);
}

// ── bytes_consumed accuracy ───────────────────────────────────────────────────

/// Multiple records concatenated: bytes_consumed lets the reader advance correctly.
#[test]
fn concatenated_records_advance_cursor_correctly() {
  let r1 = SnapshotRecord::<u32, SocketAddr>::Clock(10.into());
  let r2 = SnapshotRecord::<u32, SocketAddr>::Alive(test_node(3, 3000));
  let r3 = SnapshotRecord::<u32, SocketAddr>::Leave;

  let mut stream = Vec::new();
  stream.extend_from_slice(&r1.encode().unwrap());
  stream.extend_from_slice(&r2.encode().unwrap());
  stream.extend_from_slice(&r3.encode().unwrap());

  let (got1, n1) = SnapshotRecord::<u32, SocketAddr>::decode(&stream).unwrap();
  assert!(matches!(got1, SnapshotRecord::Clock(t) if t == LamportTime::new(10)));
  let (got2, n2) = SnapshotRecord::<u32, SocketAddr>::decode(&stream[n1..]).unwrap();
  assert!(matches!(got2, SnapshotRecord::Alive(_)));
  let (got3, n3) = SnapshotRecord::<u32, SocketAddr>::decode(&stream[n1 + n2..]).unwrap();
  assert!(matches!(got3, SnapshotRecord::Leave));
  assert_eq!(n1 + n2 + n3, stream.len());
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn empty_buffer_is_truncated() {
  let r = SnapshotRecord::<u32, SocketAddr>::decode(&[]);
  assert!(matches!(r, Err(SnapshotError::Truncated { .. })));
}

#[test]
fn clock_record_truncated_body_is_error() {
  // Only 4 bytes of the 8-byte clock body.
  let buf = [TAG_CLOCK, 1, 2, 3, 4];
  let r = SnapshotRecord::<u32, SocketAddr>::decode(&buf[..]);
  assert!(
    matches!(r, Err(SnapshotError::Truncated { .. })),
    "truncated clock body must be Truncated"
  );
}

#[test]
fn alive_record_truncated_node_len_is_error() {
  // Only 2 bytes of the 4-byte length prefix.
  let buf = [TAG_ALIVE, 0, 0];
  let r = SnapshotRecord::<u32, SocketAddr>::decode(&buf[..]);
  assert!(matches!(r, Err(SnapshotError::Truncated { .. })));
}

#[test]
fn alive_record_truncated_node_body_is_error() {
  // Length prefix says 100 bytes but buffer has none.
  let buf = [TAG_ALIVE, 100, 0, 0, 0];
  let r = SnapshotRecord::<u32, SocketAddr>::decode(&buf[..]);
  assert!(matches!(r, Err(SnapshotError::Truncated { .. })));
}

#[test]
fn unknown_tag_is_rejected() {
  let buf = [0xFFu8];
  let r = SnapshotRecord::<u32, SocketAddr>::decode(&buf[..]);
  assert!(matches!(r, Err(SnapshotError::UnknownTag(0xFF))));
}

// ── ReplayResult tests ────────────────────────────────────────────────────────

#[test]
fn replay_empty_stream_gives_empty_result() {
  let r = ReplayResult::<u32, SocketAddr>::replay(vec![], false);
  assert!(r.alive_nodes.is_empty());
  assert_eq!(u64::from(r.last_clock), 0);
  assert_eq!(u64::from(r.last_event_clock), 0);
  assert_eq!(u64::from(r.last_query_clock), 0);
}

#[test]
fn replay_alive_records_accumulate() {
  let recs = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Alive(test_node(2, 1002)),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  assert_eq!(r.alive_nodes.len(), 2);
}

#[test]
fn replay_not_alive_removes_node() {
  let n1 = test_node(1, 1001);
  let recs = vec![
    SnapshotRecord::Alive(n1),
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::NotAlive(n1),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  // Node 1 was removed; only node 2 remains.
  assert_eq!(r.alive_nodes.len(), 1, "NotAlive must remove the node");
  assert!(
    r.alive_nodes.iter().any(|n| *n.id_ref() == 2u32),
    "node 2 must still be alive"
  );
}

#[test]
fn replay_clocks_take_last_value() {
  let recs = vec![
    SnapshotRecord::<u32, SocketAddr>::Clock(3.into()),
    SnapshotRecord::Clock(7.into()),
    SnapshotRecord::EventClock(11.into()),
    SnapshotRecord::QueryClock(5.into()),
    SnapshotRecord::QueryClock(9.into()),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  assert_eq!(u64::from(r.last_clock), 7);
  assert_eq!(u64::from(r.last_event_clock), 11);
  assert_eq!(u64::from(r.last_query_clock), 9);
}

#[test]
fn replay_leave_without_rejoin_clears_state() {
  let recs = vec![
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::Clock(5.into()),
    SnapshotRecord::Leave,
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  assert!(
    r.alive_nodes.is_empty(),
    "Leave (no rejoin) must clear alive set"
  );
  assert_eq!(
    u64::from(r.last_clock),
    0,
    "Leave (no rejoin) must reset clock"
  );
  assert_eq!(u64::from(r.last_event_clock), 0);
  assert_eq!(u64::from(r.last_query_clock), 0);
}

#[test]
fn replay_leave_with_rejoin_keeps_state() {
  let recs = vec![
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::Clock(5.into()),
    SnapshotRecord::Leave,
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, true);
  assert_eq!(
    r.alive_nodes.len(),
    1,
    "Leave with rejoin must keep alive set"
  );
  assert_eq!(
    u64::from(r.last_clock),
    5,
    "Leave with rejoin must keep clock"
  );
}

#[test]
fn replay_leave_then_alive_after_rejoin_accumulates() {
  // Leave (ignore with rejoin=true), then more Alive records after.
  let recs = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Leave,
    SnapshotRecord::Alive(test_node(2, 1002)),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, true);
  // Both nodes are alive: Leave was ignored.
  assert_eq!(r.alive_nodes.len(), 2);
}

#[test]
fn replay_leave_then_alive_without_rejoin_clears_then_accumulates() {
  // Leave (clear with rejoin=false), then more Alive records after.
  let recs = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Leave,
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::Clock(8.into()),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  // Only node 2 (alive after leave); node 1 was cleared.
  assert_eq!(r.alive_nodes.len(), 1);
  assert_eq!(r.alive_nodes[0], test_node(2, 1002));
  assert_eq!(u64::from(r.last_clock), 8);
}

#[test]
fn replay_comment_is_ignored() {
  let recs = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::<u32, SocketAddr>::Comment,
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  assert_eq!(r.alive_nodes.len(), 1);
}

#[cfg(feature = "coordinates")]
#[test]
fn replay_coordinate_record_is_ignored() {
  use crate::Coordinate;
  let coord = Coordinate {
    vec: vec![1.0; 8],
    error: 0.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let recs = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Coordinate(super::CoordinateRecord::new(test_node(1, 1001), coord)),
    SnapshotRecord::<u32, SocketAddr>::Clock(3.into()),
  ];
  let r = ReplayResult::<u32, SocketAddr>::replay(recs, false);
  // Coordinate is ignored; alive set and clock are unaffected.
  assert_eq!(r.alive_nodes.len(), 1);
  assert_eq!(u64::from(r.last_clock), 3);
}

// ── Tag constants ─────────────────────────────────────────────────────────────

#[test]
fn discriminant_bytes_match_oracle() {
  // Verify that each encoded record starts with the correct tag as per the
  // serf-core snapshot.rs layout (Alive=0, NotAlive=1, Clock=2, EventClock=3,
  // QueryClock=4, Leave=6, Comment=7).
  let node = test_node(1, 1234);

  let cases: &[(u8, SnapshotRecord<u32, SocketAddr>)] = &[
    (0, SnapshotRecord::Alive(node)),
    (1, SnapshotRecord::NotAlive(node)),
    (2, SnapshotRecord::Clock(1.into())),
    (3, SnapshotRecord::EventClock(1.into())),
    (4, SnapshotRecord::QueryClock(1.into())),
    (6, SnapshotRecord::Leave),
    (7, SnapshotRecord::Comment),
  ];
  for (expected_tag, rec) in cases {
    let bytes = rec.encode().unwrap();
    assert_eq!(
      bytes[0], *expected_tag,
      "record {rec:?} must encode with tag {expected_tag}"
    );
  }
}

// ── Coordinate feature tests ──────────────────────────────────────────────────

#[cfg(feature = "coordinates")]
mod coordinates {
  use super::*;
  use crate::Coordinate;

  fn test_coord() -> Coordinate {
    Coordinate {
      vec: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
      error: 0.5,
      adjustment: 0.1,
      height: 0.01,
    }
  }

  #[test]
  fn coordinate_record_round_trips() {
    // Node<u32, SocketAddr> is Copy so we can use it freely after passing into the record.
    let node = test_node(42, 4242);
    let coord = test_coord();
    let rec = CoordinateRecord::new(node, coord.clone());
    let r = SnapshotRecord::Coordinate(rec);
    let bytes = r.encode().unwrap();
    // First byte must be the coordinate tag (5).
    assert_eq!(bytes[0], 5u8, "Coordinate tag must be 5");
    let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
    assert_eq!(n, bytes.len());
    match back {
      SnapshotRecord::Coordinate(got) => {
        assert_eq!(*got.node(), node); // node is Copy; still usable
        assert_eq!(got.coordinate().vec, coord.vec);
        assert_eq!(got.coordinate().error, coord.error);
        assert_eq!(got.coordinate().adjustment, coord.adjustment);
        assert_eq!(got.coordinate().height, coord.height);
      }
      other => panic!("expected Coordinate, got {other:?}"),
    }
  }

  #[test]
  fn coordinate_tag_is_five() {
    let node = test_node(1, 1000);
    let rec = SnapshotRecord::Coordinate(CoordinateRecord::new(node, test_coord()));
    let bytes = rec.encode().unwrap();
    assert_eq!(bytes[0], 5u8);
  }

  #[test]
  fn zero_dimensional_coordinate_round_trips() {
    let node = test_node(1, 1000);
    let coord = Coordinate {
      vec: vec![],
      error: 0.0,
      adjustment: 0.0,
      height: 0.0,
    };
    let rec = SnapshotRecord::Coordinate(CoordinateRecord::new(node, coord.clone()));
    let bytes = rec.encode().unwrap();
    let (back, n) = SnapshotRecord::<u32, SocketAddr>::decode(&bytes).unwrap();
    assert_eq!(n, bytes.len());
    match back {
      SnapshotRecord::Coordinate(got) => {
        assert_eq!(*got.node(), node);
        assert!(got.coordinate().vec.is_empty());
      }
      other => panic!("expected Coordinate, got {other:?}"),
    }
  }
}

// ── Determinism: snapshot replay dial order ────────────────────────────────────

#[test]
fn snapshot_replay_dial_order_deterministic() {
  // Replaying the same alive nodes in two different record orderings must
  // produce identical alive_nodes lists (same nodes, same sequence).
  // This confirms that snapshot replay does not depend on HashSet iteration order.
  let result_fwd = ReplayResult::<u32, SocketAddr>::replay(make_records_fwd(), false);
  let result_rev = ReplayResult::<u32, SocketAddr>::replay(make_records_rev(), false);

  // Both replays must yield the same set of alive nodes.
  assert_eq!(
    result_fwd.alive_nodes.len(),
    result_rev.alive_nodes.len(),
    "both replays must yield the same node count"
  );
  // The fwd replay preserves record order (1, 2, 3); the rev replay preserves
  // its record order (3, 2, 1).  The key property: each run is self-consistent —
  // running the same record stream twice must yield an identical Vec, not a
  // HashSet-order-dependent one.
  let result_fwd2 = ReplayResult::<u32, SocketAddr>::replay(make_records_fwd(), false);
  let result_rev2 = ReplayResult::<u32, SocketAddr>::replay(make_records_rev(), false);
  assert_eq!(
    result_fwd.alive_nodes, result_fwd2.alive_nodes,
    "same record stream must produce identical alive_nodes on every call"
  );
  assert_eq!(
    result_rev.alive_nodes, result_rev2.alive_nodes,
    "same record stream (reversed) must produce identical alive_nodes on every call"
  );
  // Verify insertion-order is preserved: fwd → [1,2,3], rev → [3,2,1].
  assert_eq!(
    result_fwd.alive_nodes,
    vec![test_node(1, 1001), test_node(2, 1002), test_node(3, 1003)],
    "forward record order must yield nodes in insertion order"
  );
  assert_eq!(
    result_rev.alive_nodes,
    vec![test_node(3, 1003), test_node(2, 1002), test_node(1, 1001)],
    "reversed record order must yield nodes in insertion order"
  );
}

#[test]
fn snapshot_replay_not_alive_dedup_is_stable() {
  // Alive → NotAlive → re-Alive: the node must appear at the re-insertion position.
  // Also verifies that dedup and removal are both stable.
  let records = vec![
    SnapshotRecord::Alive(test_node(1, 1001)),
    SnapshotRecord::Alive(test_node(2, 1002)),
    SnapshotRecord::NotAlive(test_node(1, 1001)), // removes node 1
    SnapshotRecord::Alive(test_node(1, 1001)),    // re-inserts node 1 at end
  ];
  let result = ReplayResult::<u32, SocketAddr>::replay(records, false);
  // Node 2 was inserted first and never removed; node 1 was re-inserted after 2.
  assert_eq!(
    result.alive_nodes,
    vec![test_node(2, 1002), test_node(1, 1001)],
    "re-inserted node must appear at its re-insertion position"
  );
}
