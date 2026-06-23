use std::net::SocketAddr;

use buffa::Message as _;
use memberlist_proto::Node;
use smol_str::SmolStr;

use super::serf::v1::{
  ConflictResponseMessage as PbConflictResponseMessage,
  Coordinate as PbCoordinate,
  Filter as PbFilter,
  JoinMessage as PbJoinMessage,
  LeaveMessage as PbLeaveMessage,
  QueryMessage as PbQueryMessage,
  QueryResponseMessage as PbQueryResponseMessage,
  Tags as PbTags,
  UserEventMessage as PbUserEventMessage,
};
use crate::{
  ConflictResponseMessage,
  Coordinate,
  Filter,
  JoinMessage,
  LamportTime,
  LeaveMessage,
  QueryFlag,
  QueryMessage,
  QueryResponseMessage,
  TagFilter,
  Tags,
  UserEventMessage,
  conflict_response_from_pb,
  conflict_response_to_pb,
  coordinate_from_pb,
  coordinate_to_pb,
  filter_from_pb,
  filter_to_pb,
  join_from_pb,
  join_to_pb,
  leave_from_pb,
  leave_to_pb,
  query_from_pb,
  query_response_from_pb,
  query_response_to_pb,
  query_to_pb,
  tags_from_pb,
  tags_to_pb,
  user_event_from_pb,
  user_event_to_pb,
};

// ── UserEventMessage ─────────────────────────────────────────────────────────

#[test]
fn user_event_message_roundtrip_pb() {
  let typed = UserEventMessage {
    ltime: LamportTime::new(42),
    cc: true,
    name: SmolStr::from("deploy"),
    payload: bytes::Bytes::from_static(b"hello-serf"),
  };

  // typed → pb → bytes → pb → typed
  let pb = user_event_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbUserEventMessage::decode_from_slice(encoded.as_slice())
    .expect("decode_from_slice failed");
  let roundtripped = user_event_from_pb(&decoded_pb).expect("user_event_from_pb failed");

  assert_eq!(roundtripped, typed);
}

