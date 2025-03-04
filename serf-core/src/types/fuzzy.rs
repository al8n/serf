use super::*;

use core::hash::Hash;

use crate::types::bytes::Bytes;
use memberlist_core::proto::Data;

/// Message for fuzzy testing
#[derive(Clone, Debug)]
pub enum Message<I, A> {
  /// Leave message
  Leave(LeaveMessage<I>),
  /// Join message
  Join(JoinMessage<I>),
  /// PushPull message
  PushPull(PushPullMessage<I>),
  /// UserEvent message
  UserEvent(UserEventMessage),
  /// Query message
  Query(QueryMessage<I, A>),
  /// QueryResponse message
  QueryResponse(QueryResponseMessage<I, A>),
  /// ConflictResponse message
  ConflictResponse(ConflictResponseMessage<I, A>),
  #[cfg(feature = "encryption")]
  /// KeyRequest message
  KeyRequest(KeyRequestMessage),
  #[cfg(feature = "encryption")]
  /// KeyResponse message
  KeyResponse(KeyResponseMessage),
}

#[cfg(feature = "arbitrary")]
const _: () = {
  use arbitrary::{Arbitrary, Unstructured};

  impl<'a, I, A> Arbitrary<'a> for Message<I, A>
  where
    I: Arbitrary<'a> + Hash + Eq,
    A: Arbitrary<'a>,
  {
    fn arbitrary(g: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
      loop {
        let variant = MessageType::arbitrary(g)?;

        return Ok(match variant {
          MessageType::ConflictResponse => Message::ConflictResponse(Arbitrary::arbitrary(g)?),
          MessageType::Join => Message::Join(Arbitrary::arbitrary(g)?),
          MessageType::Leave => Message::Leave(Arbitrary::arbitrary(g)?),
          MessageType::PushPull => Message::PushPull(Arbitrary::arbitrary(g)?),
          MessageType::Query => Message::Query(Arbitrary::arbitrary(g)?),
          MessageType::QueryResponse => Message::QueryResponse(Arbitrary::arbitrary(g)?),
          MessageType::UserEvent => Message::UserEvent(Arbitrary::arbitrary(g)?),
          #[cfg(feature = "encryption")]
          MessageType::KeyRequest => Message::KeyRequest(Arbitrary::arbitrary(g)?),
          #[cfg(feature = "encryption")]
          MessageType::KeyResponse => Message::KeyResponse(Arbitrary::arbitrary(g)?),
          _ => continue,
        });
      }
    }
  }
};

#[cfg(feature = "quickcheck")]
const _: () = {
  use quickcheck::{Arbitrary, Gen};

  impl<I, A> Arbitrary for Message<I, A>
  where
    I: Arbitrary + Hash + Eq,
    A: Arbitrary,
  {
    fn arbitrary(g: &mut Gen) -> Self {
      loop {
        let variant = MessageType::arbitrary(g);

        return match variant {
          MessageType::ConflictResponse => Message::ConflictResponse(Arbitrary::arbitrary(g)),
          MessageType::Join => Message::Join(Arbitrary::arbitrary(g)),
          MessageType::Leave => Message::Leave(Arbitrary::arbitrary(g)),
          MessageType::PushPull => Message::PushPull(Arbitrary::arbitrary(g)),
          MessageType::Query => Message::Query(Arbitrary::arbitrary(g)),
          MessageType::QueryResponse => Message::QueryResponse(Arbitrary::arbitrary(g)),
          MessageType::UserEvent => Message::UserEvent(Arbitrary::arbitrary(g)),
          #[cfg(feature = "encryption")]
          MessageType::KeyRequest => Message::KeyRequest(Arbitrary::arbitrary(g)),
          #[cfg(feature = "encryption")]
          MessageType::KeyResponse => Message::KeyResponse(Arbitrary::arbitrary(g)),
          _ => continue,
        };
      }
    }
  }
};

fn encode<T: Encodable>(data: &T) -> Bytes {
  encode_message_to_bytes(data).unwrap()
}

fn encode_relay<I, A, T>(data: &T, node: &Node<I, A>) -> Bytes
where
  I: Data,
  A: Data,
  T: Encodable,
{
  encode_relay_message_to_bytes(data, node).unwrap()
}

