use std::sync::atomic::Ordering;

use futures::{FutureExt, StreamExt};
use memberlist_core::{
  bytes::{BufMut, Bytes, BytesMut}, proto::Data, tracing, transport::{MaybeResolvedAddress, Node}, types::{Meta, OneOrMore, SmallVec}, CheapClone
};
use smol_str::SmolStr;

use crate::{
  delegate::,
  error::{Error, JoinError},
  event::EventProducer,
  types::{LeaveMessage, Member, MessageType, SerfMessage, Tags, UserEventMessage},
};

use super::*;

impl<T> Serf<T>
where
  T: Transport,
{
  /// Creates a new Serf instance with the given transport and options.
  pub async fn new(
    transport: T::Options,
    opts: Options,
  ) -> Result<Self, Error<T, DefaultDelegate<T>>> {
    Self::new_in(
      None,
      None,
      transport,
      opts,
      #[cfg(any(test, feature = "test"))]
      None,
    )
    .await
  }

  /// Creates a new Serf instance with the given transport and options.
  pub async fn with_event_producer(
    transport: T::Options,
    opts: Options,
    ev: EventProducer<T, DefaultDelegate<T>>,
  ) -> Result<Self, Error<T, DefaultDelegate<T>>> {
    Self::new_in(
      Some(ev.tx),
      None,
      transport,
      opts,
      #[cfg(any(test, feature = "test"))]
      None,
    )
    .await
  }
}

