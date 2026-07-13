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
  let mut e = StreamEndpoint::new(coord, Options::new());
  let _ = e.poll_event();
  e
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
  e.user_event("deploy", Bytes::from_static(b"v2"), false, Instant::ORIGIN)
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

  e.test_merge_remote_state(encoded);

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

/// A completed Join push/pull surfaces `Event::ExchangeCompleted` with
/// `kind() == ExchangeKind::PushPull` on the dialer side.
///
/// The inner coordinator emits `memberlist_proto::Event::ExchangeCompleted`
/// when the outbound bridge is reaped; serf's `on_inner_event` must forward
/// it rather than swallowing it so that a driver awaiting a join can resolve
/// directly from the event stream.
#[test]
fn completed_join_push_pull_surfaces_exchange_completed_event() {
  use crate::{ExchangeKind, ExchangeStatus, event::Event};

  let now = Instant::ORIGIN;
  let mut dialer = ep(1, 7946);
  let mut acceptor = ep(2, 7000);

  // Dialer: start a Join push/pull and grab the Connect action's exchange id.
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

  // Acceptor: admit the inbound connection.
  let accept_exchange = acceptor
    .accept_connection(sa(7946), now)
    .expect("acceptor admits the inbound connection");

  // Shuttle bytes and half-close signals until the exchange completes.
  for _ in 0..256 {
    let mut moved = false;

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

    dialer.handle_timeout(now);
    acceptor.handle_timeout(now);

    // Look for Event::ExchangeCompleted on the dialer side.
    while let Some(ev) = dialer.poll_event() {
      if let Event::ExchangeCompleted(ref c) = ev {
        assert_eq!(
          c.kind(),
          ExchangeKind::PushPull,
          "ExchangeCompleted kind must be PushPull for a Join push/pull"
        );
        assert_eq!(
          c.outcome(),
          ExchangeStatus::Succeeded,
          "Join push/pull outcome must be Succeeded"
        );
        return;
      }
    }
    while acceptor.poll_event().is_some() {}

    if !moved {
      break;
    }
  }

  panic!(
    "no Event::ExchangeCompleted(kind=PushPull) surfaced from the dialer after a \
     completed loopback Join push/pull"
  );
}

// ── composition seam: the driver-facing forwarders ────────────────────────────
//
// The stream and QUIC super-machines expose the same serf surface over different
// transports; these pin the stream side of that surface so the two cannot drift.

use core::time::Duration;
use memberlist_proto::{EncodeOptions, Node, encode_outgoing, typed::Alive};

use crate::{
  event::{Event, MemberEventKind},
  options::Options as SerfOptions,
};

/// `Instant::ORIGIN + s` seconds.
fn t_secs(s: u64) -> Instant {
  Instant::ORIGIN + Duration::from_secs(s)
}

/// Wrap a raw membership endpoint into the plain-TCP reliable coordinator.
fn coord(
  inner: memberlist_proto::Endpoint<u32, SocketAddr>,
) -> memberlist_proto::streams::StreamEndpoint<u32, SocketAddr, RawRecords> {
  memberlist_proto::streams::StreamEndpoint::<_, _, RawRecords>::new(
    inner,
    LabelOptions::new_in(Some(CLUSTER.to_vec()), ()),
    Box::new(|_addr: &SocketAddr| None),
    Box::new(|addr: &SocketAddr| *addr),
  )
}

/// The inner memberlist options every fixture roots at.
fn inner_opts(id: u32, port: u16) -> EndpointOptions<u32, SocketAddr> {
  EndpointOptions::new(id, sa(port))
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap())
}

/// A raw membership endpoint at `id` / `port`, deterministically seeded.
fn inner(id: u32, port: u16) -> memberlist_proto::Endpoint<u32, SocketAddr> {
  memberlist_proto::Endpoint::new_at(
    inner_opts(id, port),
    Instant::ORIGIN,
    SmallRng::seed_from_u64(0),
  )
}

/// Encode one memberlist gossip `Message` exactly as the wire carries it, under
/// the loopback cluster label.
fn gossip_frame(msg: &memberlist_proto::typed::Message<u32, SocketAddr>) -> Bytes {
  encode_outgoing(msg, &EncodeOptions::new(None)).expect("encode memberlist gossip frame")
}

