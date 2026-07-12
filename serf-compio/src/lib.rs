//! compio-based async driver for the Sans-I/O serf machine.
//!
//! `serf-compio` binds the pure [`serf-proto`] Sans-I/O serf machine to the
//! [compio](https://github.com/compio-rs/compio) async runtime. It owns the
//! driver task, the command channel, the observation delegate dispatch, and the
//! transport plumbing — everything that connects the stateless machine to real
//! I/O. The machine itself (`serf-proto`) carries no runtime reference and runs
//! identically regardless of which driver crate wraps it.
//!
//! # Feature flags
//!
//! Transport (at least one required):
//! - `tcp` (default) — plain-TCP reliable coordinator.
//! - `tls` — TLS-over-TCP reliable coordinator (implies `tcp`).
//! - `quic` — QUIC coordinator.
//! - `quic-rustls-ring` — QUIC with the rustls+ring TLS backend (implies `quic`).
//!
//! Optional protocol extensions:
//! - `coordinates` — Vivaldi network coordinate estimation.
//! - `aes-gcm` — AES-256-GCM encryption backend.
//! - `chacha20-poly1305` — ChaCha20-Poly1305 encryption backend.
//! - `tag-regex` (default) — regex-backed tag-filter matching.
//!
//! Optional config layering:
//! - `serde` — `Serialize`/`Deserialize` on `*Options` types.
//! - `clap` — `clap::Args` on `*Options` types (CLI flags + env vars).
//! - `tracing` — emit `tracing` spans around public driver operations.
//!
//! Optional resolver support:
//! - `dns` — `DnsResolver` backed by `hickory-proto`.
//! - `getifs` — auto-detect the advertise address from host interfaces.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

#[cfg(feature = "tcp")]
mod bridge;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod command;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod delegate;
mod driver;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod drop_counter;
mod error;
mod events;
mod resolver;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod snapshot;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod transport;

#[cfg(any(feature = "tcp", feature = "quic"))]
mod serf;

#[cfg(feature = "tcp")]
mod tcp;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "quic")]
mod quic;

use rand::{
  SeedableRng,
  rngs::{StdRng, SysRng},
};

/// A fresh [`StdRng`] seeded directly from the OS entropy source ([`SysRng`],
/// i.e. `getrandom`) — never from a thread-local generator, so a process that
/// forks after building a node cannot inherit a parent's RNG state and derive
/// the same gossip schedule.
///
/// Drawn before the driver task is spawned (the result is passed to
/// [`Serf::new`], which returns a `Result`), so an OS entropy failure surfaces
/// as [`SerfError::Entropy`] rather than panicking in the spawned task after the
/// handle was already returned.
pub fn gossip_rng() -> crate::Result<StdRng> {
  os_seeded_std_rng()
}

/// Draw a fresh OS-seeded [`StdRng`] — the shared seed source behind both
/// [`gossip_rng`] (the memberlist gossip schedule) and each transport's
/// independent serf-core RNG seed.
///
/// Every call draws fresh OS entropy, so two machines built in one process get
/// mutually-independent RNG streams; the serf core's RNG (which picks query IDs
/// and relay targets) is therefore never correlated with the gossip RNG or with
/// another node's, ruling out the colliding `(ltime, id)` a shared/zero seed
/// would produce.
pub(crate) fn os_seeded_std_rng() -> crate::Result<StdRng> {
  StdRng::try_from_rng(&mut SysRng).map_err(|e| crate::SerfError::Entropy(std::io::Error::other(e)))
}

pub use error::{
  GossipMtuTooSmall, InvalidAdvertiseAddr, InvalidGossipMtu, InvalidOption, Result, SerfError,
};

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use delegate::{
  Delegate, MemberDelegate, MergeDelegate, QueryDelegate, UserEventDelegate, VoidDelegate,
};

#[cfg(all(encryption, unix))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(any(feature = "aes-gcm", feature = "chacha20-poly1305"), unix)))
)]
pub use delegate::{FileKeyringDelegate, KeyringFileError};
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use delegate::{KeyringDelegate, VoidKeyringDelegate};
/// The rotation-durability acknowledgement contract, shared with the other
/// runtime drivers through `serf-driver`.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use serf_driver::{KeyringPersistError, KeyringPersistRx, KeyringPersistence};

/// Gossip-encryption config types re-exported from `memberlist-proto`, so a
/// caller can build a transport's `with_encryption` keyring without naming
/// `memberlist-proto` directly. `EncryptionOptions` carries an optional
/// `Keyring` (primary + secondary `SecretKey`s); attaching one enables
/// encryption, leaving it absent keeps every plane plaintext.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use events::EventStream;

pub use resolver::{
  AdvertiseAddrResolver, AdvertiseResolutionError, FirstAddrResolver, Ipv4PreferringResolver,
  Ipv6PreferringResolver, OsResolver, Resolver, SocketAddrResolver,
};

#[cfg(feature = "dns")]
#[cfg_attr(docsrs, doc(cfg(feature = "dns")))]
pub use resolver::{DEFAULT_DNS_TIMEOUT, DnsResolver};

#[cfg(feature = "getifs")]
#[cfg_attr(docsrs, doc(cfg(feature = "getifs")))]
pub use resolver::{LocalAddrResolver, LocalAddrScope, local_advertise};

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use transport::{Transport, TransportRuntime};

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub use tcp::{TcpTransport, TcpTransportOptions};

#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use tls::{SniProvider, TlsOptions, TlsTransport, TlsTransportOptions};

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
pub use quic::{QuicOptions, QuicTransport, QuicTransportOptions};

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use snapshot::SerfSnapshot;

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use serf::Serf;

pub use driver::options::{
  Channel, DEFAULT_BRIDGE_INBOUND_CAP, DEFAULT_BRIDGE_RECV_BUF_LEN, DEFAULT_CLOSE_TIMEOUT,
  DEFAULT_CMD_FAIRNESS_BUDGET, DEFAULT_DIAL_TIMEOUT, DEFAULT_EVENT_QUEUE_CAP,
  DEFAULT_IDLE_WAKE_INTERVAL, DEFAULT_ITER_DRAIN_CAP, DEFAULT_LEAVE_TIMEOUT,
  DEFAULT_OBSERVATION_CHANNEL, ParseChannelError, RuntimeOptions, StreamTransportOptions,
};
