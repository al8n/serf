use std::time::Duration;

use either::Either;
use futures::{FutureExt, StreamExt};
use memberlist_core::{
  CheapClone,
  agnostic_lite::AfterHandle,
  bytes::Bytes,
  delegate::EventDelegate,
  proto::{Data, Meta, NodeState, OneOrMore, TinyVec},
  tracing,
  transport::{MaybeResolvedAddress, Node},
};
use rand::{Rng, SeedableRng};
use serf_proto::{MessageRef, Tags, UserEventMessageRef};
use smol_str::SmolStr;

use crate::{
  QueueOptions,
  coalesce::{MemberEventCoalescer, UserEventCoalescer, coalesced_event},
  coordinate::CoordinateOptions,
  error::Error,
  event::{InternalQueryEvent, MemberEvent, MemberEventType, QueryContext, QueryEvent},
  snapshot::{Snapshot, open_and_replay_snapshot},
  types::{
    DelegateVersion, Epoch, JoinMessage, LeaveMessage, Member, MemberState, MemberStatus,
    MemberlistDelegateVersion, MemberlistProtocolVersion, MessageType, NodeIntent, ProtocolVersion,
    QueryFlag, QueryMessage, QueryResponseMessage, UserEvent, UserEventMessage,
  },
};

use self::internal_query::SerfQueries;

use super::*;

/// Re-export the unit tests
#[cfg(feature = "test")]
#[cfg_attr(docsrs, doc(cfg(feature = "test")))]
pub mod tests;

