//! TLS-backed serf driver over the agnostic runtime — the TLS sibling of the
//! plain-TCP plane.
//!
//! [`TlsTransport`] owns the bound UDP gossip socket and TCP reliable listener,
//! exactly like [`TcpTransport`](crate::TcpTransport); it differs only in the
//! reliable record layer, which drives rustls's record layer manually over the
//! plain, `R`-agnostic TCP stream the shared stream driver already uses — there is
//! NO runtime TLS stream. The machine-layer
//! `serf_proto::StreamEndpoint<I, SocketAddr, Labeled<TlsRecords>>` is built inside
//! [`Transport::run`] from the stored [`TlsOptions`](memberlist_proto::TlsOptions)
//! (cert/key/verifier bundle) and the per-peer SNI provider; the gossip datagram
//! plane stays plain UDP. This is the `Send`/`agnostic` sibling of serf-compio's
//! `!Send`, compio-bound `TlsTransport`.
//!
//! TLS secures the *reliable* push-pull plane. The unreliable gossip plane is
//! still AEAD-protected by the optional encryption keyring (carried through to the
//! coordinator the same way the TCP plane carries it), so an encrypted cluster
//! protects both planes.
//!
//! ## Server name
//!
//! TLS verifies the peer's certificate against a server name. The `sni_provider`
//! closure on [`TlsTransportOptions`] is called per dial with the peer's
//! membership address; it must return `Some(name)` matching the peer cert's
//! SAN/CN. Returning `None` aborts the dial before the handshake. The default
//! closure returns `Some("localhost".to_string())` for every peer — matching the
//! bundled smoke test's self-signed localhost-SAN cert. Production operators
//! supply a closure mapping each peer to its actual DNS name or SAN.

#![cfg(feature = "tls")]

use core::num::NonZeroU8;
use std::{io::ErrorKind, net::SocketAddr};

use agnostic::{
  Runtime,
  net::{Net, TcpListener, UdpSocket},
};
use hostaddr::HostAddr;
use memberlist_proto::{
  CheapClone, Data, Endpoint, EndpointOptions, Id, MaybeResolved, TlsRecords,
  streams::{LabelOptions, Labeled, StreamEndpoint as Coordinator},
};
use rand::rngs::StdRng;
use smol_str::SmolStr;

/// TLS machine-options bundle (server + client `rustls` config) handed to
/// [`TlsTransport`]. Re-exported from `memberlist-proto` so callers don't need a
/// direct dep on it.
pub use memberlist_proto::TlsOptions;

#[cfg(encryption)]
use memberlist_proto::EncryptionOptions;

use crate::{
  SerfError,
  delegate::Delegate,
  driver::options::StreamTransportOptions,
  resolver::{AdvertiseAddrResolver, Resolver},
  transport::{Transport, TransportRuntime},
};

/// Boxed SNI provider closure: maps a peer's `SocketAddr` to the expected TLS
/// server name used for certificate verification. Returns `None` to abort the
/// dial before the handshake.
pub type SniProvider = Box<dyn Fn(&SocketAddr) -> Option<String> + Send + Sync>;

