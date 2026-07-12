//! The [`Serf`] handle — construction, lifecycle, command dispatch, and lock-free
//! reads.
//!
//! [`Serf::new`] binds a [`Transport`], builds the [`Shared`] state + events
//! channel, and spawns the driver pump on the agnostic runtime `R`. Every command
//! method pushes one [`Command`] onto the shared queue and (for the awaited kinds)
//! parks on a one-shot reply the driver resolves. `Clone` / `Drop` reference-count
//! the shared driver; membership reads go through the lock-free [`SerfSnapshot`]
//! and the snapshot read-forwarders (`members` / `local_member` / `state` /
//! `advertise_node` / `default_query_*`). The ergonomic per-backend constructor
//! [`Serf::tcp`] instantiates the transport for the caller; `tls` / `quic` slot in
//! the same way once those backends land.

use core::{marker::PhantomData, time::Duration};
use std::{net::SocketAddr, sync::Arc};

use agnostic::Runtime;
use bytes::Bytes;
use futures_channel::oneshot;
use memberlist_proto::{Instant, Node};
use serf_proto::{
  LamportTime,
  endpoint::{QueryId, QueryParams},
  event::{Event, QueryEvent},
  members::{Member, MemberStatus, SerfState},
  options::Options as SerfOptions,
  typed::Tags,
};
use smallvec::SmallVec;
use smol_str::SmolStr;

#[cfg(feature = "coordinates")]
use crate::command::CachedCoordinateCmd;
#[cfg(encryption)]
use crate::command::{KeyCmd, ListKeysCmd};
#[cfg(encryption)]
use crate::delegate::KeyringDelegate;
#[cfg(feature = "quic")]
use crate::quic::{QuicTransport, QuicTransportOptions};
#[cfg(feature = "tcp")]
use crate::tcp::{TcpTransport, TcpTransportOptions};
#[cfg(feature = "tls")]
use crate::tls::{TlsTransport, TlsTransportOptions};
use crate::{
  MaybeResolved,
  command::{
    Command, ForceLeaveCmd, JoinCmd, JoinKind, JoinReply, LeaveCmd, QueryCmd, RespondCmd,
    SetTagsCmd, ShutdownCmd, UserEventCmd, WaitForCompletionArgs,
  },
  delegate::Delegate,
  driver::options::RuntimeOptions,
  error::{InvalidOption, JoinFailed, Result, SerfError},
  events::EventStream,
  resolver::{AdvertiseAddrResolver, Resolver},
  shared::Shared,
  snapshot::SerfSnapshot,
  transport::{Transport, TransportRuntime},
};
#[cfg(encryption)]
use memberlist_proto::SecretKey;
#[cfg(any(feature = "tcp", feature = "quic"))]
use memberlist_proto::{CheapClone, Data};

/// The initial published snapshot: the local node, `Alive`, with empty tags and
/// zeroed Lamport clocks. Superseded by the driver's first real republish.
fn initial_snapshot<I>(local_id: &I, advertise: SocketAddr) -> SerfSnapshot<I, SocketAddr>
where
  I: Clone + PartialEq,
{
  let member = Member::new(
    Node::new(local_id.clone(), advertise),
    Tags::new(),
    MemberStatus::Alive,
  );
  SerfSnapshot::new(
    vec![Arc::new(member)],
    local_id,
    SerfState::Alive,
    LamportTime::from(0u64),
    LamportTime::from(0u64),
    LamportTime::from(0u64),
  )
}

/// A handle to a running serf node.
///
/// Cheap to clone; every clone shares the one backend driver, which runs until the
/// last handle is dropped (or [`shutdown`](Serf::shutdown) is called). Membership
/// reads are lock-free via the published [`SerfSnapshot`].
///
/// `Serf<I, A, R>` carries the wire id type `I`, the resolver's unresolved address
/// type `A`, and the agnostic runtime `R` its driver was spawned on. `I` flows
/// into the snapshot and events channel (both `<I, SocketAddr>`); `A` ties `join`'s
/// seeds to the address domain the node was built with; `R` brands the handle so a
/// tokio-backed node is a distinct type from a smol-backed one.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct Serf<I, A, R> {
  shared: Arc<Shared<I>>,
  events_rx: flume::Receiver<Event<I, SocketAddr>>,
  /// Per-call await-result join deadline offset, cached from the runtime options
  /// so each `join` can stamp its absolute `WaitForCompletion` deadline.
  join_deadline: Duration,
  /// Cached `query_timeout_mult` from the serf options, so
  /// [`default_query_timeout`](Serf::default_query_timeout) derives the query
  /// timeout from the live snapshot member count without a driver round-trip.
  query_timeout_mult: usize,
  /// Ties the handle to the resolver's unresolved address type. Not held in any
  /// field — `join` enforces seeds resolve in this address domain.
  _a: PhantomData<fn(A)>,
  /// Brands the handle with the agnostic runtime its driver was spawned on. Not
  /// held in any field — the driver task is spawned detached.
  _r: PhantomData<fn(R)>,
}

