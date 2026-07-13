use core::time::Duration;

use super::{InvalidOptions, Options};

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
  assert_eq!(o.leave_propagate_delay(), Duration::from_secs(1));
  assert_eq!(o.queue_check_interval(), Duration::from_secs(30));
  // Coalescing disabled by default
  assert_eq!(o.coalesce_period(), Duration::ZERO);
  assert_eq!(o.quiescent_period(), Duration::ZERO);
  assert_eq!(o.user_coalesce_period(), Duration::ZERO);
  assert_eq!(o.user_quiescent_period(), Duration::ZERO);
  // The user-coalescer volume cap defaults to 1024 (bounded, not disabled).
  assert_eq!(
    o.max_coalesced_user_events(),
    Some(Options::DEFAULT_MAX_COALESCED_USER_EVENTS)
  );
  assert_eq!(Options::DEFAULT_MAX_COALESCED_USER_EVENTS.get(), 1024);
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

// ── coalescing config ────────────────────────────────────────────────────────

#[test]
fn coalescing_disabled_by_default() {
  let o = Options::new();
  assert!(
    !o.member_coalesce_enabled(),
    "member coalescing off by default"
  );
  assert!(!o.user_coalesce_enabled(), "user coalescing off by default");
}

#[test]
fn coalescing_enabled_only_when_both_periods_are_non_zero() {
  // A single non-zero period does NOT enable coalescing (mirrors the legacy gate).
  let only_coalesce = Options::new().with_coalesce_period(Duration::from_secs(10));
  assert!(!only_coalesce.member_coalesce_enabled());
  let only_quiescent = Options::new().with_quiescent_period(Duration::from_secs(2));
  assert!(!only_quiescent.member_coalesce_enabled());

  let both = Options::new()
    .with_coalesce_period(Duration::from_secs(10))
    .with_quiescent_period(Duration::from_secs(2));
  assert!(both.member_coalesce_enabled());

  let user_both = Options::new()
    .with_user_coalesce_period(Duration::from_secs(10))
    .with_user_quiescent_period(Duration::from_secs(2));
  assert!(user_both.user_coalesce_enabled());
}

#[test]
fn max_coalesced_user_events_builder_and_disable() {
  use core::num::NonZeroUsize;

  // The `with_` builder overrides the default.
  let bounded = Options::new().with_max_coalesced_user_events(NonZeroUsize::new(64));
  assert_eq!(bounded.max_coalesced_user_events(), NonZeroUsize::new(64));

  // `None` disables the bound.
  let unbounded = Options::new().with_max_coalesced_user_events(None);
  assert_eq!(unbounded.max_coalesced_user_events(), None);

  // The in-place setter matches the `with_` builder.
  let mut o = Options::new();
  o.set_max_coalesced_user_events(NonZeroUsize::new(64));
  assert_eq!(o.max_coalesced_user_events(), NonZeroUsize::new(64));
}

#[test]
fn set_period_builders_match_with_builders() {
  let mut o = Options::new();
  o.set_coalesce_period(Duration::from_secs(10))
    .set_quiescent_period(Duration::from_secs(2))
    .set_user_coalesce_period(Duration::from_secs(30))
    .set_user_quiescent_period(Duration::from_secs(5));
  assert_eq!(o.coalesce_period(), Duration::from_secs(10));
  assert_eq!(o.quiescent_period(), Duration::from_secs(2));
  assert_eq!(o.user_coalesce_period(), Duration::from_secs(30));
  assert_eq!(o.user_quiescent_period(), Duration::from_secs(5));
}

#[test]
fn validate_accepts_disabled_and_well_ordered_periods() {
  // Disabled coalescing is always valid.
  assert!(Options::new().validate().is_ok());
  // quiescent strictly less than coalesce is valid.
  let ok = Options::new()
    .with_coalesce_period(Duration::from_secs(10))
    .with_quiescent_period(Duration::from_secs(2))
    .with_user_coalesce_period(Duration::from_secs(30))
    .with_user_quiescent_period(Duration::from_secs(5));
  assert!(ok.validate().is_ok());
}

#[test]
fn validate_rejects_quiescent_not_less_than_coalesce() {
  // Member: quiescent == coalesce is rejected.
  let member_bad = Options::new()
    .with_coalesce_period(Duration::from_secs(5))
    .with_quiescent_period(Duration::from_secs(5));
  assert!(matches!(
    member_bad.validate(),
    Err(InvalidOptions::MemberCoalesce(_))
  ));

  // User: quiescent > coalesce is rejected.
  let user_bad = Options::new()
    .with_user_coalesce_period(Duration::from_secs(2))
    .with_user_quiescent_period(Duration::from_secs(10));
  assert!(matches!(
    user_bad.validate(),
    Err(InvalidOptions::UserCoalesce(_))
  ));
}

// ── user-event size ceiling ──────────────────────────────────────────────────

#[test]
fn default_user_event_size_limit_matches_go_serf() {
  // The default ceiling is 9 KiB, matching Go serf's fixed construction-time
  // limit, so a default configuration behaves identically to Go.
  assert_eq!(Options::DEFAULT_USER_EVENT_SIZE_LIMIT, 9 * 1024);
  assert_eq!(Options::DEFAULT_USER_EVENT_SIZE_LIMIT, 9216);
  assert_eq!(
    Options::new().user_event_size_limit(),
    Options::DEFAULT_USER_EVENT_SIZE_LIMIT
  );
}

#[test]
fn validate_rejects_max_user_event_size_over_ceiling() {
  let over = Options::new().with_max_user_event_size(Options::DEFAULT_USER_EVENT_SIZE_LIMIT + 1);
  assert!(matches!(
    over.validate(),
    Err(InvalidOptions::UserEventSize(_))
  ));
}

