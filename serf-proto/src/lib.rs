//! The serf wire codec and Sans-I/O state machine — pure, no-I/O types shared
//! by the serf driver crates.
//!
//! Depends on `memberlist-proto` for the `Data`/`DataRef` codec primitives; defines serf's
//! own message set and framing on top of them.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

// Alias `alloc` to the name `std` so genuine-heap `std::` paths compile unchanged
// under no_std+alloc (and `#[macro_use]` brings `vec!`/`format!` crate-wide).
// Core-resident items are imported from `core::` directly, never via this alias.
#[cfg(all(not(feature = "std"), feature = "alloc"))]
#[macro_use]
extern crate alloc as std;

#[cfg(feature = "std")]
extern crate std;

// The protocol state is intrinsically heap-backed (Vec/Box/String/maps), so a
// build with neither capability tier is unsupported. Fail with a clear message
// instead of a cascade of "cannot find type `Vec`" errors.
#[cfg(not(any(feature = "std", feature = "alloc")))]
compile_error!("serf-proto requires the `std` or `alloc` feature");

#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use any::{AnyMessage, EncodeError};
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use bridge::BridgeError;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use bridge::{BridgeError as SecretKeyCodecError, secret_key_from_bytes, secret_key_to_bytes};
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use framing::{FrameError, MessageType};
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use typed::{
  ConflictResponseMessage, JoinMessage, LeaveMessage, PushPullMessage, RelayMessage,
};
pub use typed::{
  Coordinate, Filter, QueryFlag, QueryMessage, QueryResponseMessage, TagFilter, Tags, UserEvent,
  UserEventMessage, UserEvents,
};
#[cfg(all(
  any(feature = "aes-gcm", feature = "chacha20-poly1305"),
  any(feature = "tcp", feature = "quic")
))]
pub(crate) use typed::{KeyRequestMessage, KeyResponseMessage};

/// A lamport logical clock value — a monotonically increasing counter used to
/// order serf events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct LamportTime(pub(crate) u64);

impl core::fmt::Display for LamportTime {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl From<u64> for LamportTime {
  fn from(t: u64) -> Self {
    Self(t)
  }
}

impl From<LamportTime> for u64 {
  fn from(t: LamportTime) -> Self {
    t.0
  }
}

impl LamportTime {
  /// Zero lamport time.
  pub const ZERO: Self = LamportTime(0);

  /// Creates a new `LamportTime` from a `u64`.
  pub const fn new(t: u64) -> Self {
    Self(t)
  }
}

#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod any;
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod bridge;
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod framing;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod mathf;
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod messages;
pub mod typed;

#[cfg(feature = "coordinates")]
mod coordinate_client;
#[cfg(feature = "coordinates")]
pub use coordinate_client::{
  CoordinateClient, CoordinateClientStats, CoordinateError, CoordinateOptions,
};

#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod coalesce;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use coalesce::DropCounter;
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod reconnect_delegate;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use reconnect_delegate::ReconnectDelegate;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub mod endpoint;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub mod event;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub mod members;
pub mod options;
pub mod snapshot;

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub mod stream_endpoint;

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
#[doc(inline)]
pub use stream_endpoint::StreamEndpoint;

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
pub mod quic_endpoint;

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
#[doc(inline)]
pub use quic_endpoint::QuicEndpoint;

#[cfg(feature = "coordinates")]
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
pub use snapshot::CoordinateRecord;
pub use snapshot::{ReplayResult, SnapshotError, SnapshotRecord};

#[cfg(all(
  any(feature = "aes-gcm", feature = "chacha20-poly1305"),
  any(feature = "tcp", feature = "quic")
))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(
    any(feature = "aes-gcm", feature = "chacha20-poly1305"),
    any(feature = "tcp", feature = "quic")
  )))
)]
pub use event::{KeyRequest, KeyRequestOperation, KeyResponseArgs};

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
pub use memberlist_proto::SecretKey;

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use memberlist_proto::event::{ExchangeCompleted, ExchangeId, ExchangeKind, ExchangeStatus};

/// `FxHashMap`/`FxHashSet` backed by hashbrown (no_std-capable) with rustc-hash's
/// Fx hasher — rustc-hash's own `Fx*` map aliases are std-only.
pub(crate) type FxHashMap<K, V> = hashbrown::HashMap<K, V, rustc_hash::FxBuildHasher>;
pub(crate) type FxHashSet<T> = hashbrown::HashSet<T, rustc_hash::FxBuildHasher>;
