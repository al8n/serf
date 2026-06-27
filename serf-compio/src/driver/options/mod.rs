//! Per-`Serf` runtime tuning knobs.
//!
//! [`RuntimeOptions`] carries the generic-free runtime knobs; per-backend knobs
//! live on each backend's `*TransportOptions` struct.
//!
//! Each `DEFAULT_*` constant is `pub` so callers can derive new values from the
//! default (e.g. `DEFAULT_LEAVE_TIMEOUT * 3`).

use core::time::Duration;

#[cfg(any(feature = "tcp", feature = "quic"))]
use crate::error::{InvalidOption, SerfError};

#[cfg(feature = "clap")]
use humantime::parse_duration;

/// Default per-call deadline for [`Serf::leave`](crate::Serf::leave).
pub const DEFAULT_LEAVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default fallback driver-loop sleep when the coordinator's `poll_timeout`
/// returns `None`.
pub const DEFAULT_IDLE_WAKE_INTERVAL: Duration = Duration::from_secs(60);

/// Default per-iteration drain cap for inbound surfaces.
pub const DEFAULT_ITER_DRAIN_CAP: usize = 256;

/// Default per-iteration cmd-channel fairness budget.
pub const DEFAULT_CMD_FAIRNESS_BUDGET: usize = 4;

/// Default events-channel capacity.
pub const DEFAULT_EVENT_QUEUE_CAP: usize = 1024;

/// Default outbound dial budget for the stream-transport driver.
pub const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Default bridge-inbound channel capacity for the stream-transport driver.
pub const DEFAULT_BRIDGE_INBOUND_CAP: usize = 1024;

/// Default per-bridge TCP read buffer size for the stream-transport driver.
pub const DEFAULT_BRIDGE_RECV_BUF_LEN: usize = 16 * 1024;

/// Default bound on a per-bridge graceful-drain write for the stream-transport
/// driver: 10 seconds.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How the per-driver delegate **observation channel** is bounded.
///
/// The driver hands every machine `Event` to a separate observation task that
/// runs the user [`Delegate`](crate::Delegate) hooks, decoupled from the
/// protocol loop so a slow hook never stalls the FSM.
///
/// - [`Unbounded`](Self::Unbounded): the hand-off never drops, so the delegate
///   observes every event in order — but a delegate that persistently runs
///   slower than inbound traffic grows the channel without limit.
/// - [`Bounded`](Self::Bounded): caps the channel at `n` queued events. When
///   full, the driver drops the newest event rather than blocking and counts
///   the drop in `observation_dropped`.
///
/// `Bounded(0)` is rejected at construction: a zero-capacity channel is a
/// rendezvous the driver's non-blocking send can never deposit into.
///
/// As a config value (serde / CLI) it is open-vocabulary: `Unbounded` is the
/// bare string `"unbounded"`, `Bounded(n)` is `{"bounded": n}` under serde and
/// `bounded:n` on the CLI (via [`FromStr`](core::str::FromStr) /
/// [`Display`](core::fmt::Display)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Channel {
  /// Never drop; the channel grows without bound.
  Unbounded,
  /// Cap the channel at this many queued events; drop-newest and count in
  /// `observation_dropped` when full.
  Bounded(usize),
}

impl core::fmt::Display for Channel {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::Unbounded => f.write_str("unbounded"),
      Self::Bounded(n) => write!(f, "bounded:{n}"),
    }
  }
}

/// Parse a [`Channel`] from its string form — `"unbounded"` or `"bounded:<n>"`.
/// The inverse of [`Channel`]'s `Display`.
impl core::str::FromStr for Channel {
  type Err = ParseChannelError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s.eq_ignore_ascii_case("unbounded") {
      return Ok(Self::Unbounded);
    }
    let cap = s
      .strip_prefix("bounded:")
      .or_else(|| s.strip_prefix("bounded="))
      .ok_or(ParseChannelError(()))?;
    let n = cap.parse::<usize>().map_err(|_| ParseChannelError(()))?;
    Ok(Self::Bounded(n))
  }
}

/// The error from [`Channel::from_str`]: the input was neither `"unbounded"`
/// nor a `"bounded:<n>"` with a valid capacity.
///
/// Opaque — the private unit field seals construction to this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid observation channel (expected `unbounded` or `bounded:<n>`)")]
pub struct ParseChannelError(());

/// Default delegate observation channel: [`Channel::Bounded`] at 1024 events.
pub const DEFAULT_OBSERVATION_CHANNEL: Channel = Channel::Bounded(1024);