impl<T, D> Serf<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  /// Creates a new Serf instance with the given transport and options.
  pub async fn with_delegate(
    transport: T::Options,
    opts: Options,
    delegate: D,
  ) -> Result<Self, Error<T, D>> {
    Self::new_in(
      None,
      Some(delegate),
      transport,
      opts,
      #[cfg(any(test, feature = "test"))]
      None,
    )
    .await
  }

  /// Creates a new Serf instance with the given transport, options, event sender, and delegate.
  pub async fn with_event_producer_and_delegate(
    transport: T::Options,
    opts: Options,
    ev: EventProducer<T, D>,
    delegate: D,
  ) -> Result<Self, Error<T, D>> {
    Self::new_in(
      Some(ev.tx),
      Some(delegate),
      transport,
      opts,
      #[cfg(any(test, feature = "test"))]
      None,
    )
    .await
  }

  /// Returns the local node's ID
  #[inline]
  pub fn local_id(&self) -> &T::Id {
    self.inner.memberlist.local_id()
  }

  /// Returns the local node's ID and the advertised address
  #[inline]
  pub fn advertise_node(&self) -> Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress> {
    self.inner.memberlist.advertise_node()
  }

  /// A predicate that determines whether or not encryption
  /// is enabled, which can be possible in one of 2 cases:
  ///   - Single encryption key passed at agent start (no persistence)
  ///   - Keyring file provided at agent start
  #[inline]
  #[cfg(feature = "encryption")]
  #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
  pub fn encryption_enabled(&self) -> bool {
    self.inner.memberlist.encryption_enabled()
  }

  /// Returns a receiver that can be used to wait for
  /// Serf to shutdown.
  #[inline]
  pub fn shutdown_rx(&self) -> async_channel::Receiver<()> {
    self.inner.shutdown_rx.clone()
  }

  /// The current state of this Serf instance.
  #[inline]
  pub fn state(&self) -> SerfState {
    *self.inner.state.lock()
  }

  /// Returns a point-in-time snapshot of the members of this cluster.
  #[inline]
  pub async fn members(
    &self,
  ) -> OneOrMore<Member<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>> {
    self
      .inner
      .members
      .read()
      .await
      .states
      .values()
      .map(|s| s.member.cheap_clone())
      .collect()
  }

  /// Used to provide operator debugging information
  #[inline]
  pub async fn stats(&self) -> Stats {
    let (num_members, num_failed, num_left, health_score) = {
      let members = self.inner.members.read().await;
      let num_members = members.states.len();
      let num_failed = members.failed_members.len();
      let num_left = members.left_members.len();
      let health_score = self.inner.memberlist.health_score();
      (num_members, num_failed, num_left, health_score)
    };

    #[cfg(not(feature = "encryption"))]
    let encrypted = false;
    #[cfg(feature = "encryption")]
    let encrypted = self.inner.memberlist.encryption_enabled();

    Stats {
      members: num_members,
      failed: num_failed,
      left: num_left,
      health_score,
      member_time: self.inner.clock.time().into(),
      event_time: self.inner.event_clock.time().into(),
      query_time: self.inner.query_clock.time().into(),
      intent_queue: self.inner.broadcasts.num_queued().await,
      event_queue: self.inner.event_broadcasts.num_queued().await,
      query_queue: self.inner.query_broadcasts.num_queued().await,
      encrypted,
      coordinate_resets: self
        .inner
        .coord_core
        .as_ref()
        .map(|coord| coord.client.stats().resets),
    }
  }

  /// Returns the number of nodes in the serf cluster, regardless of
  /// their health or status.
  #[inline]
  pub async fn num_members(&self) -> usize {
    self.inner.members.read().await.states.len()
  }

  /// Returns the key manager for the current serf instance
  #[cfg(feature = "encryption")]
  #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
  #[inline]
  pub fn key_manager(&self) -> &crate::key_manager::KeyManager<T, D> {
    &self.inner.key_manager
  }

  /// Returns the Member information for the local node
  #[inline]
  pub async fn local_member(
    &self,
  ) -> Member<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress> {
    self
      .inner
      .members
      .read()
      .await
      .states
      .get(self.inner.memberlist.local_id())
      .unwrap()
      .member
      .cheap_clone()
  }

  /// Used to dynamically update the tags associated with
  /// the local node. This will propagate the change to the rest of
  /// the cluster. Blocks until a the message is broadcast out.
  #[inline]
  pub async fn set_tags(&self, tags: Tags) -> Result<(), Error<T, D>> {
    // Check that the meta data length is okay
    let tags_encoded_len = tags.encoded_len_with_length_delimited();
    if tags_encoded_len > Meta::MAX_SIZE {
      return Err(Error::tags_too_large(tags_encoded_len));
    }
    // update the config
    self.inner.opts.tags.store(Arc::new(tags));

    // trigger a memberlist update
    self
      .inner
      .memberlist
      .update_node(self.inner.opts.broadcast_timeout)
      .await
      .map_err(From::from)
  }

  /// Used to broadcast a custom user event with a given
  /// name and payload. If the configured size limit is exceeded and error will be returned.
  /// If coalesce is enabled, nodes are allowed to coalesce this event.
  #[inline]
  pub async fn user_event(
    &self,
    name: impl Into<SmolStr>,
    payload: impl Into<Bytes>,
    coalesce: bool,
  ) -> Result<(), Error<T, D>> {
    let name: SmolStr = name.into();
    let payload: Bytes = payload.into();
    let payload_size_before_encoding = name.len() + payload.len();

    // Check size before encoding to prevent needless encoding and return early if it's over the specified limit.
    if payload_size_before_encoding > self.inner.opts.max_user_event_size {
      return Err(Error::user_event_limit_too_large(
        self.inner.opts.max_user_event_size,
      ));
    }

    if payload_size_before_encoding > USER_EVENT_SIZE_LIMIT {
      return Err(Error::user_event_too_large(USER_EVENT_SIZE_LIMIT));
    }

    // Create a message
    let msg = UserEventMessage {
      ltime: self.inner.event_clock.time(),
      name: name.clone(),
      payload,
      cc: coalesce,
    };

    // Start broadcasting the event
    let len = <D as >::message_encoded_len(&msg);

    // Check the size after encoding to be sure again that
    // we're not attempting to send over the specified size limit.
    if len > self.inner.opts.max_user_event_size {
      return Err(Error::raw_user_event_too_large(len));
    }

    if len > USER_EVENT_SIZE_LIMIT {
      return Err(Error::raw_user_event_too_large(len));
    }

    let mut raw = BytesMut::with_capacity(len + 1); // + 1 for message type byte
    raw.put_u8(MessageType::UserEvent as u8);
    raw.resize(len + 1, 0);

    let actual_encoded_len = <D as >::encode_message(&msg, &mut raw[1..])
      .map_err(Error::transform_delegate)?;
    debug_assert_eq!(
      actual_encoded_len, len,
      "expected encoded len {} mismatch the actual encoded len {}",
      len, actual_encoded_len
    );

    self.inner.event_clock.increment();

    // Process update locally
    self.handle_user_event(msg).await;

    self
      .inner
      .event_broadcasts
      .queue_broadcast(SerfBroadcast {
        msg: raw.freeze(),
        notify_tx: None,
      })
      .await;
    Ok(())
  }

  /// Used to broadcast a new query. The query must be fairly small,
  /// and an error will be returned if the size limit is exceeded. This is only
  /// available with protocol version 4 and newer. Query parameters are optional,
  /// and if not provided, a sane set of defaults will be used.
  pub async fn query(
    &self,
    name: impl Into<SmolStr>,
    payload: impl Into<Bytes>,
    params: Option<QueryParam<T::Id>>,
  ) -> Result<QueryResponse<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>, Error<T, D>>
  {
    self
      .query_in(name.into(), payload.into(), params, None)
      .await
  }

  /// Joins an existing Serf cluster. Returns the id of node
  /// successfully contacted. If `ignore_old` is true, then any
  /// user messages sent prior to the join will be ignored.
  pub async fn join(
    &self,
    node: Node<T::Id, MaybeResolvedAddress<T>>,
    ignore_old: bool,
  ) -> Result<Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>, Error<T, D>> {
    // Do a quick state check
    let current_state = self.state();
    if current_state != SerfState::Alive {
      return Err(Error::bad_join_status(current_state));
    }

    // Hold the joinLock, this is to make eventJoinIgnore safe
    let _join_lock = self.inner.join_lock.lock().await;

    // Ignore any events from a potential join. This is safe since we hold
    // the joinLock and nobody else can be doing a Join
    if ignore_old {
      self.inner.event_join_ignore.store(true, Ordering::SeqCst);
    }

    // Have memberlist attempt to join
    match self.inner.memberlist.join(node).await {
      Ok(node) => {
        // Start broadcasting the update
        if let Err(e) = self.broadcast_join(self.inner.clock.time()).await {
          if ignore_old {
            self.inner.event_join_ignore.store(false, Ordering::SeqCst);
          }
          return Err(e);
        }
        if ignore_old {
          self.inner.event_join_ignore.store(false, Ordering::SeqCst);
        }

        Ok(node)
      }
      Err(e) => {
        if ignore_old {
          self.inner.event_join_ignore.store(false, Ordering::SeqCst);
        }
        Err(Error::from(e))
      }
    }
  }

  /// Joins an existing Serf cluster. Returns the id of nodes
  /// successfully contacted. If `ignore_old` is true, then any
  /// user messages sent prior to the join will be ignored.
  pub async fn join_many(
    &self,
    existing: impl Iterator<Item = Node<T::Id, MaybeResolvedAddress<T>>>,
    ignore_old: bool,
  ) -> Result<
    SmallVec<Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>>,
    JoinError<T, D>,
  > {
    // Do a quick state check
    let current_state = self.state();
    if current_state != SerfState::Alive {
      return Err(JoinError {
        joined: SmallVec::new(),
        errors: existing
          .into_iter()
          .map(|node| (node, Error::bad_join_status(current_state)))
          .collect(),
        broadcast_error: None,
      });
    }

    // Hold the joinLock, this is to make eventJoinIgnore safe
    let _join_lock = self.inner.join_lock.lock().await;

    // Ignore any events from a potential join. This is safe since we hold
    // the joinLock and nobody else can be doing a Join
    if ignore_old {
      self.inner.event_join_ignore.store(true, Ordering::SeqCst);
    }

    // Have memberlist attempt to join
    match self.inner.memberlist.join_many(existing).await {
      Ok(joined) => {
        // Start broadcasting the update
        if let Err(e) = self.broadcast_join(self.inner.clock.time()).await {
          self.inner.event_join_ignore.store(false, Ordering::SeqCst);
          return Err(JoinError {
            joined,
            errors: Default::default(),
            broadcast_error: Some(e),
          });
        }
        self.inner.event_join_ignore.store(false, Ordering::SeqCst);
        Ok(joined)
      }
      Err(e) => {
        let (joined, errors) = e.into();
        // If we joined any nodes, broadcast the join message
        if !joined.is_empty() {
          // Start broadcasting the update
          if let Err(e) = self.broadcast_join(self.inner.clock.time()).await {
            self.inner.event_join_ignore.store(false, Ordering::SeqCst);
            return Err(JoinError {
              joined,
              errors: errors
                .into_iter()
                .map(|(addr, err)| (addr, err.into()))
                .collect(),
              broadcast_error: Some(e),
            });
          }

          self.inner.event_join_ignore.store(false, Ordering::SeqCst);
          Err(JoinError {
            joined,
            errors: errors
              .into_iter()
              .map(|(addr, err)| (addr, err.into()))
              .collect(),
            broadcast_error: None,
          })
        } else {
          self.inner.event_join_ignore.store(false, Ordering::SeqCst);
          Err(JoinError {
            joined,
            errors: errors
              .into_iter()
              .map(|(addr, err)| (addr, err.into()))
              .collect(),
            broadcast_error: None,
          })
        }
      }
    }
  }

  /// Gracefully exits the cluster. It is safe to call this multiple
  /// times.
  /// If the Leave broadcast timeout, Leave() will try to finish the sequence as best effort.
  pub async fn leave(&self) -> Result<(), Error<T, D>> {
    // Check the current state
    {
      let mut s = self.inner.state.lock();
      match *s {
        SerfState::Left => return Ok(()),
        SerfState::Leaving => return Err(Error::bad_leave_status(*s)),
        SerfState::Shutdown => return Err(Error::bad_leave_status(*s)),
        _ => {
          // Set the state to leaving
          *s = SerfState::Leaving;
        }
      }
    }

    // If we have a snapshot, mark we are leaving
    if let Some(ref snap) = self.inner.snapshot {
      snap.leave().await;
    }

    // Construct the message for the graceful leave
    let msg = LeaveMessage {
      ltime: self.inner.clock.time(),
      id: self.inner.memberlist.local_id().cheap_clone(),
      prune: false,
    };

    self.inner.clock.increment();

    // Process the leave locally
    self.handle_node_leave_intent(&msg).await;

    let msg = SerfMessage::Leave(msg);

    // Only broadcast the leave message if there is at least one
    // other node alive.
    if self.has_alive_members().await {
      let (notify_tx, notify_rx) = async_channel::bounded(1);
      self.broadcast(msg, Some(notify_tx)).await?;

      futures::select! {
        _ = notify_rx.recv().fuse() => {
          // We got a response, so we are done
        }
        _ = <T::Runtime as RuntimeLite>::sleep(self.inner.opts.broadcast_timeout).fuse() => {
          tracing::warn!("serf: timeout while waiting for graceful leave");
        }
      }
    }

    // Attempt the memberlist leave
    if let Err(e) = self
      .inner
      .memberlist
      .leave(self.inner.opts.broadcast_timeout)
      .await
    {
      tracing::warn!("serf: timeout waiting for leave broadcast: {}", e);
    }

    // Wait for the leave to propagate through the cluster. The broadcast
    // timeout is how long we wait for the message to go out from our own
    // queue, but this wait is for that message to propagate through the
    // cluster. In particular, we want to stay up long enough to service
    // any probes from other nodes before they learn about us leaving.
    <T::Runtime as RuntimeLite>::sleep(self.inner.opts.leave_propagate_delay).await;

    // Transition to Left only if we not already shutdown
    {
      let mut s = self.inner.state.lock();
      match *s {
        SerfState::Shutdown => {}
        _ => {
          *s = SerfState::Left;
        }
      }
    }
    Ok(())
  }

  /// Forcibly removes a failed node from the cluster
  /// immediately, instead of waiting for the reaper to eventually reclaim it.
  /// This also has the effect that Serf will no longer attempt to reconnect
  /// to this node.
  pub async fn remove_failed_node(&self, id: T::Id) -> Result<(), Error<T, D>> {
    self.force_leave(id, false).await
  }

  /// Forcibly removes a failed node from the cluster
  /// immediately, instead of waiting for the reaper to eventually reclaim it.
  /// This also has the effect that Serf will no longer attempt to reconnect
  /// to this node.
  pub async fn remove_failed_node_prune(&self, id: T::Id) -> Result<(), Error<T, D>> {
    self.force_leave(id, true).await
  }

  /// Forcefully shuts down the Serf instance, stopping all network
  /// activity and background maintenance associated with the instance.
  ///
  /// This is not a graceful shutdown, and should be preceded by a call
  /// to Leave. Otherwise, other nodes in the cluster will detect this node's
  /// exit as a node failure.
  ///
  /// It is safe to call this method multiple times.
  pub async fn shutdown(&self) -> Result<(), Error<T, D>> {
    {
      let mut s = self.inner.state.lock();
      match *s {
        SerfState::Shutdown => return Ok(()),
        SerfState::Left => {}
        _ => {
          tracing::warn!("serf: shutdown without a leave");
        }
      }

      // Wait to close the shutdown channel until after we've shut down the
      // memberlist and its associated network resources, since the shutdown
      // channel signals that we are cleaned up outside of Serf.
      *s = SerfState::Shutdown;
    }
    self.inner.memberlist.shutdown().await?;
    self.inner.shutdown_tx.close();

    // Wait for the snapshoter to finish if we have one
    if let Some(ref snap) = self.inner.snapshot {
      snap.wait().await;
    }

    loop {
      if let Ok(mut handles) = self.inner.handles.try_borrow_mut() {
        let mut futs = core::mem::take(&mut *handles);
        while futs.next().await.is_some() {}
        break;
      }
    }

    Ok(())
  }

  /// Returns the network coordinate of the local node.
  pub fn cooridate(&self) -> Result<Coordinate, Error<T, D>> {
    if let Some(ref coord) = self.inner.coord_core {
      return Ok(coord.client.get_coordinate());
    }

    Err(Error::coordinates_disabled())
  }

  /// Returns the network coordinate for the node with the given
  /// name. This will only be valid if `disable_coordinates` is set to `false`.
  pub fn cached_coordinate(&self, id: &T::Id) -> Result<Option<Coordinate>, Error<T, D>> {
    if let Some(ref coord) = self.inner.coord_core {
      return Ok(coord.cache.read().get(id).cloned());
    }

    Err(Error::coordinates_disabled())
  }

  /// Returns the underlying [`Memberlist`] instance
  #[inline]
  pub fn memberlist(&self) -> &Memberlist<T, SerfDelegate<T, D>> {
    &self.inner.memberlist
  }
}

#[viewit::viewit(vis_all = "", getters(vis_all = "pub", prefix = "get"), setters(skip))]
#[cfg_attr(feature = "async-graphql", derive(async_graphql::SimpleObject))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Stats {
  members: usize,
  failed: usize,
  left: usize,
  health_score: usize,
  member_time: u64,
  event_time: u64,
  query_time: u64,
  intent_queue: usize,
  event_queue: usize,
  query_queue: usize,
  encrypted: bool,
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  coordinate_resets: Option<usize>,
}
