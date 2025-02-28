use std::sync::Arc;

use memberlist_core::proto::{
  CheapClone, Data, DataRef, DecodeError, EncodeError, OneOrMore, WireType,
  utils::{merge, skip, split},
};

use super::{
  DelegateVersion, Epoch, LamportTime, MemberlistDelegateVersion, MemberlistProtocolVersion,
  MessageType, Node, ProtocolVersion, Tags, TagsRef,
};

use std::collections::HashMap;

/// Used to track members that are no longer active due to
/// leaving, failing, partitioning, etc. It tracks the member along with
/// when that member was marked as leaving.
#[viewit::viewit]
#[derive(Clone, Debug)]
pub(crate) struct MemberState<I, A> {
  member: Member<I, A>,
  /// lamport clock time of last received message
  status_time: LamportTime,
  /// wall clock time of leave
  leave_time: Option<Epoch>,
}

/// Used to buffer intents for out-of-order deliveries.
#[derive(Debug)]
pub(crate) struct NodeIntent {
  pub(crate) ty: MessageType,
  pub(crate) wall_time: Epoch,
  pub(crate) ltime: LamportTime,
}

pub(crate) struct Members<I, A> {
  pub(crate) states: HashMap<I, MemberState<I, A>>,
  pub(crate) recent_intents: HashMap<I, NodeIntent>,
  pub(crate) left_members: OneOrMore<MemberState<I, A>>,
  pub(crate) failed_members: OneOrMore<MemberState<I, A>>,
}

impl<I, A> Default for Members<I, A> {
  fn default() -> Self {
    Self {
      states: Default::default(),
      recent_intents: Default::default(),
      left_members: Default::default(),
      failed_members: Default::default(),
    }
  }
}

const MEMBER_STATUS_NONE: u8 = 0;
const MEMBER_STATUS_ALIVE: u8 = 1;
const MEMBER_STATUS_LEAVING: u8 = 2;
const MEMBER_STATUS_LEFT: u8 = 3;
const MEMBER_STATUS_FAILED: u8 = 4;

/// The member status.
#[derive(
  Debug, Default, Copy, Clone, Eq, PartialEq, Hash, derive_more::IsVariant, derive_more::Display,
)]
#[repr(u8)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum MemberStatus {
  /// None status
  #[display("none")]
  #[default]
  None,
  /// Alive status
  #[display("alive")]
  Alive,
  /// Leaving status
  #[display("leaving")]
  Leaving,
  /// Left status
  #[display("left")]
  Left,
  /// Failed status
  #[display("failed")]
  Failed,
  /// Unknown state (used for forwards and backwards compatibility)
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl From<u8> for MemberStatus {
  fn from(value: u8) -> Self {
    match value {
      MEMBER_STATUS_NONE => Self::None,
      MEMBER_STATUS_ALIVE => Self::Alive,
      MEMBER_STATUS_LEAVING => Self::Leaving,
      MEMBER_STATUS_LEFT => Self::Left,
      MEMBER_STATUS_FAILED => Self::Failed,
      val => Self::Unknown(val),
    }
  }
}

impl From<MemberStatus> for u8 {
  fn from(val: MemberStatus) -> Self {
    match val {
      MemberStatus::None => MEMBER_STATUS_NONE,
      MemberStatus::Alive => MEMBER_STATUS_ALIVE,
      MemberStatus::Leaving => MEMBER_STATUS_LEAVING,
      MemberStatus::Left => MEMBER_STATUS_LEFT,
      MemberStatus::Failed => MEMBER_STATUS_FAILED,
      MemberStatus::Unknown(val) => val,
    }
  }
}

impl MemberStatus {
  /// Get the string representation of the member status
  #[inline]
  pub fn as_str(&self) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(match self {
      Self::None => "none",
      Self::Alive => "alive",
      Self::Leaving => "leaving",
      Self::Left => "left",
      Self::Failed => "failed",
      Self::Unknown(val) => return format!("unknown({})", val).into(),
    })
  }
}

