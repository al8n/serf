use buffa::Message as _;
use smol_str::SmolStr;

use super::serf::v1::{
  Coordinate as PbCoordinate,
  Filter as PbFilter,
  Tags as PbTags,
  UserEventMessage as PbUserEventMessage,
};
use crate::{
  Coordinate,
  Filter,
  LamportTime,
  QueryFlag,
  TagFilter,
  Tags,
  UserEventMessage,
  coordinate_from_pb,
  coordinate_to_pb,
  filter_from_pb,
  filter_to_pb,
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
  let typed = Filter::Id(vec![SmolStr::from("node-1"), SmolStr::from("node-2")]);

  let pb = filter_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped = filter_from_pb(&decoded_pb).expect("filter_from_pb");

  assert_eq!(roundtripped, typed);
}

#[test]
fn filter_tag_with_expr_roundtrip() {
  let typed = Filter::Tag(TagFilter {
    tag: SmolStr::from("role"),
    expr: Some(SmolStr::from("^web.*")),
  });

  let pb = filter_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped = filter_from_pb(&decoded_pb).expect("filter_from_pb");

  assert_eq!(roundtripped, typed);
}

#[test]
fn filter_tag_without_expr_roundtrip() {
  let typed = Filter::Tag(TagFilter {
    tag: SmolStr::from("dc"),
    expr: None,
  });

  let pb = filter_to_pb(&typed);
  let encoded = pb.encode_to_vec();
  let decoded_pb = PbFilter::decode_from_slice(encoded.as_slice()).expect("decode_from_slice");
  let roundtripped = filter_from_pb(&decoded_pb).expect("filter_from_pb");

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
    filter_from_pb(&pb).is_err(),
    "expected BridgeError::UnknownVariant for missing kind"
  );
}
