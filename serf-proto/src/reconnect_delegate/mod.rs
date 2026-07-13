//! The [`ReconnectDelegate`] hook: a per-member override for the reaper's
//! reconnect / tombstone timeout.
//!
//! Unlike serf's other delegates (async, driver-side observers and vetoes), this
//! hook fires inside the machine's own autonomous reap tick, where the driver is
//! not in the path — and it is pure and synchronous. So it lives in the machine
//! as an injected pure dependency rather than on a driver, mirroring Go serf's
//! `ReconnectDelegate`, which the reaper consults per member inside its shared
//! reap pass (`legacy/serf-core/src/serf/base.rs`, the `reap!` macro).

use core::time::Duration;

use crate::members::Member;

#[cfg(test)]
mod tests;

/// Overrides the reap timeout for individual members.
///
/// Consulted by the reaper for every member it considers: the failed list
/// (base timeout = `reconnect_timeout`) and the left list (base timeout =
/// `tombstone_timeout`), mirroring Go serf's per-member override applied
/// inside its shared reap pass. Return `timeout` unchanged to keep the
/// configured value, or a per-member override (e.g. a longer window for known
/// slow-to-return nodes).
///
/// The hook is synchronous and runs inside the machine's reap tick; it must
/// not block. `Send + Sync` are supertraits so a boxed delegate leaves the
/// endpoint's auto-traits intact for the multi-threaded drivers.
pub trait ReconnectDelegate<I, A>: Send + Sync {
  /// The reap timeout to use for `member`, given the configured base `timeout`.
  fn reconnect_timeout(&self, member: &Member<I, A>, timeout: Duration) -> Duration;
}
