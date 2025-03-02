use std::hash::Hash;

use memberlist_core::{
  bytes::Bytes,
  proto::{Data, DataRef},
};
use quickcheck::{Arbitrary, Gen};

use super::*;

fn data_round_trip<T: Data + PartialEq>(data: &T) {
  let mut buf = vec![0; data.encoded_len() + 2];
  let len = data.encode(&mut buf).unwrap();
  let buf = &buf[..len];
  let (readed, decoded) = DataRef::decode(&buf[..len]).unwrap();
  let decoded = T::from_ref(decoded).unwrap();
  assert_eq!(len, readed);
  assert_eq!(data, &decoded);

  let mut buf = vec![0; data.encoded_len_with_length_delimited() + 2];
  let len = data.encode_length_delimited(&mut buf).unwrap();
  let buf = &buf[..len];
  let (readed, decoded) = DataRef::decode_length_delimited(&buf[..len]).unwrap();
  let decoded = T::from_ref(decoded).unwrap();
  assert_eq!(len, readed);
  assert_eq!(data, &decoded);
}

macro_rules! data_round_trip {
  ($(
    $(#[$attr:meta])*
    $ty:ty
  ),+$(,)?) => {
    $(
      paste::paste! {
        $(#[$attr])*
        #[quickcheck_macros::quickcheck]
        fn [< data_round_trip_ $ty:snake >](data: $ty) {
          data_round_trip(&data);
        }
      }
    )*
  };
}

type StringFilter = Filter<String>;
type U64Filter = Filter<u64>;

type QueryMessageStringString = QueryMessage<String, String>;
type QueryMessageU64String = QueryMessage<u64, String>;
type QueryMessageStringU64 = QueryMessage<String, u64>;
type QueryMessageU64U64 = QueryMessage<u64, u64>;

type PushPullMessageString = PushPullMessage<String>;
type PushPullMessageU64 = PushPullMessage<u64>;

type MemberStringString = Member<String, String>;
type MemberU64String = Member<u64, String>;
type MemberStringU64 = Member<String, u64>;
type MemberU64U64 = Member<u64, u64>;

type LeaveMessageString = LeaveMessage<String>;
type LeaveMessageU64 = LeaveMessage<u64>;

type JoinMessageString = JoinMessage<String>;
type JoinMessageU64 = JoinMessage<u64>;

type ConflictResponseMessageStringString = ConflictResponseMessage<String, String>;
type ConflictResponseMessageU64U64 = ConflictResponseMessage<u64, u64>;
type ConflictResponseMessageStringU64 = ConflictResponseMessage<String, u64>;

type QueryResponseMessageStringString = QueryResponseMessage<String, String>;
type QueryResponseMessageU64U64 = QueryResponseMessage<u64, u64>;
type QueryResponseMessageStringU64 = QueryResponseMessage<String, u64>;

data_round_trip! {
  ConflictResponseMessageStringString,
  ConflictResponseMessageU64U64,
  ConflictResponseMessageStringU64,
  JoinMessageString,
  JoinMessageU64,
  LeaveMessageString,
  LeaveMessageU64,
  MemberStringString,
  MemberU64String,
  MemberStringU64,
  MemberU64U64,
  Tags,
  TagFilter,
  StringFilter,
  U64Filter,
  UserEvent,
  UserEvents,
  UserEventMessage,
  PushPullMessageU64,
  PushPullMessageString,
  QueryMessageStringString,
  QueryMessageU64String,
  QueryMessageStringU64,
  QueryMessageU64U64,
  QueryResponseMessageStringString,
  QueryResponseMessageU64U64,
  QueryResponseMessageStringU64,
  #[cfg(feature = "encryption")]
  KeyRequestMessage,
  #[cfg(feature = "encryption")]
  KeyResponseMessage,
}

#[derive(Clone, Debug)]
enum Message<I, A> {
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

impl<I, A> Arbitrary for Message<I, A>
where
  I: Arbitrary + Hash + Eq,
  A: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    loop {
      let variant = MessageType::arbitrary(g);

      return match variant {
        MessageType::ConflictResponse => {
          Message::ConflictResponse(ConflictResponseMessage::arbitrary(g))
        }
        MessageType::Join => Message::Join(JoinMessage::arbitrary(g)),
        MessageType::Leave => Message::Leave(LeaveMessage::arbitrary(g)),
        MessageType::PushPull => Message::PushPull(PushPullMessage::arbitrary(g)),
        MessageType::Query => Message::Query(QueryMessage::arbitrary(g)),
        MessageType::QueryResponse => Message::QueryResponse(QueryResponseMessage::arbitrary(g)),
        MessageType::UserEvent => Message::UserEvent(UserEventMessage::arbitrary(g)),
        #[cfg(feature = "encryption")]
        MessageType::KeyRequest => Message::KeyRequest(KeyRequestMessage::arbitrary(g)),
        #[cfg(feature = "encryption")]
        MessageType::KeyResponse => Message::KeyResponse(KeyResponseMessage::arbitrary(g)),
        _ => continue,
      };
    }
  }
}

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

fn encodable_round_trip<I, A>(msg: Message<I, A>, node: Option<Node<I, A>>) -> bool
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
      let MessageRef::Relay { node, payload, .. } = decoded else { return false };
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

macro_rules! encodable_round_trip {
  (@message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< message _encodable_round_trip_ $a:snake _ $b:snake >](msg: Message<$a, $b>) -> bool {
          encodable_round_trip(msg, None)
        }
      }
    )*
  };
  (@relay_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< relay_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: Message<$a, $b>, node: Node<$a, $b>) -> bool {
          encodable_round_trip(msg, Some(node))
        }
      }
    )*
  };
  (@query_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< query_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: QueryMessage<$a, $b>, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::Query(msg), node)
        }
      }
    )*
  };
  (@query_response_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< query_response_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: QueryResponseMessage<$a, $b>, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::QueryResponse(msg), node)
        }
      }
    )*
  };
  (@join_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< join_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: JoinMessage<$a>, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::Join(msg), node)
        }
      }
    )*
  };
  (@push_pull_message $(<$a:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< push_pull_message _encodable_round_trip_ $a:snake >](msg: PushPullMessage<$a>, node: Option<Node<$a, $a>>) -> bool {
          encodable_round_trip(Message::PushPull(msg), node)
        }
      }
    )*
  };
  (@leave_message $(<$a:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< leave_message _encodable_round_trip_ $a:snake >](msg: LeaveMessage<$a>, node: Option<Node<$a, $a>>) -> bool {
          encodable_round_trip(Message::Leave(msg), node)
        }
      }
    )*
  };
  (@conflict_response_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< conflict_response_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: ConflictResponseMessage<$a, $b>, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::ConflictResponse(msg), node)
        }
      }
    )*
  };
  (@user_event_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< user_event_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: UserEventMessage, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::UserEvent(msg), node)
        }
      }
    )*
  };
  (@key_request_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< key_request_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: KeyRequestMessage, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::KeyRequest(msg), node)
        }
      }
    )*
  };
  (@key_response_message $(<$a:ty, $b:ty>),+$(,)?) => {
    $(
      paste::paste! {
        #[quickcheck_macros::quickcheck]
        fn [< key_response_message _encodable_round_trip_ $a:snake _ $b:snake >](msg: KeyResponseMessage, node: Option<Node<$a, $b>>) -> bool {
          encodable_round_trip(Message::KeyResponse(msg), node)
        }
      }
    )*
  };
}

encodable_round_trip!(
  @message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @relay_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @query_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @query_response_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @conflict_response_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @join_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

encodable_round_trip!(
  @push_pull_message
    <u64>,
    <String>,
);

encodable_round_trip!(
  @leave_message
    <u64>,
    <String>,
);

encodable_round_trip!(
  @user_event_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

#[cfg(feature = "encryption")]
encodable_round_trip!(
  @key_request_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

#[cfg(feature = "encryption")]
encodable_round_trip!(
  @key_response_message
    <u64, u64>,
    <String, String>,
    <u64, String>,
    <String, u64>,
);

#[test]
fn test() {
  let data = [19, 33, 9, 1, 18, 18, 24, 17, 19, 115, 101, 114, 102, 95, 106, 111, 105, 110, 95, 108, 101, 97, 118, 101, 49, 95, 118, 52, 18, 1, 0, 12, 1, 14, 1];

  let msg = super::decode_message::<smol_str::SmolStr, std::net::SocketAddr>(&data).unwrap();
  // println!("{:?}", msg);
}
