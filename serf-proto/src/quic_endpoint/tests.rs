//! `QuicEndpoint` super-machine tests over the real memberlist QUIC coordinator.
//!
//! These cover the composition seam the per-runtime QUIC driver depends on:
//! construction, the serf-command surface forwarding to the core, and a
//! two-endpoint loopback that drives serf's `merge_remote_state` through the
//! coordinator's real QUIC push-pull path (relay one side's `poll_transmit` UDP
//! datagrams into the other's `handle_udp`) until the serf membership converges.
//!
//! The QUIC coordinator owns the quinn handshake + reliable bidi lifecycle
//! internally and dials itself, so the loopback ferries the single combined UDP
//! egress in both directions (the quinn handshake needs both paths relayed)
//! rather than per-exchange stream chunks.
//!
//! Packet / FSM-level coverage lives in `endpoint::tests`, which drives the same
//! super-machine surface through `handle_packet` / `handle_timeout`.

use bytes::Bytes;
use core::{net::SocketAddr, time::Duration};
use std::sync::Arc;

use memberlist_proto::{
  DatagramSendStatus, EncodeOptions, EndpointOptions, Instant, Node, PushPullKind, QuicOptions,
  SeedableRng, SmallRng, UnreliableTransport, encode_outgoing,
  typed::{Alive, Message},
};
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::{
  AnyMessage, LamportTime, QuicEndpoint,
  endpoint::{QueryId, QueryParams},
  event::{Event, MemberEventKind},
  members::{IntentKind, MemberStatus},
  options::Options,
  typed::{QueryFlag, QueryMessage, QueryResponseMessage, RelayMessage, Tags, UserEventMessage},
};

fn sa(port: u16) -> SocketAddr {
  format!("127.0.0.1:{port}").parse().unwrap()
}

/// `Instant::ORIGIN + s` seconds — the wall clock the tests thread through the
/// machine.
fn t_secs(s: u64) -> Instant {
  Instant::ORIGIN + Duration::from_secs(s)
}

/// A self-signed cert + key for `localhost`, for the test TLS bundle.
fn self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
  let cert = CertificateDer::from(ck.cert.der().to_vec());
  let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
  (vec![cert], key)
}

fn test_endpoint_config(reset_key: &[u8]) -> quinn_proto::EndpointConfig {
  let hmac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, reset_key);
  quinn_proto::EndpointConfig::new(Arc::new(hmac))
}

fn test_server() -> quinn_proto::ServerConfig {
  let (chain, key) = self_signed();
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let rustls_server = rustls::ServerConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
  let qsc =
    quinn_proto::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server)).unwrap();
  quinn_proto::ServerConfig::with_crypto(Arc::new(qsc))
}

/// Accept-any server-cert verifier — test only.
#[derive(Debug)]
struct AnyServer;

impl rustls::client::danger::ServerCertVerifier for AnyServer {
  fn verify_server_cert(
    &self,
    _end_entity: &CertificateDer,
    _intermediates: &[CertificateDer],
    _server_name: &rustls_pki_types::ServerName,
    _ocsp_response: &[u8],
    _now: rustls_pki_types::UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    _message: &[u8],
    _cert: &CertificateDer,
    _dss: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }

  fn verify_tls13_signature(
    &self,
    _message: &[u8],
    _cert: &CertificateDer,
    _dss: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }

  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    rustls::crypto::ring::default_provider()
      .signature_verification_algorithms
      .supported_schemes()
  }
}

fn test_client() -> quinn_proto::ClientConfig {
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let cfg = rustls::ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyServer))
    .with_no_client_auth();
  let qcc = quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(cfg)).unwrap();
  quinn_proto::ClientConfig::new(Arc::new(qcc))
}

/// A QUIC config bundle with a 20s idle timeout (well past a single-instant
/// localhost handshake) and the given unreliable-transport wire.
fn quic_options_on(wire: UnreliableTransport) -> QuicOptions {
  let mut transport = quinn_proto::TransportConfig::default();
  transport.max_idle_timeout(Some(
    quinn_proto::IdleTimeout::try_from(Duration::from_secs(20)).unwrap(),
  ));
  QuicOptions::new(
    test_endpoint_config(&[0x5au8; 32]),
    test_server(),
    test_client(),
    transport,
    "localhost",
    wire,
  )
}

/// The default bundle: datagram-mode unreliable transport.
fn test_quic_options() -> QuicOptions {
  quic_options_on(UnreliableTransport::Datagram)
}

/// Wrap `inner_opts` into the memberlist QUIC coordinator, seeding quinn's rng
/// from the bound port so two loopback coordinators draw distinct connection ids.
fn coord(
  inner_opts: EndpointOptions<u32, SocketAddr>,
  quic: QuicOptions,
  port: u16,
) -> memberlist_proto::QuicEndpoint<u32> {
  let inner =
    memberlist_proto::Endpoint::new_at(inner_opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let mut seed = [0u8; 32];
  seed[..2].copy_from_slice(&port.to_le_bytes());
  memberlist_proto::QuicEndpoint::<u32>::with_quinn_rng_seed(inner, quic, Some(seed))
}

/// The inner memberlist options every fixture roots at.
fn inner_opts(id: u32, port: u16) -> EndpointOptions<u32, SocketAddr> {
  EndpointOptions::new(id, sa(port))
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap())
}

/// Build a serf `QuicEndpoint<u32>` rooted at `id` / `port` with caller-supplied
/// serf `opts`, seeded deterministically.  The inner memberlist emits
/// `NodeJoined(self)` on construction; drain it so every test starts from the
/// post-self-join-drained state (self is a member).
fn ep_with_options(id: u32, port: u16, opts: Options) -> QuicEndpoint<u32> {
  let mut e = QuicEndpoint::new(coord(inner_opts(id, port), test_quic_options(), port), opts);
  let _ = e.poll_event();
  e
}

/// Build a serf `QuicEndpoint<u32>` rooted at `id` / `port` with default serf
/// options.
fn ep(id: u32, port: u16) -> QuicEndpoint<u32> {
  ep_with_options(id, port, Options::new())
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

#[test]
fn startup_self_join_coalesces_from_the_scheduling_instant() {
  // The coordinator queues the local self-join during construction, before any
  // live-time entry point has run. `start_scheduling` — the driver's first call,
  // made with its live clock — must fold that queued join under that instant:
  // deferring it to a later un-latched `poll_event` would arm the coalescing
  // window at the machine's origin, already overdue, flushing the self join
  // immediately instead of holding it for the configured window.
  let inner_opts = EndpointOptions::new(1u32, sa(7946))
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner =
    memberlist_proto::Endpoint::new_at(inner_opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let coord = memberlist_proto::QuicEndpoint::<u32>::with_quinn_rng_seed(
    inner,
    test_quic_options(),
    Some([0x5au8; 32]),
  );
  let opts = Options::new()
    .with_coalesce_period(Duration::from_secs(10))
    .with_quiescent_period(Duration::from_secs(2));
  let mut e: QuicEndpoint<u32> = QuicEndpoint::new(coord, opts);

  // The driver arms the schedulers at its live clock; the queued self-join is
  // folded here, opening the member window at t100 with its quiescent deadline
  // at t102.
  let t = |s: u64| Instant::ORIGIN + Duration::from_secs(s);
  e.start_scheduling(t(100));
  assert!(
    e.poll_event().is_none(),
    "the startup self join is buffered in the window, not delivered immediately"
  );

  // Before the window's own deadline nothing flushes — a window armed at the
  // origin would be long overdue here and would flush the join early.
  e.handle_timeout(t(101));
  assert!(
    e.poll_event().is_none(),
    "the startup window holds until the deadline armed from the scheduling instant"
  );

  // At the deadline the self join is delivered.
  e.handle_timeout(t(102));
  let ev = e
    .poll_event()
    .expect("the startup window flushes at its own deadline");
  match ev {
    Event::Member(me) => {
      assert_eq!(me.kind(), MemberEventKind::Join);
      let ids: Vec<u32> = me.members().iter().map(|m| *m.node().id_ref()).collect();
      assert_eq!(ids, vec![1], "the self join is delivered from the window");
    }
    other => panic!("expected the coalesced self join, got {other:?}"),
  }
}

/// A reconnect dial against a failed member targets the failed peer's address.
/// On QUIC the coordinator IS the driver and dials itself (sieving its own
/// `DialRequested` into a private queue), so the observable serf-side fact is
/// the chosen address captured at the `start_push_pull` call site.
#[test]
fn reconnect_dial_targets_the_failed_peer() {
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
}

/// Decode a push-pull body fed through the coordinator's merge path and assert
/// serf folds the remote clock state in. Exercises serf's `merge_remote_state`
/// (the serf-side of state exchange) over the real QUIC coordinator, without
/// driving the QUIC wire protocol.
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

/// Two-endpoint QUIC loopback: a dialer initiates a push-pull, the QUIC
/// coordinators run the quinn handshake and the reliable bidi exchange in both
/// directions (their single combined UDP egress relayed into the peer's
/// `handle_udp`), and the acceptor folds the dialer's serf push-pull body —
/// asserting the serf member clock converges across a real QUIC exchange.
///
/// Serf's push-pull body carries the three Lamport clocks + member status
/// ltimes (not the full memberlist roster, which SWIM disseminates), so the
/// observable serf-layer outcome is the acceptor witnessing the dialer's higher
/// member clock (`merge_remote_state` witnesses each clock at `remote - 1`).
#[test]
fn loopback_push_pull_converges_member_clock() {
  let now = Instant::now();
  let dialer_addr = sa(7946);
  let acceptor_addr = sa(7000);
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

  // Dialer: start a push-pull. The QUIC coordinator dials in-band (opens the
  // quinn connection itself); the Initial emerges on the next `poll_transmit`.
  dialer
    .transport_mut()
    .start_push_pull(acceptor_addr, PushPullKind::Join, now);

  // Ferry the single combined UDP egress both directions and tick both sides
  // until the acceptor has witnessed the dialer's clock, or both coordinators
  // go idle. A real driver writes each `poll_transmit` datagram to the named
  // peer's socket; here we deliver it straight into the peer's `handle_udp`.
  let mut converged = false;
  for _ in 0..400 {
    let mut moved = false;

    while let Some((to, bytes)) = dialer.poll_transmit() {
      if to == acceptor_addr {
        acceptor.handle_udp(dialer_addr, &bytes, now);
        moved = true;
      }
    }
    while let Some((to, bytes)) = acceptor.poll_transmit() {
      if to == dialer_addr {
        dialer.handle_udp(acceptor_addr, &bytes, now);
        moved = true;
      }
    }

    // Tick both so quinn drives the handshake, the bridges pump their
    // send/recv halves, and the merge is sieved into serf.
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
    "the loopback QUIC push-pull exchange converged: the acceptor witnessed \
     the dialer's serf member clock (started at 0, advanced to {})",
    acceptor.member_time()
  );
  // The acceptor also learned the dialer (node 1) as a member through the
  // exchange's membership push.
  assert!(
    acceptor.test_member_status(1).is_some(),
    "the acceptor learned the dialer (node 1) as a member over the exchange"
  );
}

/// The operator forwarders reach the inner machine: a fresh endpoint is
/// healthy (score zero), a merge predicate installs through the coordinator,
/// and the coordinate-reset counter reads through to the client when
/// coordinates are enabled.
#[test]
fn operator_forwarders_reach_the_inner_machine() {
  let mut e = ep(1, 7952);
  assert_eq!(e.health_score(), 0, "a fresh node is healthy");

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

// ── construction ──────────────────────────────────────────────────────────────

/// The two coalescer shed counters are INJECTED, not created: `new_with_rng_in`
/// must thread `user_drop` into the user coalescer's slot and `member_drop` into
/// the member coalescer's, without transposing them.  A driver that shares these
/// counters with a detached handle reads the wrong metric if they cross.
#[test]
fn new_with_rng_in_threads_each_injected_drop_counter_to_its_own_slot() {
  let e: QuicEndpoint<u32> = QuicEndpoint::new_with_rng_in(
    coord(inner_opts(1, 7946), test_quic_options(), 7946),
    Options::new(),
    SmallRng::seed_from_u64(0),
    7u64,
    9u64,
  );
  assert_eq!(
    e.coalesced_user_events_dropped(),
    7,
    "the user shed count reads back the injected user_drop"
  );
  assert_eq!(
    e.coalesced_member_events_dropped(),
    9,
    "the member shed count reads back the injected member_drop"
  );
}

/// The super-machine roots serf at the coordinator's local id: `new` reads the
/// id off the membership endpoint rather than taking it as a separate parameter,
/// so the two can never disagree.
#[test]
fn local_id_is_the_coordinators_membership_id() {
  let e = ep(42, 7946);
  assert_eq!(*e.local_id(), 42u32);
  assert_eq!(
    e.num_members(),
    1,
    "the construction self-join leaves the local node as the sole member"
  );
  let snapshot = e.members_snapshot();
  assert_eq!(snapshot.len(), 1, "the snapshot carries the local member");
  assert_eq!(*snapshot[0].node().id_ref(), 42u32);
  assert_eq!(snapshot[0].status(), MemberStatus::Alive);
}

/// `members_snapshot` publishes every tracked member — alive, leaving, left, and
/// failed-within-the-reap-window alike — so a driver's observable view does not
/// silently drop tombstones.
#[test]
fn members_snapshot_carries_every_tracked_status() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Leaving, 1.into());
  e.test_seed_failed_member_by_status(3, 1.into(), Instant::ORIGIN);
  e.test_seed_left_member_by_status(4, 1.into(), Instant::ORIGIN);

  let mut got: Vec<(u32, MemberStatus)> = e
    .members_snapshot()
    .iter()
    .map(|m| (*m.node().id_ref(), m.status()))
    .collect();
  got.sort_by_key(|(id, _)| *id);

  assert_eq!(
    got,
    vec![
      (1, MemberStatus::Alive),
      (2, MemberStatus::Leaving),
      (3, MemberStatus::Failed),
      (4, MemberStatus::Left),
    ],
    "the snapshot must publish every tracked member with its live status"
  );
}

// ── reconnect delegate ────────────────────────────────────────────────────────

/// A per-member reconnect-timeout override installed through the builder
/// `with_reconnect_delegate` shortens the reaper's failed-member window: the
/// delegate's 10 s timeout reaps at t+11 s, where the flat 24 h default would
/// still be holding the member.
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

  let e = QuicEndpoint::<u32>::new(
    coord(inner_opts(1, 7946), test_quic_options(), 7946),
    Options::new(),
  );
  let mut e = e.with_reconnect_delegate(Some(Box::new(TenSeconds)));
  let _ = e.poll_event();
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.test_fire_reap(t_secs(11));
  assert_eq!(
    e.test_member_status(2),
    None,
    "the delegate's 10s timeout reaps the failed member at t+11s"
  );
}

