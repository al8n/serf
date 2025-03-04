use memberlist_core::{CheapClone, transport::Id};

use crate::types::Member;

use super::{
  DefaultMergeDelegate, Delegate, MergeDelegate, NoopReconnectDelegate, ReconnectDelegate,
};

use std::sync::Arc;

/// `CompositeDelegate` is a helpful struct to split the [`Delegate`] into multiple small delegates,
/// so that users do not need to implement full [`Delegate`] when they only want to custom some methods
/// in the [`Delegate`].
pub struct CompositeDelegate<I, A, M = DefaultMergeDelegate<I, A>, R = NoopReconnectDelegate<I, A>>
{
  merge: M,
  reconnect: R,
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
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R> CompositeDelegate<I, A, M, R>
where
  M: MergeDelegate<Id = I, Address = A>,
{
  /// Set the [`MergeDelegate`] for the `CompositeDelegate`.
  pub fn with_merge_delegate<NM>(self, merge: NM) -> CompositeDelegate<I, A, NM, R> {
    CompositeDelegate {
      merge,
      reconnect: self.reconnect,
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R> CompositeDelegate<I, A, M, R> {
  /// Set the [`ReconnectDelegate`] for the `CompositeDelegate`.
  pub fn with_reconnect_delegate<NR>(self, reconnect: NR) -> CompositeDelegate<I, A, M, NR> {
    CompositeDelegate {
      reconnect,
      merge: self.merge,
      _m: std::marker::PhantomData,
    }
  }
}

impl<I, A, M, R> MergeDelegate for CompositeDelegate<I, A, M, R>
where
  I: Id + Send + Sync + 'static,
  A: CheapClone + Send + Sync + 'static,
  M: MergeDelegate<Id = I, Address = A>,
  R: Send + Sync + 'static,
{
  type Error = M::Error;

  type Id = M::Id;

  type Address = M::Address;

  async fn notify_merge(
    &self,
    members: Arc<[Member<Self::Id, Self::Address>]>,
  ) -> Result<(), Self::Error> {
    self.merge.notify_merge(members).await
  }
}

impl<I, A, M, R> ReconnectDelegate for CompositeDelegate<I, A, M, R>
where
  I: Id + Send + Sync + 'static,
  A: CheapClone + Send + Sync + 'static,
  M: Send + Sync + 'static,
  R: ReconnectDelegate<Id = I, Address = A>,
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

impl<I, A, M, R> Delegate for CompositeDelegate<I, A, M, R>
where
  I: Id + Send + Sync + 'static,
  A: CheapClone + Send + Sync + 'static,
  M: MergeDelegate<Id = I, Address = A>,
  R: ReconnectDelegate<Id = I, Address = A>,
{
  type Id = I;

  type Address = A;
}
