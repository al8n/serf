use memberlist_proto::{Data, EncodeError, WireType, bytes::Bytes, utils::merge};

use super::{
  ConflictResponseMessage, ConflictResponseMessageBorrow, JoinMessage, LeaveMessage,
  PushPullMessageBorrow, QueryMessage, QueryResponseMessage, UserEventMessage,
};

#[cfg(feature = "encryption")]
use super::{KeyRequestMessage, KeyResponseMessage};

const LEAVE_MESSAGE_TAG: u8 = 1;
const JOIN_MESSAGE_TAG: u8 = 2;
const PUSH_PULL_MESSAGE_TAG: u8 = 3;
const USER_EVENT_MESSAGE_TAG: u8 = 4;
const QUERY_MESSAGE_TAG: u8 = 5;
const QUERY_RESPONSE_MESSAGE_TAG: u8 = 6;
const CONFLICT_RESPONSE_MESSAGE_TAG: u8 = 7;
const RELAY_MESSAGE_TAG: u8 = 8;
#[cfg(feature = "encryption")]
const KEY_REQUEST_MESSAGE_TAG: u8 = 9;
#[cfg(feature = "encryption")]
const KEY_RESPONSE_MESSAGE_TAG: u8 = 10;

const LEAVE_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, LEAVE_MESSAGE_TAG);
const JOIN_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, JOIN_MESSAGE_TAG);
const PUSH_PULL_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, PUSH_PULL_MESSAGE_TAG);
const USER_EVENT_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, USER_EVENT_MESSAGE_TAG);
const QUERY_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, QUERY_MESSAGE_TAG);
const QUERY_RESPONSE_MESSAGE_BYTE: u8 =
  merge(WireType::LengthDelimited, QUERY_RESPONSE_MESSAGE_TAG);
const CONFLICT_RESPONSE_MESSAGE_BYTE: u8 =
  merge(WireType::LengthDelimited, CONFLICT_RESPONSE_MESSAGE_TAG);
const RELAY_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, RELAY_MESSAGE_TAG);
#[cfg(feature = "encryption")]
const KEY_REQUEST_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, KEY_REQUEST_MESSAGE_TAG);
#[cfg(feature = "encryption")]
const KEY_RESPONSE_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, KEY_RESPONSE_MESSAGE_TAG);

/// The types of gossip messages Serf will send along
/// memberlist.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum MessageType {
  /// Leave message
  Leave,
  /// Join message
  Join,
  /// PushPull message
  PushPull,
  /// UserEvent message
  UserEvent,
  /// Query message
  Query,
  /// QueryResponse message
  QueryResponse,
  /// ConflictResponse message
  ConflictResponse,
  /// Relay message
  Relay,
  /// KeyRequest message
  #[cfg(feature = "encryption")]
  KeyRequest,
  /// KeyResponse message
  #[cfg(feature = "encryption")]
  KeyResponse,
  /// Unknown message type, used for forwards and backwards compatibility
  Unknown(u8),
}

impl MessageType {
  /// Get the string representation of the message type
  #[inline]
  pub fn as_str(&self) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(match self {
      Self::Leave => "leave",
      Self::Join => "join",
      Self::PushPull => "push_pull",
      Self::UserEvent => "user_event",
      Self::Query => "query",
      Self::QueryResponse => "query_response",
      Self::ConflictResponse => "conflict_response",
      Self::Relay => "relay",
      #[cfg(feature = "encryption")]
      Self::KeyRequest => "key_request",
      #[cfg(feature = "encryption")]
      Self::KeyResponse => "key_response",
      Self::Unknown(val) => return std::borrow::Cow::Owned(format!("unknown({val})")),
    })
  }
}

impl From<u8> for MessageType {
  fn from(value: u8) -> Self {
    match value {
      LEAVE_MESSAGE_TAG => Self::Leave,
      JOIN_MESSAGE_TAG => Self::Join,
      PUSH_PULL_MESSAGE_TAG => Self::PushPull,
      USER_EVENT_MESSAGE_TAG => Self::UserEvent,
      QUERY_MESSAGE_TAG => Self::Query,
      QUERY_RESPONSE_MESSAGE_TAG => Self::QueryResponse,
      CONFLICT_RESPONSE_MESSAGE_TAG => Self::ConflictResponse,
      RELAY_MESSAGE_TAG => Self::Relay,
      #[cfg(feature = "encryption")]
      KEY_REQUEST_MESSAGE_TAG => Self::KeyRequest,
      #[cfg(feature = "encryption")]
      KEY_RESPONSE_MESSAGE_TAG => Self::KeyResponse,
      val => Self::Unknown(val),
    }
  }
}

impl From<MessageType> for u8 {
  fn from(val: MessageType) -> Self {
    match val {
      MessageType::Leave => LEAVE_MESSAGE_TAG,
      MessageType::Join => JOIN_MESSAGE_TAG,
      MessageType::PushPull => PUSH_PULL_MESSAGE_TAG,
      MessageType::UserEvent => USER_EVENT_MESSAGE_TAG,
      MessageType::Query => QUERY_MESSAGE_TAG,
      MessageType::QueryResponse => QUERY_RESPONSE_MESSAGE_TAG,
      MessageType::ConflictResponse => CONFLICT_RESPONSE_MESSAGE_TAG,
      MessageType::Relay => RELAY_MESSAGE_TAG,
      #[cfg(feature = "encryption")]
      MessageType::KeyRequest => KEY_REQUEST_MESSAGE_TAG,
      #[cfg(feature = "encryption")]
      MessageType::KeyResponse => KEY_RESPONSE_MESSAGE_TAG,
      MessageType::Unknown(val) => val,
    }
  }
}

