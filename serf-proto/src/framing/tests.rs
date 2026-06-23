use buffa::Message as _;
use bytes::Bytes;

use super::{FrameError, MessageType, decode_message, encode_message};
use crate::messages::serf::v1::UserEventMessage as PbUserEventMessage;
use crate::{LamportTime, UserEventMessage, user_event_from_pb, user_event_to_pb};

// ── MessageType round-trips ───────────────────────────────────────────────────

#[test]
fn message_type_tag_round_trip() {
  let cases: &[(MessageType, u8)] = &[
    (MessageType::Leave, 1),
    (MessageType::Join, 2),
    (MessageType::PushPull, 3),
    (MessageType::UserEvent, 4),
    (MessageType::Query, 5),
    (MessageType::QueryResponse, 6),
    (MessageType::ConflictResponse, 7),
    (MessageType::Relay, 8),
  ];

  for &(ref ty, expected_byte) in cases {
    let byte = u8::from(*ty);
    assert_eq!(
      byte, expected_byte,
      "{ty:?} should have tag byte {expected_byte}"
    );
    assert_eq!(
      MessageType::from(byte),
      *ty,
      "u8 {byte} should round-trip to {ty:?}"
    );
  }
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn message_type_key_tag_round_trip() {
  let cases: &[(MessageType, u8)] = &[
    (MessageType::KeyRequest, 9),
    (MessageType::KeyResponse, 10),
  ];

  for &(ref ty, expected_byte) in cases {
    let byte = u8::from(*ty);
    assert_eq!(
      byte, expected_byte,
      "{ty:?} should have tag byte {expected_byte}"
    );
    assert_eq!(
      MessageType::from(byte),
      *ty,
      "u8 {byte} should round-trip to {ty:?}"
    );
  }
}

/// Without an encryption feature, key tag bytes must decode as Unknown.
#[cfg(not(any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
#[test]
fn message_type_key_tags_are_unknown_without_encryption() {
  assert_eq!(MessageType::from(9u8), MessageType::Unknown(9));
  assert_eq!(MessageType::from(10u8), MessageType::Unknown(10));
}

#[test]
fn message_type_unknown_preserved() {
  let unknown_byte: u8 = 42;
  let ty = MessageType::from(unknown_byte);
  assert_eq!(ty, MessageType::Unknown(42));
  assert_eq!(u8::from(ty), unknown_byte);
}

// ── encode_message / decode_message ──────────────────────────────────────────

#[test]
fn user_event_frame_round_trip() {
  // Build a typed UserEventMessage and convert to the pb shape.
  let typed = UserEventMessage {
    ltime: LamportTime::new(7),
    cc: true,
    name: smol_str::SmolStr::from("deploy"),
    payload: Bytes::from_static(b"hello-serf"),
  };
  let pb = user_event_to_pb(&typed);

  // Encode into a serf frame.
  let frame_vec =
    encode_message(MessageType::UserEvent, &pb).expect("encode_message should succeed");
  assert!(
    !frame_vec.is_empty(),
    "encoded frame must not be empty"
  );

  // The leading byte must be the UserEvent tag.
  assert_eq!(
    frame_vec[0],
    u8::from(MessageType::UserEvent),
    "first byte must be the UserEvent tag (4)"
  );

  // decode_message recovers the tag and zero-copy body slice.
  let frame = Bytes::from(frame_vec);
  let (recovered_ty, body, consumed) =
    decode_message(&frame).expect("decode_message should succeed");

  assert_eq!(recovered_ty, MessageType::UserEvent);
  assert_eq!(
    consumed,
    frame.len(),
    "consumed must equal the full frame length"
  );

  // The body bytes decode back to the original pb message.
  let decoded_pb =
    PbUserEventMessage::decode_from_slice(body.as_ref()).expect("decode_from_slice should succeed");
  let roundtripped = user_event_from_pb(&decoded_pb).expect("user_event_from_pb should succeed");

  assert_eq!(roundtripped, typed);
}

#[test]
fn decode_message_empty_errors() {
  let empty = Bytes::new();
  assert!(matches!(decode_message(&empty), Err(super::FrameError::Empty)));
}

#[test]
fn decode_message_truncated_errors() {
  // Only the tag byte — no varint length yet.
  let truncated = Bytes::from_static(&[4u8]);
  assert!(matches!(
    decode_message(&truncated),
    Err(super::FrameError::Incomplete(_))
  ));
}

#[test]
fn encode_message_encodes_empty_body() {
  // An empty buffa message (all fields at default) should produce a valid
  // frame containing just the tag + a single zero-varint (body_len=0).
  let pb = PbUserEventMessage::default();
  let frame_vec =
    encode_message(MessageType::UserEvent, &pb).expect("encode_message should succeed");
  let frame = Bytes::from(frame_vec);
  let (ty, body, consumed) = decode_message(&frame).expect("decode_message should succeed");
  assert_eq!(ty, MessageType::UserEvent);
  assert_eq!(body.len(), 0);
  assert_eq!(consumed, frame.len());
}

// ── Security-relevant decoder edge cases ─────────────────────────────────────

#[test]
fn decode_varint_overflow_is_error_not_panic() {
  // A 5-byte LEB128 whose 5th byte has bits beyond position 28 set would
  // overflow u32. The decoder must reject it with VarintOverflow, not panic.
  // Byte sequence: 4 continuation bytes (all 0x80) + one final byte > 0x0f.
  let buf: &[u8] = &[
    u8::from(MessageType::UserEvent), // tag
    0x80, 0x80, 0x80, 0x80, 0x10, // 5-byte LEB128 with 5th byte = 0x10 > 0x0f
  ];
  let frame = Bytes::copy_from_slice(buf);
  assert!(
    matches!(decode_message(&frame), Err(FrameError::VarintOverflow)),
    "a 5th LEB128 byte > 0x0f must yield VarintOverflow"
  );
}

#[test]
fn decode_large_declared_body_len_is_incomplete_not_alloc() {
  // A well-formed header declaring body_len = 1_000_000 in a 5-byte buffer
  // must yield Incomplete, not a panic or a huge allocation attempt.
  // Encode body_len = 1_000_000 as LEB128: 0xC0 0x84 0x3D (3 bytes).
  let body_len: u32 = 1_000_000;
  let mut header = vec![u8::from(MessageType::UserEvent)];
  let mut v = body_len;
  while v >= 0x80 {
    header.push(((v & 0x7f) as u8) | 0x80);
    v >>= 7;
  }
  header.push(v as u8);
  // The buffer is just the header — no body bytes follow.
  let frame = Bytes::from(header);
  assert!(
    matches!(decode_message(&frame), Err(FrameError::Incomplete(_))),
    "a well-formed header with body_len=1_000_000 but no body bytes must yield Incomplete"
  );
}

#[test]
fn unknown_tag_round_trips_encode_decode() {
  // An Unknown(200) tag must survive encode + decode: the tag byte is
  // preserved and the body (empty default message) is recovered intact.
  let pb = PbUserEventMessage::default();
  let frame_vec =
    encode_message(MessageType::Unknown(200), &pb).expect("encode_message with Unknown tag should succeed");
  let frame = Bytes::from(frame_vec);
  assert_eq!(frame[0], 200, "first byte must be the Unknown tag value 200");
  let (recovered_ty, body, consumed) =
    decode_message(&frame).expect("decode_message with Unknown tag should succeed");
  assert_eq!(recovered_ty, MessageType::Unknown(200));
  assert_eq!(body.len(), 0);
  assert_eq!(consumed, frame.len());
}
