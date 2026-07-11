//! The shared coalescer-drop counter split into a write-capable half held by the
//! driver-owned endpoint and a read-only half held by the handle's [`Shared`].
//!
//! Both wrap the SAME `Arc<AtomicU64>` (minted once by [`drop_channel`]), so the
//! endpoint's increment is observed by a `Serf` handle clone on any worker
//! without the driver copying the value out each pump iteration. The reader
//! exposes no mutator, so a handle has no type-level path to write — the endpoint
//! is the structural sole writer.
//!
//! [`Shared`]: crate::shared::Shared

use std::sync::{
  Arc,
  atomic::{AtomicU64, Ordering},
};

use serf_proto::DropCounter;

/// The write-capable half, moved into the serf endpoint the driver pumps. It is
/// the sole writer of its backing atomic.
pub(crate) struct ReactorDropCounter(Arc<AtomicU64>);

impl DropCounter for ReactorDropCounter {
  #[inline]
  fn incr_saturating(&mut self) {
    // A saturating compare-and-swap rather than a wrapping `fetch_add`: the shed
    // path is low-frequency, so the retry cost is negligible, and the counter
    // stops at `u64::MAX` instead of wrapping to zero. Robust to any future writer
    // topology even though the endpoint is the sole writer today.
    let mut cur = self.0.load(Ordering::Relaxed);
    loop {
      let next = cur.saturating_add(1);
      match self
        .0
        .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
      {
        Ok(_) => break,
        Err(actual) => cur = actual,
      }
    }
  }

  #[inline]
  fn get(&self) -> u64 {
    self.0.load(Ordering::Relaxed)
  }
}

/// The read-only half, kept on the handle's [`Shared`]. Exposes only a load, so a
/// `Serf` clone can never write the counter.
///
/// [`Shared`]: crate::shared::Shared
pub(crate) struct DropReader(Arc<AtomicU64>);

impl DropReader {
  /// The endpoint's current cumulative shed count.
  #[inline]
  pub(crate) fn get(&self) -> u64 {
    self.0.load(Ordering::Relaxed)
  }
}

/// Mint one shared counter as a `(writer, reader)` pair over a single backing
/// atomic, so the endpoint's writes and the handle's reads cannot accidentally
/// address two separate allocations.
pub(crate) fn drop_channel() -> (ReactorDropCounter, DropReader) {
  let arc = Arc::new(AtomicU64::new(0));
  (ReactorDropCounter(arc.clone()), DropReader(arc))
}

#[cfg(test)]
mod tests;
