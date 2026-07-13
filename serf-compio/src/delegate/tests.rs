use super::*;
use smol_str::SmolStr;
use std::net::SocketAddr;

/// The zero-cost default delegate satisfies the whole observation composite, so
/// a driver that needs no hooks constructs a node without boilerplate.
#[cfg(any(feature = "tcp", feature = "quic"))]
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

/// The re-exported merge predicate is the machine's synchronous push/pull
/// filter: a plain permit-all impl satisfies it, and its verdict is the value
/// the machine acts on.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn a_sync_predicate_satisfies_the_merge_delegate() {
  struct PermitAll;
  impl MergeDelegate<SmolStr, SocketAddr> for PermitAll {
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
  fn assert_merge<T>(t: &T) -> bool
  where
    T: MergeDelegate<SmolStr, SocketAddr>,
  {
    t.notify_merge(memberlist_proto::MaybeOwned::Borrowed(&[]))
  }
  assert!(
    assert_merge(&PermitAll),
    "a permit-all predicate admits the exchange"
  );
}
