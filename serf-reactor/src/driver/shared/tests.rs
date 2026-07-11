use super::*;

/// The leave-drain retention bookkeeping: `Ready(Ok)` sends (retaining nothing),
/// `Pending` retains FIFO at the back (returning `true`), and `Ready(Err)` is
/// attempted — logged, retaining nothing. The datagram bytes are copied only on
/// the retain path.
#[test]
fn retain_leave_datagram_records_only_pending() {
  let peer: SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let mut retained: LeaveDrain = VecDeque::new();

  // Ready(Ok): sent, nothing retained.
  assert!(!retain_leave_datagram(
    &mut retained,
    peer,
    b"alpha",
    Poll::Ready(Ok(5))
  ));
  assert!(retained.is_empty(), "a completed send retains nothing");

  // Pending: retained at the back, returns true (socket backpressured).
  assert!(retain_leave_datagram(
    &mut retained,
    peer,
    b"beta",
    Poll::Pending
  ));
  assert_eq!(retained.len(), 1);
  assert_eq!(retained.back().unwrap(), &(peer, b"beta".to_vec()));

  // A second Pending appends behind the first (FIFO order preserved).
  let peer2: SocketAddr = "127.0.0.1:7947".parse().unwrap();
  assert!(retain_leave_datagram(
    &mut retained,
    peer2,
    b"gamma",
    Poll::Pending
  ));
  assert_eq!(retained.len(), 2);
  assert_eq!(retained.front().unwrap(), &(peer, b"beta".to_vec()));
  assert_eq!(retained.back().unwrap(), &(peer2, b"gamma".to_vec()));

  // Ready(Err): attempted (logged), not retained; the queue is unchanged.
  assert!(!retain_leave_datagram(
    &mut retained,
    peer,
    b"delta",
    Poll::Ready(Err(io::Error::other("send failed"))),
  ));
  assert_eq!(
    retained.len(),
    2,
    "an errored send is attempted, not retained"
  );
}
