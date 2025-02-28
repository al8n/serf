use std::io::Read;

use super::*;

/// Unit test for the snapshoter.
pub async fn snapshoter<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
  T: Transport<Id = SmolStr>,
{
  let dir = tempfile::tempdir().unwrap();
  let p = dir.path().join("snapshoter");
  let s = Serf::<T>::new(transport_opts, test_config()).await.unwrap();

  let clock = LamportClock::new();
  let (out_tx, out_rx) = async_channel::bounded(64);
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();
  let (event_tx, _, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // Write some user events
  let ue = UserEventMessage::default()
    .with_ltime(42.into())
    .with_name("bar".into());
  event_tx.send(ue.clone().into()).await.unwrap();

  // Write some queries
  let qe = QueryEvent {
    ltime: 50.into(),
    name: "bar".into(),
    payload: Default::default(),
    ctx: Arc::new(QueryContext {
      query_timeout: Duration::default(),
      span: Mutex::new(None),
      this: s,
    }),
    id: 0,
    from: Node::new("baz".into(), addr.clone()),
    relay_factor: 0,
  };
  event_tx.send(qe.clone().into()).await.unwrap();

  // Write some membership events
  clock.witness(100.into());

  let mejoin = MemberEvent {
    ty: MemberEventType::Join,
    members: TinyVec::from(Member::new(
      Node::new("foo".into(), addr.clone()),
      Default::default(),
      MemberStatus::None,
    ))
    .into(),
  };

  let mefail = MemberEvent {
    ty: MemberEventType::Failed,
    members: TinyVec::from(Member::new(
      Node::new("foo".into(), addr.clone()),
      Default::default(),
      MemberStatus::None,
    ))
    .into(),
  };

  event_tx.send(mejoin.clone().into()).await.unwrap();
  event_tx.send(mefail.clone().into()).await.unwrap();
  event_tx.send(mejoin.clone().into()).await.unwrap();

  // Check these get passed through
  futures::select! {
    e = out_rx.recv().fuse() => {
      let e = e.unwrap();
      match e {
        CrateEvent::User(e) => {
          assert_eq!(e, ue);
        },
        _ => panic!("expected user event"),
      }
    },
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(200)).fuse() => {
      panic!("timeout");
    }
  }

  futures::select! {
    e = out_rx.recv().fuse() => {
      let e = e.unwrap();
      match e {
        CrateEvent::Query(e) => {
          if qe.ne(&e) {
            panic!("expected query event mismatch");
          }
        },
        _ => panic!("expected query event"),
      }
    },
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(200)).fuse() => {
      panic!("timeout");
    }
  }

  futures::select! {
    e = out_rx.recv().fuse() => {
      let e = e.unwrap();
      match e {
        CrateEvent::Member(e) => {
          assert_eq!(e, mejoin);
        },
        _ => panic!("expected member event"),
      }
    },
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(200)).fuse() => {
      panic!("timeout");
    }
  }

  futures::select! {
    e = out_rx.recv().fuse() => {
      let e = e.unwrap();
      match e {
        CrateEvent::Member(e) => {
          assert_eq!(e, mefail);
        },
        _ => panic!("expected member event"),
      }
    },
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(200)).fuse() => {
      panic!("timeout");
    }
  }

  futures::select! {
    e = out_rx.recv().fuse() => {
      let e = e.unwrap();
      match e {
        CrateEvent::Member(e) => {
          assert_eq!(e, mejoin);
        },
        _ => panic!("expected member event"),
      }
    },
    _ = <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(200)).fuse() => {
      panic!("timeout");
    }
  }

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;

  // Open the snapshoter
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();

  assert_eq!(res.last_clock, 100.into());
  assert_eq!(res.last_event_clock, 42.into());
  assert_eq!(res.last_query_clock, 50.into());

  let (out_tx, _out_rx) = async_channel::bounded(64);
  let (_event_tx, alive_nodes, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  assert_eq!(alive_nodes.len(), 1);
  let n = &alive_nodes[0];
  assert_eq!(n.id(), "foo");
  assert_eq!(n.address().clone().into_resolved().unwrap(), addr);

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;

  // Open the snapshoter, make sure nothing dies reading with coordinates
  // disabled.
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();

  let (out_tx, _out_rx) = async_channel::bounded(64);
  let (_event_tx, _, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();
  shutdown_tx.close();
  handle.wait().await;
}

/// Unit test for the snapshoter force compact.
pub async fn snapshoter_force_compact<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
  T: Transport<Id = SmolStr>,
{
  let dir = tempfile::tempdir().unwrap();
  let p = dir.path().join("snapshoter_force_compact");
  let s = Serf::<T>::new(transport_opts, test_config()).await.unwrap();

  let clock = LamportClock::new();
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);

  // Create a very low limit
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();
  let (out_tx, _out_rx) = async_channel::unbounded();
  let (event_tx, _, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    1024,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // Write lots of user events
  for i in 0..1024 {
    let ue = UserEventMessage::default().with_ltime(i.into());
    event_tx.send(ue.clone().into()).await.unwrap();
  }

  // Write lots of queries
  for i in 0..1024 {
    let qe = QueryEvent {
      ltime: i.into(),
      name: "bar".into(),
      payload: Default::default(),
      ctx: Arc::new(QueryContext {
        query_timeout: Duration::default(),
        span: Mutex::new(None),
        this: s.clone(),
      }),
      id: 0,
      from: Node::new("baz".into(), addr.clone()),
      relay_factor: 0,
    };
    event_tx.send(qe.clone().into()).await.unwrap();
  }

  // Wait for drain
  while !event_tx.is_empty() {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(20)).await;
  }

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;

  // Open the snapshoter
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();

  assert_eq!(res.last_event_clock, 1023.into());
  assert_eq!(res.last_query_clock, 1023.into());
}

