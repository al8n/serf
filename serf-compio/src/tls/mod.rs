//! TLS-backed serf driver — the TLS sibling of the TCP plane.
//!
//! [`TlsTransport`] owns the bound UDP gossip socket and TCP reliable listener,
//! exactly like [`TcpTransport`](crate::TcpTransport); it differs only in the
//! reliable record layer, which wraps every accepted/dialed byte stream in a
//! rustls session. The machine-layer
//! `serf_proto::StreamEndpoint<I, SocketAddr, Labeled<TlsRecords>>` is built
//! inside [`TlsTransport::run`] from the stored
//! [`TlsOptions`](memberlist_proto::TlsOptions) (cert/key/verifier bundle) and
//! the per-peer SNI provider; the gossip datagram plane stays plain UDP.
//!
//! TLS secures the *reliable* push-pull plane. The unreliable gossip plane is
//! still AEAD-protected by the optional encryption keyring (carried through to
//! the coordinator the same way the TCP plane carries it), so an encrypted
//! cluster protects both planes.
//!
//! ## Server name
//!
//! TLS verifies the peer's certificate against a server name. The
//! `sni_provider` closure on [`TlsTransportOptions`] is called per dial with the
//! peer's membership address; it must return `Some(name)` matching the peer
//! cert's SAN/CN. Returning `None` aborts the dial before the handshake. The
//! default closure returns `Some("localhost".to_string())` for every peer —
//! matching the bundled smoke test's self-signed localhost-SAN cert. Production
//! operators supply a closure mapping each peer to its actual DNS name or SAN.

#![cfg(feature = "tls")]

use core::num::NonZeroU8;
use std::{io::ErrorKind, net::SocketAddr};

use compio::net::{TcpListener, UdpSocket};
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
  /// Construct with defaults. Caller MUST chain [`with_local_id`](Self::with_local_id),
  /// [`with_advertise_addr`](Self::with_advertise_addr), and
  /// [`with_tls_options`](Self::with_tls_options) before passing to
  /// `TlsTransport::new`. The default `sni_provider` returns
  /// `Some("localhost".to_string())` for every peer — matching the bundled
  /// smoke test's self-signed localhost-SAN cert.
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
  /// per-peer SAN certs, supply a closure mapping each dialed `SocketAddr` to
  /// its expected SNI string. Returning `None` for a peer causes the outbound
  /// TLS dial to fail before the handshake.
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
  /// AEAD-protect the gossip datagrams — every node sharing the cluster MUST
  /// carry the same keyring to interop. The reliable plane is secured by the
  /// TLS session independently of this keyring.
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
/// [`TransportRuntime::serf_options`](crate::TransportRuntime).
pub struct TlsTransport<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: I,
  local_address: MaybeResolved<A, SocketAddr>,
  advertise_socket: SocketAddr,
  gossip_socket: UdpSocket,
  tcp_listener: TcpListener,
  stream_options: StreamTransportOptions,
  sni_provider: SniProvider,
  tls_options: TlsOptions,
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

