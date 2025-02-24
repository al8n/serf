use std::time::Duration;

use memberlist_core::{CheapClone, transport::Id};

use crate::types::Member;

/// Implemented to allow overriding the reconnect timeout for individual members.
#[auto_impl::auto_impl(Box, Arc)]
pub trait ReconnectDelegate: Send + Sync + 'static {
  /// The id type of the delegate
  type Id: Id;
  /// The address type of the delegate
  type Address: CheapClone + Send + Sync + 'static;

  /// Returns the reconnect timeout for the given member.
  fn reconnect_timeout(
    &self,
    member: &Member<Self::Id, Self::Address>,
    timeout: Duration,
  ) -> Duration;
}

/// Noop implementation of `ReconnectDelegate`.
#[derive(Debug)]
pub struct NoopReconnectDelegate<I, A>(std::marker::PhantomData<(I, A)>);

impl<I, A> Default for NoopReconnectDelegate<I, A> {
  fn default() -> Self {
    Self(Default::default())
  }
}

impl<I, A> Clone for NoopReconnectDelegate<I, A> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I, A> Copy for NoopReconnectDelegate<I, A> {}

impl<I, A> ReconnectDelegate for NoopReconnectDelegate<I, A>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
{
  type Id = I;
  type Address = A;

  fn reconnect_timeout(
    &self,
    _member: &Member<Self::Id, Self::Address>,
    timeout: Duration,
  ) -> Duration {
    timeout
  }
}
