//! Unit tests for the shared bounded past-due UDP drain.
//!
//! Both driver pumps (QUIC and stream) reach `handle_timeout` through the same
//! [`drain_past_due_udp`] helper, so these socket-level tests are the structural
//! proof for BOTH pumps. They assert the drain consumes EVERY queued datagram
//! (not just the first), honors the early stop when `on_datagram` resolves the
//! deadline, and is bounded by `budget`. On the local kqueue backend the eager
//! recv syscall makes a buffered datagram `Ready` on its first poll, so these are
//! deterministic.

use super::*;

use std::{cell::Cell, net::SocketAddr};

use compio::net::UdpSocket;

/// Bind a loopback driver/peer UDP socket pair, returning `(driver, peer,
/// driver_addr)` so the peer can buffer datagrams into the driver's queue.
async fn socket_pair() -> (UdpSocket, UdpSocket, SocketAddr) {
  let any: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let driver = UdpSocket::bind(any).await.expect("bind driver socket");
  let driver_addr = driver.local_addr().expect("driver local_addr");
  let peer = UdpSocket::bind(any).await.expect("bind peer socket");
  (driver, peer, driver_addr)
}

/// Give loopback UDP time to deliver the just-sent datagrams into the driver's
/// receive buffer before the drain runs, so the eager recv reads them on its
/// first poll. This mirrors production — the past-due Ack has been buffered since
/// well before the deadline — and is not a drain-internal time window.
async fn settle() {
  compio::time::sleep(core::time::Duration::from_millis(100)).await;
}

/// The core regression: two datagrams sit buffered (a non-Ack precedes the
/// would-be Ack). Reading only the first could fire `handle_timeout` before the
/// second was read; the bounded drain must consume BOTH — the reap-then-recheck
/// surfaces the second — and report that it drained.
#[compio::test]
async fn drains_all_ready_datagrams_not_just_one() {
  let (driver, peer, dst) = socket_pair().await;
  peer.send_to(vec![0xA1u8], dst).await.0.expect("send first");
  peer
    .send_to(vec![0xA2u8], dst)
    .await
    .0
    .expect("send second");
  settle().await;

  let count = Cell::new(0usize);
  let drained = drain_past_due_udp(&driver, 64, 8, |_src, _bytes| {
    count.set(count.get() + 1);
    // The deadline is still past after each non-Ack datagram, so keep draining.
    true
  })
  .await;

  assert!(
    drained,
    "the drain reports it consumed at least one datagram"
  );
  assert_eq!(
    count.get(),
    2,
    "the bounded drain must consume BOTH queued datagrams, not just the first"
  );

  // Ignoring Err: test cleanup of the probe sockets.
  let _ = driver.close().await;
  let _ = peer.close().await;
}

/// Once a consumed datagram resolves the probe deadline (`on_datagram` returns
/// `false`) the drain stops immediately, leaving the remaining datagrams for the
/// main select.
#[compio::test]
async fn stops_when_deadline_resolved() {
  let (driver, peer, dst) = socket_pair().await;
  peer.send_to(vec![0xC1u8], dst).await.0.expect("send first");
  peer
    .send_to(vec![0xC2u8], dst)
    .await
    .0
    .expect("send second");
  settle().await;

  let count = Cell::new(0usize);
  let drained = drain_past_due_udp(&driver, 64, 8, |_src, _bytes| {
    count.set(count.get() + 1);
    // The first datagram resolved the deadline (a buffered Ack): stop draining.
    false
  })
  .await;

  assert!(drained);
  assert_eq!(
    count.get(),
    1,
    "a resolved deadline stops the drain after the resolving datagram"
  );

  // Ignoring Err: test cleanup of the probe sockets.
  let _ = driver.close().await;
  let _ = peer.close().await;
}

/// More datagrams are queued than the budget, so the drain stops at the cap: the
/// loop is bounded by `budget` no matter how many datagrams remain queued.
#[compio::test]
async fn drain_is_bounded_by_budget() {
  let (driver, peer, dst) = socket_pair().await;
  for b in [0xB1u8, 0xB2, 0xB3] {
    peer.send_to(vec![b], dst).await.0.expect("send");
  }
  settle().await;

  let count = Cell::new(0usize);
  let drained = drain_past_due_udp(&driver, 64, 2, |_src, _bytes| {
    count.set(count.get() + 1);
    true
  })
  .await;

  assert!(drained);
  assert_eq!(
    count.get(),
    2,
    "the drain stops at the budget cap even when more datagrams are queued"
  );

  // Ignoring Err: test cleanup of the probe sockets.
  let _ = driver.close().await;
  let _ = peer.close().await;
}