/// Per-`Serf` runtime tuning knobs. Generic-free; per-backend knobs live on
/// each backend's `*TransportOptions` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
pub struct RuntimeOptions {
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  leave_timeout: Duration,
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  idle_wake_interval: Duration,
  iter_drain_cap: usize,
  cmd_fairness_budget: usize,
  event_queue_cap: usize,
  observation_channel: Channel,
}

// `clap::Args` is NOT derived on `RuntimeOptions`. The derived
// `update_from_arg_matches` treats every `default_value` / `default_value_t`
// arg as present even when the operator did not pass it, so a partial
// `try_update_from` carrying one unrelated flag would reset every other
// defaulted knob to its default. A private mirror carries the `#[arg(...)]`
// attributes and derives `Args`; `RuntimeOptions` delegates its `Args` /
// `FromArgMatches` to the mirror and, on update, applies ONLY the args whose
// value came from the command line or an env var.
#[cfg(feature = "clap")]
const _: () = {
  use clap::{ArgMatches, Args, Command, Error, FromArgMatches, parser::ValueSource};

  #[derive(Args)]
  struct RuntimeOptionsCli {
    #[arg(
      id = "runtime-leave-timeout",
      long = "runtime-leave-timeout",
      env = "SERF_RUNTIME_LEAVE_TIMEOUT",
      value_parser = parse_duration,
      // The humantime spelling of DEFAULT_LEAVE_TIMEOUT.
      default_value = "5s",
    )]
    leave_timeout: Duration,
    #[arg(
      id = "runtime-idle-wake-interval",
      long = "runtime-idle-wake-interval",
      env = "SERF_RUNTIME_IDLE_WAKE_INTERVAL",
      value_parser = parse_duration,
      // The humantime spelling of DEFAULT_IDLE_WAKE_INTERVAL.
      default_value = "60s",
    )]
    idle_wake_interval: Duration,
    #[arg(
      id = "runtime-iter-drain-cap",
      long = "runtime-iter-drain-cap",
      env = "SERF_RUNTIME_ITER_DRAIN_CAP",
      default_value_t = DEFAULT_ITER_DRAIN_CAP,
    )]
    iter_drain_cap: usize,
    #[arg(
      id = "runtime-cmd-fairness-budget",
      long = "runtime-cmd-fairness-budget",
      env = "SERF_RUNTIME_CMD_FAIRNESS_BUDGET",
      default_value_t = DEFAULT_CMD_FAIRNESS_BUDGET,
    )]
    cmd_fairness_budget: usize,
    #[arg(
      id = "runtime-event-queue-cap",
      long = "runtime-event-queue-cap",
      env = "SERF_RUNTIME_EVENT_QUEUE_CAP",
      default_value_t = DEFAULT_EVENT_QUEUE_CAP,
    )]
    event_queue_cap: usize,
    #[arg(
      id = "runtime-observation-channel",
      long = "runtime-observation-channel",
      env = "SERF_RUNTIME_OBSERVATION_CHANNEL",
      default_value_t = DEFAULT_OBSERVATION_CHANNEL,
    )]
    observation_channel: Channel,
  }

  impl From<RuntimeOptionsCli> for RuntimeOptions {
    fn from(c: RuntimeOptionsCli) -> Self {
      Self {
        leave_timeout: c.leave_timeout,
        idle_wake_interval: c.idle_wake_interval,
        iter_drain_cap: c.iter_drain_cap,
        cmd_fairness_budget: c.cmd_fairness_budget,
        event_queue_cap: c.event_queue_cap,
        observation_channel: c.observation_channel,
      }
    }
  }

  impl Args for RuntimeOptions {
    fn augment_args(cmd: Command) -> Command {
      RuntimeOptionsCli::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: Command) -> Command {
      RuntimeOptionsCli::augment_args_for_update(cmd)
    }
  }

  impl FromArgMatches for RuntimeOptions {
    fn from_arg_matches(m: &ArgMatches) -> Result<Self, Error> {
      RuntimeOptionsCli::from_arg_matches(m).map(Into::into)
    }

    fn update_from_arg_matches(&mut self, m: &ArgMatches) -> Result<(), Error> {
      // Apply ONLY operator-supplied overrides — args whose value came from the
      // command line or an env var, not a clap default. A bare derived update
      // treats every `default_value` arg as present and would reset unset fields.
      macro_rules! take {
        ($id:literal, $field:ident, $ty:ty) => {
          if matches!(
            m.value_source($id),
            Some(ValueSource::CommandLine) | Some(ValueSource::EnvVariable)
          ) {
            if let Some(v) = m.get_one::<$ty>($id) {
              self.$field = *v;
            }
          }
        };
      }
      take!("runtime-leave-timeout", leave_timeout, Duration);
      take!("runtime-idle-wake-interval", idle_wake_interval, Duration);
      take!("runtime-iter-drain-cap", iter_drain_cap, usize);
      take!("runtime-cmd-fairness-budget", cmd_fairness_budget, usize);
      take!("runtime-event-queue-cap", event_queue_cap, usize);
      take!("runtime-observation-channel", observation_channel, Channel);
      Ok(())
    }
  }
};

