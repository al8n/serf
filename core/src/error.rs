use std::{collections::HashMap, sync::Arc};

use showbiz_core::{transport::Transport, Address, NodeId};

use crate::{
  delegate::{MergeDelegate, SerfDelegate},
  snapshot::SnapshotError,
  Member,
};

#[derive(Debug)]
pub struct VoidError;

impl std::fmt::Display for VoidError {
  fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    Ok(())
  }
}

impl std::error::Error for VoidError {}

#[derive(thiserror::Error)]
pub enum Error<T: Transport, D: MergeDelegate> {
  #[error("ruserf: {0}")]
  Showbiz(#[from] ShowbizError<T, D>),
  #[error("ruserf: user event size limit exceeds limit of {0} bytes")]
  UserEventLimitTooLarge(usize),
  #[error("ruserf: user event exceeds sane limit of {0} bytes before encoding")]
  UserEventTooLarge(usize),
  #[error("ruserf: can't join after leave or shutdown")]
  BadStatus,
  #[error("ruserf: user event exceeds sane limit of {0} bytes after encoding")]
  RawUserEventTooLarge(usize),
  #[error("ruserf: query exceeds limit of {0} bytes")]
  QueryTooLarge(usize),
  #[error("ruserf: query response is past the deadline")]
  QueryTimeout,
  #[error("ruserf: query response ({got} bytes) exceeds limit of {limit} bytes")]
  QueryResponseTooLarge { limit: usize, got: usize },
  #[error("ruserf: query response already sent")]
  QueryAlreadyResponsed,
  #[error("ruserf: failed to truncate response so that it fits into message")]
  FailTruncateResponse,
  #[error("ruserf: encoded length of tags exceeds limit of {0} bytes")]
  TagsTooLarge(usize),
  #[error("ruserf: relayed response exceeds limit of {0} bytes")]
  RelayedResponseTooLarge(usize),
  #[error("ruserf: relay error\n{0}")]
  Relay(RelayError<T, D>),
  #[error("ruserf: encode error: {0}")]
  Encode(#[from] rmp_serde::encode::Error),
  #[error("ruserf: decode error: {0}")]
  Decode(#[from] rmp_serde::decode::Error),
  #[error("ruserf: failed to deliver query response, dropping")]
  QueryResponseDeliveryFailed,
  #[error("ruserf: coordinates are disabled")]
  CoordinatesDisabled,
  #[error("ruserf: {0}")]
  Snapshot(#[from] SnapshotError),
}

pub struct ShowbizError<T: Transport, D: MergeDelegate>(
  showbiz_core::error::Error<T, SerfDelegate<T, D>>,
);

impl<D: MergeDelegate, T: Transport> From<showbiz_core::error::Error<T, SerfDelegate<T, D>>>
  for ShowbizError<T, D>
{
  fn from(value: showbiz_core::error::Error<T, SerfDelegate<T, D>>) -> Self {
    Self(value)
  }
}

impl<D: MergeDelegate, T: Transport> core::fmt::Display for ShowbizError<T, D> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl<D: MergeDelegate, T: Transport> From<showbiz_core::error::Error<T, SerfDelegate<T, D>>>
  for Error<T, D>
{
  fn from(value: showbiz_core::error::Error<T, SerfDelegate<T, D>>) -> Self {
    Self::Showbiz(ShowbizError(value))
  }
}

pub struct RelayError<T: Transport, D: MergeDelegate>(
  #[allow(clippy::type_complexity)]
  Vec<(
    Arc<Member>,
    showbiz_core::error::Error<T, SerfDelegate<T, D>>,
  )>,
);

impl<D: MergeDelegate, T: Transport>
  From<
    Vec<(
      Arc<Member>,
      showbiz_core::error::Error<T, SerfDelegate<T, D>>,
    )>,
  > for RelayError<T, D>
{
  fn from(
    value: Vec<(
      Arc<Member>,
      showbiz_core::error::Error<T, SerfDelegate<T, D>>,
    )>,
  ) -> Self {
    Self(value)
  }
}

impl<D, T> core::fmt::Display for RelayError<T, D>
where
  D: MergeDelegate,
  T: Transport,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    for (member, err) in self.0.iter() {
      writeln!(
        f,
        "\tfailed to send relay response to {}: {}",
        member.id(),
        err
      )?;
    }
    Ok(())
  }
}

/// `JoinError` is returned when join is partially/totally failed.
pub struct JoinError<T: Transport, D: MergeDelegate> {
  pub(crate) joined: Vec<NodeId>,
  pub(crate) errors: HashMap<Address, Error<T, D>>,
  pub(crate) broadcast_error: Option<Error<T, D>>,
}

impl<D: MergeDelegate, T: Transport> JoinError<T, D> {
  pub const fn broadcast_error(&self) -> Option<&Error<T, D>> {
    self.broadcast_error.as_ref()
  }

  pub const fn errors(&self) -> &HashMap<Address, Error<T, D>> {
    &self.errors
  }

  pub const fn joined(&self) -> &Vec<NodeId> {
    &self.joined
  }

  pub fn num_joined(&self) -> usize {
    self.joined.len()
  }
}

impl<D: MergeDelegate, T: Transport> core::fmt::Debug for JoinError<T, D> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self)
  }
}

impl<D: MergeDelegate, T: Transport> core::fmt::Display for JoinError<T, D> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    if !self.joined.is_empty() {
      writeln!(f, "Successes: {:?}", self.joined)?;
    }

    if !self.errors.is_empty() {
      writeln!(f, "Failures:")?;
      for (address, err) in self.errors.iter() {
        writeln!(f, "\t{}: {}", address, err)?;
      }
    }

    if let Some(err) = &self.broadcast_error {
      writeln!(f, "Broadcast Error: {err}")?;
    }
    Ok(())
  }
}

impl<D: MergeDelegate, T: Transport> std::error::Error for JoinError<T, D> {}
