use super::*;
#[cfg(any(feature = "tcp", feature = "quic", feature = "serde", feature = "clap"))]
use core::time::Duration;

#[test]
fn runtime_options_defaults_are_accessible() {
  let opts = RuntimeOptions::new();
  assert_eq!(opts.join_deadline(), DEFAULT_JOIN_DEADLINE);
  assert_eq!(opts.leave_timeout(), DEFAULT_LEAVE_TIMEOUT);
  assert_eq!(opts.idle_wake_interval(), DEFAULT_IDLE_WAKE_INTERVAL);
  assert_eq!(opts.iter_drain_cap(), DEFAULT_ITER_DRAIN_CAP);
  assert_eq!(opts.event_queue_cap(), DEFAULT_EVENT_QUEUE_CAP);
  assert_eq!(opts.observation_channel(), DEFAULT_OBSERVATION_CHANNEL);
}

#[test]
fn stream_transport_options_defaults_are_accessible() {
  let opts = StreamTransportOptions::new();
  assert_eq!(opts.dial_timeout(), DEFAULT_DIAL_TIMEOUT);
  assert_eq!(opts.close_timeout(), DEFAULT_CLOSE_TIMEOUT);
  assert_eq!(opts.bridge_inbound_cap(), DEFAULT_BRIDGE_INBOUND_CAP);
  assert_eq!(opts.bridge_recv_buf_len(), DEFAULT_BRIDGE_RECV_BUF_LEN);
}

#[test]
fn channel_display_and_from_str_round_trip() {
  assert_eq!(Channel::Unbounded.to_string(), "unbounded");
  assert_eq!(Channel::Bounded(42).to_string(), "bounded:42");
  assert_eq!("unbounded".parse::<Channel>().unwrap(), Channel::Unbounded);
  assert_eq!(
    "bounded:42".parse::<Channel>().unwrap(),
    Channel::Bounded(42)
  );
  assert!("nonsense".parse::<Channel>().is_err());
}

#[cfg(feature = "tcp")]
#[test]
fn stream_transport_validate_rejects_zero_recv_buf() {
  let opts = StreamTransportOptions::new().with_bridge_recv_buf_len(0);
  assert!(opts.validate().is_err());
}

#[cfg(feature = "tcp")]
#[test]
fn stream_transport_validate_rejects_zero_close_timeout() {
  let opts = StreamTransportOptions::new().with_close_timeout(Duration::ZERO);
  assert!(opts.validate().is_err());
}

#[cfg(feature = "tcp")]
#[test]
fn stream_transport_validate_rejects_zero_bridge_inbound_cap() {
  let opts = StreamTransportOptions::new().with_bridge_inbound_cap(0);
  assert!(opts.validate().is_err());
}

// A `Bounded(0)` observation channel is a flume rendezvous the driver's
// non-blocking `try_send` can never deposit into; `validate` rejects it at
// construction instead.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_rejects_zero_observation_channel() {
  let opts = RuntimeOptions::new().with_observation_channel(Channel::Bounded(0));
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// A zero `event_queue_cap` makes the `flume::bounded` event channel a rendezvous
// the non-blocking forward can never deposit into; `validate` rejects it.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_rejects_zero_event_queue_cap() {
  let opts = RuntimeOptions::new().with_event_queue_cap(0);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_accepts_unbounded_and_nonzero_caps() {
  assert!(RuntimeOptions::new().validate().is_ok());
  assert!(
    RuntimeOptions::new()
      .with_observation_channel(Channel::Unbounded)
      .validate()
      .is_ok()
  );
  assert!(
    RuntimeOptions::new()
      .with_observation_channel(Channel::Bounded(1))
      .with_event_queue_cap(1)
      .validate()
      .is_ok()
  );
}

// A zero `idle_wake_interval` makes a quiescent endpoint re-arm a zero-duration
// timer every pass — a busy-spin that pegs a CPU core; `validate` rejects it.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_rejects_zero_idle_wake_interval() {
  let opts = RuntimeOptions::new().with_idle_wake_interval(Duration::ZERO);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// A zero `join_deadline` makes every parked await-join waiter past-due on insert,
// so the reaper replies `JoinAllFailed` before any push/pull can complete;
// `validate` rejects it at construction.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_rejects_zero_join_deadline() {
  let opts = RuntimeOptions::new().with_join_deadline(Duration::ZERO);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// `iter_drain_cap == 0` (the per-iteration batch cap; the pump still makes