impl<T, D> Serf<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  #[cfg(any(feature = "test", test))]
  pub(crate) async fn with_message_dropper(
    transport: T::Options,
    opts: Options,
    message_dropper: Box<dyn delegate::MessageDropper>,
  ) -> Result<Self, Error<T, D>> {
    Self::new_in(
      None,
      None,
      transport,
      opts,
      #[cfg(any(feature = "test", test))]
      Some(message_dropper),
    )
    .await
  }

  pub(crate) async fn new_in(
    ev: Option<async_channel::Sender<CrateEvent<T, D>>>,
    delegate: Option<D>,
    transport: T::Options,
    opts: Options,
    #[cfg(any(test, feature = "test"))] message_dropper: Option<Box<dyn delegate::MessageDropper>>,
  ) -> Result<Self, Error<T, D>> {
    if opts.max_user_event_size > USER_EVENT_SIZE_LIMIT {
      return Err(Error::user_event_limit_too_large(USER_EVENT_SIZE_LIMIT));
    }

    // Check that the meta data length is okay
    {
      let tags = opts.tags.load();
      if !tags.as_ref().is_empty() {
        let len = tags.encoded_len_with_length_delimited();
        let meta_encoded_len = 1 + (len as u32).encoded_len() + len;
        if meta_encoded_len > Meta::MAX_SIZE {
          return Err(Error::tags_too_large(len));
        }
      }
    }

    let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);

    let handles = FuturesUnordered::new();
    let event_tx = ev.map(|mut event_tx| {
      // Check if serf member event coalescing is enabled
      if opts.coalesce_period > Duration::ZERO && opts.quiescent_period > Duration::ZERO {
        let c = MemberEventCoalescer::new();

        event_tx = coalesced_event(
          event_tx,
          shutdown_rx.clone(),
          opts.coalesce_period,
          opts.quiescent_period,
          c,
        );
      }

      // Check if user event coalescing is enabled
      if opts.user_coalesce_period > Duration::ZERO && opts.user_quiescent_period > Duration::ZERO {
        let c = UserEventCoalescer::new();
        event_tx = coalesced_event(
          event_tx,
          shutdown_rx.clone(),
          opts.user_coalesce_period,
          opts.user_quiescent_period,
          c,
        );
      }

      event_tx
    });

    // Listen for internal Serf queries. This is setup before the snapshotter, since
    // we want to capture the query-time, but the internal listener does not passthrough
    // the queries
    let (event_tx, handle) = SerfQueries::new(event_tx.clone(), shutdown_rx.clone());
    handles.push(handle);

    let clock = LamportClock::new();
    let event_clock = LamportClock::new();
    let query_clock = LamportClock::new();
    let mut event_min_time = LamportTime::ZERO;
    let mut query_min_time = LamportTime::ZERO;

    // Try access the snapshot
    let (old_clock, old_event_clock, old_query_clock, event_tx, alive_nodes, handle) =
      if let Some(sp) = opts.snapshot_path.as_ref() {
        let rs = open_and_replay_snapshot(sp, opts.rejoin_after_leave)?;
        let old_clock = rs.last_clock;
        let old_event_clock = rs.last_event_clock;
        let old_query_clock = rs.last_query_clock;
        let (event_tx, alive_nodes, handle) = Snapshot::from_replay_result(
          rs,
          SNAPSHOT_SIZE_LIMIT,
          opts.rejoin_after_leave,
          clock.clone(),
          event_tx,
          shutdown_rx.clone(),
          #[cfg(feature = "metrics")]
          opts.memberlist_options.metric_labels().clone(),
        )?;
        event_min_time = old_event_clock + LamportTime::new(1);
        query_min_time = old_query_clock + LamportTime::new(1);
        (
          old_clock,
          old_event_clock,
          old_query_clock,
          event_tx,
          alive_nodes,
          Some(handle),
        )
      } else {
        (
          LamportTime::new(0),
          LamportTime::new(0),
          LamportTime::new(0),
          event_tx,
          TinyVec::new(),
          None,
        )
      };

    // Set up network coordinate client.
    let coord = (!opts.disable_coordinates).then_some({
      CoordinateClient::with_options(CoordinateOptions {
        #[cfg(feature = "metrics")]
        metric_labels: opts.memberlist_options.metric_labels().clone(),
        ..Default::default()
      })
    });
    let members = Arc::new(RwLock::new(Members::default()));
    let num_members = NumMembers::from(members.clone());
    // Setup the various broadcast queues, which we use to send our own
    // custom broadcasts along the gossip channel.
    let broadcasts = Arc::new(TransmitLimitedQueue::<SerfBroadcast, _>::new(
      opts.memberlist_options.retransmit_mult(),
      num_members.clone(),
    ));
    let event_broadcasts = Arc::new(TransmitLimitedQueue::<SerfBroadcast, _>::new(
      opts.memberlist_options.retransmit_mult(),
      num_members.clone(),
    ));
    let query_broadcasts = Arc::new(TransmitLimitedQueue::<SerfBroadcast, _>::new(
      opts.memberlist_options.retransmit_mult(),
      num_members.clone(),
    ));

    // Create a buffer for events and queries
    let event_buffer = vec![None; opts.event_buffer_size];
    let query_buffer = vec![None; opts.query_buffer_size];

    // Ensure our lamport clock is at least 1, so that the default
    // join LTime of 0 does not cause issues
    clock.increment();
    event_clock.increment();
    query_clock.increment();

    // Restore the clock from snap if we have one
    clock.witness(old_clock);
    event_clock.witness(old_event_clock);
    query_clock.witness(old_query_clock);

    // Create the underlying memberlist that will manage membership
    // and failure detection for the Serf instance.
    let memberlist = Memberlist::with_delegate(
      {
        #[cfg(any(test, feature = "test"))]
        {
          match message_dropper {
            Some(dropper) => SerfDelegate::with_dropper(delegate, dropper, opts.tags.clone()),
            None => SerfDelegate::new(delegate, opts.tags.clone()),
          }
        }
        #[cfg(not(any(test, feature = "test")))]
        {
          SerfDelegate::new(delegate, opts.tags.clone())
        }
      },
      transport,
      opts.memberlist_options.clone(),
    )
    .await?;

    let c = SerfCore {
      clock,
      event_clock,
      query_clock,
      broadcasts,
      memberlist,
      members,
      event_broadcasts,
      event_join_ignore: AtomicBool::new(false),
      event_core: RwLock::new(EventCore {
        min_time: event_min_time,
        buffer: event_buffer,
      }),
      query_broadcasts,
      query_core: Arc::new(RwLock::new(QueryCore {
        min_time: query_min_time,
        responses: HashMap::new(),
        buffer: query_buffer,
      })),
      opts,
      handles: AtomicRefCell::new(handles),
      state: parking_lot::Mutex::new(SerfState::Alive),
      join_lock: Mutex::new(()),
      snapshot: handle,
      #[cfg(feature = "encryption")]
      key_manager: crate::key_manager::KeyManager::new(),
      shutdown_tx,
      shutdown_rx: shutdown_rx.clone(),
      coord_core: coord.map(|cc| {
        Arc::new(CoordCore {
          client: cc,
          cache: parking_lot::RwLock::new(HashMap::new()),
        })
      }),
      event_tx,
    };
    let this = Serf { inner: Arc::new(c) };
    // update delegate
    let that = this.clone();
    let memberlist_delegate = this.inner.memberlist.delegate().unwrap();
    memberlist_delegate.store(that);
    let local_node = this.inner.memberlist.local_state().await;
    if let Some(local_node) = local_node {
      memberlist_delegate.notify_join(local_node).await;
    }

    // update key manager
    #[cfg(feature = "encryption")]
    {
      let that = this.clone();
      this.inner.key_manager.store(that);
    }

    let handles = this.inner.handles.borrow();
    // Start the background tasks. See the documentation above each method
    // for more information on their role.
    let h = Reaper {
      coord_core: this.inner.coord_core.clone(),
      memberlist: this.inner.memberlist.clone(),
      members: this.inner.members.clone(),
      event_tx: this.inner.event_tx.clone(),
      shutdown_rx: shutdown_rx.clone(),
      reap_interval: this.inner.opts.reap_interval,
      reconnect_timeout: this.inner.opts.reconnect_timeout,
      recent_intent_timeout: this.inner.opts.recent_intent_timeout,
      tombstone_timeout: this.inner.opts.tombstone_timeout,
    }
    .spawn();
    handles.push(h);

    let h = Reconnector {
      members: this.inner.members.clone(),
      memberlist: this.inner.memberlist.clone(),
      shutdown_rx: shutdown_rx.clone(),
      reconnect_interval: this.inner.opts.reconnect_interval,
    }
    .spawn();
    handles.push(h);

    let h = QueueChecker {
      name: "serf.queue.intent",
      queue: this.inner.broadcasts.clone(),
      members: this.inner.members.clone(),
      opts: this.inner.opts.queue_opts(),
      shutdown_rx: shutdown_rx.clone(),
    }
    .spawn::<T::Runtime>();
    handles.push(h);

    let h = QueueChecker {
      name: "serf.queue.event",
      queue: this.inner.event_broadcasts.clone(),
      members: this.inner.members.clone(),
      opts: this.inner.opts.queue_opts(),
      shutdown_rx: shutdown_rx.clone(),
    }
    .spawn::<T::Runtime>();
    handles.push(h);

    let h = QueueChecker {
      name: "serf.queue.query",
      queue: this.inner.query_broadcasts.clone(),
      members: this.inner.members.clone(),
      opts: this.inner.opts.queue_opts(),
      shutdown_rx: shutdown_rx.clone(),
    }
    .spawn::<T::Runtime>();
    handles.push(h);

    // Attempt to re-join the cluster if we have known nodes
    if !alive_nodes.is_empty() {
      let memberlist = this.inner.memberlist.clone();
      Self::handle_rejoin(memberlist, alive_nodes);
    }
    drop(handles);
    Ok(this)
  }

  pub(crate) async fn has_alive_members(&self) -> bool {
    let members = self.inner.members.read().await;
    for member in members.states.values() {
      if member.member.node.id() == self.inner.memberlist.local_id() {
        continue;
      }

      if member.member.status == MemberStatus::Alive {
        return true;
      }
    }

    false
  }

  /// Takes a Serf message type, encodes it for the wire, and queues
  /// the broadcast. If a notify channel is given, this channel will be closed
  /// when the broadcast is sent.
  pub(crate) async fn broadcast(
    &self,
    msg: Bytes,
    notify_tx: Option<async_channel::Sender<()>>,
  ) -> Result<(), Error<T, D>> {
    self
      .inner
      .broadcasts
      .queue_broadcast(SerfBroadcast { msg, notify_tx })
      .await;
    Ok(())
  }

  /// Broadcasts a new join intent with a
  /// given clock value. It is used on either join, or if
  /// we need to refute an older leave intent. Cannot be called
  /// with the memberLock held.
  pub(crate) async fn broadcast_join(&self, ltime: LamportTime) -> Result<(), Error<T, D>> {
    // Construct message to update our lamport clock
    let msg = JoinMessage::new(ltime, self.inner.memberlist.local_id().cheap_clone());
    self.inner.clock.witness(ltime);

    // Process update locally
    self.handle_node_join_intent(&msg).await;

    let msg = serf_proto::Encodable::encode_to_bytes(&msg)?;
    // Start broadcasting the update
    if let Err(e) = self.broadcast(msg, None).await {
      tracing::warn!(err=%e, "serf: failed to broadcast join intent");
      return Err(e);
    }

    Ok(())
  }

  /// Serialize the current keyring and save it to a file.
  #[cfg(feature = "encryption")]
  pub(crate) async fn write_keyring_file(&self) -> std::io::Result<()> {
    use base64::{Engine as _, engine::general_purpose};

    let Some(path) = self.inner.opts.keyring_file() else {
      return Ok(());
    };

    if let Some(keyring) = self.inner.memberlist.keyring() {
      let encoded_keys = keyring
        .keys()
        .map(|k| general_purpose::STANDARD.encode(k))
        .collect::<TinyVec<_>>();

      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.truncate(true).write(true).create(true).mode(0o600);
        return opts.open(path).and_then(|file| {
          serde_json::to_writer_pretty(file, &encoded_keys)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
      }
      // TODO: I don't know how to set permissions on windows
      // need helps :)
      #[cfg(windows)]
      {
        let mut opts = std::fs::OpenOptions::new();
        opts.truncate(true).write(true).create(true);
        return opts.open(path).and_then(|file| {
          serde_json::to_writer_pretty(file, &encoded_keys)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
      }
    }

    Ok(())
  }

  #[cfg(feature = "test")]
  pub(crate) async fn get_queue_max(&self) -> usize {
    let mut max = self.inner.opts.max_queue_depth;
    if self.inner.opts.min_queue_depth > 0 {
      let num_members = self.inner.members.read().await.states.len();
      max = num_members * 2;

      if max < self.inner.opts.min_queue_depth {
        max = self.inner.opts.min_queue_depth;
      }
    }
    max
  }

  /// Forcibly removes a failed node from the cluster
  /// immediately, instead of waiting for the reaper to eventually reclaim it.
  /// This also has the effect that Serf will no longer attempt to reconnect
  /// to this node.
  pub(crate) async fn force_leave(&self, id: T::Id, prune: bool) -> Result<(), Error<T, D>> {
    // Construct the message to broadcast
    let msg = LeaveMessage {
      ltime: self.inner.clock.time(),
      id,
      prune,
    };

    // Process our own event
    self.handle_node_leave_intent(&msg).await;

    // If we have no members, then we don't need to broadcast
    if !self.has_alive_members().await {
      return Ok(());
    }

    let msg = serf_proto::Encodable::encode_to_bytes(&msg)?;
    // Broadcast the remove
    let (ntx, nrx) = async_channel::bounded(1);
    self.broadcast(msg, Some(ntx)).await?;

    // Wait for the broadcast
    <T::Runtime as RuntimeLite>::timeout(self.inner.opts.broadcast_timeout, nrx.recv())
      .await
      .map_err(|_| Error::removal_broadcast_timeout())?
      .map_err(|_| Error::broadcast_channel_closed())
  }
}

struct Reaper<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  coord_core: Option<Arc<CoordCore<T::Id>>>,
  memberlist: Memberlist<T, SerfDelegate<T, D>>,
  members: Arc<RwLock<Members<T::Id, T::ResolvedAddress>>>,
  event_tx: async_channel::Sender<CrateEvent<T, D>>,
  shutdown_rx: async_channel::Receiver<()>,
  reap_interval: Duration,
  reconnect_timeout: Duration,
  recent_intent_timeout: Duration,
  tombstone_timeout: Duration,
}

macro_rules! erase_node {
  ($tx:ident <- $coord:ident($members:ident[$id:ident].$m:ident)) => {{
    // takes a node completely out of the member list
    $members.states.remove($id);

    // Tell the coordinate client the node has gone away and delete
    // its cached coordinates.
    if let Some(cc) = $coord {
      cc.client.forget_node($id);
      cc.cache.write().remove($id);
    }

    // Send out event
    let _ = $tx
      .send(CrateEvent::from(MemberEvent {
        ty: MemberEventType::Reap,
        members: Arc::new(TinyVec::from($m.member.clone())),
      }))
      .await;
  }};
}

macro_rules! reap {
  (
    $tx:ident <- $local_id:ident.$reconnector:ident($timeout: ident($members: ident.$ty: ident, $coord:ident))
  ) => {{
    let mut n = $members.$ty.len();
    let mut i = 0;
    while i < n {
      let m = $members.$ty[i].clone();
      let mut member_timeout = $timeout;
      if let Some(r) = $reconnector {
        member_timeout = r.reconnect_timeout(&m.member, member_timeout);
      }

      // Skip if the timeout is not yet reached
      if let Some(leave_time) = m.leave_time {
        if leave_time.elapsed() <= member_timeout {
          i += 1;
          continue;
        }
      }

      // Delete from the list
      $members.$ty.swap_remove(i);
      n -= 1;

      // Delete from members and send out event
      let id = m.member.node.id();
      tracing::info!("serf: event member reap: {} reaps {}", $local_id, id);

      erase_node!($tx <- $coord($members[id].m));
    }
  }};
}

impl<T, D> Reaper<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  async fn run(self) {
    let tick = <T::Runtime as RuntimeLite>::interval(self.reap_interval);
    futures::pin_mut!(tick);
    loop {
      futures::select! {
        _ = tick.next().fuse() => {
          let mut ms = self.members.write().await;
          let local_id = self.memberlist.local_id();
          Self::reap_failed(local_id, &mut ms, &self.event_tx, self.memberlist.delegate().and_then(|d| d.delegate()), self.coord_core.as_deref(), self.reconnect_timeout).await;
          Self::reap_left(local_id, &mut ms, &self.event_tx, self.memberlist.delegate().and_then(|d| d.delegate()), self.coord_core.as_deref(), self.tombstone_timeout).await;
          reap_intents(&mut ms.recent_intents, Epoch::now(), self.recent_intent_timeout);
          if self.shutdown_rx.is_closed() {
            break;
          }
        }
        _ = self.shutdown_rx.recv().fuse() => {
          break;
        }
      }
    }
    tracing::debug!("serf: reaper exits");
  }

  fn spawn(self) -> <<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()> {
    <T::Runtime as RuntimeLite>::spawn(async move {
      self.run().await;
    })
  }

  async fn reap_failed(
    local_id: &T::Id,
    old: &mut Members<T::Id, T::ResolvedAddress>,
    event_tx: &async_channel::Sender<CrateEvent<T, D>>,
    reconnector: Option<&D>,
    coord: Option<&CoordCore<T::Id>>,
    timeout: Duration,
  ) {
    reap!(event_tx <- local_id.reconnector(timeout(old.failed_members, coord)))
  }

  async fn reap_left(
    local_id: &T::Id,
    old: &mut Members<T::Id, T::ResolvedAddress>,
    event_tx: &async_channel::Sender<CrateEvent<T, D>>,
    reconnector: Option<&D>,
    coord: Option<&CoordCore<T::Id>>,
    timeout: Duration,
  ) {
    reap!(event_tx <- local_id.reconnector(timeout(old.left_members, coord)))
  }
}

