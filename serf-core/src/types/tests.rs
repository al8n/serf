use memberlist_core::proto::{Data, DataRef};

use super::{
  coordinate::Coordinate,
  fuzzy::{Message, encodable_round_trip},
  *,
};

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
  Coordinate,
}

data_round_trip! {
  // Coordinate,
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
