use byteorder::{ByteOrder, NetworkEndian};
use indexmap::{IndexMap, IndexSet};
use memberlist_types::TinyVec;
use transformable::Transformable;

use super::{LamportTime, LamportTimeTransformError, UserEvents, UserEventsTransformError};

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
  status_ltimes: IndexMap<I, LamportTime>,
  /// List of left nodes
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the list of left nodes")),
    setter(attrs(doc = "Sets the list of left nodes (Builder pattern)"))
  )]
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
  events: TinyVec<Option<UserEvents>>,
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

/// Used when doing a state exchange. This
/// is a relatively large message, but is sent infrequently
#[viewit::viewit(getters(skip), setters(skip))]
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PushPullMessageRef<'a, I> {
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

impl<I> Clone for PushPullMessageRef<'_, I> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I> Copy for PushPullMessageRef<'_, I> {}

impl<'a, I> From<&'a PushPullMessage<I>> for PushPullMessageRef<'a, I> {
  #[inline]
  fn from(msg: &'a PushPullMessage<I>) -> Self {
    Self {
      ltime: msg.ltime,
      status_ltimes: &msg.status_ltimes,
      left_members: &msg.left_members,
      event_ltime: msg.event_ltime,
      events: &msg.events,
      query_ltime: msg.query_ltime,
    }
  }
}

impl<'a, I> From<&'a mut PushPullMessage<I>> for PushPullMessageRef<'a, I> {
  #[inline]
  fn from(msg: &'a mut PushPullMessage<I>) -> Self {
    Self {
      ltime: msg.ltime,
      status_ltimes: &msg.status_ltimes,
      left_members: &msg.left_members,
      event_ltime: msg.event_ltime,
      events: &msg.events,
      query_ltime: msg.query_ltime,
    }
  }
}

impl<I> super::Encodable for PushPullMessageRef<'_, I>
where
  I: Transformable,
{
  type Error = PushPullMessageTransformError<I>;

  /// Returns the encoded length of the message
  fn encoded_len(&self) -> usize {
    4 + Transformable::encoded_len(&self.ltime)
      + 4
      + self
        .status_ltimes
        .iter()
        .map(|(k, v)| Transformable::encoded_len(k) + Transformable::encoded_len(v))
        .sum::<usize>()
      + 4
      + self
        .left_members
        .iter()
        .map(Transformable::encoded_len)
        .sum::<usize>()
      + Transformable::encoded_len(&self.event_ltime)
      + 4
      + self
        .events
        .iter()
        .map(|e| match e {
          Some(e) => 1 + Transformable::encoded_len(e),
          None => 1,
        })
        .sum::<usize>()
      + Transformable::encoded_len(&self.query_ltime)
  }

  /// Encodes the message into the given buffer
  fn encode(&self, dst: &mut [u8]) -> Result<usize, PushPullMessageTransformError<I>> {
    let encoded_len = self.encoded_len();
    if dst.len() < encoded_len {
      return Err(PushPullMessageTransformError::BufferTooSmall);
    }

    let mut offset = 0;
    NetworkEndian::write_u32(&mut dst[offset..offset + 4], encoded_len as u32);
    offset += 4;

    offset += Transformable::encode(&self.ltime, &mut dst[offset..])?;
    let len = self.status_ltimes.len() as u32;
    NetworkEndian::write_u32(&mut dst[offset..offset + 4], len);
    offset += 4;
    for (node, ltime) in self.status_ltimes.iter() {
      offset += Transformable::encode(node, &mut dst[offset..]).map_err(Self::Error::Id)?;
      offset += Transformable::encode(ltime, &mut dst[offset..])?;
    }

    let len = self.left_members.len() as u32;
    NetworkEndian::write_u32(&mut dst[offset..offset + 4], len);
    offset += 4;
    for node in self.left_members.iter() {
      offset += Transformable::encode(node, &mut dst[offset..]).map_err(Self::Error::Id)?;
    }

    offset += Transformable::encode(&self.event_ltime, &mut dst[offset..])?;
    let len = self.events.len() as u32;
    NetworkEndian::write_u32(&mut dst[offset..offset + 4], len);
    offset += 4;
    for e in self.events.iter() {
      match e {
        Some(e) => {
          dst[offset] = 1;
          offset += 1;
          offset += Transformable::encode(e, &mut dst[offset..])?;
        }
        None => {
          dst[offset] = 0;
          offset += 1;
        }
      }
    }

    offset += Transformable::encode(&self.query_ltime, &mut dst[offset..])?;

    debug_assert_eq!(
      offset, encoded_len,
      "expect write {} bytes, but actual write {} bytes",
      encoded_len, offset
    );

    Ok(offset)
  }
}

/// Error that can occur when transforming a [`PushPullMessage`] or [`PushPullMessageRef`].
#[derive(thiserror::Error)]
pub enum PushPullMessageTransformError<I>
where
  I: Transformable,
{
  /// Not enough bytes to decode [`PushPullMessage`]
  #[error("not enough bytes to decode PushPullMessage")]
  NotEnoughBytes,
  /// Encode buffer too small
  #[error("encode buffer too small")]
  BufferTooSmall,
  /// Error transforming [`I`]
  #[error(transparent)]
  Id(I::Error),
  /// Error when we do not have enough nodes
  #[error("expect {expect} nodes, but actual decode {got} nodes")]
  MissingLeftMember {
    /// Expect
    expect: usize,
    /// Actual
    got: usize,
  },
  /// Error when we do not have enough status time
  #[error("expect {expect} status time, but actual decode {got} status time")]
  MissingNodeStatusTime {
    /// Expect
    expect: usize,
    /// Actual
    got: usize,
  },
  /// Error transforming [`LamportTime`]
  #[error(transparent)]
  LamportTime(#[from] LamportTimeTransformError),
  /// Error transforming [`UserEvents`]
  #[error(transparent)]
  UserEvents(#[from] UserEventsTransformError),
  /// Error when we do not have enough events
  #[error("expect {expect} events, but actual decode {got} events")]
  MissingEvents {
    /// Expect
    expect: usize,
    /// Actual
    got: usize,
  },
}

impl<I> core::fmt::Debug for PushPullMessageTransformError<I>
where
  I: Transformable,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self)
  }
}

