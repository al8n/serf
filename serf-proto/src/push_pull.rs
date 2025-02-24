use indexmap::{IndexMap, IndexSet};
use memberlist_proto::TinyVec;


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
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::arbitrary_indexmap))]
  status_ltimes: IndexMap<I, LamportTime>,
  /// List of left nodes
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the list of left nodes")),
    setter(attrs(doc = "Sets the list of left nodes (Builder pattern)"))
  )]
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::arbitrary_indexset))]
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
