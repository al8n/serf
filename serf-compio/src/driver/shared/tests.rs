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
