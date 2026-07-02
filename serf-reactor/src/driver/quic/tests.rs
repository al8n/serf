//! Unit tests for the QUIC driver's recv-buffer sizing and the shared-UDP-path
//! timer/reap gate. The recv-buffer sizing is a pure decision that keeps neither the
//! gossip plane nor the raw-QUIC plane truncated on the shared UDP socket; the gate
//! tests drive the real pump and prove a recv-ERROR stop is not read as quiescence.
//! The full end-to-end pump behaviour (join / converge / user-event / query / leave,
//! encrypted convergence, mismatched-key enforcement, datagram egress) is covered by
//! the real-node suite in `tests/quic.rs`.

use super::*;

/// The recv buffer follows the LARGER of the two planes sharing the one socket. A
/// caller can set a quinn `max_udp_payload_size` above the gossip MTU — quinn's own
/// default 1472 already exceeds the 1400 default `gossip_mtu` — so the buffer must
/// be sized from the QUIC plane, not silently left at the gossip size that would
/// truncate a full-size QUIC packet.
#[test]
fn recv_buf_sizes_to_the_larger_plane() {
  // QUIC plane far above the gossip plane (jumbo `max_udp_payload_size`): the buffer
  // follows the QUIC plane regardless of the small AEAD overhead.
  assert_eq!(recv_buf_len_for(1400, 9000), 9000);

  // quinn's default max UDP payload (1472) exceeds the default gossip MTU (1400), so
  // the buffer is sized to 1472, not 1400 — a full-size QUIC packet is not
  // truncated. Holds with or without an AEAD backend since
  // 1472 > 1400 + ENCRYPTED_WRAPPER_OVERHEAD.
  assert_eq!(recv_buf_len_for(1400, 1472), 1472);
}

/// When the gossip plane is the larger of the two, the QUIC ceiling never shrinks it
/// below the AEAD-inflated gossip requirement.
#[test]
fn recv_buf_keeps_the_gossip_requirement() {
  // Large configured gossip MTU, default-ish QUIC payload: the gossip plane wins and
  // keeps its encrypted-wrapper headroom.
  assert_eq!(
    recv_buf_len_for(16_000, 1472),
    16_000 + ENCRYPTED_WRAPPER_OVERHEAD
  );

  // The gossip plane is capped at the IPv4 UDP maximum; a small QUIC payload cannot
  // shrink it below that cap.
  assert_eq!(recv_buf_len_for(70_000, 1472), GOSSIP_RECV_BUF_MAX);
}

/// quinn permits a `max_udp_payload_size` up to 65527, just above the gossip plane's
/// 65507 cap; the QUIC plane is NOT clamped to that cap, so such a packet is
/// buffered in full.
#[test]
fn recv_buf_does_not_clamp_quic_below_quinn_max() {
  assert_eq!(recv_buf_len_for(70_000, 65_527), 65_527);
}

/// Pump-level regressions for the shared-UDP-path timer/reap gate, driven over a
/// real `QuicDriver::poll` on a tokio runtime with a real bound socket.
#[cfg(all(feature = "quic-rustls-ring", feature = "tokio"))]
mod gate {
  use super::*;

  use core::num::NonZeroU8;
  use std::{
    sync::{Arc, atomic::AtomicBool},
    task::{Wake, Waker},
  };

