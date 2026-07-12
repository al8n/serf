use super::*;
use smol_str::SmolStr;
use std::net::SocketAddr;

/// `VoidDelegate` satisfies the observation [`Delegate`] composite with the
/// canonical `(SmolStr, SocketAddr)` id/address pair.
#[test]
fn void_delegate_satisfies_observation_composite() {
  fn assert_delegate<D>(_d: &D)
  where
    D: Delegate<Id = SmolStr, Address = SocketAddr>,
  {
  }
  let v: VoidDelegate<SmolStr, SocketAddr> = VoidDelegate::default();
  assert_delegate(&v);
}

/// A boxed machine merge delegate threads through the constructor slot — a
/// type-level check that the re-exported trait and its Box blanket compose.
#[test]
fn boxed_merge_delegate_satisfies_trait() {
  struct AcceptAll;
  impl MergeDelegate<SmolStr, SocketAddr> for AcceptAll {
    fn notify_merge(
      &self,
      _peers: memberlist_proto::MaybeOwned<
        '_,
        [memberlist_proto::typed::NodeState<SmolStr, SocketAddr>],
      >,
    ) -> bool {
      true
    }
  }
  fn assert_merge<T>(_: &T)
  where
    T: MergeDelegate<SmolStr, SocketAddr>,
  {
  }
  let boxed: Box<dyn MergeDelegate<SmolStr, SocketAddr>> = Box::new(AcceptAll);
  assert_merge(&boxed);
}

/// The reactor delegate surface is `Send + Sync + 'static`: the driver holds it
/// behind an `Arc` shared across the runtime's worker threads.
#[test]
fn void_delegate_is_send_sync() {
  fn assert_send_sync<T: Send + Sync + 'static>() {}
  assert_send_sync::<VoidDelegate<SmolStr, SocketAddr>>();
}
