use memberlist_proto::TinyVec;
use smol_str::SmolStr;

/// The type of filter
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, derive_more::IsVariant, derive_more::Display)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum FilterType {
  /// Filter by node ids
  #[display("id")]
  Id,
  /// Filter by tag
  #[display("tag")]
  Tag,
  /// Unknown filter type
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl FilterType {
  /// Get the string representation of the filter type
  #[inline]
  pub fn as_str(&self) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(match self {
      Self::Id => "id",
      Self::Tag => "tag",
      Self::Unknown(val) => return std::borrow::Cow::Owned(format!("unknown({})", val)),
    })
  }
}

impl From<u8> for FilterType {
  fn from(value: u8) -> Self {
    match value {
      0 => Self::Id,
      1 => Self::Tag,
      val => Self::Unknown(val),
    }
  }
}

impl From<FilterType> for u8 {
  fn from(val: FilterType) -> Self {
    match val {
      FilterType::Id => 0,
      FilterType::Tag => 1,
      FilterType::Unknown(val) => val,
    }
  }
}

/// Used with a queryFilter to specify the type of
/// filter we are sending
#[derive(Debug, Clone, Eq, PartialEq, derive_more::IsVariant)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Filter<I> {
  /// Filter by node ids
  Id(TinyVec<I>),
  /// Filter by tag
  Tag {
    /// The tag to filter by
    tag: SmolStr,
    /// The expression to filter by
    expr: SmolStr,
  },
}

impl<I> Filter<I> {
  /// Returns the type of filter
  #[inline]
  pub const fn ty(&self) -> FilterType {
    match self {
      Self::Id(_) => FilterType::Id,
      Self::Tag { .. } => FilterType::Tag,
    }
  }
}

