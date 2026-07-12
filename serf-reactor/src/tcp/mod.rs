//! TCP-backed serf driver over the agnostic runtime.
//!
//! [`TcpTransport`] owns the bound UDP gossip socket and TCP reliable listener.
//! The machine-layer `serf_proto::StreamEndpoint<I, SocketAddr, RawRecords, …>` is
//! built inside [`TcpTransport::run`] from the stored stream knobs and the serf
//! `Options` carried by the [`TransportRuntime`](crate::TransportRuntime). This is
//! the `Send`/`agnostic` sibling of serf-compio's `!Send`, compio-bound
//! `TcpTransport`.

#![cfg(feature = "tcp")]

use core::{num::NonZeroU8, time::Duration};
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
/// address, and the stream-transport tuning knobs.
///
/// Serf exposes no memberlist cluster label: neither this block nor the serf
/// `Options` carries one, so both the reliable and gossip planes run unlabeled (the
/// coordinator built in `Transport::run` passes `None`). Plain TCP has no
/// transport-layer peer authentication at all — there is no TLS trust anchor — so
/// the reliable-plane cluster boundary is the encryption keyring (a shared symmetric
/// secret that on plain TCP AEAD-protects both planes) plus network policy; an
/// unencrypted plain-TCP cluster is segregated by network reachability alone.
/// memberlist-reactor's lower-level options DO surface a label; serf, layered on
/// top, does not.
pub struct TcpTransportOptions<I = SmolStr, A = HostAddr<SmolStr>> {
  local_id: Option<I>,
  advertise_addr: Option<MaybeResolved<A, SocketAddr>>,
  stream: StreamTransportOptions,
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
  /// Reclaim window for a same-name member returning at a NEW address: a
  /// dead member older than this is revived in place of a conflict. See
  /// [`with_dead_node_reclaim_time`](Self::with_dead_node_reclaim_time).
  dead_node_reclaim_time: Option<Duration>,
  /// SWIM suspicion max-timeout multiplier override. `None` keeps the coordinator
  /// default. See [`with_suspicion_max_timeout_mult`](Self::with_suspicion_max_timeout_mult).
  suspicion_max_timeout_mult: Option<u32>,
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

  /// Builder: stream-transport tuning knobs.
  #[must_use]
  #[inline]
  pub fn with_stream(mut self, opts: StreamTransportOptions) -> Self {
    self.stream = opts;
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
  /// implementation's dead-node reclaim. Left unset (the default), a
  /// same-name Alive from a different address is a name conflict, never a
  /// revival.
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
  /// Push/pull interval override, applied to the coordinator's `EndpointOptions` in
  /// [`Transport::run`]. `None` keeps the default; `Some(Duration::ZERO)` disables
  /// periodic anti-entropy.
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

  async fn run<D, G>(self, runtime: TransportRuntime<Self::Id, D>, gossip_rng: G)
  where
    D: Delegate<Id = Self::Id, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    // `Serf::new` is generic over `T` and cannot build the record-layer-specific
    // endpoint; build it here from `self`'s stored config. Serf ranks its user
    // broadcasts on three tiers (intent / event / query → ranks 0 / 1 / 2), so the
    // inner memberlist endpoint needs at least three broadcast tiers.
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
    // Snapshot the reliable push/pull exchange timeout from the SAME options the
    // coordinator is built from, so the driver reconciles an await-result join's
    // caller deadline against the exact deadline the coordinator will stamp.
    let stream_timeout = inner_opts.stream_timeout();
    let inner = Endpoint::new(inner_opts, gossip_rng);
    // Plain TCP has no SNI (`|_| None`) and a membership address that IS the
    // transport socket (`|addr| *addr`). Serf threads no memberlist cluster label —
    // there is none in its options — so the reliable record layer runs unlabeled
    // (`None`). Plain TCP has no transport-layer peer auth, so the reliable-plane
    // boundary is the encryption keyring (or, absent one, network policy alone).
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
    let rejoin_after_leave;
    let endpoint = serf_proto::StreamEndpoint::<
      Self::Id,
      SocketAddr,
      RawRecords,
      G,
      StdRng,
      crate::drop_counter::ReactorDropCounter,
    >::new_with_rng_in(
      coord,
      {
        rejoin_after_leave = runtime.serf_options.rejoin_after_leave();
        runtime.serf_options
      },
      self.serf_rng,
      runtime.user_drop,
      runtime.member_drop,
    )
    .with_reconnect_delegate(runtime.reconnect_delegate);
    let mut endpoint = endpoint;
    if let Some(md) = runtime.merge_delegate {
      endpoint.set_merge_delegate(md);
    }
    let snapshotter = match runtime.snapshot {
      Some((writer, records)) => {
        let replay = serf_proto::snapshot::ReplayResult::replay(records, rejoin_after_leave);
        // Ignoring Err: load_snapshot refuses only on a machine that already
        // lost an id-conflict vote; a freshly built endpoint is Alive.
        let _ = endpoint.load_snapshot(replay, memberlist_proto::Instant::now());
        Some(writer)
      }
      None => None,
    };

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
      stream_timeout,
      snapshotter,
      #[cfg(encryption)]
      runtime.keyring,
    );
    driver.await;
  }
}