/// Unit test for the snapshoter leave
pub async fn snapshoter_leave<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
  T: Transport<Id = SmolStr>,
{
  let dir = tempfile::tempdir().unwrap();
  let p = dir.path().join("snapshoter_leave");
  let s = Serf::<T>::new(transport_opts, test_config()).await.unwrap();

  let clock = LamportClock::new();
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();
  let (out_tx, _out_rx) = async_channel::unbounded();
  let (event_tx, _, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // Write a user event
  let ue = UserEventMessage::default()
    .with_ltime(42.into())
    .with_name("bar".into());
  event_tx.send(ue.clone().into()).await.unwrap();

  // Write a query
  let qe = QueryEvent {
    ltime: 50.into(),
    name: "uptime".into(),
    payload: Default::default(),
    ctx: Arc::new(QueryContext {
      query_timeout: Duration::default(),
      span: Mutex::new(None),
      this: s.clone(),
    }),
    id: 0,
    from: Node::new("baz".into(), addr.clone()),
    relay_factor: 0,
  };
  event_tx.send(qe.clone().into()).await.unwrap();

  // Write some member events
  clock.witness(100.into());

  let mejoin = MemberEvent {
    ty: MemberEventType::Join,
    members: TinyVec::from(Member::new(
      Node::new("foo".into(), addr.clone()),
      Default::default(),
      MemberStatus::None,
    ))
    .into(),
  };
  event_tx.send(mejoin.clone().into()).await.unwrap();

  // wait for drain
  while !event_tx.is_empty() {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(20)).await;
  }

  // Leave the cluster!
  handle.leave().await;

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;
  s.shutdown().await.unwrap();
  drop(s);

  // Open the snapshoter
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, false).unwrap();
  assert!(res.last_clock == 0.into(), "last_clock: {}", res.last_clock);
  assert!(
    res.last_event_clock == 0.into(),
    "last_event_clock: {}",
    res.last_event_clock
  );
  assert!(
    res.last_query_clock == 0.into(),
    "last_query_clock: {}",
    res.last_query_clock
  );
  let (out_tx, _out_rx) = async_channel::unbounded();
  let (_, alive_nodes, _) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  assert!(alive_nodes.is_empty());

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;
}

