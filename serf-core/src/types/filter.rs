use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, RepeatedDecoder, TinyVec, WireType,
  utils::{merge, skip, split},
};

pub use tag_filter::*;
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
#[derive(
  Debug,
  Clone,
  PartialEq,
  Eq,
  derive_more::IsVariant,
  derive_more::From,
  derive_more::Unwrap,
  derive_more::TryUnwrap,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[unwrap(ref, ref_mut)]
#[try_unwrap(ref, ref_mut)]
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
#[derive(Clone, Copy, Debug)]
pub enum FilterRef<'a> {
  /// Filter by node ids
  Id(RepeatedDecoder<'a>),
  /// Filter by tag
  Tag(TagFilterRef<'a>),
}

impl<'a, I> DataRef<'a, Filter<I>> for FilterRef<'a>
where
  I: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let buf_len = buf.len();
    let mut offset = 0;
    let mut ids_offsets = None;
    let mut num_ids = 0;
    let mut f = None;

    while offset < buf_len {
      match buf[offset] {
        val if val == Filter::<I>::id_byte() => {
          offset += 1;
          let readed = skip(I::WIRE_TYPE, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = ids_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            ids_offsets = Some((offset - 1, offset + readed));
          }
          num_ids += 1;
          offset += readed;
        }
        val if val == Filter::<I>::tag_byte() => {
          if let Some(Self::Tag(_)) = f {
            return Err(DecodeError::duplicate_field(
              "Filter",
              "tag",
              FILTER_TAG_TAG,
            ));
          }

          if ids_offsets.is_some() {
            return Err(DecodeError::duplicate_field("Filter", "id", FILTER_ID_TAG));
          }

          offset += 1;
          let (read, tag) =
            <TagFilterRef as DataRef<'_, TagFilter>>::decode_length_delimited(&buf[offset..])?;
          offset += read;
          f = Some(FilterRef::Tag(tag));
        }
        b => {
          let (wire_type, _) = split(b);
          let wt = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += 1;
          offset += skip(wt, &buf[offset..])?;
        }
      }
    }

    Ok((
      offset,
      if let Some(tag) = f {
        tag
      } else if let Some((start, end)) = ids_offsets {
        Self::Id(
          RepeatedDecoder::new(FILTER_ID_TAG, I::WIRE_TYPE, buf)
            .with_nums(num_ids)
            .with_offsets(start, end),
        )
      } else {
        Self::Id(RepeatedDecoder::new(FILTER_ID_TAG, I::WIRE_TYPE, buf).with_nums(0))
      },
    ))
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
  type Ref<'a> = FilterRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    match val {
      FilterRef::Id(decoder) => decoder
        .iter::<I>()
        .map(|res| res.and_then(I::from_ref))
        .collect::<Result<_, DecodeError>>()
        .map(Self::Id),
      FilterRef::Tag(tag) => TagFilter::from_ref(tag).map(Self::Tag),
    }
  }

  fn encoded_len(&self) -> usize {
    match self {
      Filter::Id(ids) => ids
        .iter()
        .map(|id| 1 + id.encoded_len_with_length_delimited())
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
        ids
          .iter()
          .try_fold(&mut offset, |offset, id| {
            if *offset >= buf_len {
              return Err(EncodeError::insufficient_buffer(
                self.encoded_len(),
                buf_len,
              ));
            }

            buf[*offset] = Self::id_byte();
            *offset += 1;

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