macro_rules! bail {
  ($this:ident($offset:expr, $len:ident)) => {
    if $offset >= $len {
      return Err(EncodeError::insufficient_buffer(
        Encodable::encoded_len($this),
        $len,
      ));
    }
  };
}

/// A trait for encoding messages.
pub trait Encodable {
  /// Encodes the message into a buffer.
  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError>;

  /// Encodes a relay message into a buffer.
  fn encode_relay(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;
    let buf_len = buf.len();

    if offset >= buf_len {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len_with_relay(),
        buf_len,
      ));
    }

    buf[offset] = RELAY_MESSAGE_BYTE;
    offset += 1;

    offset += self
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len_with_relay(), buf_len))?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq(offset, self.encoded_len_with_relay());

    Ok(offset)
  }

  /// Encodes the message into a [`Bytes`].
  fn encode_to_bytes(&self) -> Result<Bytes, EncodeError> {
    let len = self.encoded_len();
    let mut buf = vec![0; len];
    self.encode(&mut buf).map(|_| Bytes::from(buf))
  }

  /// Encodes a relay message into a [`Bytes`].
  fn encode_relay_to_bytes(&self) -> Result<Bytes, EncodeError> {
    let len = self.encoded_len_with_relay();
    let mut buf = vec![0; len];
    self.encode_relay(&mut buf).map(|_| Bytes::from(buf))
  }

  /// Returns the encoded length of the message.
  fn encoded_len(&self) -> usize;

  /// Returns the encoded length of the message with a relay tag.
  fn encoded_len_with_relay(&self) -> usize {
    1 + self.encoded_len()
  }
}

impl<T: Encodable> Encodable for &T {
  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    (*self).encode(buf)
  }

  fn encoded_len(&self) -> usize {
    (*self).encoded_len()
  }
}

macro_rules! impl_encodable {
  (
    $(
      $(#[$attr:meta])*
      $type:ident $(<$($generic:ident), +$(,)?>)? = $id:expr,
    )*
  ) => {
    $(
      $(#[$attr])*
      impl $(<$($generic), +>)? Encodable for $type $(<$($generic), +>)?
      $(
        where
          $($generic: Data,)+
      )?
      {
        fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
          let mut offset = 0;
          let buf_len = buf.len();
          bail!(self(offset, buf_len));

          buf[offset] = $id;
          offset += 1;

          offset += self.encode_length_delimited(&mut buf[offset..])?;

          #[cfg(debug_assertions)]
          super::debug_assert_write_eq(offset, Encodable::encoded_len(self));

          Ok(offset)
        }

        fn encoded_len(&self) -> usize {
          1 + self.encoded_len_with_length_delimited()
        }
      }
    )*
  };
}

impl_encodable!(
  LeaveMessage<I> = LEAVE_MESSAGE_BYTE,
  JoinMessage<I> = JOIN_MESSAGE_BYTE,
  // PushPullMessage<I> = PUSH_PULL_MESSAGE_BYTE,
  UserEventMessage = USER_EVENT_MESSAGE_BYTE,
  QueryMessage<I, A> = QUERY_MESSAGE_BYTE,
  QueryResponseMessage<I, A> = QUERY_RESPONSE_MESSAGE_BYTE,
  ConflictResponseMessage<I, A> = CONFLICT_RESPONSE_MESSAGE_BYTE,
  #[cfg(feature = "encryption")]
  KeyRequestMessage = KEY_REQUEST_MESSAGE_BYTE,
  #[cfg(feature = "encryption")]
  KeyResponseMessage = KEY_RESPONSE_MESSAGE_BYTE,
);

impl<I, A> super::Encodable for ConflictResponseMessageBorrow<'_, I, A>
where
  I: Data,
  A: Data,
{
  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;
    let buf_len = buf.len();
    bail!(self(offset, buf_len));

    buf[offset] = CONFLICT_RESPONSE_MESSAGE_BYTE;
    offset += 1;

    offset += self.encode_in(&mut buf[offset..])?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq(offset, Encodable::encoded_len(self));

    Ok(offset)
  }

  fn encoded_len(&self) -> usize {
    1 + self.encoded_len_in()
  }
}

impl<I> super::Encodable for PushPullMessageBorrow<'_, I>
where
  I: Data,
{
  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;
    let buf_len = buf.len();
    bail!(self(offset, buf_len));

    buf[offset] = PUSH_PULL_MESSAGE_BYTE;
    offset += 1;

    offset += self.encode_in(&mut buf[offset..])?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq(offset, Encodable::encoded_len(self));

    Ok(offset)
  }

  fn encoded_len(&self) -> usize {
    1 + self.encoded_len_in()
  }
}