impl RuntimeOptions {
  /// Construct from the canonical base defaults.
  #[inline]
  pub const fn new() -> Self {
    Self {
      leave_timeout: DEFAULT_LEAVE_TIMEOUT,
      idle_wake_interval: DEFAULT_IDLE_WAKE_INTERVAL,
      iter_drain_cap: DEFAULT_ITER_DRAIN_CAP,
      cmd_fairness_budget: DEFAULT_CMD_FAIRNESS_BUDGET,
      event_queue_cap: DEFAULT_EVENT_QUEUE_CAP,
      observation_channel: DEFAULT_OBSERVATION_CHANNEL,
    }
  }

  /// Builder: per-call deadline for [`Serf::leave`](crate::Serf::leave).
  #[must_use]
  #[inline]
  pub const fn with_leave_timeout(mut self, d: Duration) -> Self {
    self.leave_timeout = d;
    self
  }

  /// Builder: fallback driver-loop sleep when the coordinator has no pending
  /// deadline.
  #[must_use]
  #[inline]
  pub const fn with_idle_wake_interval(mut self, d: Duration) -> Self {
    self.idle_wake_interval = d;
    self
  }

  /// Builder: per-iteration drain cap.
  #[must_use]
  #[inline]
  pub const fn with_iter_drain_cap(mut self, n: usize) -> Self {
    self.iter_drain_cap = n;
    self
  }

  /// Builder: per-iteration cmd-channel fairness budget.
  #[must_use]
  #[inline]
  pub const fn with_cmd_fairness_budget(mut self, n: usize) -> Self {
    self.cmd_fairness_budget = n;
    self
  }

  /// Builder: events-channel capacity.
  #[must_use]
  #[inline]
  pub const fn with_event_queue_cap(mut self, n: usize) -> Self {
    self.event_queue_cap = n;
    self
  }

  /// Builder: how the delegate observation channel is bounded.
  #[must_use]
  #[inline]
  pub const fn with_observation_channel(mut self, c: Channel) -> Self {
    self.observation_channel = c;
    self
  }

  /// Per-call deadline for [`Serf::leave`](crate::Serf::leave).
  #[inline]
  pub const fn leave_timeout(&self) -> Duration {
    self.leave_timeout
  }

  /// Fallback driver-loop sleep.
  #[inline]
  pub const fn idle_wake_interval(&self) -> Duration {
    self.idle_wake_interval
  }

  /// Per-iteration drain cap.
  #[inline]
  pub const fn iter_drain_cap(&self) -> usize {
    self.iter_drain_cap
  }

  /// Per-iteration cmd-channel fairness budget.
  #[inline]
  pub const fn cmd_fairness_budget(&self) -> usize {
    self.cmd_fairness_budget
  }

  /// Events-channel capacity.
  #[inline]
  pub const fn event_queue_cap(&self) -> usize {
    self.event_queue_cap
  }

  /// How the delegate observation channel is bounded.
  #[inline]
  pub const fn observation_channel(&self) -> Channel {
    self.observation_channel
  }

