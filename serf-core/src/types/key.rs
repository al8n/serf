use indexmap::IndexMap;
use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, RepeatedDecoder, SecretKey, SecretKeys, WireType,
  utils::{merge, skip, split},
};
use smol_str::SmolStr;

/// KeyRequest is used to contain input parameters which get broadcasted to all
/// nodes as part of a key query operation.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyRequestMessage {
  /// The secret key
  #[viewit(
    getter(const, attrs(doc = "Returns the secret key")),
    setter(const, attrs(doc = "Sets the secret key (Builder pattern)"))
  )]
  key: Option<SecretKey>,
}

const KEY_REQ_KEY_TAG: u8 = 1;
const KEY_REQ_KEY_BYTE: u8 = merge(WireType::LengthDelimited, KEY_REQ_KEY_TAG);

impl DataRef<'_, Self> for KeyRequestMessage {
  fn decode(buf: &'_ [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut key = None;

    while offset < buf_len {
      match buf[offset] {
        KEY_REQ_KEY_BYTE => {
          offset += 1;

          let (bytes_read, val) = <SecretKey as Data>::decode_length_delimited(&buf[offset..])?;
          offset += bytes_read;
          key = Some(val);
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += skip(wire_type, &buf[offset..])?;
        }
      }
    }

    Ok((offset, Self { key }))
  }
}

impl Data for KeyRequestMessage {
  type Ref<'a> = Self;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(val)
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;
    if let Some(key) = &self.key {
      len += 1 + key.encoded_len_with_length_delimited();
    }
    len
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    let mut offset = 0;

    if let Some(key) = &self.key {
      if buf_len < offset + 1 {
        return Err(EncodeError::insufficient_buffer(
          self.encoded_len(),
          buf_len,
        ));
      }
      buf[offset] = KEY_REQ_KEY_BYTE;
      offset += 1;

      let bytes_written = key.encode_length_delimited(&mut buf[offset..])?;
      offset += bytes_written;
    }

    Ok(offset)
  }
}

/// Key response message
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyResponseMessage {
  /// Indicates true/false if there were errors or not
  #[viewit(
    getter(const, attrs(doc = "Returns true/false if there were errors or not")),
    setter(
      const,
      attrs(doc = "Sets true/false if there were errors or not (Builder pattern)")
    )
  )]
  result: bool,
  /// Contains error messages or other information
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the error messages or other information")
    ),
    setter(attrs(doc = "Sets the error messages or other information (Builder pattern)"))
  )]
  message: SmolStr,
  /// Used in listing queries to relay a list of installed keys
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns a list of installed keys")),
    setter(attrs(doc = "Sets the the list of installed keys (Builder pattern)"))
  )]
  keys: SecretKeys,
  /// Used in listing queries to relay the primary key
  #[viewit(
    getter(const, attrs(doc = "Returns the primary key")),
    setter(attrs(doc = "Sets the primary key (Builder pattern)"))
  )]
  primary_key: Option<SecretKey>,
}

impl KeyResponseMessage {
  /// Adds a key to the list of keys
  #[inline]
  pub fn add_key(&mut self, key: SecretKey) -> &mut Self {
    self.keys.push(key);
    self
  }
}

const KEY_RESPONSE_RESULT_TAG: u8 = 1;
const KEY_RESPONSE_RESULT_BYTE: u8 = merge(WireType::Byte, KEY_RESPONSE_RESULT_TAG);
const KEY_RESPONSE_MESSAGE_TAG: u8 = 2;
const KEY_RESPONSE_MESSAGE_BYTE: u8 = merge(WireType::LengthDelimited, KEY_RESPONSE_MESSAGE_TAG);
const KEY_RESPONSE_KEYS_TAG: u8 = 3;
const KEY_RESPONSE_KEYS_BYTE: u8 = merge(WireType::LengthDelimited, KEY_RESPONSE_KEYS_TAG);
const KEY_RESPONSE_PRIMARY_KEY_TAG: u8 = 4;
const KEY_RESPONSE_PRIMARY_KEY_BYTE: u8 =
  merge(WireType::LengthDelimited, KEY_RESPONSE_PRIMARY_KEY_TAG);

/// The reference type for [`KeyResponseMessage`].
#[viewit::viewit(getters(style = "ref", vis_all = "pub"), setters(skip), vis_all = "")]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct KeyResponseMessageRef<'a> {
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns true/false if there were errors or not")
  ))]
  result: bool,
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the error messages or other information")
  ))]
  message: &'a str,
  #[viewit(getter(const, attrs(doc = "Returns a list of installed keys")))]
  keys: RepeatedDecoder<'a>,
  #[viewit(getter(
    const,
    attrs(doc = "Returns the primary key"),
    result(converter(fn = "Option::as_ref"), type = "Option<&SecretKey>"),
  ))]
  primary_key: Option<SecretKey>,
}

