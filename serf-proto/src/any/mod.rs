//! The [`AnyMessage`] enum — an owned, tag-dispatched serf message.
//!
//! [`AnyMessage`] is the owned counterpart to the legacy `MessageRef<'a>`.
//! It wraps one typed message variant per [`MessageType`] and provides a
//! single decode entry-point ([`AnyMessage::decode`]) that accepts a framed
//! `Bytes` buffer, uses [`decode_message`] to extract the tag and body, then
//! delegates to the appropriate bridge function to produce the typed variant.
//!
//! Unknown tag bytes and empty / truncated buffers propagate as [`DecodeError`].

use bytes::Bytes;
use memberlist_proto::Data;

use crate::{
  BridgeError,
  ConflictResponseMessage,
  FrameError,
  JoinMessage,
  LeaveMessage,
  MessageType,
  PushPullMessage,
  QueryMessage,
  QueryResponseMessage,
  RelayMessage,
  UserEventMessage,
  bridge::{
    conflict_response_from_pb,
    join_from_pb,
    leave_from_pb,
    push_pull_from_pb,
    query_from_pb,
    query_response_from_pb,
    relay_from_pb,
    user_event_from_pb,
  },
  framing::decode_message,
  messages::serf::v1 as pb,
};
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::{
  KeyRequestMessage,
  KeyResponseMessage,
  bridge::{key_request_from_pb, key_response_from_pb},
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
  #[cfg_attr(docsrs, doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))))]
  KeyRequest(KeyRequestMessage),
  /// KeyResponse — encryption key management response.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))))]
  KeyResponse(KeyResponseMessage),
}

impl<I, A> AnyMessage<I, A>
where
  I: Data,
  A: Data,
{
  /// Returns the [`MessageType`] tag for this message variant.
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

  /// Decode the leading serf frame in `buf` into an [`AnyMessage`].
  ///
  /// Calls [`decode_message`] to split the tag byte and body, then dispatches
  /// on the tag to the appropriate buffa decoder and bridge function.
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
  pub fn decode(buf: &Bytes) -> Result<Self, DecodeError>
  where
    I: Data,
    A: Data,
  {
    use buffa::Message as _;

    let (ty, body, _consumed) = decode_message(buf)?;

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
        Ok(Self::ConflictResponse(conflict_response_from_pb::<I, A>(&pb)?))
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
