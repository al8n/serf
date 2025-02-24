use super::*;

/// Unit test for reap handler shutdown
pub async fn serf_reap_handler_shutdown<T>(opts: T::Options)
where
  T: Transport,
{
  let s = Serf::<T>::new(opts, test_config()).await.unwrap();

  // Make sure the reap handler exits on shutdown.
  let (tx, rx) = async_channel::bounded::<()>(1);

  let s1 = s.clone();
  let reap = Reaper {
    coord_core: s1.inner.coord_core.clone(),
    memberlist: s1.inner.memberlist.clone(),
    members: s1.inner.members.clone(),
    event_tx: s1.inner.event_tx.clone(),
    shutdown_rx: s1.inner.shutdown_rx.clone(),
    reap_interval: s1.inner.opts.reap_interval,
    reconnect_timeout: s1.inner.opts.reconnect_timeout,
    recent_intent_timeout: s1.inner.opts.recent_intent_timeout,
    tombstone_timeout: s1.inner.opts.tombstone_timeout,
  };
  <T::Runtime as RuntimeLite>::spawn_detach(async move {
    reap.run().await;
    tx.close();
  });

  s.shutdown().await.unwrap();

  futures::select! {
    _ = rx.recv().fuse() => {},
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(1)).fuse() => {
      panic!("reap handler did not exit");
    }
  }
}

/// Unit test for reap handler
pub async fn serf_reap_handler<T>(
  opts: T::Options,
  addr: <T::Resolver as AddressResolver>::ResolvedAddress,
) where
  T: Transport<Id = SmolStr>,
{
  let s = Serf::<T>::new(
    opts,
    test_config()
      .with_reap_interval(Duration::from_nanos(1))
      .with_tombstone_timeout(Duration::from_secs(6))
      .with_recent_intent_timeout(Duration::from_secs(7)),
  )
  .await
  .unwrap();

  {
    let mut members = s.inner.members.write().await;
    let n = Node::new("foo".into(), addr.clone());
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now()),
    });
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(5)),
    });
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(10)),
    });
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"alice".into(),
      MessageType::Join,
      1.into(),
      Epoch::now,
    );
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"bob".into(),
      MessageType::Join,
      2.into(),
      || Epoch::now() - Duration::from_secs(10),
    );
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"carol".into(),
      MessageType::Leave,
      1.into(),
      Epoch::now,
    );
    upsert_intent::<SmolStr>(
      &mut members.recent_intents,
      &"doug".into(),
      MessageType::Leave,
      2.into(),
      || Epoch::now() - Duration::from_secs(10),
    );
  }

  let s1 = s.clone();
  <T::Runtime as RuntimeLite>::spawn_detach(async move {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(1)).await;
    s1.shutdown().await.unwrap();
  });

  let reap = Reaper {
    coord_core: s.inner.coord_core.clone(),
    memberlist: s.inner.memberlist.clone(),
    members: s.inner.members.clone(),
    event_tx: s.inner.event_tx.clone(),
    shutdown_rx: s.inner.shutdown_rx.clone(),
    reap_interval: s.inner.opts.reap_interval,
    reconnect_timeout: s.inner.opts.reconnect_timeout,
    recent_intent_timeout: s.inner.opts.recent_intent_timeout,
    tombstone_timeout: s.inner.opts.tombstone_timeout,
  };
  reap.run().await;

  let members = s.inner.members.read().await;
  assert_eq!(members.left_members.len(), 2);

  recent_intent(&members.recent_intents, &"alice".into(), MessageType::Join).unwrap();
  assert!(recent_intent(&members.recent_intents, &"bob".into(), MessageType::Join).is_none());
  recent_intent(&members.recent_intents, &"carol".into(), MessageType::Leave).unwrap();
  assert!(recent_intent(&members.recent_intents, &"doug".into(), MessageType::Leave).is_none());
}

/// Unit test for reap
pub async fn serf_reap<T>(opts: T::Options, addr: <T::Resolver as AddressResolver>::ResolvedAddress)
where
  T: Transport<Id = SmolStr>,
{
  let s = Serf::<T>::new(opts, test_config()).await.unwrap();

  {
    let mut members = s.inner.members.write().await;
    let n = Node::new("foo".into(), addr.clone());
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now()),
    });
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(5)),
    });
    members.left_members.push(MemberState {
      member: Member::new(n.clone(), Default::default(), MemberStatus::None),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(10)),
    });

    let (tx, _rx) = async_channel::bounded(64);

    Reaper::<T, DefaultDelegate<T>>::reap_left(
      s.local_id(),
      &mut members,
      &tx,
      None,
      None,
      Duration::from_secs(6),
    )
    .await;
  }

  s.shutdown().await.unwrap();
}
