//! `Transport<R>` — abstracts a per-backend serf driver (TCP/TLS/QUIC) over the
//! agnostic runtime `R`.
//!
//! Concrete impls live in `src/{tcp,tls,quic}.rs`. The Sans-I/O machine endpoint
//! (`serf_proto::StreamEndpoint<I, SocketAddr, …>` for TCP/TLS) is built inside
//! `T::run` from the transport's stored config — a generic `Serf::new` cannot
//! build the backend's private record-layer config + dial closures, so the
//! endpoint never flows through the [`TransportRuntime`] bundle.
//!
//! This is the `Send`/`agnostic` sibling of serf-compio's `!Send`, compio-implicit
//! `Transport`: the trait is generic over `R: agnostic::Runtime`, and `new` /
//! `run` return `Send` futures.

use core::future::Future;
use std::net::SocketAddr;

use agnostic::Runtime;
use memberlist_proto::MaybeResolved;

use crate::{
  delegate::Delegate,
  resolver::{AdvertiseAddrResolver, Resolver},
};

pub mod runtime;
pub use runtime::TransportRuntime;

/// Abstracts a per-backend serf driver over the agnostic runtime `R`. The trait
/// owns construction, resource ownership (bound sockets / TCP listener / quinn
/// endpoint), the Sans-I/O machine endpoint, and — via
/// [`run`](Transport::run) — the spawned driver pump.
///
/// `Self::Error` is bounded by `From<std::io::Error>` — convertible from the OS
/// layer. `Resolver` and `AdvertiseAddrResolver` are call-site arguments to
/// `Self::new`, NOT associated types, so users can swap resolvers without changing
/// the `Transport` type.
///
/// `Self::new` / `Self::run` return `Send` futures so a node can be built and
/// driven on a multi-threaded agnostic runtime.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait Transport<R>: Sized + Send + 'static
where
  R: Runtime,
{
  /// Per-backend error type.
  type Error: core::error::Error + From<std::io::Error> + Send + Sync + 'static;

  /// Node identifier type.
  type Id;

  /// User-facing unresolved address type (e.g. `hostaddr::HostAddr<SmolStr>`).
  type Address;

  /// Per-backend transport-knobs block embedded into `Options<Self>`.
  type Options;

  /// Construct the transport. Resolves `options.advertise_addr` via the
  /// caller-supplied resolvers if it is `MaybeResolved::Unresolved(…)`; binds the
  /// UDP gossip socket + TCP listener (or the QUIC endpoint) and stores them.
  fn new<RES, AR>(
    options: Self::Options,
    resolver: &RES,
    advertise_resolver: &AR,
  ) -> impl Future<Output = Result<Self, Self::Error>> + Send
  where
    RES: Resolver<Address = Self::Address>,
    AR: AdvertiseAddrResolver;

  /// Local node identifier.
  fn local_id(&self) -> &Self::Id;

  /// Original advertise input form — `Resolved` if the user supplied a concrete
  /// `SocketAddr`; `Unresolved` if a hostname was resolved at construction.
  fn local_address(&self) -> &MaybeResolved<Self::Address, SocketAddr>;

  /// Bound advertise `SocketAddr` — what the local node gossips to peers and what
  /// the UDP / QUIC socket is bound to.
  fn advertise_address(&self) -> &SocketAddr;

  /// Run the driver pump. Consumes `self` (sockets and listener move into the
  /// driver), the [`TransportRuntime`] bundle (shared state, events sender,
  /// delegate, tuning knobs), and the gossip RNG the node constructor drew; the
  /// body builds the machine endpoint from `self`'s stored config, spawns the
  /// observation + accept tasks, and awaits the stream driver. Returns when
  /// shutdown is requested.
  fn run<D, G>(
    self,
    runtime: TransportRuntime<Self::Id, D>,
    gossip_rng: G,
  ) -> impl Future<Output = ()> + Send
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static;
}

