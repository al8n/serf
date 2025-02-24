use super::*;

/// Unit test for remove failed node
pub async fn serf_remove_failed_node<T>(
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

  // Now force the shutdown of s2 so it appears to fail.
  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval();

  <T::Runtime as RuntimeLite>::sleep(t * 5).await;

  let s2id = serfs[1].local_id().clone();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Failed).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(10) {
      println!("{:?}", members.states);
      panic!("Failed to mark node as failed");
    }
  }

  // Now remove the failed node
  serfs[0].remove_failed_node(s2id.clone()).await.unwrap();

  serfs.swap(1, 2);

  let start = Epoch::now();
  let mut cond1 = false;
  let mut cond2 = false;
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
      cond1 = true;
    }

    let members = serfs[1].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Left).is_ok() {
      cond2 = true;
    }

    if cond1 && cond2 {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("Failed to remove failed node");
    }
  }

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for remove failed node prune
pub async fn serf_remove_failed_node_prune<T>(
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

  // Now force the shutdown of s2 so it appears to fail.
  serfs[1].shutdown().await.unwrap();

  let t = serfs[1].inner.opts.memberlist_options.probe_interval();

  <T::Runtime as RuntimeLite>::sleep(t * 5).await;

  let s2id = serfs[1].local_id().clone();

  let start = Epoch::now();
  loop {
    <T::Runtime as RuntimeLite>::sleep(Duration::from_millis(25)).await;

    let members = serfs[0].inner.members.read().await;
    if test_member_status(&members.states, s2id.clone(), MemberStatus::Failed).is_ok() {
      break;
    }

    if start.elapsed() > Duration::from_secs(7) {
      panic!("Failed to mark node as failed");
    }
  }

  // Now remove the failed node
  serfs[0]
    .remove_failed_node_prune(s2id.clone())
    .await
    .unwrap();

  serfs.swap(1, 2);

  wait_until_num_nodes(2, &serfs[..2]).await;

  for s in serfs.iter() {
    s.shutdown().await.unwrap();
  }
}

/// Unit test for remove failed node ourself
pub async fn serf_remove_failed_node_ourself<T>(transport_opts1: T::Options)
where
  T: Transport<Id = SmolStr>,
{
  let s1 = Serf::<T>::new(transport_opts1, test_config())
    .await
    .unwrap();

  let serfs = [s1];
  wait_until_num_nodes(1, &serfs).await;

  serfs[0]
    .remove_failed_node("somebody".into())
    .await
    .unwrap();

  serfs[0].shutdown().await.unwrap();
}

/// Unit test for remove old member
#[test]
fn test_remove_old_member() {
  let mut members = [
    MemberState {
      member: Member::new(
        Node::new("foo".into(), 100),
        Default::default(),
        MemberStatus::None,
      ),
      status_time: 0.into(),
      leave_time: None,
    },
    MemberState {
      member: Member::new(
        Node::new("bar".into(), 100),
        Default::default(),
        MemberStatus::None,
      ),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(5)),
    },
    MemberState {
      member: Member::new(
        Node::new("baz".into(), 100),
        Default::default(),
        MemberStatus::None,
      ),
      status_time: 0.into(),
      leave_time: Some(Epoch::now() - Duration::from_secs(5)),
    },
  ]
  .into_iter()
  .collect();
  remove_old_member::<SmolStr, u64>(&mut members, &"bar".into());
  assert_eq!(members.len(), 2);
}
