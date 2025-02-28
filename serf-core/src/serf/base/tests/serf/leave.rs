use super::*;

/// Unit tests for the leave intent buffer early
pub async fn leave_intent_buffer_early<T>(transport_opts: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();

  // Deliver a leave intent message early
  let j = LeaveMessage {
    ltime: 10.into(),
    id: "test".into(),
    prune: false,
  };

  assert!(s1.handle_node_leave_intent(&j).await, "should rebroadcast");
  assert!(
    !s1.handle_node_leave_intent(&j).await,
    "should not rebroadcast"
  );

  // Check that we buffered
  {
    let members = s1.inner.members.read().await;
    let ltime = recent_intent(&members.recent_intents, &"test".into(), MessageType::Leave).unwrap();
    assert_eq!(ltime, 10.into(), "bad buffer");
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the leave intent old message
pub async fn leave_intent_old_message<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
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
          memberlist_protocol_version: serf_proto::MemberlistProtocolVersion::V1,
          memberlist_delegate_version: serf_proto::MemberlistDelegateVersion::V1,
          protocol_version: serf_proto::ProtocolVersion::V1,
          delegate_version: serf_proto::DelegateVersion::V1,
        },
        status_time: 12.into(),
        leave_time: None,
      },
    );
  }

  let j = LeaveMessage {
    ltime: 10.into(),
    id: "test".into(),
    prune: false,
  };

  assert!(
    !s1.handle_node_leave_intent(&j).await,
    "should not rebroadcast"
  );

  {
    let members = s1.inner.members.read().await;
    assert!(
      recent_intent(&members.recent_intents, &"test".into(), MessageType::Leave).is_none(),
      "should not have buffered intent"
    );
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the leave intent newer
pub async fn leave_intent_newer<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
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
          memberlist_protocol_version: serf_proto::MemberlistProtocolVersion::V1,
          memberlist_delegate_version: serf_proto::MemberlistDelegateVersion::V1,
          protocol_version: serf_proto::ProtocolVersion::V1,
          delegate_version: serf_proto::DelegateVersion::V1,
        },
        status_time: 12.into(),
        leave_time: None,
      },
    );
  }

  let j = LeaveMessage {
    ltime: 14.into(),
    id: "test".into(),
    prune: false,
  };

  assert!(s1.handle_node_leave_intent(&j).await, "should rebroadcast");

  {
    let members = s1.inner.members.read().await;
    assert!(
      recent_intent(&members.recent_intents, &"test".into(), MessageType::Leave).is_none(),
      "should not have buffered intent"
    );

    let m = members.states.get("test").unwrap();
    assert_eq!(
      m.member.status,
      MemberStatus::Leaving,
      "should update status"
    );
    assert_eq!(s1.inner.clock.time(), 15.into(), "should update clock");
  }

  s1.shutdown().await.unwrap();
}

/// Unit tests for the force leave failed
pub async fn serf_force_leave_failed<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  transport_opts3: T::Options,
) where
  T: Transport,
{
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();
  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();
  let s3 = Serf::<T>::new(transport_opts3, test_config())
    .await
    .unwrap();

  let mut serfs = [s1, s2, s3];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  let node = serfs[2]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(3, &serfs).await;

  serfs[1].shutdown().await.unwrap();

  let s2id = serfs[1].local_id().clone();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Failed).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }
  serfs[0].force_leave(s2id, true).await.unwrap();
  serfs.swap(1, 2);
  wait_until_num_nodes(2, &serfs[..2]).await;
}

/// Unit tests for the force leave leaving
pub async fn serf_force_leave_leaving<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  transport_opts3: T::Options,
) where
  T: Transport,
{
  const TOMBSTONE_TIMEOUT: Duration = Duration::from_secs(3600);
  const LEAVE_PROPAGATE_DELAY: Duration = Duration::from_secs(5);

  let s1 = Serf::<T>::new(
    transport_opts1,
    test_config()
      .with_tombstone_timeout(TOMBSTONE_TIMEOUT)
      .with_leave_propagate_delay(LEAVE_PROPAGATE_DELAY),
  )
  .await
  .unwrap();
  let s2 = Serf::<T>::new(
    transport_opts2,
    test_config()
      .with_tombstone_timeout(TOMBSTONE_TIMEOUT)
      .with_leave_propagate_delay(LEAVE_PROPAGATE_DELAY),
  )
  .await
  .unwrap();
  let s3 = Serf::<T>::new(
    transport_opts3,
    test_config()
      .with_tombstone_timeout(TOMBSTONE_TIMEOUT)
      .with_leave_propagate_delay(LEAVE_PROPAGATE_DELAY),
  )
  .await
  .unwrap();

  let mut serfs = [s1, s2, s3];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  let node = serfs[2]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(3, &serfs).await;

  //Put s2 in left state
  serfs[1].leave().await.unwrap();

  let s2id = serfs[1].local_id().clone();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  serfs[0].force_leave(s2id, true).await.unwrap();
  serfs.swap(1, 2);
  wait_until_num_nodes(2, &serfs[..2]).await;
}

