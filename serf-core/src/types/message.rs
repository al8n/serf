use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, Node, WireType,
  bytes::Bytes,
  utils::{merge, skip},
};

use super::{
  ConflictResponseMessage, ConflictResponseMessageBorrow, ConflictResponseMessageRef, JoinMessage,
  LeaveMessage, PushPullMessage, PushPullMessageBorrow, PushPullMessageRef, QueryMessage,
  QueryMessageRef, QueryResponseMessage, QueryResponseMessageRef, UserEventMessage,
  UserEventMessageRef,
};

#[cfg(feature = "encryption")]
use super::{KeyRequestMessage, KeyResponseMessage, KeyResponseMessageRef};

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
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, derive_more::Display, derive_more::IsVariant)]
#[repr(u8)]
#[non_exhaustive]
pub enum MessageType {
  /// Leave message
  #[display("leave")]
  Leave,
  /// Join message
  #[display("join")]
  Join,
  /// PushPull message
  #[display("push_pull")]
  PushPull,
  /// UserEvent message
  #[display("user_event")]
  UserEvent,
  /// Query message
  #[display("query")]
  Query,
  /// QueryResponse message
  #[display("query_response")]
  QueryResponse,
  /// ConflictResponse message
  #[display("conflict_response")]
  ConflictResponse,
  /// Relay message
  #[display("relay")]
  Relay,
  /// KeyRequest message
  #[cfg(feature = "encryption")]
  #[display("key_request")]
  KeyRequest,
  /// KeyResponse message
  #[cfg(feature = "encryption")]
  #[display("key_response")]
  KeyResponse,
  /// Unknown message type, used for forwards and backwards compatibility
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl MessageType {
  /// All message types in order
  pub const ALL: &[Self] = &[
    Self::Leave,
    Self::Join,
    Self::PushPull,
    Self::UserEvent,
    Self::Query,
    Self::QueryResponse,
    Self::ConflictResponse,
    Self::Relay,
    #[cfg(feature = "encryption")]
    Self::KeyRequest,
    #[cfg(feature = "encryption")]
    Self::KeyResponse,
  ];

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
        encoded_message_len($this),
        $len,
      ));
    }
  };
  (@relay $this:ident($offset:expr, $len:ident, $node:ident)) => {
    if $offset >= $len {
      return Err(EncodeError::insufficient_buffer(
        encoded_relay_message_len($this, $node),
        $len,
      ));
    }
  };
}

const RELAY_NODE_TAG: u8 = 1;
const RELAY_MSG_TAG: u8 = 2;

const RELAY_NODE_BYTE: u8 = merge(WireType::LengthDelimited, RELAY_NODE_TAG);
const RELAY_MSG_BYTE: u8 = merge(WireType::LengthDelimited, RELAY_MSG_TAG);

/// A trait for encoding messages.
pub trait Encodable {
  const ID: u8;

  /// Encodes the message into a buffer.
  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError>;

  /// Returns the encoded length of the message.
  fn encoded_len(&self) -> usize;
}

impl<T: Encodable> Encodable for &T {
  const ID: u8 = T::ID;

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
        const ID: u8 = $id;

        fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
          Data::encode(self, buf)
        }

        fn encoded_len(&self) -> usize {
          Data::encoded_len(self)
        }
      }
    )*
  };
}

impl_encodable!(
  LeaveMessage<I> = LEAVE_MESSAGE_BYTE,
  JoinMessage<I> = JOIN_MESSAGE_BYTE,
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
  const ID: u8 = CONFLICT_RESPONSE_MESSAGE_BYTE;

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    self.encode_in(buf)
  }

  fn encoded_len(&self) -> usize {
    self.encoded_len_in()
  }
}

impl<I> super::Encodable for PushPullMessage<I>
where
  I: Data + Eq + core::hash::Hash,
{
  const ID: u8 = PUSH_PULL_MESSAGE_BYTE;

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    Data::encode(self, buf)
  }

  fn encoded_len(&self) -> usize {
    Data::encoded_len(self)
  }
}

impl<I> super::Encodable for PushPullMessageBorrow<'_, I>
where
  I: Data,
{
  const ID: u8 = PUSH_PULL_MESSAGE_BYTE;

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    self.encode_in(buf)
  }

  fn encoded_len(&self) -> usize {
    self.encoded_len_in()
  }
}

