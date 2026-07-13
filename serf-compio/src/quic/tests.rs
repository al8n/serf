//! End-to-end smoke test: two QUIC serf nodes on the loopback interface, one
//! joining the other, asserting the membership event propagates through the full
//! pump (Join command → QUIC push-pull dial → quinn handshake → coordinator merge
//! → serf `Member` event → `EventStream`).

use core::time::Duration;
use std::{net::SocketAddr, sync::Arc};

use futures_util::StreamExt;
use memberlist_proto::{MaybeResolved, UnreliableTransport};
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serf_proto::{
  event::{Event, MemberEventKind},
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

use crate::{
  Channel, FirstAddrResolver, QuicOptions, QuicTransport, QuicTransportOptions, Resolver,
  RuntimeOptions, Serf, SerfError, SocketAddrResolver, Transport, VoidDelegate, gossip_rng,
};

/// A loopback address with a port nothing listens on — its QUIC push/pull dial
/// never completes a handshake, so its exchange fails. The port is below the OS
/// ephemeral range, so a `:0` test bind never collides with it.
fn blackhole_addr() -> SocketAddr {
  "127.0.0.1:7214".parse().expect("loopback addr")
}

/// Resolver that always resolves to an empty address list.
struct EmptyResolver;

impl Resolver for EmptyResolver {
  type Address = String;
  type Error = std::io::Error;

  async fn resolve(&self, _addr: &String) -> Result<Vec<SocketAddr>, std::io::Error> {
    Ok(Vec::new())
  }
}

#[cfg(encryption)]
use crate::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};

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

/// A QUIC config bundle with a 20s idle timeout (well past a localhost handshake)
/// and datagram-mode unreliable transport. A fresh bundle is built per node so
/// each owns its own cert and quinn endpoint config.
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

/// QUIC config bundle whose endpoint config accepts a max UDP payload (9000,
/// jumbo-frame sizing) well above the default 1400 gossip MTU. Exercises the
/// recv-buffer sizing that must cover the larger raw-QUIC plane.
fn test_quic_options_jumbo() -> QuicOptions {
  let mut endpoint = test_endpoint_config(&[0x5au8; 32]);
  endpoint
    .max_udp_payload_size(9000)
    .expect("9000 is within quinn's accepted [1200, 65527] range");
  let mut transport = quinn_proto::TransportConfig::default();
  transport.max_idle_timeout(Some(
    quinn_proto::IdleTimeout::try_from(Duration::from_secs(20)).unwrap(),
  ));
  QuicOptions::new(
    endpoint,
    test_server(),
    test_client(),
    transport,
    "localhost",
    UnreliableTransport::Datagram,
  )
}

/// Build and spawn a QUIC serf node bound to an ephemeral loopback port.
async fn spawn_node(id: &str) -> Serf<SmolStr> {
  spawn_node_with(id, test_quic_options()).await
}

/// Build and spawn a QUIC serf node from a caller-supplied [`QuicOptions`].
async fn spawn_node_with(id: &str, quic: QuicOptions) -> Serf<SmolStr> {
  try_spawn_node_at(id, quic, "127.0.0.1:0".parse().expect("loopback addr"))
    .await
    .expect("spawn serf node")
}

/// Build a QUIC serf node bound to a specific advertise address, returning the
/// construction result so the same-address rebind regression can assert a freed
/// UDP port accepts an immediate rebind.
async fn try_spawn_node_at(
  id: &str,
  quic: QuicOptions,
  bind: SocketAddr,
) -> Result<Serf<SmolStr>, SerfError> {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(quic);
  Serf::new::<QuicTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
}

/// Build VALID QUIC transport options (a real `quic_config` is supplied so the
/// runtime-option rejection, not the missing-config guard, is what fires) paired
/// with a deliberately invalid `runtime`, and assert `Serf::new` rejects it with
/// [`SerfError::InvalidOption`] — before binding a socket or spawning the
/// detached driver — rather than panicking the driver task on a zero-cap channel.
async fn assert_quic_new_rejects(runtime: RuntimeOptions) {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(test_quic_options());
  let res =
    Serf::new::<QuicTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      runtime,
      SerfOptions::new(),
      gossip_rng().expect("seed gossip rng"),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::rc::Rc::new(VoidKeyringDelegate),
    )
    .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero-capacity channel option must be rejected at construction"),
  }
}

