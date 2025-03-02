use indexmap::{IndexMap, IndexSet};
use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, RepeatedDecoder, TinyVec, TupleEncoder, WireType,
  utils::{merge, skip, split},
};

use super::{LamportTime, UserEvents};

/// Used when doing a state exchange. This
/// is a relatively large message, but is sent infrequently
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(bound(
    serialize = "I: core::cmp::Eq + core::hash::Hash + serde::Serialize",
    deserialize = "I: core::cmp::Eq + core::hash::Hash + serde::Deserialize<'de>"
  ))
)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(
  feature = "arbitrary",
  arbitrary(bound = "I: arbitrary::Arbitrary<'arbitrary> + core::cmp::Eq + core::hash::Hash")
)]
pub struct PushPullMessage<I> {
  /// Current node lamport time
  #[viewit(
    getter(const, style = "move", attrs(doc = "Returns the lamport time")),
    setter(const, attrs(doc = "Sets the lamport time (Builder pattern)"))
  )]
  ltime: LamportTime,
  /// Maps the node to its status time
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the maps the node to its status time")
    ),
    setter(attrs(doc = "Sets the maps the node to its status time (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::arbitrary_indexmap))]
  status_ltimes: IndexMap<I, LamportTime>,
  /// List of left nodes
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the list of left nodes")),
    setter(attrs(doc = "Sets the list of left nodes (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::arbitrary_indexset))]
  left_members: IndexSet<I>,
  /// Lamport time for event clock
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the lamport time for event clock")
    ),
    setter(
      const,
      attrs(doc = "Sets the lamport time for event clock (Builder pattern)")
    )
  )]
  event_ltime: LamportTime,
  /// Recent events
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the recent events")),
    setter(attrs(doc = "Sets the recent events (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::into::<Vec<UserEvents>, TinyVec<UserEvents>>))]
  events: TinyVec<UserEvents>,
  /// Lamport time for query clock
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the lamport time for query clock")
    ),
    setter(
      const,
      attrs(doc = "Sets the lamport time for query clock (Builder pattern)")
    )
  )]
  query_ltime: LamportTime,
}

impl<I> PartialEq for PushPullMessage<I>
where
  I: core::hash::Hash + Eq,
{
  fn eq(&self, other: &Self) -> bool {
    self.ltime == other.ltime
      && self.status_ltimes == other.status_ltimes
      && self.left_members == other.left_members
      && self.event_ltime == other.event_ltime
      && self.events == other.events
      && self.query_ltime == other.query_ltime
  }
}

const LTIME_TAG: u8 = 1;
const STATUS_LTIMES_TAG: u8 = 2;
const LEFT_MEMBERS_TAG: u8 = 3;
const EVENT_LTIME_TAG: u8 = 4;
const EVENTS_TAG: u8 = 5;
const QUERY_LTIME_TAG: u8 = 6;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const STATUS_LTIMES_BYTE: u8 = merge(WireType::LengthDelimited, STATUS_LTIMES_TAG);
const EVENT_LTIME_BYTE: u8 = merge(WireType::Varint, EVENT_LTIME_TAG);
const EVENTS_BYTE: u8 = merge(WireType::LengthDelimited, EVENTS_TAG);
const QUERY_LTIME_BYTE: u8 = merge(WireType::Varint, QUERY_LTIME_TAG);

#[inline]
const fn left_members_byte<I: Data>() -> u8 {
  merge(I::WIRE_TYPE, LEFT_MEMBERS_TAG)
}

/// Used when doing a state exchange. This
/// is a relatively large message, but is sent infrequently
#[viewit::viewit(vis_all = "", getters(vis_all = "pub"), setters(skip))]
#[derive(Debug)]
pub struct PushPullMessageRef<'a, I> {
  /// Current node lamport time
  #[viewit(getter(const, style = "move", attrs(doc = "Returns the lamport time")))]
  ltime: LamportTime,
  /// Maps the node to its status time
  #[viewit(getter(
    const,
    style = "ref",
    attrs(doc = "Returns the maps the node to its status time")
  ))]
  status_ltimes: RepeatedDecoder<'a>,
  /// List of left nodes
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the list of left nodes")))]
  left_members: RepeatedDecoder<'a>,
  /// Lamport time for event clock
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the lamport time for event clock")
  ))]
  event_ltime: LamportTime,
  /// Recent events
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the recent events")))]
  events: RepeatedDecoder<'a>,
  /// Lamport time for query clock
  #[viewit(getter(
    const,
    style = "move",
    attrs(doc = "Returns the lamport time for query clock")
  ))]
  query_ltime: LamportTime,
  #[viewit(getter(skip))]
  _m: core::marker::PhantomData<I>,
}

impl<I> Clone for PushPullMessageRef<'_, I> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I> Copy for PushPullMessageRef<'_, I> {}

