use buffa::Message as _;

use super::serf::v1::UserEventMessage as PbUserEventMessage;
use crate::{LamportTime, UserEventMessage, user_event_from_pb, user_event_to_pb};

#[test]
fn user_event_message_roundtrip_pb() {
  let typed = UserEventMessage {
    ltime: LamportTime::new(42),
    cc: true,
    name: smol_str::SmolStr::from("deploy"),
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