/// Clearing the delegate restores the flat configured timeouts: the same
/// failed member that the 10 s delegate would have reaped at t+11 s survives once
/// the delegate is set back to `None`.
#[test]
fn clearing_the_reconnect_delegate_restores_the_flat_timeout() {
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

  let mut e = ep(1, 7946);
  e.set_reconnect_delegate(Some(Box::new(TenSeconds)));
  e.set_reconnect_delegate(None);
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.test_fire_reap(t_secs(11));
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Failed),
    "with no delegate the flat 24h reconnect_timeout still holds the member at t+11s"
  );
}

// ── transport-level driver surface ────────────────────────────────────────────

/// Encode one memberlist gossip `Message` exactly as the wire carries it (no
/// cluster label), for the ingress paths that take raw frame bytes.
fn gossip_frame(msg: &Message<u32, SocketAddr>) -> Bytes {
  encode_outgoing(msg, &EncodeOptions::new(None)).expect("encode memberlist gossip frame")
}

/// An `Alive` for `id` at `addr`, the gossip message that admits a peer into the
/// inner membership.
fn alive(id: u32, addr: SocketAddr) -> Message<u32, SocketAddr> {
  Message::Alive(Alive::new(1, Node::new(id, addr)))
}

/// A well-formed gossip frame fed to `handle_packet` reaches the coordinator:
/// the inner machine admits the peer and the resulting `NodeJoined` is sieved
/// into serf on the same call, so the member and its `Member(Join)` event are
/// observable without a further tick.
#[test]
fn handle_packet_with_an_alive_frame_admits_the_peer_into_serf() {
  let mut e = ep(1, 7946);
  e.handle_packet(sa(7000), gossip_frame(&alive(2, sa(7000))), Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "the decoded Alive must reach the coordinator and surface as a serf member"
  );
  assert_eq!(e.num_members(), 2, "self plus the admitted peer");
  let ev = e
    .poll_event()
    .expect("the admitted peer emits a serf event");
  assert!(
    matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Join),
    "expected Member(Join) for the admitted peer, got {ev:?}"
  );
}

/// `handle_message` is the compound-aware ingress: it feeds an ALREADY-decoded
/// message straight to the coordinator, skipping the per-call `parse_message`
/// that `handle_packet` performs, and sieves the result identically.
#[test]
fn handle_message_admits_a_typed_alive_without_reparsing() {
  let mut e = ep(1, 7946);
  e.handle_message(sa(7000), alive(2, sa(7000)), Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "a typed Alive fed to handle_message must land in serf membership"
  );
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join),
    "handle_message must sieve the inner NodeJoined into serf"
  );
}

/// The single conceptual socket carries both quinn packets and plain-UDP gossip.
/// A datagram whose first byte is a memberlist tag is NOT handed to quinn: the
/// coordinator buffers it for the codec-owning driver to drain via
/// `poll_memberlist_ingress`, decode, and feed back through `handle_packet`.
#[test]
fn handle_udp_buffers_a_gossip_datagram_for_the_codec_owning_driver() {
  let mut e = ep(1, 7946);
  let frame = gossip_frame(&alive(2, sa(7000)));
  e.handle_udp(sa(7000), &frame, Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    None,
    "handle_udp must not decode the gossip frame itself — the codec layer owns that"
  );
  let (from, bytes) = e
    .poll_memberlist_ingress()
    .expect("the gossip datagram is buffered for the driver to drain");
  assert_eq!(from, sa(7000), "the ingress carries the sender address");
  assert_eq!(bytes, frame, "the buffered bytes are the datagram verbatim");
  assert!(
    e.poll_memberlist_ingress().is_none(),
    "the ingress queue drains once"
  );

  // The full round: decode-and-feed the drained bytes back through handle_packet.
  e.handle_packet(from, bytes, Instant::ORIGIN);
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "handle_udp -> poll_memberlist_ingress -> handle_packet admits the peer"
  );
}

/// `poll_timeout` folds the coordinator's deadline with serf's own.  Each of the
/// four fold cases is pinned:  an un-scheduled coordinator under an alive serf
/// yields serf's earliest periodic deadline (the reap interval); once the
/// coordinator's schedulers are armed its sooner probe/gossip deadline wins.
#[test]
fn poll_timeout_folds_the_coordinator_and_serf_deadlines() {
  // (None, Some): the coordinator's schedulers are unset until `start_scheduling`,
  // so only serf's periodic deadlines are in play. reap_interval (15s) is the
  // earliest of reap / reconnect (30s) / queue-check (30s).
  let mut e = ep(1, 7946);
  assert_eq!(
    e.poll_timeout(),
    Some(t_secs(15)),
    "an un-scheduled coordinator leaves serf's reap deadline as the wake"
  );

  // (Some, Some): the armed coordinator's probe/gossip deadline is sooner than
  // serf's 15s reap, so the fold must surrender to the coordinator.
  e.start_scheduling(Instant::ORIGIN);
  let inner = e
    .transport_mut()
    .poll_timeout()
    .expect("start_scheduling arms the coordinator's probe / gossip / push-pull timers");
  assert!(
    inner < t_secs(15),
    "the coordinator's first scheduled deadline is sooner than serf's reap"
  );
  assert_eq!(
    e.poll_timeout(),
    Some(inner),
    "the fold takes the minimum of the coordinator's and serf's deadlines"
  );
}

/// A machine that lost an id-conflict vote schedules no serf wakeups; with the
/// coordinator un-scheduled too, `poll_timeout` is `None` — the driver must not
/// spin on a dead machine.  Arming the coordinator then re-supplies the only
/// remaining deadline.
#[test]
fn poll_timeout_is_none_on_a_shutdown_machine_with_no_coordinator_schedule() {
  let mut e = ep(1, 7946);
  shut_down_via_lost_conflict(&mut e);

  assert_eq!(
    e.poll_timeout(),
    None,
    "a shut-down serf with an un-scheduled coordinator requests no wakeup"
  );

  // (Some, None): the coordinator's schedule is the only live deadline left.
  e.start_scheduling(Instant::ORIGIN);
  let inner = e.transport_mut().poll_timeout();
  assert!(inner.is_some(), "the coordinator schedules its own timers");
  assert_eq!(
    e.poll_timeout(),
    inner,
    "with serf shut down the coordinator's deadline is the fold"
  );
}

/// The gossip MTU and the max reliable-stream frame size are the coordinator's
/// configured limits, not constants: the driver sizes its recv buffer and its
/// observation budget from them, so the forwarders must report what was
/// configured.
#[test]
fn wire_size_forwarders_report_the_configured_limits() {
  let opts = inner_opts(1, 7946)
    .with_gossip_mtu(1234)
    .with_max_stream_frame_size(4096);
  let mut e = QuicEndpoint::<u32>::new(coord(opts, test_quic_options(), 7946), Options::new());
  let _ = e.poll_event();

  assert_eq!(e.gossip_mtu(), 1234, "the configured gossip MTU reads back");
  assert_eq!(
    e.max_stream_frame_size(),
    4096,
    "the configured max reliable-stream frame size reads back"
  );
}

/// The unreliable (gossip + probe) wire is chosen at construction and the driver
/// reads it back to route each outbound gossip transmit: a `Datagram`-mode
/// coordinator queues QUIC datagrams, a `Udp`-mode one uses the shared socket.
#[test]
fn unreliable_transport_reports_the_configured_wire() {
  let mut datagram = QuicEndpoint::<u32>::new(
    coord(
      inner_opts(1, 7946),
      quic_options_on(UnreliableTransport::Datagram),
      7946,
    ),
    Options::new(),
  );
  let _ = datagram.poll_event();
  assert_eq!(
    datagram.unreliable_transport(),
    UnreliableTransport::Datagram
  );

  let mut udp = QuicEndpoint::<u32>::new(
    coord(
      inner_opts(2, 7000),
      quic_options_on(UnreliableTransport::Udp),
      7000,
    ),
    Options::new(),
  );
  let _ = udp.poll_event();
  assert_eq!(udp.unreliable_transport(), UnreliableTransport::Udp);
}

