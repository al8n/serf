use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, WireType,
  utils::{merge, skip, split},
};

use super::LamportTime;

const LTIME_TAG: u8 = 1;
const PRUNE_TAG: u8 = 2;
const ID_TAG: u8 = 3;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const PRUNE_BYTE: u8 = merge(WireType::Byte, PRUNE_TAG);

/// The message broadcasted to signal the intentional to
/// leave.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct LeaveMessage<I> {
  /// The lamport time
  #[viewit(
    getter(const, attrs(doc = "Returns the lamport time for this message")),
    setter(
      const,
      attrs(doc = "Sets the lamport time for this message (Builder pattern)")
    )
  )]
  ltime: LamportTime,
  /// The id of the node
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the node")),
    setter(attrs(doc = "Sets the node (Builder pattern)"))
  )]
  id: I,

  /// If prune or not
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns if prune or not")),
    setter(attrs(doc = "Sets prune or not (Builder pattern)"))
  )]
  prune: bool,
}

impl<I> LeaveMessage<I> {
  const fn id_byte() -> u8
  where
    I: Data,
  {
    merge(I::WIRE_TYPE, ID_TAG)
  }
}

impl<'a, I> DataRef<'a, LeaveMessage<I>> for LeaveMessage<I::Ref<'a>>
where
  I: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut ltime = None;
    let mut id = None;
    let mut prune = None;

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "LeaveMessage",
              "ltime",
              LTIME_TAG,
            ));
          }
          offset += 1;

          let (read, value) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += read;
          ltime = Some(value);
        }
        PRUNE_BYTE => {
          if prune.is_some() {
            return Err(DecodeError::duplicate_field(
              "LeaveMessage",
              "prune",
              PRUNE_TAG,
            ));
          }
          offset += 1;

          let (read, value) = <bool as DataRef<'_, bool>>::decode(&buf[offset..])?;
          offset += read;
          prune = Some(value);
        }
        val if val == LeaveMessage::<I>::id_byte() => {
          offset += 1;
          let (read, id_ref) = I::Ref::decode_length_delimited(&buf[offset..])?;
          offset += read;
          id = Some(id_ref);
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += skip(wire_type, &buf[offset..])?;
        }
      }
    }

    Ok((
      offset,
      Self {
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("LeaveMessage", "ltime"))?,
        id: id.ok_or_else(|| DecodeError::missing_field("LeaveMessage", "id"))?,
        prune: prune.unwrap_or_default(),
      },
    ))
  }
}

impl<I> Data for LeaveMessage<I>
where
  I: Data,
{
  type Ref<'a> = LeaveMessage<I::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    I::from_ref(val.id).map(|id| Self {
      ltime: val.ltime,
      id,
      prune: val.prune,
    })
  }

  fn encoded_len(&self) -> usize {
    1 + self.ltime.encoded_len()
      + if self.prune { 1 + 1 } else { 0 }
      + 1
      + self.id.encoded_len_with_length_delimited()
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;
    let buf_len = buf.len();

    if offset >= buf_len {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = LTIME_BYTE;
    offset += 1;
    offset += self
      .ltime
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    if self.prune {
      if offset >= buf_len {
        return Err(EncodeError::insufficient_buffer(
          self.encoded_len(),
          buf_len,
        ));
      }

      buf[offset] = PRUNE_BYTE;
      offset += 1;
      offset += <bool as Data>::encode(&true, &mut buf[offset..])
        .map_err(|e| e.update(self.encoded_len(), buf_len))?;
    }

    if offset >= buf_len {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = Self::id_byte();
    offset += 1;
    offset += self
      .id
      .encode_length_delimited(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}
