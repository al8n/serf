//! Conversion between serf typed message shapes and the buffa-generated codec types.
//!
//! The typed side holds rich Rust types (`LamportTime`, `SmolStr`, `Bytes`);
//! the buffa side stores them as primitive protobuf types. These functions are
//! the single boundary where those conversions happen.

use std::borrow::Cow;

use smol_str::SmolStr;

use crate::{
  LamportTime,
  messages::serf::v1 as pb,
  typed::{Coordinate, Filter, TagFilter, Tags, UserEventMessage},
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