/// A single member of the Serf cluster.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
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
    setter(const, attrs(doc = "Sets the serf protocol version (Builder pattern)"))
  )]
  protocol_version: ProtocolVersion,
  /// The serf delegate version
  #[viewit(
    getter(const, attrs(doc = "Returns the serf delegate version")),
    setter(const, attrs(doc = "Sets the serf delegate version (Builder pattern)"))
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

const NODE_TAG: u8 = 1;
const TAGS_TAG: u8 = 2;
const STATUS_TAG: u8 = 3;
const MEMBERLIST_PROTOCOL_VERSION_TAG: u8 = 4;
const MEMBERLIST_DELEGATE_VERSION_TAG: u8 = 5;
const PROTOCOL_VERSION_TAG: u8 = 6;
const DELEGATE_VERSION_TAG: u8 = 7;

const NODE_BYTE: u8 = merge(WireType::LengthDelimited, NODE_TAG);
const TAGS_BYTE: u8 = merge(WireType::LengthDelimited, TAGS_TAG);
const STATUS_BYTE: u8 = merge(WireType::Byte, STATUS_TAG);
const MEMBERLIST_PROTOCOL_VERSION_BYTE: u8 = merge(WireType::Byte, MEMBERLIST_PROTOCOL_VERSION_TAG);
const MEMBERLIST_DELEGATE_VERSION_BYTE: u8 = merge(WireType::Byte, MEMBERLIST_DELEGATE_VERSION_TAG);
const PROTOCOL_VERSION_BYTE: u8 = merge(WireType::Byte, PROTOCOL_VERSION_TAG);
const DELEGATE_VERSION_BYTE: u8 = merge(WireType::Byte, DELEGATE_VERSION_TAG);

/// A reference type to [`Member`]
#[viewit::viewit(vis_all = "", getters(vis_all = "pub"), setters(skip))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemberRef<'a, I, A> {
  /// The node
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the node")))]
  node: Node<I, A>,
  /// The tags
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the tags")))]
  tags: TagsRef<'a>,
  /// The status
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the status")))]
  status: MemberStatus,
  /// The memberlist protocol version
  #[viewit(getter(const, attrs(doc = "Returns the memberlist protocol version")))]
  memberlist_protocol_version: MemberlistProtocolVersion,
  /// The memberlist delegate version
  #[viewit(getter(const, attrs(doc = "Returns the memberlist delegate version")))]
  memberlist_delegate_version: MemberlistDelegateVersion,
  /// The serf protocol version
  #[viewit(getter(const, attrs(doc = "Returns the serf protocol version")))]
  protocol_version: ProtocolVersion,
  /// The serf delegate version
  #[viewit(getter(const, attrs(doc = "Returns the serf delegate version")))]
  delegate_version: DelegateVersion,
}