/// An `Alive` for `id` at `addr` — the gossip message that admits a peer.
fn alive(id: u32, addr: SocketAddr) -> memberlist_proto::typed::Message<u32, SocketAddr> {
  memberlist_proto::typed::Message::Alive(Alive::new(1, Node::new(id, addr)))
}

/// The two coalescer shed counters are INJECTED: `new_with_rng_in` must thread
/// `user_drop` into the user coalescer's slot and `member_drop` into the member
/// coalescer's, without transposing them.
#[test]
fn new_with_rng_in_threads_each_injected_drop_counter_to_its_own_slot() {
  let e: StreamEndpoint<u32, SocketAddr, RawRecords> = StreamEndpoint::new_with_rng_in(
    coord(inner(1, 7946)),
    SerfOptions::new(),
    SmallRng::seed_from_u64(0),
    7u64,
    9u64,
  );
  assert_eq!(e.coalesced_user_events_dropped(), 7);
  assert_eq!(e.coalesced_member_events_dropped(), 9);
}

/// The super-machine roots serf at the coordinator's local id, and publishes the
/// membership view the driver observes.
#[test]
fn local_id_and_members_snapshot_come_from_the_coordinator() {
  let mut e = ep(42, 7946);
  assert_eq!(*e.local_id(), 42u32);
  assert_eq!(e.health_score(), 0, "a fresh node is healthy");

  e.test_seed_member(2, MemberStatus::Leaving, 1.into());
  let mut got: Vec<(u32, MemberStatus)> = e
    .members_snapshot()
    .iter()
    .map(|m| (*m.node().id_ref(), m.status()))
    .collect();
  got.sort_by_key(|(id, _)| *id);
  assert_eq!(
    got,
    vec![(2, MemberStatus::Leaving), (42, MemberStatus::Alive)],
    "the snapshot publishes every tracked member with its live status"
  );
}

/// A per-member reconnect-timeout override installed through the builder shortens
/// the reaper's failed-member window.
#[test]
fn builder_reconnect_delegate_overrides_the_failed_reap_timeout() {
  struct TenSeconds;
  impl crate::ReconnectDelegate<u32, SocketAddr> for TenSeconds {
    fn reconnect_timeout(
      &self,
      _member: &crate::members::Member<u32, SocketAddr>,
      _default: Duration,
    ) -> Duration {
      Duration::from_secs(10)
    }
  }

  let mut e =
    StreamEndpoint::<u32, SocketAddr, RawRecords>::new(coord(inner(1, 7946)), SerfOptions::new())
      .with_reconnect_delegate(Some(Box::new(TenSeconds)));
  let _ = e.poll_event();
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.test_fire_reap(t_secs(11));
  assert_eq!(
    e.test_member_status(2),
    None,
    "the delegate's 10s timeout reaps the failed member at t+11s"
  );
}

/// A well-formed gossip frame fed to `handle_packet` reaches the coordinator and
/// the resulting `NodeJoined` is sieved into serf on the same call; `handle_message`
/// is the same path for an already-decoded message.
#[test]
fn a_decoded_alive_admits_the_peer_through_either_ingress() {
  let mut packet_side = ep(1, 7946);
  packet_side.handle_packet(sa(7000), gossip_frame(&alive(2, sa(7000))), Instant::ORIGIN);
  assert_eq!(
    packet_side.test_member_status(2),
    Some(MemberStatus::Alive),
    "handle_packet decodes the frame and admits the peer"
  );
  assert!(
    matches!(packet_side.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join)
  );

  let mut typed_side = ep(1, 7946);
  typed_side.handle_message(sa(7000), alive(2, sa(7000)), Instant::ORIGIN);
  assert_eq!(
    typed_side.test_member_status(2),
    Some(MemberStatus::Alive),
    "handle_message feeds the already-decoded message to the same path"
  );
}

/// `handle_gossip` buffers a raw inbound datagram for the codec-owning driver to
/// drain via `poll_memberlist_ingress`, decode, and feed back through
/// `handle_packet` — the machine never decodes it in place.
#[test]
fn handle_gossip_buffers_the_datagram_for_the_codec_owning_driver() {
  let mut e = ep(1, 7946);
  let frame = gossip_frame(&alive(2, sa(7000)));
  e.handle_gossip(sa(7000), &frame, Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    None,
    "handle_gossip must not decode the frame itself"
  );
  let (from, bytes) = e
    .poll_memberlist_ingress()
    .expect("the datagram is buffered for the driver");
  assert_eq!(from, sa(7000));

  e.handle_packet(from, bytes, Instant::ORIGIN);
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "handle_gossip -> poll_memberlist_ingress -> handle_packet admits the peer"
  );
}

