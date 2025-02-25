use indexmap::IndexMap;
use smol_str::SmolStr;

/// Tags of a node
#[derive(
  Debug,
  Default,
  PartialEq,
  Clone,
  derive_more::From,
  derive_more::Into,
  derive_more::Deref,
  derive_more::DerefMut,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Tags(
  #[cfg_attr(feature = "arbitrary", arbitrary(with = crate::arbitrary_impl::arbitrary_indexmap))]
  IndexMap<SmolStr, SmolStr>,
);

impl IntoIterator for Tags {
  type Item = (SmolStr, SmolStr);
  type IntoIter = indexmap::map::IntoIter<SmolStr, SmolStr>;

  fn into_iter(self) -> Self::IntoIter {
    self.0.into_iter()
  }
}

impl FromIterator<(SmolStr, SmolStr)> for Tags {
  fn from_iter<T: IntoIterator<Item = (SmolStr, SmolStr)>>(iter: T) -> Self {
    Self(iter.into_iter().collect())
  }
}

impl<'a> FromIterator<(&'a str, &'a str)> for Tags {
  fn from_iter<T: IntoIterator<Item = (&'a str, &'a str)>>(iter: T) -> Self {
    Self(
      iter
        .into_iter()
        .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
        .collect(),
    )
  }
}

impl Tags {
  /// Create a new Tags
  #[inline]
  pub fn new() -> Self {
    Self(IndexMap::new())
  }

  /// Create a new Tags with a capacity
  pub fn with_capacity(cap: usize) -> Self {
    Self(IndexMap::with_capacity(cap))
  }
}
