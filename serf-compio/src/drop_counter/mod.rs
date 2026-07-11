//! The shared coalescer-drop counter split into a write-capable half held by the
//! driver-owned endpoint and a read-only half held by the handle's `Shared`.
//!
//! Both wrap the SAME `Rc<Cell<u64>>` (minted once by [`drop_channel`]). compio's
//! driver is single-threaded and `!Send`, so an `Rc<Cell<u64>>` — atomics-free —
//! is the right backing: the endpoint increments it on the pump and a `Serf`
//! handle clone reads it on the same executor thread, with no publish step. The
//! reader exposes no mutator, so a handle has no type-level path to write.

use std::{cell::Cell, rc::Rc};

use serf_proto::DropCounter;

/// The write-capable half, moved into the serf endpoint the driver pumps.
pub(crate) struct CompioDropCounter(Rc<Cell<u64>>);

impl DropCounter for CompioDropCounter {
  #[inline]
  fn incr_saturating(&mut self) {
    self.0.set(self.0.get().saturating_add(1));
  }

  #[inline]
  fn get(&self) -> u64 {
    self.0.get()
  }
}

/// The read-only half, kept on the handle's `Shared`. Exposes only a load, so a
/// `Serf` clone can never write the counter.
pub(crate) struct DropReader(Rc<Cell<u64>>);

impl DropReader {
  /// The endpoint's current cumulative shed count.
  #[inline]
  pub(crate) fn get(&self) -> u64 {
    self.0.get()
  }
}

/// Mint one shared counter as a `(writer, reader)` pair over a single backing
/// cell, so the endpoint's writes and the handle's reads cannot accidentally
/// address two separate allocations.
pub(crate) fn drop_channel() -> (CompioDropCounter, DropReader) {
  let cell = Rc::new(Cell::new(0u64));
  (CompioDropCounter(cell.clone()), DropReader(cell))
}

#[cfg(test)]
mod tests;
