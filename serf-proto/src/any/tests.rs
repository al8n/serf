//! Round-trip tests for AnyMessage encode and decode.

use std::net::SocketAddr;

use bytes::Bytes;
use memberlist_proto::Node;
use smol_str::SmolStr;

use super::{AnyMessage, DecodeError};
#[cfg(feature = "aes-gcm")]
use crate::bridge::{key_request_to_pb, key_response_to_pb};
use crate::{
  BridgeError, ConflictResponseMessage, JoinMessage, LamportTime, LeaveMessage, MessageType,
  PushPullMessage, QueryFlag, QueryMessage, QueryResponseMessage, RelayMessage, UserEvent,
  UserEventMessage, UserEvents,
  bridge::{
    conflict_response_to_pb, join_to_pb, leave_to_pb, push_pull_to_pb, query_response_to_pb,
    query_to_pb, relay_to_pb, user_event_to_pb,
  },
  framing::encode_message,
  messages::serf::v1 as pb,
};
#[cfg(feature = "aes-gcm")]
use crate::{KeyRequestMessage, KeyResponseMessage};
#[cfg(feature = "aes-gcm")]
use memberlist_proto::SecretKey;

// Convenience aliases used throughout.
type I = SmolStr;
type A = SocketAddr;

fn sample_addr() -> A {
  "127.0.0.1:7946".parse().unwrap()
}

// ─── UserEvent ────────────────────────────────────────────────────────────────

#[test]
fn any_message_user_event_round_trip() {
  let typed = UserEventMessage {
    ltime: LamportTime::new(1),
    cc: true,
    name: SmolStr::from("deploy"),
    payload: Bytes::from_static(b"hello"),
  };
  let pb = user_event_to_pb(&typed);
  let frame = encode_message(MessageType::UserEvent, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::UserEvent(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.cc, typed.cc);
      assert_eq!(m.name, typed.name);
      assert_eq!(m.payload, typed.payload);
    }
    other => panic!("expected UserEvent, got {:?}", other.message_type()),
  }
}

// ─── JoinMessage ─────────────────────────────────────────────────────────────

#[test]
fn any_message_join_round_trip() {
  let typed: JoinMessage<I> = JoinMessage::new(LamportTime::new(2), SmolStr::from("node-join"));
  let pb = join_to_pb(&typed).expect("join_to_pb");
  let frame = encode_message(MessageType::Join, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::Join(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.id, typed.id);
    }
    other => panic!("expected Join, got {:?}", other.message_type()),
  }
}

// ─── LeaveMessage ─────────────────────────────────────────────────────────────

#[test]
fn any_message_leave_round_trip() {
  let typed: LeaveMessage<I> =
    LeaveMessage::new(LamportTime::new(3), SmolStr::from("node-leave"), true);
  let pb = leave_to_pb(&typed).expect("leave_to_pb");
  let frame = encode_message(MessageType::Leave, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::Leave(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.id, typed.id);
      assert_eq!(m.prune, typed.prune);
    }
    other => panic!("expected Leave, got {:?}", other.message_type()),
  }
}

// ─── PushPullMessage ──────────────────────────────────────────────────────────

#[test]
fn any_message_push_pull_round_trip() {
  let typed: PushPullMessage<I> = PushPullMessage {
    ltime: LamportTime::new(10),
    status_ltimes: vec![(SmolStr::from("node-a"), LamportTime::new(5))],
    left_members: vec![SmolStr::from("node-gone")],
    event_ltime: LamportTime::new(8),
    events: vec![UserEvents {
      ltime: LamportTime::new(7),
      events: vec![UserEvent {
        name: SmolStr::from("ev"),
        payload: Bytes::from_static(b"p"),
      }],
    }],
    query_ltime: LamportTime::new(9),
  };
  let pb = push_pull_to_pb(&typed).expect("push_pull_to_pb");
  let frame = encode_message(MessageType::PushPull, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::PushPull(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.event_ltime, typed.event_ltime);
      assert_eq!(m.query_ltime, typed.query_ltime);
      assert_eq!(m.status_ltimes.len(), 1);
      assert_eq!(m.left_members.len(), 1);
      assert_eq!(m.events.len(), 1);
    }
    other => panic!("expected PushPull, got {:?}", other.message_type()),
  }
}

// ─── QueryMessage ─────────────────────────────────────────────────────────────