/// `poll_timeout` folds the coordinator's deadline with serf's own: an
/// un-scheduled coordinator leaves serf's reap deadline as the wake, and a
/// shut-down machine with an un-scheduled coordinator requests no wake at all.
#[test]
fn poll_timeout_folds_the_coordinator_and_serf_deadlines() {
  let mut e = ep(1, 7946);
  assert_eq!(
    e.poll_timeout(),
    Some(t_secs(15)),
    "an un-scheduled coordinator leaves serf's reap deadline as the wake"
  );

  e.start_scheduling(Instant::ORIGIN);
  let inner_deadline = e
    .transport_mut()
    .poll_timeout()
    .expect("start_scheduling arms the coordinator's timers");
  assert!(inner_deadline < t_secs(15));
  assert_eq!(
    e.poll_timeout(),
    Some(inner_deadline),
    "the fold takes the minimum of the two deadlines"
  );

  // A machine that lost an id-conflict vote schedules no serf wakeups.
  let mut dead = ep(1, 7946);
  let qid = dead.test_register_conflict_query(t_secs(3600));
  dead.test_fold_conflict_response(qid, 200u32, true);
  dead.test_fold_conflict_response(qid, 201u32, false);
  dead.test_fold_conflict_response(qid, 202u32, false);
  dead.test_fire_due_query_closes(t_secs(3601));
  assert!(matches!(dead.poll_event(), Some(Event::Shutdown)));
  assert_eq!(
    dead.poll_timeout(),
    None,
    "a shut-down serf with an un-scheduled coordinator requests no wakeup"
  );

  // Arming the coordinator re-supplies the only deadline left on a dead machine.
  dead.start_scheduling(Instant::ORIGIN);
  let only = dead.transport_mut().poll_timeout();
  assert!(only.is_some(), "the coordinator schedules its own timers");
  assert_eq!(
    dead.poll_timeout(),
    only,
    "with serf shut down the coordinator's deadline is the whole fold"
  );
}

