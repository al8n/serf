use super::*;

use std::{format, string::String, vec, vec::Vec};

use memberlist_proto::EndpointInitError;
use serf_embedded::{InitError as EngineInitError, MemberlistInitError};

use crate::{SerfOptions, SerfState};

/// A resolver error with a recognisable rendering, so the boxed `Resolve` arms
/// can be checked for actually carrying their cause into the message.
#[derive(Debug)]
struct ResolverFault;

impl fmt::Display for ResolverFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("the resolver gave up")
  }
}

impl core::error::Error for ResolverFault {}

/// The typed cause an over-ceiling `max_user_event_size` produces.
fn invalid_serf_options() -> InvalidOptions {
  SerfOptions::new()
    .with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1)
    .validate()
    .expect_err("an over-ceiling max_user_event_size must fail validation")
}

fn socket_timeout_payload() -> SocketTimeoutOutOfRange {
  SocketTimeoutOutOfRange {
    socket_timeout: Duration::from_millis(5),
    close_timeout: Duration::from_millis(10),
    stream_timeout: Duration::from_millis(20),
    max: Duration::from_secs(3600),
    tick_hz: 1_000,
  }
}

/// One representative value of every [`InitError`] variant.
fn all_init_errors() -> Vec<InitError> {
  vec![
    InitError::TcpPoolTooSmall(1),
    InitError::ZeroBridgeRing,
    InitError::SocketTimeoutOutOfRange(socket_timeout_payload()),
    InitError::Engine(EngineInitError::Memberlist(MemberlistInitError::ZeroPort)),
    InitError::Resolve(Box::new(ResolverFault)),
    InitError::NoAddresses,
    InitError::Entropy,
    InitError::InvalidSerfOptions(invalid_serf_options()),
  ]
}

// ── InitError ─────────────────────────────────────────────────────────────────

#[test]
fn init_error_renders_each_variant_with_its_cause() {
  assert_eq!(
    format!("{}", InitError::TcpPoolTooSmall(1)),
    "the TCP socket pool needs at least 2 sockets (a listener plus one dial/accept socket); got 1"
  );
  assert_eq!(
    format!("{}", InitError::ZeroBridgeRing),
    "tcp_socket_rx_bytes and tcp_socket_tx_bytes must both be non-zero"
  );
  assert_eq!(
    format!("{}", InitError::NoAddresses),
    "advertise address resolution returned no addresses"
  );
  assert_eq!(
    format!("{}", InitError::Entropy),
    "entropy source failed while seeding the RNGs"
  );
  assert_eq!(
    format!("{}", InitError::Resolve(Box::new(ResolverFault))),
    "advertise address resolution failed: the resolver gave up"
  );

  // The engine arm is transparent: the shared engine's own rendering is what a
  // caller reads, undecorated.
  let engine = EngineInitError::Memberlist(MemberlistInitError::ZeroPort);
  let engine_rendered = format!("{engine}");
  assert_eq!(format!("{}", InitError::Engine(engine)), engine_rendered);
  assert!(!engine_rendered.is_empty());

  let invalid = format!("{}", InitError::InvalidSerfOptions(invalid_serf_options()));
  assert!(
    invalid.starts_with("invalid serf options: "),
    "the serf-options arm names its surface: {invalid}"
  );
  assert!(
    invalid.contains("max_user_event_size"),
    "the typed cause is carried through: {invalid}"
  );
}

/// The out-of-range rendering names every value an operator needs to fix the
/// configuration: the offending timeout, both deadlines it must exceed, the
/// maximum, and the tick rate the rounded comparison used.
#[test]
fn socket_timeout_out_of_range_renders_every_bound() {
  let payload = socket_timeout_payload();
  let rendered = format!("{}", InitError::SocketTimeoutOutOfRange(payload));
  for needle in ["socket_timeout", "close_timeout", "stream_timeout", "1000"] {
    assert!(
      rendered.contains(needle),
      "{needle} missing from {rendered}"
    );
  }
  // The four durations render through their `Debug` form.
  for duration in [
    payload.socket_timeout,
    payload.close_timeout,
    payload.stream_timeout,
    payload.max,
  ] {
    assert!(
      rendered.contains(&format!("{duration:?}")),
      "{duration:?} missing from {rendered}"
    );
  }
  assert_eq!(payload.tick_hz, 1_000);
}