/// A datagram offered for a peer with no established QUIC connection is
/// `NotReady`: the driver must fall back to the plain-UDP path.  Connection
/// liveness is never a membership signal, so this is a routing answer and not a
/// failure the machine records.
#[test]
fn queue_unreliable_datagram_is_not_ready_without_an_established_connection() {
  let mut e = ep(1, 7946);
  let status =
    e.queue_unreliable_datagram(sa(7000), Bytes::from_static(b"gossip"), Instant::ORIGIN);
  assert_eq!(
    status,
    DatagramSendStatus::NotReady,
    "with no pooled connection the driver must be told to fall back, not silently drop"
  );
  assert!(
    e.poll_event().is_none(),
    "a not-ready datagram is never a membership event"
  );
}

/// `flush_outbound_transmits` drains quinn's queued outbound at `now` WITHOUT
/// advancing any membership timer, so a packet leaves on the tick it was
/// queued.  A dial started with `start_push_pull` must therefore surface its
/// handshake datagram after a bare flush — no `handle_timeout` needed.
#[test]
fn flush_outbound_transmits_releases_the_dial_without_a_membership_tick() {
  let mut e = ep(1, 7946);
  let before = e.member_time();
  e.start_push_pull(sa(7000), PushPullKind::Join, Instant::ORIGIN);

  e.flush_outbound_transmits(Instant::ORIGIN);
  let (to, bytes) = e
    .poll_transmit()
    .expect("the dial's handshake datagram is flushed on the same instant");
  assert_eq!(
    to,
    sa(7000),
    "the handshake is addressed to the dialed peer"
  );
  assert!(
    !bytes.is_empty(),
    "the flushed datagram carries the Initial"
  );
  assert_eq!(
    e.member_time(),
    before,
    "a flush must not advance any membership clock"
  );
}

/// `start_join_push_pull(ignore_old = true)` records the returned exchange id as
/// a per-EXCHANGE ignore-join target, so the merge it produces suppresses replay
/// of the peer's pre-join user events.  A plain (non-`ignore_old`) join records
/// nothing, and a join that terminates without merging is cleared by the driver.
#[test]
fn ignore_old_join_records_its_exchange_and_the_driver_can_clear_it() {
  let mut e = ep(1, 7946);

  let plain = e.start_join_push_pull(sa(7000), false, Instant::ORIGIN);
  assert!(
    !e.test_has_ignore_join_stream(plain),
    "a join without ignore_old records no ignore-join target"
  );

  let ignoring = e.start_join_push_pull(sa(7001), true, Instant::ORIGIN);
  assert!(
    e.test_has_ignore_join_stream(ignoring),
    "an ignore_old join records its exchange id"
  );
  assert_ne!(
    plain, ignoring,
    "each dial gets its own exchange id, so the ignore set is per-exchange"
  );

  // A join that terminates without a merge is cleared by the driver; the call is
  // idempotent for an id the success-path merge already consumed.
  e.clear_ignore_join_stream(ignoring);
  assert!(
    !e.test_has_ignore_join_stream(ignoring),
    "clear_ignore_join_stream removes the terminated join's entry"
  );
  e.clear_ignore_join_stream(ignoring);
  assert!(
    !e.test_has_ignore_join_stream(ignoring),
    "clearing an already-absent id is a no-op"
  );
}

/// A remote push-pull body carrying one buffered user event `name` at `ltime`,
/// with the peer's event clock one past it.
fn push_pull_with_event(name: &str, ltime: u64) -> Bytes {
  use crate::typed::{PushPullMessage, UserEvent, UserEvents};

  let pp: PushPullMessage<u32> = PushPullMessage::new(
    1.into(),
    Vec::new(),
    Vec::new(),
    (ltime + 1).into(),
    vec![UserEvents {
      ltime: ltime.into(),
      events: vec![UserEvent {
        name: name.into(),
        payload: Bytes::from_static(b"x"),
      }],
    }],
    1.into(),
  );
  AnyMessage::<u32, SocketAddr>::PushPull(pp)
    .encode()
    .expect("encode push-pull body")
}

/// The ignore-join entry is consumed by the merge it was recorded for: the join
/// merge carrying the recorded exchange id raises the event floor to the peer's
/// event clock, so the peer's pre-join user events never replay — and the entry
/// is one-shot.
#[test]
fn ignore_old_join_merge_suppresses_the_peers_pre_join_user_events() {
  let mut e = ep(1, 7946);
  let sid = e.start_join_push_pull(sa(7000), true, Instant::ORIGIN);

  e.test_merge_remote_state_with_stream(push_pull_with_event("pre-join", 3), true, sid);

  assert!(
    e.poll_event().is_none(),
    "the ignore_old join's merge must not replay the peer's pre-join user events"
  );
  assert!(
    !e.test_has_ignore_join_stream(sid),
    "the merge consumes the ignore-join entry (one-shot)"
  );
  assert_eq!(
    e.test_event_min_time(),
    4,
    "the event floor is raised to the peer's event clock, fencing off its history"
  );
}

/// Without the ignore-join entry, the SAME join merge REPLAYS the peer's buffered
/// user events into the local event stream — the behaviour the `ignore_old` flag
/// exists to suppress.
#[test]
fn plain_join_merge_replays_the_peers_user_events() {
  let mut e = ep(1, 7946);
  let sid = e.start_join_push_pull(sa(7000), false, Instant::ORIGIN);

  e.test_merge_remote_state_with_stream(push_pull_with_event("replayed", 3), true, sid);

  let ev = e.poll_event().expect("the peer's user event replays");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "replayed"),
    "a merge with no ignore-join entry replays the peer's buffered user events, got {ev:?}"
  );
  assert_eq!(
    e.test_event_min_time(),
    0,
    "a plain join leaves the event floor at zero"
  );
}

/// The `suppress_pre_join_events` merge path drops the peer's buffered user
/// events without consulting any exchange id — the direct form, used when the
/// caller has already resolved that this join ignores old events.
#[test]
fn suppressed_merge_drops_the_peers_user_events() {
  let mut e = ep(1, 7946);
  e.test_merge_remote_state_suppressed(push_pull_with_event("suppressed", 3));
  assert!(
    e.poll_event().is_none(),
    "a suppressed merge must not replay the peer's user events"
  );
  assert_eq!(e.test_event_min_time(), 4, "the event floor is raised");
}

// ── serf commands over the QUIC coordinator ───────────────────────────────────

/// `join` announces the local join intent on the coordinator's gossip plane: it
/// stamps the member clock and enqueues the intent broadcast.  The driver owns
/// the seed dials separately (`start_join_push_pull`); `join` itself is the
/// announcement alone.
#[test]
fn join_announces_the_local_intent_on_the_broadcast_queue() {
  let mut e = ep(1, 7946);
  assert!(e.join().is_ok(), "join on an alive endpoint must succeed");
  assert!(
    e.user_broadcast_queue_len() > 0,
    "join must enqueue the local join intent for gossip"
  );
  assert_eq!(
    e.test_intent_ltime(1, IntentKind::Join),
    None,
    "the local node is already a member, so its own join is applied, not buffered"
  );
}

/// `leave` drives the lifecycle Alive -> Leaving, witnesses the member clock, and
/// hands the farewell to the coordinator, which arms the leave-complete deadline
/// once the inner machine reports it has left the cluster.
#[test]
fn leave_transitions_to_leaving_and_arms_the_complete_deadline_on_inner_left() {
  let mut e = ep(1, 7946);
  e.leave(Instant::ORIGIN).expect("leave from Alive succeeds");
  assert!(
    e.state().is_leaving(),
    "leave must set the state to Leaving"
  );
  assert!(
    e.member_time() >= 1,
    "leave stamps and witnesses the member clock"
  );
  assert_eq!(
    e.leave_complete_deadline(),
    None,
    "the deadline is armed by the inner LeftCluster, not by leave() itself"
  );

  e.test_inner_left_cluster();
  assert_eq!(
    e.leave_complete_deadline(),
    Some(Instant::ORIGIN + Options::new().leave_propagate_delay()),
    "the inner LeftCluster arms the leave-complete deadline at the propagate delay"
  );
}

/// A second `leave` on an already-Leaving machine is rejected with the typed
/// state error rather than restarting the chain.
#[test]
fn double_leave_is_rejected() {
  let mut e = ep(1, 7946);
  e.leave(Instant::ORIGIN).expect("first leave succeeds");
  assert!(
    matches!(
      e.leave(Instant::ORIGIN),
      Err(crate::endpoint::Error::BadLeaveState(_))
    ),
    "a second leave must be refused with BadLeaveState"
  );
}

/// `force_leave(prune = true)` forgets the member outright instead of leaving a
/// tombstone for the reaper: the state entry, the failed/left index lists and
/// the recent-intent entry all go, and a `Member(Reap)` event is emitted now.
#[test]
fn force_leave_with_prune_forgets_the_member_immediately() {
  let mut e = ep(1, 7946);
  // status_time 0 keeps the force-leave's stamped ltime ahead of the member's,
  // so the intent is fresh rather than stale.
  e.test_seed_failed_member_by_status(2, 0.into(), Instant::ORIGIN);
  assert!(
    e.test_in_failed_members(2),
    "the peer starts in failed_members"
  );

  e.force_leave(2, true, Instant::ORIGIN)
    .expect("force_leave on a known member succeeds");

  assert_eq!(
    e.test_member_status(2),
    None,
    "a pruned force-leave forgets the member outright"
  );
  assert!(
    !e.test_in_failed_members(2),
    "the pruned member is scrubbed from failed_members"
  );
  assert!(
    !e.test_in_left_members(2),
    "the pruned member is not left behind in left_members either"
  );
  let reaped = core::iter::from_fn(|| e.poll_event())
    .any(|ev| matches!(ev, Event::Member(ref me) if me.kind() == MemberEventKind::Reap));
  assert!(reaped, "the prune emits Member(Reap) immediately");
}

/// `force_leave(prune = false)` leaves an alive member as a `Leaving` tombstone
/// for the reaper — the contrast that proves the prune flag is load-bearing.
#[test]
fn force_leave_without_prune_leaves_a_tombstone() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Alive, 0.into());

  e.force_leave(2, false, Instant::ORIGIN)
    .expect("force_leave on a known member succeeds");

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "an unpruned force-leave tombstones the member as Leaving"
  );
}

/// `set_tags` is a two-sided write: the serf-side member's tags AND the
/// coordinator's advertised node meta must both carry the new value, or the
/// local view and the wire diverge.
#[test]
fn set_tags_updates_both_the_serf_member_and_the_advertised_meta() {
  let mut e = ep(1, 7946);
  let tags: Tags = [("role", "web"), ("dc", "eu")].into_iter().collect();

  e.set_tags(tags.clone(), Instant::ORIGIN)
    .expect("set_tags within the meta cap succeeds");

  assert_eq!(
    e.test_local_tags(),
    Some(tags),
    "the local serf member carries the new tags"
  );
  let meta = e
    .test_local_meta()
    .expect("the coordinator tracks the local node");
  assert!(
    !meta.as_bytes().is_empty(),
    "the coordinator re-advertises the encoded tags as node meta"
  );
}

/// Tags that do not fit the memberlist meta cap are refused, and the refusal is
/// pre-mutation: the previous tags survive.
#[test]
fn oversized_tags_are_refused_and_leave_the_previous_tags_intact() {
  let mut e = ep(1, 7946);
  let small: Tags = [("role", "web")].into_iter().collect();
  e.set_tags(small.clone(), Instant::ORIGIN)
    .expect("a small tag map fits the meta cap");

  let huge: Tags = (0..64)
    .map(|i| (format!("key{i}"), "x".repeat(64)))
    .collect::<Vec<_>>()
    .into_iter()
    .map(|(k, v)| (smol_str::SmolStr::from(k), smol_str::SmolStr::from(v)))
    .collect();
  assert!(
    matches!(
      e.set_tags(huge, Instant::ORIGIN),
      Err(crate::endpoint::Error::SetTagsMeta(_))
    ),
    "tags over the meta cap must be refused"
  );
  assert_eq!(
    e.test_local_tags(),
    Some(small),
    "a refused set_tags must not clobber the previous tags"
  );
}

