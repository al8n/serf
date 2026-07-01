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

/// `NoopMergeDelegate` satisfies `MergeDelegate` with `Error = Infallible` — a
/// type-level check; no I/O needed.
#[test]
fn noop_merge_delegate_satisfies_trait() {
  fn assert_merge<T>(_: &T)
  where
    T: MergeDelegate<SmolStr, SocketAddr, Error = core::convert::Infallible>,
  {
  }
  assert_merge(&NoopMergeDelegate);
}

/// The reactor delegate surface is `Send + Sync + 'static`: the driver holds it
/// behind an `Arc` shared across the runtime's worker threads.
#[test]
fn void_delegate_and_noop_merge_are_send_sync() {
  fn assert_send_sync<T: Send + Sync + 'static>() {}
  assert_send_sync::<VoidDelegate<SmolStr, SocketAddr>>();
  assert_send_sync::<NoopMergeDelegate>();
}
