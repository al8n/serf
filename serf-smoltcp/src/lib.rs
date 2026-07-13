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

// Alias `alloc` to the name `std` so genuine-heap `std::` paths compile unchanged
// under no_std+alloc. Core-resident items are imported from `core::` directly,
// never via this alias; heap macros are written path-qualified (`std::vec!`), so
// no crate-wide `#[macro_use]` is needed.
#[cfg(all(not(feature = "std"), feature = "alloc"))]
extern crate alloc as std;

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(any(feature = "std", feature = "alloc")))]
compile_error!("serf-smoltcp requires the `std` or `alloc` feature");

pub use bytes::Bytes;
pub use config::Options;
pub use error::{GossipMtuTooLarge, InitError, JoinError, MediumMismatch};
pub use interface::{
  EthernetAddress, HardwareAddress, InterfaceOptions, IpAddress, IpCidr, Ipv4Address, Ipv6Address,
  Medium, Route,
};
pub use resolver::{Resolver, SocketAddrResolver};
pub use serf::Serf;

// The Sans-I/O machine types that appear in the public construction / command
// signatures, re-exported so a caller need not depend on `memberlist-proto`
// directly.
pub use memberlist_proto::{EndpointOptions, Instant};
// serf's driving-core surface, re-exported from `serf-embedded` so the smoltcp API
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

mod addr;
mod config;
mod error;
mod gossip_io;
mod interface;
mod resolver;
mod serf;
mod stream_io;
