use crate::{
  Serf,
  broadcast::SerfBroadcast,
  delegate::Delegate,
  error::{Error, SerfDelegateError, SerfError},
  event::QueryMessageExt,
  types::{
    DelegateVersion, JoinMessage, LamportTime, LeaveMessage, Member, MemberStatus,
    MemberlistDelegateVersion, MemberlistProtocolVersion, MessageRef, MessageType, ProtocolVersion,
    PushPullMessageBorrow, UserEventMessage,
  },
};

use std::{
  borrow::Cow,
  sync::{Arc, OnceLock, atomic::Ordering},
};

use arc_swap::ArcSwap;
use either::Either;
use indexmap::IndexSet;
use memberlist_core::{
  CheapClone, META_MAX_SIZE,
  bytes::{Buf, BufMut, Bytes, BytesMut},
  delegate::{
    AliveDelegate, ConflictDelegate, Delegate as MemberlistDelegate, EventDelegate,
    MergeDelegate as MemberlistMergeDelegate, NodeDelegate, PingDelegate,
  },
  proto::{Data, Meta, NodeState, SmallVec, State, TinyVec},
  tracing,
  transport::{AddressResolver, Transport},
};
use serf_proto::{PushPullMessage, Tags};

// PingVersion is an internal version for the ping message, above the normal
// versioning we get from the protocol version. This enables small updates
// to the ping message without a full protocol bump.
const PING_VERSION: u8 = 1;

#[cfg(any(test, feature = "test"))]
pub(crate) trait MessageDropper: Send + Sync + 'static {
  fn should_drop(&self, ty: MessageType) -> bool;
}

/// The memberlist delegate for Serf.
pub struct SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  serf: OnceLock<Serf<T, D>>,
  delegate: Option<D>,
  tags: Arc<ArcSwap<Tags>>,
  #[cfg(any(test, feature = "test"))]
  pub(crate) message_dropper: Option<Box<dyn MessageDropper>>,
  /// Only used for testing purposes
  #[cfg(any(test, feature = "test"))]
  pub(crate) ping_versioning_test: core::sync::atomic::AtomicBool,
  #[cfg(any(test, feature = "test"))]
  pub(crate) ping_dimension_test: core::sync::atomic::AtomicBool,
}

impl<D, T> SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  pub(crate) fn new(d: Option<D>, tags: Arc<ArcSwap<Tags>>) -> Self {
    Self {
      serf: OnceLock::new(),
      delegate: d,
      tags,
      #[cfg(any(test, feature = "test"))]
      message_dropper: None,
      #[cfg(any(test, feature = "test"))]
      ping_versioning_test: core::sync::atomic::AtomicBool::new(false),
      #[cfg(any(test, feature = "test"))]
      ping_dimension_test: core::sync::atomic::AtomicBool::new(false),
    }
  }

  #[cfg(any(test, feature = "test"))]
  pub(crate) fn with_dropper(
    d: Option<D>,
    dropper: Box<dyn MessageDropper>,
    tags: Arc<ArcSwap<Tags>>,
  ) -> Self {
    Self {
      serf: OnceLock::new(),
      delegate: d,
      tags,
      #[cfg(any(test, feature = "test"))]
      message_dropper: Some(dropper),
      #[cfg(any(test, feature = "test"))]
      ping_versioning_test: core::sync::atomic::AtomicBool::new(false),
      #[cfg(any(test, feature = "test"))]
      ping_dimension_test: core::sync::atomic::AtomicBool::new(false),
    }
  }

  pub(crate) fn delegate(&self) -> Option<&D> {
    self.delegate.as_ref()
  }

  pub(crate) fn store(&self, s: Serf<T, D>) {
    // No error, we never call this in parallel
    let _ = self.serf.set(s);
  }

  fn this(&self) -> &Serf<T, D> {
    self.serf.get().unwrap()
  }
}

