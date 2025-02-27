use memberlist_core::{CheapClone, proto::TinyVec, transport::Id};
use std::future::Future;

use crate::types::Member;

/// Used to involve a client in
/// a potential cluster merge operation. Namely, when
/// a node does a promised push/pull (as part of a join),
/// the delegate is involved and allowed to cancel the join
/// based on custom logic. The merge delegate is NOT invoked
/// as part of the push-pull anti-entropy.
#[auto_impl::auto_impl(Box, Arc)]
pub trait MergeDelegate: Send + Sync + 'static {
  /// The error type of the delegate
  type Error: std::error::Error + Send + Sync + 'static;
  /// The id type of the delegate
  type Id: Id;
  /// The address type of the delegate
  type Address: CheapClone + Send + Sync + 'static;

  /// Invoked when a merge could take place.
  /// Provides a list of the nodes known by the peer. If
  /// the return value is `Err`, the merge is canceled.
  fn notify_merge(
    &self,
    members: TinyVec<Member<Self::Id, Self::Address>>,
  ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// A default implementation of the `MergeDelegate` trait.
#[derive(Debug, Clone, Copy)]
pub struct DefaultMergeDelegate<I, A>(std::marker::PhantomData<(I, A)>);

impl<I, A> Default for DefaultMergeDelegate<I, A> {
  fn default() -> Self {
    Self(Default::default())
  }
}

impl<I, A> MergeDelegate for DefaultMergeDelegate<I, A>
where
  I: Id + Send + Sync + 'static,
  A: CheapClone + Send + Sync + 'static,
{
  type Error = std::convert::Infallible;
  type Id = I;
  type Address = A;

  async fn notify_merge(
    &self,
    _members: TinyVec<Member<Self::Id, Self::Address>>,
  ) -> Result<(), Self::Error> {
    Ok(())
  }
}