#[test]
fn init_error_predicates_select_exactly_one_variant() {
  type Predicate = (&'static str, fn(&InitError) -> bool);
  let predicates: [Predicate; 7] = [
    ("tcp_pool_too_small", InitError::is_tcp_pool_too_small),
    ("zero_bridge_ring", InitError::is_zero_bridge_ring),
    (
      "socket_timeout_out_of_range",
      InitError::is_socket_timeout_out_of_range,
    ),
    ("engine", InitError::is_engine),
    ("resolve", InitError::is_resolve),
    ("no_addresses", InitError::is_no_addresses),
    ("entropy", InitError::is_entropy),
  ];

  // `all_init_errors` lists the variants in the same order as the predicates,
  // with the untested `InvalidSerfOptions` last: each error must satisfy its own
  // predicate and no other.
  for (i, err) in all_init_errors().iter().enumerate() {
    for (j, (name, predicate)) in predicates.iter().enumerate() {
      assert_eq!(
        predicate(err),
        i == j,
        "is_{name} on {err:?} must be {}",
        i == j
      );
    }
  }
}

#[test]
fn init_error_converts_from_both_engine_halves() {
  // The memberlist half nests through the engine's own error.
  let from_memberlist: InitError = MemberlistInitError::ZeroPort.into();
  assert!(matches!(
    from_memberlist,
    InitError::Engine(EngineInitError::Memberlist(MemberlistInitError::ZeroPort))
  ));

  // And the engine's error passes through whole.
  let from_engine: InitError = EngineInitError::InvalidSerfOptions(invalid_serf_options()).into();
  assert!(matches!(
    from_engine,
    InitError::Engine(EngineInitError::InvalidSerfOptions(_))
  ));
}

/// `source` chains only for the variants that wrap another error, so a caller
/// walking the chain reaches the engine fault or the resolver's own error and
/// stops at every leaf.
#[test]
fn init_error_source_chains_only_for_wrapping_variants() {
  use std::error::Error as _;

  let engine = InitError::Engine(EngineInitError::Memberlist(
    MemberlistInitError::AdvertisePortMismatch,
  ));
  assert!(engine.source().is_some());

  let resolve = InitError::Resolve(Box::new(ResolverFault));
  let source = resolve.source().expect("the boxed resolver error chains");
  assert_eq!(format!("{source}"), "the resolver gave up");

  for leaf in [
    InitError::TcpPoolTooSmall(0),
    InitError::ZeroBridgeRing,
    InitError::SocketTimeoutOutOfRange(socket_timeout_payload()),
    InitError::NoAddresses,
    InitError::Entropy,
    InitError::InvalidSerfOptions(invalid_serf_options()),
  ] {
    assert!(
      leaf.source().is_none(),
      "a leaf variant carries no source: {leaf:?}"
    );
  }
}

/// The shared engine's own error is the chained cause: its serf-options half
/// exposes the typed `InvalidOptions`, while its memberlist half carries its
/// cause through `Display` instead (the memberlist error is not an `Error` on
/// the minimal no_std build).
#[test]
fn the_engine_error_chains_its_serf_options_half_only() {
  use std::error::Error as _;

  let options = EngineInitError::InvalidSerfOptions(invalid_serf_options());
  assert!(options.source().is_some());
  assert!(
    format!("{options}").starts_with("invalid serf options: "),
    "{options}"
  );

  let memberlist = EngineInitError::from(MemberlistInitError::Endpoint(
    EndpointInitError::AwarenessMultiplierZero,
  ));
  assert!(
    memberlist.source().is_none(),
    "the memberlist half carries its cause through Display, not the source chain"
  );
  assert!(!format!("{memberlist}").is_empty());
}

#[test]
fn init_error_debug_is_never_empty() {
  for err in all_init_errors() {
    assert!(!format!("{err:?}").is_empty());
  }
}

// ── JoinError ─────────────────────────────────────────────────────────────────

fn all_join_errors() -> Vec<JoinError> {
  vec![
    JoinError::Resolve(Box::new(ResolverFault)),
    JoinError::Control(SerfError::BadLeaveState(SerfState::Leaving)),
    JoinError::NoAddresses,
    JoinError::Failed(JoinFailed::new(3, 0)),
    JoinError::Shutdown,
  ]
}

#[test]
fn join_error_renders_each_variant_with_its_cause() {
  assert_eq!(
    format!("{}", JoinError::Resolve(Box::new(ResolverFault))),
    "seed address resolution failed: the resolver gave up"
  );
  assert_eq!(
    format!("{}", JoinError::NoAddresses),
    "no wire address resolved for any seed"
  );
  assert_eq!(
    format!("{}", JoinError::Shutdown),
    "the node shut down after losing an id-conflict vote before the join resolved"
  );

  // The control arm names the rejection and carries the engine's own message.
  let control = SerfError::BadLeaveState(SerfState::Leaving);
  let rendered = format!("{}", JoinError::Control(control));
  assert!(rendered.starts_with("join was rejected: "), "{rendered}");
  assert!(
    rendered.contains(&format!("{}", SerfError::BadLeaveState(SerfState::Leaving))),
    "{rendered}"
  );

  // The failed arm is transparent: the reached/requested counts are the message.
  assert_eq!(
    format!("{}", JoinError::Failed(JoinFailed::new(3, 0))),
    "join reached 0 of 3 seed(s)"
  );
}

#[test]
fn join_error_predicates_select_exactly_one_variant() {
  type Predicate = (&'static str, fn(&JoinError) -> bool);
  let predicates: [Predicate; 5] = [
    ("resolve", JoinError::is_resolve),
    ("control", JoinError::is_control),
    ("no_addresses", JoinError::is_no_addresses),
    ("failed", JoinError::is_failed),
    ("shutdown", JoinError::is_shutdown),
  ];

  for (i, err) in all_join_errors().iter().enumerate() {
    for (j, (name, predicate)) in predicates.iter().enumerate() {
      assert_eq!(
        predicate(err),
        i == j,
        "is_{name} on {err:?} must be {}",
        i == j
      );
    }
  }
}

#[test]
fn join_error_converts_from_a_rejected_command() {
  let err: JoinError = SerfError::BadLeaveState(SerfState::Left).into();
  assert!(matches!(
    err,
    JoinError::Control(SerfError::BadLeaveState(SerfState::Left))
  ));
}

/// The failed-join payload chains as the cause, so a caller can recover the
/// requested-seed count from the source chain rather than re-parsing the message.
#[test]
fn join_error_source_chains_the_resolver_control_and_failure_causes() {
  use std::error::Error as _;

  let resolve = JoinError::Resolve(Box::new(ResolverFault));
  assert_eq!(
    format!("{}", resolve.source().expect("the resolver error chains")),
    "the resolver gave up"
  );

  let control = JoinError::Control(SerfError::BadLeaveState(SerfState::Leaving));
  assert!(control.source().is_some());

  let failed = JoinError::Failed(JoinFailed::new(4, 0));
  let source = failed.source().expect("the failure payload chains");
  assert_eq!(format!("{source}"), "join reached 0 of 4 seed(s)");

  for leaf in [JoinError::NoAddresses, JoinError::Shutdown] {
    assert!(leaf.source().is_none(), "{leaf:?} is a leaf");
  }
}

/// The payload a fully-dispatched but unreachable join carries: the seed count
/// it asked for, and the zero it reached.
#[test]
fn join_failed_payload_reports_the_seed_counts() {
  let failed = JoinFailed::new(5, 0);
  assert_eq!(failed.requested(), 5);
  assert_eq!(failed.contacted(), 0);
  assert_eq!(format!("{failed}"), "join reached 0 of 5 seed(s)");
}

// ── OpError ───────────────────────────────────────────────────────────────────

#[test]
fn op_error_renders_each_variant_with_its_cause() {
  assert_eq!(
    format!("{}", OpError::Shutdown),
    "the node has shut down after losing an id-conflict vote; it no longer accepts commands"
  );

  // The engine arm is transparent: the rejection reads exactly as the engine
  // reported it.
  let rejected = SerfError::BadLeaveState(SerfState::Leaving);
  let expected = format!("{rejected}");
  assert_eq!(format!("{}", OpError::Serf(rejected)), expected);
}

#[test]
fn op_error_predicates_and_accessor_agree_on_the_variant() {
  let shutdown = OpError::Shutdown;
  assert!(shutdown.is_shutdown());
  assert!(!shutdown.is_serf());
  assert!(
    shutdown.as_serf().is_none(),
    "a shutdown refusal carries no engine error"
  );

  let rejected = OpError::Serf(SerfError::BadLeaveState(SerfState::Left));
  assert!(rejected.is_serf());
  assert!(!rejected.is_shutdown());
  assert!(matches!(
    rejected.as_serf(),
    Some(SerfError::BadLeaveState(SerfState::Left))
  ));
}

#[test]
fn op_error_converts_from_a_rejected_command() {
  let err: OpError = SerfError::LeaveClockExhausted.into();
  assert!(matches!(err, OpError::Serf(SerfError::LeaveClockExhausted)));
  assert!(err.as_serf().is_some());
}

#[test]
fn op_error_source_chains_only_the_engine_rejection() {
  use std::error::Error as _;

  let rejected = OpError::Serf(SerfError::LeaveClockExhausted);
  let source = rejected.source().expect("the engine rejection chains");
  assert_eq!(
    format!("{source}"),
    format!("{}", SerfError::LeaveClockExhausted)
  );

  assert!(
    OpError::Shutdown.source().is_none(),
    "a shutdown refusal is a leaf"
  );
}

#[test]
fn op_error_debug_is_never_empty() {
  for err in [
    OpError::Shutdown,
    OpError::Serf(SerfError::LeaveClockExhausted),
  ] {
    let shown: String = format!("{err:?}");
    assert!(!shown.is_empty());
  }
}
