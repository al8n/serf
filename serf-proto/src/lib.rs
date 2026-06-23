//! The serf wire codec — pure, no-I/O message types shared by the serf driver crates.
//!
//! Depends on `memberlist-proto` for the `Data`/`DataRef` codec primitives; defines serf's
//! own message set and framing on top of them.
#![deny(missing_docs)]

pub use any::{AnyMessage, DecodeError, EncodeError};
pub use bridge::BridgeError;
pub use framing::{FrameError, IncompleteFrame, MessageType};
pub use typed::{
  ConflictResponseMessage, Coordinate, Filter, JoinMessage, LeaveMessage, PushPullMessage,
  QueryFlag, QueryMessage, QueryResponseMessage, RelayMessage, TagFilter, Tags, UserEvent,
  UserEventMessage, UserEvents,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
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

pub mod any;
pub(crate) mod bridge;
pub(crate) mod framing;
pub(crate) mod messages;
pub mod typed;
