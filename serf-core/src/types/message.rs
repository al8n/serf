use memberlist_core::transport::Node;

/// Used to store the end destination of a relayed message
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[repr(transparent)]
pub(crate) struct RelayHeader<I, A> {
  pub(crate) dest: Node<I, A>,
}
