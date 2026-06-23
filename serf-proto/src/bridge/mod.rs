//! Conversion between serf typed message shapes and the buffa-generated codec types.
//!
//! The typed side holds rich Rust types (`LamportTime`, `SmolStr`, `Bytes`);
//! the buffa side stores them as primitive protobuf types. These functions are
//! the single boundary where those conversions happen.

use std::borrow::Cow;

use bytes::Bytes;
use memberlist_proto::{Data, DataRef, data::DecodeError, data::EncodeError};
use smol_str::SmolStr;

use crate::{
  LamportTime,
  messages::serf::v1 as pb,
  typed::{
    Coordinate,
    ConflictResponseMessage,
    Filter,
    JoinMessage,
    LeaveMessage,
    TagFilter,
    Tags,
    UserEventMessage,
  },
};

// ─── BridgeError ─────────────────────────────────────────────────────────────

/// Errors that can occur when converting between typed shapes and buffa types.
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
  /// A required field was absent in the wire message.
  #[error("missing required field: {0}")]
  MissingField(Cow<'static, str>),
  /// A `oneof` field held no recognised variant.
  #[error("unknown or missing oneof variant in {0}")]
  UnknownVariant(Cow<'static, str>),
  /// An encode error occurred while serialising a `memberlist_proto::Data` field.
  #[error("encode error: {0}")]
  Encode(#[from] EncodeError),
  /// A decode error occurred while deserialising a `memberlist_proto::Data` field.
  #[error("decode error: {0}")]
  Decode(#[from] DecodeError),
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
    name: SmolStr::from(b.name.as_str()),
    payload: b.payload.clone(),
  })
}

// ─── Coordinate ──────────────────────────────────────────────────────────────

/// Convert a typed [`Coordinate`] → `pb::Coordinate`.
pub fn coordinate_to_pb(t: &Coordinate) -> pb::Coordinate {
  pb::Coordinate {
    portion: t.vec.clone(),
    error: t.error,
    adjustment: t.adjustment,
    height: t.height,
    ..Default::default()
  }
}

/// Convert `pb::Coordinate` → typed [`Coordinate`].
pub fn coordinate_from_pb(b: &pb::Coordinate) -> Coordinate {
  Coordinate {
    vec: b.portion.clone(),
    error: b.error,
    adjustment: b.adjustment,
    height: b.height,
  }
}

// ─── Tags ─────────────────────────────────────────────────────────────────────

/// Convert typed [`Tags`] → `pb::Tags`.
pub fn tags_to_pb(t: &Tags) -> pb::Tags {
  pb::Tags {
    entries: t.0.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    ..Default::default()
  }
}

/// Convert `pb::Tags` → typed [`Tags`].
pub fn tags_from_pb(b: &pb::Tags) -> Tags {
  Tags(
    b.entries
      .iter()
      .map(|(k, v)| (SmolStr::from(k.as_str()), SmolStr::from(v.as_str())))
      .collect(),
  )
}

// ─── Filter ──────────────────────────────────────────────────────────────────

/// Convert a typed [`Filter`] → `pb::Filter`.
pub fn filter_to_pb(t: &Filter) -> pb::Filter {
  let kind = match t {
    Filter::Id(ids) => pb::filter::Kind::NodeIds(Box::new(pb::NodeIdList {
      ids: ids.iter().map(|s| s.to_string()).collect(),
      ..Default::default()
    })),
    Filter::Tag(tf) => pb::filter::Kind::Tag(Box::new(pb::TagFilter {
      tag: tf.tag.to_string(),
      expr: tf.expr.as_deref().map(str::to_owned),
      ..Default::default()
    })),
  };
  pb::Filter {
    kind: Some(kind),
    ..Default::default()
  }
}

/// Convert `pb::Filter` → typed [`Filter`].
pub fn filter_from_pb(b: &pb::Filter) -> Result<Filter, BridgeError> {
  match b.kind.as_ref() {
    Some(pb::filter::Kind::NodeIds(list)) => {
      let ids = list.ids.iter().map(|s| SmolStr::from(s.as_str())).collect();
      Ok(Filter::Id(ids))
    }
    Some(pb::filter::Kind::Tag(tf)) => Ok(Filter::Tag(TagFilter {
      tag: SmolStr::from(tf.tag.as_str()),
      expr: tf.expr.as_deref().map(SmolStr::from),
    })),
    None => Err(BridgeError::UnknownVariant("Filter.kind".into())),
  }
}

// ─── Data ↔ Bytes helpers ────────────────────────────────────────────────────

/// Encode a `memberlist_proto::Data` value to a raw `Bytes` buffer (no length prefix).
///
/// Allocates a buffer sized by `encoded_len`, writes the encoding via
/// `encode`, and wraps it in `Bytes`. This mirrors the pattern in
/// `memberlist_proto::bridge` for serialising opaque `I`/`A` fields.
fn data_to_bytes<T>(val: &T) -> Result<Bytes, BridgeError>
where
  T: Data,
{
  let mut buf = vec![0u8; val.encoded_len()];
  val.encode(&mut buf)?;
  Ok(Bytes::from(buf))
}

/// Decode a `memberlist_proto::Data` value from raw bytes (no length prefix).
///
/// Rejects trailing data: the whole slice must be consumed so a malformed
/// wire field is caught at the wire→machine boundary.
fn data_from_bytes<T>(buf: &Bytes) -> Result<T, BridgeError>
where
  T: Data,
{
  let (bytes_read, val) = <T::Ref<'_> as DataRef<'_, T>>::decode(buf.as_ref())?;
  if bytes_read != buf.len() {
    return Err(BridgeError::Decode(DecodeError::custom(format!(
      "trailing data in encoded field: decoder consumed {bytes_read} of {} bytes",
      buf.len()
    ))));
  }
  Ok(T::from_ref(val)?)
}

// ─── JoinMessage ─────────────────────────────────────────────────────────────

/// Convert a typed [`JoinMessage<I>`] → `pb::JoinMessage`.
///
/// The node-id `I` is serialised to opaque `bytes` via `memberlist_proto::Data`.
pub fn join_to_pb<I>(t: &JoinMessage<I>) -> Result<pb::JoinMessage, BridgeError>
where
  I: Data,
{
  Ok(pb::JoinMessage {
    ltime: Some(t.ltime.into()),
    id: data_to_bytes(&t.id)?,
    ..Default::default()
  })
}

/// Convert `pb::JoinMessage` → typed [`JoinMessage<I>`].
///
/// Rejects a missing `ltime` (required field). The `id` bytes are decoded via
/// `memberlist_proto::DataRef`.
pub fn join_from_pb<I>(b: &pb::JoinMessage) -> Result<JoinMessage<I>, BridgeError>
where
  I: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("JoinMessage.ltime".into()))?;
  let id: I = data_from_bytes(&b.id)?;
  Ok(JoinMessage {
    ltime: LamportTime::from(ltime),
    id,
  })
}

