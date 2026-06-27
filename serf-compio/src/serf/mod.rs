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

use std::{
  cell::{Cell, RefCell},
  net::SocketAddr,
  rc::Rc,
  sync::Arc,
};

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
use smol_str::SmolStr;

#[cfg(encryption)]
use crate::command::{KeyCmd, ListKeysCmd};
use crate::{
  command::{
    Command, ForceLeaveCmd, JoinCmd, LeaveCmd, QueryCmd, RespondCmd, SetEventJoinIgnoreCmd,
    SetTagsCmd, ShutdownCmd, UserEventCmd,
  },
  delegate::Delegate,
  driver::options::RuntimeOptions,
  error::{Result, SerfError},
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
  snapshot: SnapshotCell<I>,
  shutdown_flag: Rc<Cell<bool>>,
  local_id: I,
  advertise: SocketAddr,
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

    let transport = T::new(options, resolver, advertise_resolver).await?;
    let local_id = transport.local_id().clone();
    let advertise = *transport.advertise_address();

    let (commands_tx, commands_rx) = flume::unbounded::<Command<I, SocketAddr>>();
    let (events_tx, events_rx) =
      flume::bounded::<Event<I, SocketAddr>>(runtime_options.event_queue_cap());
    let events_dropped = Rc::new(Cell::new(0u64));
    let observation_dropped = Rc::new(Cell::new(0u64));
    let shutdown_flag = Rc::new(Cell::new(false));
    let snapshot: SnapshotCell<I> = Rc::new(RefCell::new(Rc::new(initial_snapshot(
      &local_id, advertise,
    ))));

    // Retain a handle-side clone of each counter before the driver takes
    // ownership. Both sides share the same Cell so reads on the Serf handle
    // always reflect the driver's live count.
    let events_dropped_handle = events_dropped.clone();
    let observation_dropped_handle = observation_dropped.clone();

    let runtime = TransportRuntime::<T, D>::new(
      delegate,
      commands_rx,
      events_tx,
      events_dropped,
      observation_dropped,
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
        snapshot,
        shutdown_flag,
        local_id,
        advertise,
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

  /// Initiate joins to `seeds` (already-resolved addresses).
  ///
  /// This is **dispatch-only**: it announces the local join intent and starts a
  /// push-pull to each seed, returning the count of seeds the driver dispatched
  /// a push-pull to. The returned count is NOT a contact count — a seed may be
  /// unreachable and its exchange fail afterward. Actual cluster membership is
  /// reported through the [`Event`](serf_proto::event::Event) stream and the
  /// published [`snapshot`](Self::snapshot) as peers are merged.
  pub async fn join(&self, seeds: Vec<SocketAddr>) -> Result<usize> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::Join(JoinCmd { seeds, reply: tx }))?;
    await_reply(rx).await
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

  /// Enable or disable suppression of member-join events in the observation
  /// stream.
  pub async fn set_event_join_ignore(&self, ignore: bool) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.send(Command::SetEventJoinIgnore(SetEventJoinIgnoreCmd {
      ignore,
      reply: tx,
    }))?;
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

#[cfg(all(test, feature = "tcp"))]
mod tests;
