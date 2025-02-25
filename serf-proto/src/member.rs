use std::sync::Arc;

use memberlist_proto::CheapClone;

use super::{
  DelegateVersion, MemberlistDelegateVersion, MemberlistProtocolVersion, Node, ProtocolVersion,
  Tags,
};

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
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
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
