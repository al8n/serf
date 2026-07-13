//! The engine's construction error.
//!
//! [`SerfEngine`](crate::SerfEngine) construction validates two independent
//! configuration surfaces: the shared memberlist runtime configuration
//! (transports, keys, advertise address — rejected as
//! [`memberlist_embedded::InitError`]) and serf's own options (rejected as
//! [`InvalidOptions`](serf_proto::options::InvalidOptions) by
//! [`Options::validate`](serf_proto::options::Options::validate)).  This enum
//! carries both, so every constructor — including a driver built directly on the
//! engine — enforces the full validation rather than only the memberlist half.

#[cfg(test)]
mod tests;

use serf_proto::options::InvalidOptions;

/// Construction failure of a [`SerfEngine`](crate::SerfEngine).
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
  /// The shared memberlist runtime configuration was rejected.
  Memberlist(memberlist_embedded::InitError),
  /// Serf's own options failed
  /// [`Options::validate`](serf_proto::options::Options::validate) — a
  /// self-contradictory coalescing pair or an over-ceiling
  /// `max_user_event_size`.
  InvalidSerfOptions(InvalidOptions),
}

impl core::fmt::Display for InitError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::Memberlist(e) => write!(f, "{e}"),
      Self::InvalidSerfOptions(e) => write!(f, "invalid serf options: {e}"),
    }
  }
}

impl core::error::Error for InitError {
  fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
    match self {
      // The memberlist half implements `Error` only on feature sets beyond the
      // minimal no_std build; its cause is carried through this variant's
      // `Display` instead of the source chain.
      Self::Memberlist(_) => None,
      Self::InvalidSerfOptions(e) => Some(e),
    }
  }
}

impl From<memberlist_embedded::InitError> for InitError {
  fn from(e: memberlist_embedded::InitError) -> Self {
    Self::Memberlist(e)
  }
}
