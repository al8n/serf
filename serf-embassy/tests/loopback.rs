//! Two-node loopback over real embassy-net stacks wired by a channel-backed
//! paired [`Driver`](embassy_net::driver::Driver).
//!
//! Each test stands up two embassy-net stacks (static IPs `169.254.1.1/.2`), a
//! `Serf` + `Runner` on each, and drives every future concurrently under one
//! [`block_on`](futures::executor::block_on): the two serf run loops, the two
//! embassy-net stack run loops, and the operation under test, raced against a
//! wall-clock timeout so a regression fails fast instead of hanging.
//!
//! This is the behavioral gate for the embassy driver: an async join converging
//! over TCP push/pull, and a graceful leave — over real sockets, not mocks.

// nested `if let X = ev { if cond }` kept for readability, as in the crate roots.
#![allow(clippy::collapsible_if)]

mod support;

use std::time::Instant as StdInstant;

use embassy_futures::select::{Either, select};
use embassy_net::{IpEndpoint, StackResources};
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{Event, TransformOptions, now};

use support::cluster::{
  NodeBufs, POOL, TEST_TIMEOUT, addr, build_node, build_sockets, build_stack, devices, drive,
  join_and_converge,
};

/// Substrate check: a raw UDP datagram crosses the two paired-device stacks.
/// Isolates the device/stack/waker plumbing from the serf protocol so a
/// convergence failure can be attributed correctly.
#[test]
fn raw_udp_crosses_the_paired_link() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (mut udp_a, _tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (mut udp_b, _tcp_b) = build_sockets(stack_b, &mut bufs_b);

  block_on(async {
    udp_a.bind(7000).expect("bind a");
    udp_b.bind(7000).expect("bind b");
    let op = async {
      let dst = IpEndpoint::from(addr(2, 7000));
      let mut buf = [0u8; 32];
      loop {
        // Ignoring Err: best-effort retry — resend until B's socket is up and the
        // recv below observes the datagram, so a dropped early send is harmless.
        let _ = udp_a.send_to(b"ping", dst).await;
        match select(
          udp_b.recv_from(&mut buf),
          Timer::after(Duration::from_millis(50)),
        )
        .await
        {
          Either::First(Ok((n, _))) => return buf[..n].to_vec(),
          _ => continue,
        }
      }
    };
    let nets = select(net_a.run(), net_b.run());
    let got = match select(op, select(nets, Timer::after(TEST_TIMEOUT))).await {
      Either::First(v) => v,
      Either::Second(_) => panic!("raw UDP did not cross the link within {TEST_TIMEOUT:?}"),
    };
    assert_eq!(got, b"ping");
  });
}

/// Two nodes; B joins A as a seed; both converge on a 2-member view; then B leaves
/// gracefully and observes `Event::LeftCluster`.
#[test]
fn two_node_join_converges_and_leaves() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now = now();
  let (ml_a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, TransformOptions::default());
  let (ml_b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, TransformOptions::default());

  let start = StdInstant::now();
  block_on(async {
    let op = async {
      // B joins A; the await resolves once the push/pull reaches the seed.
      let reached = ml_b
        .join(
          &serf_embassy::SocketAddrResolver,
          &[serf_embassy::MaybeResolved::Resolved(addr(1, 7946))],
          false,
        )
        .await
        .expect("join from a running node");
      assert!(
        reached.iter().any(|a| *a == addr(1, 7946)),
        "the join must report A as reached: {reached:?}"
      );
      // Wait for BOTH to converge (A learns B a tick after the exchange).
      loop {
        if ml_a.num_members() == 2 && ml_b.num_members() == 2 {
          break;
        }
        Timer::after(Duration::from_millis(10)).await;
      }
      // B leaves gracefully and must observe LeftCluster.
      ml_b.leave().expect("leave from a running node");
      loop {
        if let Some(ev) = ml_b.poll_event() {
          if matches!(ev, Event::LeftCluster) {
            return true;
          }
        } else {
          Timer::after(Duration::from_millis(5)).await;
        }
      }
    };
    let left = drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
    assert!(left, "B did not observe LeftCluster after leaving");
  });
  let elapsed = start.elapsed();

  assert_eq!(ml_a.num_members(), 2, "A did not converge to 2 members");
  println!("two_node_join_converges_and_leaves: converged in {elapsed:?}");
}

/// The `join_and_converge` helper is exercised implicitly by the other suites;
/// this asserts it standalone so a convergence regression is attributed here.
#[test]
fn join_and_converge_helper_reaches_two_members() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now = now();
  let (ml_a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, TransformOptions::default());
  let (ml_b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, TransformOptions::default());

  block_on(async {
    let op = async {
      join_and_converge(&ml_a, &ml_b).await;
      (ml_a.num_members(), ml_b.num_members())
    };
    let (a_n, b_n) = drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
    assert_eq!((a_n, b_n), (2, 2));
  });
}
