use smol_str::SmolStr;

use std::time::Duration;

use memberlist_proto::{
  Data, DataRef, DecodeError, EncodeError, Node, RepeatedDecoder, TinyVec, WireType,
  bytes::Bytes,
  utils::{merge, skip, split},
};

use super::{Filter, LamportTime};

pub use response::*;

mod response;

bitflags::bitflags! {
  /// Flags for query message
  #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
  #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
  #[cfg_attr(feature = "serde", serde(transparent))]
  pub struct QueryFlag: u32 {
    /// Ack flag is used to force receiver to send an ack back
    const ACK = 1 << 0;
    /// NoBroadcast is used to prevent re-broadcast of a query.
    /// this can be used to selectively send queries to individual members
    const NO_BROADCAST = 1 << 1;
  }
}

const LTIME_TAG: u8 = 1;
const ID_TAG: u8 = 2;
const FROM_TAG: u8 = 3;
const FILTERS_TAG: u8 = 4;
const FLAGS_TAG: u8 = 5;
const RELAY_FACTOR_TAG: u8 = 6;
const TIMEOUT_TAG: u8 = 7;
const NAME_TAG: u8 = 8;
const PAYLOAD_TAG: u8 = 9;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const ID_BYTE: u8 = merge(WireType::Varint, ID_TAG);
const FROM_BYTE: u8 = merge(WireType::LengthDelimited, FROM_TAG);
const FILTERS_BYTE: u8 = merge(WireType::LengthDelimited, FILTERS_TAG);
const FLAGS_BYTE: u8 = merge(WireType::Varint, FLAGS_TAG);
const RELAY_FACTOR_BYTE: u8 = merge(WireType::Varint, RELAY_FACTOR_TAG);
const TIMEOUT_BYTE: u8 = merge(WireType::Varint, TIMEOUT_TAG);
const NAME_BYTE: u8 = merge(WireType::LengthDelimited, NAME_TAG);
const PAYLOAD_BYTE: u8 = merge(WireType::LengthDelimited, PAYLOAD_TAG);

/// Query message
#[viewit::viewit(getters(style = "ref"), setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct QueryMessage<I, A> {
  /// Event lamport time
  #[viewit(
    getter(const, style = "move", attrs(doc = "Returns the event lamport time")),
    setter(const, attrs(doc = "Sets the event lamport time (Builder pattern)"))
  )]
  ltime: LamportTime,
  /// query id, randomly generated
  #[viewit(
    getter(const, style = "move", attrs(doc = "Returns the query id")),
    setter(attrs(doc = "Sets the query id (Builder pattern)"))
  )]
  id: u32,
  /// source node
  #[viewit(
    getter(const, attrs(doc = "Returns the from node")),
    setter(attrs(doc = "Sets the from node (Builder pattern)"))
  )]
  from: Node<I, A>,
  /// Potential query filters
  #[viewit(
    getter(const, attrs(doc = "Returns the potential query filters")),
    setter(attrs(doc = "Sets the potential query filters (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::into::<Vec<Filter<I>>, TinyVec<Filter<I>>>))]
  filters: TinyVec<Filter<I>>,
  /// Used to provide various flags
  #[viewit(
    getter(const, style = "move", attrs(doc = "Returns the flags")),
    setter(attrs(doc = "Sets the flags (Builder pattern)"))
  )]
  flags: QueryFlag,
  /// Used to set the number of duplicate relayed responses
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the number of duplicate relayed responses")
    ),
    setter(attrs(doc = "Sets the number of duplicate relayed responses (Builder pattern)"))
  )]
  relay_factor: u8,
  /// Maximum time between delivery and response
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the maximum time between delivery and response")
    ),
    setter(attrs(doc = "Sets the maximum time between delivery and response (Builder pattern)"))
  )]
  timeout: Duration,
  /// Query nqme
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the name of the query")),
    setter(attrs(doc = "Sets the name of the query (Builder pattern)"))
  )]
  name: SmolStr,
  /// Query payload
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the payload")),
    setter(attrs(doc = "Sets the payload (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::into::<Vec<u8>, Bytes>))]
  payload: Bytes,
}

impl<I, A> QueryMessage<I, A> {
  /// Checks if the ack flag is set
  #[inline]
  pub fn ack(&self) -> bool {
    self.flags.contains(QueryFlag::ACK)
  }

  /// Checks if the no broadcast flag is set
  #[inline]
  pub fn no_broadcast(&self) -> bool {
    self.flags.contains(QueryFlag::NO_BROADCAST)
  }
}

/// The reference type of [`QueryMessage`]
#[viewit::viewit(vis_all = "", getters(vis_all = "pub", style = "ref"), setters(skip))]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct QueryMessageRef<'a, I, A> {
  /// Event lamport time
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the event lamport time")))]
  ltime: LamportTime,
  /// query id, randomly generated
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the query id")))]
  id: u32,
  /// source node
  #[viewit(getter(const, attrs(doc = "Returns the from node")))]
  from: Node<I, A>,
  /// Potential query filters
  #[viewit(getter(const, attrs(doc = "Returns the potential query filters")))]
  filters: RepeatedDecoder<'a>,
  /// Used to provide various flags
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the flags")))]
  flags: QueryFlag,
  /// Used to set the number of duplicate relayed responses
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the number of duplicate relayed responses")
  ))]
  relay_factor: u8,
  /// Maximum time between delivery and response
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the maximum time between delivery and response")
  ))]
  timeout: Duration,
  /// Query nqme
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the name of the query")))]
  name: &'a str,
  /// Query payload
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the payload")))]
  payload: &'a [u8],
}