/// Unit test for the snapshoter leave rejoin
pub async fn snapshoter_leave_rejoin<T>(
  transport_opts: T::Options,
  addr: T::ResolvedAddress,
) where
  T: Transport<Id = SmolStr>,
{
  let dir = tempfile::tempdir().unwrap();
  let p = dir.path().join("snapshoter_leave_rejoin");
  let s = Serf::<T>::new(transport_opts, test_config()).await.unwrap();

  let clock = LamportClock::new();
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, true).unwrap();
  let (out_tx, _out_rx) = async_channel::unbounded();
  let (event_tx, _, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    true,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // Write a user event
  let ue = UserEventMessage::default()
    .with_ltime(42.into())
    .with_name("bar".into());
  event_tx.send(ue.clone().into()).await.unwrap();

  // Write a query
  let qe = QueryEvent {
    ltime: 50.into(),
    name: "uptime".into(),
    payload: Default::default(),
    ctx: Arc::new(QueryContext {
      query_timeout: Duration::default(),
      span: Mutex::new(None),
      this: s.clone(),
    }),
    id: 0,
    from: Node::new("baz".into(), addr.clone()),
    relay_factor: 0,
  };
  event_tx.send(qe.clone().into()).await.unwrap();

  // Write some member events
  clock.witness(100.into());

  let mejoin = MemberEvent {
    ty: MemberEventType::Join,
    members: TinyVec::from(Member::new(
      Node::new("foo".into(), addr.clone()),
      Default::default(),
      MemberStatus::None,
    ))
    .into(),
  };
  event_tx.send(mejoin.clone().into()).await.unwrap();

  // wait for drain
  while !event_tx.is_empty() {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(20)).await;
  }

  // Leave the cluster!
  handle.leave().await;
  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;

  let mut f = std::fs::File::open(&p).unwrap();
  let mut d = vec![];
  f.read_to_end(&mut d).unwrap();

  s.shutdown().await.unwrap();
  drop(s);

  // Open the snapshoter
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, DefaultDelegate<T>, _>(&p, true).unwrap();
  assert!(res.last_clock == 100.into());
  assert!(res.last_event_clock == 42.into());
  assert!(res.last_query_clock == 50.into());
  let (out_tx, _out_rx) = async_channel::unbounded();
  let (_, alive_nodes, handle) = Snapshot::<T, DefaultDelegate<T>>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    false,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  assert!(!alive_nodes.is_empty());

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;
}

