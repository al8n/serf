use memberlist_proto::{
  Data, DataRef, DecodeError, EncodeError, Node, WireType,
  bytes::Bytes,
  utils::{merge, skip, split},
};

use crate::LamportTime;

use super::QueryFlag;

const LTIME_TAG: u8 = 1;
const ID_TAG: u8 = 2;
const FROM_TAG: u8 = 3;
const FLAGS_TAG: u8 = 4;
const PAYLOAD_TAG: u8 = 5;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const ID_BYTE: u8 = merge(WireType::Varint, ID_TAG);
const FROM_BYTE: u8 = merge(WireType::LengthDelimited, FROM_TAG);
const FLAGS_BYTE: u8 = merge(WireType::Varint, FLAGS_TAG);
const PAYLOAD_BYTE: u8 = merge(WireType::LengthDelimited, PAYLOAD_TAG);

/// Query response message
#[viewit::viewit(getters(style = "ref"), setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct QueryResponseMessage<I, A> {
  /// Event lamport time
  #[viewit(
    getter(const, attrs(doc = "Returns the lamport time for this message")),
    setter(
      const,
      attrs(doc = "Sets the lamport time for this message (Builder pattern)")
    )
  )]
  ltime: LamportTime,
  /// query id
  #[viewit(
    getter(const, attrs(doc = "Returns the query id")),
    setter(attrs(doc = "Sets the query id (Builder pattern)"))
  )]
  id: u32,
  /// node
  #[viewit(
    getter(const, attrs(doc = "Returns the from node")),
    setter(attrs(doc = "Sets the from node (Builder pattern)"))
  )]
  from: Node<I, A>,
  /// Used to provide various flags
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the flags")),
    setter(attrs(doc = "Sets the flags (Builder pattern)"))
  )]
  flags: QueryFlag,
  /// Optional response payload
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the payload")),
    setter(attrs(doc = "Sets the payload (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::into::<Vec<u8>, Bytes>))]
  payload: Bytes,
}

impl<I, A> QueryResponseMessage<I, A> {
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

/// The reference type to a query response message
#[viewit::viewit(vis_all = "", getters(vis_all = "pub", style = "ref"), setters(skip))]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct QueryResponseMessageRef<'a, I, A> {
  /// Event lamport time
  #[viewit(getter(const, attrs(doc = "Returns the lamport time for this message")))]
  ltime: LamportTime,
  /// query id
  #[viewit(getter(const, attrs(doc = "Returns the query id")))]
  id: u32,
  /// node
  #[viewit(getter(const, attrs(doc = "Returns the from node")))]
  from: Node<I, A>,
  /// Used to provide various flags
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the flags")))]
  flags: QueryFlag,
  /// Optional response payload
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the payload")))]
  payload: &'a [u8],
}

impl<'a, I, A> DataRef<'a, QueryResponseMessage<I, A>>
  for QueryResponseMessageRef<'a, I::Ref<'a>, A::Ref<'a>>
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
    let mut flags = None;
    let mut payload = None;

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryResponseMessage",
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
            return Err(DecodeError::duplicate_field(
              "QueryResponseMessage",
              "id",
              ID_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <u32 as DataRef<'_, u32>>::decode(&buf[offset..])?;
          offset += o;
          id = Some(v);
        }
        FROM_BYTE => {
          if from.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryResponseMessage",
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
        FLAGS_BYTE => {
          if flags.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryResponseMessage",
              "flags",
              FLAGS_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <u32 as DataRef<'_, u32>>::decode(&buf[offset..])?;
          offset += o;
          flags = Some(QueryFlag::from_bits_retain(v));
        }
        PAYLOAD_BYTE => {
          if payload.is_some() {
            return Err(DecodeError::duplicate_field(
              "QueryResponseMessage",
              "payload",
              PAYLOAD_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <&[u8] as DataRef<'_, Bytes>>::decode(&buf[offset..])?;
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

    Ok((
      offset,
      Self {
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("QueryResponseMessage", "ltime"))?,
        id: id.ok_or_else(|| DecodeError::missing_field("QueryResponseMessage", "id"))?,
        from: from.ok_or_else(|| DecodeError::missing_field("QueryResponseMessage", "from"))?,
        flags: flags.ok_or_else(|| DecodeError::missing_field("QueryResponseMessage", "flags"))?,
        payload: payload.unwrap_or_default(),
      },
    ))
  }
}

impl<I, A> Data for QueryResponseMessage<I, A>
where
  I: Data,
  A: Data,
{
  type Ref<'a> = QueryResponseMessageRef<'a, I::Ref<'a>, A::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      ltime: val.ltime,
      id: val.id,
      from: Node::from_ref(val.from)?,
      flags: val.flags,
      payload: Bytes::copy_from_slice(val.payload),
    })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 1 + self.ltime.encoded_len();
    len += 1 + self.id.encoded_len();
    len += 1 + self.from.encoded_len_with_length_delimited();
    len += 1 + self.flags.bits().encoded_len(); // flags
    let plen = self.payload.len();
    if plen > 0 {
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

    let buf_len = buf.len();
    let mut offset = 0;

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

    bail!(self(offset, buf_len));
    buf[offset] = FLAGS_BYTE;
    offset += 1;

    offset += <u32 as Data>::encode(&self.flags.bits(), &mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

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
    super::super::debug_assert_write_eq(offset, self.encoded_len());

    Ok(offset)
  }
}
