use super::LamportTime;

/// The message broadcasted after we join to
/// associated the node with a lamport clock
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
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