/// A reference type to a relay message.
#[viewit::viewit(vis_all = "pub(crate)", getters(vis_all = "pub"), setters(skip))]
#[derive(Debug, Clone, Copy)]
pub struct RelayMessageRef<'a, I, A> {
  /// The node
  #[viewit(getter(style = "ref", attrs(doc = "Get the node to relay to")))]
  node: Node<I, A>,
  /// The offset of the payload to the original buffer
  #[viewit(getter(
    style = "move",
    attrs(doc = "Get the offset of the payload to the original buffer")
  ))]
  payload_offset: usize,
  /// The relay message payload
  #[viewit(getter(style = "move", attrs(doc = "Get the relay message payload")))]
  payload: &'a [u8],
}

/// A reference to a message.
#[derive(Debug, derive_more::IsVariant, derive_more::Unwrap, derive_more::TryUnwrap)]
#[unwrap(ref)]
#[try_unwrap(ref)]
#[non_exhaustive]
pub enum MessageRef<'a, I, A> {
  /// Leave message
  Leave(LeaveMessage<I>),
  /// Join message
  Join(JoinMessage<I>),
  /// PushPull message
  PushPull(PushPullMessageRef<'a, I>),
  /// UserEvent message
  UserEvent(UserEventMessageRef<'a>),
  /// Query message
  Query(QueryMessageRef<'a, I, A>),
  /// QueryResponse message
  QueryResponse(QueryResponseMessageRef<'a, I, A>),
  /// ConflictResponse message
  ConflictResponse(ConflictResponseMessageRef<'a, I, A>),
  /// Relay message
  Relay(RelayMessageRef<'a, I, A>),
  #[cfg(feature = "encryption")]
  /// KeyRequest message
  KeyRequest(KeyRequestMessage),
  #[cfg(feature = "encryption")]
  /// KeyResponse message
  KeyResponse(KeyResponseMessageRef<'a>),
}

impl<I, A> MessageRef<'_, I, A> {
  /// Returns the message type.
  #[inline]
  pub fn ty(&self) -> MessageType {
    match self {
      Self::Leave(_) => MessageType::Leave,
      Self::Join(_) => MessageType::Join,
      Self::PushPull(_) => MessageType::PushPull,
      Self::UserEvent(_) => MessageType::UserEvent,
      Self::Query(_) => MessageType::Query,
      Self::QueryResponse(_) => MessageType::QueryResponse,
      Self::ConflictResponse(_) => MessageType::ConflictResponse,
      Self::Relay { .. } => MessageType::Relay,
      #[cfg(feature = "encryption")]
      Self::KeyRequest(_) => MessageType::KeyRequest,
      #[cfg(feature = "encryption")]
      Self::KeyResponse(_) => MessageType::KeyResponse,
    }
  }
}

/// Encode a message into a Bytes.
pub fn encode_message_to_bytes<T>(msg: &T) -> Result<Bytes, EncodeError>
where
  T: Encodable,
{
  let len = encoded_message_len(msg);
  let mut buf = vec![0; len];
  encode_message(msg, &mut buf).map(|_| Bytes::from(buf))
}

/// Encode a relay message into a Bytes.
pub fn encode_relay_message_to_bytes<T, I, A>(
  msg: &T,
  node: &Node<I, A>,
) -> Result<Bytes, EncodeError>
where
  T: Encodable,
  I: Data,
  A: Data,
{
  let len = encoded_relay_message_len(msg, node);
  let mut buf = vec![0; len];
  encode_relay_message(msg, node, &mut buf).map(|_| Bytes::from(buf))
}

/// Encode a message into a buffer.
pub fn encode_message<T>(msg: &T, buf: &mut [u8]) -> Result<usize, EncodeError>
where
  T: Encodable,
{
  let mut offset = 0;
  let buf_len = buf.len();
  bail!(msg(offset, buf_len));

  buf[offset] = T::ID;
  offset += 1;

  let encoded_len = msg.encoded_len();
  if encoded_len > u32::MAX as usize {
    return Err(EncodeError::TooLarge);
  }

  offset += (encoded_len as u32)
    .encode(&mut buf[offset..])
    .map_err(|e| e.update(encoded_message_len(msg), buf_len))?;

  offset += msg
    .encode(&mut buf[offset..])
    .map_err(|e| e.update(encoded_message_len(msg), buf_len))?;

  #[cfg(debug_assertions)]
  {
    struct Message<T>(core::marker::PhantomData<T>);
    super::debug_assert_write_eq::<Message<T>>(offset, encoded_message_len(msg));
  }

  Ok(offset)
}

/// Encode a relay message into a buffer.
pub fn encode_relay_message<T, I, A>(
  msg: &T,
  node: &Node<I, A>,
  buf: &mut [u8],
) -> Result<usize, EncodeError>
where
  T: Encodable,
  I: Data,
  A: Data,
{
  let mut offset = 0;
  let buf_len = buf.len();
  bail!(@relay msg(offset, buf_len, node));

  buf[offset] = RELAY_MESSAGE_BYTE;
  offset += 1;

  bail!(@relay msg(offset, buf_len, node));
  buf[offset] = RELAY_NODE_BYTE;
  offset += 1;
  offset += node
    .encode_length_delimited(&mut buf[offset..])
    .map_err(|e| e.update(encoded_relay_message_len(msg, node), buf_len))?;

  bail!(@relay msg(offset, buf_len, node));
  buf[offset] = RELAY_MSG_BYTE;
  offset += 1;

  bail!(@relay msg(offset, buf_len, node));
  buf[offset] = T::ID;
  offset += 1;

  let encoded_len = msg.encoded_len();
  if encoded_len > u32::MAX as usize {
    return Err(EncodeError::TooLarge);
  }

  offset += (encoded_len as u32)
    .encode(&mut buf[offset..])
    .map_err(|e| e.update(encoded_relay_message_len(msg, node), buf_len))?;
  offset += msg
    .encode(&mut buf[offset..])
    .map_err(|e| e.update(encoded_relay_message_len(msg, node), buf_len))?;

  #[cfg(debug_assertions)]
  {
    struct Message<T>(core::marker::PhantomData<T>);
    super::debug_assert_write_eq::<Message<T>>(offset, encoded_relay_message_len(msg, node));
  }

  Ok(offset)
}

/// Returns the encoded length of a message.
pub fn encoded_message_len<T>(msg: &T) -> usize
where
  T: Encodable,
{
  let encoded_len = msg.encoded_len();
  1 + (encoded_len as u32).encoded_len() + encoded_len
}

/// Returns the encoded length of the relay message.
pub fn encoded_relay_message_len<T, I, A>(msg: &T, node: &Node<I, A>) -> usize
where
  T: Encodable,
  I: Data,
  A: Data,
{
  1 + 1 + node.encoded_len_with_length_delimited() + 1 + {
    let encoded_len = msg.encoded_len();
    1 + (encoded_len as u32).encoded_len() + encoded_len
  }
}

/// Decode a message from a buffer.
pub fn decode_message<I, A>(
  buf: &[u8],
) -> Result<MessageRef<'_, I::Ref<'_>, A::Ref<'_>>, DecodeError>
where
  I: Data + Eq + core::hash::Hash,
  A: Data,
{
  let mut offset = 0;
  let buf_len = buf.len();
  let mut msg = None;

  while offset < buf_len {
    match buf[offset] {
      LEAVE_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            LEAVE_MESSAGE_TAG,
          ));
        }
        offset += 1;

        let (len, val) =
          <LeaveMessage<I::Ref<'_>> as DataRef<'_, LeaveMessage<I>>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        msg = Some(MessageRef::Leave(val));
      }
      JOIN_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            JOIN_MESSAGE_TAG,
          ));
        }

        offset += 1;
        let (len, val) =
          <JoinMessage<I::Ref<'_>> as DataRef<'_, JoinMessage<I>>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        msg = Some(MessageRef::Join(val));
      }
      PUSH_PULL_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            PUSH_PULL_MESSAGE_TAG,
          ));
        }

        offset += 1;
        let (len, val) = <PushPullMessageRef<'_, I::Ref<'_>> as DataRef<'_, PushPullMessage<I>>>::decode_length_delimited(&buf[offset..])?;
        offset += len;
        msg = Some(MessageRef::PushPull(val));
      }
      USER_EVENT_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            USER_EVENT_MESSAGE_TAG,
          ));
        }

        offset += 1;
        let (len, val) =
          <UserEventMessageRef<'_> as DataRef<'_, UserEventMessage>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        msg = Some(MessageRef::UserEvent(val));
      }
      QUERY_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            QUERY_MESSAGE_TAG,
          ));
        }
        offset += 1;
        let (len, val) = <QueryMessageRef<'_, I::Ref<'_>, A::Ref<'_>> as DataRef<
          '_,
          QueryMessage<I, A>,
        >>::decode_length_delimited(&buf[offset..])?;
        offset += len;
        msg = Some(MessageRef::Query(val));
      }
      QUERY_RESPONSE_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            QUERY_RESPONSE_MESSAGE_TAG,
          ));
        }
        offset += 1;
        let (len, val) = <QueryResponseMessageRef<'_, I::Ref<'_>, A::Ref<'_>> as DataRef<
          '_,
          QueryResponseMessage<I, A>,
        >>::decode_length_delimited(&buf[offset..])?;
        offset += len;
        msg = Some(MessageRef::QueryResponse(val));
      }
      CONFLICT_RESPONSE_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            CONFLICT_RESPONSE_MESSAGE_TAG,
          ));
        }
        offset += 1;
        let (len, val) = <ConflictResponseMessageRef<'_, I::Ref<'_>, A::Ref<'_>> as DataRef<
          '_,
          ConflictResponseMessage<I, A>,
        >>::decode_length_delimited(&buf[offset..])?;
        offset += len;
        msg = Some(MessageRef::ConflictResponse(val));
      }
      RELAY_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            RELAY_MESSAGE_TAG,
          ));
        }
        offset += 1;
        let (readed, (node, payload)) = decode_relay::<I, A>(&buf[offset..])?;
        offset += readed;
        msg = Some(MessageRef::Relay(RelayMessageRef {
          node,
          payload,
          payload_offset: offset - payload.len(),
        }));
      }
      #[cfg(feature = "encryption")]
      KEY_REQUEST_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            KEY_REQUEST_MESSAGE_TAG,
          ));
        }

        offset += 1;
        let (len, val) =
          <KeyRequestMessage as DataRef<'_, KeyRequestMessage>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        msg = Some(MessageRef::KeyRequest(val));
      }
      #[cfg(feature = "encryption")]
      KEY_RESPONSE_MESSAGE_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "Message",
            "value",
            KEY_RESPONSE_MESSAGE_TAG,
          ));
        }

        offset += 1;
        let (len, val) =
          <KeyResponseMessageRef<'_> as DataRef<'_, KeyResponseMessage>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        msg = Some(MessageRef::KeyResponse(val));
      }
      _ => offset += skip("Message", &buf[offset..])?,
    }
  }

  let msg = msg.ok_or(DecodeError::missing_field("Message", "value"))?;
  Ok(msg)
}

