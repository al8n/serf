use std::path::Path;

mod options;

/// Profile is used to control the timing profiles used in `Serf`.
#[derive(
  Debug, Default, PartialEq, Eq, Clone, Copy, Hash,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[cfg_attr(feature = "cli", clap(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Profile {
  /// Lan is used for local area networks.
  #[default]
  Lan,
  /// Wan is used for wide area networks.
  Wan,
  /// Local is used for local.
  Local,
}

/// Convert to a bunch of paths.
pub trait ToPaths: sealed::Sealed {
  /// Convert to a path.
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>>;
}

impl<P: AsRef<Path>> ToPaths for P {
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>> {
    std::iter::once(self)
  }
}

impl<P: AsRef<Path>> ToPaths for [P] {
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>> {
    self.iter()
  }
}

mod sealed {
  use std::path::Path;

  pub trait Sealed {}

  impl<P: AsRef<Path>> Sealed for P {}
  impl<P: AsRef<Path>> Sealed for [P] {}
}