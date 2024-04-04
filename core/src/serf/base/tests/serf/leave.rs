use super::*;

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

  //Put s2 in failed state
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
