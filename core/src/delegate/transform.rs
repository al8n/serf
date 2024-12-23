use memberlist_core::{
  bytes::Bytes,
  transport::{Id, Node, Transformable},
  CheapClone,
};
use serf_types::{
  FilterTransformError, JoinMessage, LeaveMessage, Member, MessageType, NodeTransformError,
  PushPullMessage, QueryMessage, QueryResponseMessage, SerfMessageTransformError,
  TagsTransformError, UserEventMessage,
};

use crate::{
  coordinate::{Coordinate, CoordinateTransformError},
  types::{AsMessageRef, Filter, SerfMessage, Tags, UnknownMessageType},
};

/// A delegate for encoding and decoding.
pub trait TransformDelegate: Send + Sync + 'static {
  /// The error type for the transformation.
  type Error: std::error::Error + From<UnknownMessageType> + Send + Sync + 'static;
  /// The Id type.
  type Id: Id;
  /// The Address type.
  type Address: CheapClone + Send + Sync + 'static;

  /// Encodes the filter into bytes.
  fn encode_filter(filter: &Filter<Self::Id>) -> Result<Bytes, Self::Error>;

  /// Decodes the filter from the given bytes, returning the number of bytes consumed and the filter.
  fn decode_filter(bytes: &[u8]) -> Result<(usize, Filter<Self::Id>), Self::Error>;

  /// Returns the encoded length of the node.
  fn node_encoded_len(node: &Node<Self::Id, Self::Address>) -> usize;

  /// Encodes the node into the given buffer, returning the number of bytes written.
  fn encode_node(
    node: &Node<Self::Id, Self::Address>,
    dst: &mut [u8],
  ) -> Result<usize, Self::Error>;

  /// Decodes [`Node`] from the given bytes, returning the number of bytes consumed and the node.
  fn decode_node(
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, Node<Self::Id, Self::Address>), Self::Error>;

  /// Returns the encoded length of the id.
  fn id_encoded_len(id: &Self::Id) -> usize;

  /// Encodes the id into the given buffer, returning the number of bytes written.
  fn encode_id(id: &Self::Id, dst: &mut [u8]) -> Result<usize, Self::Error>;

  /// Decodes the id from the given bytes, returning the number of bytes consumed and the id.
  fn decode_id(bytes: &[u8]) -> Result<(usize, Self::Id), Self::Error>;

  /// Returns the encoded length of the address.
  fn address_encoded_len(address: &Self::Address) -> usize;

  /// Encodes the address into the given buffer, returning the number of bytes written.
  fn encode_address(address: &Self::Address, dst: &mut [u8]) -> Result<usize, Self::Error>;

  /// Decodes the address from the given bytes, returning the number of bytes consumed and the address.
  fn decode_address(bytes: &[u8]) -> Result<(usize, Self::Address), Self::Error>;

  /// Encoded length of the coordinate.
  fn coordinate_encoded_len(coordinate: &Coordinate) -> usize;

  /// Encodes the coordinate into the given buffer, returning the number of bytes written.
  fn encode_coordinate(coordinate: &Coordinate, dst: &mut [u8]) -> Result<usize, Self::Error>;

  /// Decodes the coordinate from the given bytes, returning the number of bytes consumed and the coordinate.
  fn decode_coordinate(bytes: &[u8]) -> Result<(usize, Coordinate), Self::Error>;

  /// Encoded length of the tags.
  fn tags_encoded_len(tags: &Tags) -> usize;

  /// Encodes the tags into the given buffer, returning the number of bytes written.
  fn encode_tags(tags: &Tags, dst: &mut [u8]) -> Result<usize, Self::Error>;

  /// Decodes the tags from the given bytes, returning the number of bytes consumed and the tags.
  fn decode_tags(bytes: &[u8]) -> Result<(usize, Tags), Self::Error>;

  /// Encoded length of the message.
  fn message_encoded_len(msg: impl AsMessageRef<Self::Id, Self::Address>) -> usize;

  /// Encodes the message into the given buffer, returning the number of bytes written.
  ///
  /// **NOTE**:
  ///
  /// 1. The buffer must be large enough to hold the encoded message.
  ///    The length of the buffer can be obtained by calling [`TransformDelegate::message_encoded_len`].
  /// 2. A message type byte will be automatically prepended to the buffer,
  ///    so users do not need to encode the message type byte by themselves.
  fn encode_message(
    msg: impl AsMessageRef<Self::Id, Self::Address>,
    dst: impl AsMut<[u8]>,
  ) -> Result<usize, Self::Error>;

  /// Decodes the message from the given bytes, returning the number of bytes consumed and the message.
  fn decode_message(
    ty: MessageType,
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, SerfMessage<Self::Id, Self::Address>), Self::Error>;
}

