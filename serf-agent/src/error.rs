// use serf_core::error::Error;

use serf_core::{delegate::Delegate, transport::Transport};
use smol_str::SmolStr;

#[derive(Debug, thiserror::Error)]
pub enum Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  #[error(transparent)]
  Serf(#[from] serf_core::error::Error<T, D>),
  #[error("event handler has {0} already been registered")]
  DuplicatedEventHandler(SmolStr),
  #[error("queries cannot contain the '{prefix}' prefix", prefix = serf_core::event::INTERNAL_QUERY_PREFIX)]
  InternalQueryPrefix,
}
