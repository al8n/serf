//! Errors from constructing a [`Serf`](crate::Serf) and from an await-result
//! [`join`](crate::Serf::join).

use alloc::boxed::Box;
use core::{fmt, time::Duration};

use serf_embedded::{InvalidOptions, JoinFailed, SerfError};

/// Why constructing a [`Serf`](crate::Serf) node failed.
///
/// Layers a few embassy-driver construction faults (the TCP socket pool size and
/// the bridge ring capacities) on top of the transport-agnostic
/// [`serf_embedded::InitError`] the shared engine surfaces (port, advertise
/// address, gossip MTU, encryption-keyring, machine-endpoint faults). Every
/// variant is a misconfiguration reported in place of a panic.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
  /// Fewer than two TCP sockets were supplied.
  ///
  /// Construction dedicates one pooled socket to the listener and uses the rest
  /// for dials/accepts: zero sockets is no reliable plane at all, and one leaves
  /// the listener holding the only socket with none free to dial — the node
  /// could never dial a seed to join. The functional minimum is a listener plus
  /// one dial/accept socket. The supplied count is carried for diagnostics.
  TcpPoolTooSmall(usize),
  /// A configured bridge ring capacity
  /// ([`Options::tcp_socket_rx_bytes`](crate::Options::tcp_socket_rx_bytes) or
  /// [`tcp_socket_tx_bytes`](crate::Options::tcp_socket_tx_bytes)) is zero.
  ///
  /// A zero-byte inbound ring can never buffer a received byte for the engine to
  /// drain, and a zero-byte outbound ring can never accept a byte from the
  /// engine to write — a silently-dead reliable plane. Both must be non-zero.
  ZeroBridgeRing,
  /// The per-socket inactivity timeout is out of the valid range.
  ///
  /// [`Options::socket_timeout`](crate::Options::socket_timeout), as embassy-net installs it
  /// into smoltcp (floored to whole microseconds — the embassy tick count handed over via
  /// `as_micros`), must be at least one microsecond and strictly greater than BOTH the
  /// graceful-close bound ([`close_timeout`](crate::Options::close_timeout)) and the
  /// machine's reliable-exchange deadline (`EndpointOptions::stream_timeout`) — otherwise
  /// embassy-net could abort a slow-but-valid exchange before the engine's own policy
  /// fires — AND no larger than a sane maximum, so it cannot overflow the
  /// embassy-time-to-smoltcp duration conversion into a wrapped (effectively past)
  /// deadline. Because the bound is enforced on the value rounded DOWN to whole installed
  /// microseconds, a coarse or very fine tick rate can reject a timeout that looks valid as
  /// a `core::Duration`; the offending values, the maximum, and the platform tick rate are
  /// carried for diagnostics.
  SocketTimeoutOutOfRange(SocketTimeoutOutOfRange),
  /// The shared engine rejected the configuration (see
  /// [`serf_embedded::InitError`]): a zero/over-ceiling gossip MTU, a
  /// non-routable or port-mismatched advertise address, a zero port or
  /// close-timeout, an unusable encryption keyring, or a machine-endpoint init
  /// failure (including an entropy draw failure).
  Engine(serf_embedded::InitError),
  /// The address resolver failed while resolving the advertise address.
  ///
  /// The resolver's error type is generic, so it is boxed to preserve the
  /// `source()` chain; a caller that knows its concrete resolver can downcast.
  /// No `Send`/`Sync` bound — the embassy [`AddressResolver`](crate::AddressResolver)
  /// is single-threaded by design, so its error need not cross threads.
  Resolve(Box<dyn core::error::Error + 'static>),
  /// The address resolver succeeded but yielded no address for the advertise
  /// address, so the node would have nothing to advertise.
  NoAddresses,
  /// The platform entropy source ([`getrandom`]) failed while seeding the default
  /// gossip / serf RNGs in [`Serf::new`](crate::Serf::new). Use
  /// [`Serf::new_with_rng`](crate::Serf::new_with_rng) to supply your own RNGs
  /// and avoid the platform entropy draw entirely.
  Entropy,
  /// The serf-level [`SerfOptions`](crate::SerfOptions) failed
  /// [`validate`](crate::SerfOptions::validate): `max_user_event_size` exceeds
  /// the absolute `USER_EVENT_SIZE_LIMIT` ceiling, or a coalescing quiescent
  /// period is not strictly less than its coalesce period. Carries the typed
  /// cause.
  InvalidSerfOptions(InvalidOptions),
}

impl InitError {
  /// Whether construction failed because the TCP socket pool had fewer than two
  /// sockets.
  #[inline]
  pub const fn is_tcp_pool_too_small(&self) -> bool {
    matches!(self, InitError::TcpPoolTooSmall(_))
  }