  use agnostic::tokio::TokioRuntime;
  use memberlist_proto::{
    Endpoint, EndpointOptions, Node, QuicEndpoint as Coordinator, QuicOptions,
  };
  use rand::rngs::StdRng;
  use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified},
    version::TLS13,
  };
  use rustls_pki_types::{CertificateDer, PrivateKeyDer};
  use serf_proto::{
    members::{Member, MemberStatus},
    options::Options as SerfOptions,
    typed::Tags,
  };
  use smol_str::SmolStr;

  /// The tokio-backed reactor QUIC driver under test.
  type TestDriver = QuicDriver<SmolStr, TokioRuntime, StdRng, StdRng>;

  fn sa(s: &str) -> SocketAddr {
    s.parse().expect("loopback addr")
  }

  /// Drive the driver through exactly one `Future::poll`. The pump re-polls
  /// unconditionally on `more`, so the no-op waker is a valid, harmless sink.
  fn poll_once(driver: &mut TestDriver) -> Poll<()> {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(driver).poll(&mut cx)
  }

  /// A `Waker` that records whether it was woken. A SYNCHRONOUS wake during a poll is
  /// the busy-spin signal: it means the pump requested an immediate re-poll
  /// (`wake_by_ref`) rather than parking on a timer. Timer / channel registrations do
  /// NOT wake synchronously, so the flag stays clear when the pump correctly parks.
  #[derive(Default)]
  struct SpinFlag {
    woken: AtomicBool,
  }

  impl Wake for SpinFlag {
    fn wake(self: Arc<Self>) {
      self.woken.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
      self.woken.store(true, Ordering::SeqCst);
    }
  }

  /// A self-signed `localhost` cert + key for the test quinn `ServerConfig`.
  fn self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let ck =
      rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("self-signed cert");
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
      .expect("tls13 server")
      .with_no_client_auth()
      .with_single_cert(chain, key)
      .expect("single cert");
    let qsc = quinn_proto::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))
      .expect("quic server config");
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
      .expect("tls13 client")
      .dangerous()
      .with_custom_certificate_verifier(Arc::new(AnyServer))
      .with_no_client_auth();
    let qcc =
      quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(cfg)).expect("quic client");
    quinn_proto::ClientConfig::new(Arc::new(qcc))
  }

  /// A datagram-mode QUIC config bundle (the mode is irrelevant to the recv gate,
  /// but matches the production default the endpoint is built with).
  fn test_quic_options() -> QuicOptions {
    QuicOptions::new(
      test_endpoint_config(&[0x5au8; 32]),
      test_server(),
      test_client(),
      quinn_proto::TransportConfig::default(),
      "localhost",
      UnreliableTransport::Datagram,
    )
  }

  /// A minimal initial published snapshot (the local node, `Alive`, zeroed clocks);
  /// the gate tests never assert on it.
  fn initial_snapshot(id: &str, advertise: SocketAddr) -> SerfSnapshot<SmolStr, SocketAddr> {
    let member = Member::new(
      Node::new(SmolStr::new(id), advertise),
      Tags::new(),
      MemberStatus::Alive,
    );
    SerfSnapshot::new(
      vec![Arc::new(member)],
      &SmolStr::new(id),
      SerfState::Alive,
      LamportTime::from(0u64),
      LamportTime::from(0u64),
      LamportTime::from(0u64),
    )
  }

  /// Build a real `QuicDriver` over a bound UDP socket, scheduling deliberately OFF
  /// (built via `QuicDriver::new`, not `spawn_quic_driver`) so no stray coordinator
  /// deadline supplies a timer the test means to attribute to the parked join.
  async fn build_driver(
    iter_drain_cap: usize,
  ) -> (
    TestDriver,
    Receiver<Event<SmolStr, SocketAddr>>,
    Arc<Shared<SmolStr>>,
  ) {
    let advertise = sa("127.0.0.1:7946");
    let socket = <<TokioRuntime as Runtime>::Net as Net>::UdpSocket::bind("127.0.0.1:0")
      .await
      .expect("bind udp socket");
    let inner_opts = EndpointOptions::new(SmolStr::new("drv"), advertise)
      .with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));
    let quic_config = test_quic_options();
    let quic_max_udp_payload = quic_config.endpoint_ref().get_max_udp_payload_size();
    let inner = Endpoint::new(inner_opts, StdRng::seed_from_u64(0));
    let coord = Coordinator::new(inner, quic_config);
    let endpoint = QuicEndpoint::<SmolStr, StdRng, StdRng>::new_with_rng(
      coord,
      SerfOptions::new(),
      StdRng::seed_from_u64(1),
    );
    let shared = Arc::new(Shared::new(initial_snapshot("drv", advertise)));
    let obs_payload_bytes = Arc::new(AtomicU64::new(0));
    let (obs_tx, obs_rx) = flume::unbounded();
    let driver = QuicDriver::<SmolStr, TokioRuntime, StdRng, StdRng>::new(
      endpoint,
      socket,
      quic_max_udp_payload,
      shared.clone(),
      obs_tx,
      obs_payload_bytes,
      None,
      RuntimeOptions::new().with_iter_drain_cap(iter_drain_cap),
      None,
      #[cfg(encryption)]
      Arc::new(crate::VoidKeyringDelegate),
    );
    (driver, obs_rx, shared)
  }

  /// A recv-ERROR stop is NOT the kernel-empty (`Poll::Pending`) signal, so the
  /// shared-UDP-path gate must NOT read it as quiescence and fire the past-due
  /// join/leave reap: a completing QUIC push/pull packet may still sit in the kernel
  /// behind the error. The reap must defer until a real `Poll::Pending` stop.
  ///
  /// A live outbound push/pull with no peer feeds `pending` with an exchange id that
  /// never completes, so ONLY the past-due deadline reap can resolve the join — the
  /// clean signal for whether the gate fired. The first poll's recv is scripted to
  /// report an error stop (a bound UDP socket cannot be made to error on demand); the
  /// second poll's recv is a genuine `Poll::Pending`.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn recv_error_stop_is_not_quiescence() {
    let now = Instant::now();
    let (mut driver, _obs_rx, _shared) = build_driver(8).await;

    let sid = driver
      .endpoint
      .start_join_push_pull(sa("127.0.0.1:7300"), false, now);
    let eid = ExchangeId::from(sid);
    let (tx, mut rx) = oneshot::channel::<JoinReply>();
    let mut pending = HashSet::new();
    pending.insert(eid);
    driver.pending_joins.push(PendingJoin {
      pending,
      contacted: SmallVec::new(),
      ignore_streams: SmallVec::new(),
      requested: 1,
      // Past-due, so the deadline reap is due on the very first poll.
      deadline: now - Duration::from_secs(1),
      reply: Some(tx),
    });

    // Poll A: the recv loop stops on an ERROR. The past-due reap must NOT fire —
    // the error is backlog-uncertain, so the join stays parked.
    driver.recv_errors_remaining = 1;
    let _ = poll_once(&mut driver);
    assert!(
      rx.try_recv().expect("reply channel live").is_none(),
      "a recv-ERROR stop was read as quiescence: the past-due reap fired and resolved \
       the join, but a completing packet may sit behind the error",
    );

    // Poll B: the recv loop now stops on a `Poll::Pending` quiescent stop, scripted so
    // it is deterministic across platforms. (On Windows the join's QUIC packet to a
    // closed port draws an ICMP port-unreachable, so a real `recv_from` would return
    // `ConnectionReset` — another non-quiescent error stop — rather than `Pending`.)
    // The gate is quiescent, so the past-due reap fires and resolves the stuck join.
    driver.recv_force_pending = true;
    let _ = poll_once(&mut driver);
    match rx.try_recv().expect("reply channel live") {
      Some(Err((reached, SerfError::JoinAllFailed(_)))) => {
        assert!(reached.is_empty(), "no seed was contacted: {reached:?}");
      }
      other => {
        panic!("a quiescent (Poll::Pending) recv stop must fire the past-due reap: {other:?}")
      }
    }
  }

  /// A recv-ERROR stop is (correctly) non-quiescent for the timer/reap GATE, but that
  /// must NOT drive an immediate self-wake: with nothing bounded to make progress on —
  /// no saturated batch, and only a FUTURE deadline pending — a `wake_by_ref` every
  /// poll would busy-spin a core between deadlines. The pump must instead PARK on the
  /// bounded `RECV_ERROR_BACKOFF` timer: it returns `Poll::Pending` WITHOUT waking its
  /// waker synchronously, and the timer alone re-polls (retrying the errored recv)
  /// after the backoff.
  ///
  /// Fail-on-revert: with the recv-error stop feeding an unconditional `more`, the
  /// pump self-wakes on the very first poll and this asserts false.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn persistent_recv_error_does_not_spin() {
    let now = Instant::now();
    let (mut driver, _obs_rx, _shared) = build_driver(8).await;

    // A FUTURE (not-yet-due) deadline pending: the join sits parked, so nothing is due
    // this poll and the only thing that could wake the pump is the (buggy) recv-error
    // self-wake. `pending` empty + `reply` live keeps the waiter in the timer horizon
    // without a real exchange whose egress could set `more` and mask the spin.
    let (tx, _rx) = oneshot::channel::<JoinReply>();
    driver.pending_joins.push(PendingJoin {
      pending: HashSet::new(),
      contacted: SmallVec::new(),
      ignore_streams: SmallVec::new(),
      requested: 1,
      deadline: now + Duration::from_secs(30),
      reply: Some(tx),
    });

    // Script every recv-loop poll to report an ERROR stop for the whole test.
    driver.recv_errors_remaining = usize::MAX;

    let flag = Arc::new(SpinFlag::default());
    let waker = Waker::from(flag.clone());

    // Repeatedly poll: each must park (Pending) WITHOUT a synchronous self-wake. The
    // 5ms backoff timer cannot fire in the microseconds before the flag is read, and
    // each poll re-arms it (cancelling the prior), so only a busy-spin bug trips this.
    for _ in 0..5 {
      flag.woken.store(false, Ordering::SeqCst);
      let mut cx = Context::from_waker(&waker);
      let poll = Pin::new(&mut driver).poll(&mut cx);
      assert!(
        poll.is_pending(),
        "the pump must stay pending under a persistent recv error",
      );
      assert!(
        !flag.woken.load(Ordering::SeqCst),
        "a persistent recv error self-woke the pump (busy-spin) instead of parking on \
         the bounded RECV_ERROR_BACKOFF timer",
      );
    }
  }
}
