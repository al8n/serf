//! `Transport` trait — abstracts a per-backend serf driver (TCP/TLS/QUIC).
//!
//! Concrete impls live in `src/{tcp,tls,quic}.rs`. The Sans-I/O machine
//! endpoint (`serf_proto::StreamEndpoint<I, SocketAddr, …>` for TCP/TLS;
//! `serf_proto::QuicEndpoint<I>` for QUIC) is built inside `T::run` from the
//! transport's stored config — a generic `Serf::new` cannot build the
//! backend's private record-layer config + dial closures, so the endpoint
//! never flows through the [`TransportRuntime<T, D>`] bundle.

use core::future::Future;
use std::net::SocketAddr;

use crate::{
  delegate::Delegate,
  resolver::{AdvertiseAddrResolver, Resolver},
};
use memberlist_proto::MaybeResolved;

pub mod runtime;
pub use runtime::TransportRuntime;

/// Abstracts a per-backend serf driver. The trait owns construction,
/// resource ownership (bound sockets / TCP listener / quinn endpoint),
/// the Sans-I/O machine endpoint, and the I/O event loop.
///
/// `Self::Error` is bounded by `From<std::io::Error>` — convertible from
/// the OS layer.
///
/// `Resolver` and `AdvertiseAddrResolver` are call-site arguments to
/// `Self::new`, NOT associated types, so users can swap resolvers without
/// changing the `Transport` type. The compatibility bound
/// `RES: Resolver<Address = Self::Address>` enforces type alignment.
///
/// `Self::run`'s future is `!Send` — compio is thread-per-core,
/// `!Send`-first. A future driver crate built on a `Send`-required runtime
/// defines its own `Transport` trait with a `Send` bound.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub trait Transport: Sized + 'static {
  /// Per-backend error type.
  type Error: core::error::Error + From<std::io::Error> + Send + Sync + 'static;

  /// Node identifier type.
  type Id;

  /// User-facing unresolved address type (e.g. `hostaddr::HostAddr<SmolStr>`).
  type Address;

  /// Per-backend transport-knobs block embedded into `Options<Self>`.
  type Options;

  /// Construct the transport. Resolves `options.advertise_addr` via the
  /// caller-supplied resolvers if it is `MaybeResolved::Unresolved(…)`;
  /// binds the UDP gossip socket, the TCP listener, or the QUIC endpoint
  /// and stores them.
  fn new<RES, AR>(
    options: Self::Options,
    resolver: &RES,
    advertise_resolver: &AR,
  ) -> impl Future<Output = Result<Self, Self::Error>>
  where
    RES: Resolver<Address = Self::Address>,
    AR: AdvertiseAddrResolver;

  /// Local node identifier.
  fn local_id(&self) -> &Self::Id;

  /// Original advertise input form — `Resolved` if the user supplied a
  /// concrete `SocketAddr`; `Unresolved` if a hostname was resolved at
  /// construction.
  fn local_address(&self) -> &MaybeResolved<Self::Address, SocketAddr>;

  /// Bound advertise `SocketAddr` — what the local node gossips to peers
  /// and what the UDP / QUIC socket is bound to.
  fn advertise_address(&self) -> &SocketAddr;

  /// Run the I/O event loop. Consumes `self` (sockets and listener move
  /// into the loop), the `TransportRuntime<Self, D>` bundle (channels,
  /// snapshot, delegate, tuning knobs), and the gossip RNG `gossip_rng` the
  /// node constructor drew; the body builds the machine endpoint from
  /// `self`'s stored config. Returns when shutdown is requested.
  fn run<D, G>(self, runtime: TransportRuntime<Self, D>, gossip_rng: G) -> impl Future<Output = ()>
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static;
}