  /// Whether a configured bridge ring capacity was zero.
  #[inline]
  pub const fn is_zero_bridge_ring(&self) -> bool {
    matches!(self, InitError::ZeroBridgeRing)
  }

  /// Whether the per-socket inactivity timeout was out of range.
  #[inline]
  pub const fn is_socket_timeout_out_of_range(&self) -> bool {
    matches!(self, InitError::SocketTimeoutOutOfRange(_))
  }

  /// Whether the shared engine rejected the configuration.
  #[inline]
  pub const fn is_engine(&self) -> bool {
    matches!(self, InitError::Engine(_))
  }

  /// Whether the resolver failed on the advertise address.
  #[inline]
  pub const fn is_resolve(&self) -> bool {
    matches!(self, InitError::Resolve(_))
  }

  /// Whether the resolver yielded no address for the advertise address.
  #[inline]
  pub const fn is_no_addresses(&self) -> bool {
    matches!(self, InitError::NoAddresses)
  }

  /// Whether the platform entropy source failed while seeding the RNGs.
  #[inline]
  pub const fn is_entropy(&self) -> bool {
    matches!(self, InitError::Entropy)
  }
}

/// Payload for [`InitError::SocketTimeoutOutOfRange`]: the configured socket timeout,
/// the two engine deadlines it must exceed, the maximum it must not exceed, and the
/// platform tick rate the rounded comparison used (a coarse rate can reject a timeout
/// that looks valid before it is rounded down to whole ticks).
#[derive(Debug, Clone, Copy)]
pub struct SocketTimeoutOutOfRange {
  /// The configured per-socket inactivity timeout.
  pub socket_timeout: Duration,
  /// The graceful-close bound it must exceed (after flooring to installed microseconds).
  pub close_timeout: Duration,
  /// The machine's reliable-exchange deadline it must exceed (after flooring to installed
  /// microseconds).
  pub stream_timeout: Duration,
  /// The maximum it must not exceed, so its conversion cannot overflow.
  pub max: Duration,
  /// The platform `embassy-time` tick rate (Hz) that determines the installed-microsecond
  /// flooring the comparison used.
  pub tick_hz: u64,
}

impl fmt::Display for InitError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      InitError::TcpPoolTooSmall(n) => write!(
        f,
        "the TCP socket pool needs at least 2 sockets (a listener plus one \
         dial/accept socket); got {n}"
      ),
      InitError::ZeroBridgeRing => {
        f.write_str("tcp_socket_rx_bytes and tcp_socket_tx_bytes must both be non-zero")
      }
      InitError::SocketTimeoutOutOfRange(s) => write!(
        f,
        "socket_timeout ({:?}), as installed into smoltcp (floored to whole microseconds \
         at the {} Hz platform tick rate), must be at least one microsecond and greater \
         than both close_timeout ({:?}) and stream_timeout ({:?}), and no larger than {:?}",
        s.socket_timeout, s.tick_hz, s.close_timeout, s.stream_timeout, s.max
      ),
      InitError::Engine(e) => write!(f, "{e}"),
      InitError::Resolve(e) => write!(f, "advertise address resolution failed: {e}"),
      InitError::NoAddresses => f.write_str("advertise address resolution returned no addresses"),
      InitError::Entropy => f.write_str("entropy source failed while seeding the RNGs"),
      InitError::InvalidSerfOptions(e) => write!(f, "invalid serf options: {e}"),
    }
  }
}

impl From<serf_embedded::InitError> for InitError {
  fn from(e: serf_embedded::InitError) -> Self {
    InitError::Engine(e)
  }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl std::error::Error for InitError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      InitError::Engine(e) => Some(e),
      InitError::Resolve(e) => Some(e.as_ref()),
      _ => None,
    }
  }
}

/// Why an await-result [`join`](crate::Serf::join) did not succeed.
///
/// A join first resolves each seed through the supplied resolver, then dispatches
/// the intent to the engine and awaits its outcome; any step can fail, and a
/// fully-dispatched join can still reach no seed.
///
/// `Resolve` is `!Clone` / `!PartialEq` (it boxes the resolver's generic error),
/// so this enum derives neither — match on the variant (or the `is_*` predicates)
/// instead.
#[derive(Debug)]
#[non_exhaustive]
pub enum JoinError {
  /// The address resolver failed while resolving a seed.
  ///
  /// The resolver's error type is generic, so it is boxed to preserve the
  /// `source()` chain; a caller that knows its concrete resolver can downcast.
  /// No `Send`/`Sync` bound — the embassy [`AddressResolver`](crate::AddressResolver)
  /// is single-threaded by design, so its error need not cross threads.
  Resolve(Box<dyn core::error::Error + 'static>),
  /// The engine rejected the join up front (e.g. the node is not in the running
  /// state).
  Control(SerfError),
  /// A non-empty seed set resolved to no wire address — a discovery failure
  /// rather than a successful no-op join.
  NoAddresses,
  /// Every dispatched push/pull terminated without contacting a seed. The
  /// [`JoinFailed`] payload carries the requested-seed count.
  Failed(JoinFailed),
  /// The run loop stopped after a lost id-conflict `Event::Shutdown` before this
  /// join could resolve — the node is no longer active. A join in flight when the
  /// conflict-loss lands resolves here rather than hanging, and a join attempted
  /// after shutdown fails fast with it. Mirrors serf-reactor's `SerfError::Shutdown`
  /// for the join reply.
  Shutdown,
}

impl JoinError {
  /// Whether this is a resolver failure.
  #[inline]
  pub const fn is_resolve(&self) -> bool {
    matches!(self, JoinError::Resolve(_))
  }

