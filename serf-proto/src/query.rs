use smol_str::SmolStr;


use std::time::Duration;

use memberlist_proto::{Node, TinyVec, bytes::Bytes};

use super::LamportTime;

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
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::into::<Vec<Bytes>, TinyVec<Bytes>>))]
  filters: TinyVec<Bytes>,
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

/// Query response message
#[viewit::viewit(getters(style = "ref"), setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
