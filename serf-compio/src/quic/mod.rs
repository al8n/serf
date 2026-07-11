//! QUIC-backed serf driver — the QUIC sibling of the TCP plane.
//!
//! [`QuicTransport`] owns the bound `UdpSocket`; QUIC carries no separate TCP
//! listener — quinn-proto multiplexes the reliable push-pull streams over the
//! single UDP socket, and serf's datagram gossip rides the same socket. The
//! machine-layer `serf_proto::QuicEndpoint<I>` is built inside
//! [`QuicTransport::run`] from the stored [`QuicOptions`] and the
//! `serf_proto::options::Options` carried by the
//! [`TransportRuntime`](crate::TransportRuntime).
//!
//! ## TLS server name
//!
//! QUIC's TLS 1.3 handshake requires a server name to verify the peer's
//! certificate against. [`QuicOptions::new`] installs a cluster-uniform string
//! used for every peer; deployments whose certs name each peer's hostname/IP
//! supply a per-peer SNI closure via `QuicOptions::new_with_sni_provider`.

#![cfg(feature = "quic")]

use core::num::NonZeroU8;
use std::{io::ErrorKind, net::SocketAddr};

use compio::net::UdpSocket;
use hostaddr::HostAddr;
use memberlist_proto::{
  CheapClone, Data, EndpointOptions, Id, MaybeResolved, QuicEndpoint as Coordinator,
};
use rand::rngs::StdRng;
use smol_str::SmolStr;

#[cfg(encryption)]
use memberlist_proto::EncryptionOptions;

/// QUIC config bundle handed to [`QuicTransport`]. Re-exported from
/// `memberlist-proto` so callers don't need a direct dep.
pub use memberlist_proto::QuicOptions;

use crate::{
  SerfError,
  delegate::Delegate,
  resolver::{AdvertiseAddrResolver, Resolver},
  transport::{Transport, TransportRuntime},
};

/// Per-backend QUIC-specific transport options.
///
/// Embedded into the transport constructor. Bundles the local node identifier,
/// the (possibly-unresolved) advertise address, and the caller-built
/// [`QuicOptions`] (quinn-proto `EndpointConfig` / `ServerConfig` /
/// `ClientConfig` / `TransportConfig` bundle plus SNI provider). The cluster
/// label and inbound-label-check policy are supplied via the serf `Options`
/// block (not here), feeding both planes from a single validated source.
pub struct QuicTransportOptions<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: Option<I>,
  advertise_addr: Option<MaybeResolved<A, SocketAddr>>,
  quic_config: Option<QuicOptions>,
  /// Gossip-encryption policy. The default (no keyring) leaves the gossip
  /// datagrams plaintext; attaching a keyring via
  /// [`with_encryption`](Self::with_encryption) makes the coordinator's
  /// `encrypt_gossip`/`decrypt_gossip` AEAD-protect them. The reliable plane
  /// rides quinn's own TLS, so the keyring covers only the gossip datagrams.
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A> QuicTransportOptions<I, A> {
  /// Construct with defaults. Caller MUST chain [`with_local_id`](Self::with_local_id),
  /// [`with_advertise_addr`](Self::with_advertise_addr), and
  /// [`with_quic_config`](Self::with_quic_config) before passing to
  /// `QuicTransport::new`.
  #[inline]
  pub fn new() -> Self {
    Self {
      local_id: None,
      advertise_addr: None,
      quic_config: None,
      #[cfg(encryption)]
      encryption: EncryptionOptions::new(),
    }
  }

  /// Builder: local node identifier.
  #[must_use]
  #[inline]
  pub fn with_local_id(mut self, id: I) -> Self {
    self.local_id = Some(id);
    self
  }

  /// Builder: advertise address (resolved or unresolved).
  #[must_use]
  #[inline]
  pub fn with_advertise_addr(mut self, addr: MaybeResolved<A, SocketAddr>) -> Self {
    self.advertise_addr = Some(addr);
    self
  }

  /// Builder: QUIC config bundle (caller-built quinn-proto configs + SNI).
  #[must_use]
  #[inline]
  pub fn with_quic_config(mut self, cfg: QuicOptions) -> Self {
    self.quic_config = Some(cfg);
    self
  }

  /// Builder: gossip-encryption policy.
  ///
  /// The default (no keyring) keeps the gossip datagrams plaintext, so an
  /// unencrypted node still builds and interoperates. Attach a keyring
  /// (`EncryptionOptions::new().with_keyring(Keyring::new(primary_key))`) to
  /// AEAD-protect the gossip datagrams; every node sharing the cluster MUST
  /// carry the same keyring to interop. The reliable plane is quinn TLS and is
  /// unaffected by this keyring.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  #[must_use]
  #[inline]
  pub fn with_encryption(mut self, encryption: EncryptionOptions) -> Self {
    self.encryption = encryption;
    self
  }

  /// Local node identifier, if set.
  #[inline]
  pub const fn local_id(&self) -> Option<&I> {
    self.local_id.as_ref()
  }

  /// Advertise address, if set.
  #[inline]
  pub const fn advertise_addr(&self) -> Option<&MaybeResolved<A, SocketAddr>> {
    self.advertise_addr.as_ref()
  }

  /// QUIC config bundle, if set.
  #[inline]
  pub const fn quic_config(&self) -> Option<&QuicOptions> {
    self.quic_config.as_ref()
  }

  /// Gossip-encryption policy.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  #[inline]
  pub const fn encryption(&self) -> &EncryptionOptions {
    &self.encryption
  }
}

