use std::hash::Hash;

use quickcheck::{Arbitrary, Gen};

use super::*;

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
        MessageType::ConflictResponse => Message::ConflictResponse(ConflictResponseMessage::arbitrary(g)),
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


#[quickcheck_macros::quickcheck]
fn message_arbitrary(msg: Message<String, String>, node: Option<Node<String, String>>) -> bool {
  todo!()
}

