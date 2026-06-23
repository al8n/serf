//! Conversion between serf typed message shapes and the buffa-generated codec types.
//!
//! The typed side holds rich Rust types (`LamportTime`, `SmolStr`, `Bytes`);
//! the buffa side stores them as primitive protobuf types. These functions are
//! the single boundary where those conversions happen.

use std::borrow::Cow;

use bytes::Bytes;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use memberlist_proto::SecretKey;
use memberlist_proto::{
  Data, DataRef,
  data::{DecodeError, EncodeError},
};
use smol_str::SmolStr;

#[cfg(test)]
use crate::typed::{Coordinate, Tags};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::typed::{KeyRequestMessage, KeyResponseMessage};
use crate::{
  LamportTime,
  messages::serf::v1 as pb,
  typed::{
    ConflictResponseMessage, Filter, JoinMessage, LeaveMessage, PushPullMessage, QueryFlag,
    QueryMessage, QueryResponseMessage, RelayMessage, TagFilter, UserEvent, UserEventMessage,
    UserEvents,
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
  /// A field value was present but outside the accepted range or domain.
  #[error("invalid field value: {0}")]
  InvalidValue(Cow<'static, str>),
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
pub(crate) fn user_event_to_pb(t: &UserEventMessage) -> pb::UserEventMessage {
  pb::UserEventMessage {
    ltime: Some(t.ltime.into()),
    cc: t.cc,
    name: t.name.to_string(),
    payload: t.payload.clone(),
    ..Default::default()
  }
}

/// Convert `pb::UserEventMessage` → typed [`UserEventMessage`].
pub(crate) fn user_event_from_pb(
  b: &pb::UserEventMessage,
) -> Result<UserEventMessage, BridgeError> {
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
#[cfg(test)]
pub(crate) fn coordinate_to_pb(t: &Coordinate) -> pb::Coordinate {
  pb::Coordinate {
    portion: t.vec.clone(),
    error: t.error,
    adjustment: t.adjustment,
    height: t.height,
    ..Default::default()
  }
}

/// Convert `pb::Coordinate` → typed [`Coordinate`].
#[cfg(test)]
pub(crate) fn coordinate_from_pb(b: &pb::Coordinate) -> Coordinate {
  Coordinate {
    vec: b.portion.clone(),
    error: b.error,
    adjustment: b.adjustment,
    height: b.height,
  }
}

// ─── Tags ─────────────────────────────────────────────────────────────────────

/// Convert typed [`Tags`] → `pb::Tags`.
#[cfg(test)]
pub(crate) fn tags_to_pb(t: &Tags) -> pb::Tags {
  pb::Tags {
    entries: t
      .0
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect(),
    ..Default::default()
  }
}

/// Convert `pb::Tags` → typed [`Tags`].
#[cfg(test)]
pub(crate) fn tags_from_pb(b: &pb::Tags) -> Tags {
  Tags(
    b.entries
      .iter()
      .map(|(k, v)| (SmolStr::from(k.as_str()), SmolStr::from(v.as_str())))
      .collect(),
  )
}

// ─── Filter ──────────────────────────────────────────────────────────────────

/// Convert a typed [`Filter<I>`] → `pb::Filter`.
///
/// The node-id type `I` is encoded as opaque `bytes` via `memberlist_proto::Data`
/// for the `Id` variant; `Tag` variants encode as before.
pub(crate) fn filter_to_pb<I>(t: &Filter<I>) -> Result<pb::Filter, BridgeError>
where
  I: Data,
{
  let kind = match t {
    Filter::Id(ids) => {
      let mut encoded_ids = Vec::with_capacity(ids.len());
      for id in ids {
        encoded_ids.push(data_to_bytes(id)?);
      }
      pb::filter::Kind::NodeIds(Box::new(pb::NodeIdList {
        ids: encoded_ids,
        ..Default::default()
      }))
    }
    Filter::Tag(tf) => pb::filter::Kind::Tag(Box::new(pb::TagFilter {
      tag: tf.tag.to_string(),
      expr: tf.expr.as_deref().map(str::to_owned),
      ..Default::default()
    })),
  };
  Ok(pb::Filter {
    kind: Some(kind),
    ..Default::default()
  })
}

/// Convert `pb::Filter` → typed [`Filter<I>`].
///
/// The `Id` variant decodes each `bytes` entry as `I` via `memberlist_proto::DataRef`.
pub(crate) fn filter_from_pb<I>(b: &pb::Filter) -> Result<Filter<I>, BridgeError>
where
  I: Data,
{
  match b.kind.as_ref() {
    Some(pb::filter::Kind::NodeIds(list)) => {
      let ids = list
        .ids
        .iter()
        .map(|buf| data_from_bytes::<I>(buf))
        .collect::<Result<Vec<I>, BridgeError>>()?;
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
/// Delegates to [`Data::encode_to_bytes`], which allocates a correctly-sized
/// buffer and returns it as `Bytes`. This mirrors the pattern in
/// `memberlist_proto::bridge` for serialising opaque `I`/`A` fields.
fn data_to_bytes<T>(val: &T) -> Result<Bytes, BridgeError>
where
  T: Data,
{
  Ok(val.encode_to_bytes()?)
}

/// Decode a `memberlist_proto::Data` value from raw bytes (no length prefix).
///
/// Accepts any `&[u8]` slice — the caller may pass a `Bytes` ref via
/// `buf.as_ref()` or a plain slice directly.
///
/// Rejects trailing data: the whole slice must be consumed so a malformed
/// wire field is caught at the wire→machine boundary.
fn data_from_bytes<T>(buf: &[u8]) -> Result<T, BridgeError>
where
  T: Data,
{
  let (bytes_read, val) = <T::Ref<'_> as DataRef<'_, T>>::decode(buf)?;
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
pub(crate) fn join_to_pb<I>(t: &JoinMessage<I>) -> Result<pb::JoinMessage, BridgeError>
where
  I: Data,
{
  Ok(pb::JoinMessage {
    ltime: Some(t.ltime.into()),
    id: Some(data_to_bytes(&t.id)?),
    ..Default::default()
  })
}

/// Convert `pb::JoinMessage` → typed [`JoinMessage<I>`].
///
/// Rejects a missing `ltime` and a missing `id` (both required fields). The
/// `id` bytes are decoded via `memberlist_proto::DataRef`.
pub(crate) fn join_from_pb<I>(b: &pb::JoinMessage) -> Result<JoinMessage<I>, BridgeError>
where
  I: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("JoinMessage.ltime".into()))?;
  let id_bytes = b
    .id
    .as_ref()
    .ok_or(BridgeError::MissingField("JoinMessage.id".into()))?;
  let id: I = data_from_bytes(id_bytes)?;
  Ok(JoinMessage {
    ltime: LamportTime::from(ltime),
    id,
  })
}

// ─── LeaveMessage ────────────────────────────────────────────────────────────

/// Convert a typed [`LeaveMessage<I>`] → `pb::LeaveMessage`.
///
/// The node-id `I` is serialised to opaque `bytes` via `memberlist_proto::Data`.
pub(crate) fn leave_to_pb<I>(t: &LeaveMessage<I>) -> Result<pb::LeaveMessage, BridgeError>
where
  I: Data,
{
  Ok(pb::LeaveMessage {
    ltime: Some(t.ltime.into()),
    prune: t.prune,
    id: Some(data_to_bytes(&t.id)?),
    ..Default::default()
  })
}

/// Convert `pb::LeaveMessage` → typed [`LeaveMessage<I>`].
///
/// Rejects a missing `ltime` and a missing `id` (both required fields). The
/// `id` bytes are decoded via `memberlist_proto::DataRef`.
pub(crate) fn leave_from_pb<I>(b: &pb::LeaveMessage) -> Result<LeaveMessage<I>, BridgeError>
where
  I: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("LeaveMessage.ltime".into()))?;
  let id_bytes = b
    .id
    .as_ref()
    .ok_or(BridgeError::MissingField("LeaveMessage.id".into()))?;
  let id: I = data_from_bytes(id_bytes)?;
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
pub(crate) fn conflict_response_to_pb<I, A>(
  t: &ConflictResponseMessage<I, A>,
) -> Result<pb::ConflictResponseMessage, BridgeError>
where
  I: Data,
  A: Data,
{
  Ok(pb::ConflictResponseMessage {
    member: Some(data_to_bytes(&t.member)?),
    ..Default::default()
  })
}

/// Convert `pb::ConflictResponseMessage` → typed [`ConflictResponseMessage<I,A>`].
///
/// Rejects a missing `member` (required field). The `member` bytes are decoded
/// as a `Node<I,A>` via `memberlist_proto::DataRef`.
pub(crate) fn conflict_response_from_pb<I, A>(
  b: &pb::ConflictResponseMessage,
) -> Result<ConflictResponseMessage<I, A>, BridgeError>
where
  I: Data,
  A: Data,
{
  let member_bytes = b.member.as_ref().ok_or(BridgeError::MissingField(
    "ConflictResponseMessage.member".into(),
  ))?;
  let member: memberlist_proto::Node<I, A> = data_from_bytes(member_bytes)?;
  Ok(ConflictResponseMessage { member })
}

// ─── QueryMessage ─────────────────────────────────────────────────────────────

/// Convert a typed [`QueryMessage<I,A>`] → `pb::QueryMessage`.
///
/// - `from: Node<I,A>` is serialised to opaque `bytes` via `memberlist_proto::Data`.
/// - Each `Filter<I>` in `filters` is encoded via [`filter_to_pb`].
/// - `timeout` is stored as nanoseconds in a `uint64`.
/// - `flags` is stored as the raw `u32` bit-pattern.
pub(crate) fn query_to_pb<I, A>(t: &QueryMessage<I, A>) -> Result<pb::QueryMessage, BridgeError>
where
  I: Data,
  A: Data,
{
  let filters = t
    .filters
    .iter()
    .map(|f| filter_to_pb::<I>(f))
    .collect::<Result<Vec<pb::Filter>, BridgeError>>()?;

  let timeout_nanos = u64::try_from(t.timeout.as_nanos()).map_err(|_| {
    BridgeError::InvalidValue("QueryMessage.timeout exceeds u64::MAX nanoseconds".into())
  })?;

  Ok(pb::QueryMessage {
    ltime: Some(t.ltime.into()),
    id: Some(t.id),
    from: Some(data_to_bytes(&t.from)?),
    filters,
    flags: Some(t.flags.bits()),
    relay_factor: Some(t.relay_factor as u32),
    timeout_nanos: Some(timeout_nanos),
    name: t.name.to_string(),
    payload: t.payload.clone(),
    ..Default::default()
  })
}

/// Convert `pb::QueryMessage` → typed [`QueryMessage<I,A>`].
///
/// Rejects missing `ltime`, `id`, `from`, `flags`, `relay_factor`, and
/// `timeout_nanos` (all required by the legacy protocol). Rejects `relay_factor`
/// values that exceed `u8::MAX`. Decodes `from` as `Node<I,A>` and each `Filter`
/// via [`filter_from_pb`].
pub(crate) fn query_from_pb<I, A>(b: &pb::QueryMessage) -> Result<QueryMessage<I, A>, BridgeError>
where
  I: Data,
  A: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("QueryMessage.ltime".into()))?;
  let id = b
    .id
    .ok_or(BridgeError::MissingField("QueryMessage.id".into()))?;
  let from_bytes = b
    .from
    .as_ref()
    .ok_or(BridgeError::MissingField("QueryMessage.from".into()))?;
  let from: memberlist_proto::Node<I, A> = data_from_bytes(from_bytes)?;
  let filters = b
    .filters
    .iter()
    .map(|f| filter_from_pb::<I>(f))
    .collect::<Result<Vec<Filter<I>>, BridgeError>>()?;
  let flags = QueryFlag::from_bits_truncate(
    b.flags
      .ok_or(BridgeError::MissingField("QueryMessage.flags".into()))?,
  );
  let relay_factor = u8::try_from(b.relay_factor.ok_or(BridgeError::MissingField(
    "QueryMessage.relay_factor".into(),
  ))?)
  .map_err(|_| BridgeError::InvalidValue("QueryMessage.relay_factor exceeds u8::MAX".into()))?;
  // Safe: query timeouts are measured in seconds to minutes, well within u64::MAX nanoseconds.
  let timeout = std::time::Duration::from_nanos(b.timeout_nanos.ok_or(
    BridgeError::MissingField("QueryMessage.timeout_nanos".into()),
  )?);

  Ok(QueryMessage {
    ltime: LamportTime::from(ltime),
    id,
    from,
    filters,
    flags,
    relay_factor,
    timeout,
    name: SmolStr::from(b.name.as_str()),
    payload: b.payload.clone(),
  })
}

// ─── QueryResponseMessage ─────────────────────────────────────────────────────

/// Convert a typed [`QueryResponseMessage<I,A>`] → `pb::QueryResponseMessage`.
///
/// - `from: Node<I,A>` is serialised to opaque `bytes` via `memberlist_proto::Data`.
/// - `flags` is stored as the raw `u32` bit-pattern.
pub(crate) fn query_response_to_pb<I, A>(
  t: &QueryResponseMessage<I, A>,
) -> Result<pb::QueryResponseMessage, BridgeError>
where
  I: Data,
  A: Data,
{
  Ok(pb::QueryResponseMessage {
    ltime: Some(t.ltime.into()),
    id: Some(t.id),
    from: Some(data_to_bytes(&t.from)?),
    flags: Some(t.flags.bits()),
    payload: t.payload.clone(),
    ..Default::default()
  })
}

/// Convert `pb::QueryResponseMessage` → typed [`QueryResponseMessage<I,A>`].
///
/// Rejects missing `ltime`, `id`, `from`, and `flags` (all required by the
/// legacy protocol). Decodes `from` as `Node<I,A>` via `memberlist_proto::DataRef`.
/// Flags are decoded with `from_bits_retain` to preserve any future extension bits.
pub(crate) fn query_response_from_pb<I, A>(
  b: &pb::QueryResponseMessage,
) -> Result<QueryResponseMessage<I, A>, BridgeError>
where
  I: Data,
  A: Data,
{
  let ltime = b.ltime.ok_or(BridgeError::MissingField(
    "QueryResponseMessage.ltime".into(),
  ))?;
  let id = b
    .id
    .ok_or(BridgeError::MissingField("QueryResponseMessage.id".into()))?;
  let from_bytes = b.from.as_ref().ok_or(BridgeError::MissingField(
    "QueryResponseMessage.from".into(),
  ))?;
  let from: memberlist_proto::Node<I, A> = data_from_bytes(from_bytes)?;
  let flags = QueryFlag::from_bits_retain(b.flags.ok_or(BridgeError::MissingField(
    "QueryResponseMessage.flags".into(),
  ))?);

  Ok(QueryResponseMessage {
    ltime: LamportTime::from(ltime),
    id,
    from,
    flags,
    payload: b.payload.clone(),
  })
}

// ─── UserEvent ────────────────────────────────────────────────────────────────

/// Convert a typed [`UserEvent`] → `pb::UserEvent`.
pub(crate) fn user_event_single_to_pb(t: &UserEvent) -> pb::UserEvent {
  pb::UserEvent {
    name: t.name.to_string(),
    payload: t.payload.clone(),
    ..Default::default()
  }
}

/// Convert `pb::UserEvent` → typed [`UserEvent`].
pub(crate) fn user_event_single_from_pb(b: &pb::UserEvent) -> UserEvent {
  UserEvent {
    name: SmolStr::from(b.name.as_str()),
    payload: b.payload.clone(),
  }
}

// ─── UserEvents ───────────────────────────────────────────────────────────────

/// Convert a typed [`UserEvents`] → `pb::UserEvents`.
pub(crate) fn user_events_to_pb(t: &UserEvents) -> pb::UserEvents {
  pb::UserEvents {
    ltime: Some(t.ltime.into()),
    events: t.events.iter().map(user_event_single_to_pb).collect(),
    ..Default::default()
  }
}

/// Convert `pb::UserEvents` → typed [`UserEvents`].
///
/// Rejects a missing `ltime` and an empty `events` list — a batch with zero
/// events carries no information and would silently consume buffer history
/// entries. The legacy `serf-core` invariant is `OneOrMore` (at least one event
/// per batch); this decoder enforces the same constraint.
pub(crate) fn user_events_from_pb(b: &pb::UserEvents) -> Result<UserEvents, BridgeError> {
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("UserEvents.ltime".into()))?;
  if b.events.is_empty() {
    return Err(BridgeError::MissingField(
      "UserEvents.events (must be non-empty)".into(),
    ));
  }
  Ok(UserEvents {
    ltime: LamportTime::from(ltime),
    events: b.events.iter().map(user_event_single_from_pb).collect(),
  })
}

// ─── PushPullMessage ─────────────────────────────────────────────────────────

/// Convert a typed [`PushPullMessage<I>`] → `pb::PushPullMessage`.
///
/// - `status_ltimes`: each `(I, LamportTime)` pair is encoded as a
///   `pb::NodeStatusTime` with the node-id in the `id` bytes field.
/// - `left_members`: each `I` is encoded to opaque `bytes` via
///   `memberlist_proto::Data`.
/// - `events`: each [`UserEvents`] batch is encoded via [`user_events_to_pb`].
pub(crate) fn push_pull_to_pb<I>(t: &PushPullMessage<I>) -> Result<pb::PushPullMessage, BridgeError>
where
  I: Data,
{
  let status_ltimes = t
    .status_ltimes
    .iter()
    .map(|(id, ltime)| {
      data_to_bytes(id).map(|id_bytes| pb::NodeStatusTime {
        id: Some(id_bytes),
        ltime: Some((*ltime).into()),
        ..Default::default()
      })
    })
    .collect::<Result<Vec<_>, BridgeError>>()?;

  let left_members = t
    .left_members
    .iter()
    .map(|id| data_to_bytes(id))
    .collect::<Result<Vec<_>, BridgeError>>()?;

  let events = t.events.iter().map(user_events_to_pb).collect();

  Ok(pb::PushPullMessage {
    ltime: Some(t.ltime.into()),
    status_ltimes,
    left_members,
    event_ltime: Some(t.event_ltime.into()),
    events,
    query_ltime: Some(t.query_ltime.into()),
    ..Default::default()
  })
}

/// Convert `pb::PushPullMessage` → typed [`PushPullMessage<I>`].
///
/// Rejects missing `ltime`, `event_ltime`, `query_ltime`, and each
/// `NodeStatusTime.id` / `NodeStatusTime.ltime` (all required by the legacy
/// protocol). Decodes each `NodeStatusTime.id` and each `left_members` entry as
/// `I` via `memberlist_proto::DataRef`.
pub(crate) fn push_pull_from_pb<I>(
  b: &pb::PushPullMessage,
) -> Result<PushPullMessage<I>, BridgeError>
where
  I: Data,
{
  let ltime = b
    .ltime
    .ok_or(BridgeError::MissingField("PushPullMessage.ltime".into()))?;
  let event_ltime = b.event_ltime.ok_or(BridgeError::MissingField(
    "PushPullMessage.event_ltime".into(),
  ))?;
  let query_ltime = b.query_ltime.ok_or(BridgeError::MissingField(
    "PushPullMessage.query_ltime".into(),
  ))?;

  let status_ltimes = b
    .status_ltimes
    .iter()
    .map(|nst| {
      let id_bytes = nst
        .id
        .as_ref()
        .ok_or(BridgeError::MissingField("NodeStatusTime.id".into()))?;
      let id: I = data_from_bytes(id_bytes)?;
      let ltime = nst
        .ltime
        .ok_or(BridgeError::MissingField("NodeStatusTime.ltime".into()))?;
      Ok((id, LamportTime::from(ltime)))
    })
    .collect::<Result<Vec<(I, LamportTime)>, BridgeError>>()?;

  let left_members = b
    .left_members
    .iter()
    .map(|buf| data_from_bytes::<I>(buf))
    .collect::<Result<Vec<I>, BridgeError>>()?;

  let events = b
    .events
    .iter()
    .map(user_events_from_pb)
    .collect::<Result<Vec<UserEvents>, BridgeError>>()?;

  Ok(PushPullMessage {
    ltime: LamportTime::from(ltime),
    status_ltimes,
    left_members,
    event_ltime: LamportTime::from(event_ltime),
    events,
    query_ltime: LamportTime::from(query_ltime),
  })
}

// ─── SecretKey ↔ Bytes helpers ───────────────────────────────────────────────

/// Encode a [`SecretKey`] as `[algorithm_tag][raw_key_bytes]`.
///
/// The leading algorithm tag byte makes the wire encoding self-describing so
/// decoding is unambiguous even when two ciphers share the same key length
/// (e.g. AES-256 and ChaCha20-Poly1305 are both 32 bytes).
///
/// The transient plaintext buffer is zeroed before it is freed so the raw key
/// material does not linger in heap memory after this call returns.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn secret_key_to_bytes(key: &SecretKey) -> Bytes {
  use zeroize::Zeroize as _;
  let raw = key.as_bytes();
  let mut buf = Vec::with_capacity(1 + raw.len());
  buf.push(key.algorithm().tag());
  buf.extend_from_slice(raw);
  let out = Bytes::copy_from_slice(&buf);
  buf.zeroize();
  out
}

/// Decode a [`SecretKey`] from `[algorithm_tag][raw_key_bytes]` wire bytes.
///
/// Returns [`BridgeError::InvalidValue`] when the tag is unknown to this build
/// or the byte count does not match the algorithm's expected key length.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn secret_key_from_bytes(buf: &Bytes) -> Result<SecretKey, BridgeError> {
  if buf.is_empty() {
    return Err(BridgeError::InvalidValue(
      "key bytes must carry at least the algorithm tag".into(),
    ));
  }
  let tag = buf[0];
  let raw = &buf[1..];
  match tag {
    #[cfg(feature = "aes-gcm")]
    1 => {
      // AES-GCM: length determines the AES variant.
      match raw.len() {
        16 => {
          let k: [u8; 16] = raw.try_into().unwrap();
          Ok(SecretKey::Aes128(k))
        }
        24 => {
          let k: [u8; 24] = raw.try_into().unwrap();
          Ok(SecretKey::Aes192(k))
        }
        32 => {
          let k: [u8; 32] = raw.try_into().unwrap();
          Ok(SecretKey::Aes256(k))
        }
        n => Err(BridgeError::InvalidValue(
          format!("AES-GCM key must be 16, 24, or 32 bytes; got {n}").into(),
        )),
      }
    }
    #[cfg(feature = "chacha20-poly1305")]
    2 => {
      // ChaCha20-Poly1305: always 32 bytes.
      if raw.len() != 32 {
        return Err(BridgeError::InvalidValue(
          format!("ChaCha20-Poly1305 key must be 32 bytes; got {}", raw.len()).into(),
        ));
      }
      let k: [u8; 32] = raw.try_into().unwrap();
      Ok(SecretKey::ChaCha20Poly1305(k))
    }
    other => Err(BridgeError::InvalidValue(
      format!("unknown or unsupported algorithm tag {other}").into(),
    )),
  }
}

