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
  EndpointOptions, Instant, PushPullKind, QuicOptions, SeedableRng, SmallRng, UnreliableTransport,
};
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::{QuicEndpoint, members::MemberStatus, options::Options};

fn sa(port: u16) -> SocketAddr {
  format!("127.0.0.1:{port}").parse().unwrap()
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
/// localhost handshake) and datagram-mode unreliable transport.
fn test_quic_options() -> QuicOptions {
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
    UnreliableTransport::Datagram,
  )
}

/// Build a serf `QuicEndpoint<u32>` rooted at `id` / `port`, seeded
/// deterministically.  The quinn rng seed is derived from the port so the two
/// loopback endpoints draw distinct connection IDs.
fn ep(id: u32, port: u16) -> QuicEndpoint<u32> {
  let inner_opts = EndpointOptions::new(id, sa(port))
    .with_user_broadcast_tiers(core::num::NonZeroU8::new(3).unwrap());
  let inner =
    memberlist_proto::Endpoint::new_at(inner_opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  let mut seed = [0u8; 32];
  seed[..2].copy_from_slice(&port.to_le_bytes());
  let coord = memberlist_proto::QuicEndpoint::<u32>::with_quinn_rng_seed(
    inner,
    test_quic_options(),
    Some(seed),
  );
  let mut e = QuicEndpoint::new(coord, Options::new());
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