  /// Whether the engine rejected the join up front.
  #[inline]
  pub const fn is_control(&self) -> bool {
    matches!(self, JoinError::Control(_))
  }

  /// Whether a non-empty seed set resolved to no wire address.
  #[inline]
  pub const fn is_no_addresses(&self) -> bool {
    matches!(self, JoinError::NoAddresses)
  }

  /// Whether every dispatched push/pull terminated without contacting a seed.
  #[inline]
  pub const fn is_failed(&self) -> bool {
    matches!(self, JoinError::Failed(_))
  }

  /// Whether the run loop stopped after a lost id-conflict shutdown before the
  /// join resolved.
  #[inline]
  pub const fn is_shutdown(&self) -> bool {
    matches!(self, JoinError::Shutdown)
  }
}

impl fmt::Display for JoinError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      JoinError::Resolve(e) => write!(f, "seed address resolution failed: {e}"),
      JoinError::Control(e) => write!(f, "join was rejected: {e}"),
      JoinError::NoAddresses => f.write_str("no wire address resolved for any seed"),
      JoinError::Failed(e) => write!(f, "{e}"),
      JoinError::Shutdown => {
        f.write_str("the node shut down after losing an id-conflict vote before the join resolved")
      }
    }
  }
}

impl From<SerfError> for JoinError {
  fn from(e: SerfError) -> Self {
    JoinError::Control(e)
  }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl std::error::Error for JoinError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      JoinError::Resolve(e) => Some(e.as_ref()),
      JoinError::Control(e) => Some(e),
      JoinError::Failed(e) => Some(e),
      JoinError::NoAddresses | JoinError::Shutdown => None,
    }
  }
}

/// Why a [`Serf`](crate::Serf) command (`user_event`, `query`, `leave`,
/// `force_leave`, `respond`, `set_tags`, key management) could not run.
///
/// Either the engine rejected the command ([`Serf`](Self::Serf)), or the run loop
/// has already stopped after a lost id-conflict `Event::Shutdown`
/// ([`Shutdown`](Self::Shutdown)) — after which the node is no longer active and
/// the handle rejects every command up front, before touching the (now
/// winding-down) engine, so a losing node cannot keep mutating serf state under the
/// duplicate identity.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpError {
  /// The run loop stopped after a lost id-conflict `Event::Shutdown`; the node is
  /// no longer active and rejects further commands. Mirrors serf-reactor's
  /// `SerfError::Shutdown`.
  Shutdown,
  /// The engine rejected the command (e.g. an oversized user event, a
  /// past-deadline `respond`, or a leave/join from an invalid lifecycle state).
  Serf(SerfError),
}

impl OpError {
  /// Whether the node has shut down after a lost id-conflict vote and no longer
  /// accepts commands.
  #[inline]
  pub const fn is_shutdown(&self) -> bool {
    matches!(self, OpError::Shutdown)
  }

  /// Whether the engine rejected the command.
  #[inline]
  pub const fn is_serf(&self) -> bool {
    matches!(self, OpError::Serf(_))
  }

  /// The underlying [`SerfError`] when the engine rejected the command, else
  /// `None` (a shutdown refusal carries no engine error).
  #[inline]
  pub const fn as_serf(&self) -> Option<&SerfError> {
    match self {
      OpError::Serf(e) => Some(e),
      OpError::Shutdown => None,
    }
  }
}

impl fmt::Display for OpError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      OpError::Shutdown => f.write_str(
        "the node has shut down after losing an id-conflict vote; it no longer accepts commands",
      ),
      OpError::Serf(e) => write!(f, "{e}"),
    }
  }
}

impl From<SerfError> for OpError {
  fn from(e: SerfError) -> Self {
    OpError::Serf(e)
  }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl std::error::Error for OpError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      OpError::Serf(e) => Some(e),
      OpError::Shutdown => None,
    }
  }
}
