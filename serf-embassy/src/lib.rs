#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/serf/main/art/logo_72x72.png")]
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// `collapsible_if`: the nested `if cond { if let ... }` form is kept deliberately —
// flattening multi-level guards into one long let-chain reads worse here.
#![allow(clippy::collapsible_if, clippy::type_complexity, unexpected_cfgs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

#[cfg(not(any(feature = "std", feature = "alloc")))]
compile_error!("serf-embassy requires the `std` or `alloc` feature");

// Always link the `alloc` crate so path-qualified `alloc::` access (the mailbox
// rings, the `Rc`/`Vec` the runner and handle use) compiles in every feature
// configuration. On a `std` build `alloc` is re-exported by `std`; on no_std it
// is the bare `alloc` crate.
extern crate alloc;

// Alias `alloc` to the name `std` so genuine-heap `std::` paths compile unchanged
// under no_std+alloc, matching the `serf-embedded` crate this driver builds on
// (its public API surfaces `std::sync::Arc` / `std::vec::Vec`, which resolve to
// `alloc` here). Core-resident items are imported from `core::` directly.
#[cfg(all(not(feature = "std"), feature = "alloc"))]
extern crate alloc as std;

mod config;
mod error;
mod gossip_io;
mod mailbox;
mod resolver;
mod runner;
mod serf;
mod shared;
mod stream_io;
mod time;
mod worker;

pub use bytes::Bytes;
pub use config::Options;
pub use error::{InitError, JoinError, OpError, SocketTimeoutOutOfRange};
pub use gossip_io::SerfGossip;
pub use resolver::{AddressResolver, SocketAddrResolver};
pub use runner::Runner;
pub use serf::Serf;
pub use stream_io::{SerfStream, SlotId};
pub use time::{EmbassyInstant, now};

// The Sans-I/O machine types named in the public construction / command
// signatures, re-exported so a caller need not depend on `memberlist-proto`
// directly.
pub use memberlist_proto::{EndpointOptions, Instant, Node, Rng, SeedableRng, SmallRng};
// serf's driving-core surface, re-exported from `serf-embedded` so the embassy API
// is self-contained: the event set, the resolver result + admission predicates, the
// transform config, the await-result join types, and serf's config / error types.
pub use serf_embedded::{
  AliveDelegate, DEFAULT_EVENT_BUFFER_CAP, Event, InvalidOptions, JoinFailed, JoinId, LabelError,
  MAX_RESOLVED_ADDRS_PER_SEED, MaybeOwned, MaybeResolved, MergeDelegate, ReachedSet,
  ReconnectDelegate, ResolvedAddrs, SerfError, SerfOptions, TransformOptions,
  socket_addr_is_routable,
};
// serf's own protocol types named in the command signatures.
pub use serf_proto::{
  endpoint::{QueryId, QueryParams},
  event::QueryEvent,
  members::{Member, MemberStatus, SerfState},
  typed::Tags,
};

// CIDR peer-admission policy, installed via `Options::with_cidr_policy`.
#[cfg(feature = "cidr")]
#[cfg_attr(docsrs, doc(cfg(feature = "cidr")))]
pub use serf_embedded::{AddrParseError, CidrPolicy, IpNet};
// AEAD keyring types (for a caller assembling a `TransformOptions` encryption
// policy) and the inbound key-management request/response types the driver acts on.
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use serf_embedded::{EncryptionOptions, Keyring, SecretKey};
#[cfg(encryption)]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use serf_proto::event::{KeyRequest, KeyRequestOperation, KeyResponseArgs};