impl<D, T> NodeDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  async fn node_meta(&self, limit: usize) -> Meta {
    let tags = self.tags.load();
    match tags.is_empty() {
      false => {
        let encoded_len = tags.encoded_len();
        let limit = limit.min(Meta::MAX_SIZE);
        if encoded_len > limit {
          panic!(
            "node tags {:?} exceeds length limit of {} bytes",
            tags, limit
          );
        }

        let mut role_bytes = vec![0; encoded_len];
        match tags.encode(&mut role_bytes) {
          Ok(len) => {
            if len > limit {
              panic!(
                "node tags {:?} exceeds length limit of {} bytes",
                tags, limit
              );
            }

            role_bytes.try_into().unwrap()
          }
          Err(e) => {
            tracing::error!(err=%e, "serf: failed to encode tags");
            Meta::empty()
          }
        }
      }
      true => Meta::empty(),
    }
  }

  async fn notify_message(&self, buf: Cow<'_, [u8]>) {
    fn to_owned(buf: Cow<'_, [u8]>) -> Bytes {
      match buf {
        Cow::Borrowed(buf) => Bytes::copy_from_slice(buf),
        Cow::Owned(buf) => Bytes::from(buf),
      }
    }

    // If we didn't actually receive any data, then ignore it.
    if buf.is_empty() {
      return;
    }

    #[cfg(feature = "metrics")]
    {
      metrics::histogram!(
        "serf.messages.received",
        self
          .this()
          .inner
          .opts
          .memberlist_options
          .metric_labels
          .iter()
      )
      .record(buf.len() as f64);
    }

    let this = self.this();
    let mut rebroadcast = false;
    let mut rebroadcast_queue = &this.inner.broadcasts;
    let mut relay = None;
    match serf_proto::decode_message::<T::Id, T::ResolvedAddress>(buf.as_ref()) {
      Ok(msg) => {
        #[cfg(any(test, feature = "test"))]
        {
          if let Some(ref dropper) = this.inner.memberlist.delegate().unwrap().message_dropper {
            if dropper.should_drop(MessageType::Join) {
              return;
            }
          }
        }

        match msg {
          MessageRef::Leave(l) => {
            tracing::debug!("serf: leave message: {:?}", l.id());
            // TODO(al8n): do not read to owned here
            match <LeaveMessage<T::Id> as Data>::from_ref(l) {
              Err(e) => {
                tracing::error!(err=%e, "serf: failed to decode leave message");
              }
              Ok(l) => {
                rebroadcast = this.handle_node_leave_intent(&l).await;
              }
            };
          }
          MessageRef::Join(j) => {
            tracing::debug!("serf: join message: {:?}", j.id());
            // TODO(al8n): do not read to owned here

            match <JoinMessage<T::Id> as Data>::from_ref(j) {
              Err(e) => {
                tracing::error!(err=%e, "serf: failed to decode join message");
              }
              Ok(j) => {
                rebroadcast = this.handle_node_join_intent(&j).await;
              }
            };
          }
          MessageRef::UserEvent(ue) => {
            tracing::debug!("serf: user event message: {}", ue.name());
            rebroadcast = this.handle_user_event(either::Either::Left(ue)).await;
            rebroadcast_queue = &this.inner.event_broadcasts;
          }
          MessageRef::Query(q) => {
            tracing::debug!("serf: query message: {}", q.name());
            match q.decode_internal_query() {
              Some(Err(e)) => {
                tracing::warn!(err=%e, "serf: failed to decode message");
              }
              Some(Ok(res)) => match this.handle_query(either::Either::Left(q), Some(res)).await {
                Ok(val) => {
                  rebroadcast = val;
                  rebroadcast_queue = &this.inner.query_broadcasts;
                }
                Err(e) => {
                  tracing::warn!(err=%e, "serf: failed to decode query message");
                }
              },
              None => match this.handle_query(either::Either::Left(q), None).await {
                Ok(val) => {
                  rebroadcast = val;
                  rebroadcast_queue = &this.inner.query_broadcasts;
                }
                Err(e) => {
                  tracing::warn!(err=%e, "serf: failed to decode query message");
                }
              },
            }
          }
          MessageRef::QueryResponse(qr) => {
            tracing::debug!("serf: query response message: {:?}", qr.from());
            if let Err(e) = this.handle_query_response(qr).await {
              tracing::warn!(err=%e, "serf: failed to decode query response message");
            }
          }
          MessageRef::Relay {
            node,
            payload,
            payload_offset,
          } => {
            tracing::debug!("serf: relaying response to node: {:?}", node);
            match Data::from_ref(*node.address()) {
              Err(e) => {
                tracing::error!(err=%e, "serf: failed to encode address");
              }
              Ok(addr) => match buf {
                Cow::Borrowed(_) => {
                  relay = Some((addr, Either::Left(Bytes::copy_from_slice(payload))));
                }
                Cow::Owned(_) => {
                  relay = Some((addr, Either::Right((payload_offset, payload.len()))));
                }
              },
            }
          }
          msg => {
            tracing::warn!("serf: receive unexpected message type: {}", msg.ty());
          }
        }
      }
      Err(e) => {
        tracing::warn!(err=%e, "serf: failed to decode message");
      }
    }

    if rebroadcast {
      rebroadcast_queue
        .queue_broadcast(SerfBroadcast {
          msg: to_owned(buf),
          notify_tx: None,
        })
        .await;
    } else if let Some((addr, payload)) = relay {
      let msg = match payload {
        Either::Left(p) => p,
        Either::Right((offset, len)) => {
          let mut buf = to_owned(buf);
          buf.advance(offset);
          buf.split_to(len)
        }
      };

      if let Err(e) = this.inner.memberlist.send(&addr, msg).await {
        tracing::error!(err=%e, "serf: failed to forwarding message to {}", addr);
      }
    }
  }

  async fn broadcast_messages<F>(
    &self,
    limit: usize,
    encoded_len: F,
  ) -> impl Iterator<Item = Bytes> + Send
  where
    F: Fn(Bytes) -> (usize, Bytes) + Send + Sync + 'static,
  {
    let this = self.this();
    let mut msgs = this.inner.broadcasts.get_broadcasts(limit).await;

    // Determine the bytes used already
    let mut bytes_used = 0;
    for msg in msgs.iter() {
      let (encoded_len, _) = encoded_len(msg.clone());
      bytes_used += encoded_len;
      #[cfg(feature = "metrics")]
      {
        metrics::histogram!(
          "serf.messages.sent",
          this.inner.opts.memberlist_options.metric_labels.iter()
        )
        .record(encoded_len as f64);
      }
    }

    // Get any additional query broadcasts
    let query_msgs = this
      .inner
      .query_broadcasts
      .get_broadcasts(limit - bytes_used)
      .await;
    for msg in query_msgs.iter() {
      let (encoded_len, _) = encoded_len(msg.clone());
      bytes_used += encoded_len;
      #[cfg(feature = "metrics")]
      {
        metrics::histogram!(
          "serf.messages.sent",
          this.inner.opts.memberlist_options.metric_labels.iter()
        )
        .record(encoded_len as f64);
      }
    }

    // Get any additional event broadcasts
    let event_msgs = this
      .inner
      .event_broadcasts
      .get_broadcasts(limit - bytes_used)
      .await;
    for msg in event_msgs.iter() {
      let (encoded_len, _) = encoded_len(msg.clone());
      bytes_used += encoded_len;
      #[cfg(feature = "metrics")]
      {
        metrics::histogram!(
          "serf.messages.sent",
          this.inner.opts.memberlist_options.metric_labels.iter()
        )
        .record(encoded_len as f64);
      }
    }
    msgs.extend(query_msgs);
    msgs.extend(event_msgs);
    msgs.into_iter()
  }

  async fn local_state(&self, _join: bool) -> Bytes {
    let this = self.this();
    let members = this.inner.members.read().await;
    let events = this.inner.event_core.read().await;

    // Create the message to send
    let status_ltimes = members
      .states
      .values()
      .map(|v| (v.member.node.id().cheap_clone(), v.status_time))
      .collect();
    let left_members = members
      .left_members
      .iter()
      .map(|v| v.member.node().id().cheap_clone())
      .collect::<IndexSet<T::Id>>();
    let pp = PushPullMessageBorrow {
      ltime: this.inner.clock.time(),
      status_ltimes: &status_ltimes,
      left_members: &left_members,
      event_ltime: this.inner.event_clock.time(),
      events: events.buffer.as_slice(),
      query_ltime: this.inner.query_clock.time(),
    };
    drop(members);

    match serf_proto::Encodable::encode_to_bytes(&pp) {
      Ok(buf) => buf,
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to encode local state");
        Bytes::new()
      }
    }
  }

  async fn merge_remote_state(&self, buf: &[u8], is_join: bool) {
    if buf.is_empty() {
      tracing::error!("serf: remote state is zero bytes");
      return;
    }

    // Check the message type
    let msg = match serf_proto::decode_message::<T::Id, T::ResolvedAddress>(buf) {
      Ok(msg) => msg,
      Err(e) => {
        tracing::error!(err=%e, "serf: fail to decode remote state");
        return;
      }
    };

    #[cfg(any(test, feature = "test"))]
    {
      if let Some(ref dropper) = self
        .this()
        .inner
        .memberlist
        .delegate()
        .unwrap()
        .message_dropper
      {
        if dropper.should_drop(MessageType::PushPull) {
          return;
        }
      }
    }

    match msg {
      MessageRef::PushPull(pp) => {
        let ltime = pp.ltime();
        let event_ltime = pp.event_ltime();
        let query_ltime = pp.query_ltime();
        let this = self.this();
        // Witness the Lamport clocks first.
        // We subtract 1 since no message with that clock has been sent yet
        if ltime > LamportTime::ZERO {
          this.inner.clock.witness(ltime - LamportTime::new(1));
        }
        if event_ltime > LamportTime::ZERO {
          this
            .inner
            .event_clock
            .witness(event_ltime - LamportTime::new(1));
        }
        if query_ltime > LamportTime::ZERO {
          this
            .inner
            .query_clock
            .witness(query_ltime - LamportTime::new(1));
        }

        let pp = match <PushPullMessage<T::Id> as Data>::from_ref(pp) {
          Ok(pp) => pp,
          Err(e) => {
            tracing::error!(err=%e, "serf: failed to decode push pull message");
            return;
          }
        };

        // Process the left nodes first to avoid the LTimes from incrementing
        // in the wrong order. Note that we don't have the actual Lamport time
        // for the leave message, so we go one past the join time, since the
        // leave must have been accepted after that to get onto the left members
        // list. If we didn't do this then the message would not get processed.
        for node in &pp.left_members {
          if let Some(&ltime) = pp.status_ltimes.get(node) {
            this
              .handle_node_leave_intent(&LeaveMessage {
                ltime: ltime + LamportTime::new(1),
                id: node.cheap_clone(),
                prune: false,
              })
              .await;
          } else {
            tracing::error!(
              "serf: {} is in left members, but cannot find the lamport time for it in status",
              node
            );
          }
        }

        // Update any other LTimes
        for (node, ltime) in pp.status_ltimes {
          // Skip the left nodes
          if pp.left_members.contains(&node) {
            continue;
          }

          // Create an artificial join message
          this
            .handle_node_join_intent(&JoinMessage { ltime, id: node })
            .await;
        }

        // If we are doing a join, and eventJoinIgnore is set
        // then we set the eventMinTime to the EventLTime. This
        // prevents any of the incoming events from being processed
        let event_join_ignore = this.inner.event_join_ignore.load(Ordering::Acquire);
        if is_join && event_join_ignore {
          let mut ec = this.inner.event_core.write().await;
          if event_ltime > ec.min_time {
            ec.min_time = event_ltime;
          }
        }

        // Process all the events
        for events in pp.events {
          for e in events.events {
            this
              .handle_user_event(either::Either::Right(UserEventMessage {
                ltime: events.ltime,
                name: e.name,
                payload: e.payload,
                cc: false,
              }))
              .await;
          }
        }
      }
      msg => {
        tracing::error!("serf: remote state has bad message type {}", msg.ty());
      }
    }
  }
}

