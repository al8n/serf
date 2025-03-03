use memberlist_core::proto::{
  CheapClone, Data, DataRef, DecodeError, EncodeError, WireType,
  bytes::Bytes,
  utils::{merge, skip},
};
use smol_str::SmolStr;

use super::super::LamportTime;

/// Used for user-generated events
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Default, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct UserEventMessage {
  /// The lamport time
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the lamport time for this message")
    ),
    setter(
      const,
      attrs(doc = "Sets the lamport time for this message (Builder pattern)")
    )
  )]
  ltime: LamportTime,
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
  /// "Can Coalesce".
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns if this message can be coalesced")
    ),
    setter(
      const,
      attrs(doc = "Sets if this message can be coalesced (Builder pattern)")
    )
  )]
  cc: bool,
}

impl CheapClone for UserEventMessage {
  fn cheap_clone(&self) -> Self {
    Self {
      ltime: self.ltime,
      name: self.name.cheap_clone(),
      payload: self.payload.clone(),
      cc: self.cc,
    }
  }
}

const LTIME_TAG: u8 = 1;
const CC_TAG: u8 = 2;
const NAME_TAG: u8 = 3;
const PAYLOAD_TAG: u8 = 4;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const CC_BYTE: u8 = merge(WireType::Byte, CC_TAG);
const NAME_BYTE: u8 = merge(WireType::LengthDelimited, NAME_TAG);
const PAYLOAD_BYTE: u8 = merge(WireType::LengthDelimited, PAYLOAD_TAG);

/// The reference type of [`UserEventMessage`]
#[viewit::viewit(vis_all = "", getters(vis_all = "pub", style = "ref"), setters(skip))]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct UserEventMessageRef<'a> {
  /// The lamport time
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the lamport time for this message")
  ))]
  ltime: LamportTime,
  /// The name of the event
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the name of the event")))]
  name: &'a str,
  /// The payload of the event
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the payload of the event")))]
  payload: &'a [u8],
  /// "Can Coalesce".
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns if this message can be coalesced")
  ))]
  cc: bool,
}

impl<'a> DataRef<'a, UserEventMessage> for UserEventMessageRef<'a> {
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut ltime = None;
    let mut name = None;
    let mut payload = None;
    let mut cc = None;

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEventMessage",
              "ltime",
              LTIME_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += o;
          ltime = Some(v);
        }
        CC_BYTE => {
          if cc.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEventMessage",
              "cc",
              CC_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <bool as DataRef<'_, bool>>::decode(&buf[offset..])?;
          offset += o;
          cc = Some(v);
        }
        NAME_BYTE => {
          if name.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEventMessage",
              "name",
              NAME_TAG,
            ));
          }
          offset += 1;
          let (o, v) = <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&buf[offset..])?;
          offset += o;
          name = Some(v);
        }
        PAYLOAD_BYTE => {
          if payload.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEventMessage",
              "payload",
              PAYLOAD_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <&[u8] as DataRef<'_, Bytes>>::decode_length_delimited(&buf[offset..])?;
          offset += o;
          payload = Some(v);
        }
        _ => offset += skip("UserEventMessage", &buf[offset..])?,
      }
    }

    Ok((
      offset,
      Self {
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("UserEventMessage", "ltime"))?,
        name: name.unwrap_or_default(),
        payload: payload.unwrap_or_default(),
        cc: cc.unwrap_or_default(),
      },
    ))
  }
}

impl Data for UserEventMessage {
  type Ref<'a> = UserEventMessageRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      ltime: val.ltime,
      name: SmolStr::from(val.name),
      payload: Bytes::copy_from_slice(val.payload),
      cc: val.cc,
    })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 1 + self.ltime.encoded_len();
    let nlen = self.name.len();
    if nlen > 0 {
      len += 1 + self.name.encoded_len_with_length_delimited();
    }

    let plen = self.payload.len();
    if plen > 0 {
      len += 1 + self.payload.encoded_len_with_length_delimited();
    }

    if self.cc {
      len += 1 + 1;
    }

    len
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    macro_rules! bail {
      ($this:ident($offset:expr, $len:ident)) => {
        if $offset >= $len {
          return Err(EncodeError::insufficient_buffer(self.encoded_len(), $len));
        }
      };
    }

    let buf_len = buf.len();
    let mut offset = 0;

    bail!(self(offset, buf_len));
    buf[offset] = LTIME_BYTE;
    offset += 1;
    offset += self
      .ltime
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    if self.cc {
      bail!(self(offset, buf_len));
      buf[offset] = CC_BYTE;
      offset += 1;
      bail!(self(offset, buf_len));
      buf[offset] = 1;
      offset += 1;
    }

    if !self.name.is_empty() {
      bail!(self(offset, buf_len));
      buf[offset] = NAME_BYTE;
      offset += 1;
      offset += self.name.encode_length_delimited(&mut buf[offset..])?;
    }

    if !self.payload.is_empty() {
      bail!(self(offset, buf_len));
      buf[offset] = PAYLOAD_BYTE;
      offset += 1;
      offset += self.payload.encode_length_delimited(&mut buf[offset..])?;
    }

    #[cfg(debug_assertions)]
    super::super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}
