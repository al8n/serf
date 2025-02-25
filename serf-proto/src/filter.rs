use memberlist_proto::{
  Data, DataRef, DecodeError, EncodeError, TinyVec, WireType,
  utils::{merge, split},
};

pub use id_filter::*;
pub use tag_filter::*;

mod id_filter;
mod tag_filter;

/// The type of filter
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, derive_more::IsVariant, derive_more::Display)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum FilterType {
  /// Filter by node ids
  #[display("id")]
  Id,
  /// Filter by tag
  #[display("tag")]
  Tag,
  /// Unknown filter type
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl FilterType {
  /// Get the string representation of the filter type
  #[inline]
  pub fn as_str(&self) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(match self {
      Self::Id => "id",
      Self::Tag => "tag",
      Self::Unknown(val) => return std::borrow::Cow::Owned(format!("unknown({})", val)),
    })
  }
}

impl From<u8> for FilterType {
  fn from(value: u8) -> Self {
    match value {
      0 => Self::Id,
      1 => Self::Tag,
      val => Self::Unknown(val),
    }
  }
}

impl From<FilterType> for u8 {
  fn from(val: FilterType) -> Self {
    match val {
      FilterType::Id => 0,
      FilterType::Tag => 1,
      FilterType::Unknown(val) => val,
    }
  }
}

/// Used with a queryFilter to specify the type of
/// filter we are sending
#[derive(Debug, Clone, PartialEq, Eq, derive_more::IsVariant)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Filter<I> {
  /// Filter by node ids
  Id(TinyVec<I>),
  /// Filter by tag
  Tag(TagFilter),
}

impl<I> Filter<I> {
  /// Returns the type of filter
  #[inline]
  pub const fn ty(&self) -> FilterType {
    match self {
      Self::Id(_) => FilterType::Id,
      Self::Tag { .. } => FilterType::Tag,
    }
  }
}

const FILTER_ID_TAG: u8 = 1;
const FILTER_TAG_TAG: u8 = 2;

/// The reference type to [`Filter`]
pub enum FilterRef<'a, I> {
  /// Filter by node ids
  Id(IdDecoder<'a, I>),
  /// Filter by tag
  Tag(TagFilterRef<'a>),
}

impl<I> Clone for FilterRef<'_, I> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I> Copy for FilterRef<'_, I> {}

impl<I> core::fmt::Debug for FilterRef<'_, I> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Id(id) => f.debug_tuple("FilterRef::Id").field(id).finish(),
      Self::Tag(t) => f.debug_tuple("FilterRef::Tag").field(t).finish(),
    }
  }
}

impl<'a, I> DataRef<'a, Filter<I>> for FilterRef<'a, I>
where
  I: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let buf_len = buf.len();
    if buf_len < 1 {
      return Err(DecodeError::buffer_underflow());
    }

    let mut offset = 0;

    match buf[0] {
      val if val == Filter::<I>::id_byte() => {
        offset += 1;
        Ok((offset, Self::Id(IdDecoder::new(&buf[offset..]))))
      }
      val if val == Filter::<I>::tag_byte() => {
        offset += 1;
        let (read, tag) =
          <TagFilterRef as DataRef<'_, TagFilter>>::decode_length_delimited(&buf[offset..])?;
        offset += read;
        Ok((offset, Self::Tag(tag)))
      }
      b => {
        let (wire_type, tag) = split(b);
        WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;

        Err(DecodeError::unknown_tag("Filter", tag))
      }
    }
  }
}

impl<I> Filter<I>
where
  I: Data,
{
  const fn id_byte() -> u8 {
    merge(I::WIRE_TYPE, FILTER_ID_TAG)
  }

  const fn tag_byte() -> u8 {
    merge(WireType::LengthDelimited, FILTER_TAG_TAG)
  }
}

impl<I> Data for Filter<I>
where
  I: Data,
{
  type Ref<'a> = FilterRef<'a, I>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    match val {
      FilterRef::Id(decoder) => decoder
        .map(|res| res.and_then(I::from_ref))
        .collect::<Result<_, DecodeError>>()
        .map(Self::Id),
      FilterRef::Tag(tag) => TagFilter::from_ref(tag).map(Self::Tag),
    }
  }

  fn encoded_len(&self) -> usize {
    1usize
      + match self {
        Filter::Id(ids) => ids
          .iter()
          .map(|id| id.encoded_len_with_length_delimited())
          .sum::<usize>(),
        Filter::Tag(tag) => 1 + tag.encoded_len_with_length_delimited(),
      }
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    if buf_len < 1 {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    let mut offset = 0;

    match self {
      Filter::Id(ids) => {
        buf[offset] = Self::id_byte();
        offset += 1;

        ids
          .iter()
          .try_fold(&mut offset, |offset, id| {
            *offset += id.encode_length_delimited(&mut buf[*offset..])?;

            Ok(offset)
          })
          .map_err(|e: EncodeError| e.update(self.encoded_len(), buf_len))?;

        Ok(offset)
      }
      Filter::Tag(tag) => {
        buf[offset] = Self::tag_byte();
        offset += 1;

        if offset > buf_len {
          return Err(EncodeError::insufficient_buffer(
            self.encoded_len(),
            buf_len,
          ));
        }

        offset += tag
          .encode_length_delimited(&mut buf[offset..])
          .map_err(|e| e.update(self.encoded_len(), buf_len))?;

        Ok(offset)
      }
    }
  }
}
