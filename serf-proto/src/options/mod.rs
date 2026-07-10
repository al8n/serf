//! Configuration knobs for the serf `Endpoint`.
//!
//! All timer defaults are taken verbatim from Go serf `options.go` / the legacy
//! `serf-core/src/options.rs` port.  The struct itself is a plain value type
//! (no atomics, no locks) consistent with the single-threaded Sans-I/O machine.

use core::time::Duration;

/// Configuration for the serf `Endpoint`.
///
/// Construct via [`Options::new`] (which sets all defaults) or build up from
/// `new()` using the `with_*` setter methods.  `Default` delegates to `new()`.
///
/// Timer semantics follow Go serf `options.go`:
/// - Coalescing is disabled by default (all four coalesce periods are `ZERO`);
///   enable by setting a non-zero `coalesce_period` and `quiescent_period`.
/// - `recent_intent_timeout` guards against out-of-order intent delivery; it
///   should be long enough that a re-broadcasted intent does not arrive after
///   the buffer has already expired.
#[derive(Debug, Clone)]
pub struct Options {
  // ── timers ────────────────────────────────────────────────────────────────
  /// How often the reaper runs to remove tombstoned left/failed nodes.
  reap_interval: Duration,
  /// How often we attempt to reconnect to failed nodes.
  reconnect_interval: Duration,
  /// How long we try to reconnect to a failed node before giving up.
  reconnect_timeout: Duration,
  /// How long gracefully-left tombstones are kept for anti-entropy syncing.
  tombstone_timeout: Duration,
  /// How long recent join/leave intents are buffered to handle out-of-order
  /// delivery before the inner memberlist `NodeJoined`/`NodeLeft` event.
  recent_intent_timeout: Duration,
  /// How long to wait for a broadcast (leave, force-remove) to propagate.
  broadcast_timeout: Duration,
  /// Extra delay after calling inner `leave()` before transitioning to `Left`,
  /// giving in-flight probes time to observe the leave intent.
  leave_propagate_delay: Duration,
  /// How often the broadcast queue depth is checked for pruning/warning.
  queue_check_interval: Duration,

  // ── coalescing (disabled when both period fields are ZERO) ────────────────
  /// Duration over which member events are coalesced.  Zero = disabled.
  coalesce_period: Duration,
  /// If no new events arrive within this window, coalescing fires immediately.
  quiescent_period: Duration,
  /// Same as `coalesce_period` but for user (application-defined) events only.
  user_coalesce_period: Duration,
  /// Same as `quiescent_period` but for user events only.
  user_quiescent_period: Duration,

  // ── buffers ───────────────────────────────────────────────────────────────
  /// Number of user-event slots in the dedup ring buffer.
  event_buffer_size: usize,
  /// Number of query slots in the dedup ring buffer.
  query_buffer_size: usize,

  // ── size limits ───────────────────────────────────────────────────────────
  /// Maximum `name + payload` byte size for a user event.
  max_user_event_size: usize,
  /// Maximum inbound payload size for a query message.
  query_size_limit: usize,
  /// Maximum outbound payload size for a query response.
  query_response_size_limit: usize,

  // ── queue pruning ─────────────────────────────────────────────────────────
  /// Hard cap on the broadcast queue; messages beyond this are dropped.
  max_queue_depth: usize,
  /// If `> 0`, replaces `max_queue_depth` with `max(min_queue_depth, 2 * cluster_size)`.
  min_queue_depth: usize,
  /// Emit a warning when the broadcast queue exceeds this depth.
  queue_depth_warning: usize,

  // ── behavior ──────────────────────────────────────────────────────────────
  /// When `true`, the endpoint runs the conflict-resolution query when two
  /// nodes claim the same id.
  enable_id_conflict_resolution: bool,
  /// Multiplier applied to gossip interval to compute the default query timeout:
  /// `timeout = gossip_interval * query_timeout_mult * log(N+1)`.
  query_timeout_mult: usize,
  /// A node is considered to have "flapped" if it fails and rejoins within
  /// this window (used for telemetry).
  flap_timeout: Duration,
  /// When `true`, the endpoint rejoins the last known cluster on startup even
  /// after a graceful leave was recorded in the snapshot.
  rejoin_after_leave: bool,