pub fn encodable_round_trip<I, A>(msg: Message<I, A>, node: Option<Node<I, A>>) -> bool
where
  I: Data + Eq + Hash,
  A: Data + PartialEq,
{
  macro_rules! encode_variant {
    (< $($g:ty), +$(,)? > $variant:ident ($ty:ty) <- $input:ident) => {{
      let data = encode(&$input);
      assert_eq!(data.len(), encoded_message_len(&$input), "message: length mismatch");
      let decoded = super::decode_message :: < $($g),* > (&data).unwrap();
      let MessageRef::$variant(decoded) = decoded else { return false };

      let owned = <$ty as Data>::from_ref(decoded).unwrap();
      assert_eq!($input, owned, "message: decoded mismatch");
      true
    }};
    (@relay< $($g:ty), +$(,)? > $variant:ident ($ty:ty) <- ($input:ident, $node:ident)) => {{
      let data = encode_relay(&$input, &$node);
      assert_eq!(data.len(), encoded_relay_message_len(&$input, &$node), "relay message: length mismatch");
      let decoded = super::decode_message :: < $($g),* > (&data).unwrap();
      let MessageRef::Relay(RelayMessageRef { node, payload, .. }) = decoded else { return false };
      assert_eq!(<Node<$($g),*> as Data>::from_ref(node).unwrap(), $node, "relay message: node mismatch");

      let decoded = super::decode_message :: < $($g),* > (&payload).unwrap();
      let MessageRef::$variant(decoded) = decoded else { return false };

      let owned = <$ty as Data>::from_ref(decoded).unwrap();
      assert_eq!($input, owned, "relay message: decoded mismatch");
      true
    }};
  }

  match node {
    Some(node) => match msg {
      Message::Leave(leave_message) => {
        encode_variant!(@relay<I, A> Leave(LeaveMessage<I>) <- (leave_message, node))
      }
      Message::Join(join_message) => {
        encode_variant!(@relay<I, A> Join(JoinMessage<I>) <- (join_message, node))
      }
      Message::PushPull(push_pull_message) => {
        encode_variant!(@relay<I, A> PushPull(PushPullMessage<I>) <- (push_pull_message, node))
      }
      Message::UserEvent(user_event_message) => {
        encode_variant!(@relay<I, A> UserEvent(UserEventMessage) <- (user_event_message, node))
      }
      Message::Query(query_message) => {
        encode_variant!(@relay<I, A> Query(QueryMessage<I, A>) <- (query_message, node))
      }
      Message::QueryResponse(query_response_message) => {
        encode_variant!(@relay<I, A> QueryResponse(QueryResponseMessage<I, A>) <- (query_response_message, node))
      }
      Message::ConflictResponse(conflict_response_message) => {
        encode_variant!(@relay<I, A> ConflictResponse(ConflictResponseMessage<I, A>) <- (conflict_response_message, node))
      }
      #[cfg(feature = "encryption")]
      Message::KeyRequest(key_request_message) => {
        encode_variant!(@relay<I, A> KeyRequest(KeyRequestMessage) <- (key_request_message, node))
      }
      #[cfg(feature = "encryption")]
      Message::KeyResponse(key_response_message) => {
        encode_variant!(@relay<I, A> KeyResponse(KeyResponseMessage) <- (key_response_message, node))
      }
    },
    None => match msg {
      Message::Leave(msg) => encode_variant!(<I, A> Leave(LeaveMessage<I>) <- msg),
      Message::Join(join_message) => encode_variant!(<I, A> Join(JoinMessage<I>) <- join_message),
      Message::PushPull(push_pull_message) => {
        encode_variant!(<I, A> PushPull(PushPullMessage<I>) <- push_pull_message)
      }
      Message::UserEvent(user_event_message) => {
        encode_variant!(<I, A> UserEvent(UserEventMessage) <- user_event_message)
      }
      Message::Query(query_message) => {
        encode_variant!(<I, A> Query(QueryMessage<I, A>) <- query_message)
      }
      Message::QueryResponse(query_response_message) => {
        encode_variant!(<I, A> QueryResponse(QueryResponseMessage<I, A>) <- query_response_message)
      }
      Message::ConflictResponse(conflict_response_message) => {
        encode_variant!(<I, A> ConflictResponse(ConflictResponseMessage<I, A>) <- conflict_response_message)
      }
      #[cfg(feature = "encryption")]
      Message::KeyRequest(key_request_message) => {
        encode_variant!(<I, A> KeyRequest(KeyRequestMessage) <- key_request_message)
      }
      #[cfg(feature = "encryption")]
      Message::KeyResponse(key_response_message) => {
        encode_variant!(<I, A> KeyResponse(KeyResponseMessage) <- key_response_message)
      }
    },
  }
}