#[test]
fn any_message_query_round_trip() {
  let typed: QueryMessage<I, A> = QueryMessage {
    ltime: LamportTime::new(20),
    id: 42,
    from: Node::new(SmolStr::from("node-q"), sample_addr()),
    filters: vec![],
    flags: QueryFlag::ACK,
    relay_factor: 1,
    timeout: std::time::Duration::from_secs(1),
    name: SmolStr::from("my-query"),
    payload: Bytes::from_static(b"qp"),
  };
  let pb = query_to_pb(&typed).expect("query_to_pb");
  let frame = encode_message(MessageType::Query, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::Query(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.id, typed.id);
      assert_eq!(m.name, typed.name);
      assert_eq!(m.flags, typed.flags);
    }
    other => panic!("expected Query, got {:?}", other.message_type()),
  }
}

// ─── QueryResponseMessage ─────────────────────────────────────────────────────

#[test]
fn any_message_query_response_round_trip() {
  let typed: QueryResponseMessage<I, A> = QueryResponseMessage {
    ltime: LamportTime::new(21),
    id: 42,
    from: Node::new(SmolStr::from("node-resp"), sample_addr()),
    flags: QueryFlag::ACK,
    payload: Bytes::from_static(b"rp"),
  };
  let pb = query_response_to_pb(&typed).expect("query_response_to_pb");
  let frame = encode_message(MessageType::QueryResponse, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::QueryResponse(m) => {
      assert_eq!(m.ltime, typed.ltime);
      assert_eq!(m.id, typed.id);
      assert!(m.ack());
    }
    other => panic!("expected QueryResponse, got {:?}", other.message_type()),
  }
}

// ─── ConflictResponseMessage ──────────────────────────────────────────────────

#[test]
fn any_message_conflict_response_round_trip() {
  let typed: ConflictResponseMessage<I, A> =
    ConflictResponseMessage::new(Node::new(SmolStr::from("winner"), sample_addr()));
  let pb = conflict_response_to_pb(&typed).expect("conflict_response_to_pb");
  let frame = encode_message(MessageType::ConflictResponse, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::ConflictResponse(m) => {
      assert_eq!(m.member.id_ref(), typed.member.id_ref());
      assert_eq!(m.member.addr_ref(), typed.member.addr_ref());
    }
    other => panic!("expected ConflictResponse, got {:?}", other.message_type()),
  }
}

// ─── RelayMessage ─────────────────────────────────────────────────────────────

#[test]
fn any_message_relay_round_trip() {
  let typed: RelayMessage<I, A> = RelayMessage::new(
    Node::new(SmolStr::from("relay-target"), sample_addr()),
    Bytes::from_static(b"\x04\x03xyz"),
  );
  let pb = relay_to_pb(&typed).expect("relay_to_pb");
  let frame = encode_message(MessageType::Relay, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::Relay(m) => {
      assert_eq!(m.destination.id_ref(), typed.destination.id_ref());
      assert_eq!(m.destination.addr_ref(), typed.destination.addr_ref());
      assert_eq!(m.payload, typed.payload);
    }
    other => panic!("expected Relay, got {:?}", other.message_type()),
  }
}

// ─── KeyRequestMessage ────────────────────────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn any_message_key_request_round_trip() {
  let typed = KeyRequestMessage::new(Some(SecretKey::Aes256([0xABu8; 32])));
  let pb = key_request_to_pb(&typed);
  let frame = encode_message(MessageType::KeyRequest, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::KeyRequest(m) => {
      assert_eq!(m.key, typed.key);
    }
    other => panic!("expected KeyRequest, got {:?}", other.message_type()),
  }
}

// ─── KeyResponseMessage ───────────────────────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn any_message_key_response_round_trip() {
  let typed = KeyResponseMessage {
    result: true,
    message: SmolStr::from("ok"),
    keys: vec![SecretKey::Aes128([0x11u8; 16])],
    primary_key: Some(SecretKey::Aes128([0x11u8; 16])),
  };
  let pb = key_response_to_pb(&typed);
  let frame = encode_message(MessageType::KeyResponse, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("AnyMessage::decode");
  match msg {
    AnyMessage::KeyResponse(m) => {
      assert_eq!(m.result, typed.result);
      assert_eq!(m.message, typed.message);
      assert_eq!(m.keys.len(), 1);
    }
    other => panic!("expected KeyResponse, got {:?}", other.message_type()),
  }
}

// ─── AnyMessage::encode round-trips (encode → decode → same variant+fields) ───

