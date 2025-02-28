use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, OneOrMore, RepeatedDecoder, WireType,
  utils::{merge, skip, split},
};

use super::{super::LamportTime, UserEvent};

const LTIME_TAG: u8 = 1;
const EVENTS_TAG: u8 = 2;

const LTIME_BYTE: u8 = merge(WireType::Varint, LTIME_TAG);
const EVENTS_BYTE: u8 = merge(WireType::LengthDelimited, EVENTS_TAG);

/// Used to buffer events to prevent re-delivery
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct UserEvents {
  /// The lamport time
  #[viewit(
    getter(const, attrs(doc = "Returns the lamport time for this message")),
    setter(
      const,
      attrs(doc = "Sets the lamport time for this message (Builder pattern)")
    )
  )]
  ltime: LamportTime,

  /// The user events
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the user events")),
    setter(attrs(doc = "Sets the user events (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::types::arbitrary_impl::into::<Vec<UserEvent>, OneOrMore<UserEvent>>))]
  events: OneOrMore<UserEvent>,
}

/// The reference type for [`UserEvents`]
#[viewit::viewit(vis_all = "", getters(vis_all = "pub", style = "ref"), setters(skip))]
#[derive(Debug, Clone, Copy)]
pub struct UserEventsRef<'a> {
  /// The lamport time
  #[viewit(getter(const, attrs(doc = "Returns the lamport time for this message")))]
  ltime: LamportTime,

  /// The user events
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the bu user events")))]
  events: RepeatedDecoder<'a>,
}

impl<'a> DataRef<'a, UserEvents> for UserEventsRef<'a> {
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut ltime = None;
    let mut events_offsets = None;
    let mut num_events = 0;

    while offset < buf_len {
      match buf[offset] {
        LTIME_BYTE => {
          if ltime.is_some() {
            return Err(DecodeError::duplicate_field(
              "UserEvents",
              "ltime",
              LTIME_TAG,
            ));
          }
          offset += 1;
          let (size, val) = <LamportTime as DataRef<'_, LamportTime>>::decode(&buf[offset..])?;
          ltime = Some(val);
          offset += size;
        }
        EVENTS_BYTE => {
          let readed = super::skip(WireType::LengthDelimited, &buf[offset..])?;
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
        ltime: ltime.ok_or_else(|| DecodeError::missing_field("UserEvents", "ltime"))?,
        events: if let Some((start, end)) = events_offsets {
          RepeatedDecoder::new(EVENTS_TAG, WireType::LengthDelimited, buf)
            .with_nums(num_events)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(EVENTS_TAG, WireType::LengthDelimited, buf)
        },
      },
    ))
  }
}

impl Data for UserEvents {
  type Ref<'a> = UserEventsRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    val
      .events
      .iter::<UserEvent>()
      .map(|ev| ev.and_then(UserEvent::from_ref))
      .collect::<Result<OneOrMore<_>, DecodeError>>()
      .map(|events| Self {
        ltime: val.ltime,
        events,
      })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;
    len += 1 + self.ltime.encoded_len();
    len += self
      .events
      .iter()
      .map(|e| 1 + e.encoded_len_with_length_delimited())
      .sum::<usize>();
    len
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let buf_len = buf.len();
    let mut offset = 0;

    if offset >= buf_len {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len(),
        buf_len,
      ));
    }

    buf[offset] = LTIME_BYTE;
    offset += 1;

    offset += self
      .ltime
      .encode(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len(), buf_len))?;

    self
      .events
      .iter()
      .try_fold(&mut offset, |offset, ev| {
        *offset += ev.encode_length_delimited(&mut buf[*offset..])?;

        Ok(offset)
      })
      .map(|offset| *offset)
      .map_err(|e: EncodeError| e.update(self.encoded_len(), buf_len))
  }
}