impl<'a, I, A> DataRef<'a, QueryMessage<I, A>> for QueryMessageRef<'a, I::Ref<'a>, A::Ref<'a>>
where
  I: Data,
  A: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut ltime = None;
    let mut id = None;
    let mut from = None;
    let mut filters_offsets = None;
    let mut num_filters = 0;
    let mut flags = None;
    let mut relay_factor = None;
    let mut timeout = None;
    let mut name = None;
    let mut payload = None;

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
              "ltime",
              LTIME_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += o;
          ltime = Some(v);
        }
        ID_BYTE => {
          if id.is_some() {
            return Err(DecodeError::duplicate_field("QueryMessage", "id", ID_TAG));
          }

          offset += 1;
          let (o, v) = <u32 as DataRef<'_, u32>>::decode(&buf[offset..])?;
          offset += o;
          id = Some(v);
        }
        FROM_BYTE => {
          if from.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
              "from",
              FROM_TAG,
            ));
          }

          offset += 1;
          let (o, v) =
            <Node<I::Ref<'_>, A::Ref<'_>> as DataRef<'_, Node<I, A>>>::decode(&buf[offset..])?;
          offset += o;
          from = Some(v);
        }
        FILTERS_BYTE => {
          let readed = skip(WireType::LengthDelimited, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = filters_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            filters_offsets = Some((offset - 1, offset + readed));
          }
          num_filters += 1;
          offset += readed;
        }
        FLAGS_BYTE => {
          if flags.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
              "flags",
              FLAGS_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <u32 as DataRef<'_, u32>>::decode(&buf[offset..])?;
          offset += o;
          flags = Some(QueryFlag::from_bits_truncate(v));
        }
        RELAY_FACTOR_BYTE => {
          if relay_factor.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
              "relay_factor",
              RELAY_FACTOR_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <u8 as DataRef<'_, u8>>::decode(&buf[offset..])?;
          offset += o;
          relay_factor = Some(v);
        }
        TIMEOUT_BYTE => {
          if timeout.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
              "timeout",
              TIMEOUT_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <Duration as DataRef<'_, Duration>>::decode(&buf[offset..])?;
          offset += o;
          timeout = Some(v);
        }
        NAME_BYTE => {
          if name.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryMessage",
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
              "QueryMessage",
              "payload",
              PAYLOAD_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <&[u8] as DataRef<'_, Bytes>>::decode_length_delimited(&buf[offset..])?;
          offset += o;
          payload = Some(v);
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += skip(wire_type, &buf[offset..])?;
        }
      }
    }

    let filters =
      RepeatedDecoder::new(FILTERS_TAG, WireType::LengthDelimited, buf).with_nums(num_filters);

    Ok((
      offset,
      Self {
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("QueryMessage", "ltime"))?,
        id: id.ok_or_else(|| DecodeError::missing_field("QueryMessage", "id"))?,
        from: from.ok_or_else(|| DecodeError::missing_field("QueryMessage", "from"))?,
        filters: if let Some((start, end)) = filters_offsets {
          filters.with_offsets(start, end)
        } else {
          filters
        },
        flags: flags.ok_or_else(|| DecodeError::missing_field("QueryMessage", "flags"))?,
        relay_factor: relay_factor
          .ok_or_else(|| DecodeError::missing_field("QueryMessage", "relay_factor"))?,
        timeout: timeout.ok_or_else(|| DecodeError::missing_field("QueryMessage", "timeout"))?,
        name: name.unwrap_or_default(),
        payload: payload.unwrap_or_default(),
      },
    ))
  }
}