/// A `Bounded(0)` observation channel (direct builder) is rejected by the QUIC
/// driver's `Serf::new` instead of panicking the detached driver task.
#[compio::test]
async fn quic_new_rejects_zero_observation_channel() {
  assert_quic_new_rejects(RuntimeOptions::new().with_observation_channel(Channel::Bounded(0)))
    .await;
}

/// A zero `event_queue_cap` (direct builder) is rejected at construction.
#[compio::test]
async fn quic_new_rejects_zero_event_queue_cap() {
  assert_quic_new_rejects(RuntimeOptions::new().with_event_queue_cap(0)).await;
}

/// A zero `cmd_fairness_budget` (direct builder) starves the command drain under
/// an inbound flood, so the QUIC driver's `Serf::new` rejects it at construction.
#[compio::test]
async fn quic_new_rejects_zero_cmd_fairness_budget() {
  assert_quic_new_rejects(RuntimeOptions::new().with_cmd_fairness_budget(0)).await;
}

/// A `Bounded(0)` observation channel sourced from a serde config is rejected.
#[cfg(feature = "serde")]
#[compio::test]
async fn quic_new_rejects_zero_observation_channel_from_serde() {
  let runtime: RuntimeOptions =
    serde_json::from_str(r#"{"observation_channel":{"bounded":0}}"#).expect("deserialize");
  assert_quic_new_rejects(runtime).await;
}

/// A `bounded:0` observation channel parsed from a clap flag is rejected.
#[cfg(feature = "clap")]
#[compio::test]
async fn quic_new_rejects_zero_observation_channel_from_clap() {
  use clap::Parser;

  #[derive(Parser)]
  struct Cli {
    #[command(flatten)]
    runtime: RuntimeOptions,
  }

  let cli = Cli::try_parse_from(["app", "--runtime-observation-channel", "bounded:0"])
    .expect("clap parses bounded:0");
  assert_quic_new_rejects(cli.runtime).await;
}

/// Two nodes on loopback: A joins B; A must observe B joining the cluster through
/// its event stream over a real QUIC push-pull exchange, then both shut down
/// cleanly.
#[compio::test]
async fn two_node_quic_join_observes_membership() {
  let b = spawn_node("node-b").await;
  let a = spawn_node("node-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  // Subscribe BEFORE the join so a `Member` event cannot race ahead of the
  // subscription (the channel buffers either way, but this is the clean order).
  let mut a_events = a.events();

  // Node A joins node B (await-result): the call returns the reached address
  // once the QUIC push-pull to B's seed completes.
  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  // Node A should observe node B joining via a `Member(Join)` event.
  let observed = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    matches!(observed, Ok(true)),
    "node A should observe node B joining the cluster within the timeout"
  );

  // Exercise the graceful shutdown command path on both nodes.
  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// The QUIC driver binds a single UDP socket and awaits its `close()` before
/// acking shutdown, so `shutdown().await` must release that bound port before it
/// resolves: a second QUIC node binding the SAME advertise address the instant
/// the first shuts down must construct successfully, not fail with `AddrInUse`.
#[compio::test]
async fn quic_shutdown_releases_bound_address_for_rebind() {
  let first = spawn_node("rebind-first").await;
  let addr = first.advertise_address();
  first.shutdown().await.expect("first node shuts down");

  let second = try_spawn_node_at("rebind-second", test_quic_options(), addr)
    .await
    .expect("rebinding the freed UDP address must succeed, not AddrInUse");
  assert_eq!(
    second.advertise_address(),
    addr,
    "the second node rebinds the exact freed address"
  );
  second.shutdown().await.expect("second node shuts down");
}

/// All `Serf` handles dropping under a continuous gossip flood must still shut the
/// QUIC driver down. Under the flood the higher-priority recv arm starves the main
/// select's command arm, so the command-channel disconnect is observable ONLY by
/// the iter-top command drain; a dropped handle must therefore free the bound UDP
/// socket for an immediate same-address rebind rather than spinning forever and
/// leaking it.
#[compio::test]
async fn quic_command_disconnect_under_flood_releases_bound_port() {
  let node = spawn_node("flood-drop").await;
  let addr = node.advertise_address();

  // Flood the driver's single UDP socket so the biased select's recv arm stays
  // ready — the load under which the command-channel disconnect must still tear
  // the driver down. A first byte of 1 is demuxed to the gossip plane (not a quinn
  // packet). A detached task keyed off a stop flag so it ends with the test.
  let stop = std::rc::Rc::new(std::cell::Cell::new(false));
  let flood_stop = stop.clone();
  compio::runtime::spawn(async move {
    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
    let peer = compio::net::UdpSocket::bind(bind)
      .await
      .expect("bind flood peer");
    while !flood_stop.get() {
      // Ignoring Err: best-effort flood; a transient send error is non-fatal.
      let _ = peer.send_to(vec![1u8, 0, 0, 0], addr).await.0;
      // Yield via a short sleep so the flood cannot monopolize the single-threaded
      // runtime — a loopback send can complete inline, which would starve the
      // driver and the rebind poll. The gossip socket's kernel queue keeps the
      // recv arm ready across the gap.
      compio::time::sleep(Duration::from_millis(1)).await;
    }
    // Ignoring Err: test cleanup of the flood socket.
    let _ = peer.close().await;
  })
  .detach();

  // Drop every handle: the command channel disconnects. Under the flood the main
  // select's command arm is starved, so the iter-top drain's Disconnect branch is
  // the only path that can observe it and tear down.
  drop(node);

  // The driver must terminate and release its bound UDP port; poll for the rebind
  // under a generous timeout so a regression (driver never exits) fails as a
  // timeout, not a hang.
  let rebound = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      if let Ok(gossip) = compio::net::UdpSocket::bind(addr).await {
        break gossip;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await;

  stop.set(true);
  let gossip = rebound.expect(
    "the QUIC driver must release its bound UDP port after a command-channel disconnect under flood",
  );
  // Ignoring Err: test cleanup of the rebind probe socket.
  let _ = gossip.close().await;
}

/// Two nodes whose quinn `EndpointConfig` accepts a max UDP payload (9000) far
/// above the default 1400 gossip MTU still form a cluster: the driver sizes its
/// recv buffer for the larger raw-QUIC plane, so a QUIC packet above the gossip
/// MTU is not truncated before the coordinator demuxes it. The discriminating
/// buffer-length assertion lives in the driver's `tests.rs`
/// (`recv_buf_len_for`); this proves threading the ceiling through construction
/// keeps a real handshake working end-to-end.
#[compio::test]
async fn two_node_quic_join_with_large_max_udp_payload() {
  let b = spawn_node_with("node-b", test_quic_options_jumbo()).await;
  let a = spawn_node_with("node-a", test_quic_options_jumbo()).await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  let mut a_events = a.events();

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  let observed = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    matches!(observed, Ok(true)),
    "a cluster with an above-MTU quinn max UDP payload should still form"
  );

  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// Binding the wildcard `0.0.0.0:0` reads an unspecified IP back from the
/// socket; gossiping it would publish an undialable contact, so construction
/// must reject it with `InvalidAdvertiseAddr` (the `quic_config` is supplied so
/// the advertise check, not the missing-config guard, is what fires).
#[compio::test]
async fn new_rejects_wildcard_advertise() {
  let wildcard: SocketAddr = "0.0.0.0:0".parse().expect("wildcard addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("wild-node"))
    .with_advertise_addr(MaybeResolved::Resolved(wildcard))
    .with_quic_config(test_quic_options());
  let res =
    QuicTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  match res {
    Err(SerfError::InvalidAdvertiseAddr(e)) => {
      assert!(
        e.addr().ip().is_unspecified(),
        "the rejected address carries the unspecified IP read back from the wildcard bind"
      );
    }
    Err(other) => panic!("expected InvalidAdvertiseAddr, got {other:?}"),
    Ok(_) => panic!("a wildcard advertise must be rejected, but construction succeeded"),
  }
}

/// A construction failure AFTER the socket is bound must close it (awaited)
/// before returning `Err`, or the bound UDP port leaks and a same-address rebind
/// races into `AddrInUse` (a plain drop is not a synchronous fd release on
/// compio/Windows-IOCP). A wildcard `0.0.0.0:0` advertise binds a concrete
/// OS-assigned port but is then rejected by `validate_advertise_addr` for its
/// unspecified IP; the exact freed `0.0.0.0:<port>` must immediately re-accept a
/// UDP bind, proving the socket did not leak on the error path. A real
/// `quic_config` is supplied so the advertise rejection — not the missing-config
/// guard — is what fires.
#[compio::test]
async fn new_failure_closes_bound_socket_for_rebind() {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("rebind-fail"))
    .with_advertise_addr(MaybeResolved::Resolved(
      "0.0.0.0:0".parse().expect("wildcard addr"),
    ))
    .with_quic_config(test_quic_options());
  let res =
    QuicTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  let freed = match res {
    Err(SerfError::InvalidAdvertiseAddr(e)) => e.addr(),
    Err(other) => panic!("expected a post-bind InvalidAdvertiseAddr failure, got {other:?}"),
    Ok(_) => panic!("a post-bind failure must reject construction, but it succeeded"),
  };

  let gossip = compio::net::UdpSocket::bind(freed)
    .await
    .expect("the freed UDP port must rebind, not AddrInUse");
  // Ignoring Err: test cleanup of the probe socket.
  let _ = gossip.close().await;
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled so the encrypted test works under either backend.
#[cfg(encryption)]
fn test_secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// Build and spawn a QUIC serf node on an ephemeral loopback port with
/// `encryption` installed as its gossip keyring policy.
#[cfg(encryption)]
async fn spawn_encrypted_node(id: &str, encryption: EncryptionOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(test_quic_options())
    .with_encryption(encryption);
  Serf::new::<QuicTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    None,
    None,
    None,
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// Two QUIC nodes sharing one gossip keyring: A joins B and must observe B
/// joining through its event stream. The reliable push-pull rides quinn's own
/// TLS, while the gossip datagrams are AEAD-sealed by the configured keyring —
/// so this proves the keyring reaches the QUIC coordinator and that an encrypted
/// QUIC cluster forms and interoperates end-to-end.
#[cfg(encryption)]
#[compio::test]
async fn two_node_quic_join_observes_membership_encrypted() {
  let enc = EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42)));
  let b = spawn_encrypted_node("node-b", enc.clone()).await;
  let a = spawn_encrypted_node("node-a", enc).await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  let mut a_events = a.events();

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  let observed = compio::time::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == &b_id) {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;

  assert!(
    matches!(observed, Ok(true)),
    "node A should observe node B joining the encrypted QUIC cluster within the timeout"
  );

  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}

/// Build and spawn a QUIC serf node with a custom `RuntimeOptions` (used by the
/// blackhole join tests to shorten the await-join deadline — a QUIC dial to a
/// closed UDP port has no fast reset, so its exchange resolves only at the
/// deadline reaper).
async fn spawn_node_with_runtime(id: &str, runtime: RuntimeOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(test_quic_options());
  Serf::new::<QuicTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    runtime,
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// `join_many` over two QUIC seeds — one reachable (node B), one a blackhole —
/// returns only the reached seed. B's handshake succeeds quickly; the blackhole
/// exchange stays pending until the (shortened) deadline, at which the waiter is
/// reaped with the reachable seed already in its contacted set.
#[compio::test]
async fn quic_join_many_returns_only_reached_seeds() {
  let b = spawn_node("jm-b").await;
  let a = spawn_node_with_runtime(
    "jm-a",
    RuntimeOptions::new().with_join_deadline(Duration::from_secs(6)),
  )
  .await;
  let b_addr = b.advertise_address();

  let reached = a
    .join_many(
      &SocketAddrResolver,
      [
        MaybeResolved::Resolved(b_addr),
        MaybeResolved::Resolved(blackhole_addr()),
      ]
      .into_iter(),
      false,
    )
    .await
    .expect("join_many reaches the reachable seed");

  assert_eq!(reached.len(), 1, "only the reachable seed is contacted");
  assert_eq!(
    reached[0], b_addr,
    "the reached set carries node B's address"
  );

  a.shutdown().await.expect("jm-a shuts down");
  b.shutdown().await.expect("jm-b shuts down");
}

/// An await-result `join` against an unreachable blackhole QUIC seed surfaces
/// `SerfError::JoinAllFailed { requested: 1, contacted: 0 }` at the deadline.
#[compio::test]
async fn quic_join_unreachable_seed_surfaces_join_all_failed() {
  let a = spawn_node_with_runtime(
    "blackhole-joiner",
    RuntimeOptions::new().with_join_deadline(Duration::from_secs(3)),
  )
  .await;

  let err = a
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(blackhole_addr()),
      false,
    )
    .await
    .expect_err("join against a blackhole must fail");

  match err {
    SerfError::JoinAllFailed(payload) => {
      assert_eq!(payload.requested(), 1, "one seed requested");
      assert_eq!(payload.contacted(), 0, "no seed contacted");
    }
    other => panic!("expected JoinAllFailed, got {other:?}"),
  }

  a.shutdown().await.expect("joiner shuts down");
}

/// A non-empty `join` whose resolver returns zero addresses surfaces
/// `JoinAllFailed` rather than a silent success (no command is even dispatched).
#[compio::test]
async fn quic_join_zero_resolution_surfaces_join_all_failed() {
  let a = spawn_node("empty-resolve-joiner").await;

  let err = a
    .join(
      &EmptyResolver,
      MaybeResolved::Unresolved("svc-a".into()),
      false,
    )
    .await
    .expect_err("a seed resolving to zero addresses must fail");

  match err {
    SerfError::JoinAllFailed(payload) => {
      assert_eq!(payload.requested(), 1, "one input seed requested");
      assert_eq!(payload.contacted(), 0);
    }
    other => panic!("expected JoinAllFailed, got {other:?}"),
  }

  a.shutdown().await.expect("joiner shuts down");
}

// ── SWIM / timing knobs ───────────────────────────────────────────────────────

/// Every SWIM knob starts UNSET, so a caller that sets none keeps the
/// coordinator's own defaults — `Transport::run` applies an override only when
/// it is `Some`.
#[test]
fn swim_knobs_start_unset() {
  let opts = crate::QuicTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(opts.push_pull_interval().is_none());
  assert!(opts.probe_interval().is_none());
  assert!(opts.probe_timeout().is_none());
  assert!(opts.gossip_interval().is_none());
  assert!(opts.suspicion_mult().is_none());
  assert!(opts.dead_node_reclaim_time().is_none());
  assert!(opts.suspicion_max_timeout_mult().is_none());
}

/// Every builder writes its OWN field: the accessors read back exactly what was
/// set, with distinct values per knob so a crossed assignment surfaces.
#[test]
fn swim_knob_builders_round_trip_each_knob() {
  let opts = crate::QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_push_pull_interval(Duration::from_millis(1))
    .with_probe_interval(Duration::from_millis(2))
    .with_probe_timeout(Duration::from_millis(3))
    .with_gossip_interval(Duration::from_millis(4))
    .with_suspicion_mult(5)
    .with_dead_node_reclaim_time(Duration::from_millis(6))
    .with_suspicion_max_timeout_mult(7);

  assert_eq!(opts.push_pull_interval(), Some(Duration::from_millis(1)));
  assert_eq!(opts.probe_interval(), Some(Duration::from_millis(2)));
  assert_eq!(opts.probe_timeout(), Some(Duration::from_millis(3)));
  assert_eq!(opts.gossip_interval(), Some(Duration::from_millis(4)));
  assert_eq!(opts.suspicion_mult(), Some(5));
  assert_eq!(
    opts.dead_node_reclaim_time(),
    Some(Duration::from_millis(6))
  );
  assert_eq!(opts.suspicion_max_timeout_mult(), Some(7));
}

/// A zero push/pull interval is a MEANINGFUL setting (it disables periodic
/// anti-entropy, isolating the gossip plane), so it must round-trip as
/// `Some(ZERO)` — never collapse back to the `None` that means "keep the
/// coordinator default".
#[test]
fn zero_push_pull_interval_is_set_not_unset() {
  let opts = crate::QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_push_pull_interval(Duration::ZERO);
  assert_eq!(opts.push_pull_interval(), Some(Duration::ZERO));
}
