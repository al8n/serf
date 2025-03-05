use std::marker::PhantomData;

use crate::delegate::MergeDelegate;

use super::*;

/// Unit tests for the join intent buffer early
pub async fn join_intent_buffer_early<T>(transport_opts: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();

  // Deliver a join intent message early
  let j = JoinMessage {
    ltime: 10.into(),
    id: "test".into(),
  };

  assert!(s1.handle_node_join_intent(&j).await, "should rebroadcast");
  assert!(
    !s1.handle_node_join_intent(&j).await,
    "should not rebroadcast"
  );

  // Check that we buffered
  {
    let members = s1.inner.members.read().await;
    let ltime = recent_intent(&members.recent_intents, &"test".into(), MessageType::Join).unwrap();
    assert_eq!(ltime, 10.into(), "bad buffer");
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join intent old message
pub async fn join_intent_old_message<T>(transport_opts: T::Options, addr: T::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();

  {
    let mut members = s1.inner.members.write().await;
    members.states.insert(
      "test".into(),
      MemberState {
        member: Member {
          node: Node::new("test".into(), addr),
          tags: Arc::new(Default::default()),
          status: MemberStatus::Alive,
          memberlist_protocol_version: crate::types::MemberlistProtocolVersion::V1,
          memberlist_delegate_version: crate::types::MemberlistDelegateVersion::V1,
          protocol_version: crate::types::ProtocolVersion::V1,
          delegate_version: crate::types::DelegateVersion::V1,
        },
        status_time: 12.into(),
        leave_time: None,
      },
    );
  }

  let j = JoinMessage {
    ltime: 10.into(),
    id: "test".into(),
  };

  assert!(
    !s1.handle_node_join_intent(&j).await,
    "should not rebroadcast"
  );

  // Check that we didn't buffer anything
  {
    let members = s1.inner.members.read().await;
    assert!(
      recent_intent(&members.recent_intents, &"test".into(), MessageType::Join).is_none(),
      "should not have buffered intent"
    );
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join intent newer
pub async fn join_intent_newer<T>(transport_opts: T::Options, addr: T::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();
  {
    let mut members = s1.inner.members.write().await;
    members.states.insert(
      "test".into(),
      MemberState {
        member: Member {
          node: Node::new("test".into(), addr),
          tags: Arc::new(Default::default()),
          status: MemberStatus::Alive,
          memberlist_protocol_version: crate::types::MemberlistProtocolVersion::V1,
          memberlist_delegate_version: crate::types::MemberlistDelegateVersion::V1,
          protocol_version: crate::types::ProtocolVersion::V1,
          delegate_version: crate::types::DelegateVersion::V1,
        },
        status_time: 12.into(),
        leave_time: None,
      },
    );
  }

  let j = JoinMessage {
    ltime: 14.into(),
    id: "test".into(),
  };

  assert!(s1.handle_node_join_intent(&j).await, "should rebroadcast");

  {
    let members = s1.inner.members.read().await;
    assert!(
      recent_intent(&members.recent_intents, &"test".into(), MessageType::Join).is_none(),
      "should not have buffered intent"
    );

    let m = members.states.get("test").unwrap();
    assert_eq!(m.status_time, 14.into(), "should update join time");
    assert_eq!(s1.inner.clock.time(), 15.into(), "should update clock");
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join intent reset leaving
pub async fn join_intent_reset_leaving<T>(transport_opts: T::Options, addr: T::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();

  {
    let mut members = s1.inner.members.write().await;
    members.states.insert(
      "test".into(),
      MemberState {
        member: Member {
          node: Node::new("test".into(), addr),
          tags: Arc::new(Default::default()),
          status: MemberStatus::Leaving,
          memberlist_protocol_version: crate::types::MemberlistProtocolVersion::V1,
          memberlist_delegate_version: crate::types::MemberlistDelegateVersion::V1,
          protocol_version: crate::types::ProtocolVersion::V1,
          delegate_version: crate::types::DelegateVersion::V1,
        },
        status_time: 12.into(),
        leave_time: None,
      },
    );
  }

  let j = JoinMessage {
    ltime: 14.into(),
    id: "test".into(),
  };

  assert!(s1.handle_node_join_intent(&j).await, "should rebroadcast");

  {
    let members = s1.inner.members.read().await;
    assert!(
      recent_intent(&members.recent_intents, &"test".into(), MessageType::Join).is_none(),
      "should not have buffered intent"
    );

    let m = members.states.get("test").unwrap();
    assert_eq!(m.status_time, 14.into(), "should update join time");
    assert_eq!(m.member.status, MemberStatus::Alive, "should update status");
    assert_eq!(s1.inner.clock.time(), 15.into(), "should update clock");
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join leave ltime logic
pub async fn join_leave_ltime<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts1, opts).await.unwrap();
  let opts = test_config();
  let s2 = Serf::<T>::new(transport_opts2, opts).await.unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let (id, addr) = serfs[1].inner.memberlist.advertise_node().into_components();
  serfs[0]
    .join(Node::new(id, MaybeResolvedAddress::resolved(addr)), false)
    .await
    .unwrap();

  wait_until_num_nodes(2, &serfs).await;

  let now = Epoch::now();

  loop {
    let members = serfs[1].inner.members.read().await;
    let mut cond1 = false;
    let mut cond2 = false;
    if let Some(m) = members.states.get(serfs[0].inner.memberlist.local_id()) {
      if m.status_time == 1.into() {
        cond1 = true;
      }

      if serfs[1].inner.clock.time() > m.status_time {
        cond2 = true;
      }
    }

    if cond1 && cond2 {
      break;
    }

    if now.elapsed() > Duration::from_secs(7) {
      panic!("timed out waiting for status time to be updated");
    }

    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;
  }

  let old_clock = serfs[1].inner.clock.time();

  serfs[0].leave().await.unwrap();

  loop {
    let mut cond1 = false;

    if serfs[1].inner.clock.time() > old_clock {
      cond1 = true;
    }

    if cond1 {
      break;
    }

    if now.elapsed() > Duration::from_secs(7) {
      panic!(
        "leave should increment ({} / {})",
        serfs[1].inner.clock.time(),
        old_clock
      );
    }

    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;
  }

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit tests for the join pending intent logic
pub async fn join_pending_intent<T>(transport_opts: T::Options, addr: T::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();
  {
    let mut members = s1.inner.members.write().await;
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"test".into(),
      MessageType::Join,
      5.into(),
      Epoch::now,
    );
  }

  s1.handle_node_join(Arc::new(NodeState {
    id: "test".into(),
    addr,
    meta: Meta::empty(),
    state: memberlist_core::proto::State::Alive,
    protocol_version: crate::types::MemberlistProtocolVersion::V1,
    delegate_version: crate::types::MemberlistDelegateVersion::V1,
  }))
  .await;

  {
    let members = s1.inner.members.read().await;
    let m = members.states.get("test").unwrap();
    assert_eq!(m.status_time, 5.into());
    assert_eq!(m.member.status, MemberStatus::Alive);
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join pending intent logic
pub async fn join_pending_intents<T>(transport_opts: T::Options, addr: T::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();
  {
    let mut members = s1.inner.members.write().await;
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"test".into(),
      MessageType::Join,
      5.into(),
      Epoch::now,
    );
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"test".into(),
      MessageType::Leave,
      6.into(),
      Epoch::now,
    );
  }

  s1.handle_node_join(Arc::new(NodeState {
    id: "test".into(),
    addr,
    meta: Meta::empty(),
    state: memberlist_core::proto::State::Alive,
    protocol_version: crate::types::MemberlistProtocolVersion::V1,
    delegate_version: crate::types::MemberlistDelegateVersion::V1,
  }))
  .await;

  {
    let members = s1.inner.members.read().await;
    let m = members.states.get("test").unwrap();
    assert_eq!(m.status_time, 6.into());
    assert_eq!(m.member.status, MemberStatus::Leaving);
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the join leave
pub async fn serf_join_leave<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();
  let reap_interval = s1.inner.opts.reap_interval();
  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node.clone(), false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[1].leave().await.unwrap();

  <T::Runtime as RuntimeLite>::sleep(reap_interval * 2).await;

  wait_until_num_nodes(1, &serfs).await;
}

/// Unit test for serf join leave join
pub async fn serf_join_leave_join<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
  T::Options: Clone,
{
  let s1 = Serf::<T>::new(
    transport_opts1,
    test_config().with_reap_interval(Duration::from_secs(10)),
  )
  .await
  .unwrap();
  let s2 = Serf::<T>::new(
    transport_opts2.clone(),
    test_config().with_reap_interval(Duration::from_secs(10)),
  )
  .await
  .unwrap();

  let mut serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node.clone(), false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[1].leave().await.unwrap();
  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval() * 5;
  // Give the reaper time to reap nodes
  <T::Runtime as RuntimeLite>::sleep(t).await;

  // s1 should see the node as having left
  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    let mut any_left = false;
    for member in members.states.values() {
      if member.member.status == MemberStatus::Left {
        any_left = true;
        break;
      }
    }

    if any_left {
      break;
    }

    if !any_left && start.elapsed() > Duration::from_secs(7) {
      panic!("Node should have left");
    }
  }

  // Bring the node back
  let s2 = Serf::<T>::new(
    transport_opts2,
    test_config().with_reap_interval(Duration::from_secs(10)),
  )
  .await
  .unwrap();

  serfs[1] = s2;
  wait_until_num_nodes(1, &serfs[1..]).await;

  // Re-attempt the join
  let node = serfs[1]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node.clone(), false).await.unwrap();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    let mut any_left = false;
    for member in members.states.values() {
      if member.member.status == MemberStatus::Left {
        any_left = true;
        break;
      }
    }

    if !any_left {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("all nodes should be alive!");
    }
  }

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for serf join ignore old
pub async fn serf_join_ignore_old<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();

  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s2 = Serf::<T>::with_event_producer(transport_opts2, test_config(), event_tx)
    .await
    .unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  // Fire a user event
  serfs[0]
    .user_event("event!", Bytes::from_static(b"test"), false)
    .await
    .unwrap();
  <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(10)).await;

  serfs[0]
    .user_event("second", Bytes::from_static(b"foobar"), false)
    .await
    .unwrap();
  <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(10)).await;

  // join with ignoreOld set to true! should not get events
  let node = serfs[0]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[1].join(node.clone(), true).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  // check the events to make sure we got nothing
  test_user_events(event_rx.rx, vec![], vec![]).await;

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

#[derive(Debug, thiserror::Error)]
#[error("merge canceled")]
struct CancelMergeError;

#[derive(Clone)]
struct CancelMergeDelegate<A: CheapClone + Send + Sync + 'static> {
  invoked: Arc<AtomicBool>,
  _phantom: PhantomData<A>,
}

impl<A: CheapClone + Send + Sync + 'static> MergeDelegate for CancelMergeDelegate<A> {
  type Error = CancelMergeError;

  type Id = SmolStr;

  type Address = A;

  async fn notify_merge(
    &self,
    _members: Arc<[Member<Self::Id, Self::Address>]>,
  ) -> Result<(), Self::Error> {
    self.invoked.store(true, Ordering::SeqCst);
    Err(CancelMergeError)
  }
}

/// Unit test for serf join cancel
pub async fn serf_join_cancel<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let cmd1 = CancelMergeDelegate {
    invoked: Arc::new(AtomicBool::new(false)),
    _phantom: PhantomData,
  };
  let s1 = Serf::<T, _>::with_delegate(
    transport_opts1,
    test_config(),
    DefaultDelegate::<T>::new().with_merge_delegate(cmd1.clone()),
  )
  .await
  .unwrap();
  let cmd2 = CancelMergeDelegate {
    invoked: Arc::new(AtomicBool::new(false)),
    _phantom: PhantomData,
  };
  let s2 = Serf::<T, _>::with_delegate(
    transport_opts2,
    test_config(),
    DefaultDelegate::<T>::new().with_merge_delegate(cmd2.clone()),
  )
  .await
  .unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(0, &serfs).await;

  let node = serfs[1]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);

  let err = serfs[0].join(node.clone(), false).await.unwrap_err();
  assert!(err.to_string().contains("merge canceled"));

  wait_until_num_nodes(0, &serfs).await;

  assert!(cmd1.invoked.load(Ordering::SeqCst));
  assert!(cmd2.invoked.load(Ordering::SeqCst));

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}