// ─── KeyRequestMessage ────────────────────────────────────────────────────────

/// Convert a typed [`KeyRequestMessage`] → `pb::KeyRequestMessage`.
///
/// The key (if present) is encoded as `[algorithm_tag][raw_key_bytes]` so the
/// wire encoding is self-describing. Requires the `aes-gcm` or
/// `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
pub(crate) fn key_request_to_pb(t: &KeyRequestMessage) -> pb::KeyRequestMessage {
  pb::KeyRequestMessage {
    key: t.key.as_ref().map(secret_key_to_bytes),
    ..Default::default()
  }
}

/// Convert `pb::KeyRequestMessage` → typed [`KeyRequestMessage`].
///
/// The key bytes (if present) are decoded as `[algorithm_tag][raw_key_bytes]`.
/// Returns [`BridgeError::InvalidValue`] if the bytes are present but malformed.
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
pub(crate) fn key_request_from_pb(
  b: &pb::KeyRequestMessage,
) -> Result<KeyRequestMessage, BridgeError> {
  let key = b.key.as_ref().map(secret_key_from_bytes).transpose()?;
  Ok(KeyRequestMessage { key })
}

// ─── KeyResponseMessage ───────────────────────────────────────────────────────

/// Convert a typed [`KeyResponseMessage`] → `pb::KeyResponseMessage`.
///
/// Each key is encoded as `[algorithm_tag][raw_key_bytes]`. Requires the
/// `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
pub(crate) fn key_response_to_pb(t: &KeyResponseMessage) -> pb::KeyResponseMessage {
  pb::KeyResponseMessage {
    result: t.result,
    message: t.message.to_string(),
    keys: t.keys.iter().map(secret_key_to_bytes).collect(),
    primary_key: t.primary_key.as_ref().map(secret_key_to_bytes),
    ..Default::default()
  }
}

