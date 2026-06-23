//! The serf wire codec — pure, no-I/O message types shared by the serf driver crates.
//!
//! Depends on `memberlist-proto` for the `Data`/`DataRef` codec primitives; defines serf's
//! own message set and framing on top of them.
#![deny(missing_docs)]

pub use bridge::{
  BridgeError,
  coordinate_from_pb,
  coordinate_to_pb,
  filter_from_pb,
  filter_to_pb,
  tags_from_pb,
  tags_to_pb,
  user_event_from_pb,
  user_event_to_pb,
};
pub use framing::{FrameError, MessageType, decode_message, encode_message};
pub use typed::{Coordinate, Filter, QueryFlag, TagFilter, Tags, UserEventMessage};

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