impl<I, A> Data for QueryMessage<I, A>
where
  I: Data,
  A: Data,
{
  type Ref<'a> = QueryMessageRef<'a, I::Ref<'a>, A::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    val
      .filters
      .iter::<Filter<I>>()
      .map(|res| res.and_then(Data::from_ref))
      .collect::<Result<TinyVec<_>, DecodeError>>()
      .and_then(|filters| {
        Ok(Self {
          ltime: val.ltime,
          id: val.id,
          from: Node::from_ref(val.from)?,
          filters,
          flags: val.flags,
          relay_factor: val.relay_factor,
          timeout: val.timeout,
          name: SmolStr::from(val.name),
          payload: Bytes::copy_from_slice(val.payload),
        })
      })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;

    len += 1 + self.ltime.encoded_len();
    len += 1 + self.id.encoded_len();
    len += 1 + self.from.encoded_len_with_length_delimited();
    len += self
      .filters
      .iter()
      .map(|f| 1 + f.encoded_len_with_length_delimited())
      .sum::<usize>();

    len += 1 + self.flags.bits().encoded_len();
    len += 1 + self.relay_factor.encoded_len();
    len += 1 + self.timeout.encoded_len();

    let nlen = self.name.len();

    if nlen != 0 {
      len += 1 + self.name.encoded_len_with_length_delimited();
    }

    let plen = self.payload.len();

    if plen != 0 {
      len += 1 + self.payload.encoded_len_with_length_delimited();
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

    let mut offset = 0;
    let buf_len = buf.len();

    bail!(self(offset, buf_len));
    buf[offset] = LTIME_BYTE;
    offset += 1;

    offset += self
      .ltime
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = ID_BYTE;
    offset += 1;

    offset += self
      .id
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = FROM_BYTE;
    offset += 1;

    offset += self
      .from
      .encode_length_delimited(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    for filter in self.filters.iter() {
      bail!(self(offset, buf_len));
      buf[offset] = FILTERS_BYTE;
      offset += 1;

      offset += filter
        .encode_length_delimited(&mut buf[offset..])
        .map_err(|e| e.update(self.encoded_len(), buf_len))?;
    }

    bail!(self(offset, buf_len));
    buf[offset] = FLAGS_BYTE;
    offset += 1;

    offset += <u32 as Data>::encode(&self.flags.bits(), &mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = RELAY_FACTOR_BYTE;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = self.relay_factor;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = TIMEOUT_BYTE;
    offset += 1;

    offset += self
      .timeout
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    if !self.name.is_empty() {
      bail!(self(offset, buf_len));
      buf[offset] = NAME_BYTE;
      offset += 1;

      offset += self
        .name
        .encode_length_delimited(&mut buf[offset..])
        .map_err(|e| e.update(self.encoded_len(), buf_len))?;
    }

    if !self.payload.is_empty() {
      bail!(self(offset, buf_len));
      buf[offset] = PAYLOAD_BYTE;
      offset += 1;

      offset += self
        .payload
        .encode_length_delimited(&mut buf[offset..])
        .map_err(|e| e.update(self.encoded_len(), buf_len))?;
    }

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq(offset, self.encoded_len());

    Ok(offset)
  }
}