#[test]
fn any_message_encode_user_event_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::UserEvent(UserEventMessage {
    ltime: LamportTime::new(7),
    cc: false,
    name: SmolStr::from("ev"),
    payload: Bytes::from_static(b"data"),
  });
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::UserEvent(m) => {
      assert_eq!(m.ltime, LamportTime::new(7));
      assert_eq!(m.name, "ev");
    }
    other => panic!("expected UserEvent, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_join_round_trip() {
  let msg: AnyMessage<I, A> =
    AnyMessage::Join(JoinMessage::new(LamportTime::new(2), SmolStr::from("n")));
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::Join(m) => assert_eq!(m.ltime, LamportTime::new(2)),
    other => panic!("expected Join, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_leave_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::Leave(LeaveMessage::new(
    LamportTime::new(3),
    SmolStr::from("n"),
    true,
  ));
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::Leave(m) => {
      assert_eq!(m.ltime, LamportTime::new(3));
      assert!(m.prune);
    }
    other => panic!("expected Leave, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_push_pull_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::PushPull(PushPullMessage {
    ltime: LamportTime::new(10),
    status_ltimes: vec![],
    left_members: vec![],
    event_ltime: LamportTime::new(8),
    events: vec![],
    query_ltime: LamportTime::new(9),
  });
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::PushPull(m) => {
      assert_eq!(m.ltime, LamportTime::new(10));
      assert_eq!(m.event_ltime, LamportTime::new(8));
      assert_eq!(m.query_ltime, LamportTime::new(9));
    }
    other => panic!("expected PushPull, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_query_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::Query(QueryMessage {
    ltime: LamportTime::new(20),
    id: 1,
    from: Node::new(SmolStr::from("q"), sample_addr()),
    filters: vec![],
    flags: QueryFlag::ACK,
    relay_factor: 0,
    timeout: std::time::Duration::from_secs(1),
    name: SmolStr::from("qname"),
    payload: Bytes::new(),
  });
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::Query(m) => {
      assert_eq!(m.ltime, LamportTime::new(20));
      assert_eq!(m.name, "qname");
    }
    other => panic!("expected Query, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_query_response_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::QueryResponse(QueryResponseMessage {
    ltime: LamportTime::new(21),
    id: 1,
    from: Node::new(SmolStr::from("r"), sample_addr()),
    flags: QueryFlag::ACK,
    payload: Bytes::new(),
  });
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::QueryResponse(m) => assert_eq!(m.ltime, LamportTime::new(21)),
    other => panic!("expected QueryResponse, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_conflict_response_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::ConflictResponse(ConflictResponseMessage::new(
    Node::new(SmolStr::from("winner"), sample_addr()),
  ));
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::ConflictResponse(m) => assert_eq!(m.member.id_ref(), "winner"),
    other => panic!("expected ConflictResponse, got {:?}", other.message_type()),
  }
}

#[test]
fn any_message_encode_relay_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::Relay(RelayMessage::new(
    Node::new(SmolStr::from("dest"), sample_addr()),
    Bytes::from_static(b"\x04\x01x"),
  ));
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::Relay(m) => assert_eq!(m.destination.id_ref(), "dest"),
    other => panic!("expected Relay, got {:?}", other.message_type()),
  }
}

#[cfg(feature = "aes-gcm")]
#[test]
fn any_message_encode_key_request_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::KeyRequest(KeyRequestMessage::new(Some(
    SecretKey::Aes256([0xCCu8; 32]),
  )));
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::KeyRequest(m) => {
      assert!(m.key.is_some());
    }
    other => panic!("expected KeyRequest, got {:?}", other.message_type()),
  }
}

#[cfg(feature = "aes-gcm")]
#[test]
fn any_message_encode_key_response_round_trip() {
  let msg: AnyMessage<I, A> = AnyMessage::KeyResponse(KeyResponseMessage {
    result: true,
    message: SmolStr::from("ok"),
    keys: vec![SecretKey::Aes128([0xAAu8; 16])],
    primary_key: None,
  });
  let frame = msg.encode().expect("encode");
  let decoded: AnyMessage<I, A> = AnyMessage::decode(&frame).expect("decode");
  match decoded {
    AnyMessage::KeyResponse(m) => {
      assert!(m.result);
      assert_eq!(m.keys.len(), 1);
    }
    other => panic!("expected KeyResponse, got {:?}", other.message_type()),
  }
}

// ─── Visibility seal test ─────────────────────────────────────────────────────

#[test]
fn sealed_types_are_crate_internal() {
  // In-crate: the sealed types still EXIST after the seal (resolve here),
  // while no longer being `pub` to dependents.
  fn _assert_crate_visible(
    _: core::marker::PhantomData<super::AnyMessage<u32, std::net::SocketAddr>>,
  ) {
  }
  fn _assert_public(_: core::marker::PhantomData<crate::QueryFlag>) {}
}

// ─── Error cases ──────────────────────────────────────────────────────────────

#[test]
fn any_message_empty_buf_is_error() {
  let empty = Bytes::new();
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&empty),
      Err(DecodeError::Frame(_))
    ),
    "empty buffer must yield DecodeError::Frame"
  );
}