impl<'a, I, A> DataRef<'a, Member<I, A>> for MemberRef<'a, I::Ref<'a>, A::Ref<'a>>
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

    let mut node = None;
    let mut tags = None;
    let mut status = None;
    let mut memberlist_protocol_version = None;
    let mut memberlist_delegate_version = None;
    let mut protocol_version = None;
    let mut delegate_version = None;

    while offset < buf_len {
      match buf[offset] {
        NODE_BYTE => {
          if node.is_some() {
            return Err(DecodeError::duplicate_field("Member", "node", NODE_TAG));
          }
          offset += 1;
          let (size, val) =
            <Node<I::Ref<'_>, A::Ref<'_>> as DataRef<'_, Node<I, A>>>::decode_length_delimited(
              &buf[offset..],
            )?;
          node = Some(val);
          offset += size;
        }
        TAGS_BYTE => {
          if tags.is_some() {
            return Err(DecodeError::duplicate_field("Member", "tags", TAGS_TAG));
          }
          offset += 1;
          let (size, val) =
            <TagsRef<'_> as DataRef<'_, Tags>>::decode_length_delimited(&buf[offset..])?;
          tags = Some(val);
          offset += size;
        }
        STATUS_BYTE => {
          if status.is_some() {
            return Err(DecodeError::duplicate_field("Member", "status", STATUS_TAG));
          }
          offset += 1;
          status = Some(buf[offset].into());
          offset += 1;
        }
        MEMBERLIST_PROTOCOL_VERSION_BYTE => {
          if memberlist_protocol_version.is_some() {
            return Err(DecodeError::duplicate_field(
              "Member",
              "memberlist_protocol_version",
              MEMBERLIST_PROTOCOL_VERSION_TAG,
            ));
          }
          offset += 1;
          memberlist_protocol_version = Some(buf[offset].into());
          offset += 1;
        }
        MEMBERLIST_DELEGATE_VERSION_BYTE => {
          if memberlist_delegate_version.is_some() {
            return Err(DecodeError::duplicate_field(
              "Member",
              "memberlist_delegate_version",
              MEMBERLIST_DELEGATE_VERSION_TAG,
            ));
          }
          offset += 1;
          memberlist_delegate_version = Some(buf[offset].into());
          offset += 1;
        }
        PROTOCOL_VERSION_BYTE => {
          if protocol_version.is_some() {
            return Err(DecodeError::duplicate_field(
              "Member",
              "protocol_version",
              PROTOCOL_VERSION_TAG,
            ));
          }
          offset += 1;
          protocol_version = Some(buf[offset].into());
          offset += 1;
        }
        DELEGATE_VERSION_BYTE => {
          if delegate_version.is_some() {
            return Err(DecodeError::duplicate_field(
              "Member",
              "delegate_version",
              DELEGATE_VERSION_TAG,
            ));
          }
          offset += 1;
          delegate_version = Some(buf[offset].into());
          offset += 1;
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
        node: node.ok_or_else(|| DecodeError::missing_field("Member", "node"))?,
        tags: tags.ok_or_else(|| DecodeError::missing_field("Member", "tags"))?,
        status: status.ok_or_else(|| DecodeError::missing_field("Member", "status"))?,
        memberlist_protocol_version: memberlist_protocol_version
          .ok_or_else(|| DecodeError::missing_field("Member", "memberlist_protocol_version"))?,
        memberlist_delegate_version: memberlist_delegate_version
          .ok_or_else(|| DecodeError::missing_field("Member", "memberlist_delegate_version"))?,
        protocol_version: protocol_version
          .ok_or_else(|| DecodeError::missing_field("Member", "protocol_version"))?,
        delegate_version: delegate_version
          .ok_or_else(|| DecodeError::missing_field("Member", "delegate_version"))?,
      },
    ))
  }
}

impl<I, A> Data for Member<I, A>
where
  I: Data,
  A: Data,
{
  type Ref<'a> = MemberRef<'a, I::Ref<'a>, A::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      node: Node::from_ref(val.node)?,
      tags: Tags::from_ref(val.tags)?.into(),
      status: val.status,
      memberlist_protocol_version: val.memberlist_protocol_version,
      memberlist_delegate_version: val.memberlist_delegate_version,
      protocol_version: val.protocol_version,
      delegate_version: val.delegate_version,
    })
  }

  fn encoded_len(&self) -> usize {
    let mut len = 0;
    len += 1 + self.node.encoded_len_with_length_delimited();
    len += 1 + self.tags.encoded_len_with_length_delimited();
    len += 1 + 1; // status
    len += 1 + 1; // memberlist_protocol_version
    len += 1 + 1; // memberlist_delegate_version
    len += 1 + 1; // protocol_version
    len += 1 + 1; // delegate_version
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

    buf[offset] = NODE_BYTE;
    offset += 1;
    offset += self.node.encode_length_delimited(&mut buf[offset..])?;

    bail!(self(offset, buf_len));
    buf[offset] = TAGS_BYTE;
    offset += 1;
    offset += self.tags.encode_length_delimited(&mut buf[offset..])?;

    bail!(self(offset, buf_len));
    buf[offset] = STATUS_BYTE;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = self.status.into();
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = MEMBERLIST_PROTOCOL_VERSION_BYTE;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = self.memberlist_protocol_version.into();
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = MEMBERLIST_DELEGATE_VERSION_BYTE;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = self.memberlist_delegate_version.into();
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = PROTOCOL_VERSION_BYTE;
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = self.protocol_version.into();
    offset += 1;

    bail!(self(offset, buf_len));
    buf[offset] = DELEGATE_VERSION_BYTE;
    offset += 1;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq(offset, self.encoded_len());

    Ok(offset)
  }
}