  // ── feature gates ─────────────────────────────────────────────────────────
  /// When `true`, the Vivaldi coordinate subsystem is disabled at runtime even
  /// if the `coordinates` compile feature is active.
  #[cfg(feature = "coordinates")]
  disable_coordinates: bool,
}

impl Options {
  /// Returns a new `Options` with all defaults as specified in Go serf
  /// `options.go` and the legacy `serf-core/src/options.rs` port.
  pub fn new() -> Self {
    Self {
      reap_interval: Duration::from_secs(15),
      reconnect_interval: Duration::from_secs(30),
      reconnect_timeout: Duration::from_secs(3600 * 24),
      tombstone_timeout: Duration::from_secs(3600 * 24),
      recent_intent_timeout: Duration::from_secs(60 * 5),
      broadcast_timeout: Duration::from_secs(5),
      leave_propagate_delay: Duration::from_secs(1),
      queue_check_interval: Duration::from_secs(30),
      coalesce_period: Duration::ZERO,
      quiescent_period: Duration::ZERO,
      user_coalesce_period: Duration::ZERO,
      user_quiescent_period: Duration::ZERO,
      event_buffer_size: 512,
      query_buffer_size: 512,
      max_user_event_size: 512,
      query_size_limit: 1024,
      query_response_size_limit: 1024,
      max_queue_depth: 4096,
      min_queue_depth: 0,
      queue_depth_warning: 128,
      enable_id_conflict_resolution: true,
      query_timeout_mult: 16,
      flap_timeout: Duration::from_secs(60),
      rejoin_after_leave: false,
      #[cfg(feature = "coordinates")]
      disable_coordinates: false,
    }
  }

  // ── getters ───────────────────────────────────────────────────────────────

  /// How often the reaper runs.
  pub const fn reap_interval(&self) -> Duration {
    self.reap_interval
  }

  /// How often we attempt to reconnect to failed nodes.
  pub const fn reconnect_interval(&self) -> Duration {
    self.reconnect_interval
  }

  /// How long we try to reconnect before giving up on a failed node.
  pub const fn reconnect_timeout(&self) -> Duration {
    self.reconnect_timeout
  }

  /// How long gracefully-left tombstones are retained.
  pub const fn tombstone_timeout(&self) -> Duration {
    self.tombstone_timeout
  }

  /// How long recent intents are buffered to handle out-of-order delivery.
  pub const fn recent_intent_timeout(&self) -> Duration {
    self.recent_intent_timeout
  }

  /// Broadcast propagation timeout.
  pub const fn broadcast_timeout(&self) -> Duration {
    self.broadcast_timeout
  }

  /// Extra delay before transitioning to `Left` after calling inner `leave()`.
  pub const fn leave_propagate_delay(&self) -> Duration {
    self.leave_propagate_delay
  }

  /// How often the broadcast queue depth is checked.
  pub const fn queue_check_interval(&self) -> Duration {
    self.queue_check_interval
  }

  /// Member-event coalesce window.  Zero means coalescing is disabled.
  pub const fn coalesce_period(&self) -> Duration {
    self.coalesce_period
  }

  /// Quiescent window for member-event coalescing.
  pub const fn quiescent_period(&self) -> Duration {
    self.quiescent_period
  }

  /// User-event coalesce window.
  pub const fn user_coalesce_period(&self) -> Duration {
    self.user_coalesce_period
  }

  /// User-event quiescent window.
  pub const fn user_quiescent_period(&self) -> Duration {
    self.user_quiescent_period
  }