impl<D, T> EventDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;
  type Address = T::ResolvedAddress;

  async fn notify_join(&self, node: Arc<NodeState<Self::Id, Self::Address>>) {
    if let Some(serf) = self.serf.get() {
      serf.handle_node_join(node).await;
    }
  }

  async fn notify_leave(&self, node: Arc<NodeState<Self::Id, Self::Address>>) {
    self.this().handle_node_leave(node).await;
  }

  async fn notify_update(&self, node: Arc<NodeState<Self::Id, Self::Address>>) {
    self.this().handle_node_update(node).await;
  }
}

impl<D, T> AliveDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;
  type Address = T::ResolvedAddress;
  type Error = SerfDelegateError<D>;

  async fn notify_alive(
    &self,
    node: Arc<NodeState<Self::Id, Self::Address>>,
  ) -> Result<(), Self::Error> {
    if let Some(ref d) = self.delegate {
      let member = node_to_member::<T, D>(&node)?;
      return d
        .notify_merge(Arc::from_iter([member]))
        .await
        .map_err(SerfDelegateError::Merge);
    }

    Ok(())
  }
}

impl<D, T> MemberlistMergeDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;
  type Address = T::ResolvedAddress;
  type Error = SerfDelegateError<D>;

  async fn notify_merge(
    &self,
    peers: Arc<[NodeState<Self::Id, Self::Address>]>,
  ) -> Result<(), Self::Error> {
    if let Some(ref d) = self.delegate {
      let peers = peers
        .iter()
        .map(node_to_member::<T, D>)
        .collect::<Result<Arc<_>, _>>()?;
      return d
        .notify_merge(peers)
        .await
        .map_err(SerfDelegateError::Merge);
    }
    Ok(())
  }
}