  /// Validate the generic-free runtime knobs whose value would DETERMINISTICALLY
  /// break (not merely degrade) the driver loop, so the misconfiguration
  /// surfaces from [`Serf::new`](crate::Serf::new) — before any socket is bound
  /// or the detached driver task is spawned — rather than as a panic, a
  /// busy-spin, or a silently dead surface inside that task. Each is rejected
  /// (not clamped) so the operator learns and fixes the value; every backend
  /// (TCP/TLS/QUIC) routes through the one `Serf::new` path, so this single call
  /// covers all three.
  ///
  /// - `idle_wake_interval == 0`: the driver loop's fallback sleep when the
  ///   coordinator's `poll_timeout` has no nearer deadline
  ///   (`poll_timeout().unwrap_or(now + idle_wake_interval)`). Zero makes a
  ///   quiescent endpoint re-arm a zero-duration timer every pass — a busy-spin
  ///   that pegs a CPU core.
  /// - `cmd_fairness_budget == 0`: the iter-top command fairness drain
  ///   (`while cmd_drained < cmd_fairness_budget`). The main `select_biased!`
  ///   biases the network arms ahead of the command arm, so under a continuous
  ///   inbound flood the command arm is never reached; this drain is the ONLY
  ///   mechanism that keeps commands progressing under that load, so a zero
  ///   budget disables it and `shutdown` / `leave` / joins can hang indefinitely.
  /// - `event_queue_cap == 0`: the observation task forwards every event to the
  ///   bounded event-stream channel with a non-blocking `try_send`, and a
  ///   zero-capacity channel is a rendezvous a non-blocking send can never
  ///   deposit into, so every event would be dropped and the event-observation
  ///   surface would be non-functional.
  /// - `observation_channel == Channel::Bounded(0)`: each driver builds the
  ///   delegate observation channel with `lochan::mpsc::bounded`, which panics
  ///   on a zero capacity, so a `Bounded(0)` channel would panic the detached
  ///   driver task at startup.
  ///
  /// `iter_drain_cap == 0` and `leave_timeout == 0` are NOT rejected: the former
  /// only caps the per-iteration batch drain (the `select` arms and the uncapped
  /// timeout drain still make one-per-pass forward progress), and the latter is a
  /// loud immediate [`SerfError::LeaveTimeout`] rather than a silent break.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) fn validate(&self) -> Result<(), SerfError> {
    if self.idle_wake_interval.is_zero() {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "idle_wake_interval",
        "the driver-loop fallback sleep must be nonzero: the loop sleeps until \
           `poll_timeout().unwrap_or(now + idle_wake_interval)`, so a zero idle_wake_interval \
           makes a quiescent endpoint re-arm a zero-duration timer every pass — a busy-spin \
           that pegs a CPU core"
          .to_string(),
      )));
    }
    if self.cmd_fairness_budget == 0 {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "cmd_fairness_budget",
        "the iter-top command fairness drain must pull at least one command per pass: the main \
           select biases the network arms ahead of commands, so under a continuous inbound \
           flood the command arm is starved indefinitely and shutdown / leave / joins would \
           never be serviced; a zero budget disables the only drain that guarantees command \
           progress"
          .to_string(),
      )));
    }
    if self.event_queue_cap == 0 {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "event_queue_cap",
        "the event-stream queue capacity must be nonzero: the observation task forwards every \
           event to this bounded channel with a non-blocking `try_send`, and a zero-capacity \
           channel is a rendezvous a non-blocking send can never deposit into, so every event \
           would be dropped and the event-observation surface would be non-functional"
          .to_string(),
      )));
    }
    if matches!(self.observation_channel, Channel::Bounded(0)) {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "observation_channel",
        "the delegate observation channel must have a nonzero bounded capacity: each driver \
           builds it with `lochan::mpsc::bounded`, which panics on a zero capacity, so a \
           `Bounded(0)` channel would panic the detached driver task at startup instead of \
           running"
          .to_string(),
      )));
    }
    Ok(())
  }
}

impl Default for RuntimeOptions {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

/// Stream-transport-specific tuning knobs.
///
/// Apply to the stream-backed (TCP) [`Serf`](crate::Serf) driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamTransportOptions {
  dial_timeout: Duration,
  close_timeout: Duration,
  bridge_inbound_cap: usize,
  bridge_recv_buf_len: usize,
}

impl StreamTransportOptions {
  /// Construct with the canonical stream-transport defaults.
  #[inline]
  pub const fn new() -> Self {
    Self {
      dial_timeout: DEFAULT_DIAL_TIMEOUT,
      close_timeout: DEFAULT_CLOSE_TIMEOUT,
      bridge_inbound_cap: DEFAULT_BRIDGE_INBOUND_CAP,
      bridge_recv_buf_len: DEFAULT_BRIDGE_RECV_BUF_LEN,
    }
  }

  /// Builder: outbound `TcpStream::connect` budget.
  #[must_use]
  #[inline]
  pub const fn with_dial_timeout(mut self, d: Duration) -> Self {
    self.dial_timeout = d;
    self
  }

  /// Builder: bound on a per-bridge graceful-drain write.
  #[must_use]
  #[inline]
  pub const fn with_close_timeout(mut self, d: Duration) -> Self {
    self.close_timeout = d;
    self
  }

