//! The serf wire codec — pure, no-I/O message types shared by the serf driver crates.
//!
//! Depends on `memberlist-proto` for the `Data`/`DataRef` codec primitives; defines serf's
//! own message set and framing on top of them.
#![deny(missing_docs)]

pub use bridge::{
  BridgeError,
  conflict_response_from_pb,
  conflict_response_to_pb,
  coordinate_from_pb,
  coordinate_to_pb,
  filter_from_pb,
  filter_to_pb,
  join_from_pb,
  join_to_pb,
  leave_from_pb,
  leave_to_pb,
  push_pull_from_pb,
  push_pull_to_pb,
  query_from_pb,
  query_response_from_pb,
  query_response_to_pb,
  query_to_pb,
  relay_from_pb,
  relay_to_pb,
  tags_from_pb,
  tags_to_pb,
  user_event_from_pb,
  user_event_to_pb,
  user_event_single_from_pb,
  user_event_single_to_pb,
  user_events_from_pb,
  user_events_to_pb,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))))]
pub use bridge::{key_request_from_pb, key_request_to_pb, key_response_from_pb, key_response_to_pb};
pub use framing::{FrameError, MessageType, decode_message, encode_message};
pub use typed::{
  Coordinate,
  ConflictResponseMessage,
  Filter,
  JoinMessage,
  LeaveMessage,
  PushPullMessage,
  QueryFlag,
  QueryMessage,
  QueryResponseMessage,
  RelayMessage,
  TagFilter,
  Tags,
  UserEvent,
  UserEventMessage,
  UserEvents,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))))]
pub use typed::{KeyRequestMessage, KeyResponseMessage};

/// A lamport logical clock value — a monotonically increasing counter used to
/// order serf events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct LamportTime(pub(crate) u64);

impl core::fmt::Display for LamportTime {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl From<u64> for LamportTime {
  fn from(t: u64) -> Self {
    Self(t)
  }
}

impl From<LamportTime> for u64 {
  fn from(t: LamportTime) -> Self {
    t.0
  }
}

impl LamportTime {
  /// Zero lamport time.
  pub const ZERO: Self = LamportTime(0);

  /// Creates a new `LamportTime` from a `u64`.
  pub const fn new(t: u64) -> Self {
    Self(t)
  }
}

pub mod bridge;
pub mod framing;
pub mod messages;
pub mod typed;
