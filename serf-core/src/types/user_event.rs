use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, WireType,
  bytes::Bytes,
  utils::{merge, skip, split},
};
use smol_str::SmolStr;

pub use message::*;
pub use user_events::*;

mod message;
mod user_events;

const NAME_TAG: u8 = 1;
const PAYLOAD_TAG: u8 = 2;

const NAME_BYTE: u8 = merge(WireType::LengthDelimited, NAME_TAG);
const PAYLOAD_BYTE: u8 = merge(WireType::LengthDelimited, PAYLOAD_TAG);

/// Stores all the user events at a specific time
#[viewit::viewit(getters(style = "ref"), setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct UserEvent {
  /// The name of the event
  #[viewit(
    getter(const, attrs(doc = "Returns the name of the event")),
    setter(attrs(doc = "Sets the name of the event (Builder pattern)"))
  )]
  name: SmolStr,
  /// The payload of the event
  #[viewit(
    getter(const, attrs(doc = "Returns the payload of the event")),
    setter(attrs(doc = "Sets the payload of the event (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::into::<Vec<u8>, Bytes>))]
  payload: Bytes,
}

/// The reference to a [`UserEvent`].
#[viewit::viewit(getters(style = "ref", vis_all = "pub"), setters(skip), vis_all = "")]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct UserEventRef<'a> {
  #[viewit(getter(const, attrs(doc = "Returns the name of the event")))]
  name: &'a str,
  #[viewit(getter(const, attrs(doc = "Returns the payload of the event")))]
  payload: &'a [u8],
}

impl<'a> DataRef<'a, UserEvent> for UserEventRef<'a> {
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut name = None;
    let mut payload = None;

    while offset < buf_len {
      match buf[offset] {
        NAME_BYTE => {
          if name.is_some() {
            return Err(DecodeError::duplicate_field("UserEvent", "name", NAME_TAG));
          }
          offset += 1;
          let (size, val) =
            <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&buf[offset..])?;
          name = Some(val);
          offset += size;
        }
        PAYLOAD_BYTE => {
          if payload.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEvent",
              "payload",
              PAYLOAD_TAG,
            ));
          }
          offset += 1;
          let (size, val) = <&[u8] as DataRef<'_, Bytes>>::decode_length_delimited(&buf[offset..])?;
          payload = Some(val);
          offset += size;
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type)
            .map_err(|v| DecodeError::unknown_wire_type("UserEvent", v))?;
          offset += skip(wire_type, &buf[offset..])?;
        }
      }
    }

    Ok((
      offset,
      Self {
        name: name.unwrap_or_default(),
        payload: payload.unwrap_or_default(),
      },
    ))
  }
}

impl Data for UserEvent {
  type Ref<'a> = UserEventRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      name: SmolStr::from(val.name),
      payload: Bytes::copy_from_slice(val.payload),
    })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;
    if !self.name.is_empty() {
      len += 1 + self.name.encoded_len_with_length_delimited();
    }

    if !self.payload.is_empty() {
      len += 1 + self.payload.encoded_len_with_length_delimited();
    }
    len
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;
    let buf_len = buf.len();
    if !self.name.is_empty() {
      if offset >= buf_len {
        return Err(EncodeError::insufficient_buffer(
          self.encoded_len(),
          buf_len,
        ));
      }
      buf[offset] = NAME_BYTE;
      offset += 1;
      offset += self.name.encode_length_delimited(&mut buf[offset..])?;
    }

    if !self.payload.is_empty() {
      if offset >= buf_len {
        return Err(EncodeError::insufficient_buffer(
          self.encoded_len(),
          buf_len,
        ));
      }
      buf[offset] = PAYLOAD_BYTE;
      offset += 1;
      offset += self.payload.encode_length_delimited(&mut buf[offset..])?;
    }

    Ok(offset)
  }
}
