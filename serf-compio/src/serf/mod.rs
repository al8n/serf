//! The public [`Serf`] handle — a cheaply-clonable façade over the driver task.
//!
//! Construction binds a [`Transport`], builds the [`TransportRuntime`] bundle,
//! and spawns the driver pump on the compio runtime. Every clone shares the same
//! driver task; user calls flow through the command channel and reads happen via
//! the lock-free [`SerfSnapshot`].
//!
//! This is the minimal handle surface the driver needs to be exercised
//! end-to-end (and to give every internal command a public constructor): it
//! wires one method per [`crate::command::Command`] variant. The richer
//! ergonomics — typed query/response futures, builder-style construction — are a
//! follow-up; the protocol surface here is complete.

use core::time::Duration;
use std::{
  cell::{Cell, RefCell},
  net::SocketAddr,
  rc::Rc,
  sync::Arc,
};

use bytes::Bytes;
use futures_channel::oneshot;
use memberlist_proto::{Instant, MaybeResolved, Node};
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

#[cfg(encryption)]
use crate::command::{KeyCmd, ListKeysCmd};
use crate::{
  command::{
    Command, ForceLeaveCmd, JoinCmd, JoinKind, JoinReply, LeaveCmd, QueryCmd, RespondCmd,
    SetTagsCmd, ShutdownCmd, UserEventCmd, WaitForCompletionArgs,
  },
  delegate::Delegate,
  driver::options::RuntimeOptions,
  error::{JoinFailed, Result, SerfError},
  events::EventStream,
  resolver::{AdvertiseAddrResolver, Resolver},
  snapshot::{SerfSnapshot, SnapshotCell},
  transport::{Transport, TransportRuntime},
};

#[cfg(encryption)]
use memberlist_proto::SecretKey;

#[cfg(encryption)]
use crate::delegate::KeyringDelegate;

/// Driver-shared state every [`Serf`] clone points at.
struct Shared<I> {
  commands_tx: flume::Sender<Command<I, SocketAddr>>,
  events_rx: flume::Receiver<Event<I, SocketAddr>>,
  /// Shares the same `Rc` the driver's observation task increments. Counts
  /// events dropped at the bounded user-facing channel when a slow consumer
  /// lets it fill. Monotonically increasing.
  events_dropped: Rc<Cell<u64>>,
  /// Shares the same `Rc` the driver pump increments. Counts events dropped
  /// at the bounded internal observation channel when the delegate dispatch
  /// loop falls behind. Monotonically increasing.
  observation_dropped: Rc<Cell<u64>>,
  /// Shares the same `Rc` the driver pump republishes each iteration with the
  /// endpoint's cumulative user-coalescer drop count. The driver owns the
  /// endpoint, so a handle reads the shed count here. Monotonically increasing.
  coalesced_user_events_dropped: Rc<Cell<u64>>,
  /// Shares the same `Rc` the driver pump republishes with the endpoint's
  /// cumulative member-coalescer drop count. Monotonically increasing.
  coalesced_member_events_dropped: Rc<Cell<u64>>,
  snapshot: SnapshotCell<I>,
  shutdown_flag: Rc<Cell<bool>>,
  local_id: I,
  advertise: SocketAddr,
  /// Per-call deadline applied to await-result joins, cached from
  /// [`RuntimeOptions::join_deadline`] so the handle can stamp each
  /// `WaitForCompletion` command's absolute deadline before sending it.
  join_deadline: Duration,
  /// Cached serf options for computing [`Serf::default_query_timeout`] without
  /// a driver round-trip. Cloned from the caller's options at construction
  /// before they are moved into the driver.
  serf_options: SerfOptions,
}

/// A cheaply-clonable handle to a running serf node.
///
/// Construct one with [`Serf::new`]; clone it freely — every clone shares the
/// single driver task. Requires a stream or QUIC transport feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct Serf<I> {
  shared: Rc<Shared<I>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I> Clone for Serf<I> {
  #[inline]
  fn clone(&self) -> Self {
    Self {
      shared: self.shared.clone(),
    }
  }
}

