#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// `collapsible_if`: the nested `if cond { if let ... }` form is kept deliberately —
// flattening multi-level guards into one long let-chain reads worse here.
#![allow(clippy::collapsible_if, clippy::type_complexity, unexpected_cfgs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

// Alias `alloc` to the name `std` so genuine-heap `std::` paths compile unchanged
// under no_std+alloc. Core-resident items are imported from `core::` directly,
// never via this alias; heap macros are written path-qualified (`std::vec!`), so
// no crate-wide `#[macro_use]` is needed.
#[cfg(all(not(feature = "std"), feature = "alloc"))]
extern crate alloc as std;

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(any(feature = "std", feature = "alloc")))]
compile_error!("serf-embedded requires the `std` or `alloc` feature");

mod cidr;
pub mod engine;

pub use engine::{DEFAULT_EVENT_BUFFER_CAP, JoinFailed, JoinId, ReachedSet, SerfEngine};

// ── Reused memberlist-embedded glue ──────────────────────────────────────────
//
// serf-embedded reuses the payload-agnostic driving glue from
// `memberlist-embedded` directly and re-exports it here so a serf embedded
// driver has a single import surface for the seams, the reliable plane, the
// resolver result, the transform pipeline, and the engine sizing / error types.

pub use memberlist_embedded::{
  // Engine sizing + construction-time validation.
  DEFAULT_CLOSE_TIMEOUT,
  // The datagram + pooled-stream I/O seams a driver supplies to `SerfEngine::pump`.
  GossipIo,
  GossipMtuTooLarge,
  InitError,
  // Engine sizing (ports / close timeout / CIDR policy).
  Options,
  StreamIo,
  StreamIoError,
  // Construction-time preflight (advertise-independent config).
  validate_runtime_config,
};
// The pooled-stream reliable plane serf-embedded drives directly.
pub use memberlist_embedded::reliable::{ConnState, Connection, Pool, ReliablePlane};
// The engine sizing module (for `config::Options` / `config::DEFAULT_CLOSE_TIMEOUT`).
pub use memberlist_embedded::config;
// The bounded, no-heap resolver result shared by the embedded drivers.
pub use memberlist_embedded::resolver::{MAX_RESOLVED_ADDRS_PER_SEED, ResolvedAddrs};
// The last-line routable-address screen the engine and every egress chokepoint apply.
pub use memberlist_embedded::socket_addr_is_routable;
// The cross-transport transform configuration (label + AEAD encryption; serf's
// gossip plane carries no compression / checksum, so only those are surfaced).
pub use memberlist_embedded::{LabelError, TransformOptions};
// Admission predicates a caller installs at construction.
pub use memberlist_embedded::{AliveDelegate, MaybeOwned, MaybeResolved, MergeDelegate};
// CIDR peer-admission policy, installed via `Options::with_cidr_policy`.
#[cfg(feature = "cidr")]
#[cfg_attr(docsrs, doc(cfg(feature = "cidr")))]
pub use memberlist_embedded::{AddrParseError, CidrPolicy, IpNet};
// AEAD keyring types, for a driver assembling a `TransformOptions` encryption
// policy.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use memberlist_embedded::{EncryptionOptions, Keyring, SecretKey};

// ── serf construction / event surface ────────────────────────────────────────
//
// The serf config a `SerfEngine` is built from, and the event set its
// `poll_event` surfaces, re-exported so a driver imports them from one place.

/// serf's own [`Endpoint`](serf_proto::endpoint::Endpoint) configuration, distinct
/// from the memberlist-layer engine [`Options`].
pub use serf_proto::options::Options as SerfOptions;
pub use serf_proto::{endpoint::Error as SerfError, event::Event};