// one-per-pass forward progress) and `leave_timeout == 0` (a loud immediate
// `LeaveTimeout`) degrade-but-function, so they are accepted rather than
// rejected.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn runtime_validate_accepts_degrade_but_function_knobs() {
  assert!(
    RuntimeOptions::new()
      .with_iter_drain_cap(0)
      .validate()
      .is_ok()
  );
  assert!(
    RuntimeOptions::new()
      .with_leave_timeout(Duration::ZERO)
      .validate()
      .is_ok()
  );
}

// A zero `dial_timeout` makes every outbound dial resolve as an immediate
// biased-select timeout, so the stream transport rejects it at `validate`
// (before any socket bind).
#[cfg(feature = "tcp")]
#[test]
fn stream_transport_validate_rejects_zero_dial_timeout() {
  let opts = StreamTransportOptions::new().with_dial_timeout(Duration::ZERO);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// A zero capacity sourced via serde — both the `bounded:0` `FromStr` form and the
// `{"bounded":0}` deserialize form — parses, then fails validation rather than
// reaching a driver-task panic.
#[cfg(all(feature = "serde", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_observation_channel_from_serde() {
  let from_str: Channel = "bounded:0".parse().expect("bounded:0 parses");
  assert_eq!(from_str, Channel::Bounded(0));
  let opts: RuntimeOptions =
    serde_json::from_str(r#"{"observation_channel":{"bounded":0}}"#).expect("deserialize");
  assert_eq!(opts.observation_channel(), Channel::Bounded(0));
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

#[cfg(all(feature = "serde", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_event_queue_cap_from_serde() {
  let opts: RuntimeOptions = serde_json::from_str(r#"{"event_queue_cap":0}"#).expect("deserialize");
  assert_eq!(opts.event_queue_cap(), 0);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// The newly-validated runtime knobs are rejected the same way when sourced via
// serde — the humantime `idle_wake_interval` duration parses, then fails
// validation.
#[cfg(all(feature = "serde", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_idle_wake_interval_from_serde() {
  let opts: RuntimeOptions =
    serde_json::from_str(r#"{"idle_wake_interval":"0s"}"#).expect("deserialize");
  assert_eq!(opts.idle_wake_interval(), Duration::ZERO);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

#[cfg(all(feature = "serde", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_join_deadline_from_serde() {
  let opts: RuntimeOptions =
    serde_json::from_str(r#"{"join_deadline":"0s"}"#).expect("deserialize");
  assert_eq!(opts.join_deadline(), Duration::ZERO);
  assert!(matches!(
    opts.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// A zero capacity sourced via a clap-parsed flag is rejected the same way.
#[cfg(all(feature = "clap", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_observation_channel_from_clap() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  let cli = Cli::try_parse_from(["app", "--runtime-observation-channel", "bounded:0"])
    .expect("clap parses bounded:0");
  assert_eq!(cli.runtime.observation_channel(), Channel::Bounded(0));
  assert!(matches!(
    cli.runtime.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

// The newly-validated runtime knobs are rejected the same way when sourced via a
// clap-parsed flag.
#[cfg(all(feature = "clap", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_idle_wake_interval_from_clap() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  let cli = Cli::try_parse_from(["app", "--runtime-idle-wake-interval", "0s"])
    .expect("clap parses idle-wake-interval 0s");
  assert_eq!(cli.runtime.idle_wake_interval(), Duration::ZERO);
  assert!(matches!(
    cli.runtime.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

#[cfg(all(feature = "clap", any(feature = "tcp", feature = "quic")))]
#[test]
fn runtime_validate_rejects_zero_join_deadline_from_clap() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  let cli = Cli::try_parse_from(["app", "--runtime-join-deadline", "0s"])
    .expect("clap parses join-deadline 0s");
  assert_eq!(cli.runtime.join_deadline(), Duration::ZERO);
  assert!(matches!(
    cli.runtime.validate(),
    Err(crate::SerfError::InvalidOption(_))
  ));
}

#[cfg(feature = "serde")]
#[test]
fn runtime_options_serde_round_trip_and_partial() {
  // An empty config deserializes to the full default.
  assert_eq!(
    serde_json::from_str::<RuntimeOptions>("{}").unwrap(),
    RuntimeOptions::new()
  );
  // A full round-trip preserves every knob, including humantime durations and
  // the tagged observation channel.
  let opts = RuntimeOptions::new()
    .with_leave_timeout(Duration::from_secs(33))
    .with_iter_drain_cap(99)
    .with_observation_channel(Channel::Unbounded);
  let json = serde_json::to_string(&opts).unwrap();
  assert_eq!(serde_json::from_str::<RuntimeOptions>(&json).unwrap(), opts);
  // A partial config overrides one field and defaults the rest.
  let partial: RuntimeOptions = serde_json::from_str(r#"{"iter_drain_cap": 7}"#).unwrap();
  assert_eq!(partial.iter_drain_cap(), 7);
  assert_eq!(partial.leave_timeout(), DEFAULT_LEAVE_TIMEOUT);
  assert_eq!(partial.observation_channel(), DEFAULT_OBSERVATION_CHANNEL);
  // The bounded channel serializes as its snake_case tagged form.
  let bounded = RuntimeOptions::new().with_observation_channel(Channel::Bounded(8));
  let bjson = serde_json::to_string(&bounded).unwrap();
  assert!(bjson.contains("bounded"), "json = {bjson}");
  assert_eq!(
    serde_json::from_str::<RuntimeOptions>(&bjson).unwrap(),
    bounded
  );
}

#[cfg(feature = "serde")]
#[test]
fn runtime_options_serde_rejects_unknown_field() {
  // A misspelled field must be rejected, not silently dropped.
  assert!(serde_json::from_str::<RuntimeOptions>(r#"{"iter_drain_capp": 7}"#).is_err());
}

#[cfg(feature = "clap")]
#[test]
fn runtime_options_clap_parses_flags_and_wires_env() {
  use clap::{CommandFactory, Parser};

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  // Flags parse: a humantime duration, a usize, and the channel via FromStr.
  let cli = Cli::try_parse_from([
    "app",
    "--runtime-leave-timeout",
    "30s",
    "--runtime-iter-drain-cap",
    "12",
    "--runtime-observation-channel",
    "bounded:64",
  ])
  .unwrap();
  assert_eq!(cli.runtime.leave_timeout(), Duration::from_secs(30));
  assert_eq!(cli.runtime.iter_drain_cap(), 12);
  assert_eq!(cli.runtime.observation_channel(), Channel::Bounded(64));
  // Unspecified flags stay at the defaults.
  let dflt = Cli::try_parse_from(["app"]).unwrap();
  assert_eq!(dflt.runtime, RuntimeOptions::new());
  // The env var is wired — assert via command introspection, never `set_var`.
  let cmd = Cli::command();
  let arg = cmd
    .get_arguments()
    .find(|a| a.get_id().as_str() == "runtime-idle-wake-interval")
    .expect("runtime-idle-wake-interval arg is registered");
  assert_eq!(
    arg.get_env().and_then(|e| e.to_str()),
    Some("SERF_RUNTIME_IDLE_WAKE_INTERVAL")
  );
}

// A partial `try_update_from` carrying one unrelated flag must NOT reset the
// other defaulted knobs. clap's `default_value` / `default_value_t` makes an
// unset arg look "present" in update mode, so the value-source gate in the
// manual `update_from_arg_matches` is what keeps a seeded non-default value
// alive across an update.
#[cfg(feature = "clap")]
#[test]
fn runtime_options_partial_update_preserves_unset_fields() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    o: RuntimeOptions,
  }

  // Seed NON-default values on several fields, then run a partial update that
  // supplies ONE unrelated flag.
  let mut cli = Cli {
    o: RuntimeOptions::new()
      .with_leave_timeout(Duration::from_secs(42))
      .with_event_queue_cap(7)
      .with_observation_channel(Channel::Unbounded),
  };

  cli
    .try_update_from(["app", "--runtime-iter-drain-cap", "13"])
    .expect("partial update parses");

  // The supplied flag is applied.
  assert_eq!(cli.o.iter_drain_cap(), 13);
  // Every seeded non-default field SURVIVES the partial update.
  assert_eq!(cli.o.leave_timeout(), Duration::from_secs(42));
  assert_eq!(cli.o.event_queue_cap(), 7);
  assert_eq!(cli.o.observation_channel(), Channel::Unbounded);
}

// An explicit override on update IS applied (the value-source gate lets a
// command-line value through).
#[cfg(feature = "clap")]
#[test]
fn runtime_options_update_applies_explicit_override() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    o: RuntimeOptions,
  }

  let mut cli = Cli {
    o: RuntimeOptions::new().with_leave_timeout(Duration::from_secs(99)),
  };
  cli
    .try_update_from(["app", "--runtime-leave-timeout", "3s"])
    .expect("explicit override parses");
  assert_eq!(cli.o.leave_timeout(), Duration::from_secs(3));
}

/// The crate's `tracing` feature must reach the shared engines in
/// `serf-driver`: their persistence-failure warnings (snapshot append,
/// flush, compaction, keyring write) are the only diagnostics for silently
/// dropped records, and they compile only under `serf-driver/tracing`.
#[cfg(feature = "tracing")]
#[test]
fn tracing_forwards_to_the_shared_engines() {
  assert!(
    serf_driver::TRACING_WIRED,
    "the tracing feature must forward serf-driver/tracing"
  );
}
