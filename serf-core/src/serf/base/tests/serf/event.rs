use serf_proto::{Filter, FilterType};

use super::*;

/// Unit tests for the user event old message
pub async fn user_event_old_message<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let event_buffer = opts.event_buffer_size;
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();

  // increase the ltime artificially
  s1.inner
    .event_clock
    .witness(((event_buffer + 1000) as u64).into());
  assert!(
    !s1
      .handle_user_event(
        UserEventMessage::default()
          .with_ltime(1.into())
          .with_name("old".into())
      )
      .await,
    "should not rebroadcast"
  );
  s1.shutdown().await.unwrap();
}

/// Unit tests for the user event smae clock
pub async fn user_event_same_clock<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts, opts, event_tx)
    .await
    .unwrap();

  let msg = UserEventMessage::default()
    .with_ltime(1.into())
    .with_name("first".into())
    .with_payload(Bytes::from_static(b"test"));
  assert!(s1.handle_user_event(msg).await, "should rebroadcast");

  let msg = UserEventMessage::default()
    .with_ltime(1.into())
    .with_name("first".into())
    .with_payload(Bytes::from_static(b"newpayload"));
  assert!(s1.handle_user_event(msg).await, "should rebroadcast");

  let msg = UserEventMessage::default()
    .with_ltime(1.into())
    .with_name("second".into())
    .with_payload(Bytes::from_static(b"other"));
  assert!(s1.handle_user_event(msg).await, "should rebroadcast");

  test_user_events(
    event_rx.rx,
    ["first", "first", "second"]
      .into_iter()
      .map(Into::into)
      .collect(),
    ["test", "newpayload", "other"]
      .into_iter()
      .map(Into::into)
      .collect(),
  )
  .await;

  s1.shutdown().await.unwrap();
}

/// Unit tests for the events failed
pub async fn serf_events_failed<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();
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
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[1].shutdown().await.unwrap();

  wait_until_num_nodes(1, &serfs[..1]).await;

  // Since s2 shutdown, we check the events to make sure we got failures.
  let node = serfs[1].inner.memberlist.local_id().clone();
  test_events(
    event_rx.rx,
    node,
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Failed),
      CrateEventType::Member(MemberEventType::Reap),
    ]
    .into_iter()
    .collect(),
  )
  .await;
}

/// Unit tests for the events join
pub async fn serf_events_join<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();
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
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  let node = serfs[1].inner.memberlist.local_id().clone();
  test_events(
    event_rx.rx,
    node,
    [CrateEventType::Member(MemberEventType::Join)]
      .into_iter()
      .collect(),
  )
  .await;

  for s in serfs {
    let _ = s.shutdown().await;
  }
}

/// Unit tests for the events leave
/// Unit tests for the events failed
pub async fn serf_events_leave<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(
    transport_opts1,
    test_config().with_reap_interval(Duration::from_secs(30)),
    event_tx,
  )
  .await
  .unwrap();
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

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, node.id().clone(), MemberStatus::Left).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  // Now that s2 has left, we check the events to make sure we got
  // a leave event in s1 about the leave.
  let node = serfs[1].inner.memberlist.local_id().clone();
  test_events(
    event_rx.rx,
    node,
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Leave),
    ]
    .into_iter()
    .collect(),
  )
  .await;
}

#[derive(Debug, Clone)]
struct DropJoins {
  drop: Arc<AtomicUsize>,
}

impl MessageDropper for DropJoins {
  fn should_drop(&self, ty: MessageType) -> bool {
    match ty {
      MessageType::Join | MessageType::PushPull => self.drop.load(Ordering::SeqCst) == 1,
      _ => false,
    }
  }
}

impl DropJoins {
  fn new() -> Self {
    Self {
      drop: Arc::new(AtomicUsize::new(0)),
    }
  }
}