/// The wire-size forwarders report the coordinator's CONFIGURED limits — the
/// driver sizes its recv buffer and observation budget from them.
#[test]
fn wire_size_forwarders_report_the_configured_limits() {
  let opts = inner_opts(1, 7946)
    .with_gossip_mtu(1234)
    .with_max_stream_frame_size(4096);
  let raw = memberlist_proto::Endpoint::new_at(opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let mut e = StreamEndpoint::<u32, SocketAddr, RawRecords>::new(coord(raw), SerfOptions::new());
  let _ = e.poll_event();

  assert_eq!(e.gossip_mtu(), 1234);
  assert_eq!(e.max_stream_frame_size(), 4096);
}

/// A dial that never connects, and a live exchange that errors, both terminate
/// their exchange without producing a serf-level membership signal: transport
/// liveness is never a membership signal, so neither may invent or evict a member.
///
/// An `ignore_old` join that dies this way leaves its ignore-join entry behind for
/// the driver to clear, since no merge ever consumed it.
#[test]
fn a_failed_dial_and_an_errored_exchange_are_not_membership_signals() {
  let mut e = ep(1, 7946);

  let dial_stream = e.start_join_push_pull(sa(7000), true, Instant::ORIGIN);
  let dial_exchange = match e.poll_action() {
    Some(StreamAction::Connect(c)) => c.id(),
    other => panic!("the dialer must surface a Connect, got {other:?}"),
  };
  while e.poll_action().is_some() {}
  assert!(
    e.test_has_ignore_join_stream(dial_stream),
    "the ignore_old join records its exchange"
  );

  e.handle_dial_failed(dial_exchange, Instant::ORIGIN);
  assert!(
    e.test_member_status(2).is_none(),
    "a failed dial must not invent membership"
  );
  assert_failed_exchange(
    &mut e,
    "a failed dial resolves the exchange as Failed, never as a membership change",
  );
  assert!(
    e.test_has_ignore_join_stream(dial_stream),
    "no merge consumed the entry, so the driver must still clear it"
  );
  e.clear_ignore_join_stream(dial_stream);
  assert!(!e.test_has_ignore_join_stream(dial_stream));

  let second_stream = e.start_push_pull(sa(7001), PushPullKind::Join, Instant::ORIGIN);
  let second_exchange = match e.poll_action() {
    Some(StreamAction::Connect(c)) => c.id(),
    other => panic!("the dialer must surface a Connect, got {other:?}"),
  };
  while e.poll_action().is_some() {}

  e.handle_transport_error(second_exchange, Instant::ORIGIN);
  assert!(
    e.test_member_status(2).is_none(),
    "a transport error must not invent membership"
  );
  assert_failed_exchange(
    &mut e,
    "a transport error resolves the exchange as Failed, never as a membership change",
  );
  assert!(
    !e.test_has_ignore_join_stream(second_stream),
    "a plain join recorded no ignore-join entry to begin with"
  );
}

/// Drain `e`'s event queue, asserting the ONLY events it yields are terminal
/// push/pull `ExchangeCompleted(Failed)` resolutions — never a membership change.
fn assert_failed_exchange(e: &mut StreamEndpoint<u32, SocketAddr, RawRecords>, why: &str) {
  use crate::{ExchangeKind, ExchangeStatus};

  let mut saw = false;
  while let Some(ev) = e.poll_event() {
    match ev {
      Event::ExchangeCompleted(c) => {
        assert_eq!(c.kind(), ExchangeKind::PushPull);
        assert_eq!(c.outcome(), ExchangeStatus::Failed, "{why}");
        saw = true;
      }
      other => panic!("{why}, got {other:?}"),
    }
  }
  assert!(saw, "{why}");
}

/// A custom join-merge predicate installs through the coordinator, and the
/// coordinate-reset counter reads through to the Vivaldi client.
#[test]
fn operator_forwarders_reach_the_inner_machine() {
  let mut e = ep(1, 7946);

  struct RejectAll;
  impl memberlist_proto::delegate::MergeDelegate<u32, SocketAddr> for RejectAll {
    fn notify_merge(
      &self,
      _peers: memberlist_proto::MaybeOwned<
        '_,
        [memberlist_proto::typed::NodeState<u32, SocketAddr>],
      >,
    ) -> bool {
      false
    }
  }
  e.set_merge_delegate(RejectAll);

  #[cfg(feature = "coordinates")]
  assert_eq!(
    e.coordinate_resets(),
    Some(0),
    "a fresh coordinate client has reset nothing"
  );
}

// ── gossip-plane encryption ───────────────────────────────────────────────────

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
mod encryption {
  use super::*;
  use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

  #[cfg(feature = "aes-gcm")]
  fn key(b: u8) -> SecretKey {
    SecretKey::Aes128([b; 16])
  }
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  fn key(b: u8) -> SecretKey {
    SecretKey::ChaCha20Poly1305([b; 32])
  }

  /// With no keyring the gossip transforms are the identity; installing one
  /// re-keys the live plane so an encrypted datagram round-trips and no longer
  /// leaves as plaintext.
  #[test]
  fn setting_the_encryption_options_re_keys_the_live_gossip_plane() {
    let mut e = ep(1, 7946);
    assert!(!e.encryption_options().is_enabled());

    let plain = b"\x05gossip-frame";
    assert_eq!(
      e.encrypt_gossip(plain)
        .expect("encrypt without a keyring")
        .as_slice(),
      plain,
      "with no keyring the datagram is left unencrypted"
    );

    e.set_encryption_options(EncryptionOptions::new().with_keyring(Keyring::new(key(1))));
    assert_eq!(
      e.encryption_options().keyring().map(|kr| *kr.primary_ref()),
      Some(key(1)),
      "the coordinator reports the primary key outbound datagrams seal under"
    );

    let sealed = e.encrypt_gossip(plain).expect("encrypt under the keyring");
    assert_ne!(
      sealed.as_slice(),
      plain,
      "an encrypted-cluster datagram must not leave as plaintext"
    );
    assert_eq!(
      e.decrypt_gossip(&sealed)
        .expect("decrypt under the keyring"),
      plain,
      "the datagram round-trips through the same keyring"
    );

    let mut foreign = ep(2, 7000);
    foreign.set_encryption_options(EncryptionOptions::new().with_keyring(Keyring::new(key(2))));
    assert!(
      foreign.decrypt_gossip(&sealed).is_err(),
      "a frame the keyring cannot open must be an error, never silently admitted"
    );
  }
}
