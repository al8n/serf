use std::sync::Arc;

use byteorder::{ByteOrder, NetworkEndian};
use memberlist_types::CheapClone;

use super::{
  DelegateVersion, MemberlistDelegateVersion, MemberlistProtocolVersion, Node, NodeTransformError,
  ProtocolVersion, Tags, TagsTransformError, Transformable, UnknownDelegateVersion,
  UnknownMemberlistDelegateVersion, UnknownMemberlistProtocolVersion, UnknownProtocolVersion,
};

/// The member status.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, bytemuck::NoUninit)]
#[repr(u8)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum MemberStatus {
  /// None status
  None = 0,
  /// Alive status
  Alive = 1,
  /// Leaving status
  Leaving = 2,
  /// Left status
  Left = 3,
  /// Failed status
  Failed = 4,
}

impl core::fmt::Display for MemberStatus {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.as_str())
  }
}

impl TryFrom<u8> for MemberStatus {
  type Error = UnknownMemberStatus;

  fn try_from(value: u8) -> Result<Self, Self::Error> {
    match value {
      0 => Ok(Self::None),
      1 => Ok(Self::Alive),
      2 => Ok(Self::Leaving),
      3 => Ok(Self::Left),
      4 => Ok(Self::Failed),
      _ => Err(UnknownMemberStatus(value)),
    }
  }
}

impl MemberStatus {
  /// Get the string representation of the member status
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::None => "none",
      Self::Alive => "alive",
      Self::Leaving => "leaving",
      Self::Left => "left",
      Self::Failed => "failed",
    }
  }
}

/// Unknown member status
#[derive(Debug, Copy, Clone, thiserror::Error)]
#[error("Unknown member status: {0}")]
pub struct UnknownMemberStatus(u8);

/// A single member of the Serf cluster.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Member<I, A> {
  /// The node
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the node")),
    setter(attrs(doc = "Sets the node (Builder pattern)"))
  )]
  node: Node<I, A>,
  /// The tags
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the tags")),
    setter(attrs(doc = "Sets the tags (Builder pattern)"))
  )]
  tags: Arc<Tags>,
  /// The status
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the status")),
    setter(attrs(doc = "Sets the status (Builder pattern)"))
  )]
  status: MemberStatus,
  /// The memberlist protocol version
  #[viewit(
    getter(const, attrs(doc = "Returns the memberlist protocol version")),
    setter(
      const,
      attrs(doc = "Sets the memberlist protocol version (Builder pattern)")
    )
  )]
  memberlist_protocol_version: MemberlistProtocolVersion,
  /// The memberlist delegate version
  #[viewit(
    getter(const, attrs(doc = "Returns the memberlist delegate version")),
    setter(
      const,
      attrs(doc = "Sets the memberlist delegate version (Builder pattern)")
    )
  )]
  memberlist_delegate_version: MemberlistDelegateVersion,

  /// The serf protocol version
  #[viewit(
    getter(const, attrs(doc = "Returns the serf protocol version")),
    setter(
      const,
      attrs(doc = "Sets the serf protocol version (Builder pattern)")
    )
  )]
  protocol_version: ProtocolVersion,
  /// The serf delegate version
  #[viewit(
    getter(const, attrs(doc = "Returns the serf delegate version")),
    setter(
      const,
      attrs(doc = "Sets the serf delegate version (Builder pattern)")
    )
  )]
  delegate_version: DelegateVersion,
}

impl<I, A> Member<I, A> {
  /// Create a new member with the given node, tags, and status.
  /// Other fields are set to their default values.
  #[inline]
  pub fn new(node: Node<I, A>, tags: Tags, status: MemberStatus) -> Self {
    Self {
      node,
      tags: Arc::new(tags),
      status,
      memberlist_protocol_version: MemberlistProtocolVersion::V1,
      memberlist_delegate_version: MemberlistDelegateVersion::V1,
      protocol_version: ProtocolVersion::V1,
      delegate_version: DelegateVersion::V1,
    }
  }
}

impl<I: Clone, A: Clone> Clone for Member<I, A> {
  fn clone(&self) -> Self {
    Self {
      node: self.node.clone(),
      tags: self.tags.clone(),
      status: self.status,
      memberlist_protocol_version: self.memberlist_protocol_version,
      memberlist_delegate_version: self.memberlist_delegate_version,
      protocol_version: self.protocol_version,
      delegate_version: self.delegate_version,
    }
  }
}

impl<I: CheapClone, A: CheapClone> CheapClone for Member<I, A> {
  fn cheap_clone(&self) -> Self {
    Self {
      node: self.node.cheap_clone(),
      tags: self.tags.cheap_clone(),
      status: self.status,
      memberlist_protocol_version: self.memberlist_protocol_version,
      memberlist_delegate_version: self.memberlist_delegate_version,
      protocol_version: self.protocol_version,
      delegate_version: self.delegate_version,
    }
  }
}