/// Unit tests for the events leave avoid infinite leave rebroadcast
pub async fn serf_events_leave_avoid_infinite_rebroadcast<T, F>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  transport_opts3: T::Options,
  transport_opts4: T::Options,
  get_transport_opts: impl FnOnce(T::Id, <T::Resolver as AddressResolver>::ResolvedAddress) -> F + Copy,
) where
  T: Transport,
  F: core::future::Future<Output = T::Options>,
{
  // This test is a variation of the normal leave test that is crafted
  // specifically to handle a situation where two unique leave events for the
  // same node reach two other nodes in the wrong order which causes them to
  // infinitely rebroadcast the leave event without updating their own
  // lamport clock for that node.
  let config_local = |opts: Options| opts.with_reap_interval(Duration::from_secs(30));

  let (event_tx1, event_rx1) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts1, config_local(test_config()), event_tx1)
    .await
    .unwrap();
  let s2 = Serf::<T>::new(transport_opts2, config_local(test_config()))
    .await
    .unwrap();

  // Allow s3 and s4 to drop joins in the future.
  let d = DropJoins::new();
  let s3 = Serf::<T>::with_message_dropper(
    transport_opts3,
    config_local(test_config()),
    Box::new(d.clone()),
  )
  .await
  .unwrap();
  let s4 = Serf::<T>::with_message_dropper(
    transport_opts4,
    config_local(test_config()),
    Box::new(d.clone()),
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
  s3.join(node.clone(), false).await.unwrap();
  s4.join(node.clone(), false).await.unwrap();

  // S2 leaves gracefully
  serfs[1].leave().await.unwrap();
  serfs[1].shutdown().await.unwrap();

  // Make s3 and s4 drop inbound join messages and push-pulls for a bit so it won't see
  // s2 rejoin
  d.drop.store(1, Ordering::SeqCst);
  let s2node = serfs[1].advertise_node();
  let (s2id, s2addr) = s2node.clone().into_components();
  let _ = serfs.pop().unwrap();

  // Bring back s2 by mimicking its name and address
  <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(2)).await;

  let s2 = Serf::<T>::new(
    get_transport_opts(s2id, s2addr).await,
    config_local(test_config().with_rejoin_after_leave(true)),
  )
  .await
  .unwrap();

  let s1node = serfs[0]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  s2.join(s1node.clone(), false).await.unwrap();

  serfs.push(s2);
  serfs.push(s3);
  serfs.push(s4);

  wait_until_num_nodes(4, &serfs).await;

  // Now leave a second time but before s3 and s4 see the rejoin (due to the gate)
  serfs[1].leave().await.unwrap();

  let s2 = serfs.remove(1);
  wait_until_intent_queue_len(0, &serfs).await;

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  let mut cond3 = false;
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    if !cond1 {
      let members = serfs[0].inner.members.read().await;
      if test_member_status(&members.states, node.id().clone(), MemberStatus::Left).is_ok() {
        cond1 = true;
      }
    }

    if !cond2 {
      let members = serfs[1].inner.members.read().await;
      if test_member_status(&members.states, node.id().clone(), MemberStatus::Left).is_ok() {
        cond2 = true;
      }
    }

    if !cond3 {
      let members = serfs[2].inner.members.read().await;
      if test_member_status(&members.states, node.id().clone(), MemberStatus::Left).is_ok() {
        cond3 = true;
      }
    }

    if cond1 && cond2 && cond3 {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  // Now that s2 has left, we check the events to make sure we got
  // a leave event in s1 about the leave.
  test_events(
    event_rx1.rx,
    s2.inner.memberlist.local_id().clone(),
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Leave),
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Leave),
    ]
    .into_iter()
    .collect(),
  )
  .await;
}

