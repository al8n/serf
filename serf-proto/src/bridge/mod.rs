//! Conversion between serf typed message shapes and the buffa-generated codec types.
//!
//! The typed side holds rich Rust types (`LamportTime`, `SmolStr`, `Bytes`);
//! the buffa side stores them as primitive protobuf types. These functions are
//! the single boundary where those conversions happen.

use std::borrow::Cow;

use crate::{
  LamportTime,
  messages::serf::v1 as pb,
  typed::UserEventMessage,
};

// ─── BridgeError ─────────────────────────────────────────────────────────────

/// Errors that can occur when converting between typed shapes and buffa types.
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
  /// A required field was absent in the wire message.
  #[error("missing required field: {0}")]
  MissingField(Cow<'static, str>),
}

// ─── UserEventMessage ────────────────────────────────────────────────────────

/// Convert a typed [`UserEventMessage`] → `pb::UserEventMessage`.
pub fn user_event_to_pb(t: &UserEventMessage) -> pb::UserEventMessage {
  pb::UserEventMessage {
    ltime: Some(t.ltime.into()),
    cc: t.cc,
    name: t.name.to_string(),
    payload: t.payload.clone(),
    ..Default::default()
  }
}

/// Convert `pb::UserEventMessage` → typed [`UserEventMessage`].
pub fn user_event_from_pb(b: &pb::UserEventMessage) -> Result<UserEventMessage, BridgeError> {
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("UserEventMessage.ltime".into()))?;
  Ok(UserEventMessage {
    ltime: LamportTime::from(ltime),
    cc: b.cc,
    name: smol_str::SmolStr::from(b.name.as_str()),
    payload: b.payload.clone(),
  })
}