/// Synthesize the initial published snapshot — a single-member view of the local
/// node as `Alive` with empty tags and zero clocks. The driver republishes a
/// real snapshot once the local `NodeJoined` sieve fires.
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

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I> Serf<I>
where
  I: Clone + PartialEq + 'static,
{
  /// Build a node: construct the transport `T`, spawn its driver pump, and
  /// return a handle.
  ///
  /// `gossip_rng` seeds the memberlist gossip schedule and must be drawn from a
  /// fork-safe OS entropy source (see [`crate::gossip_rng`]).
  ///
  /// Under an encryption backend, `keyring` is the delegate the driver applies
  /// inbound key-management requests to; a node that does not manage keys can
  /// pass `std::rc::Rc::new(VoidKeyringDelegate)`.
  #[allow(clippy::too_many_arguments)]
  pub async fn new<T, RES, AR, D, G>(
    options: T::Options,
    resolver: &RES,
    advertise_resolver: &AR,
    delegate: D,
    runtime_options: RuntimeOptions,
    serf_options: SerfOptions,
    gossip_rng: G,
    #[cfg(encryption)] keyring: Rc<dyn KeyringDelegate>,
  ) -> core::result::Result<Self, T::Error>
  where
    T: Transport<Id = I>,
    RES: Resolver<Address = T::Address>,
    AR: AdvertiseAddrResolver,
    D: Delegate<Id = I, Address = SocketAddr> + 'static,
    G: rand::Rng + Send + Unpin + 'static,
    T::Error: From<SerfError>,
  {
    // Reject runtime knobs a zero capacity would deterministically break BEFORE
    // binding any socket or spawning the detached driver: a `Bounded(0)`
    // observation channel would panic the driver task at startup, and a zero
    // `event_queue_cap` would make the event-stream channel a rendezvous the
    // non-blocking forward can never deposit into, dropping every event.
    runtime_options.validate()?;

    // Cache the join deadline on the handle BEFORE `runtime_options` is moved
    // into the driver bundle, so each await-result join can stamp its absolute
    // `WaitForCompletion` deadline from `Instant::now() + join_deadline`.
    let join_deadline = runtime_options.join_deadline();

    let transport = T::new(options, resolver, advertise_resolver).await?;
    let local_id = transport.local_id().clone();
    let advertise = *transport.advertise_address();

    let (commands_tx, commands_rx) = flume::unbounded::<Command<I, SocketAddr>>();
    let (events_tx, events_rx) =
      flume::bounded::<Event<I, SocketAddr>>(runtime_options.event_queue_cap());
    let events_dropped = Rc::new(Cell::new(0u64));
    let observation_dropped = Rc::new(Cell::new(0u64));
    let coalesced_user_events_dropped = Rc::new(Cell::new(0u64));
    let coalesced_member_events_dropped = Rc::new(Cell::new(0u64));
    let shutdown_flag = Rc::new(Cell::new(false));
    let snapshot: SnapshotCell<I> = Rc::new(RefCell::new(Rc::new(initial_snapshot(
      &local_id, advertise,
    ))));

    // Retain a handle-side clone of each counter before the driver takes
    // ownership. Both sides share the same Cell so reads on the Serf handle
    // always reflect the driver's live count.
    let events_dropped_handle = events_dropped.clone();
    let observation_dropped_handle = observation_dropped.clone();
    let coalesced_user_events_dropped_handle = coalesced_user_events_dropped.clone();
    let coalesced_member_events_dropped_handle = coalesced_member_events_dropped.clone();
    // Clone the serf options before they are moved into the driver so the
    // handle can compute `default_query_timeout` / `default_query_param`
    // without a driver round-trip.
    let serf_options_handle = serf_options.clone();

    let runtime = TransportRuntime::<T, D>::new(
      delegate,
      commands_rx,
      events_tx,
      events_dropped,
      observation_dropped,
      coalesced_user_events_dropped,
      coalesced_member_events_dropped,
      snapshot.clone(),
      shutdown_flag.clone(),
      runtime_options,
      serf_options,
      #[cfg(encryption)]
      keyring,
    );

    // The driver pump owns the transport, the bound sockets, and the endpoint;
    // it runs detached until a `Shutdown` command or all handles drop.
    compio::runtime::spawn(transport.run(runtime, gossip_rng)).detach();

    Ok(Self {
      shared: Rc::new(Shared {
        commands_tx,
        events_rx,
        events_dropped: events_dropped_handle,
        observation_dropped: observation_dropped_handle,
        coalesced_user_events_dropped: coalesced_user_events_dropped_handle,
        coalesced_member_events_dropped: coalesced_member_events_dropped_handle,
        snapshot,
        shutdown_flag,
        local_id,
        advertise,
        join_deadline,
        serf_options: serf_options_handle,
      }),
    })
  }

  /// The local node identifier.
  #[inline]
  pub fn local_id(&self) -> &I {
    &self.shared.local_id
  }

  /// The bound advertise address this node gossips to peers.
  #[inline]
  pub fn advertise_address(&self) -> SocketAddr {
    self.shared.advertise
  }

  /// The latest published membership snapshot (a lock-free `Rc` load).
  #[inline]
  pub fn snapshot(&self) -> Rc<SerfSnapshot<I, SocketAddr>> {
    self.shared.snapshot.borrow().clone()
  }

  /// Number of members in the latest published snapshot.
  #[inline]
  pub fn num_members(&self) -> usize {
    self.shared.snapshot.borrow().num_members()
  }

  /// All known cluster members at the latest published snapshot instant.
  ///
  /// Returns every member the local node knows about — alive, leaving, left,
  /// and failed within the reap window. Mirrors legacy `Serf::members()`
  /// (api.rs:136).
  #[inline]
  pub fn members(&self) -> Vec<Arc<Member<I, SocketAddr>>> {
    self.snapshot().members().to_vec()
  }

  /// The local node's full membership view at the latest snapshot instant.
  ///
  /// Returns the same `Arc` that lives at `local_index` in `members()`, so
  /// the result is always consistent with the member view. Mirrors legacy
  /// `Serf::local_member()` (api.rs:202).
  #[inline]
  pub fn local_member(&self) -> Arc<Member<I, SocketAddr>> {
    self.snapshot().local()
  }

  /// The lifecycle state of the local serf endpoint.
  ///
  /// Derived from the latest published snapshot. Mirrors legacy `Serf::state()`
  /// (api.rs:130).
  #[inline]
  pub fn state(&self) -> SerfState {
    self.snapshot().state()
  }

  /// The local node as an `(id, advertise-address)` pair.
  ///
  /// Composes `local_id()` and `advertise_address()` into a [`Node`]. Mirrors
  /// legacy `Serf::advertise_node()` (api.rs:106).
  #[inline]
  pub fn advertise_node(&self) -> Node<I, SocketAddr> {
    Node::new(self.shared.local_id.clone(), self.shared.advertise)
  }

  /// Force-remove a failed node immediately without pruning the tombstone.
  ///
  /// Thin alias for `force_leave(id, false)`. Serf will stop attempting to
  /// reconnect to this node. Mirrors legacy `Serf::remove_failed_node()`
  /// (api.rs:505).
  pub async fn remove_failed_node(&self, id: I) -> Result<()> {
    self.force_leave(id, false).await
  }

  /// Force-remove a failed node immediately and prune the tombstone.
  ///
  /// Thin alias for `force_leave(id, true)`. The node is removed immediately
  /// rather than waiting for the tombstone timeout. Mirrors legacy
  /// `Serf::remove_failed_node_prune()` (api.rs:513).
  pub async fn remove_failed_node_prune(&self, id: I) -> Result<()> {
    self.force_leave(id, true).await
  }

  /// Default query timeout derived from the current snapshot member count.
  ///
  /// Computed as `200ms × query_timeout_mult × ⌈log₁₀(N+1)⌉` where N is the
  /// snapshot member count. Matches the machine's own zero-timeout resolution
  /// (endpoint/mod.rs:2665) and mirrors legacy `Serf::default_query_timeout()`
  /// (query.rs:421).
  pub fn default_query_timeout(&self) -> Duration {
    let n = self.num_members();
    let mult = self.shared.serf_options.query_timeout_mult();
    let log_factor = ((n as f64 + 1.0).log10().ceil() as u32).max(1);
    Duration::from_millis(200) * mult as u32 * log_factor
  }

  /// Default query parameters derived from the current snapshot.
  ///
  /// Returns a [`QueryParams`] with no filters, no relay, no ACK, and a
  /// timeout from [`Self::default_query_timeout`]. Mirrors legacy
  /// `Serf::default_query_param()` (query.rs:430).
  pub fn default_query_param(&self) -> QueryParams<I> {
    QueryParams {
      filters: Vec::new(),
      relay_factor: 0,
      request_ack: false,
      timeout: self.default_query_timeout(),
    }
  }

  /// Cumulative number of [`Event`]s dropped at the bounded user-facing event
  /// channel since this node started.
  ///
  /// Incremented by the driver's observation task each time [`Serf::events`]
  /// consumers are not draining fast enough and the channel is full. Each
  /// increment represents one silently discarded event. The counter is
  /// monotonically increasing.
  ///
  /// A non-zero value indicates backpressure: drain [`Serf::events`] promptly
  /// or raise `RuntimeOptions::event_queue_cap`.
  #[inline]
  pub fn events_dropped(&self) -> u64 {
    self.shared.events_dropped.get()
  }

  /// Cumulative number of [`Event`]s dropped at the bounded internal
  /// observation channel since this node started.
  ///
  /// Incremented by the driver pump each time the delegate dispatch loop falls
  /// behind and the observation queue overflows. Each increment represents one
  /// event that was never delivered to the delegate or the event stream. The
  /// counter is monotonically increasing.
  ///
  /// A non-zero value indicates a slow delegate: raise
  /// `RuntimeOptions::observation_channel` capacity.
  #[inline]
  pub fn observation_dropped(&self) -> u64 {
    self.shared.observation_dropped.get()
  }

  /// Cumulative number of coalescing user events the driver's endpoint shed
  /// because its user coalescer was at the configured buffered-volume cap
  /// (`Options::max_coalesced_user_events`) since this node started.
  ///
  /// Republished by the driver pump each iteration. Lifetime total, saturating,
  /// and never cleared by a flush; always `0` when user coalescing is disabled. A
  /// non-zero value indicates the coalescer is shedding load: raise
  /// `Options::max_coalesced_user_events` or slow the user-event source.
  #[inline]
  pub fn coalesced_user_events_dropped(&self) -> u64 {
    self.shared.coalesced_user_events_dropped.get()
  }

  /// Cumulative number of member changes the driver's endpoint shed because its
  /// member coalescer was at its per-window cardinality cap since this node
  /// started.
  ///
  /// Republished by the driver pump each iteration. Lifetime total, saturating;
  /// always `0` when member coalescing is disabled.
  #[inline]
  pub fn coalesced_member_events_dropped(&self) -> u64 {
    self.shared.coalesced_member_events_dropped.get()
  }

  /// Subscribe to the serf [`Event`] stream. Multiple subscribers round-robin
  /// (the channel is MPMC, not broadcast).
  #[inline]
  pub fn events(&self) -> EventStream<I, SocketAddr> {
    EventStream::new(self.shared.events_rx.clone())
  }

  /// Send `cmd` to the driver, failing fast if the node has shut down.
  fn send(&self, cmd: Command<I, SocketAddr>) -> Result<()> {
    if self.shared.shutdown_flag.get() {
      return Err(SerfError::Shutdown);
    }
    self
      .shared
      .commands_tx
      .send(cmd)
      .map_err(|_| SerfError::CommandSend)
  }

  /// Join an existing cluster through a single seed, waiting for the seed to be
  /// contacted. Returns the resolved [`SocketAddr`] of the seed actually reached.
  ///
  /// `node` is resolved through `resolver` (an already-resolved
  /// [`MaybeResolved::Resolved`] passes straight through). The call dispatches a
  /// push/pull to every resolved address and resolves once one succeeds (returns
  /// that address) or the configured
  /// [`join_deadline`](crate::RuntimeOptions::with_join_deadline) elapses with no
  /// contact ([`SerfError::JoinAllFailed`]). A `node` that resolves to zero
  /// addresses surfaces `JoinAllFailed` rather than a silent success.
  ///
  /// When `ignore_old` is `true`, replay of the seed's pre-join user events is
  /// suppressed: the machine records each join exchange's `StreamId` as a
  /// one-shot ignore-join target consumed at that exchange's own merge. Dropping
  /// the join future cannot leak the suppression onto a later join, and a
  /// concurrent join — even to the SAME seed — is unaffected because it is a
  /// distinct exchange.
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
      // The driver only replies `Ok` with a non-empty contacted set, so `next`
      // is the single reached address; the fallback can never fire.
      Ok(reached) => reached
        .into_iter()
        .next()
        .ok_or_else(|| SerfError::JoinAllFailed(JoinFailed::new(1, 0))),
      Err((_, e)) => Err(e),
    }
  }

  /// Join an existing cluster through several seeds, waiting for the join to
  /// complete. Returns the set of seed addresses actually contacted on success,
  /// or the legacy partial-success tuple `(reached_so_far, error)` on failure.
  ///
  /// Each `existing` seed is resolved through `resolver` and a push/pull is
  /// dispatched to every resolved address; the call resolves once every
  /// dispatched exchange terminates or the configured
  /// [`join_deadline`](crate::RuntimeOptions::with_join_deadline) elapses. At
  /// least one contact yields `Ok(contacted)`; zero contacts yields
  /// `Err((SmallVec::new(), SerfError::JoinAllFailed(..)))`. An empty `existing`
  /// iterator is a trivial `Ok(empty)` (no command is sent); a non-empty input
  /// resolving to zero addresses surfaces `JoinAllFailed`.
  ///
  /// `ignore_old` behaves as in [`join`](Self::join).
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
    // Empty input is a trivial caller-side request — `Ok(empty)` without
    // sending a command (mirrors the memberlist join's empty short-circuit).
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

  /// Fire-and-forget join: resolve `seeds`, dispatch a push/pull against each,
  /// and return the dispatched-exchange count immediately without waiting for
  /// any to complete.
  ///
  /// Unlike [`join`](Self::join) / [`join_many`](Self::join_many) this never
  /// waits for contact: the returned count is the number of resolved seed
  /// addresses handed to the driver, NOT the number reached. Actual membership
  /// surfaces through the [`events`](Self::events) stream and the
  /// [`snapshot`](Self::snapshot). Intended for long-lived background
  /// re-discovery loops that observe membership separately.
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
  /// `ignore_old` to each seed's join push/pull.
  ///
  /// `requested_if_empty` is the JoinAllFailed denominator used only when
  /// `addrs` is empty (the input seed count, since no exchange is dispatched);
  /// when `addrs` is non-empty the driver derives `requested` from the
  /// dispatched-exchange count itself.
  ///
  /// No serialising lock is needed: `ignore_old` is recorded per-EXCHANGE (keyed
  /// by the join's `StreamId`) in the machine and consumed one-shot at that
  /// exchange's own merge (or cleared by the driver when the join terminates
  /// without merging), so concurrent joins — even to the SAME seed, and a
  /// concurrent `dispatch_join` — never interfere.
  async fn join_await(
    &self,
    addrs: Vec<SocketAddr>,
    requested_if_empty: usize,
    ignore_old: bool,
  ) -> JoinReply {
    // A non-empty seed input that resolved to zero addresses is NOT a silent
    // success — surface `JoinAllFailed` so a bootstrap / discovery outage is
    // never reported as a healthy zero-contact join.
    if addrs.is_empty() {
      return Err((
        SmallVec::new(),
        SerfError::JoinAllFailed(JoinFailed::new(requested_if_empty, 0)),
      ));
    }
    let deadline = Instant::now() + self.shared.join_deadline;
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

  /// Gracefully leave the cluster. Resolves once peers have been notified or the
  /// configured leave timeout elapses.
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
    let (tx, rx) = oneshot::channel();
    self.send(Command::InstallKey(KeyCmd {
      key,
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
    let (tx, rx) = oneshot::channel();
    self.send(Command::UseKey(KeyCmd {
      key,
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
    let (tx, rx) = oneshot::channel();
    self.send(Command::RemoveKey(KeyCmd {
      key,
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
    let (tx, rx) = oneshot::channel();
    self.send(Command::ListKeys(ListKeysCmd {
      now: Instant::now(),
      reply: tx,
    }))?;
    await_reply(rx).await
  }

  /// Gracefully shut the driver down, releasing the bound ports before this
  /// resolves so an immediate rebind on the same address succeeds.
  pub async fn shutdown(&self) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::Shutdown(ShutdownCmd { reply: tx }))?;
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

/// Await an address-set join reply, mapping a dropped reply channel to the
/// [`SerfError::ReplyClosed`] tuple form.
async fn await_join_reply(rx: oneshot::Receiver<JoinReply>) -> JoinReply {
  match rx.await {
    Ok(res) => res,
    Err(_) => Err((SmallVec::new(), SerfError::ReplyClosed)),
  }
}

/// Resolve a slice of [`MaybeResolved`] seeds into a flat `Vec<SocketAddr>`.
///
/// [`Resolved`](MaybeResolved::Resolved) entries pass through directly;
/// [`Unresolved`](MaybeResolved::Unresolved) entries run through `resolver` and
/// their results are appended. A resolver failure surfaces as
/// [`SerfError::Resolve`]. Mirrors the memberlist driver's `resolve_seeds`.
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

#[cfg(all(test, feature = "tcp"))]
mod tests;