/// A local `user_event` stamps the event clock, surfaces locally, and rides the
/// coordinator's event-tier broadcast queue.
#[test]
fn user_event_stamps_the_clock_emits_locally_and_queues_the_broadcast() {
  let mut e = ep(1, 7946);
  e.user_event("deploy", Bytes::from_static(b"v2"), false, Instant::ORIGIN)
    .expect("user_event on an alive endpoint");

  assert_eq!(
    e.event_time(),
    1,
    "the local event stamps ltime 0, clock -> 1"
  );
  assert!(
    e.user_broadcast_queue_len() > 0,
    "the user event is queued on the coordinator's broadcast plane"
  );
  match e.poll_event().expect("the local user event surfaces") {
    Event::User(u) => {
      assert_eq!(u.name.as_str(), "deploy");
      assert_eq!(u.payload.as_ref(), b"v2");
    }
    other => panic!("expected Event::User, got {other:?}"),
  }
}

/// A user event over the meta size cap is refused before it can reach the wire.
#[test]
fn oversized_user_event_is_rejected() {
  let mut e = ep(1, 7946);
  let big = Bytes::from(vec![0u8; 1024]);
  assert!(
    e.user_event("big", big, false, Instant::ORIGIN).is_err(),
    "a payload over max_user_event_size (512) must be refused"
  );
  assert_eq!(
    e.event_time(),
    0,
    "a refused user event must not advance the event clock"
  );
}

/// `resync_local_state` rebuilds the coordinator's push-pull snapshot from serf's
/// live clocks and clears the dirty flag, so the next anti-entropy exchange ships
/// current state.
#[test]
fn resync_local_state_rebuilds_the_push_pull_snapshot_and_clears_dirty() {
  let mut e = ep(1, 7946);
  e.test_set_clocks(11, 22, 33);
  e.resync_local_state();

  assert!(!e.test_is_dirty(), "resync clears the dirty flag");
  let snapshot = e.test_inner_local_state_snapshot();
  let pp = e.test_decode_pushpull(&snapshot);
  assert_eq!(
    u64::from(pp.ltime),
    11,
    "the snapshot carries the member clock"
  );
  assert_eq!(
    u64::from(pp.event_ltime),
    22,
    "the snapshot carries the event clock"
  );
  assert_eq!(
    u64::from(pp.query_ltime),
    33,
    "the snapshot carries the query clock"
  );
}

/// `load_snapshot` restores the three clocks from a replayed on-disk snapshot and
/// re-dials every alive peer it names, skipping the local node.
#[test]
fn load_snapshot_restores_the_clocks_and_rejoins_the_snapshot_peers() {
  let mut e = ep(1, 7946);
  let replay = crate::snapshot::ReplayResult {
    alive_nodes: vec![
      Node::new(1u32, sa(7946)), // self — must be skipped
      Node::new(2u32, sa(7000)),
    ],
    last_clock: 5.into(),
    last_event_clock: 7.into(),
    last_query_clock: 9.into(),
  };
  e.load_snapshot(replay, Instant::ORIGIN)
    .expect("load_snapshot on an alive machine succeeds");

  assert!(e.member_time() >= 5, "the member clock recovers last_clock");
  assert_eq!(
    e.test_event_min_time(),
    8,
    "the event floor is last_event_clock + 1"
  );
  assert_eq!(
    e.test_query_min_time(),
    10,
    "the query floor is last_query_clock + 1"
  );
  assert_eq!(
    e.test_rejoin_dials(),
    vec![sa(7000)],
    "exactly the non-self alive peers are re-dialed"
  );
}

// ── membership FSM driven through the QUIC coordinator ────────────────────────

/// A fresh leave intent for an Alive peer moves it to Leaving and is rebroadcast;
/// the member clock witnesses the intent's Lamport time.
#[test]
fn live_leave_intent_transitions_alive_to_leaving() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Alive, 5.into());
  assert!(
    e.test_handle_leave_intent(2, 8.into(), Instant::ORIGIN),
    "a fresh leave intent for an Alive member is rebroadcast"
  );
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Leaving));
  assert_eq!(
    e.test_member_status_time(2),
    Some(LamportTime::new(8)),
    "the member's status_time advances to the intent's ltime"
  );
  assert!(e.member_time() >= 9, "the member clock witnessed ltime 8");
}

/// A leave intent for a Failed peer completes its departure: Failed -> Left, moved
/// from the failed list to the left list, with a `Member(Leave)` event.
#[test]
fn leave_intent_for_a_failed_peer_transitions_to_left() {
  let mut e = ep(1, 7946);
  e.test_seed_failed_member_by_status(2, 5.into(), Instant::ORIGIN);
  assert!(e.test_handle_leave_intent(2, 9.into(), Instant::ORIGIN));

  assert_eq!(e.test_member_status(2), Some(MemberStatus::Left));
  assert!(!e.test_in_failed_members(2), "moved out of failed_members");
  assert!(e.test_in_left_members(2), "moved into left_members");
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave),
    "the completed departure emits Member(Leave)"
  );
}

/// A fresh leave intent for the LOCAL node while Alive is refuted, not applied:
/// the node re-announces its own join and suppresses the rebroadcast.
#[test]
fn fresh_self_leave_intent_is_refuted() {
  let mut e = ep(1, 7946);
  e.test_seed_member(1, MemberStatus::Alive, 0.into());
  assert!(
    !e.handle_node_leave_intent(5.into(), &1, false, Instant::ORIGIN),
    "a self-leave must be refuted, never rebroadcast"
  );
  assert_eq!(
    e.test_member_status(1),
    Some(MemberStatus::Alive),
    "the refuted self-leave leaves the local node Alive"
  );
  assert!(e.member_time() >= 6, "the member clock witnessed ltime 5");
}

/// A join intent for an unknown node is buffered as a recent intent rather than
/// creating a phantom member; the buffered ltime is what a later `NodeJoined`
/// reconciles against.
#[test]
fn join_intent_for_an_unknown_node_is_buffered() {
  let mut e = ep(1, 7946);
  assert!(
    e.handle_node_join_intent(7.into(), &3, Instant::ORIGIN),
    "the first join intent for an unknown node is buffered"
  );
  assert_eq!(
    e.test_member_status(3),
    None,
    "buffering an intent must not create a member"
  );
  assert_eq!(
    e.test_intent_ltime(3, IntentKind::Join),
    Some(LamportTime::new(7)),
    "the buffered intent carries the announced ltime"
  );
  assert_eq!(e.test_recent_intents_len(), 1);
}

/// A buffered leave intent that predates the peer's `NodeJoined` is applied at
/// join time: the member materialises as Leaving, not Alive.
#[test]
fn inner_node_joined_applies_a_pending_leave_intent() {
  let mut e = ep(1, 7946);
  e.test_handle_leave_intent(2, 7.into(), Instant::ORIGIN);
  e.test_inner_node_joined(2, Instant::ORIGIN);

  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Leaving),
    "the pending leave intent is reconciled at join time"
  );
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join),
    "the join event is emitted regardless of the reconciled status"
  );
}

/// The inner SWIM machine declaring a node dead means Failed (recoverable) for an
/// Alive member, but Left (final) for one that was already Leaving.
#[test]
fn inner_node_left_maps_alive_to_failed_and_leaving_to_left() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Alive, 5.into());
  e.test_seed_member(3, MemberStatus::Leaving, 5.into());

  e.test_inner_node_left(2, Instant::ORIGIN);
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Failed));
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Failed)
  );

  e.test_inner_node_left(3, Instant::ORIGIN);
  assert_eq!(e.test_member_status(3), Some(MemberStatus::Left));
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Leave)
  );
}

/// A `NodeUpdated` refreshes the member and emits `Member(Update)`, but must not
/// dirty the push-pull snapshot: tags and address are not in that body, so
/// dirtying would force a wasted resync on every tag change.
#[test]
fn inner_node_updated_emits_update_without_dirtying_local_state() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Alive, 3.into());
  e.resync_local_state();
  assert!(!e.test_is_dirty());

  e.test_inner_node_updated(2, Instant::ORIGIN);

  assert!(
    !e.test_is_dirty(),
    "a tag-only NodeUpdated must not mark the local state dirty"
  );
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Update),
    "the Update event is still emitted"
  );
}

/// The reaper forgets a failed member once `reconnect_timeout` has elapsed and a
/// left member once `tombstone_timeout` has, emitting `Member(Reap)` for each;
/// before the timeout the member is held.
#[test]
fn reaper_forgets_failed_and_left_members_past_their_timeouts() {
  let mut e = ep(1, 7946);
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);
  e.test_seed_left_member(3, 3.into());

  e.test_fire_reap(t_secs(3600));
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Failed),
    "one hour is well inside the 24h reconnect_timeout"
  );

  let past = t_secs(3600 * 25);
  e.test_fire_reap(past);
  assert_eq!(e.test_member_status(2), None, "the failed member is reaped");
  assert_eq!(e.test_member_status(3), None, "the left member is reaped");
  let reaps = core::iter::from_fn(|| e.poll_event())
    .filter(|ev| matches!(ev, Event::Member(me) if me.kind() == MemberEventKind::Reap))
    .count();
  assert_eq!(reaps, 2, "both reaped members surface a Member(Reap)");
}

/// The reconnector picks a failed peer and dials it through the coordinator; with
/// one failed and one alive member the probability gate always fires.
#[test]
fn reconnect_dials_a_failed_peer_through_the_coordinator() {
  let mut e = ep(1, 7946);
  e.test_seed_member(1, MemberStatus::Alive, 1.into());
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.test_fire_reconnect(Instant::ORIGIN);

  assert_eq!(
    e.test_last_dial_addr(),
    Some(sa(7000)),
    "the reconnect dial targets the failed peer's address"
  );
}

/// The whole periodic pass runs off `handle_timeout`: the coordinator's SWIM timer
/// fires between serf's pre-tick snapshot resync and serf's post-tick drain, and
/// serf's own reap deadline fires in that post-tick pass.
#[test]
fn handle_timeout_fires_the_serf_reap_deadline() {
  let mut e = ep(1, 7946);
  e.test_seed_failed_member(2, sa(7000), Instant::ORIGIN);

  e.handle_timeout(t_secs(3600 * 25 + 16));

  assert_eq!(
    e.test_member_status(2),
    None,
    "handle_timeout must fire serf's reap deadline, not just the coordinator's"
  );
}

/// An inner `NodeJoined` folded through the tick path materialises the member at
/// the tick's instant.
#[test]
fn inner_joined_injected_at_a_tick_creates_the_member() {
  let mut e = ep(1, 7946);
  e.test_inject_inner_joined(2, t_secs(5));
  assert_eq!(e.test_member_status(2), Some(MemberStatus::Alive));
  assert!(
    matches!(e.poll_event(), Some(Event::Member(ref me)) if me.kind() == MemberEventKind::Join)
  );
}

// ── user-event ring: dedup, floors, and the gossip packet path ────────────────