struct Reconnector<T, D>
where
  T: Transport,
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
{
  members: Arc<RwLock<Members<T::Id, T::ResolvedAddress>>>,
  memberlist: Memberlist<T, SerfDelegate<T, D>>,
  shutdown_rx: async_channel::Receiver<()>,
  reconnect_interval: Duration,
}

impl<T, D> Reconnector<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn spawn(self) -> <<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()> {
    let mut rng = rand::rngs::StdRng::from_rng(&mut rand::rng());

    <T::Runtime as RuntimeLite>::spawn(async move {
      let tick = <T::Runtime as RuntimeLite>::interval(self.reconnect_interval);
      futures::pin_mut!(tick);
      loop {
        futures::select! {
          _ = tick.next().fuse() => {
            let mu = self.members.read().await;
            let num_failed = mu.failed_members.len();
            // Nothing to do if there are no failed members
            if num_failed == 0 {
              continue;
            }

            // Probability we should attempt to reconect is given
            // by num failed / (num members - num failed - num left)
            // This means that we probabilistically expect the cluster
            // to attempt to connect to each failed member once per
            // reconnect interval

            let num_alive = (mu.states.len() - num_failed - mu.left_members.len()).max(1);
            let prob = num_failed as f32 / num_alive as f32;
            let r: f32 = rng.random();
            if r > prob {
              tracing::debug!("serf: forgoing reconnect for random throttling");
              continue;
            }

            // Select a random member to try and join
            let idx: usize = rng.random_range(0..num_failed);
            let member = &mu.failed_members[idx];

            let (id, address) = member.member.node().cheap_clone().into_components();
            drop(mu); // release read lock
            tracing::info!("serf: attempting to reconnect to {}", id);
            // Attempt to join at the memberlist level
            if let Err(e) = self.memberlist.join(Node::new(id.cheap_clone(), MaybeResolvedAddress::resolved(address))).await {
              tracing::warn!("serf: failed to reconnect {}: {}", id, e);
            } else {
              tracing::info!("serf: successfully reconnected to {}", id);
            }
          }
          _ = self.shutdown_rx.recv().fuse() => {
            break;
          }
        }
      }

      tracing::debug!("serf: reconnector exits");
    })
  }
}