  /// Whether member-event coalescing is enabled: both the coalesce and quiescent
  /// periods are non-zero.  Mirrors the legacy `serf-core/src/serf/base.rs`
  /// enable gate (`coalesce_period > 0 && quiescent_period > 0`).
  pub const fn member_coalesce_enabled(&self) -> bool {
    !self.coalesce_period.is_zero() && !self.quiescent_period.is_zero()
  }

  /// Whether user-event coalescing is enabled: both the user coalesce and user
  /// quiescent periods are non-zero.  Mirrors the legacy enable gate.
  pub const fn user_coalesce_enabled(&self) -> bool {
    !self.user_coalesce_period.is_zero() && !self.user_quiescent_period.is_zero()
  }

  /// Number of slots in the user-event dedup ring buffer.
  pub const fn event_buffer_size(&self) -> usize {
    self.event_buffer_size
  }

  /// Number of slots in the query dedup ring buffer.
  pub const fn query_buffer_size(&self) -> usize {
    self.query_buffer_size
  }

  /// Maximum `name + payload` byte size for a user event.
  pub const fn max_user_event_size(&self) -> usize {
    self.max_user_event_size
  }

  /// Maximum inbound payload size for a query.
  pub const fn query_size_limit(&self) -> usize {
    self.query_size_limit
  }

  /// Maximum outbound payload size for a query response.
  pub const fn query_response_size_limit(&self) -> usize {
    self.query_response_size_limit
  }

  /// Hard cap on the broadcast queue depth.
  pub const fn max_queue_depth(&self) -> usize {
    self.max_queue_depth
  }

  /// Dynamic queue depth floor (`0` = disabled; uses `max_queue_depth` instead).
  pub const fn min_queue_depth(&self) -> usize {
    self.min_queue_depth
  }

  /// Broadcast queue depth at which a warning is emitted.
  pub const fn queue_depth_warning(&self) -> usize {
    self.queue_depth_warning
  }

  /// Whether conflict-resolution queries are enabled.
  pub const fn enable_id_conflict_resolution(&self) -> bool {
    self.enable_id_conflict_resolution
  }

  /// Query timeout multiplier.
  pub const fn query_timeout_mult(&self) -> usize {
    self.query_timeout_mult
  }

  /// Flap-detection window for telemetry.
  pub const fn flap_timeout(&self) -> Duration {
    self.flap_timeout
  }

  /// Whether to rejoin the last cluster after a graceful leave snapshot.
  pub const fn rejoin_after_leave(&self) -> bool {
    self.rejoin_after_leave
  }