fn decode_relay<I, A>(
  buf: &[u8],
) -> Result<(usize, (Node<I::Ref<'_>, A::Ref<'_>>, &[u8])), DecodeError>
where
  I: Data,
  A: Data,
{
  let mut offset = 0;
  let buf_len = buf.len();

  let mut node = None;
  let mut msg = None;

  while offset < buf_len {
    match buf[offset] {
      RELAY_NODE_BYTE => {
        if node.is_some() {
          return Err(DecodeError::duplicate_field(
            "RelayMessage",
            "node",
            RELAY_NODE_TAG,
          ));
        }
        offset += 1;

        let (len, val) =
          <Node<I::Ref<'_>, A::Ref<'_>> as DataRef<'_, Node<I, A>>>::decode_length_delimited(
            &buf[offset..],
          )?;
        offset += len;
        node = Some(val);
      }
      RELAY_MSG_BYTE => {
        if msg.is_some() {
          return Err(DecodeError::duplicate_field(
            "RelayMessage",
            "msg",
            RELAY_MSG_TAG,
          ));
        }
        offset += 1;

        // Skip length-delimited field by reading the length and skipping the payload
        if buf[offset..].len() < 2 {
          return Err(DecodeError::buffer_underflow());
        }

        let start_offset = offset;
        let _ = buf[offset];
        offset += 1;

        let (read, length) = <u32 as Data>::decode(&buf[offset..])?;
        offset += read;
        offset += length as usize;
        msg = Some(&buf[start_offset..offset]);
      }
      _ => offset += skip("RelayMessage", &buf[offset..])?,
    }
  }

  let node = node.ok_or(DecodeError::missing_field("RelayMessage", "node"))?;

  Ok((offset, (node, msg.unwrap_or_default())))
}
