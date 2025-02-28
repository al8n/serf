use std::sync::Arc;

use memberlist_core::{
  delegate::DelegateError as MemberlistDelegateError, proto::TinyVec, transport::Transport,
};

use crate::{
  delegate::{Delegate, MergeDelegate},
  serf::{SerfDelegate, SerfState},
  types::Member,
};

pub use crate::snapshot::SnapshotError;

/// Error trait for [`Delegate`]
#[derive(thiserror::Error)]
pub enum SerfDelegateError<D: Delegate> {
  /// Serf error
  #[error(transparent)]
  Serf(#[from] SerfError),
  /// [`MergeDelegate`] error
  #[error(transparent)]
  Merge(<D as MergeDelegate>::Error),
}

impl<D: Delegate> core::fmt::Debug for SerfDelegateError<D> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Merge(err) => write!(f, "{err:?}"),
      Self::Serf(err) => write!(f, "{err:?}"),
    }
  }
}

impl<D: Delegate> SerfDelegateError<D> {
  /// Create a delegate error from a merge delegate error.
  #[inline]
  pub const fn merge(err: <D as MergeDelegate>::Error) -> Self {
    Self::Merge(err)
  }

  /// Create a delegate error from a serf error.
  #[inline]
  pub const fn serf(err: crate::error::SerfError) -> Self {
    Self::Serf(err)
  }
}

impl<T, D> From<MemberlistDelegateError<SerfDelegate<T, D>>> for SerfDelegateError<D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn from(value: MemberlistDelegateError<SerfDelegate<T, D>>) -> Self {
    match value {
      MemberlistDelegateError::AliveDelegate(e) => e,
      MemberlistDelegateError::MergeDelegate(e) => e,
    }
  }
}

/// Error type for the serf crate.
#[derive(thiserror::Error)]
pub enum Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  /// Returned when the underlyhing memberlist error
  #[error(transparent)]
  Memberlist(#[from] memberlist_core::error::Error<T, SerfDelegate<T, D>>),
  /// Returned when the serf error
  #[error(transparent)]
  Serf(#[from] SerfError),
  /// Returned when the relay error
  #[error(transparent)]
  Relay(#[from] RelayError<T, D>),
  /// Multiple errors
  #[error("errors:\n{}", format_multiple_errors(.0))]
  Multiple(Arc<[Self]>),
}

impl<T, D> core::fmt::Debug for Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Memberlist(e) => write!(f, "{e:?}"),
      Self::Serf(e) => write!(f, "{e:?}"),
      Self::Relay(e) => write!(f, "{e:?}"),
      Self::Multiple(e) => write!(f, "{e:?}"),
    }
  }
}

impl<T, D> From<SnapshotError> for Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn from(value: SnapshotError) -> Self {
    Self::Serf(SerfError::Snapshot(value))
  }
}

impl<T, D> From<memberlist_core::proto::EncodeError> for Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn from(e: memberlist_core::proto::EncodeError) -> Self {
    Self::Serf(e.into())
  }
}

impl<T, D> From<memberlist_core::proto::DecodeError> for Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn from(e: memberlist_core::proto::DecodeError) -> Self {
    Self::Serf(e.into())
  }
}

impl<T, D> Error<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  /// Create a query response too large error
  #[inline]
  pub const fn query_response_too_large(limit: usize, got: usize) -> Self {
    Self::Serf(SerfError::QueryResponseTooLarge { limit, got })
  }

  /// Create a query timeout error
  #[inline]
  pub const fn query_timeout() -> Self {
    Self::Serf(SerfError::QueryTimeout)
  }

  /// Create a query already response error
  #[inline]
  pub const fn query_already_responsed() -> Self {
    Self::Serf(SerfError::QueryAlreadyResponsed)
  }

  /// Create a query response delivery failed error
  #[inline]
  pub const fn query_response_delivery_failed() -> Self {
    Self::Serf(SerfError::QueryResponseDeliveryFailed)
  }

  /// Create a relayed response too large error
  #[inline]
  pub const fn relayed_response_too_large(size: usize) -> Self {
    Self::Serf(SerfError::RelayedResponseTooLarge(size))
  }

  /// Create a relay error
  #[inline]
  pub const fn relay(err: RelayError<T, D>) -> Self {
    Self::Relay(err)
  }

  /// Create a fail truncate response error
  #[inline]
  pub const fn fail_truncate_response() -> Self {
    Self::Serf(SerfError::FailTruncateResponse)
  }

  /// Create a tags too large error
  #[inline]
  pub const fn tags_too_large(size: usize) -> Self {
    Self::Serf(SerfError::TagsTooLarge(size))
  }

  /// Create a query too large error
  #[inline]
  pub const fn query_too_large(size: usize) -> Self {
    Self::Serf(SerfError::QueryTooLarge(size))
  }

  /// Create a user event limit too large error
  #[inline]
  pub const fn user_event_limit_too_large(size: usize) -> Self {
    Self::Serf(SerfError::UserEventLimitTooLarge(size))
  }

  /// Create a user event limit too large error
  #[inline]
  pub const fn user_event_too_large(size: usize) -> Self {
    Self::Serf(SerfError::UserEventTooLarge(size))
  }

  /// Create a raw user event too large error
  #[inline]
  pub const fn raw_user_event_too_large(size: usize) -> Self {
    Self::Serf(SerfError::RawUserEventTooLarge(size))
  }

  /// Create a broadcast channel closed error
  #[inline]
  pub const fn broadcast_channel_closed() -> Self {
    Self::Serf(SerfError::BroadcastChannelClosed)
  }

  /// Create a removal broadcast timeout error
  #[inline]
  pub const fn removal_broadcast_timeout() -> Self {
    Self::Serf(SerfError::RemovalBroadcastTimeout)
  }

  /// Create a snapshot error
  #[inline]
  pub const fn snapshot(err: SnapshotError) -> Self {
    Self::Serf(SerfError::Snapshot(err))
  }

  /// Create a bad leave status error
  #[inline]
  pub const fn bad_leave_status(status: SerfState) -> Self {
    Self::Serf(SerfError::BadLeaveStatus(status))
  }

  /// Create a bad join status error
  #[inline]
  pub const fn bad_join_status(status: SerfState) -> Self {
    Self::Serf(SerfError::BadJoinStatus(status))
  }

  /// Create a coordinates disabled error
  #[inline]
  pub const fn coordinates_disabled() -> Self {
    Self::Serf(SerfError::CoordinatesDisabled)
  }
}

