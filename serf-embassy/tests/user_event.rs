//! User-event propagation end-to-end: after two nodes converge, a user event
//! broadcast by A crosses the gossip plane and B observes it as `Event::User`.

#![allow(clippy::collapsible_if)]

mod support;

use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{Bytes, Event, TransformOptions, now};

use embassy_net::StackResources;
use support::cluster::{
  NodeBufs, POOL, build_node, build_sockets, build_stack, devices, drive, join_and_converge,
};

/// After convergence, A broadcasts a user event; B must observe it via
/// `poll_event` as `Event::User`.
#[test]
fn user_event_propagates_a_to_b() {
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

      ml_a
        .user_event("greet", Bytes::from_static(b"hello"), false)
        .expect("user_event from a running node");

      // B observes the user event on its gossip plane.
      loop {
        if let Some(ev) = ml_b.poll_event() {
          if matches!(ev, Event::User(_)) {
            return true;
          }
        } else {
          Timer::after(Duration::from_millis(5)).await;
        }
      }
    };
    let saw = drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
    assert!(saw, "B did not observe the user event");
  });
}