/// An empty socket queue drains nothing: the recv is `Pending`, the forced reap
/// finds no completion, the re-poll is still `Pending`, and the drain reports it
/// consumed nothing.
#[compio::test]
async fn empty_socket_drains_nothing() {
  let (driver, _peer, _dst) = socket_pair().await;

  let count = Cell::new(0usize);
  let drained = drain_past_due_udp(&driver, 64, 8, |_src, _bytes| {
    count.set(count.get() + 1);
    true
  })
  .await;

  assert!(!drained, "an empty socket queue drains nothing");
  assert_eq!(count.get(), 0);

  // Ignoring Err: test cleanup of the probe socket.
  let _ = driver.close().await;
}

// ── leave-farewell classification ─────────────────────────────────────────────

/// A fresh pump has not initiated a leave and has recorded no failure, so its
/// leave outcome is a clean success.
#[test]
fn a_fresh_farewell_is_clean() {
  let acct = Farewell::new();
  assert!(!acct.initiated);
  assert!(!acct.send_failed);
  assert!(leave_outcome(acct.send_failed).is_ok());
}

/// A local send or transform failure during the fan-out resolves the leave with
/// `LeaveFarewellUndelivered`, never a false `Ok` — the caller learns that at
/// least one peer will read the departure as a failure.
#[test]
fn a_local_failure_fails_the_leave() {
  let err = leave_outcome(true).expect_err("a local send failure must fail the leave");
  assert!(matches!(
    err,
    crate::error::SerfError::LeaveFarewellUndelivered
  ));
}

/// The ICMP-reflection class (`ConnectionReset` / `ConnectionRefused`) is
/// AMBIGUOUS on a shared unconnected UDP socket — the error slot may hold a stale
/// asynchronous answer to an earlier packet for a DIFFERENT peer — so the first
/// errors are absorbed and the datagram is re-sent, each re-send both draining the
/// stale slot and re-handing the datagram to the socket.
#[test]
fn an_icmp_class_error_under_the_limit_is_resent() {
  for kind in [
    std::io::ErrorKind::ConnectionReset,
    std::io::ErrorKind::ConnectionRefused,
  ] {
    assert_eq!(
      classify_errored_farewell(0, &std::io::Error::from(kind)),
      ErroredFarewell::Resend(1),
      "the first {kind:?} is ambiguous and is re-sent"
    );
    assert_eq!(
      classify_errored_farewell(1, &std::io::Error::from(kind)),
      ErroredFarewell::Resend(2),
    );
  }
}

/// ICMP-class errors that PERSIST to the absorb limit are credibly this
/// destination's own answer (a peer that is itself gone). The reference
/// implementation logs such peers and proceeds, so the datagram is dropped and the
/// leave still succeeds — a departing node is not held back by peers that already
/// left.
#[test]
fn persistent_icmp_class_errors_are_the_peer_answering() {
  assert_eq!(
    classify_errored_farewell(
      FAREWELL_ICMP_ERROR_LIMIT - 1,
      &std::io::Error::from(std::io::ErrorKind::ConnectionReset)
    ),
    ErroredFarewell::PeerAnswered,
  );
  // PeerAnswered does not record a failure, so the leave still resolves `Ok`.
  assert!(leave_outcome(false).is_ok());
}

/// Every non-ICMP error kind is a LOCAL delivery failure — the farewell never left
/// this host — so it fails the leave. `ConnectionAborted` is a local software abort
/// on Windows, the unreachables usually report the LOCAL routing table's answer,
/// and a closed/invalid socket or broken pipe is unambiguously local.
#[test]
fn every_other_error_kind_is_a_local_failure() {
  for kind in [
    std::io::ErrorKind::ConnectionAborted,
    std::io::ErrorKind::HostUnreachable,
    std::io::ErrorKind::NetworkUnreachable,
    std::io::ErrorKind::BrokenPipe,
    std::io::ErrorKind::NotConnected,
    std::io::ErrorKind::InvalidInput,
    std::io::ErrorKind::PermissionDenied,
  ] {
    assert_eq!(
      classify_errored_farewell(0, &std::io::Error::from(kind)),
      ErroredFarewell::LocalFailure,
      "{kind:?} is a local delivery failure and must fail the leave"
    );
  }
}

/// A periodic-gossip send (no leave in flight) is best-effort: a send to an
/// unroutable destination records NO failure, so a later leave is not poisoned by
/// an unrelated dropped gossip datagram.
#[compio::test]
async fn a_pre_leave_gossip_send_never_records_a_failure() {
  let (driver, _peer, dst) = socket_pair().await;
  let mut acct = Farewell::new();

  send_gossip_datagram(&driver, dst, b"gossip".to_vec(), &mut acct).await;

  assert!(
    !acct.send_failed,
    "periodic gossip is best-effort and must never record a farewell failure"
  );
  assert!(leave_outcome(acct.send_failed).is_ok());

  // Ignoring Err: test cleanup of the probe socket.
  let _ = driver.close().await;
}