impl<D, T> ConflictDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;

  type Address = T::ResolvedAddress;

  async fn notify_conflict(
    &self,
    existing: Arc<NodeState<Self::Id, Self::Address>>,
    other: Arc<NodeState<Self::Id, Self::Address>>,
  ) {
    self.this().handle_node_conflict(existing, other).await;
  }
}

impl<D, T> PingDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;

  type Address = T::ResolvedAddress;

  async fn ack_payload(&self) -> Bytes {
    #[cfg(any(feature = "test", test))]
    if self.ping_versioning_test.load(Ordering::SeqCst) {
      // Send back the next ping version, which is bad by default.
      let mut buf = BytesMut::new();
      buf.put_u8(PING_VERSION + 1);
      buf.put_slice(b"this is bad and not a real message");
      return buf.freeze();
    }

    #[cfg(any(feature = "test", test))]
    if self.ping_dimension_test.load(Ordering::SeqCst) {
      let mut buf = BytesMut::new();
      buf.put_u8(PING_VERSION);

      // Make a bad coordinate with the wrong number of dimensions.
      let mut coord = crate::coordinate::Coordinate::new();
      let len = coord.portion.len();
      coord.portion.resize(len * 2, 0.0);

      // The rest of the message is the serialized coordinate.
      let len = coord.encoded_len();
      buf.resize(len + 1, 0);
      if let Err(e) = coord.encode(&mut buf[1..]) {
        panic!("failed to encode coordinate: {}", e);
      }
      return buf.freeze();
    }

    if let Some(c) = self.this().inner.coord_core.as_ref() {
      let coord = c.client.get_coordinate();
      let encoded_len = coord.encoded_len() + 1;
      let mut buf = BytesMut::with_capacity(encoded_len);
      buf.put_u8(PING_VERSION);
      buf.resize(encoded_len, 0);

      if let Err(e) = coord.encode(&mut buf[1..]) {
        tracing::error!(err=%e, "serf: failed to encode coordinate");
      }
      buf.into()
    } else {
      Bytes::new()
    }
  }

  async fn notify_ping_complete(
    &self,
    node: Arc<NodeState<Self::Id, Self::Address>>,
    rtt: std::time::Duration,
    payload: Bytes,
  ) {
    if payload.is_empty() {
      return;
    }

    let this = self.this();

    if let Some(ref c) = this.inner.coord_core {
      // Verify ping version in the header.
      if payload[0] != PING_VERSION {
        tracing::error!("serf: unsupported ping version: {}", payload[0]);
        return;
      }

      // Process the remainder of the message as a coordinate.
      let coord = match <super::Coordinate as Data>::decode(&payload[1..]) {
        Ok((readed, c)) => {
          tracing::trace!(read=%readed, coordinate=?c, "serf: decode coordinate successfully");
          c
        }
        Err(e) => {
          tracing::error!(err=%e, "serf: failed to decode coordinate from ping");
          return;
        }
      };

      // Apply the update.
      #[cfg(feature = "metrics")]
      let before = c.client.get_coordinate();
      match c.client.update(node.id(), &coord, rtt) {
        Ok(_after) => {
          #[cfg(feature = "metrics")]
          {
            // Publish some metrics to give us an idea of how much we are
            // adjusting each time we update.
            let d = before.distance_to(&_after).as_secs_f64() * 1.0e3;
            metrics::histogram!(
              "serf.coordinate.adjustment-ms",
              this.inner.opts.memberlist_options.metric_labels.iter()
            )
            .record(d);
          }

          // Cache the coordinate for the other node, and add our own
          // to the cache as well since it just got updated. This lets
          // users call GetCachedCoordinate with our node name, which is
          // more friendly.
          let mut cache = c.cache.write();
          cache.insert(node.id().cheap_clone(), coord);
          cache
            .entry(this.inner.memberlist.local_id().cheap_clone())
            .and_modify(|x| {
              *x = c.client.get_coordinate();
            })
            .or_insert_with(|| c.client.get_coordinate());
        }
        Err(e) => {
          #[cfg(feature = "metrics")]
          {
            metrics::counter!(
              "serf.coordinate.rejected",
              this.inner.opts.memberlist_options.metric_labels.iter()
            )
            .increment(1);
          }

          tracing::error!(err=%e, "serf: rejected coordinate from {}", node.id());
        }
      }
    }
  }
}