/// Unit tests for the remove failed events leave
pub async fn serf_remove_failed_events_leave<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
) where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();
  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node.clone(), false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval();
  <T::Runtime as RuntimeLite>::sleep(t * 15).await;

  serfs[0]
    .remove_failed_node(node.id().clone())
    .await
    .unwrap();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, node.id().clone(), MemberStatus::Left).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("timed out");
    }
  }

  // Now that s2 has failed and been marked as left, we check the
  // events to make sure we got a leave event in s1 about the leave.
  test_events(
    event_rx.rx,
    serfs[1].inner.memberlist.local_id().clone(),
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Failed),
      CrateEventType::Member(MemberEventType::Leave),
    ]
    .into_iter()
    .collect(),
  )
  .await;
}

/// Unit tests for the events user
pub async fn serf_event_user<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();
  let s2 = Serf::<T>::with_event_producer(transport_opts2, test_config(), event_tx)
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

  // Fire a user event
  serfs[0]
    .user_event("event!", Bytes::from_static(b"test"), false)
    .await
    .unwrap();

  // Fire a user event
  serfs[0]
    .user_event("second", Bytes::from_static(b"foobar"), false)
    .await
    .unwrap();

  // check the events to make sure we got
  // a leave event in s1 about the leave.
  test_user_events(
    event_rx.rx,
    ["event!", "second"].into_iter().map(Into::into).collect(),
    vec![Bytes::from_static(b"test"), Bytes::from_static(b"foobar")],
  )
  .await;
}

/// Unit tests for the events user size limit
pub async fn serf_event_user_size_limit<T>(transport_opts1: T::Options)
where
  T: Transport,
{
  let (event_tx, _event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();

  let serfs = [s1];
  wait_until_num_nodes(1, &serfs).await;

  let p = Bytes::from(vec![0; serfs[0].inner.opts.max_user_event_size]);
  let err = serfs[0]
    .user_event("this is too large an event", p, false)
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("user event exceeds"));
}

/// Unit test for default query
pub async fn default_query<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let s = [Serf::<T>::new(transport_opts, opts).await.unwrap()];

  wait_until_num_nodes(1, &s).await;
  let timeout = s[0].default_query_timeout().await;
  assert_eq!(
    timeout,
    s[0].inner.opts.memberlist_options.gossip_interval()
      * (s[0].inner.opts.query_timeout_mult as u32)
  );

  let params = s[0].default_query_param().await;

  assert!(params.filters.is_empty());
  assert!(!params.request_ack);
  assert_eq!(params.timeout, timeout);
}

/// Unit test for query params encode filters
pub async fn query_params_encode_filters<T>(transport_opts: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s = Serf::<T>::new(transport_opts, opts).await.unwrap();
  let mut params = s.default_query_param().await;
  params
    .filters
    .push(Filter::Id(["foo".into(), "bar".into()].into()));
  params.filters.push(Filter::Tag {
    tag: "role".into(),
    expr: "^web".into(),
  });
  params.filters.push(Filter::Tag {
    tag: "datacenter".into(),
    expr: "aws$".into(),
  });

  let filters = params.encode_filters::<DefaultDelegate<T>>().unwrap();
  assert_eq!(filters.len(), 3);

  let (_, node_filt) =
    decode_filter(&filters[0]).unwrap();
  assert_eq!(node_filt.ty(), FilterType::Id);

  let (_, tag_filt) =
    decode_filter(&filters[1]).unwrap();
  assert_eq!(tag_filt.ty(), FilterType::Tag);

  let (_, tag_filt) =
    decode_filter(&filters[2]).unwrap();
  assert_eq!(tag_filt.ty(), FilterType::Tag);
}