/// The event ring dedups by `(ltime, name, payload)`: the first sight of an event
/// is emitted and rebroadcast, an exact repeat is dropped silently.
#[test]
fn duplicate_user_event_is_deduped() {
  let mut e = ep(1, 7946);
  let msg = UserEventMessage {
    ltime: 4.into(),
    cc: false,
    name: "x".into(),
    payload: Bytes::from_static(b"p"),
  };

  assert!(
    e.handle_user_event(msg.clone()),
    "first sight rebroadcasts the event"
  );
  assert!(e.poll_event().is_some(), "first sight emits Event::User");
  assert!(
    !e.test_handle_user_event(msg),
    "an exact duplicate is dropped, not rebroadcast"
  );
  assert!(
    e.poll_event().is_none(),
    "a duplicate emits no second event"
  );
  assert_eq!(
    e.test_event_slot_len(4),
    1,
    "the ring slot holds exactly one copy of the deduped event"
  );
}

/// An event below the replay floor (`min_time`, set by a snapshot restore or an
/// `ignore_old` join) is dropped without emission.
#[test]
fn user_event_below_the_event_floor_is_dropped() {
  let mut e = ep(1, 7946);
  e.test_set_event_min_time(10);
  assert_eq!(e.test_event_min_time(), 10);

  let stale = UserEventMessage {
    ltime: 3.into(),
    cc: false,
    name: "old".into(),
    payload: Bytes::new(),
  };
  assert!(
    !e.test_handle_user_event(stale),
    "an event below the floor must not rebroadcast"
  );
  assert!(
    e.poll_event().is_none(),
    "no event surfaces below the floor"
  );
}

/// An event whose ltime has fallen out of the ring's window (clock has advanced
/// more than the buffer size past it) is dropped: the ring can no longer prove it
/// is not a duplicate.
#[test]
fn user_event_older_than_the_ring_window_is_dropped() {
  let mut e = ep(1, 7946);
  e.test_set_event_clock(600); // ring size 512; ltime 0 is 88 slots behind
  let stale = UserEventMessage {
    ltime: 0.into(),
    cc: false,
    name: "stale".into(),
    payload: Bytes::new(),
  };
  assert!(
    !e.test_handle_user_event(stale),
    "an event older than the ring window must be dropped"
  );
  assert!(e.poll_event().is_none());
}

/// A gossiped user event arriving on the coordinator's user-packet plane is
/// decoded, deduped, surfaced, and re-queued for onward gossip — the packet path
/// end to end.
#[test]
fn user_event_over_the_gossip_packet_path_surfaces_and_rebroadcasts() {
  let mut e = ep(1, 7946);
  let frame = AnyMessage::<u32, SocketAddr>::UserEvent(UserEventMessage {
    ltime: 1.into(),
    cc: false,
    name: "deploy".into(),
    payload: Bytes::from_static(b"v3"),
  })
  .encode()
  .expect("encode the gossiped user event");

  e.test_inject_user_packet(sa(7000), frame, Instant::ORIGIN);

  let ev = e.poll_event().expect("the gossiped user event surfaces");
  assert!(
    matches!(ev, Event::User(ref u) if u.name == "deploy" && u.payload.as_ref() == b"v3"),
    "expected the decoded Event::User, got {ev:?}"
  );
  assert!(
    e.user_broadcast_queue_len() > 0,
    "the event is re-queued on the coordinator's broadcast plane"
  );
  assert!(
    e.event_time() >= 2,
    "the inbound event witnesses the local event clock"
  );
}

/// Undecodable bytes on the gossip user-packet plane are dropped silently — the
/// machine must never panic on hostile input from the network.
#[test]
fn malformed_bytes_on_the_gossip_packet_path_are_dropped() {
  let mut e = ep(1, 7946);
  e.test_inject_user_packet(
    sa(7000),
    Bytes::from_static(b"\xff\xfe\xfd"),
    Instant::ORIGIN,
  );
  assert!(
    e.poll_event().is_none(),
    "a malformed frame yields no event"
  );
  assert_eq!(e.event_time(), 0, "and advances no clock");
}

/// A join intent arriving over the gossip packet path is buffered and requeued for
/// onward gossip on the intent tier.
#[test]
fn join_intent_over_the_gossip_packet_path_is_buffered_and_requeued() {
  let mut e = ep(1, 7946);
  let frame = AnyMessage::<u32, SocketAddr>::Join(crate::JoinMessage::new(8u64.into(), 2u32))
    .encode()
    .expect("encode the gossiped join intent");

  e.test_inject_user_packet(sa(7000), frame, Instant::ORIGIN);

  assert_eq!(
    e.test_intent_ltime(2, IntentKind::Join),
    Some(LamportTime::new(8)),
    "the gossiped join intent is buffered for the unknown node"
  );
  assert!(
    e.member_time() >= 9,
    "the gossiped intent witnesses the member clock"
  );
  assert!(
    e.user_broadcast_queue_len() > 0,
    "the intent is requeued for onward gossip"
  );
}

/// The broadcast enqueue helpers ride the coordinator's ranked user-broadcast
/// plane: intents go out at the membership rank, queries at the application rank.
#[test]
fn intent_and_query_broadcasts_reach_the_coordinator_queue() {
  let mut e = ep(1, 7946);
  assert_eq!(e.user_broadcast_queue_len(), 0, "the queue starts empty");

  e.test_enqueue_intent_broadcast(Bytes::from_static(b"intent"));
  assert_eq!(e.user_broadcast_queue_len(), 1);

  e.test_enqueue_query_broadcast(Bytes::from_static(b"query"));
  assert_eq!(
    e.user_broadcast_queue_len(),
    2,
    "both tiers land on the same coordinator queue"
  );
}

/// An installed `MessageDropper` drops the selected inbound message class before
/// the machine witnesses its clock — the fault-injection seam a driver test uses
/// to force an out-of-order delivery.
#[test]
fn message_dropper_drops_an_inbound_join_intent_before_it_witnesses_the_clock() {
  struct DropJoins;
  impl crate::MessageDropper for DropJoins {
    fn should_drop(&self, kind: crate::DropKind) -> bool {
      matches!(kind, crate::DropKind::Join)
    }
  }

  let frame = AnyMessage::<u32, SocketAddr>::Join(crate::JoinMessage::new(8u64.into(), 2u32))
    .encode()
    .expect("encode the gossiped join intent");

  let mut e = ep(1, 7946);
  e.core_mut().set_message_dropper(Arc::new(DropJoins));
  e.test_inject_user_packet(sa(7000), frame, Instant::ORIGIN);

  assert_eq!(
    e.test_intent_ltime(2, IntentKind::Join),
    None,
    "the dropped join intent must not be buffered"
  );
  assert_eq!(
    e.member_time(),
    0,
    "a dropped intent must not witness the member clock"
  );
}

// ── queries: issue, ingress, respond, relay ───────────────────────────────────

/// Build a minimal inbound `QueryMessage` from node 99.
fn test_query(ltime: LamportTime, id: u32) -> QueryMessage<u32, SocketAddr> {
  QueryMessage {
    ltime,
    id,
    from: Node::new(99u32, sa(9999)),
    filters: vec![],
    flags: QueryFlag::empty(),
    relay_factor: 0,
    timeout: Duration::from_secs(5),
    name: "ping".into(),
    payload: Bytes::from_static(b"payload"),
  }
}

/// A locally-issued query registers a pending entry, processes locally (surfacing
/// `Event::Query` with the issued name and payload), and rides the coordinator's
/// query broadcast tier.
#[test]
fn query_registers_a_pending_entry_and_surfaces_the_local_query_event() {
  let mut e = ep(1, 7946);
  let id = e
    .query(
      "status",
      Bytes::from_static(b"probe"),
      QueryParams::default(),
      Instant::ORIGIN,
    )
    .expect("query on an alive endpoint");

  assert_eq!(e.test_pending_query_count(), 1);
  assert_eq!(e.test_last_query_id(), Some(id));
  assert!(
    e.user_broadcast_queue_len() > 0,
    "the query rides the coordinator's broadcast plane"
  );

  match e
    .poll_event()
    .expect("the local node processes its own query")
  {
    Event::Query(q) => {
      assert_eq!(
        q.name(),
        "status",
        "the query event carries the issued name"
      );
      assert_eq!(
        q.payload().as_ref(),
        b"probe",
        "the query event carries the issued payload"
      );
      assert_eq!(q.ltime(), id.ltime);
    }
    other => panic!("expected Event::Query, got {other:?}"),
  }
}

/// An inbound query witnesses the query clock, registers an answerable token, and
/// surfaces as `Event::Query`; a repeat of the same `(ltime, id)` is deduped.
#[test]
fn inbound_query_witnesses_the_clock_and_dedups_by_id() {
  let mut e = ep(1, 7946);

  assert!(
    e.test_handle_query(test_query(3.into(), 7)),
    "a first-sight query is rebroadcast"
  );
  assert!(e.query_time() >= 4, "the query clock witnessed ltime 3");
  assert_eq!(e.test_received_queries_len(), 1, "the token is answerable");
  assert_eq!(e.test_query_slot_len(3), 1);
  assert!(matches!(e.poll_event(), Some(Event::Query(_))));

  assert!(
    !e.test_handle_query(test_query(3.into(), 7)),
    "the same (ltime, id) is deduped and not rebroadcast"
  );
  assert!(e.poll_event().is_none(), "a duplicate query emits no event");
}

/// An answerable token is removed once responded, and the second `respond` on the
/// same token is refused — a query is answered exactly once.
#[test]
fn respond_directed_sends_once_then_refuses() {
  let mut e = ep(1, 7946);
  let qid = QueryId {
    ltime: LamportTime::new(2),
    id: 9,
  };
  let token = e.test_register_received_query(qid, sa(1002), t_secs(10));
  assert!(!e.test_is_responded(qid), "the token starts unanswered");

  e.respond(&token, Bytes::from_static(b"ok"), Instant::ORIGIN)
    .expect("the first respond succeeds");

  let (dest, bytes) = e
    .test_last_directed_send()
    .expect("respond directed-sends the response to the querier");
  assert_eq!(dest, sa(1002), "the response goes to the querier's address");
  assert!(!bytes.is_empty(), "the response frame is non-empty");
  assert_eq!(
    e.test_received_queries_len(),
    0,
    "a successful respond consumes the token"
  );

  assert!(
    matches!(
      e.respond(&token, Bytes::new(), Instant::ORIGIN),
      Err(crate::endpoint::Error::AlreadyResponded)
    ),
    "a second respond on the same token is refused"
  );
}

/// A response past the query's deadline is refused: the querier has already closed
/// the query, so the answer would be wasted wire.
#[test]
fn respond_after_the_deadline_is_refused() {
  let mut e = ep(1, 7946);
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    sa(1002),
    t_secs(10),
  );
  assert!(
    e.respond(&token, Bytes::new(), t_secs(11)).is_err(),
    "a respond past the deadline must be refused"
  );
}

/// Expired answerable tokens are pruned by the periodic pass, and the pruned
/// deadlines are exactly the ones that elapsed.
#[test]
fn expired_received_queries_are_pruned_on_the_tick() {
  let mut e = ep(1, 7946);
  e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 1,
    },
    sa(1002),
    t_secs(5),
  );
  e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 2,
    },
    sa(1002),
    t_secs(100),
  );
  assert_eq!(e.test_received_queries_len(), 2);
  assert_eq!(e.test_peek_received_query_deadlines().len(), 2);

  e.handle_timeout(t_secs(6));

  assert_eq!(
    e.test_received_queries_len(),
    1,
    "only the elapsed token is pruned"
  );
  assert_eq!(
    e.test_peek_received_query_deadlines(),
    vec![t_secs(100)],
    "the surviving token keeps its deadline"
  );
}