impl<'a, I> DataRef<'a, PushPullMessage<I>> for PushPullMessageRef<'a, I::Ref<'a>>
where
  I: Data + Eq + core::hash::Hash,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut ltime = None;
    let mut status_ltimes_offsets = None;
    let mut num_status_ltimes = 0;
    let mut left_members_offsets = None;
    let mut num_left_members = 0;
    let mut event_ltime = None;
    let mut events_offsets = None;
    let mut num_events = 0;
    let mut query_ltime = None;

    let left_members_byte = left_members_byte::<I>();

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "PushPullMessage",
              "ltime",
              LTIME_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += o;
          ltime = Some(v);
        }
        STATUS_LTIMES_BYTE => {
          offset += 1;
          let readed = skip(WireType::LengthDelimited, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = status_ltimes_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            status_ltimes_offsets = Some((offset - 1, offset + readed));
          }
          num_status_ltimes += 1;
          offset += readed;
        }
        b if b == left_members_byte => {
          offset += 1;
          let readed = skip(I::WIRE_TYPE, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = left_members_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            left_members_offsets = Some((offset - 1, offset + readed));
          }
          num_left_members += 1;
          offset += readed;
        }
        EVENT_LTIME_BYTE => {
          if event_ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "PushPullMessage",
              "event_ltime",
              EVENT_LTIME_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += o;
          event_ltime = Some(v);
        }
        EVENTS_BYTE => {
          offset += 1;
          let readed = skip(WireType::LengthDelimited, &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = events_offsets {
            if *fnso > offset {
              *fnso = offset - 1;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            events_offsets = Some((offset - 1, offset + readed));
          }
          num_events += 1;
          offset += readed;
        }
        QUERY_LTIME_BYTE => {
          if query_ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "PushPullMessage",
              "query_ltime",
              QUERY_LTIME_TAG,
            ));
          }

          offset += 1;
          let (o, v) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          offset += o;
          query_ltime = Some(v);
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
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("PushPullMessage", "ltime"))?,
        status_ltimes: if let Some((start, end)) = status_ltimes_offsets {
          RepeatedDecoder::new(STATUS_LTIMES_TAG, WireType::LengthDelimited, buf)
            .with_nums(num_status_ltimes)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(STATUS_LTIMES_TAG, WireType::LengthDelimited, buf)
        },
        left_members: if let Some((start, end)) = left_members_offsets {
          RepeatedDecoder::new(LEFT_MEMBERS_TAG, I::WIRE_TYPE, buf)
            .with_nums(num_left_members)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(LEFT_MEMBERS_TAG, I::WIRE_TYPE, buf)
        },
        event_ltime: event_ltime
          .ok_or_else(|| DecodeError::missing_field("PushPullMessage", "event_ltime"))?,
        events: if let Some((start, end)) = events_offsets {
          RepeatedDecoder::new(EVENTS_TAG, WireType::LengthDelimited, buf)
            .with_nums(num_events)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(EVENTS_TAG, WireType::LengthDelimited, buf)
        },
        query_ltime: query_ltime
          .ok_or_else(|| DecodeError::missing_field("PushPullMessage", "query_ltime"))?,
        _m: core::marker::PhantomData,
      },
    ))
  }
}

