use super::*;

use serf_proto::DropCounter;

#[test]
fn incr_saturates_at_max() {
  let arc = Arc::new(AtomicU64::new(u64::MAX - 1));
  let mut w = ReactorDropCounter(arc);
  w.incr_saturating();
  assert_eq!(w.get(), u64::MAX);
  // A further increment must saturate, not wrap the compare-and-swap back to zero.
  w.incr_saturating();
  assert_eq!(w.get(), u64::MAX);
}

#[test]
fn reader_observes_writer_over_the_shared_atomic() {
  let (mut w, r) = drop_channel();
  assert_eq!(r.get(), 0);
  w.incr_saturating();
  w.incr_saturating();
  w.incr_saturating();
  assert_eq!(
    r.get(),
    3,
    "the read half observes the write half's increments over one atomic"
  );
}

/// The reactor endpoint carries a `Send + Sync` shared-atomic counter, so the
/// detached pump future stays spawnable on a multi-thread runtime.
#[test]
fn reactor_endpoint_is_send_and_sync() {
  fn assert_send_sync<T: Send + Sync>() {}
  assert_send_sync::<ReactorDropCounter>();
  assert_send_sync::<DropReader>();
  assert_send_sync::<
    serf_proto::endpoint::Endpoint<
      u32,
      std::net::SocketAddr,
      rand::rngs::StdRng,
      ReactorDropCounter,
    >,
  >();
}
