//! The [`AnyMessage`] enum — an owned, tag-dispatched serf message.
//!
//! [`AnyMessage`] is the owned counterpart to the legacy `MessageRef<'a>`.
//! It wraps one typed message variant per [`MessageType`] and provides a
//! single encode ([`AnyMessage::encode`]) and decode ([`AnyMessage::decode`])
//! entry-point that accepts a framed `Bytes` buffer, splits the tag byte and
//! body with the crate-internal frame decoder, then delegates to the
//! appropriate bridge function to produce the typed variant.
//!
//! Unknown tag bytes and empty / truncated buffers propagate as [`DecodeError`].

use bytes::Bytes;
use memberlist_proto::Data;

use crate::{
  BridgeError, ConflictResponseMessage, FrameError, JoinMessage, LeaveMessage, MessageType,
  PushPullMessage, QueryMessage, QueryResponseMessage, RelayMessage, UserEventMessage,
  bridge::{
    conflict_response_from_pb, conflict_response_to_pb, join_from_pb, join_to_pb, leave_from_pb,
    leave_to_pb, push_pull_from_pb, push_pull_to_pb, query_from_pb, query_response_from_pb,
    query_response_to_pb, query_to_pb, relay_from_pb, relay_to_pb, user_event_from_pb,
    user_event_to_pb,
  },
  framing::{decode_message, encode_message},
  messages::serf::v1 as pb,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::{
  KeyRequestMessage, KeyResponseMessage,
  bridge::{key_request_from_pb, key_request_to_pb, key_response_from_pb, key_response_to_pb},
};

// ─── DecodeError ──────────────────────────────────────────────────────────────

/// Error returned by [`AnyMessage::decode`].
///
/// Wraps both framing errors (truncated / empty / varint-overflow) and
/// bridge errors (missing required fields, invalid field values).
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecodeError {
  /// The frame is malformed or incomplete.
  #[error("frame error: {0}")]
  Frame(#[from] FrameError),
  /// A field could not be converted from the wire shape.
  #[error("bridge error: {0}")]
  Bridge(#[from] BridgeError),
  /// The buffa body bytes could not be decoded to the expected pb type.
  /// Carries the [`MessageType`] tag byte whose body could not be decoded.
  #[error("buffa decode failed for message type {0}")]
  Buffa(u8),
  /// The tag byte is not recognised by this build (e.g. key messages without
  /// an encryption feature, or a tag from a future protocol version).
  #[error("unknown or unsupported message tag: {0}")]
  UnknownTag(u8),
}

// ─── EncodeError ──────────────────────────────────────────────────────────────

/// Error returned by [`AnyMessage::encode`].
///
/// Wraps bridge conversion errors (e.g. timeout overflow) and framing errors
/// (e.g. body length mismatch).
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum EncodeError {
  /// A field value could not be converted to the wire shape.
  #[error("bridge error: {0}")]
  Bridge(#[from] BridgeError),
  /// The framing encoder rejected the encoded body.
  #[error("frame error: {0}")]
  Frame(#[from] FrameError),
}

// ─── AnyMessage ───────────────────────────────────────────────────────────────

/// An owned serf message, one variant per [`MessageType`].
///
/// Dispatch key: the leading tag byte of the serf plain frame determines the
/// variant. Generic over `I` (node-id type) and `A` (node-address type); both
/// must implement [`Data`] so the bridge layer can encode/decode them from the
/// opaque `bytes` fields in the wire representation.
///
/// # Key-management variants
///
/// `KeyRequest` and `KeyResponse` are available only when the `aes-gcm` or
/// `chacha20-poly1305` feature is enabled.  Frames carrying those tag bytes
/// (9 / 10) decode as [`DecodeError::UnknownTag`] in builds without an
/// encryption backend.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AnyMessage<I, A> {
  /// Leave — node announcing it is leaving the cluster.
  Leave(LeaveMessage<I>),
  /// Join — node joining the cluster.
  Join(JoinMessage<I>),
  /// PushPull — full state-sync exchange.
  PushPull(PushPullMessage<I>),
  /// UserEvent — application-level event broadcast.
  UserEvent(UserEventMessage),
  /// Query — serf RPC query fanout.
  Query(QueryMessage<I, A>),
  /// QueryResponse — response to a Query.
  QueryResponse(QueryResponseMessage<I, A>),
  /// ConflictResponse — tie-breaker for conflicting node names.
  ConflictResponse(ConflictResponseMessage<I, A>),
  /// Relay — message relayed through an intermediary node.
  Relay(RelayMessage<I, A>),
  /// KeyRequest — encryption key management request.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyRequest(KeyRequestMessage),
  /// KeyResponse — encryption key management response.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyResponse(KeyResponseMessage),
}

impl<I, A> AnyMessage<I, A>
where
  I: Data,
  A: Data,
{
  /// Returns the [`MessageType`] tag for this message variant.
  #[cfg(test)]
  pub fn message_type(&self) -> MessageType {
    match self {
      Self::Leave(_) => MessageType::Leave,
      Self::Join(_) => MessageType::Join,
      Self::PushPull(_) => MessageType::PushPull,
      Self::UserEvent(_) => MessageType::UserEvent,
      Self::Query(_) => MessageType::Query,
      Self::QueryResponse(_) => MessageType::QueryResponse,
      Self::ConflictResponse(_) => MessageType::ConflictResponse,
      Self::Relay(_) => MessageType::Relay,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      Self::KeyRequest(_) => MessageType::KeyRequest,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      Self::KeyResponse(_) => MessageType::KeyResponse,
    }
  }

  /// Encode this message into a framed `Bytes` buffer.
  ///
  /// Converts the typed message to its buffa protobuf representation via the
  /// bridge layer, then wraps it in the standard serf plain frame:
  /// `[tag][varint body_len][buffa body]`.
  ///
  /// This is the single public encoding entry-point — raw key bytes never
  /// leave the `SecretKey` type; the bridge converts them internally.
  ///
  /// # Errors
  ///
  /// - [`EncodeError::Bridge`] — a field value could not be mapped to the wire
  ///   shape (e.g. a query timeout exceeding `u64::MAX` nanoseconds).
  /// - [`EncodeError::Frame`] — the buffa body length disagrees between
  ///   `encoded_len` and the actual write (should not happen in practice).
  pub fn encode(&self) -> Result<Bytes, EncodeError>
  where
    I: Data,
    A: Data,
  {
    let frame_vec = match self {
      Self::Leave(m) => {
        let pb = leave_to_pb::<I>(m)?;
        encode_message(MessageType::Leave, &pb)?
      }
      Self::Join(m) => {
        let pb = join_to_pb::<I>(m)?;
        encode_message(MessageType::Join, &pb)?
      }
      Self::PushPull(m) => {
        let pb = push_pull_to_pb::<I>(m)?;
        encode_message(MessageType::PushPull, &pb)?
      }
      Self::UserEvent(m) => {
        let pb = user_event_to_pb(m);
        encode_message(MessageType::UserEvent, &pb)?
      }
      Self::Query(m) => {
        let pb = query_to_pb::<I, A>(m)?;
        encode_message(MessageType::Query, &pb)?
      }
      Self::QueryResponse(m) => {
        let pb = query_response_to_pb::<I, A>(m)?;
        encode_message(MessageType::QueryResponse, &pb)?
      }
      Self::ConflictResponse(m) => {
        let pb = conflict_response_to_pb::<I, A>(m)?;
        encode_message(MessageType::ConflictResponse, &pb)?
      }
      Self::Relay(m) => {
        let pb = relay_to_pb::<I, A>(m)?;
        encode_message(MessageType::Relay, &pb)?
      }
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      Self::KeyRequest(m) => {
        let pb = key_request_to_pb(m);
        encode_message(MessageType::KeyRequest, &pb)?
      }
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      Self::KeyResponse(m) => {
        let pb = key_response_to_pb(m);
        encode_message(MessageType::KeyResponse, &pb)?
      }
    };
    Ok(Bytes::from(frame_vec))
  }

  /// Decode the leading serf frame in `buf` into an [`AnyMessage`], also
  /// returning the number of bytes consumed from `buf`.
  ///
  /// Same as [`AnyMessage::decode`] but surfaces the consumed byte count so
  /// callers can verify that the frame is the only content in the buffer
  /// (exact-consumption check: `consumed == buf.len()` must hold on the gossip
  /// ingress path to reject packets with trailing junk).
  ///
  /// # Errors
  ///
  /// See [`AnyMessage::decode`].
  pub(crate) fn decode_with_consumed(buf: &Bytes) -> Result<(Self, usize), DecodeError>
  where
    I: Data,
    A: Data,
  {
    let (ty, body, consumed) = decode_message(buf)?;
    let msg = Self::decode_body(ty, body)?;
    Ok((msg, consumed))
  }

  /// Decode the leading serf frame in `buf` into an [`AnyMessage`].
  ///
  /// All production decode sites use [`AnyMessage::decode_with_consumed`] for
  /// exact-consumption enforcement; this convenience wrapper is retained for
  /// round-trip tests in `any/tests.rs` and the test adapter in
  /// `endpoint/mod.rs`.
  ///
  /// # Errors
  ///
  /// - [`DecodeError::Frame`] — the buffer is empty, truncated, or has a
  ///   bad varint length prefix.
  /// - [`DecodeError::Buffa`] — the buffa decode of the body bytes failed for
  ///   the identified message type.
  /// - [`DecodeError::Bridge`] — a required field was absent or a field value
  ///   was out of range.
  /// - [`DecodeError::UnknownTag`] — the tag byte is not recognised by this
  ///   build.
  #[cfg(test)]
  pub fn decode(buf: &Bytes) -> Result<Self, DecodeError>
  where
    I: Data,
    A: Data,
  {
    let (ty, body, _consumed) = decode_message(buf)?;
    Self::decode_body(ty, body)
  }

  /// Dispatch-decode a typed body given the tag and the raw body bytes.
  ///
  /// Shared implementation used by both [`AnyMessage::decode`] and
  /// [`AnyMessage::decode_with_consumed`] to avoid duplicating the match arm
  /// logic.
  fn decode_body(ty: MessageType, body: Bytes) -> Result<Self, DecodeError>
  where
    I: Data,
    A: Data,
  {
    use buffa::Message as _;

    match ty {
      MessageType::Leave => {
        let pb = pb::LeaveMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::Leave)))?;
        Ok(Self::Leave(leave_from_pb::<I>(&pb)?))
      }
      MessageType::Join => {
        let pb = pb::JoinMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::Join)))?;
        Ok(Self::Join(join_from_pb::<I>(&pb)?))
      }
      MessageType::PushPull => {
        let pb = pb::PushPullMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::PushPull)))?;
        Ok(Self::PushPull(push_pull_from_pb::<I>(&pb)?))
      }
      MessageType::UserEvent => {
        let pb = pb::UserEventMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::UserEvent)))?;
        Ok(Self::UserEvent(user_event_from_pb(&pb)?))
      }
      MessageType::Query => {
        let pb = pb::QueryMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::Query)))?;
        Ok(Self::Query(query_from_pb::<I, A>(&pb)?))
      }
      MessageType::QueryResponse => {
        let pb = pb::QueryResponseMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::QueryResponse)))?;
        Ok(Self::QueryResponse(query_response_from_pb::<I, A>(&pb)?))
      }
      MessageType::ConflictResponse => {
        let pb = pb::ConflictResponseMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::ConflictResponse)))?;
        Ok(Self::ConflictResponse(conflict_response_from_pb::<I, A>(
          &pb,
        )?))
      }
      MessageType::Relay => {
        let pb = pb::RelayMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::Relay)))?;
        Ok(Self::Relay(relay_from_pb::<I, A>(&pb)?))
      }
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      MessageType::KeyRequest => {
        let pb = pb::KeyRequestMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::KeyRequest)))?;
        Ok(Self::KeyRequest(key_request_from_pb(&pb)?))
      }
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      MessageType::KeyResponse => {
        let pb = pb::KeyResponseMessage::decode_from_slice(body.as_ref())
          .map_err(|_| DecodeError::Buffa(u8::from(MessageType::KeyResponse)))?;
        Ok(Self::KeyResponse(key_response_from_pb(&pb)?))
      }
      MessageType::Unknown(tag) => Err(DecodeError::UnknownTag(tag)),
    }
  }
}

#[cfg(test)]
mod tests;