impl<I, A, R> Clone for Serf<I, A, R> {
  fn clone(&self) -> Self {
    self.shared.handle_cloned();
    Self {
      shared: self.shared.clone(),
      events_rx: self.events_rx.clone(),
      join_deadline: self.join_deadline,
      query_timeout_mult: self.query_timeout_mult,
      _a: PhantomData,
      _r: PhantomData,
    }
  }
}

impl<I, A, R> Drop for Serf<I, A, R> {
  fn drop(&mut self) {
    if self.shared.handle_dropped() {
      self.shared.begin_shutdown();
      self.shared.wake_driver();
    }
  }
}

impl<I, A, R> Serf<I, A, R>
where
  I: memberlist_proto::Id + Clone + Send + Sync + Unpin + 'static,
  R: Runtime,
{
  /// Build a node: construct the transport `T`, build the shared state + events
  /// channel, and spawn its driver pump on the runtime `R`. Every clone shares the
  /// one driver.
  ///
  /// `resolver` / `advertise_resolver` resolve the advertise address at
  /// construction; they are not retained. `gossip_rng` seeds the memberlist gossip
  /// schedule (draw it via [`gossip_rng`](crate::gossip_rng)). Under an encryption
  /// backend, pass an [`Arc<dyn KeyringDelegate>`](crate::KeyringDelegate)
  /// (`Arc::new(VoidKeyringDelegate)` for a node that manages no keys).
  #[allow(clippy::too_many_arguments)]
  pub async fn new<T, RES, AR, D, G>(
    options: T::Options,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    gossip_rng: G,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> core::result::Result<Self, T::Error>
  where
    T: Transport<R, Id = I>,
    RES: Resolver<Address = T::Address>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
    T::Error: From<SerfError>,
  {
    // Reject runtime knobs a zero capacity would deterministically break BEFORE
    // binding any socket or spawning the detached driver.
    runtime_options.validate().map_err(T::Error::from)?;
    // Reject a serf-level configuration a driver cannot honor (an over-ceiling
    // `max_user_event_size`, or a self-contradictory coalescing pair) at the same
    // early stage, before any socket is bound or the detached driver is spawned.
    serf_options
      .validate()
      .map_err(|e| SerfError::InvalidOption(InvalidOption::new("serf_options", e.to_string())))
      .map_err(T::Error::from)?;
    // Cache the join deadline before `runtime_options` moves into the bundle.
    let join_deadline = runtime_options.join_deadline();
    // Cache `query_timeout_mult` before `serf_options` moves into the bundle, so
    // the handle can compute `default_query_timeout` without a driver round-trip.
    let query_timeout_mult = serf_options.query_timeout_mult();

    let transport = T::new(options, resolver, advertise_resolver).await?;
    let local_id = transport.local_id().clone();
    let advertise = *transport.advertise_address();

    let (events_tx, events_rx) =
      flume::bounded::<Event<I, SocketAddr>>(runtime_options.event_queue_cap());
    // Mint the two shed counters as (writer, reader) pairs: the driver injects the
    // writers into the endpoint, the handle reads the readers, both over the same
    // backing atomic so no publish step exists.
    let (user_drop_writer, user_drop_reader) = crate::drop_counter::drop_channel();
    let (member_drop_writer, member_drop_reader) = crate::drop_counter::drop_channel();
    let shared = Arc::new(Shared::new(
      initial_snapshot(&local_id, advertise),
      user_drop_reader,
      member_drop_reader,
    ));

    let runtime = TransportRuntime::<I, D>::new(
      delegate,
      shared.clone(),
      events_tx,
      runtime_options,
      serf_options,
      user_drop_writer,
      member_drop_writer,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    );

    // The driver pump owns the transport, the bound sockets, and the endpoint; it
    // runs detached until a `Shutdown` command or all handles drop.
    R::spawn_detach(transport.run(runtime, gossip_rng));

    Ok(Self {
      shared,
      events_rx,
      join_deadline,
      query_timeout_mult,
      _a: PhantomData,
      _r: PhantomData,
    })
  }
}

// Ergonomic per-backend constructors: instantiate the transport for the caller so
// a node can be built without naming the generic `Serf::new::<T, …>` machinery.
// `tls` / `quic` slot in the same way once those backends land.
#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
impl<I, A, R> Serf<I, A, R>
where
  I: memberlist_proto::Id
    + CheapClone
    + Clone
    + core::fmt::Debug
    + core::fmt::Display
    + Send
    + Sync
    + Unpin
    + 'static,
  A: Data + Clone + Send + Sync + 'static,
  R: Runtime,
{
  /// Build a TCP-backed serf node and spawn its driver on the runtime `R`.
  ///
  /// The ergonomic wrapper over [`Serf::new`] that instantiates the
  /// [`TcpTransport`](crate::TcpTransport) for the caller: it binds a UDP gossip
  /// socket and a TCP reliable listener on the advertise address (resolved once
  /// via `resolver` / `advertise_resolver`), then spawns the stream driver. The
  /// gossip RNG is drawn from OS entropy via [`gossip_rng`](crate::gossip_rng);
  /// use [`tcp_with_rng`](Self::tcp_with_rng) to supply your own.
  ///
  /// Under an encryption backend, pass an
  /// [`Arc<dyn KeyringDelegate>`](crate::KeyringDelegate)
  /// (`Arc::new(VoidKeyringDelegate)` for a node that manages no keys).
  #[allow(clippy::too_many_arguments)]
  pub async fn tcp<RES, AR, D>(
    options: TcpTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
  {
    Self::tcp_with_rng(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      crate::gossip_rng()?,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }

  /// Like [`tcp`](Self::tcp) but with a caller-supplied gossip RNG `G` — draw it
  /// via [`gossip_rng`](crate::gossip_rng) for fork-safe OS entropy.
  #[allow(clippy::too_many_arguments)]
  pub async fn tcp_with_rng<RES, AR, D, G>(
    options: TcpTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    gossip_rng: G,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    Self::new::<TcpTransport<I, A, R>, RES, AR, D, G>(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      gossip_rng,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }
}

// Ergonomic TLS constructor: instantiate the TLS transport for the caller so a node
// can be built without naming the generic `Serf::new::<T, …>` machinery. TLS rides
// the same stream driver as plain TCP, differing only in the record layer.
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
impl<I, A, R> Serf<I, A, R>
where
  I: memberlist_proto::Id
    + CheapClone
    + Clone
    + core::fmt::Debug
    + core::fmt::Display
    + Send
    + Sync
    + Unpin
    + 'static,
  A: Data + Clone + Send + Sync + 'static,
  R: Runtime,
{
  /// Build a TLS-backed serf node and spawn its driver on the runtime `R`.
  ///
  /// The ergonomic wrapper over [`Serf::new`] that instantiates the
  /// [`TlsTransport`](crate::TlsTransport) for the caller: it binds a UDP gossip
  /// socket and a TCP reliable listener on the advertise address (resolved once via
  /// `resolver` / `advertise_resolver`), then spawns the stream driver whose
  /// reliable record layer drives rustls over the plain agnostic TCP stream. The
  /// caller supplies the rustls server/client bundle and the per-peer SNI provider
  /// through [`TlsTransportOptions`](crate::TlsTransportOptions). The gossip RNG is
  /// drawn from OS entropy via [`gossip_rng`](crate::gossip_rng); use
  /// [`tls_with_rng`](Self::tls_with_rng) to supply your own.
  ///
  /// Under an encryption backend, pass an
  /// [`Arc<dyn KeyringDelegate>`](crate::KeyringDelegate)
  /// (`Arc::new(VoidKeyringDelegate)` for a node that manages no keys); the keyring
  /// AEAD-protects the gossip datagrams (the reliable plane rides the TLS session).
  #[allow(clippy::too_many_arguments)]
  pub async fn tls<RES, AR, D>(
    options: TlsTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
  {
    Self::tls_with_rng(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      crate::gossip_rng()?,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }

  /// Like [`tls`](Self::tls) but with a caller-supplied gossip RNG `G` — draw it via
  /// [`gossip_rng`](crate::gossip_rng) for fork-safe OS entropy.
  #[allow(clippy::too_many_arguments)]
  pub async fn tls_with_rng<RES, AR, D, G>(
    options: TlsTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    gossip_rng: G,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    Self::new::<TlsTransport<I, A, R>, RES, AR, D, G>(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      gossip_rng,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }
}

// Ergonomic QUIC constructor: instantiate the QUIC transport for the caller so a
// node can be built without naming the generic `Serf::new::<T, …>` machinery.
#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
impl<I, A, R> Serf<I, A, R>
where
  I: memberlist_proto::Id
    + CheapClone
    + Clone
    + core::fmt::Debug
    + core::fmt::Display
    + Send
    + Sync
    + Unpin
    + 'static,
  A: Data + Clone + Send + Sync + 'static,
  R: Runtime,
{
  /// Build a QUIC-backed serf node and spawn its driver on the runtime `R`.
  ///
  /// The ergonomic wrapper over [`Serf::new`] that instantiates the
  /// [`QuicTransport`](crate::QuicTransport) for the caller: it binds a single UDP
  /// socket on the advertise address (resolved once via `resolver` /
  /// `advertise_resolver`) over which the coordinator multiplexes the reliable
  /// push/pull streams and serf's datagram gossip, then spawns the QUIC driver. The
  /// caller supplies the quinn-proto config bundle through
  /// [`QuicTransportOptions::with_quic_config`](crate::QuicTransportOptions::with_quic_config).
  /// The gossip RNG is drawn from OS entropy via [`gossip_rng`](crate::gossip_rng);
  /// use [`quic_with_rng`](Self::quic_with_rng) to supply your own.
  ///
  /// Under an encryption backend, pass an
  /// [`Arc<dyn KeyringDelegate>`](crate::KeyringDelegate)
  /// (`Arc::new(VoidKeyringDelegate)` for a node that manages no keys); the keyring
  /// AEAD-protects the gossip datagrams (the reliable plane rides quinn's own TLS).
  #[allow(clippy::too_many_arguments)]
  pub async fn quic<RES, AR, D>(
    options: QuicTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
  {
    Self::quic_with_rng(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      crate::gossip_rng()?,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }

  /// Like [`quic`](Self::quic) but with a caller-supplied gossip RNG `G` — draw it
  /// via [`gossip_rng`](crate::gossip_rng) for fork-safe OS entropy.
  #[allow(clippy::too_many_arguments)]
  pub async fn quic_with_rng<RES, AR, D, G>(
    options: QuicTransportOptions<I, A>,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    gossip_rng: G,
    reconnect_delegate: Option<Box<dyn serf_proto::ReconnectDelegate<I, SocketAddr>>>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Result<Self>
  where
    RES: Resolver<Address = A>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr>,
    G: rand::Rng + Send + Unpin + 'static,
  {
    Self::new::<QuicTransport<I, A, R>, RES, AR, D, G>(
      options,
      resolver,
      advertise_resolver,
      delegate,
      runtime_options,
      serf_options,
      gossip_rng,
      reconnect_delegate,
      #[cfg(encryption)]
      keyring,
    )
    .await
  }
}

// Handle operations that read a cached snapshot or push a command over the queue —
// none touch node identity directly, so they impose no bound and stay callable on
// a `Serf` of any id type.
impl<I, A, R> Serf<I, A, R> {
  /// The latest membership snapshot, read lock-free.
  #[must_use]
  pub fn snapshot(&self) -> Arc<SerfSnapshot<I, SocketAddr>> {
    self.shared.load_snapshot()
  }

  /// The number of known members in the latest published snapshot.
  #[must_use]
  pub fn num_members(&self) -> usize {
    self.shared.load_snapshot().num_members()
  }

  /// All known cluster members at the latest published snapshot instant — alive,
  /// leaving, left, and failed within the reap window. Mirrors serf-compio's
  /// `Serf::members`.
  #[must_use]
  pub fn members(&self) -> Vec<Arc<Member<I, SocketAddr>>> {
    self.snapshot().members().to_vec()
  }

  /// The local node's full membership view at the latest published snapshot
  /// instant. Returns the same `Arc` that lives at the local index in
  /// [`members`](Self::members), so it is always consistent with that view.
  /// Mirrors serf-compio's `Serf::local_member`.
  #[must_use]
  pub fn local_member(&self) -> Arc<Member<I, SocketAddr>> {
    self.snapshot().local()
  }

  /// The lifecycle state of the local serf endpoint, derived from the latest
  /// published snapshot. Mirrors serf-compio's `Serf::state`.
  #[must_use]
  pub fn state(&self) -> SerfState {
    self.snapshot().state()
  }

  /// The bound advertise address this node gossips to peers, read from the local
  /// member of the latest published snapshot. Mirrors serf-compio's
  /// `Serf::advertise_address`.
  #[must_use]
  pub fn advertise_address(&self) -> SocketAddr {
    *self.snapshot().local_ref().node().addr_ref()
  }

  /// The local node's id, read from the local member of the latest published
  /// snapshot. Mirrors serf-compio's `Serf::local_id` (returned owned here, as the
  /// snapshot is loaded by value).
  #[must_use]
  pub fn local_id(&self) -> I
  where
    I: Clone,
  {
    self.snapshot().local_ref().node().id_ref().clone()
  }

  /// The local node as an `(id, advertise-address)` [`Node`], composed from the
  /// local member of the latest published snapshot. Mirrors serf-compio's
  /// `Serf::advertise_node`.
  #[must_use]
  pub fn advertise_node(&self) -> Node<I, SocketAddr>
  where
    I: Clone,
  {
    self.snapshot().local().node().clone()
  }

  /// Default query timeout derived from the current snapshot member count.
  ///
  /// Computed as `200ms × query_timeout_mult × ⌈log₁₀(N+1)⌉` where N is the
  /// snapshot member count, matching the machine's own zero-timeout resolution.
  /// Mirrors serf-compio's `Serf::default_query_timeout`.
  #[must_use]
  pub fn default_query_timeout(&self) -> Duration {
    let n = self.num_members();
    let log_factor = ((n as f64 + 1.0).log10().ceil() as u32).max(1);
    Duration::from_millis(200) * self.query_timeout_mult as u32 * log_factor
  }

  /// Default query parameters derived from the current snapshot: no filters, no
  /// relay, no ACK, and a timeout from
  /// [`default_query_timeout`](Self::default_query_timeout). Mirrors serf-compio's
  /// `Serf::default_query_param`.
  #[must_use]
  pub fn default_query_param(&self) -> QueryParams<I> {
    QueryParams {
      filters: Vec::new(),
      relay_factor: 0,
      request_ack: false,
      timeout: self.default_query_timeout(),
    }
  }

  /// Subscribe to the serf event stream (membership transitions, user events,
  /// queries, responses).
  #[must_use]
  pub fn events(&self) -> EventStream<I, SocketAddr>
  where
    I: 'static,
  {
    EventStream::new(self.events_rx.clone())
  }

  /// The cumulative count of events dropped at the event-stream fan-out (a slow
  /// subscriber); these are recoverable from the snapshot.
  #[must_use]
  pub fn events_dropped(&self) -> u64 {
    self.shared.events_dropped()
  }

  /// The cumulative count of events dropped at the observation channel (a slow
  /// delegate); these may include unrecoverable application data.
  #[must_use]
  pub fn observation_dropped(&self) -> u64 {
    self.shared.observation_dropped()
  }

  /// The cumulative count of coalescing user events the driver's endpoint shed
  /// because its user coalescer was at the configured buffered-volume cap
  /// (`Options::max_coalesced_user_events`). Lifetime total, saturating; always
  /// `0` when user coalescing is disabled.
  #[must_use]
  pub fn coalesced_user_events_dropped(&self) -> u64 {
    self.shared.coalesced_user_events_dropped()
  }

  /// The cumulative count of member changes the driver's endpoint shed because
  /// its member coalescer was at its per-window cardinality cap. Lifetime total,
  /// saturating; always `0` when member coalescing is disabled.
  #[must_use]
  pub fn coalesced_member_events_dropped(&self) -> u64 {
    self.shared.coalesced_member_events_dropped()
  }

  /// The cumulative count of gossip payloads sent over the QUIC datagram plane
  /// (a datagram queued onto the peer's pooled, TLS-protected connection) rather
  /// than the plain-UDP fallback. Always `0` on the stream transports and on a
  /// QUIC endpoint configured for `UnreliableTransport::Udp`.
  #[must_use]
  pub fn datagrams_sent(&self) -> u64 {
    self.shared.datagrams_sent()
  }

  /// The local node's current Vivaldi network coordinate, read lock-free from
  /// the latest published snapshot.
  ///
  /// `None` when coordinates are disabled
  /// (`Options::with_disable_coordinates(true)`) or before the driver's first
  /// snapshot publish. Coordinates converge as probe round-trips accumulate;
  /// estimate inter-node RTT by comparing two nodes' coordinates.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub fn coordinate(&self) -> Option<serf_proto::typed::Coordinate> {
    self.shared.load_snapshot().coordinate().cloned()
  }

  /// The most-recently-observed Vivaldi coordinate of the peer `id`, updated on
  /// each successful probe round-trip from that peer.
  ///
  /// Resolves `None` when coordinates are disabled or no RTT sample has been
  /// received from `id` yet.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub async fn cached_coordinate(&self, id: I) -> Result<Option<serf_proto::typed::Coordinate>> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::CachedCoordinate(CachedCoordinateCmd {
      id,
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Send `cmd` to the driver, failing fast if the node has shut down.
  fn send(&self, cmd: Command<I, SocketAddr>) -> Result<()> {
    if self.shared.is_shutdown() {
      return Err(SerfError::Shutdown);
    }
    if self.shared.push_command(cmd) {
      Ok(())
    } else {
      Err(SerfError::CommandSend)
    }
  }

  /// Join an existing cluster through a single seed, waiting for the seed to be
  /// contacted. Returns the resolved [`SocketAddr`] of the seed actually reached.
  ///
  /// When `ignore_old` is `true`, replay of the seed's pre-join user events is
  /// suppressed (the machine records the join exchange's `StreamId` as a one-shot
  /// ignore-join target consumed at that exchange's own merge).
  pub async fn join<RES>(
    &self,
    resolver: &RES,
    node: MaybeResolved<RES::Address, SocketAddr>,
    ignore_old: bool,
  ) -> Result<SocketAddr>
  where
    RES: Resolver,
  {
    let addrs = resolve_seeds(resolver, core::slice::from_ref(&node)).await?;
    match self.join_await(addrs, 1, ignore_old).await {
      // The driver only replies `Ok` with a non-empty contacted set, so `next` is
      // the single reached address; the fallback can never fire.
      Ok(reached) => reached
        .into_iter()
        .next()
        .ok_or_else(|| SerfError::JoinAllFailed(JoinFailed::new(1, 0))),
      Err((_, e)) => Err(e),
    }
  }

  /// Join through several seeds, waiting for the join to complete. Returns the set
  /// of seed addresses actually contacted on success, or the legacy
  /// partial-success tuple `(reached_so_far, error)` on failure. An empty
  /// `existing` iterator is a trivial `Ok(empty)`.
  pub async fn join_many<RES>(
    &self,
    resolver: &RES,
    existing: impl Iterator<Item = MaybeResolved<RES::Address, SocketAddr>>,
    ignore_old: bool,
  ) -> core::result::Result<SmallVec<[SocketAddr; 1]>, (SmallVec<[SocketAddr; 1]>, SerfError)>
  where
    RES: Resolver,
  {
    let seeds: Vec<MaybeResolved<RES::Address, SocketAddr>> = existing.collect();
    if seeds.is_empty() {
      return Ok(SmallVec::new());
    }
    let requested = seeds.len();
    let addrs = match resolve_seeds(resolver, &seeds).await {
      Ok(a) => a,
      Err(e) => return Err((SmallVec::new(), e)),
    };
    self.join_await(addrs, requested, ignore_old).await
  }

  /// Fire-and-forget join: resolve `seeds`, dispatch a push/pull against each, and
  /// return the dispatched-exchange count immediately without waiting for contact.
  /// Actual membership surfaces through [`events`](Self::events) and
  /// [`snapshot`](Self::snapshot).
  pub async fn dispatch_join<RES>(
    &self,
    resolver: &RES,
    seeds: &[MaybeResolved<RES::Address, SocketAddr>],
  ) -> Result<usize>
  where
    RES: Resolver,
  {
    let addrs = resolve_seeds(resolver, seeds).await?;
    let (tx, rx) = oneshot::channel();
    self.send(Command::Join(JoinCmd {
      seeds: addrs,
      kind: JoinKind::Dispatch,
      // Fire-and-forget joins never ignore old events.
      ignore_old: false,
      reply: tx,
    }))?;
    match await_join_reply(rx).await {
      Ok(dispatched) => Ok(dispatched.len()),
      Err((_, e)) => Err(e),
    }
  }

  /// Drive an await-result join over already-resolved `addrs`, threading
  /// `ignore_old` to each seed's join push/pull. `requested_if_empty` is the
  /// `JoinAllFailed` denominator used only when `addrs` is empty.
  async fn join_await(
    &self,
    addrs: Vec<SocketAddr>,
    requested_if_empty: usize,
    ignore_old: bool,
  ) -> JoinReply {
    // A non-empty seed input that resolved to zero addresses is NOT a silent
    // success — surface `JoinAllFailed` so a bootstrap outage is never reported as
    // a healthy zero-contact join.
    if addrs.is_empty() {
      return Err((
        SmallVec::new(),
        SerfError::JoinAllFailed(JoinFailed::new(requested_if_empty, 0)),
      ));
    }
    let deadline = Instant::now() + self.join_deadline;
    let (tx, rx) = oneshot::channel();
    match self.send(Command::Join(JoinCmd {
      seeds: addrs,
      kind: JoinKind::WaitForCompletion(WaitForCompletionArgs { deadline }),
      ignore_old,
      reply: tx,
    })) {
      Ok(()) => await_join_reply(rx).await,
      Err(e) => Err((SmallVec::new(), e)),
    }
  }

  /// Gracefully leave the cluster. Resolves `Ok` once the departure fan-out has
  /// been handed to the transport (peers have been notified), or an error when
  /// the configured leave timeout elapses
  /// ([`LeaveTimeout`](SerfError::LeaveTimeout)) or the local socket failed
  /// while sending the fan-out
  /// ([`LeaveFarewellUndelivered`](SerfError::LeaveFarewellUndelivered)).
  pub async fn leave(&self) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::Leave(LeaveCmd { reply: tx }))?;
    await_reply(rx).await
  }

  /// Force-remove `id` from the membership. With `prune`, the node is removed
  /// immediately rather than after the tombstone timeout.
  pub async fn force_leave(&self, id: I, prune: bool) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::ForceLeave(ForceLeaveCmd {
      id,
      prune,
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Force-remove a failed node immediately without pruning the tombstone. Thin
  /// alias for `force_leave(id, false)`; serf stops attempting to reconnect.
  /// Mirrors serf-compio's `Serf::remove_failed_node`.
  pub async fn remove_failed_node(&self, id: I) -> Result<()> {
    self.force_leave(id, false).await
  }

  /// Force-remove a failed node immediately and prune the tombstone. Thin alias
  /// for `force_leave(id, true)`; the node is removed at once rather than after
  /// the tombstone timeout. Mirrors serf-compio's `Serf::remove_failed_node_prune`.
  pub async fn remove_failed_node_prune(&self, id: I) -> Result<()> {
    self.force_leave(id, true).await
  }

  /// Broadcast a user-defined event cluster-wide.
  pub async fn user_event(
    &self,
    name: impl Into<SmolStr>,
    payload: Bytes,
    coalesce: bool,
  ) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::UserEvent(UserEventCmd::new(
      name.into(),
      payload,
      coalesce,
      tx,
    )))?;
    await_reply(rx).await
  }

  /// Issue a cluster-wide query; returns the [`QueryId`] identifying it.
  pub async fn query(
    &self,
    name: impl Into<SmolStr>,
    payload: Bytes,
    params: QueryParams<I>,
  ) -> Result<QueryId> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::Query(QueryCmd::new(
      name.into(),
      payload,
      params,
      Instant::now(),
      tx,
    )))?;
    await_reply(rx).await
  }

  /// Respond to an inbound query received via [`Event::Query`].
  pub async fn respond(&self, token: QueryEvent<I, SocketAddr>, payload: Bytes) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::Respond(RespondCmd::new(
      token,
      payload,
      Instant::now(),
      tx,
    )))?;
    await_reply(rx).await
  }

  /// Replace the local node's advertised tags.
  pub async fn set_tags(&self, tags: Tags) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::SetTags(SetTagsCmd { tags, reply: tx }))?;
    await_reply(rx).await
  }

  /// Issue a cluster-wide install-key query; returns the issued [`QueryId`].
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn install_key(&self, key: SecretKey) -> Result<QueryId> {
    self.install_key_with(key, 0).await
  }

  /// As [`install_key`](Self::install_key), with the responses relayed through
  /// `relay_factor` random intermediary nodes for delivery redundancy (`0` =
  /// direct-only, the plain form's behavior).
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn install_key_with(&self, key: SecretKey, relay_factor: u8) -> Result<QueryId> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::InstallKey(KeyCmd {
      key,
      relay_factor,
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Issue a cluster-wide use-key query to promote `key` to primary.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn use_key(&self, key: SecretKey) -> Result<QueryId> {
    self.use_key_with(key, 0).await
  }

  /// As [`use_key`](Self::use_key), with the responses relayed through
  /// `relay_factor` random intermediary nodes for delivery redundancy (`0` =
  /// direct-only, the plain form's behavior).
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn use_key_with(&self, key: SecretKey, relay_factor: u8) -> Result<QueryId> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::UseKey(KeyCmd {
      key,
      relay_factor,
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Issue a cluster-wide remove-key query.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn remove_key(&self, key: SecretKey) -> Result<QueryId> {
    self.remove_key_with(key, 0).await
  }

  /// As [`remove_key`](Self::remove_key), with the responses relayed through
  /// `relay_factor` random intermediary nodes for delivery redundancy (`0` =
  /// direct-only, the plain form's behavior).
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn remove_key_with(&self, key: SecretKey, relay_factor: u8) -> Result<QueryId> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::RemoveKey(KeyCmd {
      key,
      relay_factor,
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Issue a cluster-wide list-keys query.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn list_keys(&self) -> Result<QueryId> {
    self.list_keys_with(0).await
  }

  /// As [`list_keys`](Self::list_keys), with the responses relayed through
  /// `relay_factor` random intermediary nodes for delivery redundancy (`0` =
  /// direct-only, the plain form's behavior).
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub async fn list_keys_with(&self, relay_factor: u8) -> Result<QueryId> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::ListKeys(ListKeysCmd {
      relay_factor,
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Stop the driver and release its bound sockets, so an immediate rebind on the
  /// same address succeeds with no grace period. Returns once those sockets are
  /// released; it aborts in-flight reliable-stream exchanges but does not block on
  /// their connection cleanup.
  pub async fn shutdown(&self) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    if !self
      .shared
      .push_command(Command::Shutdown(ShutdownCmd { reply: tx }))
    {
      // The queue is already closed: a shutdown is in flight (or done). The driver
      // may still hold its bind sockets, so await teardown completion before
      // reporting success rather than returning into a still-bound port.
      self.shared.wait_shutdown_complete().await;
      return Ok(());
    }
    await_reply(rx).await
  }
}

/// Await a driver reply, mapping a dropped reply channel to
/// [`SerfError::ReplyClosed`].
async fn await_reply<O>(rx: oneshot::Receiver<Result<O>>) -> Result<O> {
  match rx.await {
    Ok(res) => res,
    Err(_) => Err(SerfError::ReplyClosed),
  }
}

/// Await a join reply, mapping a dropped reply channel to the empty-reached,
/// [`SerfError::ReplyClosed`] partial-success tuple.
async fn await_join_reply(rx: oneshot::Receiver<JoinReply>) -> JoinReply {
  match rx.await {
    Ok(res) => res,
    Err(_) => Err((SmallVec::new(), SerfError::ReplyClosed)),
  }
}

/// Resolve every `MaybeResolved` seed through `resolver` into concrete
/// [`SocketAddr`]s (an already-`Resolved` seed passes straight through).
async fn resolve_seeds<RES>(
  resolver: &RES,
  seeds: &[MaybeResolved<RES::Address, SocketAddr>],
) -> Result<Vec<SocketAddr>>
where
  RES: Resolver,
{
  let mut addrs: Vec<SocketAddr> = Vec::new();
  for seed in seeds {
    match seed {
      MaybeResolved::Resolved(s) => addrs.push(*s),
      MaybeResolved::Unresolved(a) => {
        let resolved = resolver
          .resolve(a)
          .await
          .map_err(|e| SerfError::Resolve(std::io::Error::other(e.to_string())))?;
        addrs.extend(resolved);
      }
    }
  }
  Ok(addrs)
}

#[cfg(all(test, feature = "tcp", feature = "tokio"))]
mod tests;