struct QueueChecker<I, A> {
  name: &'static str,
  queue: Arc<TransmitLimitedQueue<SerfBroadcast, NumMembers<I, A>>>,
  members: Arc<RwLock<Members<I, A>>>,
  opts: QueueOptions,
  shutdown_rx: async_channel::Receiver<()>,
}

impl<I, A> QueueChecker<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  fn spawn<R: RuntimeLite>(self) -> <<R as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()> {
    R::spawn(async move {
      let tick = R::interval(self.opts.check_interval);
      futures::pin_mut!(tick);
      loop {
        futures::select! {
          _ = tick.next().fuse() => {
            let numq = self.queue.num_queued().await;
            #[cfg(feature = "metrics")]
            {
              metrics::gauge!(self.name, self.opts.metric_labels.iter()).set(numq as f64);
            }
            if numq >= self.opts.depth_warning {
              tracing::warn!("serf: queue {} depth: {}", self.name, numq);
            }

            let max = self.get_queue_max().await;
            if numq >= max {
              tracing::warn!("serf: {} queue depth ({}) exceeds limit ({}), dropping messages!", self.name, numq, max);
              self.queue.prune(max).await;
            }
          }
          _ = self.shutdown_rx.recv().fuse() => {
            break;
          }
        }
      }

      tracing::debug!("serf: {} queue checker exits", self.name);
    })
  }

  async fn get_queue_max(&self) -> usize {
    let mut max = self.opts.max_queue_depth;
    if self.opts.min_queue_depth > 0 {
      let num_members = self.members.read().await.states.len();
      max = num_members * 2;

      if max < self.opts.min_queue_depth {
        max = self.opts.min_queue_depth;
      }
    }
    max
  }
}

