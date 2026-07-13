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

use core::{num::NonZeroU8, time::Duration};
use std::{io::ErrorKind, net::SocketAddr};

use compio::net::UdpSocket;
use hostaddr::HostAddr;
use memberlist_proto::{
  CheapClone, EndpointOptions, Id, MaybeResolved, QuicEndpoint as Coordinator,
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
  /// Override for the memberlist anti-entropy push/pull interval. `None` keeps the
  /// coordinator default; `Some(Duration::ZERO)` disables periodic push/pull
  /// entirely. See [`with_push_pull_interval`](Self::with_push_pull_interval).
  push_pull_interval: Option<Duration>,
  /// SWIM probe interval override. `None` keeps the coordinator default. See
  /// [`with_probe_interval`](Self::with_probe_interval).
  probe_interval: Option<Duration>,
  /// SWIM direct-ping timeout override. `None` keeps the coordinator default. See
  /// [`with_probe_timeout`](Self::with_probe_timeout).
  probe_timeout: Option<Duration>,
  /// Gossip interval override. `None` keeps the coordinator default. See
  /// [`with_gossip_interval`](Self::with_gossip_interval).
  gossip_interval: Option<Duration>,
  /// SWIM suspicion multiplier override. `None` keeps the coordinator default. See
  /// [`with_suspicion_mult`](Self::with_suspicion_mult).
  suspicion_mult: Option<u32>,
  /// Reclaim window for a same-name member returning at a NEW address: a dead
  /// member older than this is revived in place of a conflict. See
  /// [`with_dead_node_reclaim_time`](Self::with_dead_node_reclaim_time).
  dead_node_reclaim_time: Option<Duration>,
  /// SWIM suspicion max-timeout multiplier override. `None` keeps the coordinator
  /// default. See [`with_suspicion_max_timeout_mult`](Self::with_suspicion_max_timeout_mult).
  suspicion_max_timeout_mult: Option<u32>,
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
      push_pull_interval: None,
      probe_interval: None,
      probe_timeout: None,
      gossip_interval: None,
      suspicion_mult: None,
      dead_node_reclaim_time: None,
      suspicion_max_timeout_mult: None,
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

  /// Builder: override the memberlist anti-entropy push/pull interval.
  ///
  /// `None` (the default) keeps the coordinator's built-in interval. A positive
  /// duration re-tunes the periodic full-state sync; `Duration::ZERO` disables
  /// periodic push/pull entirely — join-time and explicit exchanges still run, but
  /// no background anti-entropy is scheduled.
  #[must_use]
  #[inline]
  pub const fn with_push_pull_interval(mut self, interval: Duration) -> Self {
    self.push_pull_interval = Some(interval);
    self
  }

  /// Builder: override the memberlist SWIM probe interval — how often the
  /// coordinator probes a random peer for liveness.
  ///
  /// `None` (the default) keeps the coordinator default (~1s). A shorter interval
  /// speeds failure detection at the cost of more probe traffic; it also shortens
  /// the suspicion timeout, which scales with the probe interval.
  #[must_use]
  #[inline]
  pub const fn with_probe_interval(mut self, interval: Duration) -> Self {
    self.probe_interval = Some(interval);
    self
  }

  /// Builder: override the memberlist SWIM direct-ping timeout — how long the
  /// coordinator waits for a probe ack before escalating to indirect probes.
  ///
  /// `None` (the default) keeps the coordinator default (~500ms). It must
  /// comfortably exceed the real network round-trip, or a live peer whose ack is
  /// merely slow is falsely suspected.
  #[must_use]
  #[inline]
  pub const fn with_probe_timeout(mut self, timeout: Duration) -> Self {
    self.probe_timeout = Some(timeout);
    self
  }

  /// Builder: override the memberlist gossip interval — how often the coordinator
  /// flushes queued gossip to a random subset of peers.
  ///
  /// `None` (the default) keeps the coordinator default (~200ms).
  #[must_use]
  #[inline]
  pub const fn with_gossip_interval(mut self, interval: Duration) -> Self {
    self.gossip_interval = Some(interval);
    self
  }

  /// Builder: override the memberlist SWIM suspicion multiplier — how long a
  /// suspected peer is held in the Suspect state before being declared Failed.
  ///
  /// The minimum suspicion timeout is `suspicion_mult * log10(N+1) * probe_interval`.
  /// `None` (the default) keeps the coordinator default.
  #[must_use]
  #[inline]
  pub const fn with_suspicion_mult(mut self, mult: u32) -> Self {
    self.suspicion_mult = Some(mult);
    self
  }

  /// Builder: allow a dead member to be revived under the SAME id at a NEW
  /// address once it has been dead longer than `window` — the reference
  /// implementation's dead-node reclaim. Left unset (the default), a same-name
  /// Alive from a different address is a name conflict, never a revival.
  #[must_use]
  #[inline]
  pub const fn with_dead_node_reclaim_time(mut self, window: Duration) -> Self {
    self.dead_node_reclaim_time = Some(window);
    self
  }

  /// Builder: override the memberlist SWIM suspicion max-timeout multiplier — the
  /// upper bound on the suspicion timeout as a multiple of the minimum.
  ///
  /// `None` (the default) keeps the coordinator default.
  #[must_use]
  #[inline]
  pub const fn with_suspicion_max_timeout_mult(mut self, mult: u32) -> Self {
    self.suspicion_max_timeout_mult = Some(mult);
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

  /// The push/pull interval override, if set.
  #[inline]
  pub const fn push_pull_interval(&self) -> Option<Duration> {
    self.push_pull_interval
  }

  /// The SWIM probe-interval override, if set.
  #[inline]
  pub const fn probe_interval(&self) -> Option<Duration> {
    self.probe_interval
  }

  /// The SWIM probe-timeout override, if set.
  #[inline]
  pub const fn probe_timeout(&self) -> Option<Duration> {
    self.probe_timeout
  }

  /// The gossip-interval override, if set.
  #[inline]
  pub const fn gossip_interval(&self) -> Option<Duration> {
    self.gossip_interval
  }

  /// The SWIM suspicion-multiplier override, if set.
  #[inline]
  pub const fn suspicion_mult(&self) -> Option<u32> {
    self.suspicion_mult
  }

  /// The configured dead-node reclaim window, if overridden.
  #[must_use]
  #[inline]
  pub const fn dead_node_reclaim_time(&self) -> Option<Duration> {
    self.dead_node_reclaim_time
  }

  /// The SWIM suspicion max-timeout-multiplier override, if set.
  #[inline]
  pub const fn suspicion_max_timeout_mult(&self) -> Option<u32> {
    self.suspicion_max_timeout_mult
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
  /// Push/pull interval override, applied to the coordinator's `EndpointOptions`
  /// in [`Transport::run`]. `None` keeps the default; `Some(Duration::ZERO)`
  /// disables periodic anti-entropy.
  push_pull_interval: Option<Duration>,
  /// SWIM failure-detection overrides applied to the coordinator's
  /// `EndpointOptions` in [`Transport::run`]. Each `None` keeps the coordinator
  /// default.
  probe_interval: Option<Duration>,
  probe_timeout: Option<Duration>,
  gossip_interval: Option<Duration>,
  suspicion_mult: Option<u32>,
  dead_node_reclaim_time: Option<Duration>,
  suspicion_max_timeout_mult: Option<u32>,
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
  A: Clone + Send + 'static,
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
      push_pull_interval: options.push_pull_interval,
      probe_interval: options.probe_interval,
      probe_timeout: options.probe_timeout,
      gossip_interval: options.gossip_interval,
      suspicion_mult: options.suspicion_mult,
      dead_node_reclaim_time: options.dead_node_reclaim_time,
      suspicion_max_timeout_mult: options.suspicion_max_timeout_mult,
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
    let mut inner_opts = EndpointOptions::new(self.local_id, self.advertise_socket)
      .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
    // A caller-supplied push/pull interval re-tunes (or, at `Duration::ZERO`,
    // disables) the periodic anti-entropy full-state sync. Left unset, the
    // coordinator keeps its own default.
    if let Some(interval) = self.push_pull_interval {
      inner_opts = inner_opts.with_push_pull_interval(interval);
    }
    // Caller-supplied SWIM failure-detection overrides: each left unset keeps the
    // coordinator's own default. Lowering these speeds up failure detection (probe
    // cadence, ack timeout, gossip cadence, and the suspicion timeout that scales
    // with the probe interval).
    if let Some(v) = self.probe_interval {
      inner_opts = inner_opts.with_probe_interval(v);
    }
    if let Some(v) = self.probe_timeout {
      inner_opts = inner_opts.with_probe_timeout(v);
    }
    if let Some(v) = self.gossip_interval {
      inner_opts = inner_opts.with_gossip_interval(v);
    }
    if let Some(v) = self.suspicion_mult {
      inner_opts = inner_opts.with_suspicion_mult(v);
    }
    if let Some(v) = self.dead_node_reclaim_time {
      inner_opts = inner_opts.with_dead_node_reclaim_time(v);
    }
    if let Some(v) = self.suspicion_max_timeout_mult {
      inner_opts = inner_opts.with_suspicion_max_timeout_mult(v);
    }
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
    let rejoin_after_leave = runtime.serf_options.rejoin_after_leave();
    let mut endpoint = serf_proto::QuicEndpoint::<
      Self::Id,
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

    crate::driver::quic::quic_driver_loop::<Self::Id, D, G, StdRng>(
      endpoint,
      self.gossip_socket,
      quic_max_udp_payload,
      runtime.commands_rx,
      runtime.events_tx,
      runtime.events_dropped,
      runtime.observation_dropped,
      runtime.datagrams_sent,
      runtime.snapshot,
      runtime.shutdown_flag,
      runtime.shutdown_complete,
      runtime.driver_options,
      runtime.delegate,
      None,
      snapshotter,
      #[cfg(encryption)]
      runtime.keyring,
    )
    .await;
  }
}

#[cfg(all(test, feature = "quic"))]
mod tests;