#[test]
fn user_event_message_ltime_required() {
  // A pb message with no ltime must be rejected by user_event_from_pb.
  let pb = PbUserEventMessage {
    ltime: None,
    cc: false,
    name: String::from("x"),
    payload: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    user_event_from_pb(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

// ── QueryFlag ────────────────────────────────────────────────────────────────

#[test]
fn query_flag_bits_roundtrip() {
  // ACK and NO_BROADCAST round-trip through u32.
  let flags = QueryFlag::ACK | QueryFlag::NO_BROADCAST;
  let bits: u32 = flags.bits();
  let recovered = QueryFlag::from_bits_truncate(bits);
  assert_eq!(recovered, flags);
}

#[test]
fn query_flag_individual_bits() {
  assert_eq!(QueryFlag::ACK.bits(), 1u32);
  assert_eq!(QueryFlag::NO_BROADCAST.bits(), 2u32);
}

#[test]
fn query_flag_empty_roundtrip() {
  let flags = QueryFlag::empty();
  let recovered = QueryFlag::from_bits_truncate(flags.bits());
  assert_eq!(recovered, flags);
}

// ── Coordinate ───────────────────────────────────────────────────────────────

#[test]
fn coordinate_roundtrip_pb() {
  let typed = Coordinate {
    vec: vec![1.0, -2.5, 3.14],
    error: 0.25,
    adjustment: -0.001,
    height: 0.000_010,
  };

  let pb = coordinate_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbCoordinate::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = coordinate_from_pb(&decoded_pb);

  assert_eq!(roundtripped.vec, typed.vec);
  assert_eq!(roundtripped.error, typed.error);
  assert_eq!(roundtripped.adjustment, typed.adjustment);
  assert_eq!(roundtripped.height, typed.height);
}

#[test]
fn coordinate_empty_vec_roundtrip() {
  let typed = Coordinate {
    vec: Vec::new(),
    error: 1.5,
    adjustment: 0.0,
    height: 0.00001,
  };
  let pb = coordinate_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbCoordinate::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = coordinate_from_pb(&decoded_pb);
  assert_eq!(roundtripped.vec, typed.vec);
  assert_eq!(roundtripped.error, typed.error);
}

// ── Tags ─────────────────────────────────────────────────────────────────────

#[test]
fn tags_empty_roundtrip() {
  let typed = Tags::new();
  let pb = tags_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbTags::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = tags_from_pb(&decoded_pb);
  assert_eq!(roundtripped, typed);
}

#[test]
fn tags_multi_entry_roundtrip() {
  let typed: Tags = [("role", "web"), ("env", "prod"), ("dc", "us-east")].into_iter().collect();

  let pb = tags_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbTags::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = tags_from_pb(&decoded_pb);

  assert_eq!(roundtripped.len(), typed.len());
  for (k, v) in &typed.0 {
    assert_eq!(roundtripped.0.get(k), Some(v));
  }
}

// ── Filter ───────────────────────────────────────────────────────────────────

#[test]
fn filter_node_ids_roundtrip() {
  // I = SmolStr (the default generic parameter).
  let typed: Filter<SmolStr> =
    Filter::Id(vec![SmolStr::from("node-1"), SmolStr::from("node-2")]);

  let pb = filter_to_pb(&typed).expect("filter_to_pb");
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped: Filter<SmolStr> = filter_from_pb(&decoded_pb).expect("filter_from_pb");

  assert_eq!(roundtripped, typed);
}

#[test]
fn filter_tag_with_expr_roundtrip() {
  let typed: Filter<SmolStr> = Filter::Tag(TagFilter {
    tag: SmolStr::from("role"),
    expr: Some(SmolStr::from("^web.*")),
  });

  let pb = filter_to_pb(&typed).expect("filter_to_pb");
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped: Filter<SmolStr> = filter_from_pb(&decoded_pb).expect("filter_from_pb");

  assert_eq!(roundtripped, typed);
}

#[test]
fn filter_tag_without_expr_roundtrip() {
  let typed: Filter<SmolStr> = Filter::Tag(TagFilter {
    tag: SmolStr::from("dc"),
    expr: None,
  });

  let pb = filter_to_pb(&typed).expect("filter_to_pb");
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped: Filter<SmolStr> = filter_from_pb(&decoded_pb).expect("filter_from_pb");

  assert_eq!(roundtripped, typed);
}

#[test]
fn filter_missing_kind_is_error() {
  // A Filter pb message with no kind set must be rejected.
  let pb = PbFilter {
    kind: None,
    ..Default::default()
  };
  assert!(
    filter_from_pb::<SmolStr>(&pb).is_err(),
    "expected BridgeError::UnknownVariant for missing kind"
  );
}

// ── JoinMessage ──────────────────────────────────────────────────────────────

type I = SmolStr;
type A = SocketAddr;

fn sample_addr() -> A {
  "127.0.0.1:7946".parse().unwrap()
}

#[test]
fn join_message_roundtrip_pb() {
  let typed: JoinMessage<I> = JoinMessage {
    ltime: LamportTime::new(99),
    id: SmolStr::from("node-join"),
  };

  let pb = join_to_pb(&typed).expect("join_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbJoinMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: JoinMessage<I> = join_from_pb(&decoded_pb).expect("join_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
}

#[test]
fn join_message_ltime_required() {
  let pb = PbJoinMessage {
    ltime: None,
    id: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    join_from_pb::<I>(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

// ── LeaveMessage ─────────────────────────────────────────────────────────────

#[test]
fn leave_message_roundtrip_pb_no_prune() {
  let typed: LeaveMessage<I> = LeaveMessage {
    ltime: LamportTime::new(7),
    id: SmolStr::from("node-leave"),
    prune: false,
  };

  let pb = leave_to_pb(&typed).expect("leave_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbLeaveMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: LeaveMessage<I> = leave_from_pb(&decoded_pb).expect("leave_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert_eq!(roundtripped.prune, false);
}

#[test]
fn leave_message_roundtrip_pb_with_prune() {
  let typed: LeaveMessage<I> = LeaveMessage {
    ltime: LamportTime::new(42),
    id: SmolStr::from("node-prune"),
    prune: true,
  };

  let pb = leave_to_pb(&typed).expect("leave_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbLeaveMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: LeaveMessage<I> = leave_from_pb(&decoded_pb).expect("leave_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert_eq!(roundtripped.prune, true);
}

#[test]
fn leave_message_ltime_required() {
  let pb = PbLeaveMessage {
    ltime: None,
    prune: false,
    id: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    leave_from_pb::<I>(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

// ── ConflictResponseMessage ───────────────────────────────────────────────────

#[test]
fn conflict_response_roundtrip_pb() {
  let node: Node<I, A> = Node::new(SmolStr::from("node-conflict"), sample_addr());
  let typed: ConflictResponseMessage<I, A> = ConflictResponseMessage::new(node);

  let pb = conflict_response_to_pb(&typed).expect("conflict_response_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbConflictResponseMessage::decode_from_slice(encoded.as_slice())
    .expect("decode_from_slice failed");
  let roundtripped: ConflictResponseMessage<I, A> =
    conflict_response_from_pb(&decoded_pb).expect("conflict_response_from_pb failed");

  assert_eq!(roundtripped.member.id_ref(), typed.member.id_ref());
  assert_eq!(roundtripped.member.addr_ref(), typed.member.addr_ref());
}

// ── QueryMessage ──────────────────────────────────────────────────────────────

#[test]
fn query_message_roundtrip_pb_no_filters() {
  let typed: QueryMessage<I, A> = QueryMessage {
    ltime: LamportTime::new(5),
    id: 1234,
    from: Node::new(SmolStr::from("node-q"), sample_addr()),
    filters: vec![],
    flags: QueryFlag::ACK,
    relay_factor: 3,
    timeout: std::time::Duration::from_millis(500),
    name: smol_str::SmolStr::from("my-query"),
    payload: bytes::Bytes::from_static(b"query-payload"),
  };

  let pb = query_to_pb(&typed).expect("query_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbQueryMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: QueryMessage<I, A> = query_from_pb(&decoded_pb).expect("query_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert_eq!(roundtripped.from.id_ref(), typed.from.id_ref());
  assert_eq!(roundtripped.from.addr_ref(), typed.from.addr_ref());
  assert_eq!(roundtripped.filters.len(), 0);
  assert_eq!(roundtripped.flags, typed.flags);
  assert_eq!(roundtripped.relay_factor, typed.relay_factor);
  assert_eq!(roundtripped.timeout, typed.timeout);
  assert_eq!(roundtripped.name, typed.name);
  assert_eq!(roundtripped.payload, typed.payload);
}

#[test]
fn query_message_roundtrip_pb_with_filters() {
  let typed: QueryMessage<I, A> = QueryMessage {
    ltime: LamportTime::new(10),
    id: 9999,
    from: Node::new(SmolStr::from("node-q2"), sample_addr()),
    filters: vec![
      Filter::Id(vec![SmolStr::from("target-1"), SmolStr::from("target-2")]),
      Filter::Tag(TagFilter {
        tag: SmolStr::from("role"),
        expr: Some(SmolStr::from("^db.*")),
      }),
    ],
    flags: QueryFlag::ACK | QueryFlag::NO_BROADCAST,
    relay_factor: 0,
    timeout: std::time::Duration::from_secs(2),
    name: smol_str::SmolStr::from("filtered-query"),
    payload: bytes::Bytes::new(),
  };

  let pb = query_to_pb(&typed).expect("query_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbQueryMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: QueryMessage<I, A> = query_from_pb(&decoded_pb).expect("query_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert_eq!(roundtripped.flags, typed.flags);
  assert_eq!(roundtripped.timeout, typed.timeout);
  assert_eq!(roundtripped.name, typed.name);
  assert_eq!(roundtripped.filters.len(), 2);

  // Check Id filter
  match &roundtripped.filters[0] {
    Filter::Id(ids) => {
      assert_eq!(ids.len(), 2);
      assert_eq!(ids[0], SmolStr::from("target-1"));
      assert_eq!(ids[1], SmolStr::from("target-2"));
    }
    other => panic!("expected Filter::Id, got {:?}", other),
  }

  // Check Tag filter
  match &roundtripped.filters[1] {
    Filter::Tag(tf) => {
      assert_eq!(tf.tag, SmolStr::from("role"));
      assert_eq!(tf.expr, Some(SmolStr::from("^db.*")));
    }
    other => panic!("expected Filter::Tag, got {:?}", other),
  }
}

#[test]
fn query_message_ltime_required() {
  let pb = PbQueryMessage {
    ltime: None,
    id: Some(1),
    from: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

#[test]
fn query_message_id_required() {
  let pb = PbQueryMessage {
    ltime: Some(1),
    id: None,
    from: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent id"
  );
}

// ── QueryResponseMessage ──────────────────────────────────────────────────────

#[test]
fn query_response_message_roundtrip_pb_ack() {
  let typed: QueryResponseMessage<I, A> = QueryResponseMessage {
    ltime: LamportTime::new(3),
    id: 42,
    from: Node::new(SmolStr::from("node-resp"), sample_addr()),
    flags: QueryFlag::ACK,
    payload: bytes::Bytes::new(),
  };

  let pb = query_response_to_pb(&typed).expect("query_response_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbQueryResponseMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: QueryResponseMessage<I, A> =
    query_response_from_pb(&decoded_pb).expect("query_response_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert_eq!(roundtripped.from.id_ref(), typed.from.id_ref());
  assert_eq!(roundtripped.from.addr_ref(), typed.from.addr_ref());
  assert_eq!(roundtripped.flags, typed.flags);
  assert!(roundtripped.ack());
  assert_eq!(roundtripped.payload, typed.payload);
}

#[test]
fn query_response_message_roundtrip_pb_with_payload() {
  let typed: QueryResponseMessage<I, A> = QueryResponseMessage {
    ltime: LamportTime::new(7),
    id: 100,
    from: Node::new(SmolStr::from("node-resp2"), sample_addr()),
    flags: QueryFlag::empty(),
    payload: bytes::Bytes::from_static(b"response-data"),
  };

  let pb = query_response_to_pb(&typed).expect("query_response_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbQueryResponseMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: QueryResponseMessage<I, A> =
    query_response_from_pb(&decoded_pb).expect("query_response_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert!(!roundtripped.ack());
  assert_eq!(roundtripped.payload, bytes::Bytes::from_static(b"response-data"));
}

#[test]
fn query_response_message_ltime_required() {
  let pb = PbQueryResponseMessage {
    ltime: None,
    id: Some(1),
    from: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    query_response_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

#[test]
fn query_response_message_id_required() {
  let pb = PbQueryResponseMessage {
    ltime: Some(1),
    id: None,
    from: bytes::Bytes::new(),
    ..Default::default()
  };
  assert!(
    query_response_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent id"
  );
}

#[test]
fn query_response_message_flags_required() {
  // A QueryResponseMessage pb with flags absent must be rejected.
  let pb = PbQueryResponseMessage {
    ltime: Some(1),
    id: Some(42),
    from: bytes::Bytes::new(),
    flags: None,
    ..Default::default()
  };
  assert!(
    query_response_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent flags"
  );
}

#[test]
fn query_message_required_fields() {
  // flags absent — must be rejected.
  let pb_no_flags = PbQueryMessage {
    ltime: Some(1),
    id: Some(1),
    from: bytes::Bytes::new(),
    flags: None,
    relay_factor: Some(0),
    timeout_nanos: Some(1_000_000_000),
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb_no_flags).is_err(),
    "expected error for absent flags"
  );

  // relay_factor absent — must be rejected.
  let pb_no_relay = PbQueryMessage {
    ltime: Some(1),
    id: Some(1),
    from: bytes::Bytes::new(),
    flags: Some(0),
    relay_factor: None,
    timeout_nanos: Some(1_000_000_000),
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb_no_relay).is_err(),
    "expected error for absent relay_factor"
  );

  // timeout_nanos absent — must be rejected.
  let pb_no_timeout = PbQueryMessage {
    ltime: Some(1),
    id: Some(1),
    from: bytes::Bytes::new(),
    flags: Some(0),
    relay_factor: Some(0),
    timeout_nanos: None,
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb_no_timeout).is_err(),
    "expected error for absent timeout_nanos"
  );
}