// ---------------------------------Hanlders Methods-------------------------------
impl<T, D> Serf<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  /// Called when a user event broadcast is
  /// received. Returns if the message should be rebroadcast.
  pub(crate) async fn handle_user_event(
    &self,
    msg: Either<UserEventMessageRef<'_>, UserEventMessage>,
  ) -> bool {
    let (ltime, name) = match &msg {
      Either::Left(msg) => (msg.ltime(), msg.name()),
      Either::Right(msg) => (msg.ltime, msg.name.as_str()),
    };

    // Witness a potentially newer time
    self.inner.event_clock.witness(ltime);

    let mut el = self.inner.event_core.write().await;

    // Ignore if it is before our minimum event time
    if ltime < el.min_time {
      return false;
    }

    // Check if this message is too old
    let bltime = LamportTime::new(el.buffer.len() as u64);
    let cur_time = self.inner.event_clock.time();
    if cur_time > bltime && ltime < cur_time - bltime {
      tracing::warn!(
        "serf: received old event {} from time {} (current: {})",
        name,
        ltime,
        cur_time
      );
      return false;
    }

    // Check if we've already seen this
    let idx = u64::from(ltime % bltime) as usize;
    let seen: Option<&mut UserEvents> = el.buffer[idx].as_mut();

    let msg = match msg {
      Either::Left(msg) => match UserEventMessage::from_ref(msg) {
        Ok(msg) => msg,
        Err(e) => {
          tracing::warn!("serf: failed to decode user event message: {}", e);
          return false;
        }
      },
      Either::Right(msg) => msg,
    };

    let user_event = UserEvent {
      name: msg.name.clone(),
      payload: msg.payload.clone(),
    };
    if let Some(seen) = seen {
      for prev in seen.events.iter() {
        if user_event.eq(prev) {
          return false;
        }
      }
      seen.events.push(user_event);
    } else {
      el.buffer[idx] = Some(UserEvents {
        ltime,
        events: OneOrMore::from(user_event),
      });
    }

    #[cfg(feature = "metrics")]
    {
      metrics::counter!(
        "serf.events",
        self.inner.opts.memberlist_options.metric_labels().iter()
      )
      .increment(1);

      // TODO: how to avoid allocating here?
      let named = format!("serf.events.{}", msg.name);
      metrics::counter!(
        named,
        self.inner.opts.memberlist_options.metric_labels().iter()
      )
      .increment(1);
    }

    if let Err(e) = self.inner.event_tx.send(msg.into()).await {
      tracing::error!("serf: failed to send user event: {}", e);
    }

    true
  }

  #[allow(clippy::too_many_arguments)]
  pub(crate) fn query_event(
    &self,
    ltime: LamportTime,
    name: SmolStr,
    payload: Bytes,
    timeout: Duration,
    id: u32,
    from: Node<T::Id, T::ResolvedAddress>,
    relay_factor: u8,
  ) -> QueryEvent<T, D> {
    QueryEvent {
      ltime,
      name,
      payload,
      ctx: Arc::new(QueryContext {
        query_timeout: timeout,
        span: Mutex::new(Some(Epoch::now())),
        this: self.clone(),
      }),
      id,
      from,
      relay_factor,
    }
  }

  pub(crate) async fn internal_query(
    &self,
    name: SmolStr,
    payload: Bytes,
    params: Option<QueryParam<T::Id>>,
    ty: InternalQueryEvent<T::Id>,
  ) -> Result<QueryResponse<T::Id, T::ResolvedAddress>, Error<T, D>> {
    self.query_in(name, payload, params, Some(ty)).await
  }

  pub(crate) async fn query_in(
    &self,
    name: SmolStr,
    payload: Bytes,
    params: Option<QueryParam<T::Id>>,
    ty: Option<InternalQueryEvent<T::Id>>,
  ) -> Result<QueryResponse<T::Id, T::ResolvedAddress>, Error<T, D>> {
    // Provide default parameters if none given.
    let params = match params {
      Some(params) if params.timeout != Duration::ZERO => params,
      Some(mut params) => {
        params.timeout = self.default_query_timeout().await;
        params
      }
      None => self.default_query_param().await,
    };

    // Get the local node
    let local = self.inner.memberlist.advertise_node();

    // Setup the flags
    let flags = if params.request_ack {
      QueryFlag::ACK
    } else {
      QueryFlag::empty()
    };

    // Create the message
    let q = QueryMessage {
      ltime: self.inner.query_clock.time(),
      id: rand::random(),
      from: local.cheap_clone(),
      filters: params.filters,
      flags,
      relay_factor: params.relay_factor,
      timeout: params.timeout,
      name: name.clone(),
      payload,
    };

    // Encode the query
    let len = serf_proto::Encodable::encoded_len(&q);

    // Check the size
    if len > self.inner.opts.query_size_limit {
      return Err(Error::query_too_large(len));
    }

    let raw = serf_proto::Encodable::encode_to_bytes(&q)?;

    // Register QueryResponse to track acks and responses
    let resp = QueryResponse::from_query(&q, self.inner.memberlist.num_online_members().await);
    self
      .register_query_response(params.timeout, resp.clone())
      .await;

    // Process query locally
    self.handle_query(Either::Right(q), ty).await?;

    // Start broadcasting the event
    self
      .inner
      .query_broadcasts
      .queue_broadcast(SerfBroadcast {
        msg: raw,
        notify_tx: None,
      })
      .await;
    Ok(resp)
  }

  /// Used to setup the listeners for the query,
  /// and to schedule closing the query after the timeout.
  pub(crate) async fn register_query_response(
    &self,
    timeout: Duration,
    resp: QueryResponse<T::Id, T::ResolvedAddress>,
  ) {
    let tresps = self.inner.query_core.clone();
    let mut resps = self.inner.query_core.write().await;
    // Map the LTime to the QueryResponse. This is necessarily 1-to-1,
    // since we increment the time for each new query.
    let ltime = resp.ltime;
    resps.responses.insert(ltime, resp);

    // Setup a timer to close the response and deregister after the timeout
    <T::Runtime as RuntimeLite>::spawn_after(timeout, async move {
      let mut resps = tresps.write().await;
      if let Some(resp) = resps.responses.remove(&ltime) {
        resp.close().await;
      }
    })
    .detach();
  }

  /// Called when a query broadcast is
  /// received. Returns if the message should be rebroadcast.
  pub(crate) async fn handle_query(
    &self,
    q: Either<
      <QueryMessage<T::Id, T::ResolvedAddress> as Data>::Ref<'_>,
      QueryMessage<T::Id, T::ResolvedAddress>,
    >,
    ty: Option<InternalQueryEvent<T::Id>>,
  ) -> Result<bool, memberlist_core::proto::DecodeError> {
    let (qm_ltime, qm_id, qm_timeout, no_broadcast, ack, name, filters) = match q.as_ref() {
      Either::Left(q) => (
        q.ltime(),
        q.id(),
        q.timeout(),
        q.no_broadcast(),
        q.ack(),
        q.name(),
        Either::Left(*q.filters()),
      ),
      Either::Right(q) => (
        q.ltime,
        q.id(),
        q.timeout(),
        q.no_broadcast(),
        q.ack(),
        q.name.as_str(),
        Either::Right(q.filters.as_slice()),
      ),
    };

    // Witness a potentially newer time
    self.inner.query_clock.witness(qm_ltime);

    let mut query = self.inner.query_core.write().await;

    // Ignore if it is before our minimum query time
    if qm_ltime < query.min_time {
      return Ok(false);
    }

    // Check if this message is too old
    let cur_time = self.inner.query_clock.time();
    let q_time = LamportTime::new(query.buffer.len() as u64);
    if cur_time > q_time && q_time < cur_time - q_time {
      tracing::warn!(
        "serf: received old query {} from time {} (current: {})",
        name,
        qm_ltime,
        cur_time
      );
      return Ok(false);
    }

    // Check if we've already seen this
    let idx = u64::from(qm_ltime % q_time) as usize;
    let seen = query.buffer[idx].as_mut();
    if let Some(seen) = seen {
      if seen.ltime == qm_ltime {
        for &prev in seen.query_ids.iter() {
          if qm_id == prev {
            // Seen this ID already
            return Ok(false);
          }
        }
      }
      seen.query_ids.push(qm_id);
    } else {
      query.buffer[idx] = Some(Queries {
        ltime: qm_ltime,
        query_ids: MediumVec::from(qm_id),
      });
    }

    // update some metrics
    #[cfg(feature = "metrics")]
    {
      metrics::counter!(
        "serf.queries",
        self.inner.opts.memberlist_options.metric_labels().iter()
      )
      .increment(1);

      // TODO: how to avoid allocating here?
      let named = format!("serf.queries.{}", name);
      metrics::counter!(
        named,
        self.inner.opts.memberlist_options.metric_labels().iter()
      )
      .increment(1);
    }

    // Check if we should rebroadcast, this may be disabled by a flag
    let mut rebroadcast = true;
    if no_broadcast {
      rebroadcast = false;
    }

    // Filter the query
    if !self.should_process_query(filters)? {
      // Even if we don't process it further, we should rebroadcast,
      // since it is the first time we've seen this.
      return Ok(rebroadcast);
    }

    // Send ack if requested, without waiting for client to respond()
    let (name, payload, from, relay_factor) = if ack {
      let ack = QueryResponseMessage {
        ltime: qm_ltime,
        id: qm_id,
        from: self.inner.memberlist.advertise_node(),
        flags: QueryFlag::ACK,
        payload: Bytes::new(),
      };

      match serf_proto::Encodable::encode_to_bytes(&ack) {
        Ok(raw) => {
          let (name, payload, from, relay_factor) = match q {
            Either::Left(q) => (
              SmolStr::new(q.name()),
              Bytes::copy_from_slice(q.payload()),
              Node::from_ref(*q.from())?,
              q.relay_factor(),
            ),
            Either::Right(q) => (q.name, q.payload, q.from, q.relay_factor),
          };

          if let Err(e) = self.inner.memberlist.send(from.address(), raw).await {
            tracing::error!(err=%e, "serf: failed to send ack");
          }

          if let Err(e) = self.relay_response(relay_factor, from.clone(), ack).await {
            tracing::error!(err=%e, "serf: failed to relay ack");
          }
          (name, payload, from, relay_factor)
        }
        Err(e) => {
          tracing::error!(err=%e, "serf: failed to format ack");
          match q {
            Either::Left(q) => (
              SmolStr::new(q.name()),
              Bytes::copy_from_slice(q.payload()),
              Node::from_ref(*q.from())?,
              q.relay_factor(),
            ),
            Either::Right(q) => (q.name, q.payload, q.from, q.relay_factor),
          }
        }
      }
    } else {
      match q {
        Either::Left(q) => (
          SmolStr::new(q.name()),
          Bytes::copy_from_slice(q.payload()),
          Node::from_ref(*q.from())?,
          q.relay_factor(),
        ),
        Either::Right(q) => (q.name, q.payload, q.from, q.relay_factor),
      }
    };

    let ev = self.query_event(
      qm_ltime,
      name,
      payload,
      qm_timeout,
      qm_id,
      from,
      relay_factor,
    );

    if let Err(e) = self
      .inner
      .event_tx
      .send(match ty {
        Some(ty) => (ty, ev).into(),
        None => ev.into(),
      })
      .await
    {
      tracing::error!(err=%e, "serf: failed to send query");
    }

    Ok(rebroadcast)
  }

  /// Called when a query response is
  /// received.
  pub(crate) async fn handle_query_response(
    &self,
    resp: <QueryResponseMessage<T::Id, T::ResolvedAddress> as Data>::Ref<'_>,
  ) -> Result<(), memberlist_core::proto::DecodeError> {
    // Look for a corresponding QueryResponse
    let qc = self
      .inner
      .query_core
      .read()
      .await
      .responses
      .get(&resp.ltime())
      .cloned();
    if let Some(query) = qc {
      // Verify the ID matches
      if query.id != resp.id() {
        tracing::warn!(
          "serf: query reply ID mismatch (local: {}, response: {})",
          query.id,
          resp.id()
        );
        return Ok(());
      }

      let resp = QueryResponseMessage::<T::Id, T::ResolvedAddress>::from_ref(resp)?;

      query
        .handle_query_response::<T, D>(
          resp,
          self.local_id(),
          #[cfg(feature = "metrics")]
          self.inner.opts.memberlist_options.metric_labels(),
        )
        .await;
    } else {
      tracing::warn!(
        "serf: reply for non-running query (LTime: {}, ID: {}) From: {:?}",
        resp.ltime(),
        resp.id(),
        resp.from()
      );
    }

    Ok(())
  }

  /// Called when a node join event is received
  /// from memberlist.
  pub(crate) async fn handle_node_join(&self, n: Arc<NodeState<T::Id, T::ResolvedAddress>>) {
    let mut members = self.inner.members.write().await;

    #[cfg(any(test, feature = "test"))]
    {
      if let Some(ref dropper) = self.inner.memberlist.delegate().unwrap().message_dropper {
        if dropper.should_drop(MessageType::Join) {
          return;
        }
      }
    }

    let node = n.node();
    let tags = if !n.meta().is_empty() {
      match <Tags as Data>::decode(n.meta()) {
        Ok((readed, tags)) => {
          tracing::trace!(read = %readed, tags=?tags, "serf: decode tags successfully");
          tags
        }
        Err(e) => {
          tracing::error!(err=%e, "serf: failed to decode tags");
          return;
        }
      }
    } else {
      Default::default()
    };

    let (old_status, fut) = if let Some(member) = members.states.get_mut(node.id()) {
      let old_status = member.member.status;
      #[cfg(feature = "metrics")]
      let dead_time = member.leave_time.map(|t| t.elapsed());
      #[cfg(feature = "metrics")]
      if old_status == MemberStatus::Failed {
        if let Some(dead_time) = dead_time {
          if dead_time < self.inner.opts.flap_timeout {
            metrics::counter!(
              "serf.member.flap",
              self.inner.opts.memberlist_options.metric_labels().iter()
            )
            .increment(1);
          }
        }
      }

      *member = MemberState {
        member: Member {
          node: node.cheap_clone(),
          tags: Arc::new(tags),
          status: MemberStatus::Alive,
          protocol_version: member.member.protocol_version,
          delegate_version: member.member.delegate_version,
          memberlist_delegate_version: member.member.memberlist_delegate_version,
          memberlist_protocol_version: member.member.memberlist_protocol_version,
        },
        status_time: member.status_time,
        leave_time: None,
      };

      (
        old_status,
        self.inner.event_tx.send(
          MemberEvent {
            ty: MemberEventType::Join,
            members: Arc::new(TinyVec::from(member.member.clone())),
          }
          .into(),
        ),
      )
    } else {
      // Check if we have a join or leave intent. The intent buffer
      // will only hold one event for this node, so the more recent
      // one will take effect.
      let mut status = MemberStatus::Alive;
      let mut status_ltime = LamportTime::new(0);
      if let Some(t) = recent_intent(&members.recent_intents, n.id(), MessageType::Join) {
        status_ltime = t;
      }

      if let Some(t) = recent_intent(&members.recent_intents, n.id(), MessageType::Leave) {
        status_ltime = t;
        status = MemberStatus::Leaving;
      }

      let ms = MemberState {
        member: Member {
          node: node.cheap_clone(),
          tags: Arc::new(tags),
          status,
          protocol_version: self.inner.opts.protocol_version,
          delegate_version: self.inner.opts.delegate_version,
          memberlist_delegate_version: self.inner.opts.memberlist_options.delegate_version(),
          memberlist_protocol_version: self.inner.opts.memberlist_options.protocol_version(),
        },
        status_time: status_ltime,
        leave_time: None,
      };
      let member = ms.member.clone();
      members.states.insert(node.id().cheap_clone(), ms);
      (
        MemberStatus::None,
        self.inner.event_tx.send(
          MemberEvent {
            ty: MemberEventType::Join,
            members: Arc::new(TinyVec::from(member)),
          }
          .into(),
        ),
      )
    };

    if matches!(old_status, MemberStatus::Failed | MemberStatus::Left) {
      remove_old_member(&mut members.failed_members, node.id());
      remove_old_member(&mut members.left_members, node.id());
    }

    // update some metrics
    #[cfg(feature = "metrics")]
    metrics::counter!(
      "serf.member.join",
      self.inner.opts.memberlist_options.metric_labels().iter()
    )
    .increment(1);

    tracing::info!("serf: member join: {}", node);
    if let Err(e) = fut.await {
      tracing::error!(err=%e, "serf: failed to send member event");
    }
  }

  /// Called when a node broadcasts a
  /// join message to set the lamport time of its join
  pub(crate) async fn handle_node_join_intent(&self, join_msg: &JoinMessage<T::Id>) -> bool {
    // Witness a potentially newer time
    self.inner.clock.witness(join_msg.ltime);

    let mut members = self.inner.members.write().await;
    match members.states.get_mut(join_msg.id()) {
      Some(member) => {
        // Check if this time is newer than what we have
        if join_msg.ltime <= member.status_time {
          return false;
        }

        // Update the LTime
        member.status_time = join_msg.ltime;

        // If we are in the leaving state, we should go back to alive,
        // since the leaving message must have been for an older time

        if member.member.status == MemberStatus::Leaving {
          member.member.status = MemberStatus::Alive;
        }

        true
      }
      None => {
        // Rebroadcast only if this was an update we hadn't seen before.
        upsert_intent(
          &mut members.recent_intents,
          join_msg.id(),
          MessageType::Join,
          join_msg.ltime,
          Epoch::now,
        )
      }
    }
  }

  pub(crate) async fn handle_node_leave(&self, n: Arc<NodeState<T::Id, T::ResolvedAddress>>) {
    let mut members = self.inner.members.write().await;

    let Some(member_state) = members.states.get_mut(n.id()) else {
      return;
    };

    let mut ms = member_state.member.status;
    let member = match ms {
      MemberStatus::Leaving => {
        member_state.member.status = MemberStatus::Left;

        ms = MemberStatus::Left;
        member_state.leave_time = Some(Epoch::now());
        let member_state = member_state.clone();
        let member = member_state.member.clone();
        members.left_members.push(member_state);
        member
      }
      MemberStatus::Alive => {
        member_state.member.status = MemberStatus::Failed;
        ms = MemberStatus::Failed;
        member_state.leave_time = Some(Epoch::now());
        let member_state = member_state.clone();
        let member = member_state.member.clone();
        members.failed_members.push(member_state);
        member
      }
      _ => {
        tracing::warn!("serf: bad state when leave: {}", ms);
        return;
      }
    };

    // Send an event along
    let ty = if ms != MemberStatus::Left {
      MemberEventType::Failed
    } else {
      MemberEventType::Leave
    };

    // Update some metrics
    #[cfg(feature = "metrics")]
    metrics::counter!(
      "serf.member.leave",
      self.inner.opts.memberlist_options.metric_labels().iter()
    )
    .increment(1);

    tracing::info!("serf: {}: {}", ty.as_str(), member.node());

    if let Err(e) = self
      .inner
      .event_tx
      .send(
        MemberEvent {
          ty,
          members: Arc::new(TinyVec::from(member)),
        }
        .into(),
      )
      .await
    {
      tracing::error!(err=%e, "serf: failed to send member event: {}", e);
    }
  }

  pub(crate) async fn handle_node_leave_intent(&self, msg: &LeaveMessage<T::Id>) -> bool {
    let state = self.state();

    // Witness a potentially newer time
    self.inner.clock.witness(msg.ltime);

    let mut members = self.inner.members.write().await;

    if !members.states.contains_key(msg.id()) {
      return upsert_intent(
        &mut members.recent_intents,
        msg.id(),
        MessageType::Leave,
        msg.ltime,
        Epoch::now,
      );
    }

    let members = atomic_refcell::AtomicRefCell::new(&mut *members);
    let mut members_mut = members.borrow_mut();
    let member = members_mut.states.get_mut(msg.id()).unwrap();
    // If the message is old, then it is irrelevant and we can skip it
    if msg.ltime <= member.status_time {
      return false;
    }

    // Refute us leaving if we are in the alive state
    // Must be done in another goroutine since we have the memberLock
    if msg.id().eq(self.inner.memberlist.local_id()) && state == SerfState::Alive {
      tracing::debug!("serf: refuting an older leave intent");
      let this = self.clone();
      let ltime = self.inner.clock.time();
      <T::Runtime as RuntimeLite>::spawn_detach(async move {
        if let Err(e) = this.broadcast_join(ltime).await {
          tracing::error!(err=%e, "serf: failed to broadcast join");
        }
      });
      return false;
    }

    // Always update the lamport time even when the status does not change
    // (despite the variable naming implying otherwise).
    //
    // By updating this statusLTime here we ensure that the earlier conditional
    // on "leaveMsg.LTime <= member.statusLTime" will prevent an infinite
    // rebroadcast when seeing two successive leave message for the same
    // member. Without this fix a leave message that arrives after a member is
    // already marked as leaving/left will cause it to be rebroadcast without
    // marking it locally as witnessed. If more than one serf instance in the
    // cluster experiences this series of events then they will rebroadcast
    // each other's messages about the affected node indefinitely.
    //
    // This eventually leads to overflowing serf intent queues
    // - https://github.com/hashicorp/consul/issues/8179
    // - https://github.com/hashicorp/consul/issues/7960
    member.status_time = msg.ltime;

    // State transition depends on current state
    match member.member.status {
      MemberStatus::None => false,
      MemberStatus::Alive => {
        member.member.status = MemberStatus::Leaving;

        if msg.prune {
          let owned = member.clone();
          drop(members_mut);
          self.handle_prune(&owned, *members.borrow_mut()).await;
        }
        true
      }
      MemberStatus::Leaving | MemberStatus::Left => {
        if msg.prune {
          let owned = member.clone();
          drop(members_mut);
          self.handle_prune(&owned, *members.borrow_mut()).await;
        }
        true
      }
      MemberStatus::Failed => {
        member.member.status = MemberStatus::Left;
        let owned = member.clone();
        drop(members_mut);

        let mut members_mut = members.borrow_mut();
        // Remove from the failed list and add to the left list. We add
        // to the left list so that when we do a sync, other nodes will
        // remove it from their failed list.
        members_mut
          .failed_members
          .retain(|m| m.member.node.id().ne(owned.member.node.id()));
        members_mut.left_members.push(owned.clone());

        // We must push a message indicating the node has now
        // left to allow higher-level applications to handle the
        // graceful leave.
        tracing::info!("serf: EventMemberLeave (forced): {}", owned.member.node);

        if let Err(e) = self
          .inner
          .event_tx
          .send(
            MemberEvent {
              ty: MemberEventType::Leave,
              members: Arc::new(TinyVec::from(owned.member.clone())),
            }
            .into(),
          )
          .await
        {
          tracing::error!(err=%e, "serf: failed to send member event");
        }

        if msg.prune {
          self.handle_prune(&owned, *members_mut).await;
        }

        true
      }
      status => {
        tracing::warn!(status=%status, "serf: received leave intent for unknown member status");
        member.member.status = MemberStatus::Leaving;

        if msg.prune {
          let owned = member.clone();
          drop(members_mut);
          self.handle_prune(&owned, *members.borrow_mut()).await;
        }
        true
      }
    }
  }

  /// Called when a node meta data update
  /// has taken place
  pub(crate) async fn handle_node_update(&self, n: Arc<NodeState<T::Id, T::ResolvedAddress>>) {
    let tags = match <Tags as Data>::decode(n.meta()) {
      Ok((readed, tags)) => {
        tracing::trace!(read = %readed, tags=?tags, "serf: decode tags successfully");
        tags
      }
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to decode tags");
        return;
      }
    };
    let mut members = self.inner.members.write().await;
    let id = n.id();
    if let Some(ms) = members.states.get_mut(id) {
      // Update the member attributes
      ms.member = Member {
        node: n.node(),
        tags: Arc::new(tags),
        status: ms.member.status,
        protocol_version: ProtocolVersion::V1,
        delegate_version: DelegateVersion::V1,
        memberlist_delegate_version: MemberlistDelegateVersion::V1,
        memberlist_protocol_version: MemberlistProtocolVersion::V1,
      };

      #[cfg(feature = "metrics")]
      metrics::counter!(
        "serf.member.update",
        self.inner.opts.memberlist_options.metric_labels().iter()
      )
      .increment(1);

      tracing::info!("serf: member update: {}", id);
      if let Err(e) = self
        .inner
        .event_tx
        .send(
          MemberEvent {
            ty: MemberEventType::Update,
            members: Arc::new(TinyVec::from(ms.member.clone())),
          }
          .into(),
        )
        .await
      {
        tracing::error!(err=%e, "serf: failed to send member event");
      }
    }
  }

  /// Waits for nodes that are leaving and then forcibly
  /// erases a member from the list of members
  pub(crate) async fn handle_prune(
    &self,
    member: &MemberState<T::Id, T::ResolvedAddress>,
    members: &mut Members<T::Id, T::ResolvedAddress>,
  ) {
    let ms = member.member.status;
    if ms == MemberStatus::Leaving {
      <T::Runtime as RuntimeLite>::sleep(
        self.inner.opts.broadcast_timeout + self.inner.opts.leave_propagate_delay,
      )
      .await;
    }

    let node = member.member.node();
    let id = node.id();
    tracing::info!("serf: EventMemberReap (forced): {}", node);

    // If we are leaving or left we may be in that list of members
    if matches!(ms, MemberStatus::Leaving | MemberStatus::Left) {
      remove_old_member(&mut members.left_members, id);
    }

    let tx = &self.inner.event_tx;
    let coord = self.inner.coord_core.as_deref();
    erase_node!(tx <- coord(members[id].member))
  }

  /// Invoked when a join detects a conflict over a name.
  /// This means two different nodes (IP/Port) are claiming the same name. Memberlist
  /// will reject the "new" node mapping, but we can still be notified.
  pub(crate) async fn handle_node_conflict(
    &self,
    existing: Arc<NodeState<T::Id, T::ResolvedAddress>>,
    other: Arc<NodeState<T::Id, T::ResolvedAddress>>,
  ) {
    // Log a basic warning if the node is not us...
    if existing.id() != self.inner.memberlist.local_id() {
      tracing::warn!(
        "serf: node conflict detected between {}({}) and {}({})",
        existing.id(),
        existing.address(),
        other.id(),
        other.address(),
      );
      return;
    }

    // The current node is conflicting! This is an error
    tracing::error!(
      "serf: node id conflicts with another node at {}. node id must be unique! (resolution enabled: {})",
      other.id(),
      self.inner.opts.enable_id_conflict_resolution
    );

    // If automatic resolution is enabled, kick off the resolution
    if self.inner.opts.enable_id_conflict_resolution {
      let this = self.clone();
      <T::Runtime as RuntimeLite>::spawn_detach(async move { this.resolve_node_conflict().await });
    }
  }

  /// Used to determine which node should remain during
  /// a name conflict. This is done by running an internal query.
  async fn resolve_node_conflict(&self) {
    // Get the local node
    let local_id = self.inner.memberlist.local_id();
    let local_advertise_addr = self.inner.memberlist.advertise_address();

    let payload = match local_id.encode_to_bytes() {
      Ok(id) => id,
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to encode local id");
        return;
      }
    };

    // Start an id resolution query
    let ty = InternalQueryEvent::Conflict(local_id.clone());
    let resp = match self
      .internal_query(SmolStr::new(ty.as_str()), payload, None, ty)
      .await
    {
      Ok(resp) => resp,
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to start node id resolution query");
        return;
      }
    };

    // Counter to determine winner
    let mut responses = 0usize;
    let mut matching = 0usize;

    // Gather responses
    let resp_rx = resp.response_rx();
    while let Ok(r) = resp_rx.recv().await {
      let res = serf_proto::decode_message::<T::Id, T::ResolvedAddress>(&r.payload);
      match res {
        Ok(msg) => {
          match msg {
            MessageRef::ConflictResponse(resp) => {
              // Update the counters
              responses += 1;
              match <T::ResolvedAddress as Data>::from_ref(*resp.member().node().address()) {
                Ok(addr) => {
                  if addr.eq(local_advertise_addr) {
                    matching += 1;
                  }
                }
                Err(e) => {
                  tracing::error!(err=%e, "serf: failed to decode conflict query response");
                  continue;
                }
              }
            }
            msg => {
              tracing::warn!(
                type = %msg.ty(),
                "serf: invalid conflict query response type",
              );
              continue;
            }
          }
        }
        Err(e) => {
          tracing::error!(err=%e, "serf: failed to decode conflict query response");
          continue;
        }
      }
    }

    // Query over, determine if we should live
    let majority = (responses / 2) + 1;
    if matching >= majority {
      tracing::info!(
        "serf: majority in node id conflict resolution [{} / {}]",
        matching,
        responses
      );
      return;
    }

    // Since we lost the vote, we need to exit
    tracing::warn!(
      "serf: minority in name conflict resolution, quiting [{} / {}]",
      matching,
      responses
    );

    if let Err(e) = self.shutdown().await {
      tracing::error!(err=%e, "serf: failed to shutdown");
    }
  }

  pub(crate) fn handle_rejoin(
    memberlist: Memberlist<T, SerfDelegate<T, D>>,
    alive_nodes: TinyVec<Node<T::Id, MaybeResolvedAddress<T>>>,
  ) {
    <T::Runtime as RuntimeLite>::spawn_detach(async move {
      for prev in alive_nodes {
        // Do not attempt to join ourself
        if prev.id().eq(memberlist.local_id()) {
          continue;
        }

        tracing::info!("serf: attempting re-join to previously known node {}", prev);
        if let Err(e) = memberlist.join(prev.cheap_clone()).await {
          tracing::warn!(
            "serf: failed to re-join to previously known node {}: {}",
            prev,
            e
          );
        } else {
          tracing::info!("serf: re-joined to previously known node: {}", prev);
          return;
        }
      }

      tracing::warn!("serf: failed to re-join to any previously known node");
    });
  }
}