#[test]
fn validate_accepts_max_user_event_size_at_or_below_ceiling() {
  // Exactly the ceiling is accepted.
  assert!(
    Options::new()
      .with_max_user_event_size(Options::DEFAULT_USER_EVENT_SIZE_LIMIT)
      .validate()
      .is_ok()
  );
  // Below the ceiling is accepted.
  assert!(
    Options::new()
      .with_max_user_event_size(Options::DEFAULT_USER_EVENT_SIZE_LIMIT - 1)
      .validate()
      .is_ok()
  );
  // The default (512) is well below the ceiling.
  assert!(Options::new().validate().is_ok());
}

#[test]
fn raised_ceiling_admits_a_larger_max_user_event_size() {
  // Raising the ceiling is the deliberate double opt-in: both knobs must move
  // for a configuration past Go's fixed limit to validate.
  let raised = Options::new()
    .with_user_event_size_limit(64 * 1024)
    .with_max_user_event_size(16 * 1024);
  assert!(raised.validate().is_ok());

  // Turning only the size knob without the ceiling still rejects.
  let size_only = Options::new().with_max_user_event_size(16 * 1024);
  assert!(matches!(
    size_only.validate(),
    Err(InvalidOptions::UserEventSize(_))
  ));
}

#[test]
fn lowered_ceiling_rejects_an_unadjusted_max_user_event_size() {
  // A ceiling below the configured (here: default 512) per-event size makes the
  // pair incoherent and is rejected.
  let lowered = Options::new().with_user_event_size_limit(256);
  assert!(matches!(
    lowered.validate(),
    Err(InvalidOptions::UserEventSize(_))
  ));
  // Lowering both knobs coherently validates.
  assert!(
    Options::new()
      .with_user_event_size_limit(256)
      .with_max_user_event_size(256)
      .validate()
      .is_ok()
  );
}

#[test]
fn zero_ceiling_is_rejected() {
  // A zero ceiling can never be satisfied meaningfully; disabling user events
  // is expressed through max_user_event_size, not the ceiling.
  let zero = Options::new()
    .with_user_event_size_limit(0)
    .with_max_user_event_size(0);
  assert!(matches!(
    zero.validate(),
    Err(InvalidOptions::UserEventSize(_))
  ));
}

#[test]
fn user_event_size_limit_builder_pair_round_trips() {
  let built = Options::new().with_user_event_size_limit(12 * 1024);
  assert_eq!(built.user_event_size_limit(), 12 * 1024);
  let mut set = Options::new();
  set.set_user_event_size_limit(10 * 1024);
  assert_eq!(set.user_event_size_limit(), 10 * 1024);
}

/// Every builder writes the field it names.  A transposed setter (a copy-paste
/// slip in a long builder chain) is invisible to a defaults test and silently
/// mis-configures the machine, so each setter is pinned to its own getter with a
/// value distinct from every default.
#[test]
fn each_builder_writes_its_own_field() {
  let o = Options::new()
    .with_recent_intent_timeout(Duration::from_secs(11))
    .with_leave_propagate_delay(Duration::from_secs(12))
    .with_query_buffer_size(13)
    .with_queue_depth_warning(14)
    .with_flap_timeout(Duration::from_secs(15));

  assert_eq!(o.recent_intent_timeout(), Duration::from_secs(11));
  assert_eq!(o.leave_propagate_delay(), Duration::from_secs(12));
  assert_eq!(o.query_buffer_size(), 13);
  assert_eq!(o.queue_depth_warning(), 14);
  assert_eq!(o.flap_timeout(), Duration::from_secs(15));

  // The untouched neighbours keep their defaults — no setter bleeds into another.
  let d = Options::new();
  assert_eq!(o.reap_interval(), d.reap_interval());
  assert_eq!(o.event_buffer_size(), d.event_buffer_size());
  assert_eq!(o.max_queue_depth(), d.max_queue_depth());
}

/// A rejected coalescing window reports BOTH periods, so an operator can see the
/// exact pair that violates `quiescent < coalesce` without re-reading the config.
#[test]
fn invalid_coalesce_window_names_both_periods() {
  let err = Options::new()
    .with_coalesce_period(Duration::from_secs(1))
    .with_quiescent_period(Duration::from_secs(5))
    .validate()
    .expect_err("a quiescent period past the coalesce period is invalid");

  let msg = err.to_string();
  assert!(
    matches!(err, InvalidOptions::MemberCoalesce(_)),
    "expected the member-coalescing-window variant, got {err:?}"
  );
  assert!(
    msg.contains("5s") && msg.contains("1s"),
    "the message must name both periods, got: {msg}"
  );
}

/// A `max_user_event_size` over the ceiling reports both numbers; a zero ceiling
/// is reported as its own distinct message, since "must not exceed 0" would read
/// as nonsense.
#[test]
fn invalid_user_event_size_names_the_size_and_the_ceiling() {
  let err = Options::new()
    .with_user_event_size_limit(100)
    .with_max_user_event_size(200)
    .validate()
    .expect_err("a max_user_event_size over the ceiling is invalid");

  let msg = err.to_string();
  assert!(
    matches!(err, InvalidOptions::UserEventSize(_)),
    "expected the user-event-size variant, got {err:?}"
  );
  assert!(
    msg.contains("200") && msg.contains("100"),
    "the message must name the size and the ceiling, got: {msg}"
  );

  let zero = Options::new()
    .with_user_event_size_limit(0)
    .validate()
    .expect_err("a zero ceiling is invalid");
  assert!(
    zero.to_string().contains("nonzero"),
    "a zero ceiling is reported on its own terms, got: {zero}"
  );
}
