use super::*;

/// Unit test for delegate node meta
pub async fn delegate_nodemeta<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let s = Serf::<T>::new(
    transport_opts,
    opts.with_tags([("role", "test")].into_iter()),
  )
  .await
  .unwrap();
  let meta = s.inner.memberlist.delegate().unwrap().node_meta(32).await;

  let (_, tags) = Tags::decode(&meta).unwrap();
  assert_eq!(tags.get("role"), Some(&SmolStr::new("test")));

  s.shutdown().await.unwrap();
}

/// Unit test for delegate node meta panic
pub async fn delegate_nodemeta_panic<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let s = Serf::<T>::new(
    transport_opts,
    opts.with_tags([("role", "test")].into_iter()),
  )
  .await
  .unwrap();
  s.inner.memberlist.delegate().unwrap().node_meta(1).await;
  s.shutdown().await.unwrap();
}

/// Unit test for delegate local state
pub async fn delegate_local_state<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let opts = test_config().with_event_buffer_size(0);
  let s1 = Serf::<T>::new(transport_opts1, opts).await.unwrap();

  let opts = test_config().with_event_buffer_size(0);
  let s2 = Serf::<T>::new(transport_opts2, opts).await.unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let (id, addr) = serfs[1].memberlist().advertise_node().into_components();

  serfs[0]
    .join(Node::new(id, MaybeResolvedAddress::resolved(addr)), false)
    .await
    .unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[0]
    .user_event("test", Bytes::from_static(b"test"), false)
    .await
    .unwrap();

  serfs[0].query("foo", Bytes::new(), None).await.unwrap();

  // s2 can leave now
  serfs[1].leave().await.unwrap();

  // Do a state dump
  let buf = serfs[0]
    .memberlist()
    .delegate()
    .unwrap()
    .local_state(false)
    .await;

  // Attempt a decode
  let pp = crate::types::decode_message::<T::Id, T::ResolvedAddress>(&buf).unwrap();

  let MessageRef::PushPull(pp) = pp else {
    panic!("bad message")
  };
  let pp = <PushPullMessage<T::Id> as Data>::from_ref(pp).unwrap();

  // Verify lamport clock
  assert_eq!(pp.ltime(), serfs[0].inner.clock.time(), "bad lamport clock");

  // Verify the status
  // Leave waits until propagation so this should only have one member
  assert_eq!(pp.status_ltimes().len(), 1, "missing ltimes");
  assert_eq!(pp.left_members().len(), 0, "should have no left memebers");
  assert_eq!(
    pp.event_ltime(),
    serfs[0].inner.event_clock.time(),
    "bad event clock"
  );
  assert_eq!(
    pp.events().len(),
    serfs[0].inner.event_core.read().await.buffer.len(),
    "should send full event buffer"
  );
  assert_eq!(
    pp.query_ltime(),
    serfs[0].inner.query_clock.time(),
    "bad query clock"
  );

  for s in serfs {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for delegate merge remote state
pub async fn delegate_merge_remote_state<T>(transport_opts: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s = Serf::<T>::new(transport_opts, opts).await.unwrap();
  let d = s.memberlist().delegate().unwrap();

  // Make a fake push pull
  let pp = PushPullMessage {
    ltime: 42.into(),
    status_ltimes: [
      (SmolStr::new("test"), 20.into()),
      (SmolStr::new("foo"), 15.into()),
    ]
    .into_iter()
    .collect(),
    left_members: ["foo".into()].into_iter().collect(),
    event_ltime: 50.into(),
    events: TinyVec::from(UserEvents {
      ltime: 45.into(),
      events: OneOrMore::from(UserEvent {
        name: "test".into(),
        payload: Bytes::new(),
      }),
    }),
    query_ltime: 100.into(),
  };

  let buf = crate::types::encode_message_to_bytes(&pp).unwrap();

  // Merge in fake state
  d.merge_remote_state(&buf, false).await;

  // Verify lamport
  assert_eq!(s.inner.clock.time(), 42.into(), "bad lamport clock");

  let members = s.inner.members.read().await;
  // Verify pending join for test
  let ltime = recent_intent(
    &members.recent_intents,
    &SmolStr::new("test"),
    MessageType::Join,
  )
  .unwrap();
  assert_eq!(ltime, 20.into(), "bad join ltime");
  // Verify pending leave for foo
  let ltime = recent_intent(
    &members.recent_intents,
    &SmolStr::new("foo"),
    MessageType::Leave,
  )
  .unwrap();
  assert_eq!(ltime, 16.into(), "bad leave ltime");

  // Verify event clock
  assert_eq!(s.inner.event_clock.time(), 50.into(), "bad event clock");
  let buf = s.inner.event_core.read().await;
  assert!(buf.buffer[45].is_some(), "missing event buffer for time");
  assert_eq!(buf.buffer[45].as_ref().unwrap().events[0].name, "test");
  assert_eq!(s.inner.query_clock.time(), 100.into(), "bad query clock");

  s.shutdown().await.unwrap();
}

/// Unit test for serf ping delegate versioning
pub async fn serf_ping_delegate_versioning<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
) where
  T: Transport,
{
  const PROBE_INTERVAL: Duration = Duration::from_millis(2);

  let s1 = Serf::<T>::new(
    transport_opts1,
    test_config()
      .with_disable_coordinates(false)
      .with_memberlist_options(memberlist_core::Options::lan().with_probe_interval(PROBE_INTERVAL)),
  )
  .await
  .unwrap();

  let s2 = Serf::<T>::new(
    transport_opts2,
    test_config()
      .with_disable_coordinates(false)
      .with_memberlist_options(memberlist_core::Options::lan().with_probe_interval(PROBE_INTERVAL)),
  )
  .await
  .unwrap();

  // Monkey patch s1 to send weird versions of the ping messages.
  s1.memberlist()
    .delegate()
    .unwrap()
    .ping_versioning_test
    .store(true, Ordering::SeqCst);

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .memberlist()
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  // They both should show 2 members, but only s1 should know about s2
  // in the cache, since s1 spoke an alien ping protocol.
  wait_until_num_nodes(2, &serfs).await;

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  let s1id = serfs[0].local_id().clone();
  let s2id = serfs[1].local_id().clone();

  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    if serfs[0].cached_coordinate(&s2id).unwrap().is_some() {
      cond1 = true;
    } else if start.elapsed() > Duration::from_secs(7) {
      panic!("s1 didn't get a coordinate for s2");
    }

    if serfs[1].cached_coordinate(&s1id).unwrap().is_none() {
      cond2 = true;
    } else if start.elapsed() > Duration::from_secs(7) {
      panic!("s2 got an unexpected coordinate for s1");
    }

    if cond1 && cond2 {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("s1: {} s2: {}", cond1, cond2);
    }
  }

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for serf ping delegate rogue coordinate
pub async fn serf_ping_delegate_rogue_coordinate<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
) where
  T: Transport,
{
  const PROBE_INTERVAL: Duration = Duration::from_millis(2);

  let opts = test_config().with_disable_coordinates(false);
  let s1 = Serf::<T>::new(
    transport_opts1,
    opts
      .with_memberlist_options(memberlist_core::Options::lan().with_probe_interval(PROBE_INTERVAL)),
  )
  .await
  .unwrap();

  let opts = test_config().with_disable_coordinates(false);
  let s2 = Serf::<T>::new(
    transport_opts2,
    opts
      .with_memberlist_options(memberlist_core::Options::lan().with_probe_interval(PROBE_INTERVAL)),
  )
  .await
  .unwrap();

  // Monkey patch s1 to send ping messages with bad coordinates.
  s1.memberlist()
    .delegate()
    .unwrap()
    .ping_dimension_test
    .store(true, Ordering::SeqCst);

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .memberlist()
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  // They both should show 2 members, but only s1 should know about s2
  // in the cache, since s1 spoke an alien ping protocol.
  wait_until_num_nodes(2, &serfs).await;

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  let s1id = serfs[0].local_id().clone();
  let s2id = serfs[1].local_id().clone();

  // They both should show 2 members, but only s1 should know about s2
  // in the cache, since s1 sent a bad coordinate.
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let s1c = serfs[0].cached_coordinate(&s2id).unwrap();
    // println!("s1c {:?}", s1c);
    if s1c.is_some() {
      cond1 = true;
    } else if start.elapsed() > Duration::from_secs(7) {
      panic!("s1 didn't get a coordinate for s2");
    }

    let s2c = serfs[1].cached_coordinate(&s1id).unwrap();
    // println!("s2c {:?}", s2c);
    if s2c.is_none() {
      cond2 = true;
    } else if start.elapsed() > Duration::from_secs(7) {
      panic!("s2 got an unexpected coordinate for s1");
    }

    if cond1 && cond2 {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("s1: {} s2: {}", cond1, cond2);
    }
  }

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}
