use indexmap::IndexMap;
use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, RepeatedDecoder, TupleEncoder, WireType,
  utils::{merge, skip, split},
};
use smol_str::SmolStr;

const TAGS_TAG: u8 = 1;
const TAGS_BYTE: u8 = merge(WireType::LengthDelimited, TAGS_TAG);

/// Tags of a node
#[derive(
  Debug,
  Default,
  PartialEq,
  Clone,
  derive_more::From,
  derive_more::Into,
  derive_more::Deref,
  derive_more::DerefMut,
  derive_more::AsRef,
  derive_more::AsMut,
  derive_more::IntoIterator,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Tags(
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::arbitrary_indexmap))]
   IndexMap<SmolStr, SmolStr>,
);

impl<K, V> FromIterator<(K, V)> for Tags
where
  K: Into<SmolStr>,
  V: Into<SmolStr>,
{
  fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
    Self(
      iter
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
    )
  }
}

impl Tags {
  /// Create a new Tags
  #[inline]
  pub fn new() -> Self {
    Self(IndexMap::new())
  }

  /// Create a new Tags with a capacity
  pub fn with_capacity(cap: usize) -> Self {
    Self(IndexMap::with_capacity(cap))
  }
}

/// The reference type to [`Tags`], which is an iterator and yields a reference to the key and value
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagsRef<'a> {
  src: RepeatedDecoder<'a>,
}

impl<'a> DataRef<'a, Tags> for TagsRef<'a> {
  fn decode(src: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = src.len();

    let mut tags_offsets = None;
    let mut num_tags = 0;

    while offset < buf_len {
      match src[offset] {
        TAGS_BYTE => {
          offset += 1;

          let readed = skip(WireType::LengthDelimited, &src[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = tags_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            tags_offsets = Some((offset - 1, offset + readed));
          }
          num_tags += 1;
          offset += readed;
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += skip(wire_type, &src[offset..])?;
        }
      }
    }

    let decoder =
      RepeatedDecoder::new(TAGS_TAG, WireType::LengthDelimited, src).with_nums(num_tags);

    Ok((
      offset,
      Self {
        src: if let Some((fnso, lnso)) = tags_offsets {
          decoder.with_offsets(fnso, lnso)
        } else {
          decoder
        },
      },
    ))
  }
}

impl Data for Tags {
  type Ref<'a> = TagsRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    val
      .src
      .iter::<(SmolStr, SmolStr)>()
      .map(|res| res.and_then(Data::from_ref))
      .collect::<Result<IndexMap<SmolStr, SmolStr>, DecodeError>>()
      .map(Self)
  }

  fn encoded_len(&self) -> usize {
    self
      .0
      .iter()
      .map(|(k, v)| 1 + TupleEncoder::new(k, v).encoded_len_with_length_delimited())
      .sum::<usize>()
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    self
      .0
      .iter()
      .try_fold(0, |mut offset, (k, v)| {
        if offset >= buf_len {
          return Err(EncodeError::insufficient_buffer(1, 0));
        }

        buf[offset] = TAGS_BYTE;
        offset += 1;
        offset += TupleEncoder::new(k, v).encode_with_length_delimited(&mut buf[offset..])?;
        Ok(offset)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len(), buf.len()))
  }
}