impl<I, A> Default for QuicTransportOptions<I, A> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

/// QUIC-backed serf transport.
///
/// Owns the bound `UdpSocket` only — quinn-proto multiplexes the reliable
/// push-pull streams over the single UDP socket, no separate listener, and
/// serf's datagram gossip shares the same socket. The machine-layer
/// `serf_proto::QuicEndpoint<I>` is built inside [`Transport::run`] from the
/// stored `quic_config` and the cluster options sourced from
/// [`TransportRuntime::serf_options`](crate::TransportRuntime).
pub struct QuicTransport<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: I,
  local_address: MaybeResolved<A, SocketAddr>,
  advertise_socket: SocketAddr,
  gossip_socket: UdpSocket,
  quic_config: QuicOptions,
  /// Independent OS-seeded seed for the serf core's RNG, drawn once per node in
  /// [`Transport::new`] and consumed when [`Transport::run`] builds the
  /// endpoint via `new_with_rng`. Distinct from the coordinator's gossip RNG so
  /// serf's query IDs and relay choices are not correlated across nodes.
  serf_rng: StdRng,
  /// Gossip-encryption policy applied to the coordinator built in
  /// [`Transport::run`]. Absent keyring ⇒ plaintext gossip (the default).
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A> Transport for QuicTransport<I, A>
where
  I: Id + CheapClone + core::fmt::Debug + core::fmt::Display + Send + Sync + 'static,
  A: Data + Clone + Send + 'static,
{
  type Error = SerfError;
  type Id = I;
  type Address = A;
  type Options = QuicTransportOptions<I, A>;

  async fn new<RES, AR>(
    options: Self::Options,
    resolver: &RES,
    advertise_resolver: &AR,
  ) -> Result<Self, Self::Error>
  where
    RES: Resolver<Address = Self::Address>,
    AR: AdvertiseAddrResolver,
  {
    // Refuse a seed keyring that carries a cross-cipher byte twin, before binding.
    #[cfg(encryption)]
    crate::transport::reject_cross_cipher_keyring(&options.encryption)?;
    let local_id = options.local_id.ok_or_else(|| {
      SerfError::Io(std::io::Error::new(
        ErrorKind::InvalidInput,
        "local_id required",
      ))
    })?;
    let advertise_input = options.advertise_addr.ok_or_else(|| {
      SerfError::Io(std::io::Error::new(
        ErrorKind::InvalidInput,
        "advertise_addr required",
      ))
    })?;
    let quic_config = options.quic_config.ok_or_else(|| {
      SerfError::Io(std::io::Error::new(
        ErrorKind::InvalidInput,
        "quic_config required",
      ))
    })?;

    let advertise_socket = match &advertise_input {
      MaybeResolved::Resolved(s) => *s,
      MaybeResolved::Unresolved(a) => {
        let candidates = resolver
          .resolve(a)
          .await
          .map_err(|e| SerfError::Resolve(std::io::Error::other(e.to_string())))?;
        advertise_resolver.pick(candidates).map_err(|e| {
          SerfError::Resolve(std::io::Error::new(
            ErrorKind::AddrNotAvailable,
            e.to_string(),
          ))
        })?
      }
    };

    // QUIC multiplexes streams + gossip over a single UDP socket; there is no
    // separate listener to claim a port, so a plain bind suffices.
    let gossip_socket = UdpSocket::bind(advertise_socket)
      .await
      .map_err(SerfError::Io)?;

    // The socket is now bound. `quic_post_bind_setup` reads the bound address
    // back (an ephemeral `:0` resolves to a concrete OS-assigned port here, which
    // the node gossips to its peers, while a wildcard `0.0.0.0:0` bind yields
    // `0.0.0.0:<port>` — rejected as an unspecified IP peers could not route serf
    // traffic back to), then draws the OS-seeded serf-core RNG (so an entropy
    // failure surfaces as `SerfError::Entropy` here). On ANY error the bound
    // socket is closed (awaited — a plain drop is not a synchronous fd release on
    // compio/Windows-IOCP) before returning, so a failed construction never leaks
    // the bound UDP port to race an immediate same-address rebind into
    // `AddrInUse`.
    let (advertise_socket, serf_rng) = match crate::transport::quic_post_bind_setup(&gossip_socket)
    {
      Ok(v) => v,
      Err(e) => {
        // Ignoring Err: closing an abandoned construction's socket is best-effort.
        let _ = gossip_socket.close().await;
        return Err(e);
      }
    };

    Ok(Self {
      local_id,
      local_address: advertise_input,
      advertise_socket,
      gossip_socket,
      quic_config,
      serf_rng,
      #[cfg(encryption)]
      encryption: options.encryption,
    })
  }

  #[inline]
  fn local_id(&self) -> &Self::Id {
    &self.local_id
  }

  #[inline]
  fn local_address(&self) -> &MaybeResolved<Self::Address, SocketAddr> {
    &self.local_address
  }

  #[inline]
  fn advertise_address(&self) -> &SocketAddr {
    &self.advertise_socket
  }

  async fn run<D, G>(self, runtime: TransportRuntime<Self, D>, gossip_rng: G)
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    // `Serf::new` is generic over `T` and cannot build the QUIC endpoint (it
    // needs the quinn-proto config bundle); build it here from `self`'s stored
    // config. Serf ranks its user broadcasts on three tiers (intent / event /
    // query → ranks 0 / 1 / 2), so the inner memberlist endpoint needs at least
    // three broadcast tiers.
    let inner_opts = EndpointOptions::new(self.local_id, self.advertise_socket)
      .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
    let inner = memberlist_proto::Endpoint::new(inner_opts, gossip_rng);
    // The shared UDP socket also carries raw QUIC packets, whose size is governed
    // by the quinn `EndpointConfig`'s accepted max UDP payload — which a caller
    // can set above the serf gossip MTU (quinn's default 1472 already exceeds the
    // 1400 default `gossip_mtu`). Read it off the config here, before it moves
    // into the coordinator, so the driver can size its recv buffer for the larger
    // of the two planes and not truncate a full-size QUIC packet.
    let quic_max_udp_payload = self.quic_config.endpoint_ref().get_max_udp_payload_size();
    // The QUIC coordinator owns the quinn endpoint and the per-peer connection
    // pool; `rng_seed = None` seeds quinn's connection-ID generator from OS
    // entropy. `QuicOptions` carries the per-peer SNI plumbing internally.
    #[allow(unused_mut)]
    let mut coord = Coordinator::new(inner, self.quic_config);
    // Install the gossip-encryption keyring so the coordinator's
    // `encrypt_gossip`/`decrypt_gossip` (forwarded from the serf endpoint pump)
    // AEAD-protect the gossip datagrams. A no-keyring policy is the identity
    // transform; the reliable plane is quinn TLS and is unaffected either way.
    #[cfg(encryption)]
    coord.set_encryption_options(self.encryption);
    // Serf's core RNG is seeded from its own OS-drawn entropy (`self.serf_rng`),
    // independent of the coordinator's gossip RNG, so two nodes never share the
    // query-ID / relay-selection stream.
    let endpoint = serf_proto::QuicEndpoint::<Self::Id, G, StdRng>::new_with_rng(
      coord,
      runtime.serf_options,
      self.serf_rng,
    );

    crate::driver::quic::quic_driver_loop::<Self::Id, D, G, StdRng>(
      endpoint,
      self.gossip_socket,
      quic_max_udp_payload,
      runtime.commands_rx,
      runtime.events_tx,
      runtime.events_dropped,
      runtime.observation_dropped,
      runtime.coalesced_user_events_dropped,
      runtime.coalesced_member_events_dropped,
      runtime.snapshot,
      runtime.shutdown_flag,
      runtime.driver_options,
      runtime.delegate,
      None,
      #[cfg(encryption)]
      runtime.keyring,
    )
    .await;
  }
}

#[cfg(all(test, feature = "quic"))]
mod tests;
