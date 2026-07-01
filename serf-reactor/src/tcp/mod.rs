//! TCP-backed serf driver over the agnostic runtime.
//!
//! [`TcpTransport`] owns the bound UDP gossip socket and TCP reliable listener.
//! The machine-layer `serf_proto::StreamEndpoint<I, SocketAddr, RawRecords, …>` is
//! built inside [`TcpTransport::run`] from the stored stream knobs and the serf
//! `Options` carried by the [`TransportRuntime`](crate::TransportRuntime). This is
//! the `Send`/`agnostic` sibling of serf-compio's `!Send`, compio-bound
//! `TcpTransport`.

#![cfg(feature = "tcp")]

use core::num::NonZeroU8;
use std::{io::ErrorKind, net::SocketAddr};

use agnostic::{
  Runtime,
  net::{Net, TcpListener, UdpSocket},
};
use hostaddr::HostAddr;
use memberlist_proto::{
  CheapClone, Data, Endpoint, EndpointOptions, Id, MaybeResolved, RawRecords,
  streams::{LabelOptions, StreamEndpoint as Coordinator},
};
use rand::rngs::StdRng;
use smol_str::SmolStr;

#[cfg(encryption)]
use memberlist_proto::EncryptionOptions;

use crate::{
  SerfError,
  delegate::Delegate,
  driver::options::StreamTransportOptions,
  resolver::{AdvertiseAddrResolver, Resolver},
  transport::{Transport, TransportRuntime},
};

/// Per-backend TCP-specific transport options.
///
/// Bundles the local node identifier, the (possibly-unresolved) advertise
/// address, and the stream-transport tuning knobs. The cluster label and
/// inbound-label-check policy are supplied via the serf `Options` block (not
/// here), feeding both planes from a single validated source.
pub struct TcpTransportOptions<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: Option<I>,
  advertise_addr: Option<MaybeResolved<A, SocketAddr>>,
  stream: StreamTransportOptions,
  /// Gossip-and-reliable encryption policy. The default (no keyring) leaves both
  /// planes plaintext; attaching a keyring via [`with_encryption`](Self::with_encryption)
  /// makes the coordinator's `encrypt_gossip`/`decrypt_gossip` (and the plain-TCP
  /// reliable record layer) AEAD-protect every datagram and stream unit.
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A> TcpTransportOptions<I, A> {
  /// Construct with defaults. Caller MUST chain
  /// [`with_local_id`](Self::with_local_id) and
  /// [`with_advertise_addr`](Self::with_advertise_addr) before passing to
  /// `TcpTransport::new`.
  #[inline]
  pub fn new() -> Self {
    Self {
      local_id: None,
      advertise_addr: None,
      stream: StreamTransportOptions::new(),
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

  /// Builder: gossip-and-reliable encryption policy.
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

  /// Gossip-and-reliable encryption policy.
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

impl<I, A> Default for TcpTransportOptions<I, A> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

/// TCP-backed serf transport.
///
/// Owns the bound `UdpSocket` (gossip unreliable plane) and `TcpListener`
/// (reliable coordinator). The machine-layer
/// `serf_proto::StreamEndpoint<I, SocketAddr, RawRecords>` is built inside
/// [`Transport::run`] from the serf options sourced from
/// [`TransportRuntime`](crate::TransportRuntime).
pub struct TcpTransport<I, A, R>
where
  R: Runtime,
{
  local_id: I,
  local_address: MaybeResolved<A, SocketAddr>,
  advertise_socket: SocketAddr,
  gossip_socket: <R::Net as Net>::UdpSocket,
  tcp_listener: <R::Net as Net>::TcpListener,
  stream_options: StreamTransportOptions,
  /// Independent OS-seeded seed for the serf core's RNG, drawn once per node in
  /// [`Transport::new`] and consumed when [`Transport::run`] builds the endpoint.
  serf_rng: StdRng,
  /// Gossip-and-reliable encryption policy applied to the coordinator built in
  /// [`Transport::run`]. Absent keyring ⇒ plaintext (the default).
  #[cfg(encryption)]
  encryption: EncryptionOptions,
}

impl<I, A, R> Transport<R> for TcpTransport<I, A, R>
where
  R: Runtime,
  I:
    Id + CheapClone + Clone + core::fmt::Debug + core::fmt::Display + Send + Sync + Unpin + 'static,
  A: Data + Clone + Send + Sync + 'static,
{
  type Error = SerfError;
  type Id = I;
  type Address = A;
  type Options = TcpTransportOptions<I, A>;

  async fn new<RES, AR>(
    options: Self::Options,
    resolver: &RES,
    advertise_resolver: &AR,
  ) -> Result<Self, Self::Error>
  where
    RES: Resolver<Address = Self::Address>,
    AR: AdvertiseAddrResolver,
  {
    // Validate stream knobs that would deterministically break the backend BEFORE
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
    let inner = Endpoint::new(inner_opts, gossip_rng);
    // Plain TCP has no SNI (`|_| None`) and a membership address that IS the
    // transport socket (`|addr| *addr`). No cluster label at this stage.
    #[allow(unused_mut)]
    let mut coord = Coordinator::<_, _, RawRecords, G>::new(
      inner,
      LabelOptions::new_in(None::<Vec<u8>>, ()),
      Box::new(|_: &SocketAddr| -> Option<String> { None }),
      Box::new(|addr: &SocketAddr| *addr),
    );
    // Install the gossip-encryption keyring. A no-keyring policy is the identity
    // transform, so an unencrypted node is unaffected.
    #[cfg(encryption)]
    coord.set_encryption_options(self.encryption);
    // Serf's core RNG is seeded from its own OS-drawn entropy (`self.serf_rng`),
    // independent of the coordinator's gossip RNG.
    let endpoint =
      serf_proto::StreamEndpoint::<Self::Id, SocketAddr, RawRecords, G, StdRng>::new_with_rng(
        coord,
        runtime.serf_options,
        self.serf_rng,
      );

    let driver = crate::driver::stream::spawn_stream_driver::<Self::Id, R, RawRecords, D, G, StdRng>(
      endpoint,
      self.gossip_socket,
      self.tcp_listener,
      runtime.shared,
      runtime.events_tx,
      runtime.delegate,
      runtime.driver_options,
      self.stream_options,
      None,
      #[cfg(encryption)]
      runtime.keyring,
    );
    driver.await;
  }
}
