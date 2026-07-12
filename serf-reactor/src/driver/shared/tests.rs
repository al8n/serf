use super::*;

fn entry(peer: SocketAddr, bytes: &[u8], icmp_errors: u8) -> LeaveDatagram {
  LeaveDatagram {
    peer,
    bytes: bytes.to_vec(),
    icmp_errors,
  }
}

/// The leave-drain retention bookkeeping for fresh sends: `Ready(Ok)` retains
/// nothing, `Pending` retains FIFO at the back (returning `true`), and the
/// datagram bytes are copied only on the retain paths.
#[test]
fn retain_leave_datagram_retains_pending_fifo() {
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
  assert_eq!(retained.back().unwrap().bytes, b"beta".to_vec());
  assert_eq!(retained.back().unwrap().icmp_errors, 0);
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
  assert_eq!(retained.front().unwrap().peer, peer);
  assert_eq!(retained.back().unwrap().peer, peer2);
}

/// Error accounting for fresh sends: an ICMP-class error (`ConnectionReset` /
/// `ConnectionRefused`) is ambiguous on a shared unconnected socket — it may
/// be a stale asynchronous answer to an earlier packet for a DIFFERENT peer,
/// and either way the current datagram was not accepted — so the datagram is
/// RETAINED for a bounded retry without failing the leave. Every other kind is
/// a local delivery failure: `send_failed` is set and nothing is retained.
#[test]
fn retain_leave_datagram_retries_icmp_and_flags_local_failures() {
  let peer: SocketAddr = "127.0.0.1:7946".parse().unwrap();

  // ICMP-class: retained with one absorbed error, leave not failed.
  for kind in [
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::ConnectionRefused,
  ] {
    let mut retained: LeaveDrain = VecDeque::new();
    let mut send_failed = false;
    assert!(retain_leave_datagram(
      &mut retained,
      peer,
      b"alpha",
      Poll::Ready(Err(io::Error::from(kind))),
      &mut send_failed,
    ));
    assert_eq!(
      retained.len(),
      1,
      "an ICMP-class error must retain ({kind:?})"
    );
    assert_eq!(retained.front().unwrap().icmp_errors, 1);
    assert!(
      !send_failed,
      "an ICMP-class error ({kind:?}) must not fail the leave outright"
    );
  }

  // Local socket failures: attempted, not retained, and the leave FAILS.
  // `ConnectionAborted` is a local software abort on Windows; the
  // unreachables usually report the LOCAL routing table's answer; and
  // `AddrNotAvailable` means the bound source address vanished from this
  // host.
  for kind in [
    io::ErrorKind::ConnectionAborted,
    io::ErrorKind::HostUnreachable,
    io::ErrorKind::NetworkUnreachable,
    io::ErrorKind::AddrNotAvailable,
    io::ErrorKind::NotConnected,
    io::ErrorKind::BrokenPipe,
    io::ErrorKind::InvalidInput,
    io::ErrorKind::Other,
  ] {
    let mut retained: LeaveDrain = VecDeque::new();
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

/// The errored-farewell classifier: ICMP-class errors retry with a bumped
/// absorb count until [`FAREWELL_ICMP_ERROR_LIMIT`], where the answer is
/// attributed to the destination itself; every other kind is local regardless
/// of the count.
#[test]
fn classify_errored_farewell_bounds_icmp_retries() {
  for kind in [
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::ConnectionRefused,
  ] {
    let err = io::Error::from(kind);
    assert_eq!(
      classify_errored_farewell(0, &err),
      ErroredFarewell::Retry(1)
    );
    assert_eq!(
      classify_errored_farewell(1, &err),
      ErroredFarewell::Retry(2)
    );
    assert_eq!(
      classify_errored_farewell(FAREWELL_ICMP_ERROR_LIMIT - 1, &err),
      ErroredFarewell::PeerAnswered
    );
    assert_eq!(
      classify_errored_farewell(u8::MAX, &err),
      ErroredFarewell::PeerAnswered,
      "the absorb count saturates rather than wrapping"
    );
  }
  for kind in [
    io::ErrorKind::ConnectionAborted,
    io::ErrorKind::HostUnreachable,
    io::ErrorKind::NetworkUnreachable,
    io::ErrorKind::AddrNotAvailable,
    io::ErrorKind::BrokenPipe,
    io::ErrorKind::Other,
  ] {
    let err = io::Error::from(kind);
    assert_eq!(
      classify_errored_farewell(0, &err),
      ErroredFarewell::LocalFailure,
      "{kind:?} must be local on the first error"
    );
    assert_eq!(
      classify_errored_farewell(u8::MAX, &err),
      ErroredFarewell::LocalFailure
    );
  }
}

/// The retry sweep's outcome fold: a delivered datagram leaves the queue; an
/// ICMP-class error under the limit re-retains it at the BACK (so the rest of
/// the queue drains ahead of the re-attempt) with the bumped count; at the
/// limit it is dropped as answered without failing the leave; a local error
/// sets `send_failed`; and `Pending` puts it back at the FRONT and stops the
/// sweep (FIFO preserved).
#[test]
fn settle_retried_farewell_dispositions() {
  let peer_a: SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let peer_b: SocketAddr = "127.0.0.1:7947".parse().unwrap();

  // Delivered: not re-added, sweep continues.
  let mut retained: LeaveDrain = VecDeque::new();
  let mut send_failed = false;
  assert!(!settle_retried_farewell(
    &mut retained,
    entry(peer_a, b"alpha", 1),
    Poll::Ready(Ok(5)),
    &mut send_failed,
  ));
  assert!(retained.is_empty());
  assert!(!send_failed);

  // ICMP-class under the limit: re-retained at the BACK with a bumped count —
  // the stale-error disambiguation retry — leaving the queue's head (another
  // peer's farewell) to drain first.
  let mut retained: LeaveDrain = VecDeque::from([entry(peer_b, b"beta", 0)]);
  let mut send_failed = false;
  assert!(!settle_retried_farewell(
    &mut retained,
    entry(peer_a, b"alpha", 1),
    Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionRefused))),
    &mut send_failed,
  ));
  assert_eq!(retained.len(), 2);
  assert_eq!(retained.front().unwrap().peer, peer_b);
  assert_eq!(retained.back().unwrap().peer, peer_a);
  assert_eq!(retained.back().unwrap().icmp_errors, 2);
  assert!(!send_failed);

  // ICMP-class at the limit: dropped as answered-by-the-network, leave intact.
  let mut retained: LeaveDrain = VecDeque::new();
  let mut send_failed = false;
  assert!(!settle_retried_farewell(
    &mut retained,
    entry(peer_a, b"alpha", FAREWELL_ICMP_ERROR_LIMIT - 1),
    Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset))),
    &mut send_failed,
  ));
  assert!(
    retained.is_empty(),
    "an answered peer's farewell is dropped"
  );
  assert!(!send_failed, "a gone peer must not fail the whole leave");

  // Local failure: dropped AND the leave fails.
  let mut retained: LeaveDrain = VecDeque::new();
  let mut send_failed = false;
  assert!(!settle_retried_farewell(
    &mut retained,
    entry(peer_a, b"alpha", 0),
    Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
    &mut send_failed,
  ));
  assert!(retained.is_empty());
  assert!(send_failed);

  // Pending: back at the FRONT (count preserved), sweep stops.
  let mut retained: LeaveDrain = VecDeque::from([entry(peer_b, b"beta", 0)]);
  let mut send_failed = false;
  assert!(settle_retried_farewell(
    &mut retained,
    entry(peer_a, b"alpha", 2),
    Poll::Pending,
    &mut send_failed,
  ));
  assert_eq!(retained.len(), 2);
  assert_eq!(retained.front().unwrap().peer, peer_a);
  assert_eq!(retained.front().unwrap().icmp_errors, 2);
  assert!(!send_failed);
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