impl<I> Data for PushPullMessage<I>
where
  I: Data + Eq + core::hash::Hash,
{
  type Ref<'a> = PushPullMessageRef<'a, I::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      ltime: val.ltime,
      status_ltimes: val
        .status_ltimes
        .iter::<(I, LamportTime)>()
        .map(|res| res.and_then(Data::from_ref))
        .collect::<Result<IndexMap<I, LamportTime>, DecodeError>>()?,
      left_members: val
        .left_members
        .iter::<I>()
        .map(|res| res.and_then(Data::from_ref))
        .collect::<Result<IndexSet<I>, DecodeError>>()?,
      event_ltime: val.event_ltime,
      events: val
        .events
        .iter::<UserEvents>()
        .map(|res| res.and_then(Data::from_ref))
        .collect::<Result<TinyVec<_>, DecodeError>>()?,
      query_ltime: val.query_ltime,
    })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0usize;

    len += 1 + self.ltime.encoded_len();

    len += self
      .status_ltimes
      .iter()
      .map(|(k, v)| 1 + TupleEncoder::new(k, v).encoded_len_with_length_delimited())
      .sum::<usize>();

    len += self
      .left_members
      .iter()
      .map(|id| 1 + id.encoded_len_with_length_delimited())
      .sum::<usize>();

    len += 1 + self.event_ltime.encoded_len();

    len += self
      .events
      .iter()
      .map(|e| 1 + e.encoded_len_with_length_delimited())
      .sum::<usize>();

    len += 1 + self.query_ltime.encoded_len();

    len
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    macro_rules! bail {
      ($this:ident($offset:expr, $len:ident)) => {
        if $offset >= $len {
          return Err(EncodeError::insufficient_buffer($this.encoded_len(), $len));
        }
      };
    }

    let mut offset = 0;
    let buf_len = buf.len();

    bail!(self(offset, buf_len));
    buf[offset] = LTIME_BYTE;
    offset += 1;
    offset += self.ltime.encode(&mut buf[offset..])?;

    self
      .status_ltimes
      .iter()
      .try_fold(&mut offset, |off, (k, v)| {
        bail!(self(*off, buf_len));
        buf[*off] = STATUS_LTIMES_BYTE;
        *off += 1;
        *off += TupleEncoder::new(k, v).encode_with_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len(), buf_len))?;

    let left_members_byte = left_members_byte::<I>();
    self
      .left_members
      .iter()
      .try_fold(&mut offset, |off, id| {
        bail!(self(*off, buf_len));
        buf[*off] = left_members_byte;
        *off += 1;
        *off += id.encode_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = EVENT_LTIME_BYTE;
    offset += 1;
    offset += self.event_ltime.encode(&mut buf[offset..])?;

    self
      .events
      .iter()
      .try_fold(&mut offset, |off, e| {
        bail!(self(*off, buf_len));
        buf[*off] = EVENTS_BYTE;
        *off += 1;
        *off += e.encode_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = QUERY_LTIME_BYTE;
    offset += 1;
    offset += self.query_ltime.encode(&mut buf[offset..])?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len());

    Ok(offset)
  }
}

/// Used when doing a state exchange. This
/// is a relatively large message, but is sent infrequently
#[viewit::viewit(getters(skip), setters(skip))]
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PushPullMessageBorrow<'a, I> {
  /// Current node lamport time
  ltime: LamportTime,
  /// Maps the node to its status time
  status_ltimes: &'a IndexMap<I, LamportTime>,
  /// List of left nodes
  left_members: &'a IndexSet<I>,
  /// Lamport time for event clock
  event_ltime: LamportTime,
  /// Recent events
  events: &'a [Option<UserEvents>],
  /// Lamport time for query clock
  query_ltime: LamportTime,
}

impl<I> Clone for PushPullMessageBorrow<'_, I> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I> Copy for PushPullMessageBorrow<'_, I> {}

impl<I> PushPullMessageBorrow<'_, I>
where
  I: Data,
{
  pub(super) fn encoded_len_in(&self) -> usize {
    let mut len = 0usize;

    len += 1 + self.ltime.encoded_len();

    len += self
      .status_ltimes
      .iter()
      .map(|(k, v)| 1 + TupleEncoder::new(k, v).encoded_len_with_length_delimited())
      .sum::<usize>();

    len += self
      .left_members
      .iter()
      .map(|id| 1 + id.encoded_len_with_length_delimited())
      .sum::<usize>();
    len += 1 + self.event_ltime.encoded_len();
    len += 1
      + self
        .events
        .iter()
        .filter_map(|e| {
          e.as_ref()
            .map(|e| 1 + e.encoded_len_with_length_delimited())
        })
        .sum::<usize>();
    len += 1 + self.query_ltime.encoded_len();

    len
  }

  pub(super) fn encode_in(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    macro_rules! bail {
      ($this:ident($offset:expr, $len:ident)) => {
        if $offset >= $len {
          return Err(EncodeError::insufficient_buffer(
            $this.encoded_len_in(),
            $len,
          ));
        }
      };
    }

    let mut offset = 0;
    let buf_len = buf.len();

    bail!(self(offset, buf_len));
    buf[offset] = LTIME_BYTE;
    offset += 1;
    offset += self.ltime.encode(&mut buf[offset..])?;

    bail!(self(offset, buf_len));
    buf[offset] = STATUS_LTIMES_BYTE;
    offset += 1;

    self
      .status_ltimes
      .iter()
      .try_fold(&mut offset, |off, (k, v)| {
        bail!(self(*off, buf_len));
        buf[*off] = STATUS_LTIMES_BYTE;
        *off += 1;
        *off += TupleEncoder::new(k, v).encode_with_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len_in(), buf_len))?;

    let left_members_byte = left_members_byte::<I>();
    self
      .left_members
      .iter()
      .try_fold(&mut offset, |off, id| {
        bail!(self(*off, buf_len));
        buf[*off] = left_members_byte;
        *off += 1;
        *off += id.encode_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len_in(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = EVENT_LTIME_BYTE;
    offset += 1;
    offset += self.event_ltime.encode(&mut buf[offset..])?;

    self
      .events
      .iter()
      .filter_map(|e| e.as_ref())
      .try_fold(&mut offset, |off, e| {
        bail!(self(*off, buf_len));
        buf[*off] = EVENTS_BYTE;
        *off += 1;
        *off += e.encode_length_delimited(&mut buf[*off..])?;
        Ok(off)
      })
      .map_err(|e: EncodeError| e.update(self.encoded_len_in(), buf_len))?;

    bail!(self(offset, buf_len));
    buf[offset] = QUERY_LTIME_BYTE;
    offset += 1;
    offset += self.query_ltime.encode(&mut buf[offset..])?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len_in());

    Ok(offset)
  }
}