/// Unit tests for the force leave left
pub async fn serf_force_leave_left<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  transport_opts3: T::Options,
) where
  T: Transport,
{
  const TOMBSTONE_TIMEOUT: Duration = Duration::from_secs(3600);

  let s1 = Serf::<T>::new(
    transport_opts1,
    test_config().with_tombstone_timeout(TOMBSTONE_TIMEOUT),
  )
  .await
  .unwrap();
  let s2 = Serf::<T>::new(
    transport_opts2,
    test_config().with_tombstone_timeout(TOMBSTONE_TIMEOUT),
  )
  .await
  .unwrap();
  let s3 = Serf::<T>::new(
    transport_opts3,
    test_config().with_tombstone_timeout(TOMBSTONE_TIMEOUT),
  )
  .await
  .unwrap();

  let mut serfs = [s1, s2, s3];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  let node = serfs[2]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(3, &serfs).await;

  //Put s2 in left state
  serfs[1].leave().await.unwrap();

  let s2id = serfs[1].local_id().clone();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  serfs[0].force_leave(s2id, true).await.unwrap();
  serfs.swap(1, 2);
  wait_until_num_nodes(2, &serfs[..2]).await;
}

/// Unit tests for the leave rejoin different role
pub async fn serf_leave_rejoin_different_role<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
) where
  T: Transport,
  T::Options: Clone,
{
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();
  let s2 = Serf::<T>::new(transport_opts2.clone(), test_config())
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

  <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(10)).await;

  // Make s3 look just like s2, but create a new node with a new role
  let s3 = Serf::<T>::new(
    transport_opts2,
    test_config().with_tags([("role", "bar")].into_iter()),
  )
  .await
  .unwrap();

  let node = serfs[0]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);

  serfs[1] = s3;

  serfs[1].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  let start = Epoch::now();
  let s3id = serfs[1].local_id().clone();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    let mut find = None;
    for (id, member) in members.states.iter() {
      if s3id.eq(id) {
        find = Some(member);
        break;
      }
    }

    if let Some(member) = find {
      let role = member.member.tags.get("role");
      assert_eq!(role, Some(&"bar".into()), "bad role: {:?}", role);
      return;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }
}

/// Unit tests for the serf leave snapshot recovery
pub async fn serf_leave_snapshot_recovery<T, F>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  get_transport: impl FnOnce(T::Id, T::ResolvedAddress) -> F + Copy,
) where
  T: Transport,
  F: core::future::Future<Output = T::Options>,
{
  let td = tempfile::tempdir().unwrap();
  let snap_path = td.path().join("serf_leave_snapshot_recovery");

  let s1 = Serf::<T>::new(
    transport_opts1,
    test_config().with_reap_interval(Duration::from_secs(30)),
  )
  .await
  .unwrap();
  let s2 = Serf::<T>::new(
    transport_opts2,
    test_config()
      .with_snapshot_path(Some(snap_path.clone()))
      .with_reap_interval(Duration::from_secs(30)),
  )
  .await
  .unwrap();

  let mut serfs = vec![s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .inner
    .memberlist
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node.clone(), false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  // Put s2 in left state
  serfs[1].leave().await.unwrap();
  serfs[1].shutdown().await.unwrap();
  let t = serfs[1].inner.opts.memberlist_options.probe_interval * 5;
  let (s2id, s2addr) = serfs[1].advertise_node().into_components();
  let _ = serfs.pop().unwrap();

  <T::Runtime as RuntimeLite>::sleep(t).await;

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  // Restart s2 from the snapshot now!
  let s2 = Serf::<T>::new(
    get_transport(s2id.clone(), s2addr).await,
    test_config()
      .with_snapshot_path(Some(snap_path.clone()))
      .with_reap_interval(Duration::from_secs(30)),
  )
  .await
  .unwrap();
  serfs.push(s2);

  // Wait for the node to auto rejoin

  // Verify that s2 did not join
  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  loop {
    if !cond1 {
      let num = serfs[1].num_members().await;
      if num == 1 {
        cond1 = true;
      }
    }

    if !cond2 {
      let members = serfs[0].inner.members.read().await;
      if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
        cond2 = true;
      }
    }

    if cond1 && cond2 {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("time out");
    }

    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;
  }
}