/// The error type for the LPE transformation.
#[derive(thiserror::Error)]
pub enum LpeTransformError<I, A>
where
  I: Transformable + core::hash::Hash + Eq,
  A: Transformable + core::hash::Hash + Eq,
{
  /// Id transformation error.
  #[error(transparent)]
  Id(<I as Transformable>::Error),
  /// Address transformation error.
  #[error(transparent)]
  Address(<A as Transformable>::Error),
  /// Coordinate transformation error.
  #[error(transparent)]
  Coordinate(#[from] CoordinateTransformError),
  /// Node transformation error.
  #[error(transparent)]
  Node(#[from] NodeTransformError<I, A>),
  /// Filter transformation error.
  #[error(transparent)]
  Filter(#[from] FilterTransformError<I>),
  /// Tags transformation error.
  #[error(transparent)]
  Tags(#[from] TagsTransformError),
  /// Serf message transformation error.
  #[error(transparent)]
  Message(#[from] SerfMessageTransformError<I, A>),
  /// Unknown message type error.
  #[error(transparent)]
  UnknownMessage(#[from] UnknownMessageType),
  /// Unexpected relay message.
  #[error("unexpected relay message")]
  UnexpectedRelayMessage,
}

impl<I, A> core::fmt::Debug for LpeTransformError<I, A>
where
  I: Transformable + core::hash::Hash + Eq,
  A: Transformable + core::hash::Hash + Eq,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self)
  }
}

/// A length-prefixed encoding [`TransformDelegate`] implementation
pub struct LpeTransfromDelegate<I, A>(std::marker::PhantomData<(I, A)>);

impl<I, A> Default for LpeTransfromDelegate<I, A> {
  fn default() -> Self {
    Self(Default::default())
  }
}

impl<I, A> Clone for LpeTransfromDelegate<I, A> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I, A> Copy for LpeTransfromDelegate<I, A> {}

impl<I, A> TransformDelegate for LpeTransfromDelegate<I, A>
where
  I: Id,
  A: Transformable + CheapClone + core::hash::Hash + Eq + Send + Sync + 'static,
{
  type Error = LpeTransformError<Self::Id, Self::Address>;
  type Id = I;
  type Address = A;

  fn encode_filter(filter: &Filter<Self::Id>) -> Result<Bytes, Self::Error> {
    filter
      .encode_to_vec()
      .map(Bytes::from)
      .map_err(Self::Error::Filter)
  }

  fn decode_filter(bytes: &[u8]) -> Result<(usize, Filter<Self::Id>), Self::Error> {
    Filter::decode(bytes).map_err(Self::Error::Filter)
  }

  fn node_encoded_len(node: &Node<Self::Id, Self::Address>) -> usize {
    Transformable::encoded_len(node)
  }

  fn encode_node(
    node: &Node<Self::Id, Self::Address>,
    dst: &mut [u8],
  ) -> Result<usize, Self::Error> {
    Transformable::encode(node, dst).map_err(Self::Error::Node)
  }

  fn decode_node(
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, Node<Self::Id, Self::Address>), Self::Error> {
    Transformable::decode(bytes.as_ref()).map_err(Self::Error::Node)
  }

  fn id_encoded_len(id: &Self::Id) -> usize {
    Transformable::encoded_len(id)
  }

  fn encode_id(id: &Self::Id, dst: &mut [u8]) -> Result<usize, Self::Error> {
    Transformable::encode(id, dst).map_err(Self::Error::Id)
  }

  fn decode_id(bytes: &[u8]) -> Result<(usize, Self::Id), Self::Error> {
    Transformable::decode(bytes).map_err(Self::Error::Id)
  }

  fn address_encoded_len(address: &Self::Address) -> usize {
    Transformable::encoded_len(address)
  }

  fn encode_address(address: &Self::Address, dst: &mut [u8]) -> Result<usize, Self::Error> {
    Transformable::encode(address, dst).map_err(Self::Error::Address)
  }

  fn decode_address(bytes: &[u8]) -> Result<(usize, Self::Address), Self::Error> {
    Transformable::decode(bytes).map_err(Self::Error::Address)
  }

  fn coordinate_encoded_len(coordinate: &Coordinate) -> usize {
    Transformable::encoded_len(coordinate)
  }

  fn encode_coordinate(coordinate: &Coordinate, dst: &mut [u8]) -> Result<usize, Self::Error> {
    Transformable::encode(coordinate, dst).map_err(Self::Error::Coordinate)
  }

  fn decode_coordinate(bytes: &[u8]) -> Result<(usize, Coordinate), Self::Error> {
    Transformable::decode(bytes).map_err(Self::Error::Coordinate)
  }

  fn tags_encoded_len(tags: &Tags) -> usize {
    Transformable::encoded_len(tags)
  }

  fn encode_tags(tags: &Tags, dst: &mut [u8]) -> Result<usize, Self::Error> {
    Transformable::encode(tags, dst).map_err(Self::Error::Tags)
  }

  fn decode_tags(bytes: &[u8]) -> Result<(usize, Tags), Self::Error> {
    Transformable::decode(bytes).map_err(Self::Error::Tags)
  }

  fn message_encoded_len(msg: impl AsMessageRef<Self::Id, Self::Address>) -> usize {
    let msg = msg.as_message_ref();
    serf_types::Encodable::encoded_len(&msg)
  }

  fn encode_message(
    msg: impl AsMessageRef<Self::Id, Self::Address>,
    mut dst: impl AsMut<[u8]>,
  ) -> Result<usize, Self::Error> {
    let msg = msg.as_message_ref();
    serf_types::Encodable::encode(&msg, dst.as_mut()).map_err(Into::into)
  }

  fn decode_message(
    ty: MessageType,
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, SerfMessage<Self::Id, Self::Address>), Self::Error> {
    match ty {
      MessageType::Leave => LeaveMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::Leave(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::Join => JoinMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::Join(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::PushPull => PushPullMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::PushPull(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::UserEvent => UserEventMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::UserEvent(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::Query => QueryMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::Query(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::QueryResponse => QueryResponseMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::QueryResponse(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::ConflictResponse => Member::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::ConflictResponse(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      MessageType::Relay => Err(Self::Error::UnexpectedRelayMessage),
      #[cfg(feature = "encryption")]
      MessageType::KeyRequest => serf_types::KeyRequestMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::KeyRequest(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      #[cfg(feature = "encryption")]
      MessageType::KeyResponse => serf_types::KeyResponseMessage::decode(bytes.as_ref())
        .map(|(n, m)| (n, SerfMessage::KeyResponse(m)))
        .map_err(|e| Self::Error::Message(e.into())),
      _ => unreachable!(),
    }
  }
}
