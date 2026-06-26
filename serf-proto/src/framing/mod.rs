//! Plain-frame encoder/decoder: `[TAG_BYTE][VARINT_LEN][BUFFA_BODY]`.
//!
//! # Wire format
//!
//! A serf message on the wire is three contiguous regions:
//!
//! ```text
//! [ 1 byte : MessageType tag ][ LEB128 u32 : body length ][ body bytes ]
//! ```
//!
//! The tag byte is the raw numeric discriminant of [`MessageType`] (e.g.
//! `UserEvent` = `4`). The body is the buffa-encoded protobuf bytes for the
//! concrete message type. This framing is serf-specific and is NOT
//! byte-compatible with the legacy `serf-core` hand-rolled codec (which used
//! the memberlist-proto `merge(WireType::LengthDelimited, TAG)` scheme);
//! serf-proto forms new-wire-only clusters.

use bytes::Bytes;

// ── MessageType ──────────────────────────────────────────────────────────────

/// Tag constants — the raw numeric discriminants used as the envelope byte.
const LEAVE_TAG: u8 = 1;
const JOIN_TAG: u8 = 2;
const PUSH_PULL_TAG: u8 = 3;
const USER_EVENT_TAG: u8 = 4;
const QUERY_TAG: u8 = 5;
const QUERY_RESPONSE_TAG: u8 = 6;
const CONFLICT_RESPONSE_TAG: u8 = 7;
const RELAY_TAG: u8 = 8;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
const KEY_REQUEST_TAG: u8 = 9;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
const KEY_RESPONSE_TAG: u8 = 10;

/// One-byte discriminant that opens every serf message frame.
///
/// Numeric values are identical to the legacy `serf-core` tag constants so
/// that future mixed-version migration tooling can map them trivially.
/// `Unknown(u8)` provides forward compatibility for tag values not yet
/// recognised by this build, including key-management messages when this build
/// was compiled without an encryption backend.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MessageType {
  /// Leave — node announcing it is leaving the cluster.
  Leave,
  /// Join — node joining the cluster.
  Join,
  /// PushPull — full state-sync exchange.
  PushPull,
  /// UserEvent — application-level event broadcast.
  UserEvent,
  /// Query — serf RPC query fanout.
  Query,
  /// QueryResponse — response to a Query.
  QueryResponse,
  /// ConflictResponse — tie-breaker for conflicting node names.
  ConflictResponse,
  /// Relay — message relayed through an intermediary node.
  Relay,
  /// KeyRequest — encryption key management request.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature; without an
  /// encryption backend the tag byte decodes as [`MessageType::Unknown`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyRequest,
  /// KeyResponse — encryption key management response.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature; without an
  /// encryption backend the tag byte decodes as [`MessageType::Unknown`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyResponse,
  /// A tag not recognised by this build — preserved for forward compatibility.
  Unknown(u8),
}

impl From<u8> for MessageType {
  fn from(b: u8) -> Self {
    match b {
      LEAVE_TAG => Self::Leave,
      JOIN_TAG => Self::Join,
      PUSH_PULL_TAG => Self::PushPull,
      USER_EVENT_TAG => Self::UserEvent,
      QUERY_TAG => Self::Query,
      QUERY_RESPONSE_TAG => Self::QueryResponse,
      CONFLICT_RESPONSE_TAG => Self::ConflictResponse,
      RELAY_TAG => Self::Relay,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      KEY_REQUEST_TAG => Self::KeyRequest,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      KEY_RESPONSE_TAG => Self::KeyResponse,
      val => Self::Unknown(val),
    }
  }
}