/// Error transforming the [`Member`]
#[derive(thiserror::Error)]
pub enum MemberTransformError<I, A>
where
  I: Transformable,
  A: Transformable,
{
  /// Error transforming the `node` field
  #[error(transparent)]
  Node(#[from] NodeTransformError<I, A>),
  /// Error transforming the `tags` field
  #[error(transparent)]
  Tags(#[from] TagsTransformError),
  /// Error transforming the `status` field
  #[error(transparent)]
  MemberStatus(#[from] UnknownMemberStatus),
  /// Encode buffer too small
  #[error("encode buffer too small")]
  BufferTooSmall,
  /// Not enough bytes to decode
  #[error("not enough bytes to decode `Member`")]
  NotEnoughBytes,

  /// Error transforming the `memberlist_protocol_version` field
  #[error(transparent)]
  MemberlistProtocolVersion(#[from] UnknownMemberlistProtocolVersion),

  /// Error transforming the `memberlist_delegate_version` field
  #[error(transparent)]
  MemberlistDelegateVersion(#[from] UnknownMemberlistDelegateVersion),

  /// Error transforming the `protocol_version` field
  #[error(transparent)]
  ProtocolVersion(#[from] UnknownProtocolVersion),

  /// Error transforming the `delegate_version` field
  #[error(transparent)]
  DelegateVersion(#[from] UnknownDelegateVersion),
}

impl<I, A> core::fmt::Debug for MemberTransformError<I, A>
where
  I: Transformable,
  A: Transformable,
{
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self)
  }
}

impl<I, A> Transformable for Member<I, A>
where
  I: Transformable,
  A: Transformable,
{
  type Error = MemberTransformError<I, A>;

  fn encode(&self, dst: &mut [u8]) -> Result<usize, Self::Error> {
    let encoded_len = self.encoded_len();
    if dst.len() < encoded_len {
      return Err(Self::Error::BufferTooSmall);
    }

    let mut offset = 0;
    NetworkEndian::write_u32(&mut dst[offset..], encoded_len as u32);
    offset += 4;

    offset += self.node.encode(&mut dst[offset..])?;
    offset += self.tags.encode(&mut dst[offset..])?;
    dst[offset] = self.status as u8;
    offset += 1;

    dst[offset] = self.memberlist_protocol_version as u8;
    offset += 1;

    dst[offset] = self.memberlist_delegate_version as u8;
    offset += 1;

    dst[offset] = self.protocol_version as u8;
    offset += 1;

    dst[offset] = self.delegate_version as u8;
    offset += 1;

    debug_assert_eq!(
      offset, encoded_len,
      "expect write {} bytes, but actually write {} bytes",
      offset, encoded_len
    );

    Ok(offset)
  }

  fn encoded_len(&self) -> usize {
    4 + self.node.encoded_len()
      + self.tags.encoded_len()
      + 1 // status
      + 1 // memberlist_protocol_version
      + 1 // memberlist_delegate_version
      + 1 // protocol_version
      + 1 // delegate_version
  }

  fn decode(src: &[u8]) -> Result<(usize, Self), Self::Error>
  where
    Self: Sized,
  {
    let src_len = src.len();

    if src_len < 4 {
      return Err(Self::Error::NotEnoughBytes);
    }

    let encoded_len = NetworkEndian::read_u32(&src[0..4]) as usize;
    if src_len < encoded_len {
      return Err(Self::Error::NotEnoughBytes);
    }

    let mut offset = 4;
    let (node_len, node) = Node::decode(&src[offset..])?;
    offset += node_len;

    let (tags_len, tags) = Tags::decode(&src[offset..])?;
    offset += tags_len;

    if src_len < offset + 5 {
      return Err(Self::Error::NotEnoughBytes);
    }

    let status = MemberStatus::try_from(src[offset])?;
    offset += 1;

    let memberlist_protocol_version = MemberlistProtocolVersion::try_from(src[offset])?;
    offset += 1;

    let memberlist_delegate_version = MemberlistDelegateVersion::try_from(src[offset])?;
    offset += 1;

    let protocol_version = ProtocolVersion::try_from(src[offset])?;
    offset += 1;

    let delegate_version = DelegateVersion::try_from(src[offset])?;
    offset += 1;

    debug_assert_eq!(
      offset, encoded_len,
      "expect read {} bytes, but actually read {} bytes",
      offset, encoded_len
    );

    Ok((
      encoded_len,
      Self {
        node,
        tags: Arc::new(tags),
        status,
        memberlist_protocol_version,
        memberlist_delegate_version,
        protocol_version,
        delegate_version,
      },
    ))
  }
}

#[cfg(test)]
mod tests {
  use std::net::SocketAddr;

  use rand::{distributions::Alphanumeric, random, thread_rng, Rng};
  use smol_str::SmolStr;

  use super::*;

  impl Member<SmolStr, SocketAddr> {
    fn random(num_tags: usize, size: usize) -> Self {
      let id = thread_rng()
        .sample_iter(Alphanumeric)
        .take(size)
        .collect::<Vec<u8>>();
      let id = String::from_utf8(id).unwrap().into();
      let addr = SocketAddr::from(([127, 0, 0, 1], random::<u16>()));
      let node = Node::new(id, addr);
      let tags = Tags::random(num_tags, size);

      Self {
        node,
        tags: Arc::new(tags),
        status: MemberStatus::Alive,
        memberlist_protocol_version: MemberlistProtocolVersion::V1,
        memberlist_delegate_version: MemberlistDelegateVersion::V1,
        protocol_version: ProtocolVersion::V1,
        delegate_version: DelegateVersion::V1,
      }
    }
  }

  #[test]
  fn member_encode_decode() {
    futures::executor::block_on(async {
      for i in 0..100 {
        let filter = Member::random(i % 10, i);
        let mut buf = vec![0; filter.encoded_len()];
        let encoded_len = filter.encode(&mut buf).unwrap();
        assert_eq!(encoded_len, filter.encoded_len());

        let (decoded_len, decoded) = Member::<SmolStr, SocketAddr>::decode(&buf).unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);

        let (decoded_len, decoded) =
          Member::<SmolStr, SocketAddr>::decode_from_reader(&mut std::io::Cursor::new(&buf))
            .unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);

        let (decoded_len, decoded) = Member::<SmolStr, SocketAddr>::decode_from_async_reader(
          &mut futures::io::Cursor::new(&buf),
        )
        .await
        .unwrap();
        assert_eq!(decoded_len, encoded_len);
        assert_eq!(decoded, filter);
      }
    });
  }
}
