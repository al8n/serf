use memberlist_core::{
  CheapClone,
  transport::{Id, Node},
  types::TinyVec,
};
use serf_proto::MessageType;

use crate::{
  coordinate::Coordinate,
  types::{AsMessageRef, Filter, Member, SerfMessage, Tags},
};

use super::{
  DefaultMergeDelegate, Delegate, LpeTransfromDelegate, MergeDelegate, NoopReconnectDelegate,
  ReconnectDelegate, TransformDelegate,
};

/// `CompositeDelegate` is a helpful struct to split the [`Delegate`] into multiple small delegates,
/// so that users do not need to implement full [`Delegate`] when they only want to custom some methods
/// in the [`Delegate`].
pub struct CompositeDelegate<
  I,
  A,
  M = DefaultMergeDelegate<I, A>,
  R = NoopReconnectDelegate<I, A>,
  T = LpeTransfromDelegate<I, A>,
> {
  merge: M,
  reconnect: R,
  transform: T,
  _m: std::marker::PhantomData<(I, A)>,
}

impl<I, A> Default for CompositeDelegate<I, A> {
  fn default() -> Self {
    Self::new()
  }
}

impl<I, A> CompositeDelegate<I, A> {
  /// Returns a new `CompositeDelegate`.
  pub fn new() -> Self {
    Self {
      merge: Default::default(),
      reconnect: Default::default(),
      transform: Default::default(),
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R, T> CompositeDelegate<I, A, M, R, T>
where
  M: MergeDelegate<Id = I, Address = A>,
{
  /// Set the [`MergeDelegate`] for the `CompositeDelegate`.
  pub fn with_merge_delegate<NM>(self, merge: NM) -> CompositeDelegate<I, A, NM, R, T> {
    CompositeDelegate {
      merge,
      reconnect: self.reconnect,
      transform: self.transform,
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R, T> CompositeDelegate<I, A, M, R, T> {
  /// Set the [`ReconnectDelegate`] for the `CompositeDelegate`.
  pub fn with_reconnect_delegate<NR>(self, reconnect: NR) -> CompositeDelegate<I, A, M, NR, T> {
    CompositeDelegate {
      reconnect,
      merge: self.merge,
      transform: self.transform,
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R, T> CompositeDelegate<I, A, M, R, T> {
  /// Set the [`TransformDelegate`] for the `CompositeDelegate`.
  pub fn with_transform_delegate<NT>(self, transform: NT) -> CompositeDelegate<I, A, M, R, NT> {
    CompositeDelegate {
      transform,
      merge: self.merge,
      reconnect: self.reconnect,
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R, T> MergeDelegate for CompositeDelegate<I, A, M, R, T>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
  M: MergeDelegate<Id = I, Address = A>,
  R: Send + Sync + 'static,
  T: Send + Sync + 'static,
{
  type Error = M::Error;

  type Id = M::Id;

  type Address = M::Address;

  async fn notify_merge(
    &self,
    members: TinyVec<Member<Self::Id, Self::Address>>,
  ) -> Result<(), Self::Error> {
    self.merge.notify_merge(members).await
  }
}

impl<I, A, M, R, T> ReconnectDelegate for CompositeDelegate<I, A, M, R, T>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
  M: Send + Sync + 'static,
  R: ReconnectDelegate<Id = I, Address = A>,
  T: Send + Sync + 'static,
{
  type Id = R::Id;

  type Address = R::Address;

  fn reconnect_timeout(
    &self,
    member: &Member<Self::Id, Self::Address>,
    timeout: std::time::Duration,
  ) -> std::time::Duration {
    self.reconnect.reconnect_timeout(member, timeout)
  }
}

impl<I, A, M, R, T> TransformDelegate for CompositeDelegate<I, A, M, R, T>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
  M: Send + Sync + 'static,
  R: Send + Sync + 'static,
  T: TransformDelegate<Id = I, Address = A>,
{
  type Error = T::Error;

  type Id = T::Id;

  type Address = T::Address;

  fn encode_filter(
    filter: &Filter<Self::Id>,
  ) -> Result<memberlist_core::bytes::Bytes, Self::Error> {
    T::encode_filter(filter)
  }

  fn decode_filter(bytes: &[u8]) -> Result<(usize, Filter<Self::Id>), Self::Error> {
    T::decode_filter(bytes)
  }

  fn node_encoded_len(node: &Node<Self::Id, Self::Address>) -> usize {
    T::node_encoded_len(node)
  }

  fn encode_node(
    node: &Node<Self::Id, Self::Address>,
    dst: &mut [u8],
  ) -> Result<usize, Self::Error> {
    T::encode_node(node, dst)
  }

  fn decode_node(
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, Node<Self::Id, Self::Address>), Self::Error> {
    T::decode_node(bytes)
  }

  fn id_encoded_len(id: &Self::Id) -> usize {
    T::id_encoded_len(id)
  }

  fn encode_id(id: &Self::Id, dst: &mut [u8]) -> Result<usize, Self::Error> {
    T::encode_id(id, dst)
  }

  fn decode_id(bytes: &[u8]) -> Result<(usize, Self::Id), Self::Error> {
    T::decode_id(bytes)
  }

  fn address_encoded_len(address: &Self::Address) -> usize {
    T::address_encoded_len(address)
  }

  fn encode_address(address: &Self::Address, dst: &mut [u8]) -> Result<usize, Self::Error> {
    T::encode_address(address, dst)
  }

  fn decode_address(bytes: &[u8]) -> Result<(usize, Self::Address), Self::Error> {
    T::decode_address(bytes)
  }

  fn coordinate_encoded_len(coordinate: &Coordinate) -> usize {
    T::coordinate_encoded_len(coordinate)
  }

  fn encode_coordinate(coordinate: &Coordinate, dst: &mut [u8]) -> Result<usize, Self::Error> {
    T::encode_coordinate(coordinate, dst)
  }

  fn decode_coordinate(bytes: &[u8]) -> Result<(usize, Coordinate), Self::Error> {
    T::decode_coordinate(bytes)
  }

  fn tags_encoded_len(tags: &Tags) -> usize {
    T::tags_encoded_len(tags)
  }

  fn encode_tags(tags: &Tags, dst: &mut [u8]) -> Result<usize, Self::Error> {
    T::encode_tags(tags, dst)
  }

  fn decode_tags(bytes: &[u8]) -> Result<(usize, Tags), Self::Error> {
    T::decode_tags(bytes)
  }

  fn message_encoded_len(msg: impl AsMessageRef<Self::Id, Self::Address>) -> usize {
    T::message_encoded_len(msg)
  }

  fn encode_message(
    msg: impl AsMessageRef<Self::Id, Self::Address>,
    dst: impl AsMut<[u8]>,
  ) -> Result<usize, Self::Error> {
    T::encode_message(msg, dst)
  }

  fn decode_message(
    ty: MessageType,
    bytes: impl AsRef<[u8]>,
  ) -> Result<(usize, SerfMessage<Self::Id, Self::Address>), Self::Error> {
    T::decode_message(ty, bytes)
  }
}

impl<I, A, M, R, T> Delegate for CompositeDelegate<I, A, M, R, T>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
  M: MergeDelegate<Id = I, Address = A>,
  R: ReconnectDelegate<Id = I, Address = A>,
  T: TransformDelegate<Id = I, Address = A>,
{
  type Id = I;

  type Address = A;
}