impl From<MessageType> for u8 {
  fn from(ty: MessageType) -> Self {
    match ty {
      MessageType::Leave => LEAVE_TAG,
      MessageType::Join => JOIN_TAG,
      MessageType::PushPull => PUSH_PULL_TAG,
      MessageType::UserEvent => USER_EVENT_TAG,
      MessageType::Query => QUERY_TAG,
      MessageType::QueryResponse => QUERY_RESPONSE_TAG,
      MessageType::ConflictResponse => CONFLICT_RESPONSE_TAG,
      MessageType::Relay => RELAY_TAG,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      MessageType::KeyRequest => KEY_REQUEST_TAG,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      MessageType::KeyResponse => KEY_RESPONSE_TAG,
      MessageType::Unknown(val) => val,
    }
  }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// The `(available, required)` byte-count pair carried by the
/// `FrameError::Incomplete` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("incomplete frame: {available} bytes available, {required} required")]
pub struct IncompleteFrame {
  available: usize,
  required: usize,
}

impl IncompleteFrame {
  /// Construct an incomplete-frame payload.
  #[inline(always)]
  pub const fn new(available: usize, required: usize) -> Self {
    Self {
      available,
      required,
    }
  }

  /// Bytes available in the buffer.
  #[inline(always)]
  pub const fn available(&self) -> usize {
    self.available
  }

  /// Bytes required to complete the frame.
  #[inline(always)]
  pub const fn required(&self) -> usize {
    self.required
  }
}

/// Errors returned by the serf plain-frame encoder / decoder underlying
/// [`AnyMessage::encode`](crate::AnyMessage::encode) and
/// [`AnyMessage::decode`](crate::AnyMessage::decode).
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum FrameError {
  /// The input buffer is empty; no frame to decode.
  #[error("frame buffer is empty")]
  Empty,
  /// The buffer holds a partial frame — more bytes are required.
  #[error(transparent)]
  Incomplete(IncompleteFrame),
  /// The varint length field overflows a `u32`.
  #[error("varint length overflows u32")]
  VarintOverflow,
  /// The buffa `encoded_len` / actual-write lengths disagree, which would
  /// desynchronize the receiver's length prefix. Carries the actual number of
  /// bytes written by `buffa::Message::encode`.
  #[error(
    "frame encode length mismatch: encoded_len predicted {0} bytes but encode wrote a different count"
  )]
  FrameTooLarge(usize),
  /// The buffa decoder rejected the body bytes.
  #[error("buffa decode error: body bytes could not be decoded")]
  Decode,
}

// ── Varint helpers ───────────────────────────────────────────────────────────

/// Append a LEB128-encoded `u32` to `out`.
fn encode_varint_u32(mut value: u32, out: &mut Vec<u8>) {
  while value >= 0x80 {
    out.push(((value & 0x7f) as u8) | 0x80);
    value >>= 7;
  }
  out.push(value as u8);
}