impl<I> Transformable for PushPullMessage<I>
where
  I: Transformable + core::hash::Hash + Eq,
{
  type Error = PushPullMessageTransformError<I>;

  fn encode(&self, dst: &mut [u8]) -> Result<usize, Self::Error> {
    super::Encodable::encode(&PushPullMessageRef::from(self), dst)
  }

  fn encoded_len(&self) -> usize {
    super::Encodable::encoded_len(&PushPullMessageRef::from(self))
  }

  fn decode(src: &[u8]) -> Result<(usize, Self), Self::Error>
  where
    Self: Sized,
  {
    let src_len = src.len();
    if src_len < 4 {
      return Err(PushPullMessageTransformError::NotEnoughBytes);
    }

    let encoded_len = NetworkEndian::read_u32(&src[..4]) as usize;
    if src_len < encoded_len {
      return Err(PushPullMessageTransformError::NotEnoughBytes);
    }

    let mut offset = 4;
    let (n, ltime) = LamportTime::decode(&src[offset..])?;
    offset += n;

    let len = NetworkEndian::read_u32(&src[offset..offset + 4]) as usize;
    offset += 4;

    let mut status_ltimes = IndexMap::with_capacity(len);
    for _ in 0..len {
      let (n, node) = I::decode(&src[offset..]).map_err(Self::Error::Id)?;
      offset += n;
      let (n, ltime) = LamportTime::decode(&src[offset..])?;
      offset += n;
      status_ltimes.insert(node, ltime);
    }

    let len = NetworkEndian::read_u32(&src[offset..offset + 4]) as usize;
    offset += 4;

    let mut left_members = IndexSet::with_capacity(len);
    for _ in 0..len {
      let (n, node) = I::decode(&src[offset..]).map_err(Self::Error::Id)?;
      offset += n;
      left_members.insert(node);
    }

    let (n, event_ltime) = LamportTime::decode(&src[offset..])?;
    offset += n;

    let len = NetworkEndian::read_u32(&src[offset..offset + 4]) as usize;
    offset += 4;

    let mut events = TinyVec::with_capacity(len);
    for _ in 0..len {
      let has_event = src[offset];
      offset += 1;
      if has_event == 1 {
        let (n, event) = UserEvents::decode(&src[offset..])?;
        offset += n;
        events.push(Some(event));
      } else {
        events.push(None);
      }
    }

    let (n, query_ltime) = LamportTime::decode(&src[offset..])?;
    offset += n;

    debug_assert_eq!(
      offset, encoded_len,
      "expect read {} bytes, but actual read {} bytes",
      encoded_len, offset
    );

    Ok((
      encoded_len,
      PushPullMessage {
        ltime,
        status_ltimes,
        left_members,
        event_ltime,
        events,
        query_ltime,
      },
    ))
  }
}

#[cfg(test)]
mod tests {
  use rand::{distributions::Alphanumeric, thread_rng, Rng};
  use smol_str::SmolStr;

  use super::*;

  impl PushPullMessage<SmolStr> {
    fn random(size: usize) -> Self {
      let mut status_ltimes = IndexMap::new();
      for _ in 0..size {
        let id = thread_rng()
          .sample_iter(Alphanumeric)
          .take(size)
          .collect::<Vec<u8>>();
        let id = String::from_utf8(id).unwrap().into();

        status_ltimes.insert(id, LamportTime::random());
      }

      let mut left_members = IndexSet::new();
      for _ in 0..size {
        let id = thread_rng()
          .sample_iter(Alphanumeric)
          .take(size)
          .collect::<Vec<u8>>();
        let id = String::from_utf8(id).unwrap().into();
        left_members.insert(id);
      }

      let mut events = TinyVec::new();
      for i in 0..size {
        if i % 2 == 0 {
          events.push(None);
        } else {
          events.push(Some(UserEvents::random(size, size % 10)));
        }
      }

      Self {
        ltime: LamportTime::random(),
        status_ltimes,
        left_members,
        event_ltime: LamportTime::random(),
        events,
        query_ltime: LamportTime::random(),
      }
    }
  }

  #[test]
  fn test_push_pull_message_transform() {
    futures::executor::block_on(async {
      for i in 0..100 {
        let msg = PushPullMessage::random(i);
        let mut buf = vec![0; msg.encoded_len()];
        let encoded_len = msg.encode(&mut buf).unwrap();
        assert_eq!(encoded_len, msg.encoded_len());

        let (decoded_len, decoded) = PushPullMessage::<SmolStr>::decode(&buf).unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, msg);

        let (decoded_len, decoded) =
          PushPullMessage::<SmolStr>::decode_from_reader(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, msg);

        let (decoded_len, decoded) =
          PushPullMessage::<SmolStr>::decode_from_async_reader(&mut futures::io::Cursor::new(&buf))
            .await
            .unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, msg);
      }
    });
  }
}