impl<D, T> MemberlistDelegate for SerfDelegate<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  type Id = T::Id;

  type Address = T::ResolvedAddress;
}

fn node_to_member<T, D>(
  node: &NodeState<T::Id, T::ResolvedAddress>,
) -> Result<Member<T::Id, T::ResolvedAddress>, SerfDelegateError<D>>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  let status = if node.state() == State::Left {
    MemberStatus::Left
  } else {
    MemberStatus::None
  };

  let meta = node.meta();
  if meta.len() > META_MAX_SIZE {
    return Err(SerfDelegateError::Serf(SerfError::TagsTooLarge(meta.len())));
  }

  Ok(Member {
    node: node.node(),
    tags: if !node.meta().is_empty() {
      <Tags as Data>::decode(node.meta())
        .map(|(read, tags)| {
          tracing::trace!(read=%read, tags=?tags, "serf: decode tags successfully");
          Arc::new(tags)
        })
        .map_err(|e| SerfDelegateError::Serf(SerfError::from(e)))?
    } else {
      Default::default()
    },
    status,
    protocol_version: ProtocolVersion::V1,
    delegate_version: DelegateVersion::V1,
    memberlist_delegate_version: node.delegate_version(),
    memberlist_protocol_version: node.protocol_version(),
  })
}