#[test]
fn any_message_unknown_tag_is_error() {
  // A well-formed frame with tag = 200 (unrecognised).
  use crate::messages::serf::v1::UserEventMessage as PbUserEventMessage;
  let pb = PbUserEventMessage::default();
  let frame = Bytes::from(encode_message(MessageType::Unknown(200), &pb).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::UnknownTag(200))
    ),
    "unknown tag must yield DecodeError::UnknownTag(200)"
  );
}

/// Without an encryption feature, key tag bytes must decode as UnknownTag.
#[cfg(not(any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
#[test]
fn any_message_key_tags_unknown_without_encryption() {
  use crate::messages::serf::v1::UserEventMessage as PbUserEventMessage;
  let pb = PbUserEventMessage::default();

  let frame9 = Bytes::from(encode_message(MessageType::Unknown(9), &pb).expect("encode_message"));
  assert!(matches!(
    AnyMessage::<I, A>::decode(&frame9),
    Err(DecodeError::UnknownTag(9))
  ));

  let frame10 = Bytes::from(encode_message(MessageType::Unknown(10), &pb).expect("encode_message"));
  assert!(matches!(
    AnyMessage::<I, A>::decode(&frame10),
    Err(DecodeError::UnknownTag(10))
  ));
}

// ─── Absent required opaque-bytes fields must be rejected ──────────────────────
//
// proto3 plain `bytes` carries no presence bit, so an absent legacy-required
// opaque field would otherwise silently decode as empty bytes. Each field is
// declared `optional` in the schema and rejected as `BridgeError::MissingField`
// before the bytes are handed to the `Data` decoder.

/// A `JoinMessage` whose `id` bytes are absent must be rejected.
#[test]
fn any_message_join_absent_id_is_rejected() {
  let body = pb::JoinMessage {
    ltime: Some(1),
    id: None,
    ..Default::default()
  };
  let frame = Bytes::from(encode_message(MessageType::Join, &body).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::Bridge(BridgeError::MissingField(_)))
    ),
    "absent JoinMessage.id must yield DecodeError::Bridge(MissingField)"
  );
}

/// A `QueryMessage` whose `from` bytes are absent must be rejected.
#[test]
fn any_message_query_absent_from_is_rejected() {
  let body = pb::QueryMessage {
    ltime: Some(1),
    id: Some(1),
    from: None,
    flags: Some(0),
    relay_factor: Some(0),
    timeout_nanos: Some(0),
    ..Default::default()
  };
  let frame = Bytes::from(encode_message(MessageType::Query, &body).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::Bridge(BridgeError::MissingField(_)))
    ),
    "absent QueryMessage.from must yield DecodeError::Bridge(MissingField)"
  );
}

/// A `ConflictResponseMessage` whose `member` bytes are absent must be rejected.
#[test]
fn any_message_conflict_response_absent_member_is_rejected() {
  let body = pb::ConflictResponseMessage {
    member: None,
    ..Default::default()
  };
  let frame =
    Bytes::from(encode_message(MessageType::ConflictResponse, &body).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::Bridge(BridgeError::MissingField(_)))
    ),
    "absent ConflictResponseMessage.member must yield DecodeError::Bridge(MissingField)"
  );
}

/// A `RelayMessage` whose `destination` bytes are absent must be rejected.
#[test]
fn any_message_relay_absent_destination_is_rejected() {
  let body = pb::RelayMessage {
    destination: None,
    payload: Bytes::from_static(b"\x04\x01x"),
    ..Default::default()
  };
  let frame = Bytes::from(encode_message(MessageType::Relay, &body).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::Bridge(BridgeError::MissingField(_)))
    ),
    "absent RelayMessage.destination must yield DecodeError::Bridge(MissingField)"
  );
}

/// A `PushPullMessage` whose `NodeStatusTime.id` bytes are absent must be rejected.
#[test]
fn any_message_push_pull_absent_status_id_is_rejected() {
  let body = pb::PushPullMessage {
    ltime: Some(1),
    status_ltimes: vec![pb::NodeStatusTime {
      id: None,
      ltime: Some(5),
      ..Default::default()
    }],
    event_ltime: Some(2),
    query_ltime: Some(3),
    ..Default::default()
  };
  let frame = Bytes::from(encode_message(MessageType::PushPull, &body).expect("encode_message"));
  assert!(
    matches!(
      AnyMessage::<I, A>::decode(&frame),
      Err(DecodeError::Bridge(BridgeError::MissingField(_)))
    ),
    "absent NodeStatusTime.id must yield DecodeError::Bridge(MissingField)"
  );
}