// ─── LeaveMessage ────────────────────────────────────────────────────────────

/// Convert a typed [`LeaveMessage<I>`] → `pb::LeaveMessage`.
///
/// The node-id `I` is serialised to opaque `bytes` via `memberlist_proto::Data`.
pub fn leave_to_pb<I>(t: &LeaveMessage<I>) -> Result<pb::LeaveMessage, BridgeError>
where
  I: Data,
{
  Ok(pb::LeaveMessage {
    ltime: Some(t.ltime.into()),
    prune: t.prune,
    id: data_to_bytes(&t.id)?,
    ..Default::default()
  })
}

/// Convert `pb::LeaveMessage` → typed [`LeaveMessage<I>`].
///
/// Rejects a missing `ltime` (required field). The `id` bytes are decoded via
/// `memberlist_proto::DataRef`.
pub fn leave_from_pb<I>(b: &pb::LeaveMessage) -> Result<LeaveMessage<I>, BridgeError>
where
  I: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("LeaveMessage.ltime".into()))?;
  let id: I = data_from_bytes(&b.id)?;
  Ok(LeaveMessage {
    ltime: LamportTime::from(ltime),
    id,
    prune: b.prune,
  })
}

// ─── ConflictResponseMessage ─────────────────────────────────────────────────

/// Convert a typed [`ConflictResponseMessage<I,A>`] → `pb::ConflictResponseMessage`.
///
/// The `Node<I,A>` member is serialised to opaque `bytes` via
/// `memberlist_proto::Data`.
pub fn conflict_response_to_pb<I, A>(
  t: &ConflictResponseMessage<I, A>,
) -> Result<pb::ConflictResponseMessage, BridgeError>
where
  I: Data,
  A: Data,
{
  Ok(pb::ConflictResponseMessage {
    member: data_to_bytes(&t.member)?,
    ..Default::default()
  })
}

/// Convert `pb::ConflictResponseMessage` → typed [`ConflictResponseMessage<I,A>`].
///
/// The `member` bytes are decoded as a `Node<I,A>` via `memberlist_proto::DataRef`.
pub fn conflict_response_from_pb<I, A>(
  b: &pb::ConflictResponseMessage,
) -> Result<ConflictResponseMessage<I, A>, BridgeError>
where
  I: Data,
  A: Data,
{
  let member: memberlist_proto::Node<I, A> = data_from_bytes(&b.member)?;
  Ok(ConflictResponseMessage { member })
}
