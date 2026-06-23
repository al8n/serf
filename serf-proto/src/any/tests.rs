//! Round-trip tests: encode_message → AnyMessage::decode → correct variant + payload.

use std::net::SocketAddr;

use bytes::Bytes;
use memberlist_proto::Node;
use smol_str::SmolStr;

use super::{AnyMessage, DecodeError};
use crate::{
  ConflictResponseMessage,
  JoinMessage,
  LamportTime,
  LeaveMessage,
  MessageType,
  PushPullMessage,
  QueryFlag,
  QueryMessage,
  QueryResponseMessage,
  RelayMessage,
  UserEvent,
  UserEventMessage,
  UserEvents,
  bridge::{
    conflict_response_to_pb,
    join_to_pb,
    leave_to_pb,
    push_pull_to_pb,
    query_response_to_pb,
    query_to_pb,
    relay_to_pb,
    user_event_to_pb,
  },
  encode_message,
};
#[cfg(feature = "aes-gcm")]
use crate::{KeyRequestMessage, KeyResponseMessage, bridge::{key_request_to_pb, key_response_to_pb}};
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
  let frame =
    encode_message(MessageType::UserEvent, &pb).expect("encode_message");
  let frame = Bytes::from(frame);

  let msg: AnyMessage<I, A> =
    AnyMessage::decode(&frame).expect("AnyMessage::decode");
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
  let typed: LeaveMessage<I> = LeaveMessage::new(LamportTime::new(3), SmolStr::from("node-leave"), true);
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

// ─── Error cases ──────────────────────────────────────────────────────────────

#[test]
fn any_message_empty_buf_is_error() {
  let empty = Bytes::new();
  assert!(
    matches!(AnyMessage::<I, A>::decode(&empty), Err(DecodeError::Frame(_))),
    "empty buffer must yield DecodeError::Frame"
  );
}

#[test]
fn any_message_unknown_tag_is_error() {
  // A well-formed frame with tag = 200 (unrecognised).
  use crate::messages::serf::v1::UserEventMessage as PbUserEventMessage;
  let pb = PbUserEventMessage::default();
  let frame = Bytes::from(
    encode_message(MessageType::Unknown(200), &pb).expect("encode_message"),
  );
  assert!(
    matches!(AnyMessage::<I, A>::decode(&frame), Err(DecodeError::UnknownTag(200))),
    "unknown tag must yield DecodeError::UnknownTag(200)"
  );
}

/// Without an encryption feature, key tag bytes must decode as UnknownTag.
#[cfg(not(any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
#[test]
fn any_message_key_tags_unknown_without_encryption() {
  use crate::messages::serf::v1::UserEventMessage as PbUserEventMessage;
  let pb = PbUserEventMessage::default();

  let frame9 =
    Bytes::from(encode_message(MessageType::Unknown(9), &pb).expect("encode_message"));
  assert!(matches!(
    AnyMessage::<I, A>::decode(&frame9),
    Err(DecodeError::UnknownTag(9))
  ));

  let frame10 =
    Bytes::from(encode_message(MessageType::Unknown(10), &pb).expect("encode_message"));
  assert!(matches!(
    AnyMessage::<I, A>::decode(&frame10),
    Err(DecodeError::UnknownTag(10))
  ));
}
