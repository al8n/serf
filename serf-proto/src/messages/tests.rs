use core::net::SocketAddr;

use buffa::Message as _;
use memberlist_proto::Node;
use smol_str::SmolStr;

use super::serf::v1::{
  ConflictResponseMessage as PbConflictResponseMessage, Coordinate as PbCoordinate,
  Filter as PbFilter, JoinMessage as PbJoinMessage, LeaveMessage as PbLeaveMessage,
  PushPullMessage as PbPushPullMessage, QueryMessage as PbQueryMessage,
  QueryResponseMessage as PbQueryResponseMessage, RelayMessage as PbRelayMessage, Tags as PbTags,
  UserEvent as PbUserEvent, UserEventMessage as PbUserEventMessage, UserEvents as PbUserEvents,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use super::serf::v1::{
  KeyRequestMessage as PbKeyRequestMessage, KeyResponseMessage as PbKeyResponseMessage,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::bridge::{
  key_request_from_pb, key_request_to_pb, key_response_from_pb, key_response_to_pb,
};
use crate::{
  ConflictResponseMessage, Coordinate, Filter, JoinMessage, LamportTime, LeaveMessage,
  PushPullMessage, QueryFlag, QueryMessage, QueryResponseMessage, RelayMessage, TagFilter, Tags,
  UserEvent, UserEventMessage, UserEvents,
  bridge::{
    conflict_response_from_pb, conflict_response_to_pb, coordinate_from_pb, coordinate_to_pb,
    filter_from_pb, filter_to_pb, join_from_pb, join_to_pb, leave_from_pb, leave_to_pb,
    push_pull_from_pb, push_pull_to_pb, query_from_pb, query_response_from_pb,
    query_response_to_pb, query_to_pb, relay_from_pb, relay_to_pb, tags_from_pb, tags_to_pb,
    user_event_from_pb, user_event_single_from_pb, user_event_single_to_pb, user_event_to_pb,
    user_events_from_pb, user_events_to_pb,
  },
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::{KeyRequestMessage, KeyResponseMessage};
#[cfg(feature = "aes-gcm")]
use memberlist_proto::SecretKey;

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
  let decoded_pb =
    PbUserEventMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
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
  let typed: Tags = [("role", "web"), ("env", "prod"), ("dc", "us-east")]
    .into_iter()
    .collect();

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
  let typed: Filter<SmolStr> = Filter::Id(vec![SmolStr::from("node-1"), SmolStr::from("node-2")]);

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
    id: Some(bytes::Bytes::new()),
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
    id: Some(bytes::Bytes::new()),
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
    timeout: core::time::Duration::from_millis(500),
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
    timeout: core::time::Duration::from_secs(2),
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
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
    ..Default::default()
  };
  assert!(
    query_from_pb::<I, A>(&pb).is_err(),
    "expected BridgeError::MissingField for absent id"
  );
}

#[test]
fn query_to_pb_timeout_overflow_is_error() {
  // A duration whose nanosecond count exceeds u64::MAX (requires u128) must
  // produce BridgeError::InvalidValue, not a silent truncation.
  // u64::MAX nanos ≈ 584 years; add one second to guarantee overflow.
  let huge_timeout =
    core::time::Duration::from_nanos(u64::MAX) + core::time::Duration::from_secs(1);
  let typed: QueryMessage<I, A> = QueryMessage {
    ltime: LamportTime::new(1),
    id: 1,
    from: Node::new(SmolStr::from("n"), sample_addr()),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: huge_timeout,
    name: SmolStr::from("q"),
    payload: bytes::Bytes::new(),
  };
  assert!(
    query_to_pb(&typed).is_err(),
    "timeout exceeding u64::MAX nanoseconds must yield BridgeError::InvalidValue"
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
  let decoded_pb = PbQueryResponseMessage::decode_from_slice(encoded.as_slice())
    .expect("decode_from_slice failed");
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
  let decoded_pb = PbQueryResponseMessage::decode_from_slice(encoded.as_slice())
    .expect("decode_from_slice failed");
  let roundtripped: QueryResponseMessage<I, A> =
    query_response_from_pb(&decoded_pb).expect("query_response_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.id, typed.id);
  assert!(!roundtripped.ack());
  assert_eq!(
    roundtripped.payload,
    bytes::Bytes::from_static(b"response-data")
  );
}

#[test]
fn query_response_message_ltime_required() {
  let pb = PbQueryResponseMessage {
    ltime: None,
    id: Some(1),
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
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
    from: Some(bytes::Bytes::new()),
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

// ── UserEvent (single) ────────────────────────────────────────────────────────

#[test]
fn user_event_single_roundtrip_pb() {
  let typed = UserEvent {
    name: SmolStr::from("deploy"),
    payload: bytes::Bytes::from_static(b"ev-payload"),
  };

  let pb = user_event_single_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbUserEvent::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = user_event_single_from_pb(&decoded_pb);

  assert_eq!(roundtripped.name, typed.name);
  assert_eq!(roundtripped.payload, typed.payload);
}

#[test]
fn user_event_single_empty_payload_roundtrip() {
  let typed = UserEvent {
    name: SmolStr::from("ping"),
    payload: bytes::Bytes::new(),
  };

  let pb = user_event_single_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbUserEvent::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = user_event_single_from_pb(&decoded_pb);

  assert_eq!(roundtripped.name, typed.name);
  assert!(roundtripped.payload.is_empty());
}

// ── UserEvents (batch) ────────────────────────────────────────────────────────

#[test]
fn user_events_roundtrip_pb() {
  let typed = UserEvents {
    ltime: LamportTime::new(7),
    events: vec![
      UserEvent {
        name: SmolStr::from("deploy"),
        payload: bytes::Bytes::from_static(b"v1"),
      },
      UserEvent {
        name: SmolStr::from("alert"),
        payload: bytes::Bytes::from_static(b"critical"),
      },
    ],
  };

  let pb = user_events_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbUserEvents::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = user_events_from_pb(&decoded_pb).expect("user_events_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.events.len(), 2);
  assert_eq!(roundtripped.events[0].name, SmolStr::from("deploy"));
  assert_eq!(roundtripped.events[1].name, SmolStr::from("alert"));
}

#[test]
fn user_events_ltime_required() {
  let pb = PbUserEvents {
    ltime: None,
    events: vec![],
    ..Default::default()
  };
  assert!(
    user_events_from_pb(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

#[test]
fn user_events_empty_events_rejected() {
  // A UserEvents batch with an empty events list must be rejected — the legacy
  // invariant is OneOrMore (at least one event per batch). An empty batch
  // carries no information and would silently consume a buffer history slot.
  let pb = PbUserEvents {
    ltime: Some(1),
    events: vec![],
    ..Default::default()
  };
  assert!(
    user_events_from_pb(&pb).is_err(),
    "expected BridgeError::MissingField for empty events list"
  );
}

// ── PushPullMessage ───────────────────────────────────────────────────────────

#[test]
fn push_pull_message_roundtrip_pb_full() {
  let typed: PushPullMessage<I> = PushPullMessage {
    ltime: LamportTime::new(100),
    status_ltimes: vec![
      (SmolStr::from("node-a"), LamportTime::new(10)),
      (SmolStr::from("node-b"), LamportTime::new(20)),
    ],
    left_members: vec![SmolStr::from("node-gone")],
    event_ltime: LamportTime::new(50),
    events: vec![UserEvents {
      ltime: LamportTime::new(49),
      events: vec![UserEvent {
        name: SmolStr::from("deploy"),
        payload: bytes::Bytes::from_static(b"v2"),
      }],
    }],
    query_ltime: LamportTime::new(75),
  };

  let pb = push_pull_to_pb(&typed).expect("push_pull_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbPushPullMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: PushPullMessage<I> =
    push_pull_from_pb(&decoded_pb).expect("push_pull_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.event_ltime, typed.event_ltime);
  assert_eq!(roundtripped.query_ltime, typed.query_ltime);

  assert_eq!(roundtripped.status_ltimes.len(), 2);
  assert_eq!(roundtripped.status_ltimes[0].0, SmolStr::from("node-a"));
  assert_eq!(roundtripped.status_ltimes[0].1, LamportTime::new(10));
  assert_eq!(roundtripped.status_ltimes[1].0, SmolStr::from("node-b"));
  assert_eq!(roundtripped.status_ltimes[1].1, LamportTime::new(20));

  assert_eq!(roundtripped.left_members.len(), 1);
  assert_eq!(roundtripped.left_members[0], SmolStr::from("node-gone"));

  assert_eq!(roundtripped.events.len(), 1);
  assert_eq!(roundtripped.events[0].ltime, LamportTime::new(49));
  assert_eq!(roundtripped.events[0].events.len(), 1);
  assert_eq!(
    roundtripped.events[0].events[0].name,
    SmolStr::from("deploy")
  );
}

#[test]
fn push_pull_message_roundtrip_pb_empty() {
  let typed: PushPullMessage<I> = PushPullMessage {
    ltime: LamportTime::new(1),
    status_ltimes: vec![],
    left_members: vec![],
    event_ltime: LamportTime::new(2),
    events: vec![],
    query_ltime: LamportTime::new(3),
  };

  let pb = push_pull_to_pb(&typed).expect("push_pull_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbPushPullMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: PushPullMessage<I> =
    push_pull_from_pb(&decoded_pb).expect("push_pull_from_pb failed");

  assert_eq!(roundtripped.ltime, typed.ltime);
  assert_eq!(roundtripped.event_ltime, typed.event_ltime);
  assert_eq!(roundtripped.query_ltime, typed.query_ltime);
  assert!(roundtripped.status_ltimes.is_empty());
  assert!(roundtripped.left_members.is_empty());
  assert!(roundtripped.events.is_empty());
}

#[test]
fn push_pull_message_ltime_required() {
  let pb = PbPushPullMessage {
    ltime: None,
    event_ltime: Some(1),
    query_ltime: Some(1),
    ..Default::default()
  };
  assert!(
    push_pull_from_pb::<I>(&pb).is_err(),
    "expected BridgeError::MissingField for absent ltime"
  );
}

#[test]
fn push_pull_message_event_ltime_required() {
  let pb = PbPushPullMessage {
    ltime: Some(1),
    event_ltime: None,
    query_ltime: Some(1),
    ..Default::default()
  };
  assert!(
    push_pull_from_pb::<I>(&pb).is_err(),
    "expected BridgeError::MissingField for absent event_ltime"
  );
}

#[test]
fn push_pull_message_query_ltime_required() {
  let pb = PbPushPullMessage {
    ltime: Some(1),
    event_ltime: Some(1),
    query_ltime: None,
    ..Default::default()
  };
  assert!(
    push_pull_from_pb::<I>(&pb).is_err(),
    "expected BridgeError::MissingField for absent query_ltime"
  );
}

#[test]
fn push_pull_node_status_time_ltime_required() {
  // A status_ltimes entry with ltime absent must cause push_pull_from_pb to reject the message.
  use super::serf::v1::NodeStatusTime;
  let pb = PbPushPullMessage {
    ltime: Some(1),
    event_ltime: Some(1),
    query_ltime: Some(1),
    status_ltimes: vec![NodeStatusTime {
      id: Some(bytes::Bytes::new()),
      ltime: None, // absent ltime — must be rejected
      ..Default::default()
    }],
    ..Default::default()
  };
  assert!(
    push_pull_from_pb::<SmolStr>(&pb).is_err(),
    "expected BridgeError::MissingField for absent NodeStatusTime.ltime"
  );
}

// ── KeyRequestMessage ─────────────────────────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn key_request_roundtrip_pb_with_key() {
  let key = SecretKey::Aes256([0xABu8; 32]);
  let typed = KeyRequestMessage { key: Some(key) };

  let pb = key_request_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbKeyRequestMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = key_request_from_pb(&decoded_pb).expect("key_request_from_pb failed");

  assert_eq!(roundtripped.key, typed.key);
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_request_roundtrip_pb_no_key() {
  let typed = KeyRequestMessage { key: None };

  let pb = key_request_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbKeyRequestMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = key_request_from_pb(&decoded_pb).expect("key_request_from_pb failed");

  assert!(roundtripped.key.is_none());
}

// ── KeyResponseMessage ────────────────────────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn key_response_roundtrip_pb_success_with_keys() {
  let k1 = SecretKey::Aes128([0x11u8; 16]);
  let k2 = SecretKey::Aes256([0x22u8; 32]);
  let primary = SecretKey::Aes128([0x11u8; 16]);
  let typed = KeyResponseMessage {
    result: true,
    message: SmolStr::from("ok"),
    keys: vec![k1, k2],
    primary_key: Some(primary),
  };

  let pb = key_response_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbKeyResponseMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = key_response_from_pb(&decoded_pb).expect("key_response_from_pb failed");

  assert!(roundtripped.result);
  assert_eq!(roundtripped.message, SmolStr::from("ok"));
  assert_eq!(roundtripped.keys.len(), 2);
  assert_eq!(roundtripped.keys[0], typed.keys[0]);
  assert_eq!(roundtripped.keys[1], typed.keys[1]);
  assert_eq!(roundtripped.primary_key, typed.primary_key);
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_response_roundtrip_pb_failure_no_keys() {
  let typed = KeyResponseMessage {
    result: false,
    message: SmolStr::from("permission denied"),
    keys: vec![],
    primary_key: None,
  };

  let pb = key_response_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbKeyResponseMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped = key_response_from_pb(&decoded_pb).expect("key_response_from_pb failed");

  assert!(!roundtripped.result);
  assert_eq!(roundtripped.message, SmolStr::from("permission denied"));
  assert!(roundtripped.keys.is_empty());
  assert!(roundtripped.primary_key.is_none());
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_response_malformed_bytes_rejected() {
  // A key entry with no algorithm tag byte must be rejected.
  let pb = PbKeyResponseMessage {
    result: true,
    message: String::from("ok"),
    keys: vec![bytes::Bytes::new()], // empty — missing algorithm tag
    primary_key: None,
    ..Default::default()
  };
  assert!(
    key_response_from_pb(&pb).is_err(),
    "expected BridgeError::InvalidValue for empty key bytes"
  );
}

// ── RelayMessage ──────────────────────────────────────────────────────────────

#[test]
fn relay_message_roundtrip_pb() {
  let destination: Node<I, A> = Node::new(SmolStr::from("relay-target"), sample_addr());
  // The payload carries a raw framed serf message; here we use arbitrary bytes
  // to verify the passthrough without interpreting the content.
  let payload = bytes::Bytes::from_static(b"\x04\x05hello");
  let typed: RelayMessage<I, A> = RelayMessage::new(destination.clone(), payload.clone());

  let pb = relay_to_pb(&typed).expect("relay_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbRelayMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: RelayMessage<I, A> = relay_from_pb(&decoded_pb).expect("relay_from_pb failed");

  assert_eq!(
    roundtripped.destination.id_ref(),
    typed.destination.id_ref()
  );
  assert_eq!(
    roundtripped.destination.addr_ref(),
    typed.destination.addr_ref()
  );
  assert_eq!(roundtripped.payload, payload);
}

#[test]
fn relay_message_empty_payload_roundtrip() {
  let destination: Node<I, A> = Node::new(SmolStr::from("target"), sample_addr());
  let typed: RelayMessage<I, A> = RelayMessage::new(destination, bytes::Bytes::new());

  let pb = relay_to_pb(&typed).expect("relay_to_pb failed");
  let encoded = pb.encode_to_vec();
  let decoded_pb =
    PbRelayMessage::decode_from_slice(encoded.as_slice()).expect("decode_from_slice failed");
  let roundtripped: RelayMessage<I, A> = relay_from_pb(&decoded_pb).expect("relay_from_pb failed");

  assert!(roundtripped.payload.is_empty());
}

#[test]
fn relay_message_empty_destination_rejected() {
  // A RelayMessage pb with `destination = b""` must be rejected by
  // relay_from_pb: an empty byte slice is not a valid encoded Node<I,A>,
  // so data_from_bytes returns a Data decode error.
  let pb = PbRelayMessage {
    destination: Some(bytes::Bytes::new()),
    payload: bytes::Bytes::from_static(b"some-payload"),
    ..Default::default()
  };
  assert!(
    relay_from_pb::<I, A>(&pb).is_err(),
    "expected a BridgeError for empty destination bytes"
  );
}