/// Unit tests for the serf snapshot recovery
pub async fn serf_snapshot_recovery<T, F>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  get_transport: impl FnOnce(T::Id, T::ResolvedAddress) -> F + Copy,
) where
  T: Transport,
  T::Options: Clone,
  F: core::future::Future<Output = T::Options>,
{
  let td = tempfile::tempdir().unwrap();
  let snap_path = td.path().join("serf_snapshot_recovery");
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();
  let s2 = Serf::<T>::new(
    transport_opts2.clone(),
    test_config().with_snapshot_path(Some(snap_path.clone())),
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

  // Fire a user event
  serfs[0]
    .user_event("event!", Bytes::from_static(b"test"), false)
    .await
    .unwrap();
  <T::Runtime as RuntimeLite>::sleep(Duration::from_secs(10)).await;

  // Now force the shutdown of s2 so it appears to fail.
  serfs[1].shutdown().await.unwrap();
  let (s2id, s2addr) = serfs[1].advertise_node().into_components();
  let t = serfs[1].inner.opts.memberlist_options.probe_interval * 10;
  let _ = serfs.pop().unwrap();

  <T::Runtime as RuntimeLite>::sleep(t).await;

  // Verify that s2 is "failed"
  {
    let members = serfs[0].inner.members.read().await;
    test_member_status(&members.states, node.id().clone(), MemberStatus::Failed).unwrap();
  }

  // Now remove the failed node
  serfs[0]
    .remove_failed_node(node.id().clone())
    .await
    .unwrap();

  // Verify that s2 is gone
  {
    let members = serfs[0].inner.members.read().await;
    test_member_status(&members.states, node.id().clone(), MemberStatus::Left).unwrap();
  }

  // Listen for events
  let (event_tx, event_rx) = EventProducer::bounded(4);
  let s2 = Serf::<T>::with_event_producer(
    get_transport(s2id, s2addr).await,
    test_config().with_snapshot_path(Some(snap_path.clone())),
    event_tx,
  )
  .await
  .unwrap();

  // Wait for the node to auto rejoin
  let start = Epoch::now();
  while start.elapsed() < Duration::from_secs(1) {
    let members = serfs[0].members().await;
    if members.len() == 2
      && members[0].status == MemberStatus::Alive
      && members[1].status == MemberStatus::Alive
    {
      break;
    }
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(10)).await;
  }

  serfs.push(s2);

  // Verify that s2 is "alive"
  {
    let node = serfs[1].local_id().clone();
    let members = serfs[0].inner.members.read().await;
    test_member_status(&members.states, node, MemberStatus::Alive).unwrap();
  }
  {
    let node = serfs[0].local_id().clone();
    let members = serfs[1].inner.members.read().await;
    test_member_status(&members.states, node, MemberStatus::Alive).unwrap();
  }

  // Check the events to make sure we got nothing
  test_user_events(event_rx.rx, vec![], vec![]).await;

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn test_snapshoter_slow_disk_not_blocking_event_tx() {
  use memberlist_core::{
    agnostic_lite::tokio::TokioRuntime,
    transport::{Lpe, resolver::socket_addr::SocketAddrResolver, tests::UnimplementedTransport},
  };
  use std::net::SocketAddr;

  crate::tests::initialize_tests_tracing();

  type Transport = UnimplementedTransport<
    SmolStr,
    SocketAddrResolver<TokioRuntime>,
    Lpe<SmolStr, SocketAddr>,
    TokioRuntime,
  >;

  type Delegate = DefaultDelegate<Transport>;

  let dir = tempfile::tempdir().unwrap();
  let p = dir
    .path()
    .join("snapshoter_slow_disk_not_blocking_event_tx");

  let clock = LamportClock::new();
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let (out_tx, out_rx) = async_channel::bounded(1024);
  let res = open_and_replay_snapshot::<_, _, Delegate, _>(&p, true).unwrap();
  let (event_tx, _, handle) = Snapshot::<Transport, Delegate>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    true,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // We need enough events to be much more than the buffers used which are size
  // 1024. This number processes easily within the 500ms we allow below on my
  // host provided there is no disk IO on the path (I verified that by just
  // returning early in tryAppend using the old blocking code). The new async
  // method should pass without disabling disk writes too!
  let num_events = 10000;

  // Write lots of member updates (way bigger than our chan buffers)
  let (start_tx, start_rx) = async_channel::bounded::<()>(1);

  TokioRuntime::spawn_detach(async move {
    let _ = start_rx.recv().await;

    for i in 0..num_events {
      let mut e = MemberEvent {
        ty: MemberEventType::Join,
        members: TinyVec::from(Member::new(
          Node::new(
            format!("foo{i}").into(),
            format!("127.0.{}.{}:5000", (i / 256) % 256, i % 256)
              .parse()
              .unwrap(),
          ),
          Default::default(),
          MemberStatus::None,
        ))
        .into(),
      };

      if i % 10 == 0 {
        e.ty = MemberEventType::Leave;
      }
      event_tx.send(e.into()).await.unwrap();
    }
    // Pace ourselves - if we just throw these out as fast as possible the
    // read loop below can't keep up and we end up dropping messages due to
    // backpressure. But we need to still send them all in well less than the
    // timeout, 10k messages at 1 microsecond should take 10 ms minimum. In
    // practice it's quite a bit more to actually process and because the
    // buffer here blocks.
    TokioRuntime::sleep(Duration::from_micros(1)).await;
  });

  // Wait for them all to process through and it should be in a lot less time
  // than if the disk IO was in serial. This was verified by running this test
  // against the old serial implementation and seeing it never come close to
  // passing on my laptop with an SSD. It's not the most robust thing ever but
  // it's at least a sanity check that we are non-blocking now, and it passes
  // reliably at least on my machine. I typically see this complete in around
  // 115ms on my machine so this should give plenty of headroom for slower CI
  // environments while still being low enough that actual disk IO would
  // reliably blow it.
  let deadline = TokioRuntime::sleep_until(std::time::Instant::now() + Duration::from_millis(500));
  futures::pin_mut!(deadline);
  let mut num_recvd = 0;
  let start = Epoch::now();

  while num_recvd < num_events {
    futures::select! {
      _ = start_tx.send(()).fuse() => {
        continue;
      },
      _ = out_rx.recv().fuse() => {
        num_recvd += 1;
      },
      _ = (&mut deadline).fuse() => {
        panic!("timed out after {:?} waiting for messages blocked on fake disk IO? got {} of {}", start.elapsed(), num_recvd, num_events);
      }
    }
  }

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn test_snapshoter_slow_disk_not_blocking_memberlist() {
  use memberlist_core::{
    agnostic_lite::tokio::TokioRuntime,
    transport::{Lpe, resolver::socket_addr::SocketAddrResolver, tests::UnimplementedTransport},
  };
  use std::net::SocketAddr;

  type Transport = UnimplementedTransport<
    SmolStr,
    SocketAddrResolver<TokioRuntime>,
    Lpe<SmolStr, SocketAddr>,
    TokioRuntime,
  >;

  type Delegate = DefaultDelegate<Transport>;

  let dir = tempfile::tempdir().unwrap();
  let p = dir
    .path()
    .join("snapshoter_slow_disk_not_blocking_memberlist");

  let clock = LamportClock::new();
  let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
  let (out_tx, _out_rx) = async_channel::bounded(1);
  let res = open_and_replay_snapshot::<_, _, Delegate, _>(&p, true).unwrap();
  let (event_tx, _, handle) = Snapshot::<Transport, Delegate>::from_replay_result(
    res,
    SNAPSHOT_SIZE_LIMIT,
    true,
    clock.clone(),
    out_tx,
    shutdown_rx.clone(),
    #[cfg(feature = "metrics")]
    Default::default(),
  )
  .unwrap();

  // We need enough events to be more than the internal buffer sizes
  let num_events = 3000;

  for i in 0..num_events {
    let mut e = MemberEvent {
      ty: MemberEventType::Join,
      members: TinyVec::from(Member::new(
        Node::new(
          format!("foo{i}").into(),
          format!("127.0.{}.{}:5000", (i / 256) % 256, i % 256)
            .parse()
            .unwrap(),
        ),
        Default::default(),
        MemberStatus::None,
      ))
      .into(),
    };

    if i % 10 == 0 {
      e.ty = MemberEventType::Leave;
    }

    futures::select! {
      _ = event_tx.send(e.into()).fuse() => {},
      default => {
        panic!("event_tx should never block");
      }
    }

    // Allow just the tiniest time so that the runtime can schedule the
    // task that's reading this even if they are both on the same physical
    // core (like in CI).
    TokioRuntime::sleep(Duration::from_micros(1)).await;
  }

  // Close the snapshoter
  shutdown_tx.close();
  handle.wait().await;
}