/// Decode a LEB128 `u32` from the front of `buf`.
///
/// Returns `(value, bytes_consumed)`.
fn decode_varint_u32(buf: &[u8]) -> Result<(u32, usize), FrameError> {
  let mut value: u32 = 0;
  let mut shift: u32 = 0;
  for (i, &byte) in buf.iter().enumerate().take(5) {
    // The 5th byte of a u32 LEB128 can only have 4 significant bits (bits
    // 28–31); a byte larger than 0x0f would set bits 32+ and overflow.
    if i == 4 && byte > 0x0f {
      return Err(FrameError::VarintOverflow);
    }
    value |= u32::from(byte & 0x7f) << shift;
    if byte & 0x80 == 0 {
      return Ok((value, i + 1));
    }
    shift += 7;
  }
  // Fell through without a terminating byte — truncated length prefix.
  Err(FrameError::Incomplete(IncompleteFrame::new(
    buf.len(),
    buf.len() + 1,
  )))
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Encode `msg` into a plain serf frame: `[tag][varint body_len][buffa body]`.
///
/// Returns a freshly allocated `Vec<u8>` containing the complete frame.
/// Fails only when the buffa body length exceeds `u32::MAX` (which buffa's
/// `u32` return type for `encoded_len` already guarantees cannot happen).
pub(crate) fn encode_message<M>(ty: MessageType, msg: &M) -> Result<Vec<u8>, FrameError>
where
  M: buffa::Message,
{
  // buffa::Message::encoded_len() returns u32 — already bounded.
  let body_len_u32: u32 = msg.encoded_len();
  let body_len: usize = body_len_u32 as usize;

  // Pre-size: 1 (tag) + up to 5 (varint) + body.
  let mut out = Vec::with_capacity(1 + 5 + body_len);
  out.push(u8::from(ty));
  encode_varint_u32(body_len_u32, &mut out);
  let body_start = out.len();
  msg.encode(&mut out);

  // Guard against an `encoded_len()` / actual-write disagreement that would
  // desynchronise the receiver's length prefix.
  let written = out.len() - body_start;
  if written != body_len {
    return Err(FrameError::FrameTooLarge(written));
  }

  Ok(out)
}

/// Peek the header of the leading plain frame in `buf` without allocating the body.
///
/// Returns `(MessageType, total_frame_len)` on success, where `total_frame_len`
/// is the full encoded length of the frame (tag byte + varint + body).  The body
/// bytes are NOT extracted — the caller can use this to apply a cheap size gate
/// before doing a full decode.
///
/// # Errors
///
/// Returns the same [`FrameError`] variants as [`decode_message`] — `Empty`,
/// `Incomplete`, or `VarintOverflow`.
pub(crate) fn peek_frame_header(buf: &[u8]) -> Result<(MessageType, usize), FrameError> {
  if buf.is_empty() {
    return Err(FrameError::Empty);
  }

  let ty = MessageType::from(buf[0]);

  let (body_len, varint_bytes) = match decode_varint_u32(&buf[1..]) {
    Ok(v) => v,
    Err(FrameError::Incomplete(_)) => {
      return Err(FrameError::Incomplete(IncompleteFrame::new(
        buf.len(),
        buf.len() + 1,
      )));
    }
    Err(e) => return Err(e),
  };

  let header_len = 1 + varint_bytes;
  let frame_end = header_len
    .checked_add(body_len as usize)
    .ok_or(FrameError::VarintOverflow)?;

  Ok((ty, frame_end))
}

/// Decode the leading plain frame from `buf`.
///
/// Returns `(MessageType, body_bytes, bytes_consumed)` on success.
/// `body_bytes` is a zero-copy sub-slice of the input [`Bytes`]; the caller
/// is responsible for decoding it to the appropriate concrete type with
/// `buffa::Message::decode` / `decode_from_slice`.
///
/// `bytes_consumed` is the total number of bytes read (tag + varint + body),
/// allowing a streaming caller to advance its read cursor.
pub(crate) fn decode_message(frame: &Bytes) -> Result<(MessageType, Bytes, usize), FrameError> {
  let buf = frame.as_ref();
  if buf.is_empty() {
    return Err(FrameError::Empty);
  }

  let ty = MessageType::from(buf[0]);

  let (body_len, varint_bytes) = match decode_varint_u32(&buf[1..]) {
    Ok(v) => v,
    Err(FrameError::Incomplete(_)) => {
      return Err(FrameError::Incomplete(IncompleteFrame::new(
        buf.len(),
        buf.len() + 1,
      )));
    }
    Err(e) => return Err(e),
  };

  let header_len = 1 + varint_bytes;
  let frame_end = header_len
    .checked_add(body_len as usize)
    .ok_or(FrameError::VarintOverflow)?;

  if buf.len() < frame_end {
    return Err(FrameError::Incomplete(IncompleteFrame::new(
      buf.len(),
      frame_end,
    )));
  }

  // Zero-copy slice of the body out of the `Bytes` allocation.
  let body = frame.slice(header_len..frame_end);
  Ok((ty, body, frame_end))
}

#[cfg(test)]
mod tests;
