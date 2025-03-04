use std::time::Duration;

pub use memberlist_core::proto::{
  DelegateVersion as MemberlistDelegateVersion, Domain, HostAddr, MaybeResolvedAddress, Node,
  NodeId, ParseDomainError, ParseHostAddrError, ParseNodeIdError,
  ProtocolVersion as MemberlistProtocolVersion, bytes,
};
pub use smol_str::*;

#[cfg(feature = "encryption")]
#[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
pub use memberlist_core::proto::encryption::*;

#[cfg(any(
  feature = "crc32",
  feature = "xxhash64",
  feature = "xxhash32",
  feature = "xxhash3",
  feature = "murmur3",
))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(
    feature = "crc32",
    feature = "xxhash64",
    feature = "xxhash32",
    feature = "xxhash3",
    feature = "murmur3"
  )))
)]
pub use memberlist_core::proto::checksum::*;

#[cfg(any(
  feature = "zstd",
  feature = "lz4",
  feature = "snappy",
  feature = "brotli",
))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(
    feature = "zstd",
    feature = "lz4",
    feature = "snappy",
    feature = "brotli"
  )))
)]
pub use memberlist_core::proto::compression::*;

#[cfg(feature = "arbitrary")]
mod arbitrary_impl;

#[cfg(feature = "quickcheck")]
mod quickcheck_impl;

#[cfg(test)]
mod tests;

mod clock;
pub use clock::*;

/// Vivialdi coordinate implementation
pub mod coordinate;

mod conflict;
pub(crate) use conflict::*;

mod filter;
pub(crate) use filter::*;

mod leave;
pub(crate) use leave::*;

mod member;
pub use member::*;

mod message;
pub(crate) use message::*;

mod join;
pub(crate) use join::*;

mod tags;
pub use tags::*;

mod push_pull;
pub(crate) use push_pull::*;

mod user_event;
pub(crate) use user_event::*;

mod query;
pub(crate) use query::*;

mod version;
pub use version::*;

#[cfg(feature = "encryption")]
mod key;
#[cfg(feature = "encryption")]
#[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
pub use key::*;

#[cfg(debug_assertions)]
#[inline]
fn debug_assert_write_eq<T: ?Sized>(actual: usize, expected: usize) {
  debug_assert_eq!(
    actual,
    expected,
    "{}: expect writting {expected} bytes, but actual write {actual} bytes",
    core::any::type_name::<T>(),
  );
}

#[cfg(windows)]
pub(crate) type Epoch = system_epoch::SystemTimeEpoch;

#[cfg(not(windows))]
pub(crate) type Epoch = instant_epoch::InstantEpoch;

#[cfg(windows)]
mod system_epoch {
  use super::*;
  use std::time::SystemTime;

  type SystemTimeEpochInner = SystemTime;

  #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
  pub(crate) struct SystemTimeEpoch(SystemTimeEpochInner);

  impl core::fmt::Debug for SystemTimeEpoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
      self.0.fmt(f)
    }
  }

  impl core::ops::Sub for SystemTimeEpoch {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Duration {
      self.0.duration_since(rhs.0).unwrap()
    }
  }

  impl core::ops::Sub<Duration> for SystemTimeEpoch {
    type Output = Self;

    fn sub(self, rhs: Duration) -> Self {
      Self(self.0 - rhs)
    }
  }

  impl core::ops::SubAssign<Duration> for SystemTimeEpoch {
    fn sub_assign(&mut self, rhs: Duration) {
      self.0 -= rhs;
    }
  }

  impl core::ops::Add<Duration> for SystemTimeEpoch {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
      SystemTimeEpoch(self.0 + rhs)
    }
  }

  impl core::ops::AddAssign<Duration> for SystemTimeEpoch {
    fn add_assign(&mut self, rhs: Duration) {
      self.0 += rhs;
    }
  }

  impl SystemTimeEpoch {
    pub(crate) fn now() -> Self {
      Self(SystemTimeEpochInner::now())
    }

    pub(crate) fn elapsed(&self) -> Duration {
      self.0.elapsed().unwrap()
    }
  }
}

#[cfg(not(windows))]
mod instant_epoch {
  use super::*;
  use std::time::Instant;

  type InstantEpochInner = Instant;

  #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
  pub(crate) struct InstantEpoch(InstantEpochInner);

  impl core::fmt::Debug for InstantEpoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
      self.0.fmt(f)
    }
  }

  impl core::ops::Sub for InstantEpoch {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Duration {
      self.0 - rhs.0
    }
  }

  impl core::ops::Sub<Duration> for InstantEpoch {
    type Output = Self;

    fn sub(self, rhs: Duration) -> Self {
      Self(self.0 - rhs)
    }
  }

  impl core::ops::SubAssign<Duration> for InstantEpoch {
    fn sub_assign(&mut self, rhs: Duration) {
      self.0 -= rhs;
    }
  }

  impl core::ops::Add<Duration> for InstantEpoch {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
      InstantEpoch(self.0 + rhs)
    }
  }

  impl core::ops::AddAssign<Duration> for InstantEpoch {
    fn add_assign(&mut self, rhs: Duration) {
      self.0 += rhs;
    }
  }

  impl InstantEpoch {
    pub(crate) fn now() -> Self {
      Self(InstantEpochInner::now())
    }

    pub(crate) fn elapsed(&self) -> Duration {
      self.0.elapsed()
    }
  }
}