  /// Builder: bridge-inbound channel capacity.
  #[must_use]
  #[inline]
  pub const fn with_bridge_inbound_cap(mut self, n: usize) -> Self {
    self.bridge_inbound_cap = n;
    self
  }

  /// Builder: per-bridge TCP read buffer size.
  #[must_use]
  #[inline]
  pub const fn with_bridge_recv_buf_len(mut self, n: usize) -> Self {
    self.bridge_recv_buf_len = n;
    self
  }

  /// Outbound `TcpStream::connect` budget.
  #[inline]
  pub const fn dial_timeout(&self) -> Duration {
    self.dial_timeout
  }

  /// Bound on a per-bridge graceful-drain write.
  #[inline]
  pub const fn close_timeout(&self) -> Duration {
    self.close_timeout
  }

  /// Bridge-inbound channel capacity.
  #[inline]
  pub const fn bridge_inbound_cap(&self) -> usize {
    self.bridge_inbound_cap
  }

  /// Per-bridge TCP read buffer size.
  #[inline]
  pub const fn bridge_recv_buf_len(&self) -> usize {
    self.bridge_recv_buf_len
  }

  /// Validate the stream-transport knobs whose value would DETERMINISTICALLY
  /// break (not merely degrade) a stream backend, rejected fail-fast at the
  /// transport's `new` (before any socket is bound) rather than constructing `Ok`
  /// over a silently-broken backend. Called at the top of both stream backends'
  /// `Transport::new` (TCP and TLS); QUIC has no bridges and so no counterpart.
  ///
  /// - `dial_timeout == 0`: every outbound `StreamAction::Connect` races
  ///   `TcpStream::connect` against `compio::time::sleep(dial_timeout)` in a
  ///   biased select. On a completion-based backend the connect is pending on
  ///   first poll, so a zero timeout wins immediately and EVERY outbound dial
  ///   resolves as a spurious dial-timeout failure. Because a serf `join` is
  ///   dispatch-only — it returns the seed count, not a per-exchange outcome —
  ///   the node returns `Ok` from `join` while no reliable push/pull exchange
  ///   ever completes, so it silently never joins.
  /// - `bridge_recv_buf_len == 0`: the per-bridge byte-mover reads into a
  ///   `vec![0u8; bridge_recv_buf_len]`, and a zero-length read returns
  ///   `Ok(0)`, which the bridge treats as peer EOF — so every reliable bridge
  ///   would report EOF instead of reading frames.
  /// - `close_timeout == 0`: the post-`Close` graceful drain fires immediately,
  ///   so every graceful close abandons (RSTs) its queued response bytes.
  /// - `bridge_inbound_cap == 0`: the single-threaded local inbound channel has
  ///   no zero-capacity rendezvous path, so the first bridge read parks forever.
  #[cfg(feature = "tcp")]
  pub(crate) fn validate(&self) -> Result<(), SerfError> {
    if self.dial_timeout.is_zero() {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "dial_timeout",
        "the outbound dial budget must be nonzero: every Connect races TcpStream::connect \
           against a sleep(dial_timeout) in a biased select where the connect is pending on \
           first poll, so a zero timeout wins immediately and every outbound push/pull dial \
           fails as a spurious timeout — and since serf's join is dispatch-only, the node \
           returns Ok from join while never completing a single reliable exchange"
          .to_string(),
      )));
    }
    if self.bridge_recv_buf_len == 0 {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "bridge_recv_buf_len",
        "the per-bridge reliable-stream read buffer must be nonzero: a zero-length read \
           returns Ok(0), which the bridge treats as peer EOF, so every reliable \
           push-pull stream exchange would break"
          .to_string(),
      )));
    }
    if self.close_timeout.is_zero() {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "close_timeout",
        "the reliable graceful-close drain timeout must be nonzero: a zero timeout fires \
           immediately, so a graceful close abandons (RSTs) queued push/pull response bytes \
           instead of draining them, truncating reliable exchanges"
          .to_string(),
      )));
    }
    if self.bridge_inbound_cap == 0 {
      return Err(SerfError::InvalidOption(InvalidOption::new(
        "bridge_inbound_cap",
        "the per-bridge inbound queue capacity must be nonzero: the thread-per-core driver's \
           local inbound channel has no zero-capacity rendezvous path, so a zero capacity \
           parks every bridge read forever and no reliable-stream frame reaches the driver"
          .to_string(),
      )));
    }
    Ok(())
  }
}

impl Default for StreamTransportOptions {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests;
