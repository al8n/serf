use core::{net::SocketAddr, time::Duration};

use super::ReconnectDelegate;
use crate::{
  Tags,
  members::{Member, MemberStatus},
};

// A boxed delegate must stay `Send + Sync` so it can live inside `Endpoint`
// without stripping the endpoint's auto-traits for the multi-threaded drivers.
#[test]
fn boxed_delegate_is_send_sync_and_object_safe() {
  fn assert_send_sync<T>()
  where
    T: Send + Sync + ?Sized,
  {
  }
  assert_send_sync::<dyn ReconnectDelegate<u32, SocketAddr>>();
  assert_send_sync::<Box<dyn ReconnectDelegate<u32, SocketAddr>>>();
}

/// A delegate that returns a fixed override regardless of the base timeout.
struct FixedTimeout(Duration);

impl<I, A> ReconnectDelegate<I, A> for FixedTimeout {
  fn reconnect_timeout(&self, _member: &Member<I, A>, _timeout: Duration) -> Duration {
    self.0
  }
}

#[test]
fn concrete_delegate_boxes_and_overrides() {
  let d: Box<dyn ReconnectDelegate<u32, SocketAddr>> =
    Box::new(FixedTimeout(Duration::from_secs(1)));
  let node = memberlist_proto::Node::new(1u32, "127.0.0.1:1".parse::<SocketAddr>().unwrap());
  let member = Member::new(node, Tags::new(), MemberStatus::Alive);
  // The override is returned verbatim, ignoring the configured base timeout.
  assert_eq!(
    d.reconnect_timeout(&member, Duration::from_secs(30)),
    Duration::from_secs(1)
  );
}