/// Convert `pb::KeyResponseMessage` → typed [`KeyResponseMessage`].
///
/// Each key bytes entry is decoded as `[algorithm_tag][raw_key_bytes]`.
/// Returns [`BridgeError::InvalidValue`] if any entry is malformed. Requires
/// the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
pub(crate) fn key_response_from_pb(
  b: &pb::KeyResponseMessage,
) -> Result<KeyResponseMessage, BridgeError> {
  let keys = b
    .keys
    .iter()
    .map(secret_key_from_bytes)
    .collect::<Result<Vec<SecretKey>, BridgeError>>()?;
  let primary_key = b
    .primary_key
    .as_ref()
    .map(secret_key_from_bytes)
    .transpose()?;
  Ok(KeyResponseMessage {
    result: b.result,
    message: SmolStr::from(b.message.as_str()),
    keys,
    primary_key,
  })
}

// ─── RelayMessage ─────────────────────────────────────────────────────────────

/// Convert a typed [`RelayMessage<I,A>`] → `pb::RelayMessage`.
///
/// The `destination: Node<I,A>` is serialised to opaque `bytes` via
/// `memberlist_proto::Data`. The `payload` bytes are copied verbatim.
pub(crate) fn relay_to_pb<I, A>(t: &RelayMessage<I, A>) -> Result<pb::RelayMessage, BridgeError>
where
  I: Data,
  A: Data,
{
  Ok(pb::RelayMessage {
    destination: Some(data_to_bytes(&t.destination)?),
    payload: t.payload.clone(),
    ..Default::default()
  })
}

/// Convert `pb::RelayMessage` → typed [`RelayMessage<I,A>`].
///
/// Rejects a missing `destination` (required field). Decodes `destination` as
/// `Node<I,A>` via `memberlist_proto::DataRef`. The `payload` bytes are
/// preserved verbatim without parsing.
pub(crate) fn relay_from_pb<I, A>(b: &pb::RelayMessage) -> Result<RelayMessage<I, A>, BridgeError>
where
  I: Data,
  A: Data,
{
  let destination_bytes = b
    .destination
    .as_ref()
    .ok_or(BridgeError::MissingField("RelayMessage.destination".into()))?;
  let destination: memberlist_proto::Node<I, A> = data_from_bytes(destination_bytes)?;
  Ok(RelayMessage {
    destination,
    payload: b.payload.clone(),
  })
}
