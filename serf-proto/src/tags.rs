use indexmap::IndexMap;
use memberlist_proto::{
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
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Tags(
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::arbitrary_indexmap))]
  IndexMap<SmolStr, SmolStr>,
);

impl IntoIterator for Tags {
  type Item = (SmolStr, SmolStr);
  type IntoIter = indexmap::map::IntoIter<SmolStr, SmolStr>;

  fn into_iter(self) -> Self::IntoIter {
    self.0.into_iter()
  }
}

impl FromIterator<(SmolStr, SmolStr)> for Tags {
  fn from_iter<T: IntoIterator<Item = (SmolStr, SmolStr)>>(iter: T) -> Self {
    Self(iter.into_iter().collect())
  }
}

impl<'a> FromIterator<(&'a str, &'a str)> for Tags {
  fn from_iter<T: IntoIterator<Item = (&'a str, &'a str)>>(iter: T) -> Self {
    Self(
      iter
        .into_iter()
        .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
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
  fn decode(src: &'a [u8]) -> Result<(usize, Self), memberlist_proto::DecodeError>
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

// #[derive(Debug)]
// struct Tag {
//   key: SmolStr,
//   value: SmolStr,
// }

// impl Tag {
//   fn split(self) -> (SmolStr, SmolStr) {
//     (self.key, self.value)
//   }
// }

// impl Data for Tag {
//   type Ref<'a> = TagRef<'a>;

//   fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
//   where
//     Self: Sized,
//   {
//     Ok(Self {
//       key: SmolStr::new(val.key),
//       value: SmolStr::new(val.value),
//     })
//   }

//   fn encoded_len(&self) -> usize {
//     TagRef::new(&self.key, &self.value).encoded_len()
//   }

//   fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
//     TagRef::new(&self.key, &self.value).encode(buf)
//   }
// }

// /// A reference to a (key, value) pair of a tag
// #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// pub struct TagRef<'a> {
//   key: &'a str,
//   value: &'a str,
// }

// impl<'a> DataRef<'a, Tag> for TagRef<'a> {
//   fn decode(src: &'a [u8]) -> Result<(usize, Self), DecodeError>
//   where
//     Self: Sized,
//   {
//     let mut offset = 0;
//     let buf_len = src.len();

//     let mut key = None;
//     let mut val = None;

//     while offset < buf_len {
//       match src[offset] {
//         Self::KEY_BYTE => {
//           if key.is_some() {
//             return Err(DecodeError::duplicate_field("Tag", "key", Self::KEY_TAG));
//           }
//           offset += 1;

//           let (read, value) =
//             <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&src[offset..])?;
//           key = Some(value);
//           offset += read;
//         }
//         Self::VALUE_BYTE => {
//           if val.is_some() {
//             return Err(DecodeError::duplicate_field(
//               "Tag",
//               "value",
//               Self::VALUE_TAG,
//             ));
//           }
//           offset += 1;

//           let (read, value) =
//             <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&src[offset..])?;
//           val = Some(value);
//           offset += read;
//         }
//         other => {
//           offset += 1;

//           let (wire_type, _) = split(other);
//           let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
//           offset += skip(wire_type, &src[offset..])?;
//         }
//       }
//     }

//     Ok((
//       offset,
//       Self {
//         key: key.unwrap_or(""),
//         value: val.unwrap_or(""),
//       },
//     ))
//   }
// }

// impl<'a> TagRef<'a> {
//   const KEY_TAG: u8 = 1;
//   const KEY_BYTE: u8 = merge(WireType::LengthDelimited, Self::KEY_TAG);
//   const VALUE_TAG: u8 = 2;
//   const VALUE_BYTE: u8 = merge(WireType::LengthDelimited, Self::VALUE_TAG);

//   fn new(key: &'a str, value: &'a str) -> Self {
//     Self { key, value }
//   }

//   fn encoded_len(&self) -> usize {
//     let klen = self.key.len();
//     let vlen = self.value.len();

//     let mut len = 0;
//     if klen != 0 {
//       len += 1 + (klen as u32).encoded_len();
//     }

//     if vlen != 0 {
//       len += 1 + (vlen as u32).encoded_len();
//     }

//     len
//   }

//   fn encoded_len_with_length_delimited(&self) -> usize {
//     let len = self.encoded_len();
//     len + (len as u32).encoded_len()
//   }

//   fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
//     let buf_len = buf.len();
//     let mut offset = 0;

//     if buf_len <= offset {
//       return Err(EncodeError::insufficient_buffer(
//         self.encoded_len(),
//         buf_len,
//       ));
//     }

//     let klen = self.key.len();
//     if klen != 0 {
//       buf[offset] = Self::KEY_BYTE;
//       offset += 1;

//       offset += (klen as u32)
//         .encode(&mut buf[offset..])
//         .map_err(|e| e.update(self.encoded_len(), buf_len))?;
//       if buf_len < offset + klen {
//         return Err(EncodeError::insufficient_buffer(
//           self.encoded_len(),
//           buf_len,
//         ));
//       }
//       buf[offset..offset + klen].copy_from_slice(self.key.as_bytes());
//       offset += klen;
//     }

//     if buf_len <= offset {
//       return Err(EncodeError::insufficient_buffer(
//         self.encoded_len(),
//         buf_len,
//       ));
//     }

//     let vlen = self.value.len();
//     if vlen != 0 {
//       buf[offset] = Self::VALUE_BYTE;
//       offset += 1;

//       offset += (vlen as u32)
//         .encode(&mut buf[offset..])
//         .map_err(|e| e.update(self.encoded_len(), buf_len))?;
//       if buf_len < offset + vlen {
//         return Err(EncodeError::insufficient_buffer(
//           self.encoded_len(),
//           buf_len,
//         ));
//       }

//       buf[offset..offset + vlen].copy_from_slice(self.value.as_bytes());
//       offset += vlen;
//     }

//     #[cfg(debug_assertions)]
//     super::debug_assert_write_eq(offset, self.encoded_len());

//     Ok(offset)
//   }

//   fn encode_with_length_delimited(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
//     let len = self.encoded_len();
//     let buf_len = buf.len();
//     if buf_len < len {
//       return Err(EncodeError::insufficient_buffer(len, buf_len));
//     }

//     let mut offset = 0;
//     offset += (len as u32).encode(&mut buf[offset..])?;
//     offset += self.encode(&mut buf[offset..])?;

//     #[cfg(debug_assertions)]
//     super::debug_assert_write_eq(offset, self.encoded_len_with_length_delimited());

//     Ok(offset)
//   }
// }
