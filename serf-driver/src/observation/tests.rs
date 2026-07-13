use bytes::Bytes;
use serf_proto::{LamportTime, UserEventMessage, event::Event};

use super::observation_payload_bytes;

fn user_event(name: &str, payload: Bytes) -> Event<u64, std::net::SocketAddr> {
  Event::User(UserEventMessage {
    ltime: LamportTime::new(1),
    cc: false,
    name: name.into(),
    payload,
  })
}

#[test]
fn user_event_charges_name_and_payload() {
  let ev = user_event("evt", Bytes::from_static(b"hello world"));
  // name "evt" (3) + payload "hello world" (11).
  assert_eq!(observation_payload_bytes(&ev), Some(14));
}

#[test]
fn user_event_with_large_name_and_empty_payload_is_charged() {
  // The name is part of the size-limited application event, so a large name with an empty
  // payload must not report Some(0) and slip past the byte cap.
  let name = "a-very-long-user-event-name";
  let ev = user_event(name, Bytes::new());
  assert_eq!(observation_payload_bytes(&ev), Some(name.len() as u64));
}

#[test]
fn shutdown_is_none() {
  let ev: Event<u64, std::net::SocketAddr> = Event::Shutdown;
  assert_eq!(observation_payload_bytes(&ev), None);
}

#[test]
fn left_cluster_is_none() {
  let ev: Event<u64, std::net::SocketAddr> = Event::LeftCluster;
  assert_eq!(observation_payload_bytes(&ev), None);
}

/// `member_tag_bytes` — the weight charged for a `Member` event — sums the tag key+value byte
/// lengths across all members, so large peer tag maps count against the byte-backstop.
#[test]
fn member_tag_bytes_sums_tag_key_value_lengths() {
  use memberlist_proto::Node;
  use serf_proto::{
    Tags,
    members::{Member, MemberStatus},
  };

  let tags: Tags = [("role", "leader"), ("dc", "us-east-1")]
    .into_iter()
    .collect();
  let expected: u64 = ("role".len() + "leader".len() + "dc".len() + "us-east-1".len()) as u64;
  let node = Node::new(
    1u64,
    "127.0.0.1:7946".parse::<std::net::SocketAddr>().unwrap(),
  );
  let member = Member::new(node, tags, MemberStatus::Alive);

  assert_eq!(
    super::member_tag_bytes(std::slice::from_ref(&member)),
    expected
  );
  let empty: [Member<u64, std::net::SocketAddr>; 0] = [];
  assert_eq!(super::member_tag_bytes(&empty), 0);
}