impl<'a> DataRef<'a, KeyResponseMessage> for KeyResponseMessageRef<'a> {
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut result = None;
    let mut message = None;
    let mut keys_offsets = None;
    let mut num_keys = 0;
    let mut primary_key = None;

    while offset < buf_len {
      match buf[offset] {
        KEY_RESPONSE_RESULT_BYTE => {
          if result.is_some() {
            return Err(DecodeError::duplicate_field(
              "KeyResponseMessage",
              "result",
              KEY_RESPONSE_RESULT_TAG,
            ));
          }
          offset += 1;
          if offset >= buf_len {
            return Err(DecodeError::buffer_underflow());
          }
          result = Some(buf[offset] != 0);
          offset += 1;
        }
        KEY_RESPONSE_MESSAGE_BYTE => {
          if message.is_some() {
            return Err(DecodeError::duplicate_field(
              "KeyResponseMessage",
              "message",
              KEY_RESPONSE_MESSAGE_TAG,
            ));
          }

          offset += 1;
          let (bytes_read, val) =
            <&str as DataRef<'_, SmolStr>>::decode_length_delimited(&buf[offset..])?;
          offset += bytes_read;
          message = Some(val);
        }
        KEY_RESPONSE_KEYS_BYTE => {
          offset += 1;
          let readed = skip(WireType::LengthDelimited, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = keys_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            keys_offsets = Some((offset - 1, offset + readed));
          }
          num_keys += 1;
          offset += readed;
        }
        KEY_RESPONSE_PRIMARY_KEY_BYTE => {
          if primary_key.is_some() {
            return Err(DecodeError::duplicate_field(
              "KeyResponseMessage",
              "primary_key",
              KEY_RESPONSE_PRIMARY_KEY_TAG,
            ));
          }

          offset += 1;
          let (bytes_read, val) = <SecretKey as Data>::decode_length_delimited(&buf[offset..])?;
          offset += bytes_read;
          primary_key = Some(val);
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
        result: result.unwrap_or_default(),
        message: message.unwrap_or_default(),
        keys: if let Some((start, end)) = keys_offsets {
          RepeatedDecoder::new(KEY_RESPONSE_KEYS_TAG, WireType::LengthDelimited, buf)
            .with_nums(num_keys)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(KEY_RESPONSE_KEYS_TAG, WireType::LengthDelimited, buf)
        },
        primary_key,
      },
    ))
  }
}

impl Data for KeyResponseMessage {
  type Ref<'a> = KeyResponseMessageRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    val
      .keys
      .iter::<SecretKey>()
      .map(|res| res.and_then(Data::from_ref))
      .collect::<Result<SecretKeys, DecodeError>>()
      .map(|keys| Self {
        result: val.result,
        message: SmolStr::new(val.message),
        keys,
        primary_key: val.primary_key,
      })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;
    if self.result {
      len += 1 + 1;
    }

    if !self.message.is_empty() {
      len += 1 + self.message.encoded_len_with_length_delimited();
    }

    len += self
      .keys
      .iter()
      .map(|key| 1 + key.encoded_len_with_length_delimited())
      .sum::<usize>();

    if let Some(key) = &self.primary_key {
      len += 1 + key.encoded_len_with_length_delimited();
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

    if self.result {
      bail!(self(offset, buf_len));
      buf[offset] = KEY_RESPONSE_RESULT_BYTE;
      offset += 1;
      bail!(self(offset, buf_len));
      buf[offset] = 1;
      offset += 1;
    }

    if !self.message.is_empty() {
      bail!(self(offset, buf_len));
      buf[offset] = KEY_RESPONSE_MESSAGE_BYTE;
      offset += 1;
      offset += self.message.encode_length_delimited(&mut buf[offset..])?;
    }

    for key in self.keys.iter() {
      bail!(self(offset, buf_len));
      buf[offset] = KEY_RESPONSE_KEYS_BYTE;
      offset += 1;
      offset += key.encode_length_delimited(&mut buf[offset..])?;
    }

    if let Some(key) = &self.primary_key {
      bail!(self(offset, buf_len));
      buf[offset] = KEY_RESPONSE_PRIMARY_KEY_BYTE;
      offset += 1;
      offset += key.encode_length_delimited(&mut buf[offset..])?;
    }

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}

/// KeyResponse is used to relay a query for a list of all keys in use.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Default)]
pub struct KeyResponse<I> {
  /// Map of node id to response message
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the map of node id to response message")
    ),
    setter(attrs(doc = "Sets the map of node id to response message (Builder pattern)"))
  )]
  messages: IndexMap<I, SmolStr>,
  /// Total nodes memberlist knows of
  #[viewit(
    getter(const, attrs(doc = "Returns the total nodes memberlist knows of")),
    setter(
      const,
      attrs(doc = "Sets total nodes memberlist knows of (Builder pattern)")
    )
  )]
  num_nodes: usize,
  /// Total responses received
  #[viewit(
    getter(const, attrs(doc = "Returns the total responses received")),
    setter(
      const,
      attrs(doc = "Sets the total responses received (Builder pattern)")
    )
  )]
  num_resp: usize,
  /// Total errors from request
  #[viewit(
    getter(const, attrs(doc = "Returns the total errors from request")),
    setter(
      const,
      attrs(doc = "Sets the total errors from request (Builder pattern)")
    )
  )]
  num_err: usize,

  /// A mapping of the value of the key bytes to the
  /// number of nodes that have the key installed.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        doc = "Returns a mapping of the value of the key bytes to the number of nodes that have the key installed."
      )
    ),
    setter(attrs(
      doc = "Sets a mapping of the value of the key bytes to the number of nodes that have the key installed (Builder pattern)"
    ))
  )]
  keys: IndexMap<SecretKey, u32>,

  /// A mapping of the value of the primary
  /// key bytes to the number of nodes that have the key installed.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        doc = "Returns a mapping of the value of the primary key bytes to the number of nodes that have the key installed."
      )
    ),
    setter(attrs(
      doc = "Sets a mapping of the value of the primary key bytes to the number of nodes that have the key installed. (Builder pattern)"
    ))
  )]
  primary_keys: IndexMap<SecretKey, u32>,
}

/// KeyRequestOptions is used to contain optional parameters for a keyring operation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyRequestOptions {
  /// The number of duplicate query responses to send by relaying through
  /// other nodes, for redundancy
  pub relay_factor: u8,
}