/// Per-backend TLS-specific transport options.
///
/// Embedded into the transport constructor. Bundles the local node identifier,
/// the (possibly-unresolved) advertise address, the stream-transport tuning
/// knobs, the per-peer SNI provider closure, the machine-layer [`TlsOptions`]
/// bundle (cert/key/verifier), and the optional gossip-encryption policy. The
/// cluster label and inbound-label-check policy are supplied via the serf
/// `Options` block (not here), feeding both planes from a single validated
/// source.
pub struct TlsTransportOptions<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: Option<I>,
  advertise_addr: Option<MaybeResolved<A, SocketAddr>>,
  stream: StreamTransportOptions,
  sni_provider: SniProvider,
  tls_options: Option<TlsOptions>,
  /// Gossip encryption policy. The default (no keyring) leaves the gossip
  /// datagrams plaintext; attaching a keyring via
  /// [`with_encryption`](Self::with_encryption) makes the coordinator's
  /// `encrypt_gossip`/`decrypt_gossip` AEAD-protect them. The reliable plane
  /// rides the TLS session, so the keyring covers only the gossip datagrams.
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A> TlsTransportOptions<I, A> {
  /// Construct with defaults. Caller MUST chain
  /// [`with_local_id`](Self::with_local_id),
  /// [`with_advertise_addr`](Self::with_advertise_addr), and
  /// [`with_tls_options`](Self::with_tls_options) before passing to
  /// `TlsTransport::new`. The default `sni_provider` returns
  /// `Some("localhost".to_string())` for every peer — matching the bundled smoke
  /// test's self-signed localhost-SAN cert.
  #[inline]
  pub fn new() -> Self {
    Self {
      local_id: None,
      advertise_addr: None,
      stream: StreamTransportOptions::new(),
      sni_provider: Box::new(|_addr: &SocketAddr| Some("localhost".to_string())),
      tls_options: None,
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

  /// Builder: stream-transport tuning knobs.
  #[must_use]
  #[inline]
  pub fn with_stream(mut self, opts: StreamTransportOptions) -> Self {
    self.stream = opts;
    self
  }

  /// Builder: SNI provider closure. Default returns
  /// `Some("localhost".to_string())` for every peer; for deployments with
  /// per-peer SAN certs, supply a closure mapping each dialed `SocketAddr` to its
  /// expected SNI string. Returning `None` for a peer causes the outbound TLS dial
  /// to fail before the handshake.
  #[must_use]
  #[inline]
  pub fn with_sni_provider(mut self, f: SniProvider) -> Self {
    self.sni_provider = f;
    self
  }

  /// Builder: TLS machine options (cert/key/verifier). Must be set before
  /// `TlsTransport::new`.
  #[must_use]
  #[inline]
  pub fn with_tls_options(mut self, opts: TlsOptions) -> Self {
    self.tls_options = Some(opts);
    self
  }

  /// Builder: gossip-encryption policy.
  ///
  /// The default (no keyring) keeps the gossip datagrams plaintext, so an
  /// unencrypted node still builds and interoperates. Attach a keyring
  /// (`EncryptionOptions::new().with_keyring(Keyring::new(primary_key))`) to
  /// AEAD-protect the gossip datagrams — every node sharing the cluster MUST carry
  /// the same keyring to interop. The reliable plane is secured by the TLS session
  /// independently of this keyring.
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

  /// Stream-transport tuning knobs.
  #[inline]
  pub const fn stream(&self) -> &StreamTransportOptions {
    &self.stream
  }

  /// SNI provider closure.
  #[inline]
  pub fn sni_provider(&self) -> &(dyn Fn(&SocketAddr) -> Option<String> + Send + Sync) {
    self.sni_provider.as_ref()
  }

  /// TLS machine options bundle, if set.
  #[inline]
  pub const fn tls_options(&self) -> Option<&TlsOptions> {
    self.tls_options.as_ref()
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

impl<I, A> Default for TlsTransportOptions<I, A> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

/// TLS-backed serf transport.
///
/// Owns the bound `UdpSocket` (gossip unreliable plane) and `TcpListener`
/// (reliable coordinator; TLS handshake-on-accept). The machine-layer
/// `serf_proto::StreamEndpoint<I, SocketAddr, Labeled<TlsRecords>>` is built
/// inside [`Transport::run`] from the stored config (`tls_options` +
/// `sni_provider`) and the cluster options sourced from
/// [`TransportRuntime`](crate::TransportRuntime).
pub struct TlsTransport<I, A, R>
where
  R: Runtime,
{
  local_id: I,
  local_address: MaybeResolved<A, SocketAddr>,
  advertise_socket: SocketAddr,
  gossip_socket: <R::Net as Net>::UdpSocket,
  tcp_listener: <R::Net as Net>::TcpListener,
  stream_options: StreamTransportOptions,
  sni_provider: SniProvider,
  tls_options: TlsOptions,
  /// Independent OS-seeded seed for the serf core's RNG, drawn once per node in
  /// [`Transport::new`] and consumed when [`Transport::run`] builds the endpoint
  /// via `new_with_rng`. Distinct from the coordinator's gossip RNG so serf's
  /// query IDs and relay choices are not correlated across nodes.
  serf_rng: StdRng,
  /// Gossip-encryption policy applied to the coordinator built in
  /// [`Transport::run`]. Absent keyring ⇒ plaintext gossip (the default).
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A, R> Transport<R> for TlsTransport<I, A, R>
where
  R: Runtime,
  I:
    Id + CheapClone + Clone + core::fmt::Debug + core::fmt::Display + Send + Sync + Unpin + 'static,
  A: Data + Clone + Send + Sync + 'static,
{
  type Error = SerfError;
  type Id = I;
  type Address = A;
  type Options = TlsTransportOptions<I, A>;

  async fn new<RES, AR>(
    options: Self::Options,
    resolver: &RES,
    advertise_resolver: &AR,
  ) -> Result<Self, Self::Error>
  where
    RES: Resolver<Address = Self::Address>,
    AR: AdvertiseAddrResolver,
  {
    // Validate stream knobs that would deterministically break the backend (e.g. a
    // zero `bridge_recv_buf_len` makes every bridge read return a false EOF) BEFORE
    // binding any socket.
    options.stream.validate()?;

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
    let tls_options = options.tls_options.ok_or_else(|| {
      SerfError::Io(std::io::Error::new(
        ErrorKind::InvalidInput,
        "tls_options required",
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

    // Bind the TCP listener first to claim an OS-assigned free port, then bind the
    // gossip UDP socket to that same port. TCP and UDP port spaces are independent,
    // so for an ephemeral (`:0`) advertise we retry the pair on a fresh port when
    // the UDP bind fails transiently: AddrInUse from the port-space race, or
    // PermissionDenied when the TCP-claimed port falls in a UDP-excluded range. A
    // dropped agnostic socket closes its FD synchronously, so an abandoned attempt
    // never leaks a bound port.
    const EPHEMERAL_BIND_RETRIES: usize = 16;
    let ephemeral = advertise_socket.port() == 0;
    let (tcp_listener, bound, gossip_socket) = {
      let mut attempt = 0usize;
      loop {
        let tcp_listener = <R::Net as Net>::TcpListener::bind(advertise_socket)
          .await
          .map_err(SerfError::Io)?;
        let bound = tcp_listener.local_addr().map_err(SerfError::Io)?;
        match <R::Net as Net>::UdpSocket::bind(bound).await {
          Ok(gossip_socket) => break (tcp_listener, bound, gossip_socket),
          Err(e)
            if ephemeral
              && matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied)
              && attempt < EPHEMERAL_BIND_RETRIES =>
          {
            // Release the claimed TCP port (drop closes the FD) and retry a fresh
            // ephemeral pair.
            drop(tcp_listener);
            attempt += 1;
          }
          Err(e) => return Err(SerfError::Io(e)),
        }
      }
    };

    // Both sockets are now bound. The readback resolves an ephemeral `:0` to a
    // concrete port but keeps an unspecified IP: `post_bind_setup` rejects an
    // advertise address peers could not route serf traffic back to, then draws the
    // OS-seeded serf-core RNG. Either failure drops BOTH bound sockets (dropping an
    // agnostic socket closes its FD) before returning.
    let serf_rng = match crate::transport::post_bind_setup(&bound) {
      Ok(rng) => rng,
      Err(e) => {
        drop(tcp_listener);
        drop(gossip_socket);
        return Err(e);
      }
    };

    Ok(Self {
      local_id,
      local_address: advertise_input,
      advertise_socket: bound,
      gossip_socket,
      tcp_listener,
      stream_options: options.stream,
      sni_provider: options.sni_provider,
      tls_options,
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

  async fn run<D, G>(self, runtime: TransportRuntime<Self::Id, D>, gossip_rng: G)
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    // `Serf::new` is generic over `T` and cannot build the record-layer-specific
    // endpoint; build it here from `self`'s stored config. Serf ranks its user
    // broadcasts on three tiers (intent / event / query → ranks 0 / 1 / 2), so the
    // inner memberlist endpoint needs at least three broadcast tiers.
    let inner_opts = EndpointOptions::new(self.local_id, self.advertise_socket)
      .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
    // Snapshot the reliable push/pull exchange timeout from the SAME options the
    // coordinator is built from, so the driver reconciles an await-result join's
    // caller deadline against the exact deadline the coordinator will stamp.
    let stream_timeout = inner_opts.stream_timeout();
    let inner = Endpoint::new(inner_opts, gossip_rng);
    // The TLS coordinator carries the per-peer SNI provider and the cert/key bundle
    // (ridden as the inner options on `LabelOptions`); the membership address IS the
    // transport socket (`|addr| *addr`). Like the plain-TCP plane this stage carries
    // no cluster label (`None`) — TLS isolation comes from the record-layer cert
    // verification and SNI.
    #[allow(unused_mut)]
    let mut coord = Coordinator::<_, _, Labeled<TlsRecords>, G>::new(
      inner,
      LabelOptions::new_in(None::<Vec<u8>>, self.tls_options),
      self.sni_provider,
      Box::new(|addr: &SocketAddr| *addr),
    );
    // Install the gossip-encryption keyring so the coordinator's
    // `encrypt_gossip`/`decrypt_gossip` (forwarded from the serf endpoint pump)
    // become real on the unreliable plane. A no-keyring policy is the identity
    // transform, so an unencrypted node is unaffected. The reliable plane is secured
    // by the TLS session regardless.
    #[cfg(encryption)]
    coord.set_encryption_options(self.encryption);
    // Serf's core RNG is seeded from its own OS-drawn entropy (`self.serf_rng`),
    // independent of the coordinator's gossip RNG, so two nodes never share the
    // query-ID / relay-selection stream.
    let endpoint = serf_proto::StreamEndpoint::<
      Self::Id,
      SocketAddr,
      Labeled<TlsRecords>,
      G,
      StdRng,
    >::new_with_rng(coord, runtime.serf_options, self.serf_rng);

    let driver =
      crate::driver::stream::spawn_stream_driver::<Self::Id, R, Labeled<TlsRecords>, D, G, StdRng>(
        endpoint,
        self.gossip_socket,
        self.tcp_listener,
        runtime.shared,
        runtime.events_tx,
        runtime.delegate,
        runtime.driver_options,
        self.stream_options,
        None,
        stream_timeout,
        #[cfg(encryption)]
        runtime.keyring,
      );
    driver.await;
  }
}

#[cfg(test)]
mod tests;
