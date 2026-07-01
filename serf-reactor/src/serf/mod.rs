//! The [`Serf`] handle — construction, lifecycle, command dispatch, and lock-free
//! reads.
//!
//! [`Serf::new`] binds a [`Transport`], builds the [`Shared`] state + events
//! channel, and spawns the driver pump on the agnostic runtime `R`. Every command
//! method pushes one [`Command`] onto the shared queue and (for the awaited kinds)
//! parks on a one-shot reply the driver resolves. `Clone` / `Drop` reference-count
//! the shared driver; membership reads go through the lock-free
//! [`SerfSnapshot`]. The ergonomic per-backend constructors (`tcp` / `tls` /
//! `quic`), the richer snapshot accessors, and the full test suite are layered on
//! in a later chunk.

use core::{marker::PhantomData, time::Duration};
use std::{net::SocketAddr, sync::Arc};

use agnostic::Runtime;
use bytes::Bytes;
use futures_channel::oneshot;
use memberlist_proto::{Instant, Node};
use serf_driver::SerfSnapshot;
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
#[cfg(encryption)]
use crate::delegate::KeyringDelegate;
use crate::{
  MaybeResolved,
  command::{
    Command, ForceLeaveCmd, JoinCmd, JoinKind, JoinReply, LeaveCmd, QueryCmd, RespondCmd,
    SetTagsCmd, ShutdownCmd, UserEventCmd, WaitForCompletionArgs,
  },
  delegate::Delegate,
  driver::options::RuntimeOptions,
  error::{JoinFailed, Result, SerfError},
  events::EventStream,
  resolver::{AdvertiseAddrResolver, Resolver},
  shared::Shared,
  transport::{Transport, TransportRuntime},
};
#[cfg(encryption)]
use memberlist_proto::SecretKey;

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
    // Cache the join deadline before `runtime_options` moves into the bundle.
    let join_deadline = runtime_options.join_deadline();

    let transport = T::new(options, resolver, advertise_resolver).await?;
    let local_id = transport.local_id().clone();
    let advertise = *transport.advertise_address();

    let (events_tx, events_rx) =
      flume::bounded::<Event<I, SocketAddr>>(runtime_options.event_queue_cap());
    let shared = Arc::new(Shared::new(initial_snapshot(&local_id, advertise)));

    let runtime = TransportRuntime::<I, D>::new(
      delegate,
      shared.clone(),
      events_tx,
      runtime_options,
      serf_options,
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
      _a: PhantomData,
      _r: PhantomData,
    })
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
