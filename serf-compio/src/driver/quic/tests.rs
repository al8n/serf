//! Unit tests for the QUIC driver's recv-buffer sizing and the past-due drain's
//! pre-timeout ingress drain.

use super::*;

use core::num::NonZeroU8;
use std::sync::Arc;

use rand::rngs::StdRng;
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serf_proto::options::Options as SerfOptions;
use smol_str::SmolStr;

use crate::QuicOptions;
use memberlist_proto::UnreliableTransport;

/// A self-signed `localhost` cert + key for the test quinn `ServerConfig`.
fn self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen");
  let cert = CertificateDer::from(ck.cert.der().to_vec());
  let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
  (vec![cert], key)
}

/// Accept-any server-cert verifier — test only; the endpoint built here never
/// completes a handshake (the test feeds only a gossip-class datagram).
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
    _m: &[u8],
    _c: &CertificateDer,
    _d: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }
  fn verify_tls13_signature(
    &self,
    _m: &[u8],
    _c: &CertificateDer,
    _d: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }
  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    rustls::crypto::ring::default_provider()
      .signature_verification_algorithms
      .supported_schemes()
  }
}

/// A minimal datagram-mode [`QuicOptions`] bundle, mirroring the transport-level
/// smoke-test config: a real (if unused) cert/verifier pair so the quinn
/// endpoint builds.
fn test_quic_options() -> QuicOptions {
  let endpoint_cfg = quinn_proto::EndpointConfig::new(Arc::new(ring::hmac::Key::new(
    ring::hmac::HMAC_SHA256,
    &[0x5au8; 32],
  )));

  let (chain, key) = self_signed();
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let rustls_server = rustls::ServerConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3")
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .expect("valid self-signed cert");
  let server = quinn_proto::ServerConfig::with_crypto(Arc::new(
    quinn_proto::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server)).expect("qsc"),
  ));

  let rustls_client = rustls::ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3")
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyServer))
    .with_no_client_auth();
  let client = quinn_proto::ClientConfig::new(Arc::new(
    quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client)).expect("qcc"),
  ));

  QuicOptions::new(
    endpoint_cfg,
    server,
    client,
    quinn_proto::TransportConfig::default(),
    "localhost",
    UnreliableTransport::Datagram,
  )
}

/// Build a standalone serf `QuicEndpoint` over a memberlist QUIC coordinator —
/// no bound socket, no driver loop; just the composed machine, for driving the
/// ingress surface directly.
fn build_endpoint() -> QuicEndpoint<SmolStr, StdRng, StdRng> {
  let local: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let inner_opts = memberlist_proto::EndpointOptions::new(SmolStr::new("node"), local)
    .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
  let inner = memberlist_proto::Endpoint::new(inner_opts, StdRng::seed_from_u64(1));
  let coord = memberlist_proto::QuicEndpoint::new(inner, test_quic_options());
  QuicEndpoint::new_with_rng(coord, SerfOptions::new(), StdRng::seed_from_u64(2))
}

/// The QUIC past-due drain must DECODE every drained gossip datagram before
/// `handle_timeout` fires, or a buffered Ack would still be undecoded when the
/// suspicion sweep runs and the live peer it acked would be falsely suspected.
///
/// On QUIC, `handle_udp` only BUFFERS the raw gossip frame (unlike the stream
/// pump, which decodes inline in `dispatch_gossip`); the decode happens in
/// `drain_ingress` (`poll_memberlist_ingress` → decode → `handle_message`). The
/// fix's per-datagram step calls `drain_ingress` for each drained frame BEFORE
/// re-polling the deadline and firing `handle_timeout`. Two frames are buffered
/// here — a non-Ack preceding a (would-be) Ack — to prove `drain_ingress`
/// consumes BOTH, not just the first; the socket-level bound that reads both
/// datagrams off the queue is proven in the driver's `shared::tests`. A full
/// timing-driven false-suspicion exercise is not deterministic as a unit test,
/// so this asserts the fallback the past-due path guarantees: the pre-timeout
/// drain leaves nothing undecoded for `handle_timeout` to race.
#[test]
fn past_due_drain_decodes_buffered_gossip_before_timeout() {
  let mut endpoint = build_endpoint();
  let peer: SocketAddr = "127.0.0.1:65000".parse().expect("peer addr");
  let now = Instant::now();

  // Two `handle_udp` calls, as two drained datagrams would: a first byte of 1
  // (the `Compound` message tag) is demuxed to the gossip plane and BUFFERED raw
  // — not processed as a quinn packet, and not decoded inline. The non-Ack
  // precedes the would-be Ack in the buffer.
  endpoint.handle_udp(peer, &[1u8, 0, 0, 0], now);
  endpoint.handle_udp(peer, &[1u8, 0, 0, 0], now);

  // The past-due path's per-datagram drain pops the buffered frames and feeds
  // each back through `handle_message`: a drained Ack resolves its probe here,
  // ahead of the suspicion sweep. Asserting `true` proves the frames were
  // buffered (not decoded inline) and that the drain consumed them.
  assert!(
    drain_ingress::<SmolStr, StdRng, StdRng>(&mut endpoint, &None),
    "the past-due pre-timeout ingress drain must consume the buffered gossip frames so no \
     buffered Ack is left undecoded when handle_timeout fires"
  );

  // BOTH frames are consumed — nothing remains buffered for a subsequent
  // `handle_timeout` to race.
  assert!(
    endpoint.poll_memberlist_ingress().is_none(),
    "drain_ingress must leave the memberlist ingress queue empty before handle_timeout"
  );
}