/// A response to a query this node originated surfaces as `Event::QueryResponse`
/// carrying the responder and its payload; a second response from the same node is
/// deduped.
#[test]
fn query_response_surfaces_with_the_responder_and_payload() {
  let mut e = ep(1, 7946);
  let id = e
    .query(
      "ping",
      Bytes::new(),
      QueryParams::default(),
      Instant::ORIGIN,
    )
    .expect("query on an alive endpoint");
  let _ = e.poll_event(); // drain the local Event::Query

  let resp = QueryResponseMessage {
    ltime: id.ltime,
    id: id.id,
    from: Node::new(2u32, sa(1002)),
    flags: QueryFlag::empty(),
    payload: Bytes::from_static(b"pong"),
  };
  e.test_handle_query_response(resp.clone());

  match e.poll_event().expect("the response surfaces") {
    Event::QueryResponse(qr) => {
      assert_eq!(qr.id(), id.id, "the response carries the query id");
      assert_eq!(*qr.from().id_ref(), 2u32, "and the responder node");
      assert_eq!(qr.payload().as_ref(), b"pong", "and its payload");
    }
    other => panic!("expected Event::QueryResponse, got {other:?}"),
  }
  assert_eq!(
    e.test_pending_query_response_count(id),
    1,
    "the responder is tallied once"
  );

  e.test_handle_query_response(resp);
  assert!(
    e.poll_event().is_none(),
    "a second response from the same node is deduped"
  );
  assert_eq!(
    e.test_pending_query_response_count(id),
    1,
    "the duplicate does not inflate the tally"
  );
}

/// A query issued with `request_ack` sets the ACK flag on the wire, and a peer's
/// ack surfaces as `Event::QueryAck` naming the acking node.
#[test]
fn ack_requested_query_surfaces_the_peer_ack() {
  let mut e = ep(1, 7946);
  let params = QueryParams::<u32> {
    request_ack: true,
    ..Default::default()
  };
  let id = e
    .query("ping", Bytes::new(), params, Instant::ORIGIN)
    .expect("query on an alive endpoint");
  let _ = e.poll_event(); // drain the local Event::Query

  let ack = QueryResponseMessage {
    ltime: id.ltime,
    id: id.id,
    from: Node::new(2u32, sa(1002)),
    flags: QueryFlag::ACK,
    payload: Bytes::new(),
  };
  e.test_handle_query_response(ack.clone());

  match e.poll_event().expect("the ack surfaces") {
    Event::QueryAck(a) => {
      assert_eq!(a.id(), id.id, "the ack carries the query id");
      assert_eq!(*a.from().id_ref(), 2u32, "and the acking node");
    }
    other => panic!("expected Event::QueryAck, got {other:?}"),
  }

  e.test_handle_query_response(ack);
  assert!(
    e.poll_event().is_none(),
    "a duplicate ack from the same node is deduped"
  );
}

/// A responder-side relay picks alive peers and directed-sends the wrapped frame
/// to each; the local node is never a relay hop.
#[test]
fn relay_response_directed_sends_through_alive_peers() {
  let mut e = ep(1, 7946);
  e.test_seed_member_at(10u32, sa(1010), MemberStatus::Alive, 1.into());
  e.test_seed_member_at(11u32, sa(1011), MemberStatus::Alive, 1.into());

  e.test_relay_response(
    Node::new(2000u32, sa(2000)),
    Bytes::from_static(b"\x06relay-payload"),
    1,
  );

  let sends = e.test_relay_all_directed_sends();
  assert_eq!(sends.len(), 1, "relay_factor 1 picks exactly one hop");
  let (dest, bytes) = &sends[0];
  assert!(
    *dest == sa(1010) || *dest == sa(1011),
    "the relay hop must be one of the alive peers, got {dest}"
  );
  assert!(!bytes.is_empty(), "the relay-wrapped frame is non-empty");
  assert!(e.poll_event().is_none(), "a delivered relay drops nothing");
}

/// A relay with fewer members than hops requested is a silent no-op — not a
/// `RelayDropped`, which is reserved for a real delivery failure.
#[test]
fn relay_response_with_too_few_members_is_a_silent_noop() {
  let mut e = ep(1, 7946);
  e.test_relay_response(Node::new(2000u32, sa(2000)), Bytes::from_static(b"f"), 2);
  assert!(
    e.poll_event().is_none(),
    "too few members must not emit RelayDropped"
  );
  assert!(
    e.test_last_directed_send().is_none(),
    "and must not send anything"
  );
}

/// A relayed frame arriving at an intermediary is forwarded VERBATIM to the named
/// destination — the relay must not re-encode the inner response.
#[test]
fn handle_relay_forwards_the_inner_frame_verbatim() {
  let mut e = ep(1, 7946);
  let inner = Bytes::from_static(b"\x06inner-qresp");
  e.test_handle_relay(RelayMessage::new(Node::new(2u32, sa(1002)), inner.clone()));

  let (dest, sent) = e
    .test_last_directed_send()
    .expect("the relay forwards to the destination");
  assert_eq!(dest, sa(1002), "forwarded to the wrapped destination");
  assert_eq!(sent, inner, "the payload is forwarded verbatim");
  assert!(e.poll_event().is_none(), "a delivered relay emits no event");
}

/// A relay addressed at the local node cannot be forwarded; the machine surfaces
/// the undeliverable hop as `Event::RelayDropped` rather than discarding it.
#[test]
fn relay_to_self_surfaces_relay_dropped() {
  let mut e = ep(1, 7946);
  e.test_handle_relay(RelayMessage::new(
    Node::new(1u32, sa(7946)),
    Bytes::from_static(b"x"),
  ));

  match e.poll_event().expect("a self-relay must surface") {
    Event::RelayDropped(d) => assert_eq!(
      *d.destination(),
      sa(7946),
      "the dropped relay names the destination it could not reach"
    ),
    other => panic!("expected Event::RelayDropped, got {other:?}"),
  }
}

// ── id-conflict resolution ────────────────────────────────────────────────────

/// Drive `e` into the Shutdown state by losing an id-conflict vote (1 agreeing
/// response against 2 disagreeing), and drain the resulting `Event::Shutdown`.
fn shut_down_via_lost_conflict(e: &mut QuicEndpoint<u32>) {
  let deadline = t_secs(3600);
  let qid = e.test_register_conflict_query(deadline);
  e.test_fold_conflict_response(qid, 200u32, true);
  e.test_fold_conflict_response(qid, 201u32, false);
  e.test_fold_conflict_response(qid, 202u32, false);
  e.test_fire_due_query_closes(t_secs(3601));
  assert!(matches!(e.poll_event(), Some(Event::Shutdown)));
  assert!(e.state().is_shutdown());
}

/// A conflict vote the local node WINS (a majority of responders agree the id is
/// ours) leaves the machine Alive with its command surface open.
#[test]
fn winning_the_conflict_vote_leaves_the_machine_alive() {
  let mut e = ep(1, 7946);
  let qid = e.test_register_conflict_query(t_secs(3600));
  e.test_fold_conflict_response(qid, 100u32, true);
  e.test_fold_conflict_response(qid, 101u32, true);
  e.test_fold_conflict_response(qid, 102u32, false);
  assert_eq!(
    e.test_pending_query_conflict_matching(qid),
    Some(2),
    "two of the three responders agree the id is ours"
  );

  e.test_fire_due_query_closes(t_secs(3601));

  assert!(e.poll_event().is_none(), "a won vote emits no Shutdown");
  assert!(e.state().is_alive(), "and leaves the machine Alive");
  assert!(
    e.user_event("post-win", Bytes::new(), false, Instant::ORIGIN)
      .is_ok(),
    "a won vote does not gate the command surface"
  );
}

/// A conflict vote the local node LOSES forces the documented Alive -> Shutdown
/// transition, and the buffered `Event::Shutdown` still drains from the dead
/// machine.
#[test]
fn losing_the_conflict_vote_shuts_the_machine_down() {
  let mut e = ep(1, 7946);
  assert!(e.state().is_alive());
  shut_down_via_lost_conflict(&mut e);
  assert!(
    e.state().is_shutdown(),
    "the machine stays Shutdown after the event drains"
  );
}

/// Every command that originates cluster work is refused on a shut-down machine,
/// and the lifecycle commands keep their own typed state errors.
#[test]
fn shutdown_refuses_the_originating_commands() {
  use crate::endpoint::Error;

  let mut e = ep(1, 7946);
  // A live token registered BEFORE the shutdown, so respond's refusal is proven
  // to precede its token lookup.
  let token = e.test_register_received_query(
    QueryId {
      ltime: LamportTime::new(1),
      id: 5,
    },
    sa(1002),
    t_secs(3600),
  );

  shut_down_via_lost_conflict(&mut e);

  let now = Instant::ORIGIN;
  let tags: Tags = [("role", "web")].into_iter().collect();

  assert!(matches!(
    e.user_event("x", Bytes::new(), false, now),
    Err(Error::Shutdown)
  ));
  assert!(matches!(
    e.query("q", Bytes::new(), QueryParams::default(), now),
    Err(Error::Shutdown)
  ));
  assert!(matches!(e.set_tags(tags, now), Err(Error::Shutdown)));
  assert!(matches!(
    e.respond(&token, Bytes::new(), now),
    Err(Error::Shutdown)
  ));
  assert!(matches!(
    e.load_snapshot(
      crate::snapshot::ReplayResult {
        alive_nodes: vec![],
        last_clock: 1.into(),
        last_event_clock: 1.into(),
        last_query_clock: 1.into(),
      },
      now
    ),
    Err(Error::Shutdown)
  ));
  assert!(matches!(e.join(), Err(Error::BadJoinState(_))));
  assert!(matches!(e.leave(now), Err(Error::BadLeaveState(_))));
  assert!(matches!(
    e.force_leave(2, false, now),
    Err(Error::BadLeaveState(_))
  ));
}

/// Build a `_serf_conflict` query naming `conflict_id` — the internal query a
/// node broadcasts when the inner machine reports two peers claiming one id.
fn conflict_query(conflict_id: u32) -> QueryMessage<u32, SocketAddr> {
  use memberlist_proto::Data;

  let mut q = test_query(3.into(), 11);
  q.name = "_serf_conflict".into();
  q.payload = conflict_id
    .encode_to_bytes()
    .expect("encode the conflicting id");
  q
}

/// A `_serf_conflict` query naming a member this node knows is answered
/// autonomously: the machine sends its view of the conflicting node straight back
/// to the originator on the gossip plane, the answerable token is consumed (the
/// driver is never asked to respond), and the query never surfaces to the
/// application.
#[test]
fn conflict_query_is_answered_autonomously_and_never_surfaces() {
  let mut e = ep(1, 7946);
  e.test_seed_member_at(2u32, sa(1002), MemberStatus::Alive, 1.into());

  e.test_handle_query(conflict_query(2));

  let transmit = e
    .poll_memberlist_transmit()
    .expect("the conflict query is answered directly to the originator");
  match transmit {
    memberlist_proto::Transmit::Packet(p) => {
      assert_eq!(
        *p.to_ref(),
        sa(9999),
        "the conflict response goes back to the querier"
      );
      assert!(
        matches!(p.message_ref(), Message::UserData(b) if !b.is_empty()),
        "the conflict response rides the serf user-data plane"
      );
    }
    other => panic!("expected a packet transmit, got {other:?}"),
  }
  assert_eq!(
    e.test_received_queries_len(),
    0,
    "an autonomously-answered query leaves no token for the driver"
  );
  assert!(
    e.poll_event().is_none(),
    "an internal conflict query must never surface as Event::Query"
  );
  assert!(
    e.query_time() > 3,
    "the conflict query still witnesses the query clock"
  );
}

