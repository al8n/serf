use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, WireType,
  utils::{merge, skip, split},
};

use super::LamportTime;

const LTIME_TAG: u8 = 1;
const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const ID_TAG: u8 = 2;

/// The message broadcasted after we join to
/// associated the node with a lamport clock
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct JoinMessage<I> {
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
}

impl<I> JoinMessage<I> {
  /// Create a new join message
  pub fn new(ltime: LamportTime, id: I) -> Self {
    Self { ltime, id }
  }

  const fn id_byte() -> u8
  where
    I: Data,
  {
    merge(I::WIRE_TYPE, ID_TAG)
  }
}

impl<'a, I> DataRef<'a, JoinMessage<I>> for JoinMessage<I::Ref<'a>>
where
  I: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let mut ltime = None;
    let mut id = None;

    while offset < buf.len() {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "JoinMessage",
              "ltime",
              LTIME_TAG,
            ));
          }
          offset += 1;

          let (read, value) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += read;
          ltime = Some(value);
        }
        b if b == JoinMessage::<I>::id_byte() => {
          if id.is_some() {
            return Err(DecodeError::duplicate_field("JoinMessage", "id", ID_TAG));
          }
          offset += 1;

          let (read, value) =
            <I::Ref<'_> as DataRef<'_, I>>::decode_length_delimited(&buf[offset..])?;
          offset += read;
          id = Some(value);
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
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("JoinMessage", "ltime"))?,
        id: id.ok_or_else(|| DecodeError::missing_field("JoinMessage", "id"))?,
      },
    ))
  }
}

impl<I> Data for JoinMessage<I>
where
  I: Data,
{
  type Ref<'a> = JoinMessage<I::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    I::from_ref(val.id).map(|id| Self {
      ltime: val.ltime,
      id,
    })
  }

  fn encoded_len(&self) -> usize {
    1 + self.ltime.encoded_len() + 1 + self.id.encoded_len_with_length_delimited()
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    let mut offset = 0;

    if buf_len < 1 {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = LTIME_BYTE;
    offset += 1;
    offset += self.ltime.encode(&mut buf[offset..])?;

    if buf_len <= offset {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = Self::id_byte();
    offset += 1;

    offset += self.id.encode_length_delimited(&mut buf[offset..])?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}
