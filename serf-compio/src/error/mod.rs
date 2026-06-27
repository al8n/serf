//! Error types for serf-compio.

use core::fmt;
use std::{io, net::SocketAddr};

pub use serf_driver::error::{GossipMtuTooSmall, InvalidOption};

/// Payload for [`SerfError::InvalidGossipMtu`]: the configured `gossip_mtu`
/// exceeds the largest plaintext gossip payload that can fit a single UDP
/// datagram once any encryption wrapper is added. Carries the configured value
/// and the effective ceiling.
#[derive(Debug)]
pub struct InvalidGossipMtu {
  configured: usize,
  ceiling: usize,
}

impl InvalidGossipMtu {
  /// Build a new payload from the configured `gossip_mtu` and the ceiling.
  #[inline]
  pub fn new(configured: usize, ceiling: usize) -> Self {
    Self {
      configured,
      ceiling,
    }
  }

  /// The configured `gossip_mtu` that was rejected.
  #[inline]
  pub fn configured(&self) -> usize {
    self.configured
  }

  /// The effective ceiling — the largest plaintext `gossip_mtu` whose wire
  /// datagram still fits a single UDP packet after any encryption wrapper.
  #[inline]
  pub fn ceiling(&self) -> usize {
    self.ceiling
  }
}

impl fmt::Display for InvalidGossipMtu {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "gossip_mtu {} exceeds the maximum sendable plaintext gossip payload of {} bytes \
       (a gossip packet is one UDP datagram capped after the encryption wrapper); \
       a larger gossip_mtu would make near-MTU gossip packets deterministically unsendable",
      self.configured, self.ceiling,
    )
  }
}

/// Payload for [`SerfError::InvalidAdvertiseAddr`]: the resolved advertise
/// address cannot serve as the local node's reachable contact identity. Two
/// independent classes are rejected:
///
/// - NOT A USABLE UNICAST CONTACT — an unspecified IP, a multicast IP, an IPv4
///   broadcast IP, or a zero port. Such an address is undialable.
/// - NOT REPRESENTABLE ON THE WIRE — a scoped/flow-labelled IPv6 `SocketAddr`
///   with a nonzero `scope_id` or `flowinfo` that the compact wire layout
///   (`[16B IP][2B port]`) cannot carry.
#[derive(Debug)]
pub struct InvalidAdvertiseAddr {
  addr: SocketAddr,
  reason: String,
}

impl InvalidAdvertiseAddr {
  /// Build a new payload from the rejected advertise address and the reason.
  #[inline]
  pub fn new(addr: SocketAddr, reason: String) -> Self {
    Self { addr, reason }
  }

  /// The advertise address that was rejected.
  #[inline]
  pub fn addr(&self) -> SocketAddr {
    self.addr
  }

  /// The reason the address cannot serve as the local node's contact identity.
  #[inline]
  pub fn reason(&self) -> &str {
    &self.reason
  }
}

impl fmt::Display for InvalidAdvertiseAddr {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "advertise address {} cannot serve as this node's reachable contact identity, \
       so peers that learn it could not route serf traffic to this node: {}",
      self.addr, self.reason,
    )
  }
}

/// Errors returned by [`Serf`](crate::Serf) operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SerfError {
  /// I/O error from the OS, socket, or compio runtime.
  #[error(transparent)]
  Io(#[from] io::Error),

  /// The OS entropy source failed while seeding the gossip RNG. Surfaced in
  /// the node constructor (which returns this `Result`) before the driver task
  /// is spawned, so a failure is surfaced here rather than panicking in the
  /// spawned task.
  #[error("OS entropy source failed while seeding the gossip RNG")]
  Entropy(#[source] io::Error),

  /// Address resolution failed (DNS error, etc.).
  #[error("address resolution: {0}")]
  Resolve(io::Error),

  /// A serf endpoint operation (join, leave, set_tags, user_event, query, …)
  /// returned a machine-level error. Carries the typed
  /// [`serf_proto::endpoint::Error`] so callers can dispatch on the specific
  /// cause.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  #[error(transparent)]
  Proto(#[from] serf_proto::endpoint::Error),

  /// Encryption codec error from memberlist-wire.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  #[error(transparent)]
  Encryption(#[from] memberlist_proto::EncryptionError),

  /// A graceful [`leave`](crate::Serf::leave) did not complete within the
  /// driver's configured leave timeout. The leave was initiated but the driver
  /// cannot confirm peers were notified.
  #[error("leave did not complete within the configured leave timeout")]
  LeaveTimeout,

  /// The driver task has shut down and is no longer accepting commands.
  #[error("driver shut down")]
  Shutdown,

  /// The local node has left the cluster; the operation requires an active node.
  #[error("the local node has left the cluster; the operation requires a running node")]
  NotRunning,

  /// The configured `gossip_mtu` exceeds the ceiling after the encryption
  /// wrapper is applied. Returned at construction (fail-fast, before any socket
  /// is bound) so the misconfiguration is surfaced rather than producing
  /// deterministically dropped gossip.
  #[error("{0}")]
  InvalidGossipMtu(InvalidGossipMtu),

  /// The configured `gossip_mtu` is below the floor needed to carry the
  /// mandatory single-datagram control packets the SWIM protocol always emits.
  #[error("{0}")]
  GossipMtuTooSmall(GossipMtuTooSmall),

  /// The resolved advertise address cannot serve as the local node's reachable
  /// contact identity.
  #[error("{0}")]
  InvalidAdvertiseAddr(InvalidAdvertiseAddr),

  /// An operator-set driver tuning knob was given a value that would
  /// deterministically break the node rather than merely degrade it.
  #[error("{0}")]
  InvalidOption(InvalidOption),

  /// Sending a command to the driver failed because the channel is closed.
  #[error("send to driver failed (channel closed)")]
  CommandSend,

  /// The driver's reply channel was dropped before a reply arrived.
  #[error("driver reply channel closed")]
  ReplyClosed,
}

/// Convenience [`Result`] for [`SerfError`].
pub type Result<T, E = SerfError> = core::result::Result<T, E>;

#[cfg(test)]
mod tests;