/// Unit test for should process functionallity
pub async fn should_process<T>(transport_opts: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let opts = test_config();
  let s = Serf::<T>::new(
    transport_opts,
    opts.with_tags([("role", "webserver"), ("datacenter", "east-aws")].into_iter()),
  )
  .await
  .unwrap();
  let mut params = s.default_query_param().await;
  params.filters.push(Filter::Id(
    [
      "foo".into(),
      "bar".into(),
      s.memberlist().local_id().clone(),
    ]
    .into_iter()
    .collect(),
  ));
  params.filters.push(Filter::Tag {
    tag: "role".into(),
    expr: "^web".into(),
  });
  params.filters.push(Filter::Tag {
    tag: "datacenter".into(),
    expr: "aws$".into(),
  });

  let filters = params.encode_filters::<DefaultDelegate<T>>().unwrap();
  assert_eq!(filters.len(), 3);

  assert!(s.should_process_query(&filters));

  // Omit node
  let mut params = s.default_query_param().await;
  params
    .filters
    .push(Filter::Id(["foo".into(), "bar".into()].into()));

  let filters = params.encode_filters::<DefaultDelegate<T>>().unwrap();
  assert!(!s.should_process_query(&filters));

  // Filter on missing tag
  let mut params = s.default_query_param().await;
  params.filters.push(Filter::Tag {
    tag: "other".into(),
    expr: "cool".into(),
  });

  let filters = params.encode_filters::<DefaultDelegate<T>>().unwrap();
  assert!(!s.should_process_query(&filters));

  // Bad tag
  let mut params = s.default_query_param().await;
  params.filters.push(Filter::Tag {
    tag: "role".into(),
    expr: "db".into(),
  });

  let filters = params.encode_filters::<DefaultDelegate<T>>().unwrap();
  assert!(!s.should_process_query(&filters));
}

/// Unit tests for the query old message
pub async fn query_old_message<T>(
  transport_opts: T::Options,
  from: Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
) where
  T: Transport,
{
  let opts = test_config();
  let event_buffer = opts.query_buffer_size;
  let s1 = Serf::<T>::new(transport_opts, opts).await.unwrap();
  // increase the ltime artificially
  s1.inner
    .query_clock
    .witness(((event_buffer + 1000) as u64).into());
  assert!(
    !s1
      .handle_query(
        QueryMessage {
          ltime: 1.into(),
          id: 0,
          from,
          filters: Default::default(),
          flags: QueryFlag::empty(),
          relay_factor: 0,
          timeout: Default::default(),
          name: "old".into(),
          payload: Bytes::new(),
        },
        None
      )
      .await,
    "should not rebroadcast"
  );

  s1.shutdown().await.unwrap();
}

/// Unit tests for the query same clock
pub async fn query_same_clock<T>(
  transport_opts: T::Options,
  from: Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
) where
  T: Transport,
{
  let opts = test_config();
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s1 = Serf::<T>::with_event_producer(transport_opts, opts, event_tx)
    .await
    .unwrap();

  let msg = QueryMessage {
    ltime: 1.into(),
    id: 1,
    from: from.clone(),
    filters: Default::default(),
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: Default::default(),
    name: "foo".into(),
    payload: Bytes::from_static(b"test"),
  };

  assert!(
    s1.handle_query(msg.clone(), None).await,
    "should rebroadcast"
  );
  assert!(
    !s1.handle_query(msg.clone(), None).await,
    "should not rebroadcast"
  );

  let msg = QueryMessage {
    ltime: 1.into(),
    id: 2,
    from: from.clone(),
    filters: Default::default(),
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: Default::default(),
    name: "bar".into(),
    payload: Bytes::from_static(b"newpayload"),
  };

  assert!(
    s1.handle_query(msg.clone(), None).await,
    "should rebroadcast"
  );
  assert!(
    !s1.handle_query(msg.clone(), None).await,
    "should not rebroadcast"
  );

  let msg = QueryMessage {
    ltime: 1.into(),
    id: 3,
    from: from.clone(),
    filters: Default::default(),
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: Default::default(),
    name: "baz".into(),
    payload: Bytes::from_static(b"other"),
  };
  assert!(
    s1.handle_query(msg.clone(), None).await,
    "should rebroadcast"
  );
  assert!(
    !s1.handle_query(msg.clone(), None).await,
    "should not rebroadcast"
  );

  test_query_events(
    event_rx.rx,
    ["foo", "bar", "baz"].into_iter().map(Into::into).collect(),
    ["test", "newpayload", "other"]
      .into_iter()
      .map(Into::into)
      .collect(),
  )
  .await;

  s1.shutdown().await.unwrap();
}