/// A `_serf_conflict` query naming a node this machine does not track is left
/// unanswered — the machine has no view to report.
#[test]
fn conflict_query_for_an_unknown_node_is_unanswered() {
  let mut e = ep(1, 7946);
  e.test_handle_query(conflict_query(77));

  assert!(
    e.poll_memberlist_transmit().is_none(),
    "an unknown conflicting id yields no response"
  );
  assert!(e.poll_event().is_none());
}

/// The originator of a conflict query does not vote in its own conflict: a
/// `_serf_conflict` naming the LOCAL id is processed but never answered.
#[test]
fn conflict_query_naming_the_local_node_is_not_answered() {
  let mut e = ep(1, 7946);
  e.test_handle_query(conflict_query(1));

  assert!(
    e.poll_memberlist_transmit().is_none(),
    "a node must not respond to a conflict query about itself"
  );
  assert!(e.poll_event().is_none());
}

/// A due query closes on its deadline while a query whose deadline has not yet
/// elapsed is left pending — the close pass must not sweep the whole list.
#[test]
fn only_due_queries_close_on_the_deadline_pass() {
  let mut e = ep(1, 7946);
  // The synthetic registration stamps the query id from the live query clock, so
  // advance it between registrations to get two distinct pending queries.
  e.test_set_clocks(0, 0, 1);
  let due = e.test_register_conflict_query(t_secs(10));
  e.test_set_clocks(0, 0, 2);
  let not_due = e.test_register_conflict_query(t_secs(1000));
  assert_ne!(due, not_due, "the two registrations are distinct queries");
  assert_eq!(e.test_pending_query_count(), 2);

  e.test_fire_due_query_closes(t_secs(11));

  assert_eq!(
    e.test_pending_query_count(),
    1,
    "only the elapsed query is closed"
  );
  assert_eq!(
    e.test_pending_query_conflict_matching(due),
    None,
    "the closed query is gone"
  );
  assert_eq!(
    e.test_pending_query_conflict_matching(not_due),
    Some(0),
    "the un-elapsed query is still pending"
  );
}

// ── key management ────────────────────────────────────────────────────────────

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
mod keys {
  use super::*;
  use crate::{
    KeyRequestMessage,
    event::{KeyRequestOperation, KeyResponseArgs},
  };
  use memberlist_proto::SecretKey;

  #[cfg(feature = "aes-gcm")]
  fn test_key() -> SecretKey {
    SecretKey::Aes128([7u8; 16])
  }
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  fn test_key() -> SecretKey {
    SecretKey::ChaCha20Poly1305([7u8; 32])
  }

  /// Each key-management command issues an internal query under its own reserved
  /// name and registers a pending entry whose `num_nodes` denominator is the
  /// member count captured at issue time.
  #[test]
  fn key_commands_issue_internal_queries() {
    let mut e = ep(1, 7946);
    e.test_seed_member(2, MemberStatus::Alive, 1.into());

    let install = e
      .install_key(test_key(), 0, Instant::ORIGIN)
      .expect("install_key issues a query");
    assert_eq!(e.test_pending_query_count(), 1);
    assert_eq!(
      e.test_last_pending_query_num_nodes(),
      Some(2),
      "the denominator is the member count at issue time"
    );
    assert!(
      e.user_broadcast_queue_len() > 0,
      "the key query rides the broadcast plane"
    );

    let use_id = e.use_key(test_key(), 0, Instant::ORIGIN).expect("use_key");
    let remove_id = e
      .remove_key(test_key(), 0, Instant::ORIGIN)
      .expect("remove_key");
    let list_id = e.list_keys(0, Instant::ORIGIN).expect("list_keys");

    assert_eq!(
      e.test_pending_query_count(),
      4,
      "each key command registers its own pending query"
    );
    let mut ids = vec![install.id, use_id.id, remove_id.id, list_id.id];
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4, "each key query gets a distinct id");
  }

  /// An inbound key query surfaces as `Event::KeyRequest` — never as
  /// `Event::Query` — carrying the operation and the key it names, and leaves an
  /// answerable token behind for the driver's `respond_key`.
  #[test]
  fn inbound_key_query_surfaces_as_a_key_request() {
    let mut e = ep(1, 7946);
    let payload =
      AnyMessage::<u32, SocketAddr>::KeyRequest(KeyRequestMessage::new(Some(test_key())))
        .encode()
        .expect("encode the key request");

    let mut q = test_query(1.into(), 55);
    q.name = "_serf_install_key".into();
    q.payload = payload;
    e.test_handle_query(q);

    assert_eq!(
      e.test_received_queries_len(),
      1,
      "the key query leaves an answerable token"
    );
    match e.poll_event().expect("the key query surfaces") {
      Event::KeyRequest(kr) => {
        assert_eq!(kr.op(), KeyRequestOperation::Install);
        assert_eq!(
          kr.key(),
          Some(&test_key()),
          "the install request carries the key to install"
        );
        assert_eq!(*kr.from().id_ref(), 99u32, "and names the originator");
        assert_eq!(
          kr.id(),
          55,
          "the request carries the query id respond_key routes the answer on"
        );
        assert_eq!(
          kr.deadline(),
          Instant::ORIGIN + Duration::from_secs(5),
          "and the deadline the answer must beat"
        );
      }
      other => panic!("expected Event::KeyRequest, got {other:?}"),
    }
    assert!(
      e.poll_event().is_none(),
      "a key query must never also surface as Event::Query"
    );
  }

  /// `list_keys` carries no key; the surfaced request reflects that, and the
  /// operation's own metadata agrees.
  #[test]
  fn list_keys_query_surfaces_without_a_key() {
    let mut e = ep(1, 7946);
    let payload = AnyMessage::<u32, SocketAddr>::KeyRequest(KeyRequestMessage::new(None))
      .encode()
      .expect("encode the list-keys request");

    let mut q = test_query(1.into(), 56);
    q.name = "_serf_list_keys".into();
    q.payload = payload;
    e.test_handle_query(q);

    match e.poll_event().expect("the list-keys query surfaces") {
      Event::KeyRequest(kr) => {
        assert_eq!(kr.op(), KeyRequestOperation::List);
        assert!(kr.key().is_none(), "a list request carries no key");
        assert!(
          !kr.op().has_key(),
          "the List operation declares that it carries no key"
        );
      }
      other => panic!("expected Event::KeyRequest, got {other:?}"),
    }
  }

  /// `respond_key` answers the surfaced request: the token is consumed and the
  /// key response is directed-sent back to the originator.
  #[test]
  fn respond_key_answers_the_originator_and_consumes_the_token() {
    let mut e = ep(1, 7946);
    let payload =
      AnyMessage::<u32, SocketAddr>::KeyRequest(KeyRequestMessage::new(Some(test_key())))
        .encode()
        .expect("encode the key request");
    let mut q = test_query(1.into(), 55);
    q.name = "_serf_install_key".into();
    q.payload = payload;
    e.test_handle_query(q);

    let req = match e.poll_event().expect("the key query surfaces") {
      Event::KeyRequest(kr) => kr,
      other => panic!("expected Event::KeyRequest, got {other:?}"),
    };

    e.respond_key(
      &req,
      KeyResponseArgs {
        result: true,
        message: smol_str::SmolStr::default(),
        keys: vec![test_key()],
        primary_key: Some(test_key()),
      },
      Instant::ORIGIN,
    )
    .expect("respond_key succeeds within the deadline");

    assert_eq!(
      e.test_received_queries_len(),
      0,
      "respond_key consumes the answerable token"
    );
    let (dest, bytes) = e
      .test_last_directed_send()
      .expect("respond_key directed-sends the key response");
    assert_eq!(dest, sa(9999), "the response goes back to the originator");
    assert!(!bytes.is_empty());
  }

  /// The responses to a key query are folded into a tally and reported once, at
  /// the query's close: the installed-key counts, the primary-key report, and each
  /// failing node's message all reach `Event::KeyResponse`.
  #[test]
  fn key_query_responses_fold_into_the_closing_tally() {
    use crate::KeyResponseMessage;

    let mut e = ep(1, 7946);
    e.test_seed_member(2, MemberStatus::Alive, 1.into());
    e.test_seed_member(3, MemberStatus::Alive, 1.into());

    let qid = e.test_register_key_query(t_secs(10));

    // Node 2 succeeded and reports the key ring; node 3 failed with a message.
    let ok = AnyMessage::<u32, SocketAddr>::KeyResponse(KeyResponseMessage {
      result: true,
      message: smol_str::SmolStr::default(),
      keys: vec![test_key()],
      primary_key: Some(test_key()),
    })
    .encode()
    .expect("encode the successful key response");
    let failed = AnyMessage::<u32, SocketAddr>::KeyResponse(KeyResponseMessage {
      result: false,
      message: "no such key".into(),
      keys: vec![],
      primary_key: None,
    })
    .encode()
    .expect("encode the failing key response");

    e.test_handle_query_response(QueryResponseMessage {
      ltime: qid.ltime,
      id: qid.id,
      from: Node::new(2u32, sa(1002)),
      flags: QueryFlag::empty(),
      payload: ok,
    });
    e.test_handle_query_response(QueryResponseMessage {
      ltime: qid.ltime,
      id: qid.id,
      from: Node::new(3u32, sa(1003)),
      flags: QueryFlag::empty(),
      payload: failed,
    });

    e.test_fire_due_query_closes(t_secs(11));

    match e.poll_event().expect("the closing key query reports") {
      Event::KeyResponse(kr) => {
        assert_eq!(kr.num_resp, 2, "both responders are counted");
        assert_eq!(kr.num_err, 1, "one responder reported a failure");
        assert_eq!(
          kr.keys.get(&test_key()).copied(),
          Some(1),
          "one node reported holding the key"
        );
        assert_eq!(
          kr.primary_keys.get(&test_key()).copied(),
          Some(1),
          "one node reported the key as primary"
        );
        assert_eq!(
          kr.messages.get(&3u32).map(|m| m.as_str()),
          Some("no such key"),
          "the failing node's message is carried under its id"
        );
      }
      other => panic!("expected Event::KeyResponse, got {other:?}"),
    }
  }

  /// A key operation issued with a relay factor rides that factor on the wire, so
  /// responders relay their answers back through intermediaries.
  #[test]
  fn key_op_relay_factor_rides_the_issued_query() {
    let mut e = ep(1, 7946);
    e.install_key(test_key(), 3, Instant::ORIGIN)
      .expect("install_key issues a query");

    match e
      .poll_event()
      .expect("the local node processes its own key query")
    {
      Event::KeyRequest(kr) => assert_eq!(
        kr.relay_factor(),
        3,
        "the issued relay factor rides the key query"
      ),
      other => panic!("expected Event::KeyRequest, got {other:?}"),
    }
  }

  /// The key-request operation names are stable wire vocabulary; each maps to its
  /// own string and only `List` carries no key.
  #[test]
  fn key_request_operations_map_to_their_wire_names() {
    assert_eq!(KeyRequestOperation::Install.as_str(), "install");
    assert_eq!(KeyRequestOperation::Use.as_str(), "use");
    assert_eq!(KeyRequestOperation::Remove.as_str(), "remove");
    assert_eq!(KeyRequestOperation::List.as_str(), "list");

    assert!(KeyRequestOperation::Install.has_key());
    assert!(KeyRequestOperation::Use.has_key());
    assert!(KeyRequestOperation::Remove.has_key());
    assert!(!KeyRequestOperation::List.has_key());
  }
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

  /// With no keyring configured the gossip transforms are the identity: a datagram
  /// goes out as plaintext and comes back unchanged.
  #[test]
  fn gossip_transforms_are_the_identity_without_a_keyring() {
    let e = ep(1, 7946);
    assert!(
      !e.encryption_options().is_enabled(),
      "a fresh coordinator has no keyring"
    );

    let plain = b"\x05gossip-frame";
    let out = e.encrypt_gossip(plain).expect("encrypt without a keyring");
    assert_eq!(
      out.as_slice(),
      plain,
      "with no keyring the datagram is left unencrypted"
    );
    assert_eq!(
      e.decrypt_gossip(&out).expect("decrypt without a keyring"),
      plain,
      "and the inbound transform is the identity too"
    );
  }

  /// Installing a keyring re-keys the live gossip plane: the reported key state and
  /// the bytes on the wire move together, and an encrypted datagram round-trips
  /// through the same keyring.
  #[test]
  fn setting_the_encryption_options_re_keys_the_live_gossip_plane() {
    let mut e = ep(1, 7946);
    e.set_encryption_options(EncryptionOptions::new().with_keyring(Keyring::new(key(1))));

    assert!(
      e.encryption_options().is_enabled(),
      "the coordinator reports the installed keyring"
    );
    assert_eq!(
      e.encryption_options().keyring().map(|kr| *kr.primary_ref()),
      Some(key(1)),
      "and reports the primary key that outbound datagrams seal under"
    );

    let plain = b"\x05gossip-frame";
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
  }

  /// A datagram sealed under one keyring is NOT readable by a node holding a
  /// different key: the driver must drop the frame rather than admit it.
  #[test]
  fn a_datagram_sealed_under_a_foreign_key_fails_to_decrypt() {
    let mut sender = ep(1, 7946);
    sender.set_encryption_options(EncryptionOptions::new().with_keyring(Keyring::new(key(1))));
    let sealed = sender
      .encrypt_gossip(b"\x05gossip-frame")
      .expect("encrypt under the sender's keyring");

    let mut receiver = ep(2, 7000);
    receiver.set_encryption_options(EncryptionOptions::new().with_keyring(Keyring::new(key(2))));

    assert!(
      receiver.decrypt_gossip(&sealed).is_err(),
      "a frame the keyring cannot open must be an error, never silently admitted"
    );
  }
}