/// The recv buffer must hold the larger of the two planes that share the QUIC
/// socket. When a caller's quinn `EndpointConfig` accepts a max UDP payload above
/// the gossip MTU — quinn's own default 1472 already exceeds the 1400 default
/// `gossip_mtu` — the buffer is sized from the QUIC plane, not silently left at
/// the gossip size that would truncate a full-size QUIC packet.
#[test]
fn recv_buf_sizes_to_the_larger_plane() {
  // QUIC plane far above the gossip plane (jumbo `max_udp_payload_size`): the
  // buffer follows the QUIC plane regardless of the small AEAD overhead.
  assert_eq!(recv_buf_len_for(1400, 9000), 9000);

  // quinn's default max UDP payload (1472) exceeds the default gossip MTU (1400),
  // so the buffer is sized to 1472, not 1400 — a full-size QUIC packet is not
  // truncated. Holds with or without an AEAD backend since
  // 1472 > 1400 + ENCRYPTED_WRAPPER_OVERHEAD.
  assert_eq!(recv_buf_len_for(1400, 1472), 1472);
}

/// When the gossip plane is the larger of the two, the QUIC ceiling never shrinks
/// it below the AEAD-inflated gossip requirement.
#[test]
fn recv_buf_keeps_the_gossip_requirement() {
  // Large configured gossip MTU, default-ish QUIC payload: the gossip plane wins
  // and keeps its encrypted-wrapper headroom.
  assert_eq!(
    recv_buf_len_for(16_000, 1472),
    16_000 + ENCRYPTED_WRAPPER_OVERHEAD
  );

  // The gossip plane is capped at the IPv4 UDP maximum; a small QUIC payload
  // cannot shrink it below that cap.
  assert_eq!(recv_buf_len_for(70_000, 1472), GOSSIP_RECV_BUF_MAX);
}

/// quinn permits a `max_udp_payload_size` up to 65527, just above the gossip
/// plane's 65507 cap; the QUIC plane is NOT clamped to that cap, so such a packet
/// is buffered in full.
#[test]
fn recv_buf_does_not_clamp_quic_below_quinn_max() {
  assert_eq!(recv_buf_len_for(70_000, 65_527), 65_527);
}

/// The drain-first timeout chokepoint must read the shared UDP socket BEFORE
/// deciding on `handle_timeout`: on a completion backend a freshly-submitted recv
/// is pending on first poll, so the main select's timer arm winning is NOT proof
/// of a would-block — a near-deadline Ack can be queued. `fire_quic_timeout` is
/// the single `handle_timeout` site, reached from both the past-due branch and
/// the main timer arm; this proves it actually invokes the drain on the real
/// socket. The socket-level proof that the drain reads EVERY queued datagram is
/// in the driver's `shared::tests`.
#[compio::test]
async fn fire_quic_timeout_drains_socket_before_handle_timeout() {
  let mut endpoint = build_endpoint();
  let any: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let driver = UdpSocket::bind(any).await.expect("bind driver socket");
  let driver_addr = driver.local_addr().expect("driver local_addr");
  let peer = UdpSocket::bind(any).await.expect("bind peer socket");

  // A single gossip compound-tagged datagram queued in the driver's socket — the
  // near-deadline "Ack" the chokepoint must consume before any suspicion.
  peer
    .send_to(vec![1u8, 0, 0, 0], driver_addr)
    .await
    .0
    .expect("queue a datagram");
  // Let loopback delivery settle so the drain's eager recv reads the datagram on
  // its first poll (production's past-due Ack is already buffered).
  compio::time::sleep(Duration::from_millis(100)).await;

  let opts = RuntimeOptions::new();
  let dirty =
    fire_quic_timeout::<SmolStr, StdRng, StdRng>(&mut endpoint, &driver, 64, opts, &None).await;
  assert!(
    dirty,
    "the chokepoint consumed the queued datagram (and/or fired handle_timeout)"
  );

  // The datagram was drained off the socket before handle_timeout: a fresh
  // bounded recv now BLOCKS (the timer wins) rather than returning the
  // still-queued datagram immediately — proving the chokepoint read the socket.
  // Scope the recv future so it drops (releasing its borrow of `driver`) before
  // the socket is closed below.
  let socket_drained = {
    let buf = vec![0u8; 64];
    let recv = driver.recv_from(buf).fuse();
    let timer = compio::time::sleep(Duration::from_millis(200)).fuse();
    pin_mut!(recv, timer);
    select_biased! {
      _ = recv => false,
      _ = timer => true,
    }
  };
  assert!(
    socket_drained,
    "fire_quic_timeout must drain the queued datagram off the socket before handle_timeout"
  );

  // Ignoring Err: test cleanup of the probe sockets.
  let _ = driver.close().await;
  let _ = peer.close().await;
}