/// Validate that the resolved advertise address can serve as the local node's
/// reachable, wire-representable contact identity.
///
/// Each transport reads the advertise `SocketAddr` back from its bound socket
/// after construction and gossips it as the node's published contact: every
/// peer that learns this node aims its UDP probes and reliable dials at this
/// address. An address that the codec encodes fine but that is undialable —
/// classically the wildcard `0.0.0.0:0` / `[::]:0`, whose `local_addr()`
/// readback keeps the unspecified IP — would let the node join a cluster as a
/// member no peer can route to, so peers eventually suspect and reap it.
///
/// Rejected with [`SerfError::InvalidAdvertiseAddr`] (not clamped — the operator
/// supplies a concrete reachable address, or auto-resolves one from the host's
/// interfaces via the `getifs` resolver) for either class:
///
/// - NOT A USABLE UNICAST CONTACT — an unspecified IP (`0.0.0.0` / `::`), a
///   multicast IP, an IPv4 broadcast IP (`255.255.255.255`), or a zero port.
/// - NOT REPRESENTABLE ON THE WIRE — a scoped/flow-labelled IPv6 address with a
///   nonzero `scope_id` or `flowinfo`, which the compact `[16B IP][2B port]`
///   wire layout carries neither field of, so peers could not decode a routable
///   contact for this node.
///
/// Loopback, private, and global unicast addresses stay valid. Called from each
/// transport's `new` on the post-readback advertise address, before any driver
/// task is spawned; on `Err` the caller explicitly closes (awaited, not a plain
/// drop) every already-bound socket before returning, so the rejected port is
/// released for an immediate rebind.
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
  // IPv4 broadcast (255.255.255.255) is a v4-only concept; match the variant.
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
  // The compact `[16B IP][2B port]` wire layout carries neither scope_id nor
  // flowinfo, so a scoped/flow-labelled IPv6 advertise address (e.g. a
  // link-local `fe80::1%scope`) cannot be encoded as a routable contact.
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
/// undialable advertise address, then draw the OS-seeded serf-core RNG.
///
/// Both are fallible AFTER the transport's socket(s) are already bound, so the
/// caller closes its bound socket(s) (awaited) before returning the `Err` this
/// produces — see [`close_stream_sockets`] / the QUIC single-socket close.
/// Grouping them keeps each transport's error path to a single close site
/// rather than one per fallible step.
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

/// The QUIC variant of [`post_bind_setup`]: read the bound socket's address back
/// (an ephemeral `:0` resolves to a concrete port here), then run the shared
/// advertise validation + RNG draw, returning both.
///
/// Borrows the socket immutably and returns owned values, so the caller can
/// close the socket (awaited) on `Err` or move it into the transport on `Ok`.
#[cfg(feature = "quic")]
pub(crate) fn quic_post_bind_setup(
  gossip: &compio::net::UdpSocket,
) -> Result<(SocketAddr, rand::rngs::StdRng), crate::SerfError> {
  let advertise_addr = gossip.local_addr().map_err(crate::SerfError::Io)?;
  let serf_rng = post_bind_setup(&advertise_addr)?;
  Ok((advertise_addr, serf_rng))
}

/// Close a stream transport's already-bound TCP listener and UDP gossip socket
/// (both awaited) on a construction error path.
///
/// A plain drop is NOT a synchronous fd release on compio (Windows IOCP closes
/// asynchronously), so a failed `new` that merely dropped its already-bound
/// sockets could race an immediate same-address rebind into `AddrInUse`.
/// Awaiting `close()` drains each fd to release before the `Err` propagates,
/// mirroring the awaited close the stream driver's teardown and the ephemeral
/// retry path already use. `tls` implies `tcp`, so this single `tcp`-gated
/// helper serves both stream transports.
#[cfg(feature = "tcp")]
pub(crate) async fn close_stream_sockets(
  listener: compio::net::TcpListener,
  gossip: compio::net::UdpSocket,
) {
  // Ignoring Err: a close error on an abandoned construction is unactionable —
  // the bound fds are being released regardless.
  let _ = listener.close().await;
  let _ = gossip.close().await;
}

#[cfg(all(test, any(feature = "tcp", feature = "quic")))]
mod tests;
