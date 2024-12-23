use byteorder::{ByteOrder, NetworkEndian};
use transformable::utils::encoded_u64_varint_len;

use crate::LamportTimeTransformError;

use super::{LamportTime, Transformable};

/// The message broadcasted after we join to
/// associated the node with a lamport clock
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JoinMessage<I> {
  /// The lamport time
  #[viewit(
    getter(const, attrs(doc = "Returns the lamport time for this message")),
    setter(
      const,
      attrs(doc = "Sets the lamport time for this message (Builder pattern)")
    )
  )]
  ltime: LamportTime,
  /// The id of the node
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the node")),
    setter(attrs(doc = "Sets the node (Builder pattern)"))
  )]
  id: I,
}

impl<I> JoinMessage<I> {
  /// Create a new join message
  pub fn new(ltime: LamportTime, id: I) -> Self {
    Self { ltime, id }
  }

  /// Set the lamport time
  #[inline]
  pub fn set_ltime(&mut self, ltime: LamportTime) -> &mut Self {
    self.ltime = ltime;
    self
  }

  /// Set the id of the node
  #[inline]
  pub fn set_id(&mut self, id: I) -> &mut Self {
    self.id = id;
    self
  }
}

/// Error that can occur when transforming a JoinMessage
#[derive(thiserror::Error)]
pub enum JoinMessageTransformError<I: Transformable> {
  /// Not enough bytes to decode JoinMessage
  #[error("not enough bytes to decode JoinMessage")]
  NotEnoughBytes,
  /// Encode buffer too small
  #[error("encode buffer too small")]
  EncodeBufferTooSmall,
  /// Error transforming Id
  #[error(transparent)]
  Id(I::Error),

  /// Error transforming LamportTime
  #[error(transparent)]
  LamportTime(#[from] LamportTimeTransformError),
}

impl<I: Transformable> core::fmt::Debug for JoinMessageTransformError<I> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self)
  }
}

impl<I> Transformable for JoinMessage<I>
where
  I: Transformable,
{
  type Error = JoinMessageTransformError<I>;

  fn encode(&self, dst: &mut [u8]) -> Result<usize, Self::Error> {
    let encoded_len = self.encoded_len();
    if dst.len() < encoded_len {
      return Err(Self::Error::EncodeBufferTooSmall);
    }

    let mut offset = 0;
    NetworkEndian::write_u32(&mut dst[offset..offset + 4], encoded_len as u32);
    offset += 4;

    offset += self.ltime.encode(&mut dst[offset..])?;
    offset += self
      .id
      .encode(&mut dst[offset..])
      .map_err(Self::Error::Id)?;

    debug_assert_eq!(
      offset, encoded_len,
      "expect write {} bytes, but actual write {} bytes",
      encoded_len, offset
    );
    Ok(offset)
  }

  fn encoded_len(&self) -> usize {
    4 + encoded_u64_varint_len(self.ltime.0) + self.id.encoded_len()
  }

  fn decode(src: &[u8]) -> Result<(usize, Self), Self::Error>
  where
    Self: Sized,
  {
    if src.len() < 4 {
      return Err(Self::Error::NotEnoughBytes);
    }

    let encoded_len = NetworkEndian::read_u32(&src[..4]) as usize;
    if src.len() < encoded_len {
      return Err(Self::Error::NotEnoughBytes);
    }

    let mut offset = 4;
    let (n, ltime) = LamportTime::decode(&src[offset..])?;
    offset += n;

    let (n, id) = I::decode(&src[offset..]).map_err(Self::Error::Id)?;
    offset += n;

    debug_assert_eq!(
      offset, encoded_len,
      "expect read {} bytes, but actual read {} bytes",
      encoded_len, offset
    );
    Ok((encoded_len, Self { ltime, id }))
  }
}

#[cfg(test)]
mod tests {
  use rand::{distributions::Alphanumeric, thread_rng, Rng};
  use smol_str::SmolStr;

  use super::*;

  impl JoinMessage<SmolStr> {
    fn random(size: usize) -> Self {
      let id = thread_rng()
        .sample_iter(Alphanumeric)
        .take(size)
        .collect::<Vec<u8>>();
      let id = String::from_utf8(id).unwrap().into();

      Self {
        ltime: LamportTime::random(),
        id,
      }
    }
  }

  #[test]
  fn test_transfrom_encode_decode() {
    futures::executor::block_on(async {
      for i in 0..100 {
        let filter = JoinMessage::random(i);
        let mut buf = vec![0; filter.encoded_len()];
        let encoded_len = filter.encode(&mut buf).unwrap();
        assert_eq!(encoded_len, filter.encoded_len());

        let (decoded_len, decoded) = JoinMessage::<SmolStr>::decode(&buf).unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);

        let (decoded_len, decoded) =
          JoinMessage::<SmolStr>::decode_from_reader(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);

        let (decoded_len, decoded) =
          JoinMessage::<SmolStr>::decode_from_async_reader(&mut futures::io::Cursor::new(&buf))
            .await
            .unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);
      }
    });
  }
}