/// Validate that the resolved advertise address can serve as the local node's
/// reachable, wire-representable contact identity.
///
/// Each transport reads the advertise `SocketAddr` back from its bound socket
/// after construction and gossips it as the node's published contact. An address
/// that the codec encodes fine but that is undialable — classically the wildcard
/// `0.0.0.0:0` / `[::]:0`, whose `local_addr()` readback keeps the unspecified IP
/// — would let the node join a cluster as a member no peer can route to.
///
/// Rejected with [`SerfError::InvalidAdvertiseAddr`](crate::SerfError::InvalidAdvertiseAddr)
/// (not clamped) for either class:
///
/// - NOT A USABLE UNICAST CONTACT — an unspecified IP (`0.0.0.0` / `::`), a
///   multicast IP, an IPv4 broadcast IP (`255.255.255.255`), or a zero port.
/// - NOT REPRESENTABLE ON THE WIRE — a scoped/flow-labelled IPv6 address with a
///   nonzero `scope_id` or `flowinfo`, which the compact `[16B IP][2B port]` wire
///   layout carries neither field of.
///
/// Loopback, private, and global unicast addresses stay valid.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn validate_advertise_addr(advertise_addr: &SocketAddr) -> Result<(), crate::SerfError> {
  let reject = |reason: &str| {
    Err(crate::SerfError::InvalidAdvertiseAddr(
      crate::error::InvalidAdvertiseAddr::new(*advertise_addr, reason.to_string()),
    ))
  };

  let ip = advertise_addr.ip();
  if ip.is_unspecified() {
    return reject(
      "an unspecified IP (0.0.0.0 / ::) is the wildcard-bind address, not a routable contact \
       — peers cannot dial it (set a concrete advertise address, or resolve one from the \
       host's interfaces, when binding the wildcard)",
    );
  }
  if ip.is_multicast() {
    return reject("a multicast IP is a group address, not a single peer's unicast contact");
  }
  if let SocketAddr::V4(v4) = advertise_addr
    && v4.ip().is_broadcast()
  {
    return reject("an IPv4 broadcast IP (255.255.255.255) is not a unicast contact");
  }
  if advertise_addr.port() == 0 {
    return reject(
      "a zero port is undialable — the bound socket's local_addr() readback must carry a \
       concrete port",
    );
  }
  if let SocketAddr::V6(v6) = advertise_addr
    && (v6.scope_id() != 0 || v6.flowinfo() != 0)
  {
    return reject(
      "a scoped/flow-labelled IPv6 address (nonzero scope_id or flowinfo) is not representable \
       on the compact `[16B IP][2B port]` wire layout, so peers could not decode a routable \
       contact for this node",
    );
  }
  Ok(())
}

/// Run the post-bind construction steps common to every transport: reject an
/// undialable advertise address, then draw the OS-seeded serf-core RNG (distinct
/// from the coordinator's gossip RNG so serf's query IDs and relay choices are not
/// correlated across nodes).
///
/// Both are fallible AFTER the transport's socket(s) are already bound, so the
/// caller drops its bound socket(s) before returning the `Err` this produces
/// (dropping an agnostic socket closes its FD synchronously).
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn post_bind_setup(
  advertise_addr: &SocketAddr,
) -> Result<rand::rngs::StdRng, crate::SerfError> {
  validate_advertise_addr(advertise_addr)?;
  crate::os_seeded_std_rng()
}

/// Reject a construction-time encryption keyring that already carries a
/// cross-cipher byte twin — two keys sharing a raw byte value across different
/// cipher variants.
///
/// The coordinator's byte-keyed rotation ops (`promote` / `remove_secondary`)
/// match on bytes alone, so such a ring would make every later key op ambiguous
/// and let a rotation promote or remove the wrong cipher's key. Each transport's
/// `Transport::new` calls this on its stored [`EncryptionOptions`] before binding
/// a socket, establishing the invariant — upheld thereafter by the drivers' live
/// key-op chokepoint — that the live keyring is cross-cipher-collision-free from
/// construction on.
///
/// [`EncryptionOptions`]: memberlist_proto::EncryptionOptions
#[cfg(encryption)]
pub(crate) fn reject_cross_cipher_keyring(
  encryption: &memberlist_proto::EncryptionOptions,
) -> Result<(), crate::SerfError> {
  if let Some(keyring) = encryption.keyring()
    && serf_driver::keyring_carries_cross_cipher_twin(keyring)
  {
    return Err(crate::SerfError::Io(std::io::Error::new(
      std::io::ErrorKind::InvalidInput,
      "encryption keyring carries a cross-cipher key collision",
    )));
  }
  Ok(())
}