// ── coordinates ───────────────────────────────────────────────────────────────

/// A completed probe round-trip feeds the RTT and the peer's piggybacked
/// coordinate into the local Vivaldi model and caches the remote coordinate.
#[cfg(feature = "coordinates")]
#[test]
fn ping_completed_updates_the_local_model_and_caches_the_remote_coordinate() {
  use crate::bridge::coordinate_to_pb;
  use buffa::Message as _;

  let mut e = ep(1, 7946);
  let peer = crate::typed::Coordinate {
    vec: vec![5.0; 8],
    error: 1.0,
    adjustment: 0.0,
    height: 0.0,
  };
  let encoded = coordinate_to_pb(&peer).encode_to_vec();
  let mut payload = Vec::with_capacity(1 + encoded.len());
  payload.push(1u8); // PING_VERSION
  payload.extend_from_slice(&encoded);

  e.test_ping_completed(2u32, Duration::from_millis(40), Bytes::from(payload));

  assert!(
    e.cached_coordinate(&2u32).is_some(),
    "the peer's coordinate is cached under its id"
  );
  assert!(
    e.get_coordinate().is_some(),
    "the local Vivaldi model is updated by the round-trip"
  );
}

/// A probe payload with the wrong version byte, or an empty one, is dropped: the
/// coordinate plane never trusts an unrecognised wire format.
#[cfg(feature = "coordinates")]
#[test]
fn ping_completed_with_an_unusable_payload_is_a_noop() {
  let mut e = ep(1, 7946);
  e.test_ping_completed(
    2u32,
    Duration::from_millis(10),
    Bytes::from_static(b"\x02junk"),
  );
  assert!(
    e.cached_coordinate(&2u32).is_none(),
    "a bad version byte must not update the cache"
  );

  e.test_ping_completed(3u32, Duration::from_millis(10), Bytes::new());
  assert!(
    e.cached_coordinate(&3u32).is_none(),
    "an empty payload must not update the cache"
  );
}

/// Coordinates disabled at construction means no Vivaldi client at all: the local
/// coordinate, the cache and the reset counter are all absent, and a completed
/// probe round-trip changes nothing.
#[cfg(feature = "coordinates")]
#[test]
fn coordinates_disabled_at_construction_leaves_no_vivaldi_client() {
  let mut e = ep_with_options(1, 7946, Options::new().with_disable_coordinates(true));

  assert_eq!(
    e.get_coordinate(),
    None,
    "a coordinates-disabled machine reports no local coordinate"
  );
  assert_eq!(
    e.coordinate_resets(),
    None,
    "and no reset counter to read through to"
  );

  e.test_ping_completed(
    2u32,
    Duration::from_millis(40),
    Bytes::from_static(b"\x01x"),
  );
  assert_eq!(
    e.cached_coordinate(&2u32),
    None,
    "a probe round-trip caches nothing when coordinates are disabled"
  );
  assert!(e.poll_event().is_none(), "and emits no event");
}

/// A reaped member's coordinate is purged from the cache — a forgotten node must
/// not keep influencing the coordinate plane.
#[cfg(feature = "coordinates")]
#[test]
fn reaping_a_member_forgets_its_coordinate() {
  use crate::bridge::coordinate_to_pb;
  use buffa::Message as _;

  let mut e = ep(1, 7946);
  let peer = crate::typed::Coordinate {
    vec: vec![1.0; 8],
    error: 0.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let encoded = coordinate_to_pb(&peer).encode_to_vec();
  let mut payload = Vec::with_capacity(1 + encoded.len());
  payload.push(1u8);
  payload.extend_from_slice(&encoded);
  e.test_ping_completed(42u32, Duration::from_millis(20), Bytes::from(payload));
  assert!(e.cached_coordinate(&42u32).is_some());

  e.test_seed_failed_member(42u32, sa(7947), Instant::ORIGIN);
  e.test_fire_reap(t_secs(90_000));

  assert!(
    e.cached_coordinate(&42u32).is_none(),
    "the reaped member's coordinate must be purged from the cache"
  );
}

// ── coalescing ────────────────────────────────────────────────────────────────

/// With member coalescing enabled the membership changes are buffered in the
/// window rather than delivered one by one: `pending_events_len` reports the
/// buffered depth, and the batch flushes as ONE `Member(Join)` at the quiescent
/// deadline.
#[test]
fn coalesced_member_changes_batch_into_one_event_at_the_flush_deadline() {
  let mut e = ep_with_options(
    1,
    7946,
    Options::new()
      .with_coalesce_period(Duration::from_secs(10))
      .with_quiescent_period(Duration::from_secs(2)),
  );
  e.handle_timeout(t_secs(100));
  while e.poll_event().is_some() {}

  e.test_inject_inner_joined(2, t_secs(100));
  e.test_inject_inner_joined(3, t_secs(100));
  assert_eq!(
    e.pending_events_len(),
    0,
    "coalesced joins are held in the window, not queued for delivery"
  );
  assert!(
    e.poll_event().is_none(),
    "nothing is delivered before the flush deadline"
  );

  e.handle_timeout(t_secs(102));
  match e.poll_event().expect("the window flushes at its deadline") {
    Event::Member(me) => {
      assert_eq!(me.kind(), MemberEventKind::Join);
      let mut ids: Vec<u32> = me.members().iter().map(|m| *m.node().id_ref()).collect();
      ids.sort_unstable();
      assert_eq!(ids, vec![2, 3], "both joins flush as one batched event");
    }
    other => panic!("expected the coalesced Member(Join) batch, got {other:?}"),
  }
  assert!(
    e.poll_event().is_none(),
    "the batch is delivered exactly once"
  );
}

/// A join intent whose Lamport time is newer than the member's recorded status
/// time clears a `Leaving` member back to Alive; a stale one is ignored.
#[test]
fn join_intent_clears_a_leaving_member_and_a_stale_one_is_ignored() {
  let mut e = ep(1, 7946);
  e.test_seed_member(2, MemberStatus::Leaving, 5.into());

  assert!(
    e.test_handle_join_intent(2, 8.into(), Instant::ORIGIN),
    "a fresh join intent for a Leaving member is rebroadcast"
  );
  assert_eq!(
    e.test_member_status(2),
    Some(MemberStatus::Alive),
    "the fresh join intent clears the member back to Alive"
  );

  assert!(
    !e.test_handle_join_intent(2, 3.into(), Instant::ORIGIN),
    "a stale join intent is not rebroadcast"
  );
  assert_eq!(
    e.test_member_status_time(2),
    Some(LamportTime::new(8)),
    "and does not regress the member's status time"
  );
}

/// A tag filter scopes a query to the members whose tag value matches: a node
/// whose tags do not satisfy the filter must not surface the query locally.
#[cfg(feature = "tag-regex")]
#[test]
fn tag_filtered_query_is_not_surfaced_by_a_non_matching_node() {
  use crate::typed::{Filter, TagFilter};

  let mut e = ep(1, 7946);
  let tags: Tags = [("role", "db")].into_iter().collect();
  e.test_seed_member_with_tags(1, tags, MemberStatus::Alive, 1.into());

  let mut q = test_query(3.into(), 21);
  q.filters = vec![Filter::Tag(TagFilter {
    tag: "role".into(),
    expr: Some("web".into()),
  })];

  assert!(
    e.test_handle_query(q),
    "a filtered-out query is still rebroadcast so matching peers see it"
  );
  assert!(
    e.poll_event().is_none(),
    "a node whose tags do not match the filter must not surface the query"
  );
}

/// The same tag filter DOES surface on a node whose tag value matches — the
/// contrast that proves the filter is evaluated, not merely dropped.
#[cfg(feature = "tag-regex")]
#[test]
fn tag_filtered_query_is_surfaced_by_a_matching_node() {
  use crate::typed::{Filter, TagFilter};

  let mut e = ep(1, 7946);
  let tags: Tags = [("role", "web")].into_iter().collect();
  e.test_seed_member_with_tags(1, tags, MemberStatus::Alive, 1.into());

  let mut q = test_query(3.into(), 22);
  q.filters = vec![Filter::Tag(TagFilter {
    tag: "role".into(),
    expr: Some("web".into()),
  })];

  assert!(e.test_handle_query(q));
  assert!(
    matches!(e.poll_event(), Some(Event::Query(_))),
    "a node whose tag matches the filter surfaces the query"
  );
}

/// The ignore-join set is a plain per-exchange set: an id can be recorded and
/// cleared directly, independent of a live dial.
#[test]
fn the_ignore_join_set_records_and_clears_an_exchange_id() {
  let mut e = ep(1, 7946);
  let id = e.start_push_pull(sa(7000), PushPullKind::Join, Instant::ORIGIN);

  assert!(!e.test_has_ignore_join_stream(id));
  e.test_note_ignore_join_stream(id);
  assert!(
    e.test_has_ignore_join_stream(id),
    "the exchange id is recorded"
  );
  e.test_clear_ignore_join_stream(id);
  assert!(!e.test_has_ignore_join_stream(id), "and cleared again");
}
