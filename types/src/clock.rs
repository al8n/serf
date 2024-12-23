use std::sync::{
  atomic::{AtomicU64, Ordering},
  Arc,
};

use transformable::{utils::*, Transformable};

/// A lamport time is a simple u64 that represents a point in time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[repr(transparent)]
pub struct LamportTime(pub(crate) u64);

impl core::fmt::Display for LamportTime {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl From<u64> for LamportTime {
  fn from(time: u64) -> Self {
    Self(time)
  }
}

impl From<LamportTime> for u64 {
  fn from(time: LamportTime) -> Self {
    time.0
  }
}

impl LamportTime {
  /// Zero lamport time
  pub const ZERO: Self = Self(0);

  /// Creates a new lamport time from the given u64
  #[inline]
  pub const fn new(time: u64) -> Self {
    Self(time)
  }

  /// Returns the lamport time as a big endian byte array
  #[inline]
  pub const fn to_be_bytes(self) -> [u8; 8] {
    self.0.to_be_bytes()
  }

  /// Returns the lamport time as a little endian byte array
  #[inline]
  pub const fn to_le_bytes(self) -> [u8; 8] {
    self.0.to_le_bytes()
  }

  /// Creates a new lamport time from a big endian byte array
  #[inline]
  pub const fn from_be_bytes(bytes: [u8; 8]) -> Self {
    Self(u64::from_be_bytes(bytes))
  }

  /// Creates a new lamport time from a little endian byte array
  #[inline]
  pub const fn from_le_bytes(bytes: [u8; 8]) -> Self {
    Self(u64::from_le_bytes(bytes))
  }
}

impl core::ops::Add<Self> for LamportTime {
  type Output = Self;

  #[inline]
  fn add(self, rhs: Self) -> Self::Output {
    Self(self.0 + rhs.0)
  }
}

impl core::ops::Sub<Self> for LamportTime {
  type Output = Self;

  #[inline]
  fn sub(self, rhs: Self) -> Self::Output {
    Self(self.0 - rhs.0)
  }
}

impl core::ops::Rem<Self> for LamportTime {
  type Output = Self;

  #[inline]
  fn rem(self, rhs: Self) -> Self::Output {
    Self(self.0 % rhs.0)
  }
}

/// Error that can occur when transforming a lamport time
#[derive(thiserror::Error, Debug)]
pub enum LamportTimeTransformError {
  /// Encode varint error
  #[error(transparent)]
  Encode(#[from] InsufficientBuffer),
  /// Decode varint error
  #[error(transparent)]
  Decode(#[from] DecodeVarintError),
}

impl Transformable for LamportTime {
  type Error = LamportTimeTransformError;

  fn encode(&self, dst: &mut [u8]) -> Result<usize, Self::Error> {
    encode_u64_varint(self.0, dst).map_err(Into::into)
  }

  fn encoded_len(&self) -> usize {
    encoded_u64_varint_len(self.0)
  }

  fn decode(src: &[u8]) -> Result<(usize, Self), Self::Error>
  where
    Self: Sized,
  {
    decode_u64_varint(src)
      .map(|(n, time)| (n, Self(time)))
      .map_err(Into::into)
  }
}

/// A thread safe implementation of a lamport clock. It
/// uses efficient atomic operations for all of its functions, falling back
/// to a heavy lock only if there are enough CAS failures.
#[derive(Debug, Clone)]
pub struct LamportClock(Arc<AtomicU64>);

impl Default for LamportClock {
  fn default() -> Self {
    Self::new()
  }
}

impl LamportClock {
  /// Creates a new lamport clock with the given initial value
  #[inline]
  pub fn new() -> Self {
    Self(Arc::new(AtomicU64::new(0)))
  }

  /// Return the current value of the lamport clock
  #[inline]
  pub fn time(&self) -> LamportTime {
    LamportTime(self.0.load(Ordering::SeqCst))
  }

  /// Increment and return the value of the lamport clock
  #[inline]
  pub fn increment(&self) -> LamportTime {
    LamportTime(self.0.fetch_add(1, Ordering::SeqCst) + 1)
  }

  /// Witness is called to update our local clock if necessary after
  /// witnessing a clock value received from another process
  #[inline]
  pub fn witness(&self, time: LamportTime) {
    loop {
      // If the other value is old, we do not need to do anything
      let current = self.0.load(Ordering::SeqCst);
      if time.0 < current {
        return;
      }

      // Ensure that our local clock is at least one ahead.
      match self
        .0
        .compare_exchange_weak(current, time.0 + 1, Ordering::SeqCst, Ordering::SeqCst)
      {
        Ok(_) => return,
        Err(_) => continue,
      }
    }
  }
}

#[cfg(test)]
impl LamportTime {
  pub(crate) fn random() -> Self {
    use rand::Rng;
    Self(rand::thread_rng().gen_range(0..u64::MAX))
  }
}

#[test]
fn test_lamport_clock() {
  let l = LamportClock::new();

  assert_eq!(l.time(), 0.into());
  assert_eq!(l.increment(), 1.into());
  assert_eq!(l.time(), 1.into());

  l.witness(41.into());
  assert_eq!(l.time(), 42.into());

  l.witness(41.into());
  assert_eq!(l.time(), 42.into());

  l.witness(30.into());
  assert_eq!(l.time(), 42.into());
}
