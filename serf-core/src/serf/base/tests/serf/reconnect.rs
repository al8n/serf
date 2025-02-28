use std::marker::PhantomData;

use memberlist_core::transport::resolver::socket_addr::SocketAddrResolver;

use crate::delegate::ReconnectDelegate;

use super::*;

/// Unit test for reconnect
pub async fn serf_reconnect<T, F>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
  get_transport: impl FnOnce(T::Id, T::ResolvedAddress) -> F + Copy,
) where
  T: Transport,
  F: core::future::Future<Output = T::Options>,
{
  let (event_tx, event_rx) = EventProducer::bounded(64);

  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();
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

  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval();
  let (s2id, s2addr) = serfs[1].advertise_node().into_components();
  let _ = serfs.pop().unwrap();
  <T::Runtime as RuntimeLite>::sleep(t * 15).await;

  // Bring back s2
  let s2 = Serf::<T>::new(get_transport(s2id, s2addr).await, test_config())
    .await
    .unwrap();
  serfs.push(s2);

  wait_until_num_nodes(2, &serfs).await;

  let node = serfs[1].local_id().clone();
  test_events(
    event_rx.rx,
    node,
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Failed),
      CrateEventType::Member(MemberEventType::Join),
    ]
    .into_iter()
    .collect(),
  )
  .await;

  for s in serfs.iter() {
    println!("start shutdown {}", s.local_id());
    s.shutdown().await.unwrap();
    println!("finish shutdown {}", s.local_id());
  }
}

/// Unit test for reconnect
pub async fn serf_reconnect_same_ip<T, R, F>(
  transport_opts1: T::Options,
  transport2_id: T::Id,
  get_transport: impl FnOnce(T::Id, T::ResolvedAddress) -> F + Copy,
) where
  T: Transport<Id = SmolStr, Resolver = SocketAddrResolver<R>>,
  T::Options: Clone,
  R: RuntimeLite,
  F: core::future::Future<Output = T::Options>,
{
  let (event_tx, event_rx) = EventProducer::bounded(64);

  let s1 = Serf::<T>::with_event_producer(transport_opts1, test_config(), event_tx)
    .await
    .unwrap();
  let s2addr = {
    let mut addr = *s1.advertise_node().address();
    let port = addr.port() + 1;
    addr.set_port(port);
    addr
  };
  let s2 = Serf::<T>::new(
    get_transport(transport2_id.clone(), s2addr).await,
    test_config(),
  )
  .await
  .unwrap();

  let ip1 = s1.inner.memberlist.advertise_address().ip();
  let ip2 = s2.inner.memberlist.advertise_address().ip();
  assert_eq!(ip1, ip2, "require same ip address 1: {ip1} 2: {ip2}");

  let mut serfs = vec![s1, s2];
  wait_until_num_nodes(1, &serfs).await;

  let node = serfs[1]
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  serfs[0].join(node, false).await.unwrap();

  wait_until_num_nodes(2, &serfs).await;

  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval();
  drop(serfs.pop().unwrap());
  <T::Runtime as RuntimeLite>::sleep(t * 10).await;

  // Bring back s2
  let s2 = Serf::<T>::new(
    get_transport(transport2_id.clone(), s2addr).await,
    test_config(),
  )
  .await
  .unwrap();
  serfs.push(s2);

  wait_until_num_nodes(2, &serfs).await;

  let node = serfs[1].local_id().clone();
  test_events(
    event_rx.rx,
    node,
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Failed),
      CrateEventType::Member(MemberEventType::Join),
    ]
    .into_iter()
    .collect(),
  )
  .await;
}

#[derive(Clone)]
struct ReconnectOverride<A> {
  timeout: Duration,
  called: Arc<AtomicBool>,
  _marker: PhantomData<A>,
}

impl<A> ReconnectDelegate for ReconnectOverride<A>
where
  A: CheapClone + Send + Sync + 'static,
{
  type Id = SmolStr;

  type Address = A;

  fn reconnect_timeout(
    &self,
    _member: &Member<Self::Id, Self::Address>,
    _timeout: Duration,
  ) -> Duration {
    self.called.store(true, Ordering::SeqCst);
    self.timeout
  }
}

/// Unit test for serf per node reconnect timeout
pub async fn serf_per_node_reconnect_timeout<T>(
  transport_opts1: T::Options,
  transport_opts2: T::Options,
) where
  T: Transport<Id = SmolStr>,
{
  let (event_tx, event_rx) = EventProducer::bounded(4);

  let ro1 = ReconnectOverride {
    timeout: Duration::from_micros(1),
    called: Arc::new(AtomicBool::new(false)),
    _marker: PhantomData,
  };

  let s1 = Serf::<T, _>::with_event_producer_and_delegate(
    transport_opts1,
    test_config().with_reconnect_timeout(Duration::from_secs(30)),
    event_tx,
    DefaultDelegate::<T>::new().with_reconnect_delegate(ro1.clone()),
  )
  .await
  .unwrap();
  let s2 = Serf::<T>::new(transport_opts2, test_config())
    .await
    .unwrap();

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;
    let n = s1.num_members().await;
    if n == 1 {
      cond1 = true;
    }
    if start.elapsed() > Duration::from_secs(7) {
      panic!("s1 got {} expected {}", n, 1);
    }

    let n = s2.num_members().await;
    if n == 1 {
      cond2 = true;
    }
    if start.elapsed() > Duration::from_secs(7) {
      panic!("s2 got {} expected {}", n, 1);
    }

    if cond1 && cond2 {
      break;
    }
  }

  let node = s2
    .advertise_node()
    .map_address(MaybeResolvedAddress::resolved);
  s1.join(node, false).await.unwrap();

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;
    let n = s1.num_members().await;
    if n == 2 {
      cond1 = true;
    }
    if start.elapsed() > Duration::from_secs(7) {
      panic!("s1 got {} expected {}", n, 2);
    }

    let n = s2.num_members().await;
    if n == 2 {
      cond2 = true;
    }
    if start.elapsed() > Duration::from_secs(7) {
      panic!("s2 got {} expected {}", n, 2);
    }

    if cond1 && cond2 {
      break;
    }
  }

  s2.shutdown().await.unwrap();

  let serfs = [s1];

  wait_until_num_nodes(1, &serfs).await;

  // Since s2 shutdown, we check the events to make sure we got failures.
  test_events(
    event_rx.rx,
    s2.local_id().clone(),
    [
      CrateEventType::Member(MemberEventType::Join),
      CrateEventType::Member(MemberEventType::Failed),
      CrateEventType::Member(MemberEventType::Reap),
    ]
    .into_iter()
    .collect(),
  )
  .await;

  assert!(
    ro1.called.load(Ordering::SeqCst),
    "reconnect override was not used"
  );
  serfs[0].shutdown().await.unwrap();
}