/// Unit test for serf query
pub async fn serf_query<T>(transport_opts1: T::Options, transport_opts2: T::Options)
where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);

  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();

  let wg = Arc::new(());
  let wg1 = wg.clone();
  let (ctx_tx, ctx_rx) = async_channel::bounded::<()>(1);
  scopeguard::defer!(ctx_tx.close(););

  <T::Runtime as RuntimeLite>::spawn_detach(async move {
    let _wg = wg1;
    loop {
      futures::select! {
        _ = ctx_rx.recv().fuse() => {
          break;
        },
        e = event_rx.rx.recv().fuse() => {
          let e = e.unwrap();
          match e.ty() {
            CrateEventType::Query => {
              match e {
                CrateEvent::Query(q) => {
                  q.respond(Bytes::from_static(b"test")).await.unwrap();
                  break;
                }
                _ => unreachable!(),
              }
            },
            _ => continue,
          }
        },
        _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(1)).fuse() => {
          tracing::error!("timeout");
          break;
        },
      }
    }
  });

  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();

  let serfs = [s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  // Start a query from s2;
  let mut params = serfs[1].default_query_param().await;
  params.request_ack = true;
  let resp = serfs[1]
    .query("load", Bytes::from_static(b"sup girl"), Some(params))
    .await
    .unwrap();

  let mut acks = vec![];
  let mut responses = vec![];

  let ack_rx = resp.ack_rx().unwrap();
  let resp_rx = resp.response_rx();

  for _ in 0..3 {
    futures::select! {
      a = ack_rx.recv().fuse() => {
        let a = a.unwrap();
        acks.push(a);
      },
      r = resp_rx.recv().fuse() => {
        let r = r.unwrap();
        assert_eq!(r.from, serfs[0].advertise_node());
        assert_eq!(r.payload, Bytes::from_static(b"test"));
        responses.push(r);
      },
      _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(1)).fuse() => {
        panic!("timeout");
      },
    }
  }
  assert_eq!(acks.len(), 2, "missing acks {acks:?}");
  assert_eq!(responses.len(), 1, "missing responses {responses:?}");
}

