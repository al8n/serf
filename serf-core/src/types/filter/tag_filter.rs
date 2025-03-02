use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, WireType,
  utils::{merge, skip, split},
};
use regex::Regex;
use smol_str::SmolStr;

const TAG_TAG: u8 = 1;
const EXPR_TAG: u8 = 2;
const TAG_BYTE: u8 = merge(WireType::LengthDelimited, TAG_TAG);
const EXPR_BYTE: u8 = merge(WireType::LengthDelimited, EXPR_TAG);

#[viewit::viewit(vis_all = "", getters(vis_all = "pub"), setters(skip))]
/// The reference type of the [`TagFilter`] type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TagFilterRef<'a> {
  #[viewit(getter(const, attrs(doc = "Returns the tag")))]
  tag: &'a str,
  #[viewit(getter(const, attrs(doc = "Returns the expression")))]
  expr: Option<&'a str>,
}

impl<'a> DataRef<'a, TagFilter> for TagFilterRef<'a> {
  fn decode(src: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = src.len();
    let mut tag = None;
    let mut expr = None;

    while offset < buf_len {
      match src[offset] {
        TAG_BYTE => {
          if tag.is_some() {
            return Err(DecodeError::duplicate_field("TagFilter", "tag", TAG_TAG));
          }
          offset += 1;

          let (read, value) =
            <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&src[offset..])?;
          offset += read;
          tag = Some(value);
        }
        EXPR_BYTE => {
          if expr.is_some() {
            return Err(DecodeError::duplicate_field("TagFilter", "expr", EXPR_TAG));
          }
          offset += 1;

          let (read, value) =
            <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&src[offset..])?;
          offset += read;
          expr = Some(value);
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type)
            .map_err(|v| DecodeError::unknown_wire_type("TagFilter", v))?;
          offset += skip(wire_type, &src[offset..])?;
        }
      }
    }

    Ok((
      offset,
      Self {
        tag: tag.unwrap_or(""),
        expr: expr.and_then(|expr| if expr.is_empty() { None } else { Some(expr) }),
      },
    ))
  }
}

/// The tag filter
#[viewit::viewit(
  vis_all = "",
  getters(vis_all = "pub", style = "ref"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TagFilter {
  #[viewit(
    getter(const, attrs(doc = "Returns the tag")),
    setter(attrs(doc = "Sets the tag (Builder pattern)"))
  )]
  tag: SmolStr,
  #[cfg_attr(feature = "serde", serde(with = "serde_regex"))]
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns the expression"),
      result(converter(fn = "Option::as_ref"), type = "Option<&Regex>"),
    ),
    setter(
      rename = "maybe_expr",
      attrs(doc = "Sets the expression (Builder pattern)")
    )
  )]
  expr: Option<Regex>,
}

impl Default for TagFilter {
  fn default() -> Self {
    Self::new()
  }
}

impl TagFilter {
  /// Creates a new tag filter
  #[inline]
  pub const fn new() -> Self {
    Self {
      tag: SmolStr::new_inline(""),
      expr: None,
    }
  }

  /// Set the expression for the tag filter
  #[inline]
  pub fn with_expr(mut self, expr: Regex) -> Self {
    self.expr = Some(expr);
    self
  }
}

impl PartialEq for TagFilter {
  fn eq(&self, other: &Self) -> bool {
    self.tag == other.tag
      && self.expr.as_ref().map(|re| re.as_str()) == other.expr.as_ref().map(|re| re.as_str())
  }
}

impl Eq for TagFilter {}

impl Data for TagFilter {
  type Ref<'a> = TagFilterRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      tag: SmolStr::from(val.tag),
      expr: val
        .expr
        .map(|expr| Regex::new(expr).map_err(|e| DecodeError::custom(e.to_string())))
        .transpose()?,
    })
  }

  fn encoded_len(&self) -> usize {
    1 + self.tag.encoded_len_with_length_delimited()
      + match self.expr.as_ref() {
        Some(re) => {
          let re = re.as_str();
          let len = re.len();
          1 + (len as u32).encoded_len() + len
        }
        None => 0,
      }
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    let mut offset = 0;

    if buf_len <= offset {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = TAG_BYTE;
    offset += 1;
    offset += self
      .tag
      .encode_length_delimited(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    if let Some(re) = self.expr.as_ref() {
      let re = re.as_str();
      let len = re.len();
      if buf_len < offset + 1 + (len as u32).encoded_len() + len {
        return Err(EncodeError::insufficient_buffer(
          self.encoded_len(),
          buf_len,
        ));
      }

      buf[offset] = EXPR_BYTE;
      offset += 1;
      offset += (len as u32).encode(&mut buf[offset..])?;
      buf[offset..offset + len].copy_from_slice(re.as_bytes());
      offset += len;
    }

    #[cfg(debug_assertions)]
    super::super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}

#[cfg(feature = "serde")]
mod serde_regex {
  use regex::Regex;
  use serde::{de, ser};

  pub fn serialize<S>(value: &Option<Regex>, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: ser::Serializer,
  {
    match value {
      Some(re) => serializer.serialize_str(re.as_str()),
      None => serializer.serialize_none(),
    }
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Regex>, D::Error>
  where
    D: de::Deserializer<'de>,
  {
    let s = <Option<&str> as de::Deserialize<'_>>::deserialize(deserializer)?;
    match s {
      Some(s) => s.try_into().map(Some).map_err(de::Error::custom),
      None => Ok(None),
    }
  }
}
