//! `StreamEndpoint` super-machine tests over the real memberlist reliable
//! coordinator (plain-TCP `RawRecords` record layer).
//!
//! These cover the composition seam the per-runtime drivers depend on:
//! construction, the serf-command surface forwarding to the core, the
//! coordinator-driven dial (`poll_action` → `Connect`), and a two-endpoint
//! loopback that drives serf's `merge_remote_state` through the coordinator's
//! real push-pull path (relay one side's `poll_transport_transmit` into the
//! other's `handle_transport_data`) until the serf membership converges.
//!
//! Packet / FSM-level coverage lives in `endpoint::tests`, which drives the same
//! `StreamEndpoint` through `handle_packet` / `handle_timeout`.

use bytes::Bytes;
use core::net::SocketAddr;

use memberlist_proto::{
  EndpointOptions, Instant, PushPullKind, RawRecords, SeedableRng, SmallRng,
  streams::{LabelOptions, StreamAction},
};

use crate::{StreamEndpoint, members::MemberStatus, options::Options};

/// Loopback cluster label shared by both sides so the record-layer handshake
/// settles.
const CLUSTER: &[u8] = b"serf-loopback";

fn sa(port: u16) -> SocketAddr {
  format!("127.0.0.1:{port}").parse().unwrap()
}