  /// Whether the Vivaldi coordinate subsystem is disabled at runtime.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub const fn disable_coordinates(&self) -> bool {
    self.disable_coordinates
  }

  // ── setters (builder pattern) ─────────────────────────────────────────────

  /// Sets `reap_interval`.
  ///
  /// Clamped to a minimum of 1 ms: a zero interval would cause `handle_timeout`
  /// to rearm the deadline to `now + 0`, which is already-due every tick and
  /// busy-loops a conforming Sans-I/O driver.  Go serf always runs the reaper
  /// (no disable path), so zero is not a meaningful disable signal.
  pub fn with_reap_interval(mut self, v: Duration) -> Self {
    self.reap_interval = v.max(Duration::from_millis(1));
    self
  }

  /// Sets `reconnect_interval`.
  ///
  /// Clamped to a minimum of 1 ms for the same reason as `reap_interval`: a
  /// zero interval busy-loops the driver.
  pub fn with_reconnect_interval(mut self, v: Duration) -> Self {
    self.reconnect_interval = v.max(Duration::from_millis(1));
    self
  }

  /// Sets `reconnect_timeout`.
  pub fn with_reconnect_timeout(mut self, v: Duration) -> Self {
    self.reconnect_timeout = v;
    self
  }

  /// Sets `tombstone_timeout`.
  pub fn with_tombstone_timeout(mut self, v: Duration) -> Self {
    self.tombstone_timeout = v;
    self
  }

  /// Sets `recent_intent_timeout`.
  pub fn with_recent_intent_timeout(mut self, v: Duration) -> Self {
    self.recent_intent_timeout = v;
    self
  }

  /// Sets `broadcast_timeout`.
  pub fn with_broadcast_timeout(mut self, v: Duration) -> Self {
    self.broadcast_timeout = v;
    self
  }

  /// Sets `leave_propagate_delay`.
  pub fn with_leave_propagate_delay(mut self, v: Duration) -> Self {
    self.leave_propagate_delay = v;
    self
  }

  /// Sets `queue_check_interval`.
  ///
  /// Clamped to a minimum of 1 ms for the same reason as `reap_interval`: a
  /// zero interval busy-loops the driver.
  pub fn with_queue_check_interval(mut self, v: Duration) -> Self {
    self.queue_check_interval = v.max(Duration::from_millis(1));
    self
  }

  /// Sets `coalesce_period`.
  pub fn with_coalesce_period(mut self, v: Duration) -> Self {
    self.coalesce_period = v;
    self
  }

  /// Sets `coalesce_period` in place.
  pub fn set_coalesce_period(&mut self, v: Duration) -> &mut Self {
    self.coalesce_period = v;
    self
  }

  /// Sets `quiescent_period`.
  pub fn with_quiescent_period(mut self, v: Duration) -> Self {
    self.quiescent_period = v;
    self
  }

  /// Sets `quiescent_period` in place.
  pub fn set_quiescent_period(&mut self, v: Duration) -> &mut Self {
    self.quiescent_period = v;
    self
  }

  /// Sets `user_coalesce_period`.
  pub fn with_user_coalesce_period(mut self, v: Duration) -> Self {
    self.user_coalesce_period = v;
    self
  }

  /// Sets `user_coalesce_period` in place.
  pub fn set_user_coalesce_period(&mut self, v: Duration) -> &mut Self {
    self.user_coalesce_period = v;
    self
  }

  /// Sets `user_quiescent_period`.
  pub fn with_user_quiescent_period(mut self, v: Duration) -> Self {
    self.user_quiescent_period = v;
    self
  }

  /// Sets `user_quiescent_period` in place.
  pub fn set_user_quiescent_period(&mut self, v: Duration) -> &mut Self {
    self.user_quiescent_period = v;
    self
  }

  /// Sets `event_buffer_size`.
  pub fn with_event_buffer_size(mut self, v: usize) -> Self {
    self.event_buffer_size = v;
    self
  }

  /// Sets `query_buffer_size`.
  pub fn with_query_buffer_size(mut self, v: usize) -> Self {
    self.query_buffer_size = v;
    self
  }

  /// Sets `max_user_event_size`.
  pub fn with_max_user_event_size(mut self, v: usize) -> Self {
    self.max_user_event_size = v;
    self
  }

  /// Sets `query_size_limit`.
  pub fn with_query_size_limit(mut self, v: usize) -> Self {
    self.query_size_limit = v;
    self
  }

  /// Sets `query_response_size_limit`.
  pub fn with_query_response_size_limit(mut self, v: usize) -> Self {
    self.query_response_size_limit = v;
    self
  }

  /// Sets `max_queue_depth`.
  pub fn with_max_queue_depth(mut self, v: usize) -> Self {
    self.max_queue_depth = v;
    self
  }

  /// Sets `min_queue_depth`.
  pub fn with_min_queue_depth(mut self, v: usize) -> Self {
    self.min_queue_depth = v;
    self
  }

  /// Sets `queue_depth_warning`.
  pub fn with_queue_depth_warning(mut self, v: usize) -> Self {
    self.queue_depth_warning = v;
    self
  }

  /// Sets `enable_id_conflict_resolution`.
  pub fn with_enable_id_conflict_resolution(mut self, v: bool) -> Self {
    self.enable_id_conflict_resolution = v;
    self
  }

  /// Sets `query_timeout_mult`.
  ///
  /// A value of 0 is clamped to 1: the default query timeout is computed as
  /// `gossip_interval * query_timeout_mult * log(N+1)`.  A zero multiplier
  /// produces a zero `Duration`, which causes every query to expire immediately
  /// before the first `poll_timeout` tick — a correctness hazard equivalent to
  /// a zero-capacity buffer.  Mirrors the `.max(1)` guards on buffer sizes.
  pub fn with_query_timeout_mult(mut self, v: usize) -> Self {
    self.query_timeout_mult = v.max(1);
    self
  }

  /// Sets `flap_timeout`.
  pub fn with_flap_timeout(mut self, v: Duration) -> Self {
    self.flap_timeout = v;
    self
  }

  /// Sets `rejoin_after_leave`.
  pub fn with_rejoin_after_leave(mut self, v: bool) -> Self {
    self.rejoin_after_leave = v;
    self
  }

  /// Sets `disable_coordinates`.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub fn with_disable_coordinates(mut self, v: bool) -> Self {
    self.disable_coordinates = v;
    self
  }

  /// Validates the coalescing configuration.
  ///
  /// When a coalescing pair is enabled (both periods non-zero) the quiescent
  /// period must be strictly less than the coalesce period: the quiescent window
  /// is the "went quiet" fast-flush, and the coalesce period is the
  /// maximum-delay cap.  If quiescent `>=` coalesce the quiescent window can
  /// never bind — almost always a misconfiguration.  This mirrors the semantics
  /// documented on the legacy `serf-core/src/options.rs` period fields.
  ///
  /// Returns `Ok(())` when coalescing is disabled or the invariant holds for
  /// every enabled pair.  The Sans-I/O [`Endpoint`](crate::endpoint::Endpoint)
  /// construction is infallible and tolerates any configuration (its flush
  /// deadline is always `min(coalesce, quiescent)`); a driver that wants to
  /// reject a nonsensical configuration up front calls this.
  pub fn validate(&self) -> Result<(), InvalidOptions> {
    if self.member_coalesce_enabled() && self.quiescent_period >= self.coalesce_period {
      return Err(InvalidOptions::MemberCoalesce(CoalesceConfig {
        coalesce_period: self.coalesce_period,
        quiescent_period: self.quiescent_period,
      }));
    }
    if self.user_coalesce_enabled() && self.user_quiescent_period >= self.user_coalesce_period {
      return Err(InvalidOptions::UserCoalesce(CoalesceConfig {
        coalesce_period: self.user_coalesce_period,
        quiescent_period: self.user_quiescent_period,
      }));
    }
    Ok(())
  }
}

/// The coalescing periods that failed [`Options::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceConfig {
  /// The configured coalesce (maximum-delay) period.
  pub coalesce_period: Duration,
  /// The configured quiescent (flush-after-quiet) period.
  pub quiescent_period: Duration,
}

impl core::fmt::Display for CoalesceConfig {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(
      f,
      "quiescent_period ({:?}) must be strictly less than coalesce_period ({:?})",
      self.quiescent_period, self.coalesce_period
    )
  }
}

/// Error returned by [`Options::validate`] for a self-contradictory coalescing
/// configuration (an enabled quiescent period not strictly less than its
/// coalesce period).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidOptions {
  /// The member-event quiescent period is not strictly less than the
  /// member-event coalesce period while member coalescing is enabled.
  #[error("member-event coalescing: {0}")]
  MemberCoalesce(CoalesceConfig),
  /// The user-event quiescent period is not strictly less than the user-event
  /// coalesce period while user coalescing is enabled.
  #[error("user-event coalescing: {0}")]
  UserCoalesce(CoalesceConfig),
}

impl Default for Options {
  /// Delegates to [`Options::new`].  Never derives `Default` when `new()` exists.
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests;
