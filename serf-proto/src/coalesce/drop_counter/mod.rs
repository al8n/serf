//! The [`DropCounter`] seam the coalescer increments and a driver exposes to the
//! application.
//!
//! The pure crate stores the two coalescer shed counts behind this trait rather
//! than as bare `u64` fields, so an async driver can supply a shared,
//! read-observable backing (an atomic or a `Cell`) that the handle reads WITHOUT
//! copying the endpoint's value out each pump iteration. The single-owner default
//! is a plain `u64`, which keeps the endpoint atomics-free and `Send + Sync`.

/// A monotone, saturating drop counter the coalescer increments and a driver
/// exposes to the application.
///
/// Implemented by the pure crate only for `u64` (the single-owner default); each
/// async driver supplies its own shared, read-observable backing so the handle
/// observes increments without a copy.
pub trait DropCounter {
  /// Increment by one, saturating at `u64::MAX`.
  fn incr_saturating(&mut self);
  /// The current cumulative count.
  fn get(&self) -> u64;
}

impl DropCounter for u64 {
  #[inline]
  fn incr_saturating(&mut self) {
    *self = self.saturating_add(1);
  }

  #[inline]
  fn get(&self) -> u64 {
    *self
  }
}

#[cfg(test)]
mod tests;