/// Used to remove an old member from a list of old
/// members.
fn remove_old_member<I: Eq, A>(old: &mut OneOrMore<MemberState<I, A>>, id: &I) {
  old.retain(|m| m.member.node.id() != id);
}

/// Clears out any intents that are older than the timeout. Make sure
/// the memberLock is held when passing in the Serf instance's recentIntents
/// member.
fn reap_intents<I>(intents: &mut HashMap<I, NodeIntent>, now: Epoch, timeout: Duration) {
  intents.retain(|_, intent| (now - intent.wall_time) <= timeout);
}

fn recent_intent<I: core::hash::Hash + Eq>(
  intents: &HashMap<I, NodeIntent>,
  id: &I,
  ty: MessageType,
) -> Option<LamportTime> {
  match intents.get(id) {
    Some(intent) if intent.ty == ty => Some(intent.ltime),
    _ => None,
  }
}

fn upsert_intent<I>(
  intents: &mut HashMap<I, NodeIntent>,
  node: &I,
  t: MessageType,
  ltime: LamportTime,
  stamper: impl FnOnce() -> Epoch,
) -> bool
where
  I: CheapClone + Eq + core::hash::Hash,
{
  match intents.entry(node.cheap_clone()) {
    std::collections::hash_map::Entry::Occupied(mut ent) => {
      let intent = ent.get_mut();
      if ltime > intent.ltime {
        intent.ty = t;
        intent.ltime = ltime;
        intent.wall_time = stamper();
        true
      } else {
        false
      }
    }
    std::collections::hash_map::Entry::Vacant(ent) => {
      ent.insert(NodeIntent {
        ty: t,
        wall_time: stamper(),
        ltime,
      });
      true
    }
  }
}