/// Unit test for serf query filter
pub async fn serf_query_filter<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  transport_opts3: T::Options,
) where
  T: Transport,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);

  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();

  let wg = Arc::new(());
  let wg1 = wg.clone();
  let (ctx_tx, ctx_rx) = async_channel::bounded::<()>(1);
  scopeguard::defer!(ctx_tx.close(););

  <T::Runtime as RuntimeLite>::spawn_detach(async move {
    let _wg = wg1;
    loop {
      futures::select! {
        _ = ctx_rx.recv().fuse() => {
          break;
        },
        e = event_rx.rx.recv().fuse() => {
          let e = e.unwrap();
          match e.ty() {
            CrateEventType::Query => {
              match e {
                CrateEvent::Query(q) => {
                  q.respond(Bytes::from_static(b"test")).await.unwrap();
                  break;
                }
                _ => unreachable!(),
              }
            },
            _ => continue,
          }
        },
        _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(1)).fuse() => {
          tracing::error!("timeout");
          break;
        },
      }
    }
  });

  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();

  let mut serfs = vec![s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  let s3 = Serf::<T>::new(transport_opts3, test_config())
    .await
    .unwrap();
  serfs.push(s3);

  wait_until_num_nodes(1, &serfs[2..]).await;

  let node = serfs[2]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(3, &serfs).await;

  // Filter to only s1!
  let mut params = serfs[1].default_query_param().await;
  params.filters.push(Filter::Id(
    [serfs[0].memberlist().local_id().clone()]
      .into_iter()
      .collect(),
  ));
  params.request_ack = true;
  params.relay_factor = 1;

  // Start a query from s2;
  let resp = serfs[1]
    .query("load", Bytes::from_static(b"sup girl"), Some(params))
    .await
    .unwrap();

  let mut acks = vec![];
  let mut responses = vec![];

  let ack_rx = resp.ack_rx().unwrap();
  let resp_rx = resp.response_rx();

  for _ in 0..2 {
    futures::select! {
      a = ack_rx.recv().fuse() => {
        let a = a.unwrap();
        acks.push(a);
      },
      r = resp_rx.recv().fuse() => {
        let r = r.unwrap();
        assert_eq!(r.from, serfs[0].advertise_node());
        assert_eq!(r.payload, Bytes::from_static(b"test"));
        responses.push(r);
      },
      _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(1)).fuse() => {
        panic!("timeout");
      },
    }
  }

  assert_eq!(acks.len(), 1, "missing acks {acks:?}");
  assert_eq!(responses.len(), 1, "missing responses {responses:?}");

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for serf query deduplicate
pub async fn serf_query_deduplicate<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let s = Serf::<T>::new(transport_opts, opts).await.unwrap();

  // Set up a dummy query and response
  let mq = QueryMessage {
    ltime: 123.into(),
    id: 123,
    from: s.advertise_node(),
    filters: Default::default(),
    flags: QueryFlag::ACK,
    relay_factor: 0,
    timeout: Duration::from_secs(1),
    name: Default::default(),
    payload: Default::default(),
  };
  let query = QueryResponse::from_query(&mq, 3);
  let mut response = QueryResponseMessage {
    ltime: mq.ltime,
    id: mq.id,
    from: s.advertise_node(),
    flags: QueryFlag::empty(),
    payload: Default::default(),
  };
  {
    let mut qc = s.inner.query_core.write().await;
    qc.responses.insert(mq.ltime, query.clone());
  }

  // Send a few duplicate responses
  s.handle_query_response(response.clone()).await;
  s.handle_query_response(response.clone()).await;
  response.flags |= QueryFlag::ACK;
  s.handle_query_response(response.clone()).await;
  s.handle_query_response(response.clone()).await;

  // Ensure we only get one NodeResponse off the channel
  let resp_rx = query.response_rx();
  futures::select! {
    _ = resp_rx.recv().fuse() => {},
    default => {
      panic!("should have a response")
    }
  }

  let ack_rx = query.ack_rx().unwrap();
  futures::select! {
    _ = ack_rx.recv().fuse() => {},
    default => {
      panic!("should have an ack")
    }
  }

  futures::select! {
    _ = resp_rx.recv().fuse() => {
      panic!("should not have a second response")
    },
    default => {}
  }

  futures::select! {
    _ = ack_rx.recv().fuse() => {
      panic!("should not have a second ack")
    },
    default => {}
  }
}

/// Unit test for serf query size limit
pub async fn serf_query_size_limit<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let size_limit = opts.query_size_limit;
  let s = Serf::<T>::new(transport_opts, opts).await.unwrap();

  let name = "this is too large a query";
  let payload = vec![0; size_limit];
  let Err(err) = s.query(name, payload, None).await else {
    panic!("expected error");
  };
  assert!(err.to_string().contains("query exceeds limit of"));
}

/// Unit test for serf query size limit increased
pub async fn serf_query_size_limit_increased<T>(transport_opts: T::Options)
where
  T: Transport,
{
  let opts = test_config();
  let size_limit = opts.query_size_limit;
  let s = Serf::<T>::new(transport_opts, opts.with_query_size_limit(size_limit * 2))
    .await
    .unwrap();

  let name = "this is too large a query";
  let payload = vec![0; size_limit];
  s.query(name, payload, None).await.unwrap();
}