/// Once `leave()` is initiated, a farewell datagram that the local socket ACCEPTS
/// records no failure, so the leave resolves `Ok`.
#[compio::test]
async fn a_delivered_farewell_keeps_the_leave_ok() {
  let (driver, _peer, dst) = socket_pair().await;
  let mut acct = Farewell::new();
  acct.initiated = true;

  send_gossip_datagram(&driver, dst, b"farewell".to_vec(), &mut acct).await;

  assert!(
    !acct.send_failed,
    "a farewell the socket accepted must not fail the leave"
  );
  assert!(leave_outcome(acct.send_failed).is_ok());

  // Ignoring Err: test cleanup of the probe socket.
  let _ = driver.close().await;
}

/// A farewell the local socket REFUSES records the failure, so the parked leave
/// resolves `LeaveFarewellUndelivered` instead of a false `Ok`.
///
/// The refusal is forced deterministically by an address-family mismatch: an
/// IPv4-bound socket cannot send to an IPv6 destination, and the OS rejects it
/// locally (never an ICMP-class reflection), which is exactly the "the farewell
/// never left this host" class. This is the raise path end to end: socket error →
/// classification → `send_failed` → the leave's error.
#[compio::test]
async fn a_refused_farewell_raises_leave_farewell_undelivered() {
  let v4: SocketAddr = "127.0.0.1:0".parse().expect("v4 loopback");
  let socket = UdpSocket::bind(v4).await.expect("bind a v4 gossip socket");
  let v6_dst: SocketAddr = "[::1]:9".parse().expect("v6 destination");

  let mut acct = Farewell::new();
  acct.initiated = true;

  send_gossip_datagram(&socket, v6_dst, b"farewell".to_vec(), &mut acct).await;

  assert!(
    acct.send_failed,
    "a farewell the local socket refused must record the delivery failure"
  );
  let err = leave_outcome(acct.send_failed).expect_err("the leave must not report a false success");
  assert!(matches!(
    err,
    crate::error::SerfError::LeaveFarewellUndelivered
  ));

  // Ignoring Err: test cleanup of the probe socket.
  let _ = socket.close().await;
}

/// The SAME refused send on a pump that has NOT initiated a leave is best-effort:
/// it records nothing, so an unrelated gossip failure can never poison a later
/// leave into a false `LeaveFarewellUndelivered`.
#[compio::test]
async fn a_refused_pre_leave_gossip_send_does_not_poison_a_later_leave() {
  let v4: SocketAddr = "127.0.0.1:0".parse().expect("v4 loopback");
  let socket = UdpSocket::bind(v4).await.expect("bind a v4 gossip socket");
  let v6_dst: SocketAddr = "[::1]:9".parse().expect("v6 destination");

  let mut acct = Farewell::new();

  send_gossip_datagram(&socket, v6_dst, b"gossip".to_vec(), &mut acct).await;

  assert!(
    !acct.send_failed,
    "periodic gossip is best-effort; a failed send must not be charged to a leave"
  );
  assert!(leave_outcome(acct.send_failed).is_ok());

  // Ignoring Err: test cleanup of the probe socket.
  let _ = socket.close().await;
}

/// The rotation-durability acknowledgement contract both pumps park a key
/// response on: an unacknowledged rotation keeps the response parked; a persisted
/// rotation sends it unchanged; a persistence failure — or a worker that vanished
/// without acknowledging — downgrades it to `result = false` carrying the error,
/// so a caller is never told a rotation was durable when it was not.
#[cfg(encryption)]
#[test]
fn parked_key_responses_settle_by_acknowledgement_outcome() {
  use std::sync::mpsc;

  use serf_driver::settle_parked_key_response;

  let ok_resp = serf_proto::event::KeyResponseArgs {
    result: true,
    message: "".into(),
    ..Default::default()
  };

  // Still pending: stays parked.
  let (tx, rx) = mpsc::channel();
  assert!(settle_parked_key_response(&rx, &ok_resp).is_none());

  // Persisted: the response goes out unchanged.
  tx.send(Ok(())).expect("ack sends");
  let settled = settle_parked_key_response(&rx, &ok_resp).expect("resolved");
  assert!(settled.result);
  assert!(settled.message.is_empty());

  // Persistence failure: downgraded, carrying the error.
  let (tx, rx) = mpsc::channel();
  tx.send(Err(std::io::Error::other("disk gone").into()))
    .expect("ack sends");
  let settled = settle_parked_key_response(&rx, &ok_resp).expect("resolved");
  assert!(!settled.result);
  assert!(settled.message.contains("disk gone"));

  // Worker vanished without acknowledging: a failure, not a silent success.
  let (tx, rx) = mpsc::channel::<Result<(), crate::KeyringPersistError>>();
  drop(tx);
  let settled = settle_parked_key_response(&rx, &ok_resp).expect("resolved");
  assert!(!settled.result);
  assert!(settled.message.contains("without acknowledging"));
}
