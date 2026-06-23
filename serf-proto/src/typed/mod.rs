//! Typed serf message shapes — the canonical in-memory representations.
//!
//! These types are what the serf state machine works with directly. The
//! `bridge` module converts them to/from the buffa-generated codec types.

use bytes::Bytes;
use smol_str::SmolStr;

use crate::LamportTime;

/// A user-generated event broadcast through the serf cluster.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UserEventMessage {
  /// The lamport clock value at the time the event was emitted.
  pub ltime: LamportTime,
  /// "Can Coalesce" — whether the event may be merged with later identical events.
  pub cc: bool,
  /// The event name.
  pub name: SmolStr,
  /// The event payload.
  pub payload: Bytes,
}
