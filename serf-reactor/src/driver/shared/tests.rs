use super::*;

/// The leave-drain retention bookkeeping: `Ready(Ok)` sends (retaining nothing),
/// `Pending` retains FIFO at the back (returning `true`), and `Ready(Err)` is
/// attempted — logged, retaining nothing. The datagram bytes are copied only on
/// the retain path.
#[test]
fn retain_leave_datagram_records_only_pending() {
  let peer: SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let mut retained: LeaveDrain = VecDeque::new();
  let mut send_failed = false;

  // Ready(Ok): sent, nothing retained.
  assert!(!retain_leave_datagram(
    &mut retained,
    peer,
    b"alpha",
    Poll::Ready(Ok(5)),
    &mut send_failed,
  ));
  assert!(retained.is_empty(), "a completed send retains nothing");
  assert!(!send_failed, "a completed send is not a failure");

  // Pending: retained at the back, returns true (socket backpressured).
  assert!(retain_leave_datagram(
    &mut retained,
    peer,
    b"beta",
    Poll::Pending,
    &mut send_failed,
  ));
  assert_eq!(retained.len(), 1);
  assert_eq!(retained.back().unwrap(), &(peer, b"beta".to_vec()));
  assert!(!send_failed, "backpressure is retention, not failure");

  // A second Pending appends behind the first (FIFO order preserved).
  let peer2: SocketAddr = "127.0.0.1:7947".parse().unwrap();
  assert!(retain_leave_datagram(
    &mut retained,
    peer2,
    b"gamma",
    Poll::Pending,
    &mut send_failed,
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
    &mut send_failed,
  ));
  assert_eq!(
    retained.len(),
    2,
    "an errored send is attempted, not retained"
  );
}

/// Error accounting: a LOCAL send failure (broken socket — the farewell never
/// left this host) sets `send_failed` so the parked leave resolves with an
/// error, while a per-peer network signal (a reset or unreachable reflected
/// for a peer that is itself gone) is log-only — it neither retains nor fails
/// the leave.
#[test]
fn retain_leave_datagram_flags_only_local_failures() {
  let peer: SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let mut retained: LeaveDrain = VecDeque::new();

  // Per-peer network signals: attempted, not retained, leave still succeeds.
  for kind in [
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::ConnectionRefused,
    io::ErrorKind::ConnectionAborted,
    io::ErrorKind::HostUnreachable,
    io::ErrorKind::NetworkUnreachable,
  ] {
    let mut send_failed = false;
    assert!(!retain_leave_datagram(
      &mut retained,
      peer,
      b"alpha",
      Poll::Ready(Err(io::Error::from(kind))),
      &mut send_failed,
    ));
    assert!(retained.is_empty());
    assert!(
      !send_failed,
      "a per-peer network signal ({kind:?}) must not fail the leave"
    );
  }

  // Local socket failures: attempted, not retained, and the leave FAILS.
  // `AddrNotAvailable` is local — the bound source address vanished from this
  // host — not a peer signal.
  for kind in [
    io::ErrorKind::NotConnected,
    io::ErrorKind::BrokenPipe,
    io::ErrorKind::InvalidInput,
    io::ErrorKind::AddrNotAvailable,
    io::ErrorKind::Other,
  ] {
    let mut send_failed = false;
    assert!(!retain_leave_datagram(
      &mut retained,
      peer,
      b"alpha",
      Poll::Ready(Err(io::Error::from(kind))),
      &mut send_failed,
    ));
    assert!(retained.is_empty());
    assert!(
      send_failed,
      "a local socket failure ({kind:?}) must fail the leave"
    );
  }
}

/// The leave outcome maps the accumulated failure flag onto the caller-facing
/// `leave().await` result: no failure resolves `Ok`, a local send/transform
/// failure resolves `LeaveFarewellUndelivered`.
#[test]
fn leave_outcome_maps_the_failure_flag() {
  assert!(leave_outcome(false).is_ok());
  assert!(matches!(
    leave_outcome(true),
    Err(crate::error::SerfError::LeaveFarewellUndelivered)
  ));
}