/// [`Serf`](crate::Serf) error.
#[derive(Debug, thiserror::Error)]
pub enum SerfError {
  /// Returned when the user event exceeds the configured limit.
  #[error("user event exceeds configured limit of {0} bytes before encoding")]
  UserEventLimitTooLarge(usize),
  /// Returned when the user event exceeds the sane limit.
  #[error("user event exceeds sane limit of {0} bytes before encoding")]
  UserEventTooLarge(usize),
  /// Returned when the join status is bad.
  #[error("join called on {0} statues")]
  BadJoinStatus(SerfState),
  /// Returned when the leave status is bad.
  #[error("leave called on {0} statues")]
  BadLeaveStatus(SerfState),
  /// Returned when the encoded user event exceeds the sane limit after encoding.
  #[error("user event exceeds sane limit of {0} bytes after encoding")]
  RawUserEventTooLarge(usize),
  /// Returned when the query size exceeds the configured limit.
  #[error("query exceeds limit of {0} bytes")]
  QueryTooLarge(usize),
  /// Returned when the query is timeout.
  #[error("query response is past the deadline")]
  QueryTimeout,
  /// Returned when the query response is too large.
  #[error("query response ({got} bytes) exceeds limit of {limit} bytes")]
  QueryResponseTooLarge {
    /// The query response size limit.
    limit: usize,
    /// The query response size.
    got: usize,
  },
  /// Returned when the query has already been responded.
  #[error("query response already sent")]
  QueryAlreadyResponsed,
  /// Returned when failed to truncate response so that it fits into message.
  #[error("failed to truncate response so that it fits into message")]
  FailTruncateResponse,
  /// Returned when the tags too large.
  #[error("encoded length of tags exceeds limit of {0} bytes")]
  TagsTooLarge(usize),
  /// Returned when the relayed response is too large.
  #[error("relayed response exceeds limit of {0} bytes")]
  RelayedResponseTooLarge(usize),
  /// Returned when failed to deliver query response, dropping.
  #[error("failed to deliver query response, dropping")]
  QueryResponseDeliveryFailed,
  /// Returned when the coordinates are disabled.
  #[error("coordinates are disabled")]
  CoordinatesDisabled,
  /// Returned when snapshot error.
  #[error(transparent)]
  Snapshot(#[from] SnapshotError),
  /// Returned when trying to decode a serf data
  #[error(transparent)]
  Decode(#[from] memberlist_core::proto::DecodeError),
  /// Returned when trying to encode a serf data
  #[error(transparent)]
  Encode(#[from] memberlist_core::proto::EncodeError),
  /// Returned when timed out broadcasting node removal.
  #[error("timed out broadcasting node removal")]
  RemovalBroadcastTimeout,
  /// Returned when the timed out broadcasting channel closed.
  #[error("timed out broadcasting channel closed")]
  BroadcastChannelClosed,
}

/// Relay error from remote nodes.
pub struct RelayError<T, D>(
  #[allow(clippy::type_complexity)]
  TinyVec<(
    Member<T::Id, T::ResolvedAddress>,
    memberlist_core::error::Error<T, SerfDelegate<T, D>>,
  )>,
)
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport;

impl<T, D>
  From<
    TinyVec<(
      Member<T::Id, T::ResolvedAddress>,
      memberlist_core::error::Error<T, SerfDelegate<T, D>>,
    )>,
  > for RelayError<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn from(
    value: TinyVec<(
      Member<T::Id, T::ResolvedAddress>,
      memberlist_core::error::Error<T, SerfDelegate<T, D>>,
    )>,
  ) -> Self {
    Self(value)
  }
}

impl<T, D> core::fmt::Display for RelayError<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    writeln!(f, "relay errors:")?;

    for (member, err) in self.0.iter() {
      writeln!(
        f,
        "\tfailed to send relay response to {}: {}",
        member.node().id(),
        err
      )?;
    }
    Ok(())
  }
}

impl<T, D> core::fmt::Debug for RelayError<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    core::fmt::Display::fmt(self, f)
  }
}

impl<T, D> std::error::Error for RelayError<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
}

fn format_multiple_errors<T, D>(errors: &[Error<T, D>]) -> String
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  errors
    .iter()
    .enumerate()
    .map(|(i, err)| format!("  {}. {}", i + 1, err))
    .collect::<Vec<_>>()
    .join("\n")
}