impl<I, A> Transport for TlsTransport<I, A>
where
  I: Id + CheapClone + core::fmt::Debug + core::fmt::Display + Send + Sync + 'static,
  A: Data + Clone + Send + 'static,
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
    // Validate stream knobs that would deterministically break the backend
    // (e.g. a zero `bridge_recv_buf_len` makes every bridge read return a
    // false EOF) BEFORE binding any socket.
    options.stream.validate()?;
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

    // Bind the TCP listener first to claim a port, then bind the UDP gossip
    // socket to the same port. For an ephemeral (`:0`) advertise address,
    // retry the pair on a fresh port if the UDP bind fails transiently:
    // AddrInUse from the TCP/UDP port-space race, or PermissionDenied on
    // Windows when the TCP-assigned port falls in a UDP-excluded range.
    const EPHEMERAL_BIND_RETRIES: usize = 16;
    let ephemeral = advertise_socket.port() == 0;
    let (tcp_listener, advertise_socket, gossip_socket) = {
      let mut attempt = 0usize;
      loop {
        let tcp_listener = TcpListener::bind(advertise_socket)
          .await
          .map_err(SerfError::Io)?;
        // Every post-listener-bind fallible step — the `local_addr` readback
        // (which resolves an ephemeral `:0` to a concrete port) and the paired
        // UDP bind — runs in this one fallible scope, so the single close site
        // below releases the just-bound listener on ANY non-success outcome. No
        // post-bind step can `?`-return past the close and leak the bound port.
        let paired: Result<(SocketAddr, UdpSocket), std::io::Error> = async {
          let bound = tcp_listener.local_addr()?;
          let gossip_socket = UdpSocket::bind(bound).await?;
          Ok((bound, gossip_socket))
        }
        .await;
        match paired {
          Ok((bound, gossip_socket)) => break (tcp_listener, bound, gossip_socket),
          Err(e) => {
            // Release the just-bound listener (awaited — a plain drop is not a
            // synchronous fd release on compio/Windows-IOCP) before retrying or
            // aborting, so its port cannot leak into a same-address rebind.
            // Ignoring Err: closing an abandoned construction's listener.
            let _ = tcp_listener.close().await;
            // A transient ephemeral collision (the TCP/UDP port-space race, or a
            // Windows UDP-excluded port) retries a fresh pair; any other error
            // (readback failure, fatal UDP bind) aborts construction.
            if ephemeral
              && attempt < EPHEMERAL_BIND_RETRIES
              && matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied)
            {
              attempt += 1;
              continue;
            }
            return Err(SerfError::Io(e));
          }
        }
      }
    };

    // Both sockets are now bound. Group every remaining fallible step so that on
    // ANY error BOTH bound sockets are closed (awaited — a plain drop is not a
    // synchronous fd release on compio/Windows-IOCP) before returning, so a
    // failed construction never leaks a bound port to race an immediate
    // same-address rebind into `AddrInUse`.
    //
    // The readback above resolves an ephemeral `:0` to a concrete port but keeps
    // an unspecified IP (a wildcard `0.0.0.0:0` bind yields `0.0.0.0:<port>`):
    // `post_bind_setup` rejects an advertise address peers could not route serf
    // traffic back to, then draws the OS-seeded serf-core RNG before the driver
    // task is spawned so an entropy failure surfaces as `SerfError::Entropy`
    // here. Either failure closes BOTH bound sockets before returning.
    let serf_rng = match crate::transport::post_bind_setup(&advertise_socket) {
      Ok(rng) => rng,
      Err(e) => {
        crate::transport::close_stream_sockets(tcp_listener, gossip_socket).await;
        return Err(e);
      }
    };

    Ok(Self {
      local_id,
      local_address: advertise_input,
      advertise_socket,
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

  async fn run<D, G>(self, runtime: TransportRuntime<Self, D>, gossip_rng: G)
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    // `Serf::new` is generic over `T` and cannot build the record-layer-specific
    // endpoint; build it here from `self`'s stored config. Serf ranks its user
    // broadcasts on three tiers (intent / event / query → ranks 0 / 1 / 2), so
    // the inner memberlist endpoint needs at least three broadcast tiers.
    let inner_opts = EndpointOptions::new(self.local_id, self.advertise_socket)
      .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
    let inner = Endpoint::new(inner_opts, gossip_rng);
    // The TLS coordinator carries the per-peer SNI provider and the cert/key
    // bundle (ridden as the inner options on `LabelOptions`); the membership
    // address IS the transport socket (`|addr| *addr`). Like the plain-TCP
    // plane this stage carries no cluster label (`None`) — TLS isolation comes
    // from the record-layer cert verification and SNI.
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
    // transform, so an unencrypted node is unaffected. The reliable plane is
    // secured by the TLS session regardless.
    #[cfg(encryption)]
    coord.set_encryption_options(self.encryption);
    // Serf's core RNG is seeded from its own OS-drawn entropy (`self.serf_rng`),
    // independent of the coordinator's gossip RNG, so two nodes never share the
    // query-ID / relay-selection stream.
    let rejoin_after_leave = runtime.serf_options.rejoin_after_leave();
    let mut endpoint = serf_proto::StreamEndpoint::<
      Self::Id,
      SocketAddr,
      Labeled<TlsRecords>,
      G,
      StdRng,
      crate::drop_counter::CompioDropCounter,
    >::new_with_rng_in(
      coord,
      runtime.serf_options,
      self.serf_rng,
      runtime.user_drop,
      runtime.member_drop,
    )
    .with_reconnect_delegate(runtime.reconnect_delegate);
    if let Some(md) = runtime.merge_delegate {
      endpoint.set_merge_delegate(md);
    }
    let snapshotter = match runtime.snapshot_file {
      Some((writer, records)) => {
        let replay = serf_proto::snapshot::ReplayResult::replay(records, rejoin_after_leave);
        // Ignoring Err: load_snapshot refuses only on a machine that already
        // lost an id-conflict vote; a freshly built endpoint is Alive.
        let _ = endpoint.load_snapshot(replay, memberlist_proto::Instant::now());
        Some(writer)
      }
      None => None,
    };

    crate::driver::stream::stream_driver_loop::<Self::Id, Labeled<TlsRecords>, D, G, StdRng>(
      endpoint,
      self.gossip_socket,
      self.tcp_listener,
      runtime.commands_rx,
      runtime.events_tx,
      runtime.events_dropped,
      runtime.observation_dropped,
      runtime.snapshot,
      runtime.shutdown_flag,
      runtime.driver_options,
      self.stream_options,
      runtime.delegate,
      None,
      snapshotter,
      #[cfg(encryption)]
      runtime.keyring,
    )
    .await;
  }
}

#[cfg(test)]
mod tests;