/// Build a serf `StreamEndpoint<u32, SocketAddr, RawRecords>` rooted at `id` /
/// `port`, seeded deterministically.
fn ep(id: u32, port: u16) -> StreamEndpoint<u32, SocketAddr, RawRecords> {
  let inner_opts = EndpointOptions::new(id, sa(port))
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner =
    memberlist_proto::Endpoint::new_at(inner_opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let coord = memberlist_proto::streams::StreamEndpoint::<_, _, RawRecords>::new(
    inner,
    LabelOptions::new_in(Some(CLUSTER.to_vec()), ()),
    Box::new(|_addr: &SocketAddr| None),
    Box::new(|addr: &SocketAddr| *addr),
  );
  StreamEndpoint::new(coord, Options::new())
}

#[test]
fn constructs_alive_with_zero_clocks() {
  let e = ep(1, 7946);
  assert!(e.state().is_alive());
  assert_eq!(e.member_time(), 0);
  assert_eq!(e.event_time(), 0);
  assert_eq!(e.query_time(), 0);
}

#[test]
fn user_event_marks_local_state_dirty() {
  let mut e = ep(1, 7946);
  e.test_clear_dirty();
  e.user_event("deploy", Bytes::from_static(b"v2"), false)
    .expect("user_event on an alive endpoint");
  assert!(
    e.test_is_dirty(),
    "a user_event must mark the local-state snapshot dirty"
  );
}

#[test]
fn handle_packet_with_garbage_bytes_is_a_noop() {
  let mut e = ep(1, 7946);
  e.handle_packet(sa(9999), Bytes::from_static(b"\xff\xff"), Instant::ORIGIN);
  assert!(
    e.poll_event().is_none(),
    "an undecodable frame yields no serf event"
  );
}

/// A reconnect dial against a failed member surfaces a coordinator
/// `StreamAction::Connect` (the coordinator IS the driver and dials itself),
/// and the chosen address is captured at the `start_push_pull` call site.
#[test]
fn reconnect_dial_surfaces_a_connect_action() {
  let mut e = ep(1, 7946);
  // Seed the local node alive and one failed peer so the reconnect gate fires
  // (prob = num_failed / num_alive = 1/1 = 1.0).
  e.test_seed_member(1, MemberStatus::Alive, 1.into());
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.test_fire_reconnect(Instant::ORIGIN);

  assert_eq!(
    e.test_last_dial_addr(),
    Some(sa(7000)),
    "the reconnect dial targets the failed peer's address"
  );
  let action = e.poll_action();
  assert!(
    matches!(action, Some(StreamAction::Connect(_))),
    "the coordinator surfaces a Connect for the reconnect dial, got {action:?}"
  );
}

/// Decode a push-pull body fed through the coordinator's merge path and assert
/// serf folds the remote clock state in. Exercises serf's `merge_remote_state`
/// (the serf-side of state exchange) over the real coordinator, without
/// re-implementing the stream wire protocol.
#[test]
fn merge_remote_state_folds_remote_clocks() {
  use crate::typed::PushPullMessage;

  let mut e = ep(1, 7946);
  e.test_clear_dirty();

  // A remote push-pull body advancing the member / event / query clocks.
  let pp: PushPullMessage<u32> = PushPullMessage::new(
    42.into(),
    Vec::new(),
    Vec::new(),
    7.into(),
    Vec::new(),
    9.into(),
  );
  let encoded = crate::AnyMessage::<u32, SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode push-pull body");

  e.test_merge_remote_state(encoded, false);

  assert_eq!(
    e.member_time(),
    42,
    "member clock witnessed the remote ltime"
  );
  assert_eq!(e.event_time(), 7, "event clock witnessed the remote ltime");
  assert_eq!(e.query_time(), 9, "query clock witnessed the remote ltime");
}

/// Two-endpoint loopback: a dialer initiates a push-pull, the acceptor admits
/// the inbound connection, bytes shuttle both directions through the
/// coordinators' real transport surface, and the acceptor folds the dialer's
/// serf push-pull body — asserting the serf member clock converges across a
/// real exchange.
///
/// Serf's push-pull body carries the three Lamport clocks + member status
/// ltimes (not the full memberlist roster, which SWIM disseminates), so the
/// observable serf-layer outcome is the acceptor witnessing the dialer's higher
/// member clock (`merge_remote_state` witnesses each clock at `remote - 1`).
#[test]
fn loopback_push_pull_converges_member_clock() {
  let now = Instant::ORIGIN;
  let mut dialer = ep(1, 7946);
  let mut acceptor = ep(2, 7000);

  // Advance the dialer's serf member clock so its push-pull body carries a
  // higher ltime than the acceptor's (which starts at 0). The acceptor must
  // witness this clock through the merge.
  dialer.test_set_clocks(50, 0, 0);
  dialer.resync_local_state();
  assert_eq!(
    acceptor.member_time(),
    0,
    "the acceptor starts at member clock 0"
  );

  // Dialer: start a push-pull and pull the Connect off the action queue.
  let dial_exchange = {
    dialer
      .transport_mut()
      .start_push_pull(sa(7000), PushPullKind::Join, now);
    match dialer.poll_action() {
      Some(StreamAction::Connect(c)) => c.id(),
      other => panic!("dialer must surface a Connect, got {other:?}"),
    }
  };
  while dialer.poll_action().is_some() {}

  // Acceptor: admit the inbound connection, taking its exchange handle.
  let accept_exchange = acceptor
    .accept_connection(sa(7946), now)
    .expect("acceptor admits the inbound connection");

  // Shuttle bytes + half-close signals both directions until the acceptor has
  // witnessed the dialer's clock or both coordinators go idle.  A real driver
  // maps each coordinator action to a transport operation: bytes from
  // `poll_transport_transmit` are written to the peer's connection, and a
  // `Shutdown` / `Close` action is a `shutdown(write)` the peer reads as an EOF.
  let mut converged = false;
  for _ in 0..256 {
    let mut moved = false;

    // dialer -> acceptor: bytes, then any half-close as an EOF.
    let mut to_acceptor = Vec::new();
    while let Some((id, _peer, bytes)) = dialer.poll_transport_transmit() {
      if id == dial_exchange {
        to_acceptor.extend_from_slice(&bytes);
      }
    }
    if !to_acceptor.is_empty() {
      acceptor.handle_transport_data(accept_exchange, &to_acceptor, false, now);
      moved = true;
    }
    while let Some(action) = dialer.poll_action() {
      if let StreamAction::Shutdown(r) | StreamAction::Close(r) | StreamAction::Abort(r) = action {
        if r.id() == dial_exchange {
          acceptor.handle_transport_data(accept_exchange, &[], true, now);
          moved = true;
        }
      }
    }

    // acceptor -> dialer: bytes, then any half-close as an EOF.
    let mut to_dialer = Vec::new();
    while let Some((id, _peer, bytes)) = acceptor.poll_transport_transmit() {
      if id == accept_exchange {
        to_dialer.extend_from_slice(&bytes);
      }
    }
    if !to_dialer.is_empty() {
      dialer.handle_transport_data(dial_exchange, &to_dialer, false, now);
      moved = true;
    }
    while let Some(action) = acceptor.poll_action() {
      if let StreamAction::Shutdown(r) | StreamAction::Close(r) | StreamAction::Abort(r) = action {
        if r.id() == accept_exchange {
          dialer.handle_transport_data(dial_exchange, &[], true, now);
          moved = true;
        }
      }
    }

    // Tick both so the bridges advance their send/recv halves and reap on
    // completion. The merge is sieved into serf inside `handle_transport_data`.
    dialer.handle_timeout(now);
    acceptor.handle_timeout(now);
    while dialer.poll_event().is_some() {}
    while acceptor.poll_event().is_some() {}

    // The acceptor witnessed the dialer's serf member clock through the merge.
    if acceptor.member_time() >= 49 {
      converged = true;
      break;
    }
    if !moved {
      break;
    }
  }

  assert!(
    converged,
    "the loopback push-pull exchange converged: the acceptor witnessed the dialer's \
     serf member clock (started at 0, advanced to {})",
    acceptor.member_time()
  );
  // The acceptor also learned the dialer (node 1) as a member through the
  // exchange's membership push.
  assert!(
    acceptor.test_member_status(1).is_some(),
    "the acceptor learned the dialer (node 1) as a member over the exchange"
  );
}
