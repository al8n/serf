//! Query-plane end-to-end over the embassy-net gossip self-delivery loopback.
//!
//! serf directs a node's response to its OWN locally-originated query to the
//! originator's advertise address = THIS node's address. A real OS UDP socket
//! loops such a self-addressed datagram back into recv; embassy-net (smoltcp
//! underneath) does not, so the driver emulates the OS self-delivery. This test
//! exercises that path: the query round-trip proves the originator observes its
//! OWN (local, self-addressed) response alongside the remote peer's. Without the
//! driver's self-delivery loopback the originator never observes its own response,
//! so `a_resp` would hold only `"b"` and the test fails.

#![allow(clippy::collapsible_if)]

mod support;

use core::{net::SocketAddr, time::Duration as CoreDuration};

use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{Bytes, Event, QueryParams, Serf, TransformOptions, now};
use smol_str::SmolStr;

use support::cluster::{
  NodeBufs, POOL, build_node, build_sockets, build_stack, devices, drive, join_and_converge,
};

/// Drain a node's buffered events: answer every observed `Event::Query` (directing
/// the reply to the query's originator), and record every `Event::QueryResponse`'s
/// responder id.
fn service_queries(node: &Serf<SmolStr, SocketAddr>, responders: &mut Vec<SmolStr>) {
  let mut pending = Vec::new();
  while let Some(ev) = node.poll_event() {
    match ev {
      Event::Query(qe) => pending.push(qe),
      Event::QueryResponse(qr) => responders.push(qr.from().id_ref().clone()),
      _ => {}
    }
  }
  for qe in pending {
    // Ignoring Err: a duplicate / past-deadline respond is a no-op the test tolerates.
    let _ = node.respond(&qe, Bytes::from_static(b"pong"));
  }
}

/// Two converged nodes; A issues a query; the originator must collect responses
/// from BOTH the remote peer AND itself. The local node self-processes its own
/// query and directs its reply to its own advertise address — a self-addressed
/// gossip datagram embassy-net does not loop back on its own.
#[test]
fn query_collects_local_and_remote_responses() {
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
        .query(
          "ping",
          Bytes::from_static(b"q"),
          QueryParams {
            timeout: CoreDuration::from_secs(4),
            ..Default::default()
          },
        )
        .expect("query from a running node");

      // A must collect responses from BOTH itself ("a", via the loopback) and B
      // ("b", over the wire). Both nodes answer every Query they observe.
      let mut a_resp: Vec<SmolStr> = Vec::new();
      let mut b_resp: Vec<SmolStr> = Vec::new();
      loop {
        service_queries(&ml_a, &mut a_resp);
        service_queries(&ml_b, &mut b_resp);
        if a_resp.iter().any(|id| id == "a") && a_resp.iter().any(|id| id == "b") {
          return a_resp;
        }
        Timer::after(Duration::from_millis(5)).await;
      }
    };
    let a_resp = drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
    assert!(
      a_resp.iter().any(|id| id == "a"),
      "originator did not observe its OWN self-addressed, looped-back response: {a_resp:?}"
    );
    assert!(
      a_resp.iter().any(|id| id == "b"),
      "originator did not observe the remote peer's response: {a_resp:?}"
    );
  });
}
