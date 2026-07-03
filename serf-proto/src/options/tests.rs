use core::time::Duration;

use super::Options;

#[test]
fn options_defaults_match_legacy() {
  let o = Options::new();
  assert_eq!(o.reap_interval(), Duration::from_secs(15));
  assert_eq!(o.reconnect_timeout(), Duration::from_secs(3600 * 24));
  assert_eq!(o.tombstone_timeout(), Duration::from_secs(3600 * 24));
  assert_eq!(o.max_user_event_size(), 512);
  assert_eq!(o.query_size_limit(), 1024);
  assert!(o.enable_id_conflict_resolution());
}

#[test]
fn options_default_and_new_are_equal() {
  // Verify Default delegates to new() — the two instances should have the
  // same field values (confirmed via individual assertions rather than PartialEq,
  // which is not derived, to avoid coupling tests to unrelated fields).
  let a = Options::new();
  let b = Options::default();
  assert_eq!(a.reap_interval(), b.reap_interval());
  assert_eq!(a.reconnect_interval(), b.reconnect_interval());
  assert_eq!(a.reconnect_timeout(), b.reconnect_timeout());
  assert_eq!(a.tombstone_timeout(), b.tombstone_timeout());
  assert_eq!(a.recent_intent_timeout(), b.recent_intent_timeout());
  assert_eq!(a.broadcast_timeout(), b.broadcast_timeout());
  assert_eq!(a.leave_propagate_delay(), b.leave_propagate_delay());
  assert_eq!(a.queue_check_interval(), b.queue_check_interval());
  assert_eq!(a.coalesce_period(), b.coalesce_period());
  assert_eq!(a.quiescent_period(), b.quiescent_period());
  assert_eq!(a.event_buffer_size(), b.event_buffer_size());
  assert_eq!(a.query_buffer_size(), b.query_buffer_size());
  assert_eq!(a.max_user_event_size(), b.max_user_event_size());
  assert_eq!(a.query_size_limit(), b.query_size_limit());
  assert_eq!(a.query_response_size_limit(), b.query_response_size_limit());
  assert_eq!(a.max_queue_depth(), b.max_queue_depth());
  assert_eq!(a.min_queue_depth(), b.min_queue_depth());
  assert_eq!(a.queue_depth_warning(), b.queue_depth_warning());
  assert_eq!(
    a.enable_id_conflict_resolution(),
    b.enable_id_conflict_resolution()
  );
  assert_eq!(a.query_timeout_mult(), b.query_timeout_mult());
  assert_eq!(a.flap_timeout(), b.flap_timeout());
  assert_eq!(a.rejoin_after_leave(), b.rejoin_after_leave());
}

#[test]
fn options_all_defaults() {
  let o = Options::new();
  // Timers
  assert_eq!(o.reap_interval(), Duration::from_secs(15));
  assert_eq!(o.reconnect_interval(), Duration::from_secs(30));
  assert_eq!(o.reconnect_timeout(), Duration::from_secs(3600 * 24));
  assert_eq!(o.tombstone_timeout(), Duration::from_secs(3600 * 24));
  assert_eq!(o.recent_intent_timeout(), Duration::from_secs(60 * 5));
  assert_eq!(o.broadcast_timeout(), Duration::from_secs(5));
  assert_eq!(o.leave_propagate_delay(), Duration::from_secs(1));
  assert_eq!(o.queue_check_interval(), Duration::from_secs(30));
  // Coalescing disabled by default
  assert_eq!(o.coalesce_period(), Duration::ZERO);
  assert_eq!(o.quiescent_period(), Duration::ZERO);
  assert_eq!(o.user_coalesce_period(), Duration::ZERO);
  assert_eq!(o.user_quiescent_period(), Duration::ZERO);
  // Buffers
  assert_eq!(o.event_buffer_size(), 512);
  assert_eq!(o.query_buffer_size(), 512);
  // Size limits
  assert_eq!(o.max_user_event_size(), 512);
  assert_eq!(o.query_size_limit(), 1024);
  assert_eq!(o.query_response_size_limit(), 1024);
  // Queue pruning
  assert_eq!(o.max_queue_depth(), 4096);
  assert_eq!(o.min_queue_depth(), 0);
  assert_eq!(o.queue_depth_warning(), 128);
  // Behavior
  assert!(o.enable_id_conflict_resolution());
  assert_eq!(o.query_timeout_mult(), 16);
  assert_eq!(o.flap_timeout(), Duration::from_secs(60));
  assert!(!o.rejoin_after_leave());
}

#[test]
fn options_builder_overrides_defaults() {
  let o = Options::new()
    .with_reap_interval(Duration::from_secs(30))
    .with_max_queue_depth(8192)
    .with_enable_id_conflict_resolution(false)
    .with_rejoin_after_leave(true);

  assert_eq!(o.reap_interval(), Duration::from_secs(30));
  assert_eq!(o.max_queue_depth(), 8192);
  assert!(!o.enable_id_conflict_resolution());
  assert!(o.rejoin_after_leave());
  // Untouched defaults remain
  assert_eq!(o.reconnect_timeout(), Duration::from_secs(3600 * 24));
}

// ── FIX 2 sweep: zero-value hazard clamps ────────────────────────────────────

/// `Options::with_query_timeout_mult(0)` must be clamped to 1.
///
/// A zero multiplier causes `gossip_interval * 0 * log(N+1) = Duration::ZERO`,
/// which makes every query expire immediately before the first `poll_timeout`
/// tick — a correctness hazard.  Mirrors the `.max(1)` guards on buffer sizes
/// and `CoordinateOptions::with_dimensionality`.
#[test]
fn query_timeout_mult_zero_is_clamped_to_one() {
  let o = Options::new().with_query_timeout_mult(0);
  assert_eq!(
    o.query_timeout_mult(),
    1,
    "with_query_timeout_mult(0) must be clamped to 1"
  );
}
