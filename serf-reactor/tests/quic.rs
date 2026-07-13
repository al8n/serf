//! Real-node QUIC serf tests: two loopback nodes exercising the reactor QUIC
//! driver end-to-end over a quinn-proto config bundle. Each test spins up
//! ephemeral `127.0.0.1:0` nodes via the ergonomic [`Serf::quic`] constructor and
//! drives the full pump — QUIC push/pull join over a real quinn handshake,
//! coordinator merge, datagram gossip, user events, queries, and graceful
//! leave/shutdown — end-to-end proof the reactor QUIC driver works over a concrete
//! runtime, meeting the same behaviour bar as the TCP suite.
//!
//! The scenario bodies are runtime-generic `async fn <R: Runtime>` helpers, so the
//! SAME scenario runs as a `#[tokio::test]` cell over `TokioRuntime` and as a
//! `_smol` cell driven by `SmolRuntime::block_on` — mirroring memberlist-reactor's
//! runtime-parameterized suite.
//!
//! Mirrors serf-compio's QUIC smoke tests and the reactor's `tests/tcp.rs`,
//! adapted to the reactor's `Send`/`agnostic` model and QUIC's single-socket,
//! stream-multiplexed transport.

#![cfg(feature = "quic")]

use core::time::Duration;
use std::{net::SocketAddr, sync::Arc};

use agnostic::Runtime;
use bytes::Bytes;
use futures_util::{StreamExt, future};
use memberlist_proto::UnreliableTransport;
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serf_proto::{
  event::{Event, MemberEventKind},
  members::{MemberStatus, SerfState},
  options::Options as SerfOptions,
};
#[cfg(encryption)]
use serf_reactor::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, QuicOptions, QuicTransportOptions, RuntimeOptions, Serf,
  SocketAddrResolver, VoidDelegate,
};
use smol_str::SmolStr;

/// The reusable multi-node fault-injection fixture, shared with the TCP and TLS
/// suites.
mod cluster;

/// A reactor QUIC node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// A self-signed cert + key for `localhost`, for the test TLS bundle.
fn self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("self-signed cert");
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

/// A fresh QUIC config bundle with a 20s idle timeout (well past a localhost
/// handshake) and the given unreliable-transport mode. A fresh bundle is built per
/// node so each owns its own cert and quinn endpoint config.
fn quic_options_with(unreliable: UnreliableTransport) -> QuicOptions {
  let mut transport = quinn_proto::TransportConfig::default();
  transport.max_idle_timeout(Some(
    quinn_proto::IdleTimeout::try_from(Duration::from_secs(20)).expect("idle timeout"),
  ));
  QuicOptions::new(
    test_endpoint_config(&[0x5au8; 32]),
    test_server(),
    test_client(),
    transport,
    "localhost",
    unreliable,
  )
}

/// The default test bundle: datagram-mode unreliable transport.
fn test_quic_options() -> QuicOptions {
  quic_options_with(UnreliableTransport::Datagram)
}

/// An ephemeral loopback bind (`127.0.0.1:0`).
fn ephemeral_bind() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

/// SWIM timing for the QUIC fault-injection cluster.
///
/// QUIC's first probe to a peer must also establish the pooled connection, so the
/// direct-ping timeout is widened relative to the shared `fast()` profile: a LIVE
/// peer whose session is still handshaking must not be falsely suspected, while an
/// abrupt kill is still detected in well under a second.
fn quic_timing() -> cluster::ClusterTiming {
  cluster::ClusterTiming::fast()
    .with_probe_interval(Duration::from_millis(200))
    .with_probe_timeout(Duration::from_millis(150))
}

/// The fixture's QUIC backend: the fast-SWIM timing mapped onto a
/// [`QuicTransportOptions`] block carrying a fresh per-node quinn bundle.
struct Quic;

impl<R> cluster::Backend<R> for Quic
where
  R: Runtime,
{
  async fn build(
    id: &str,
    bind: SocketAddr,
    timing: &cluster::ClusterTiming,
  ) -> serf_reactor::Result<cluster::Node<R>> {
    let mut opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(bind))
      .with_quic_config(test_quic_options())
      .with_probe_interval(timing.probe_interval())
      .with_probe_timeout(timing.probe_timeout())
      .with_gossip_interval(timing.gossip_interval())
      .with_suspicion_mult(timing.suspicion_mult());
    if let Some(v) = timing.dead_node_reclaim {
      opts = opts.with_dead_node_reclaim_time(v);
    }
    if let Some(v) = timing.push_pull_interval {
      opts = opts.with_push_pull_interval(v);
    }
    Serf::<SmolStr, SocketAddr, R>::quic(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      timing.serf_opts(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(VoidKeyringDelegate),
    )
    .await
  }
}

/// The QUIC fault-injection cluster.
type QuicCluster<R> = cluster::Cluster<R, Quic>;

/// Build and spawn a reactor QUIC node on an ephemeral loopback port through the
/// ergonomic `Serf::quic` constructor.
async fn spawn_node<R>(id: &str) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(test_quic_options());
  Serf::<SmolStr, SocketAddr, R>::quic(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf quic node")
}

/// Poll both nodes until each reports the full two-member cluster, or fail on a
/// generous timeout so a convergence regression surfaces as a timeout, not a hang.
async fn converge<R>(a: &Node<R>, b: &Node<R>)
where
  R: Runtime,
{
  R::timeout(Duration::from_secs(30), async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Two nodes on loopback: A joins B (await-result over a real QUIC push/pull), then
/// BOTH converge to a two-member cluster and shut down cleanly.
async fn two_node_quic_join_converges<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("conv-b").await;
  let a = spawn_node::<R>("conv-a").await;
  let b_addr = b.advertise_address();

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over QUIC");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  converge(&a, &b).await;
  assert_eq!(a.num_members(), 2, "A sees the 2-member cluster");
  assert_eq!(b.num_members(), 2, "B sees the 2-member cluster");

  a.shutdown().await.expect("conv-a shuts down");
  b.shutdown().await.expect("conv-b shuts down");
}

/// After a two-node QUIC join, a user event broadcast by B is delivered to A's event
/// stream carrying the original name and payload (datagram gossip over the shared
/// socket).
async fn user_event_delivered<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("ue-b").await;
  let a = spawn_node::<R>("ue-a").await;
  let b_addr = b.advertise_address();

  // Subscribe before joining so the user event cannot race the subscription.
  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  b.user_event("greet", Bytes::from_static(b"hello"), false)
    .await
    .expect("user event dispatched");

  let got = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "greet" => break Some(u.payload.clone()),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's user event within the timeout");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"hello")),
    "A receives B's user-event payload"
  );

  a.shutdown().await.expect("ue-a shuts down");
  b.shutdown().await.expect("ue-b shuts down");
}

/// Datagram-mode non-vacuity: with `UnreliableTransport::Datagram` the outbound
/// gossip is routed through the QUIC datagram plane (`queue_unreliable_datagram` +
/// `flush_outbound_transmits`), NOT the plain-UDP fallback. Two nodes join and
/// converge, B broadcasts a user event that A receives over gossip, and the sender's
/// `datagrams_sent` counter proves the gossip actually rode QUIC datagrams over the
/// pooled, TLS-protected connection.
///
/// This is the discriminator the plain-UDP fallback would otherwise mask: a driver
/// that bypassed the configured mode (the pre-fix always-`poll_send_to` path) still
/// delivers the event over UDP and converges, but leaves `datagrams_sent` at `0`.
/// `datagrams_sent` advances only on a `DatagramSendStatus::Queued`, so asserting it
/// is non-zero fails on that revert while the convergence assertions alone would not.
async fn datagram_mode_gossip_rides_quic_datagrams<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("dg-b").await;
  let a = spawn_node::<R>("dg-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over QUIC");
  converge(&a, &b).await;

  b.user_event("greet", Bytes::from_static(b"hello"), false)
    .await
    .expect("user event dispatched");

  let got = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "greet" => break Some(u.payload.clone()),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's user event within the timeout");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"hello")),
    "A receives B's user-event payload over datagram-mode gossip"
  );

  // The discriminator: the gossip that crossed rode QUIC datagrams, not the plain-UDP
  // fallback. Both nodes hold a warm pooled connection after the join, so their
  // periodic gossip is queued as datagrams; `datagrams_sent` advances only on a
  // `DatagramSendStatus::Queued`.
  R::timeout(Duration::from_secs(30), async {
    loop {
      if b.datagrams_sent() > 0 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect(
    "B's gossip must ride the QUIC datagram plane (datagrams_sent > 0), not the plain-UDP fallback",
  );

  a.shutdown().await.expect("dg-a shuts down");
  b.shutdown().await.expect("dg-b shuts down");
}

/// After a two-node QUIC join, a query issued by A round-trips: B receives the
/// `Event::Query`, responds, and A surfaces the matching `Event::QueryResponse`.
async fn query_round_trip<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-b").await;
  let a = spawn_node::<R>("q-a").await;
  let b_addr = b.advertise_address();

  // Subscribe both before the join so neither the query nor its response races ahead
  // of a subscription.
  let mut b_events = b.events();
  let mut a_events = a.events();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let want = Bytes::from_static(b"pong");
  a.query(
    "ping",
    Bytes::from_static(b"ping-payload"),
    a.default_query_param(),
  )
  .await
  .expect("query issued");

  // B answers the first "ping" query it sees; A collects the matching response.
  let responder = async {
    loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "ping" => {
          b.respond(qe, want.clone())
            .await
            .expect("B responds to the query");
          break;
        }
        Some(_) => {}
        None => panic!("B's event stream closed before the query arrived"),
      }
    }
  };
  let collector = async {
    loop {
      match a_events.next().await {
        Some(Event::QueryResponse(qr)) if qr.payload() == &want => break true,
        Some(_) => {}
        None => break false,
      }
    }
  };

  let got = R::timeout(Duration::from_secs(30), async {
    let (_, got) = future::join(responder, collector).await;
    got
  })
  .await
  .expect("query round-trip completes within the timeout");
  assert!(got, "A must receive B's query response");

  a.shutdown().await.expect("q-a shuts down");
  b.shutdown().await.expect("q-b shuts down");
}

/// A graceful leave completes the machine's leave chain: `leave()` resolves only
/// once `LeftCluster` fires (the reactor gates the reply on it), that event surfaces
/// on the leaver's own stream, and the local endpoint settles at `Left`.
async fn leave_emits_left_cluster<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("lv-b").await;
  let a = spawn_node::<R>("lv-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  // The reactor resolves `leave()` only once the machine's `LeftCluster` fires, so a
  // successful return already proves the graceful-leave chain completed.
  a.leave().await.expect("A leaves the cluster");

  // `LeftCluster` is also forwarded to A's own subscribers.
  let saw = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::LeftCluster) => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("A observes LeftCluster within the timeout");
  assert!(saw, "A must surface Event::LeftCluster after leave()");

  // The local endpoint state settles at `Left` (poll to absorb the snapshot-refresh
  // race after the leave chain completes).
  R::timeout(Duration::from_secs(5), async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A's endpoint state becomes Left");

  a.shutdown().await.expect("lv-a shuts down");
  b.shutdown().await.expect("lv-b shuts down");
}

/// Peer-visible graceful departure in `Datagram` mode with a shutdown racing the
/// leave: B's farewell must reach A even though B tears down immediately — the
/// leave fan-out rides plain UDP with socket-handoff retention rather than
/// quinn's congestion-gated datagram queue, so neither the racing teardown nor
/// the QUIC datagram plane can silently discard it. A must classify B's
/// departure as a Leave (never a Failed) and B's `leave().await` must resolve
/// `Ok`.
async fn leave_with_racing_shutdown_reaches_peer_as_leave<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("race-b").await;
  let a = spawn_node::<R>("race-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over QUIC");
  converge(&a, &b).await;

  let (leave, shutdown) = future::join(b.leave(), b.shutdown()).await;
  leave.expect("B leaves gracefully despite the racing shutdown");
  shutdown.expect("B shuts down");

  let kind = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me))
          if me
            .members()
            .iter()
            .any(|m| m.node().id_ref().as_str() == "race-b")
            && matches!(me.kind(), MemberEventKind::Leave | MemberEventKind::Failed) =>
        {
          break Some(me.kind());
        }
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's departure within the timeout");
  assert_eq!(
    kind,
    Some(MemberEventKind::Leave),
    "A must classify B's racing-shutdown departure as a graceful Leave, not a Failed"
  );

  a.shutdown().await.expect("race-a shuts down");
}

/// The QUIC driver binds a single UDP socket and drops it (releasing its FD) before
/// acking shutdown, so `shutdown().await` releases the bound port before it
/// resolves: a second QUIC node binding the SAME advertise address the instant the
/// first shuts down must construct successfully, not fail with `AddrInUse`.
async fn quic_shutdown_releases_bound_address_for_rebind<R>()
where
  R: Runtime,
{
  let first = spawn_node::<R>("rebind-first").await;
  let addr = first.advertise_address();
  first.shutdown().await.expect("first node shuts down");

  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("rebind-second"))
    .with_advertise_addr(MaybeResolved::Resolved(addr))
    .with_quic_config(test_quic_options());
  let second = Serf::<SmolStr, SocketAddr, R>::quic(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("rebinding the freed UDP address must succeed, not AddrInUse");
  assert_eq!(
    second.advertise_address(),
    addr,
    "the second node rebinds the exact freed address"
  );
  second.shutdown().await.expect("second node shuts down");
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled so the encrypted tests work under either backend.
#[cfg(encryption)]
fn test_secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// Build and spawn a reactor QUIC node on an ephemeral loopback port with
/// `encryption` installed as its gossip keyring policy.
///
/// Periodic anti-entropy push/pull is disabled (`with_push_pull_interval(ZERO)`).
/// On QUIC the reliable push/pull rides quinn's own TLS — NOT the gossip keyring —
/// and a `PushPullMessage` carries the buffered user events, so a background
/// full-state sync would smuggle a user event across a mismatched gossip keyring,
/// bypassing the AEAD. Disabling it leaves the gossip datagram plane as the sole
/// carrier of ongoing user events, which is exactly the plane these gossip-encryption
/// tests mean to exercise: the positive test then proves the event rode gossip (not
/// an incidental push/pull), and the negative test's absence is decisive rather than
/// a race against the next scheduled sync. Join-time exchanges are unaffected.
#[cfg(encryption)]
async fn spawn_encrypted_node<R>(id: &str, encryption: EncryptionOptions) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_quic_config(test_quic_options())
    .with_push_pull_interval(Duration::ZERO)
    .with_encryption(encryption);
  Serf::<SmolStr, SocketAddr, R>::quic(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn encrypted serf quic node")
}

/// The window within which a gossip-carried user event is proved to arrive on loopback
/// under a MATCHED keyring (the positive test) — and therefore the window its ABSENCE
/// is decisive over under a MISMATCHED keyring (the negative test). Shared by both so
/// the negative's absence is measured against the exact window the positive proves
/// delivery within, rather than an arbitrarily shorter one a real cross could outlast.
#[cfg(encryption)]
const GOSSIP_DELIVERY_WINDOW: Duration = Duration::from_secs(10);

/// Two QUIC nodes sharing one gossip keyring converge AND exchange gossip: A joins B
/// (the reliable push/pull rides quinn's own TLS, so it merges membership
/// regardless of the keyring), both reach the two-member cluster, and then a user
/// event B broadcasts — which rides the AEAD-sealed GOSSIP plane, not the reliable
/// push/pull — reaches A. The user-event delivery is the discriminating check: on
/// QUIC the gossip keyring seals only the datagram plane (the reliable plane is
/// quinn TLS), so it is a gossip-carried event, not the membership merge, that
/// proves `encrypt_gossip`/`decrypt_gossip` round-trip end-to-end rather than
/// running as identity transforms.
#[cfg(encryption)]
async fn two_node_quic_gossip_convergence_encrypted<R>()
where
  R: Runtime,
{
  let key = EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42)));
  let b = spawn_encrypted_node::<R>("enc-b", key.clone()).await;
  let a = spawn_encrypted_node::<R>("enc-a", key).await;
  let b_addr = b.advertise_address();

  // Subscribe before joining so the user event cannot race the subscription.
  let mut a_events = a.events();
  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted QUIC cluster");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  converge(&a, &b).await;

  // The gossip-plane discriminator: B broadcasts a user event, disseminated over the
  // AEAD-sealed gossip datagrams. With a shared keyring A decrypts and surfaces it.
  b.user_event("greet", Bytes::from_static(b"hello"), false)
    .await
    .expect("user event dispatched");

  let got = R::timeout(GOSSIP_DELIVERY_WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "greet" => break Some(u.payload.clone()),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's user event over the shared-key gossip plane within the shared window");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"hello")),
    "A receives B's user-event payload across the encrypted gossip plane"
  );

  a.shutdown().await.expect("enc-a shuts down");
  b.shutdown().await.expect("enc-b shuts down");
}

/// A node holding one gossip keyring and a node holding a DIFFERENT keyring share no
/// GOSSIP: on QUIC the reliable push/pull rides quinn's own TLS, so the join still
/// merges membership (the gossip keyring does not gate that plane) — but a user
/// event, which is disseminated only over the AEAD-sealed gossip datagrams, cannot
/// cross a disjoint key. Its ABSENCE at A proves the gossip encryption is real
/// enforcement, not an identity pass-through — the discriminating negative the
/// positive test above pairs with (both turn on a gossip-carried event, since the
/// membership merge crosses regardless of the key).
///
/// Determinism rests on the gossip plane being the event's SOLE carrier. Both nodes
/// run with periodic push/pull disabled (see `spawn_encrypted_node`): a background
/// full-state sync rides quinn TLS and replays a peer's buffered user events, so
/// left enabled it would carry the event over the reliable plane at a random point in
/// its interval — bypassing the gossip AEAD and racing any bounded window. With it
/// off, the join-time exchange (which precedes the broadcast, when B's event buffer
/// is still empty) is the only reliable exchange, and every ongoing user event must
/// ride gossip. A matched key WOULD surface the event within this window (the
/// positive test proves exactly that), so the absence is not vacuous.
#[cfg(encryption)]
async fn mismatched_keyring_gossip_does_not_cross<R>()
where
  R: Runtime,
{
  let b = spawn_encrypted_node::<R>(
    "mis-b",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x42))),
  )
  .await;
  let a = spawn_encrypted_node::<R>(
    "mis-a",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x43))),
  )
  .await;
  let b_addr = b.advertise_address();

  // The reliable push/pull (quinn TLS) merges membership regardless of the gossip
  // keyring, so the await-result join completes and then BOTH nodes hold each other
  // as members. Converging to that defined stable state first is what makes the
  // later absence "the event was blocked", not "it had not arrived yet".
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("the reliable push/pull merges membership over quinn TLS");
  converge(&a, &b).await;

  // Subscribe, then have B broadcast a user event over the gossip plane. Under the
  // mismatched keyring A cannot decrypt B's gossip datagrams, and periodic push/pull
  // is disabled, so no plane can carry the event to A.
  let mut a_events = a.events();
  b.user_event("secret", Bytes::from_static(b"hidden"), false)
    .await
    .expect("user event dispatched");

  // Poll over the SAME window the paired positive test proves delivery within, so the
  // absence is measured against a proven-sufficient window. A crossed event fails the
  // negation; a CLOSED stream (`None`) is NOT success — it would mean A's driver died
  // on the bad ciphertext, which cannot prove the event was blocked, so it too is a
  // failure. Only a clean timeout (the event never surfaces) is the blocked outcome.
  let outcome = R::timeout(GOSSIP_DELIVERY_WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "secret" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await;
  match outcome {
    Ok(true) => {
      panic!("node A surfaced B's gossip-carried user event across a mismatched keyring")
    }
    Ok(false) => panic!(
      "node A's event stream closed before the window elapsed — cannot conclude the \
       mismatched-key event was blocked"
    ),
    Err(_) => {}
  }
  // A clean timeout must mean "blocked", not "A died": both nodes must still hold the
  // 2-member cluster, so the absence was gossip-AEAD enforcement under a healthy,
  // converged pair.
  assert_eq!(
    a.num_members(),
    2,
    "A remains converged after the absence window"
  );
  assert_eq!(
    b.num_members(),
    2,
    "B remains converged after the absence window"
  );

  a.shutdown().await.expect("mis-a shuts down");
  b.shutdown().await.expect("mis-b shuts down");
}

/// A quic node with snapshot persistence configured.
async fn spawn_node_with_snapshot<R>(
  id: &str,
  snapshot: serf_reactor::SnapshotOptions,
  rejoin_after_leave: bool,
) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(bind))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new().with_rejoin_after_leave(rejoin_after_leave),
    None,
    None,
    Some(snapshot),
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn snapshot-backed serf quic node")
}

/// A unique snapshot path under the system temp dir, keyed by runtime as well
/// as pid: the tokio and smol cells run concurrently in one test binary and
/// must not share a snapshot file.
fn snapshot_path<R>(name: &str) -> std::path::PathBuf
where
  R: Runtime,
{
  let mut p = std::env::temp_dir();
  p.push(format!(
    "serf-e2e-quic-snap-{name}-{}-{}",
    std::process::id(),
    core::any::type_name::<R>().replace("::", "-"),
  ));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&p);
  p
}

/// The clean-leave gate over the quic driver: a graceful leave ends the
/// snapshot at the Leave record, the default posture starts fresh on restart,
/// and the opt-in posture rejoins from the persisted membership.
async fn snapshot_leave_gate_controls_rejoin<R>()
where
  R: Runtime,
{
  let path = snapshot_path::<R>("leave-gate");
  let a = spawn_node::<R>("qgate-a").await;
  let b =
    spawn_node_with_snapshot::<R>("qgate-b", serf_reactor::SnapshotOptions::new(&path), false)
      .await;
  let a_addr = a.advertise_address();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  b.leave().await.expect("qgate-b leaves gracefully");
  b.shutdown().await.expect("qgate-b shuts down");

  // The pump writes the clock floors BEFORE the leave marker, so a clean
  // shutdown ends the file at the Leave record — the terminal shape replay
  // expects and compaction preserves.
  {
    let bytes = std::fs::read(&path).expect("the snapshot survives the leave");
    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
      let (rec, used) =
        serf_proto::snapshot::SnapshotRecord::<SmolStr, SocketAddr>::decode(&bytes[cursor..])
          .expect("a clean-leave snapshot decodes whole");
      records.push(rec);
      cursor += used;
    }
    assert!(
      matches!(
        records.last(),
        Some(serf_proto::snapshot::SnapshotRecord::Leave)
      ),
      "a clean shutdown must end the snapshot at the Leave record"
    );
  }

  // Default posture: the leave clears the recovered state — no auto-rejoin.
  let b2 =
    spawn_node_with_snapshot::<R>("qgate-b", serf_reactor::SnapshotOptions::new(&path), false)
      .await;
  R::sleep(Duration::from_millis(1500)).await;
  assert_eq!(
    b2.num_members(),
    1,
    "a cleanly-left node must not auto-rejoin unless opted in"
  );
  b2.shutdown().await.expect("qgate-b2 shuts down");

  // Opt-in posture: the Leave marker is ignored and the membership recovers.
  let b3 =
    spawn_node_with_snapshot::<R>("qgate-b", serf_reactor::SnapshotOptions::new(&path), true).await;
  converge(&a, &b3).await;

  a.shutdown().await.expect("qgate-a shuts down");
  b3.shutdown().await.expect("qgate-b3 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// A key rotation over the quic driver with node B persisting through a
/// [`serf_reactor::FileKeyringDelegate`]: B's response is parked until the
/// file write is acknowledged and still collected within the query window,
/// with the file already holding the installed key when the response arrives.
#[cfg(unix)]
async fn file_backed_rotation_gates_the_response_on_persistence<R>()
where
  R: Runtime,
{
  let k1 = test_secret_key(0x55);
  let k2 = test_secret_key(0x66);

  let mut path = std::env::temp_dir();
  path.push(format!(
    "serf-quic-key-rotation-file-{}-{}",
    std::process::id(),
    core::any::type_name::<R>().replace("::", "-"),
  ));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&path);

  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let b = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("qfile-b"))
      .with_advertise_addr(MaybeResolved::Resolved(bind))
      .with_quic_config(test_quic_options())
      .with_encryption(enc()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    std::sync::Arc::new(serf_reactor::FileKeyringDelegate::new(&path)),
  )
  .await
  .expect("spawn file-backed encrypted quic node");
  let a = spawn_encrypted_node::<R>("qfile-a", enc()).await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted reliable plane");
  converge(&a, &b).await;

  let mut a_events = a.events();
  a.install_key(k2).await.expect("install_key dispatched");
  let kr = R::timeout(Duration::from_secs(20), async {
    loop {
      match a_events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("the event stream ended before the key response"),
      }
    }
  })
  .await
  .expect("A collects the key response within the timeout");
  assert!(
    kr.num_resp >= 2,
    "install_key must collect a response from BOTH nodes, including the one parked on file persistence (num_resp={})",
    kr.num_resp
  );
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");

  let persisted = serf_reactor::FileKeyringDelegate::new(&path)
    .load()
    .expect("the acknowledged write parses")
    .expect("the acknowledged write exists");
  assert!(
    persisted.secondaries().contains(&k2) || persisted.primary_ref() == &k2,
    "the response was gated on persistence, so the file already holds the installed key"
  );

  a.shutdown().await.expect("qfile-a shuts down");
  b.shutdown().await.expect("qfile-b shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Over QUIC: node A joins node B, then B is abruptly killed. A must observe
/// Join → Failed → Reap about B — a Failed (not a Leave), proving the kill
/// discards the graceful-leave datagram, followed by the reaper removing the
/// failed member under the shortened reconnect timeout.
async fn serf_events_failed<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(&["q-failed-a", "q-failed-b"], quic_timing()).await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  cluster.await_num_members(0, 1).await;

  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Reap,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Over QUIC: a graceful leave reaches the peer as a Leave (never a Failed) and
/// lands the leaver in the observer's Left tombstone view.
async fn serf_events_leave<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(
    &["q-leave-a", "q-leave-b"],
    quic_timing().with_tombstone_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.leave_graceful(1).await;

  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[MemberEventKind::Join, MemberEventKind::Leave],
    )
    .await;
  cluster.await_left_tombstone(0, subject.as_str()).await;

  cluster.shutdown_all().await;
}

/// Over QUIC: a failed node that returns at the SAME address reconnects rather
/// than being reaped — the survivor's reconnect re-dial re-establishes the QUIC
/// connection and the member revives (Join → Failed → Join).
async fn serf_reconnect<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(
    &["q-recon-a", "q-recon-b"],
    quic_timing().with_reconnect_timeout(Duration::from_secs(30)),
  )
  .await;
  let subject = cluster.id(1);

  cluster.kill_abrupt(1).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster.restart(1).await;
  cluster.await_num_members(0, 2).await;

  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Join,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Over QUIC: an operator force-leaving a FAILED member transitions it to Left on
/// every surviving node rather than leaving it to linger Failed until the reap.
async fn serf_force_leave_failed<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(
    &["q-fl-a", "q-fl-b", "q-fl-c"],
    quic_timing()
      .with_tombstone_timeout(Duration::from_secs(120))
      .with_reconnect_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(2);

  cluster.kill_abrupt(2).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .force_leave(subject.clone(), false)
    .await
    .expect("force_leave dispatches for a failed member");

  cluster.await_left_tombstone(0, subject.as_str()).await;
  cluster.await_left_tombstone(1, subject.as_str()).await;
  cluster
    .assert_member_events(
      0,
      subject.as_str(),
      &[
        MemberEventKind::Join,
        MemberEventKind::Failed,
        MemberEventKind::Leave,
      ],
    )
    .await;

  cluster.shutdown_all().await;
}

/// Over QUIC: removing a failed member WITH prune erases it outright on every
/// survivor — the membership count drops without waiting out the Left tombstone.
async fn serf_remove_failed_node_prune_erases<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(
    &["q-prune-a", "q-prune-b", "q-prune-c"],
    quic_timing().with_reconnect_timeout(Duration::from_secs(120)),
  )
  .await;
  let subject = cluster.id(2);

  cluster.kill_abrupt(2).await;
  cluster
    .await_member_event(0, subject.as_str(), MemberEventKind::Failed)
    .await;
  cluster
    .await_member_event(1, subject.as_str(), MemberEventKind::Failed)
    .await;

  cluster
    .node(0)
    .remove_failed_node_prune(subject.clone())
    .await
    .expect("remove_failed_node_prune dispatches");

  cluster.await_num_members(0, 2).await;
  cluster.await_num_members(1, 2).await;

  cluster.shutdown_all().await;
}

/// Over QUIC: a tag change propagates — the observer records an Update member
/// event for the setter and its member view carries the new value.
async fn serf_set_tags_propagates<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(&["q-tags-a", "q-tags-b"], quic_timing()).await;
  let b_id = cluster.id(1);

  let mut tags = serf_proto::Tags::new();
  tags.0.insert(SmolStr::new("role"), SmolStr::new("worker"));
  cluster
    .node(1)
    .set_tags(tags)
    .await
    .expect("B re-tags itself");
  cluster
    .await_member_event(0, b_id.as_str(), MemberEventKind::Update)
    .await;

  let seen = cluster
    .node(0)
    .members()
    .iter()
    .find(|m| m.node().id_ref().as_str() == b_id.as_str())
    .map(|m| m.tags().0.get("role").cloned())
    .expect("A tracks B");
  assert_eq!(
    seen.as_deref(),
    Some("worker"),
    "A's view of B carries the propagated role tag"
  );

  cluster.shutdown_all().await;
}

/// Over QUIC: after a peer leaves gracefully, the observer reaps it while the
/// leaver holds its own self `Left` tombstone (never a self Reap) beside the
/// still-Alive peer.
async fn serf_join_leave<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(&["q-jl-a", "q-jl-b"], quic_timing()).await;
  let peer = cluster.id(0);
  let leaver = cluster.id(1);

  cluster.leave_in_place(1).await;
  cluster.await_num_members(0, 1).await;

  assert_eq!(
    cluster.member_event_kinds(1, leaver.as_str()),
    vec![MemberEventKind::Join, MemberEventKind::Leave],
    "the leaver holds its self tombstone: Join then Leave, never a self Reap"
  );
  cluster
    .await_member_status(1, leaver.as_str(), MemberStatus::Left)
    .await;
  cluster
    .await_member_status(1, peer.as_str(), MemberStatus::Alive)
    .await;

  cluster.shutdown_all().await;
}

/// `join_many` over two seeds — one reachable, one an unroutable blackhole port —
/// returns only the reached seed's address once both exchanges terminate. The
/// blackhole seed drives the QUIC driver's dial-failure path: a seed that never
/// establishes must retire its exchange, not hang the join.
async fn join_many_returns_only_reached_seeds<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-jm-b").await;
  let a = spawn_node::<R>("q-jm-a").await;
  let b_addr = b.advertise_address();
  let blackhole: SocketAddr = "127.0.0.1:7219".parse().expect("loopback addr");

  let reached = a
    .join_many(
      &SocketAddrResolver,
      [
        MaybeResolved::Resolved(b_addr),
        MaybeResolved::Resolved(blackhole),
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

  a.shutdown().await.expect("q-jm-a shuts down");
  b.shutdown().await.expect("q-jm-b shuts down");
}

/// Over QUIC: removing a name that is not a member reports success as a no-op and
/// leaves the membership view unchanged.
async fn remove_failed_node_absent_is_a_noop<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("q-absent-a").await;
  a.remove_failed_node(SmolStr::new("no-such-node"))
    .await
    .expect("removing an absent member is an accepted no-op");
  assert_eq!(a.num_members(), 1, "the membership view is unchanged");
  a.shutdown().await.expect("q-absent-a shuts down");
}

/// Every mutating operation is REFUSED with `NotRunning` once the node has left
/// the cluster — `leave()` stops the periodic schedulers, so a post-leave mutation
/// would be issued by a node no longer participating. The read-only coordinate
/// probe is the deliberate exception: post-leave introspection stays valid.
///
/// `respond` is included by capturing a live query token BEFORE the leave, which
/// is the only way its post-leave arm can be reached at all.
async fn post_leave_operations_report_not_running<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("q-nr-a").await;
  let b = spawn_node::<R>("q-nr-b").await;
  let a_addr = a.advertise_address();

  let mut b_events = b.events();
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  // Capture a live query token on B, so the post-leave `respond` below has
  // something real to answer.
  a.query("probe", Bytes::from_static(b"q"), a.default_query_param())
    .await
    .expect("query issued");
  let token = R::timeout(Duration::from_secs(30), async {
    loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "probe" => break Some(qe),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("B surfaces the query within the timeout")
  .expect("B's event stream stays open");

  b.leave().await.expect("B leaves the cluster");

  let not_running = |e: &serf_reactor::SerfError| matches!(e, serf_reactor::SerfError::NotRunning);

  let err = b
    .join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect_err("a left node cannot rejoin in place");
  assert!(not_running(&err), "join after leave: got {err:?}");

  for (what, res) in [
    (
      "user_event",
      b.user_event("nope", Bytes::from_static(b"x"), false).await,
    ),
    (
      "query",
      b.query("nope", Bytes::from_static(b"x"), b.default_query_param())
        .await
        .map(|_| ()),
    ),
    ("respond", b.respond(token, Bytes::from_static(b"x")).await),
    ("set_tags", b.set_tags(serf_proto::Tags::new()).await),
    (
      "force_leave",
      b.force_leave(SmolStr::new("q-nr-a"), false).await,
    ),
  ] {
    let err = res.expect_err("a left node refuses to mutate");
    assert!(
      not_running(&err),
      "{what} after leave must report NotRunning, got {err:?}"
    );
  }

  // The read-only coordinate probe still answers after the leave.
  #[cfg(feature = "coordinates")]
  b.cached_coordinate(SmolStr::new("q-nr-a"))
    .await
    .expect("a read-only coordinate probe stays answerable after leave");

  a.shutdown().await.expect("q-nr-a shuts down");
  b.shutdown().await.expect("q-nr-b shuts down");
}

/// The key-management operations refuse to run on a node that has left — the same
/// `NotRunning` gate the membership mutations take, so a departed node can never
/// originate a cluster-wide rotation.
#[cfg(encryption)]
async fn post_leave_key_operations_report_not_running<R>()
where
  R: Runtime,
{
  let node = spawn_encrypted_node::<R>(
    "q-nrkey",
    EncryptionOptions::new().with_keyring(Keyring::new(test_secret_key(0x71))),
  )
  .await;
  node.leave().await.expect("the node leaves the cluster");

  let k = test_secret_key(0x72);
  for (what, res) in [
    ("install_key", node.install_key(k).await.map(|_| ())),
    ("use_key", node.use_key(k).await.map(|_| ())),
    ("remove_key", node.remove_key(k).await.map(|_| ())),
    ("list_keys", node.list_keys().await.map(|_| ())),
  ] {
    let err = res.expect_err("a left node refuses a key rotation");
    assert!(
      matches!(err, serf_reactor::SerfError::NotRunning),
      "{what} after leave must report NotRunning, got {err:?}"
    );
  }

  node.shutdown().await.expect("q-nrkey shuts down");
}

/// The full key-rotation lifecycle over QUIC: `install_key` adds a secondary on
/// every node, `use_key` promotes it to primary, `list_keys` reports the resulting
/// ring from BOTH nodes, and `remove_key` drops the retired key. Each step is
/// collected as a cluster-wide `KeyResponse`, so the assertions pin that the
/// rotation reached the peer — not merely the originator's own ring.
#[cfg(encryption)]
async fn key_rotation_lifecycle<R>()
where
  R: Runtime,
{
  let k1 = test_secret_key(0x81);
  let k2 = test_secret_key(0x82);
  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));

  let b = spawn_encrypted_node::<R>("q-rot-b", enc()).await;
  let a = spawn_encrypted_node::<R>("q-rot-a", enc()).await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted QUIC cluster");
  converge(&a, &b).await;

  /// Collect the next cluster-wide key response off A's stream.
  async fn next_key_response<R>(
    events: &mut serf_reactor::EventStream<SmolStr, SocketAddr>,
  ) -> serf_proto::event::KeyResponse<SmolStr>
  where
    R: Runtime,
  {
    R::timeout(Duration::from_secs(30), async {
      loop {
        match events.next().await {
          Some(Event::KeyResponse(kr)) => break kr,
          Some(_) => {}
          None => panic!("the event stream ended before the key response"),
        }
      }
    })
    .await
    .expect("the key response is collected within the query window")
  }

  a.install_key(k2).await.expect("install_key dispatched");
  let kr = next_key_response::<R>(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");
  assert!(
    kr.num_resp >= 2,
    "install_key must be answered by BOTH nodes (num_resp={})",
    kr.num_resp
  );

  a.use_key(k2).await.expect("use_key dispatched");
  let kr = next_key_response::<R>(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "use_key must succeed on every node");
  assert!(kr.num_resp >= 2, "use_key must be answered by BOTH nodes");

  a.list_keys().await.expect("list_keys dispatched");
  let kr = next_key_response::<R>(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "list_keys must succeed on every node");
  assert!(
    kr.num_resp >= 2,
    "list_keys must be answered by BOTH nodes — the rotation reached the peer"
  );

  // The promoted key is now the primary everywhere, so retiring the ORIGINAL key
  // must be accepted (removing a live primary is refused).
  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response::<R>(&mut a_events).await;
  assert_eq!(
    kr.num_err, 0,
    "remove_key must succeed on every node once the key is no longer primary"
  );
  assert!(
    kr.num_resp >= 2,
    "remove_key must be answered by BOTH nodes"
  );

  // The cluster still gossips under the rotated key: a user event still crosses.
  let mut a_gossip = a.events();
  b.user_event("post-rotation", Bytes::from_static(b"ok"), false)
    .await
    .expect("user event dispatched");
  let got = R::timeout(GOSSIP_DELIVERY_WINDOW, async {
    loop {
      match a_gossip.next().await {
        Some(Event::User(u)) if u.name.as_str() == "post-rotation" => {
          break Some(u.payload.clone());
        }
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("the rotated cluster still carries gossip");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"ok")),
    "gossip still crosses under the rotated primary key"
  );

  a.shutdown().await.expect("q-rot-a shuts down");
  b.shutdown().await.expect("q-rot-b shuts down");
}

/// The `Udp` unreliable-transport opt-out routes ALL gossip over the shared plain
/// UDP socket instead of the QUIC datagram plane: two nodes still join and gossip
/// a user event, and the sender's `datagrams_sent` stays at ZERO — the exact
/// discriminator the datagram-mode test asserts the opposite of.
async fn udp_mode_gossip_bypasses_the_datagram_plane<R>()
where
  R: Runtime,
{
  async fn spawn_udp_node<R>(id: &str) -> Node<R>
  where
    R: Runtime,
  {
    Serf::<SmolStr, SocketAddr, R>::quic(
      QuicTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
        .with_quic_config(quic_options_with(UnreliableTransport::Udp)),
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(VoidKeyringDelegate),
    )
    .await
    .expect("spawn udp-mode serf quic node")
  }

  let b = spawn_udp_node::<R>("udp-b").await;
  let a = spawn_udp_node::<R>("udp-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the QUIC reliable plane");
  converge(&a, &b).await;

  b.user_event("greet", Bytes::from_static(b"hello"), false)
    .await
    .expect("user event dispatched");

  let got = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "greet" => break Some(u.payload.clone()),
        Some(_) => {}
        None => break None,
      }
    }
  })
  .await
  .expect("A observes B's user event over plain-UDP gossip");
  assert_eq!(
    got,
    Some(Bytes::from_static(b"hello")),
    "the event crosses over the plain-UDP gossip plane"
  );

  // The discriminator: in `Udp` mode NO gossip payload may ride a QUIC datagram.
  // The counter advances only on a `DatagramSendStatus::Queued`, so a driver that
  // ignored the configured mode would leave it non-zero here.
  assert_eq!(
    b.datagrams_sent(),
    0,
    "the Udp opt-out must route every gossip payload over the plain socket"
  );
  assert_eq!(a.datagrams_sent(), 0);

  a.shutdown().await.expect("udp-a shuts down");
  b.shutdown().await.expect("udp-b shuts down");
}

/// The QUIC transport's construction gate: each field `QuicTransport::new`
/// requires is refused when absent, so a half-built options block can never bind
/// a socket.
async fn construction_requires_id_advertise_and_quic_config<R>()
where
  R: Runtime,
{
  async fn build<R>(
    opts: QuicTransportOptions<SmolStr, SocketAddr>,
  ) -> serf_reactor::Result<Node<R>>
  where
    R: Runtime,
  {
    Serf::<SmolStr, SocketAddr, R>::quic(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(VoidKeyringDelegate),
    )
    .await
  }

  let cases: [(&str, QuicTransportOptions<SmolStr, SocketAddr>); 3] = [
    (
      "local_id",
      QuicTransportOptions::new()
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
        .with_quic_config(test_quic_options()),
    ),
    (
      "advertise_addr",
      QuicTransportOptions::new()
        .with_local_id(SmolStr::new("no-addr"))
        .with_quic_config(test_quic_options()),
    ),
    (
      "quic_config",
      QuicTransportOptions::new()
        .with_local_id(SmolStr::new("no-cfg"))
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind())),
    ),
  ];
  for (missing, opts) in cases {
    let err = build::<R>(opts)
      .await
      .err()
      .unwrap_or_else(|| panic!("a node missing {missing} cannot be built"));
    assert!(
      matches!(err, serf_reactor::SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
      "a missing {missing} is an InvalidInput, got {err:?}"
    );
  }
}

/// A wildcard advertise address is refused AFTER the bind: the readback keeps the
/// unspecified IP, which peers could not route back to, so construction fails and
/// the bound socket is released rather than the node joining as an undialable
/// member. The released port is proven free by an immediate successful rebind of
/// the very port the failed attempt had claimed.
async fn wildcard_advertise_is_refused_and_releases_the_bind<R>()
where
  R: Runtime,
{
  let probe = spawn_node::<R>("q-wild-probe").await;
  let port = probe.advertise_address().port();
  probe.shutdown().await.expect("probe shuts down");

  let build = |id: &'static str, addr: SocketAddr| async move {
    Serf::<SmolStr, SocketAddr, R>::quic(
      QuicTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(addr))
        .with_quic_config(test_quic_options()),
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(VoidKeyringDelegate),
    )
    .await
  };

  let wildcard: SocketAddr = format!("0.0.0.0:{port}").parse().expect("wildcard addr");
  let err = build("q-wild", wildcard)
    .await
    .err()
    .expect("a wildcard advertise address is not a routable contact");
  assert!(
    matches!(err, serf_reactor::SerfError::InvalidAdvertiseAddr(_)),
    "a wildcard bind must be refused as an invalid advertise address, got {err:?}"
  );

  // The refused construction released the bound socket: the same port rebinds.
  let after = build(
    "q-wild-after",
    format!("127.0.0.1:{port}").parse().expect("loopback addr"),
  )
  .await
  .expect("the refused construction released the port it had bound");
  assert_eq!(after.advertise_address().port(), port);
  after.shutdown().await.expect("q-wild-after shuts down");
}

/// An UNRESOLVED advertise address is resolved at construction through the
/// caller's resolvers, and the node comes up on the resolved contact.
async fn unresolved_advertise_addr_is_resolved_at_construction<R>()
where
  R: Runtime,
{
  let node = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-resolve-me"))
      .with_advertise_addr(MaybeResolved::Unresolved(ephemeral_bind()))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("the unresolved advertise address resolves through the supplied resolver");

  let bound = node.advertise_address();
  assert!(bound.ip().is_loopback(), "the resolved contact is loopback");
  assert_ne!(
    bound.port(),
    0,
    "the ephemeral bind resolved to a concrete port"
  );
  node.shutdown().await.expect("q-resolve-me shuts down");
}

/// The constructor-supplied merge delegate is the predicate the machine consults
/// on the QUIC join push/pull: with a recording accept-all delegate installed on
/// B, A's join drives at least one `notify_merge` on B carrying A's node state.
async fn merge_delegate_is_consulted_on_join<R>()
where
  R: Runtime,
{
  use std::sync::atomic::{AtomicUsize, Ordering};

  struct RecordingMerge {
    hits: Arc<AtomicUsize>,
    saw_peer: Arc<AtomicUsize>,
  }
  impl serf_reactor::MergeDelegate<SmolStr, SocketAddr> for RecordingMerge {
    fn notify_merge(
      &self,
      peers: memberlist_proto::MaybeOwned<
        '_,
        [memberlist_proto::typed::NodeState<SmolStr, SocketAddr>],
      >,
    ) -> bool {
      self.hits.fetch_add(1, Ordering::Relaxed);
      if peers.iter().any(|p| p.id_ref().as_str() == "q-merge-a") {
        self.saw_peer.fetch_add(1, Ordering::Relaxed);
      }
      true
    }
  }

  let hits = Arc::new(AtomicUsize::new(0));
  let saw_peer = Arc::new(AtomicUsize::new(0));

  let b = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-merge-b"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    Some(Box::new(RecordingMerge {
      hits: hits.clone(),
      saw_peer: saw_peer.clone(),
    })),
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn q-merge-b");
  let a = spawn_node::<R>("q-merge-a").await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  assert!(
    hits.load(Ordering::Relaxed) > 0,
    "the machine must consult the constructor-supplied merge delegate on the join push/pull"
  );
  assert!(
    saw_peer.load(Ordering::Relaxed) > 0,
    "the consulted peer set must carry the joining node's state"
  );

  a.shutdown().await.expect("q-merge-a shuts down");
  b.shutdown().await.expect("q-merge-b shuts down");
}

/// After a two-node QUIC join, the snapshot read-forwarders reflect the joined
/// cluster: `members` returns both nodes, `local_member` / `local_id` return this
/// node, `state` is `Alive`, `advertise_node` composes id + advertise, and the
/// operator aggregate reports the converged, healthy view.
async fn snapshot_forwarders_reflect_joined_cluster<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-snap-b").await;
  let a = spawn_node::<R>("q-snap-a").await;
  let b_addr = b.advertise_address();
  let a_id = SmolStr::new("q-snap-a");
  let b_id = SmolStr::new("q-snap-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let members = a.members();
  assert_eq!(members.len(), 2, "members() returns the 2-member cluster");
  let ids: Vec<&SmolStr> = members.iter().map(|m| m.node().id_ref()).collect();
  assert!(ids.contains(&&a_id), "members() includes local node A");
  assert!(ids.contains(&&b_id), "members() includes peer node B");

  assert_eq!(a.local_member().node().id_ref(), &a_id);
  assert_eq!(a.local_id(), a_id);
  assert_eq!(a.state(), SerfState::Alive, "state() is Alive after join");
  assert_eq!(a.advertise_node().id_ref(), &a_id);
  assert_eq!(a.advertise_node().addr_ref(), &a.advertise_address());

  let stats = a.stats();
  assert_eq!(stats.members(), 2);
  assert_eq!(stats.failed(), 0);
  assert_eq!(stats.left(), 0);
  assert_eq!(stats.health_score(), 0, "a healthy node scores 0");
  assert!(!a.encryption_enabled(), "no keyring is configured");

  a.shutdown().await.expect("q-snap-a shuts down");
  b.shutdown().await.expect("q-snap-b shuts down");
}

/// After a two-node QUIC join, probe round-trips feed the Vivaldi coordinate
/// client: the local coordinate surfaces through the published snapshot, and the
/// peer's coordinate surfaces through the driver round-trip (`cached_coordinate`).
/// Both must go `Some` within the probe cadence — the discriminator that the QUIC
/// driver actually forwards coordinates rather than merely compiling the feature.
#[cfg(feature = "coordinates")]
async fn coordinates_surface_on_the_handle<R>()
where
  R: Runtime,
{
  let mut cluster = QuicCluster::<R>::spawn(&["q-coord-a", "q-coord-b"], quic_timing()).await;
  let b_id = cluster.id(1);

  let deadline = std::time::Instant::now() + Duration::from_secs(30);
  loop {
    let local = cluster.node(0).coordinate();
    let cached = cluster
      .node(0)
      .cached_coordinate(b_id.clone())
      .await
      .expect("cached_coordinate round-trips through the driver");
    if local.is_some() && cached.is_some() {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "coordinates must surface on the handle: local={local:?} cached={cached:?}"
    );
    R::sleep(Duration::from_millis(50)).await;
  }

  cluster.shutdown_all().await;
}

/// Leave is a SHARED in-flight operation: two concurrent `leave()` calls JOIN one
/// leave — the machine's `leave()` is invoked once and its single `LeftCluster`
/// resolves BOTH callers `Ok`. A THIRD leave issued after the chain completed is
/// an accepted no-op, not an error.
async fn concurrent_leaves_share_one_in_flight_leave<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-dl-b").await;
  let a = spawn_node::<R>("q-dl-a").await;
  let b_addr = b.advertise_address();

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  let (first, second) = future::join(a.leave(), a.leave()).await;
  first.expect("the initiating leave resolves Ok");
  second.expect("the leave that JOINED the in-flight one resolves Ok too");

  // The shared leave still drove the endpoint to Left (poll to absorb the
  // snapshot-refresh race after the leave chain completes).
  R::timeout(Duration::from_secs(5), async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the shared leave drives the endpoint to Left");

  a.leave()
    .await
    .expect("leaving an already-left node is an accepted no-op");

  a.shutdown().await.expect("q-dl-a shuts down");
  b.shutdown().await.expect("q-dl-b shuts down");
}

/// An `ignore_old` join records each seed's exchange as a one-shot replay-suppress
/// target: the join still merges membership, and the joiner does NOT replay the
/// seed's pre-join user event. A plain join is the control — it DOES surface the
/// buffered event — so the suppression is proven, not merely asserted as absence.
async fn ignore_old_join_suppresses_the_replay<R>()
where
  R: Runtime,
{
  let seed = spawn_node::<R>("q-io-seed").await;
  let seed_addr = seed.advertise_address();

  seed
    .user_event("old-news", Bytes::from_static(b"stale"), false)
    .await
    .expect("the seed buffers a pre-join user event");

  let plain = spawn_node::<R>("q-io-plain").await;
  let mut plain_events = plain.events();
  plain
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      false,
    )
    .await
    .expect("the plain join reaches the seed");
  let replayed = R::timeout(Duration::from_secs(30), async {
    loop {
      match plain_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "old-news" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("a plain join replays the seed's buffered user event");
  assert!(
    replayed,
    "the control join must surface the buffered event, or the suppression below is vacuous"
  );

  let quiet = spawn_node::<R>("q-io-quiet").await;
  let mut quiet_events = quiet.events();
  let reached = quiet
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      true,
    )
    .await
    .expect("the ignore_old join reaches the seed");
  assert_eq!(reached, seed_addr, "the ignore_old join still merges");
  R::timeout(Duration::from_secs(30), async {
    loop {
      if quiet.num_members() >= 2 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the ignore_old join still converges the membership");

  let saw = R::timeout(Duration::from_secs(2), async {
    loop {
      match quiet_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "old-news" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .unwrap_or(false);
  assert!(
    !saw,
    "an ignore_old join must not replay the seed's pre-join user events"
  );

  quiet.shutdown().await.expect("q-io-quiet shuts down");
  plain.shutdown().await.expect("q-io-plain shuts down");
  seed.shutdown().await.expect("q-io-seed shuts down");
}

/// A datagram the gossip plane cannot parse is DROPPED and the node keeps serving.
/// On QUIC the shared UDP socket also carries quinn's own packets, so the ingress
/// must shrug off junk on BOTH demux paths without poisoning the pump.
async fn malformed_gossip_datagram_is_ignored<R>()
where
  R: Runtime,
{
  let a = spawn_node::<R>("q-junk-a").await;
  let a_addr = a.advertise_address();

  let raw = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a raw sender");
  for payload in [
    &b"\xff\xff\xff\xff\xff\xff\xff\xff"[..],
    &b""[..],
    &[0x7fu8; 400][..],
  ] {
    raw
      .send_to(payload, a_addr)
      .expect("the garbage datagram is sent");
  }

  let b = spawn_node::<R>("q-junk-b").await;
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("the node still serves joins after the malformed datagrams");
  converge(&a, &b).await;

  a.shutdown().await.expect("q-junk-a shuts down");
  b.shutdown().await.expect("q-junk-b shuts down");
}

/// A subscriber that stops draining its `EventStream` must NOT stall the QUIC
/// driver: the fan-out sheds the events it cannot deliver and COUNTS them, and the
/// node keeps serving.
async fn slow_subscriber_sheds_events_and_counts_them<R>()
where
  R: Runtime,
{
  let node = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-shed-a"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new().with_event_queue_cap(1),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn q-shed-a");

  // Subscribe and NEVER poll the stream: the fan-out queue fills at once.
  let _stalled = node.events();

  for i in 0..64u32 {
    node
      .user_event("flood", Bytes::from(i.to_be_bytes().to_vec()), false)
      .await
      .expect("the driver keeps accepting commands while the subscriber stalls");
  }

  R::timeout(Duration::from_secs(20), async {
    loop {
      if node.events_dropped() > 0 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the fan-out sheds events a stalled subscriber cannot take, and counts them");

  assert_eq!(node.num_members(), 1);
  node.shutdown().await.expect("q-shed-a shuts down");
}

/// A delegate hook that PARKS must not wedge the QUIC pump: the observation channel
/// backs up, the pump retains what application data it can (a bounded overflow),
/// and once THAT is full it sheds the excess and COUNTS the loss — while the node
/// keeps accepting commands throughout.
async fn stalling_delegate_sheds_observations_and_counts_them<R>()
where
  R: Runtime,
{
  /// Enough payload events to fill the two-slot channel, then the pump's bounded
  /// retry overflow (1024 entries), and still leave a surplus that must be shed.
  const FLOOD: u32 = 1200;

  /// A delegate whose user-event hook parks until `gate`'s sender is dropped. It
  /// does NOT override the test-only message-dropper hook, so the composite's
  /// default (drop nothing) applies.
  struct StallingDelegate {
    gate: flume::Receiver<()>,
  }

  impl serf_reactor::MemberDelegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }
  impl serf_reactor::QueryDelegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }
  impl serf_reactor::UserEventDelegate for StallingDelegate {
    fn notify_user_event(
      &self,
      _event: &serf_proto::typed::UserEventMessage,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
      let gate = self.gate.clone();
      async move {
        // Ignoring Err: a disconnected gate means the test released the hook.
        let _ = gate.recv_async().await;
      }
    }
  }
  impl serf_reactor::Delegate for StallingDelegate {
    type Id = SmolStr;
    type Address = SocketAddr;
  }

  let (release, gate) = flume::bounded::<()>(0);

  let node = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-stall-a"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    StallingDelegate { gate },
    RuntimeOptions::new().with_observation_channel(serf_reactor::Channel::Bounded(2)),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn q-stall-a");

  let payload = Bytes::from(vec![0x5au8; 32]);
  for _ in 0..FLOOD {
    node
      .user_event("flood", payload.clone(), false)
      .await
      .expect("the pump keeps accepting commands while the delegate parks");
  }

  R::timeout(Duration::from_secs(20), async {
    loop {
      if node.observation_dropped() > 0 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the pump sheds observations a parked delegate cannot take, and counts them");

  assert_eq!(node.num_members(), 1);

  drop(release);
  node.shutdown().await.expect("q-stall-a shuts down");
}

/// A shutdown racing an IN-FLIGHT await-result join resolves that join
/// `Err(Shutdown)` — never leaves it parked forever. The seed address is a bound
/// UDP socket that speaks no QUIC, so the handshake never completes and the
/// exchange is still pending inside the driver when the teardown reaps the waiter.
async fn shutdown_racing_an_inflight_join_resolves_it<R>()
where
  R: Runtime,
{
  // A UDP socket that binds the port but never answers a QUIC Initial.
  let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind the silent seed");
  let seed_addr = silent.local_addr().expect("the silent seed's address");

  let node = spawn_node::<R>("q-race-join").await;

  let (join, shutdown) = future::join(
    node.join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed_addr),
      false,
    ),
    async {
      // Let the Initial go out before the teardown begins, so the waiter is
      // genuinely in flight rather than never dispatched.
      R::sleep(Duration::from_millis(200)).await;
      node.shutdown().await
    },
  )
  .await;

  shutdown.expect("the node shuts down");
  let err = join.expect_err("a join racing a shutdown cannot succeed against a silent seed");
  assert!(
    matches!(
      err,
      serf_reactor::SerfError::Shutdown | serf_reactor::SerfError::JoinAllFailed(_)
    ),
    "the in-flight join must be resolved by the teardown, not stranded (got {err:?})"
  );
}

/// A second `shutdown()` — issued once the driver has already exited and closed
/// its command queue — still resolves `Ok`, and only AFTER the bind address is
/// actually free: the late caller parks on the teardown-completion latch instead
/// of returning into a still-bound port.
async fn second_shutdown_awaits_teardown_completion<R>()
where
  R: Runtime,
{
  let node = spawn_node::<R>("q-twice-a").await;
  let addr = node.advertise_address();

  node.shutdown().await.expect("the first shutdown resolves");
  node
    .shutdown()
    .await
    .expect("a second shutdown after teardown still resolves Ok");

  let err = node
    .user_event("post", Bytes::from_static(b"x"), false)
    .await
    .expect_err("a shut-down node accepts no commands");
  assert!(
    matches!(err, serf_reactor::SerfError::Shutdown),
    "a post-shutdown command reports Shutdown, got {err:?}"
  );

  // The latch fired only once the bind address was free: rebinding it succeeds.
  let reborn = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-twice-b"))
      .with_advertise_addr(MaybeResolved::Resolved(addr))
      .with_quic_config(test_quic_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .expect("the freed address rebinds after the awaited teardown");
  assert_eq!(reborn.advertise_address(), addr);
  reborn.shutdown().await.expect("q-twice-b shuts down");
}

/// The fire-and-forget `dispatch_join` reports how many seeds it DISPATCHED
/// without waiting for any of them, and the membership it started still converges
/// through the driver's own exchange completion. A dispatch on a LEFT node is
/// refused rather than silently swallowed.
async fn dispatch_join_reports_the_dispatched_seed_count<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("q-dj-b").await;
  let a = spawn_node::<R>("q-dj-a").await;
  let b_addr = b.advertise_address();
  let blackhole: SocketAddr = "127.0.0.1:7219".parse().expect("loopback addr");

  let dispatched = a
    .dispatch_join(
      &SocketAddrResolver,
      &[
        MaybeResolved::Resolved(b_addr),
        MaybeResolved::Resolved(blackhole),
      ],
    )
    .await
    .expect("the join dispatches");
  assert_eq!(
    dispatched, 2,
    "dispatch_join counts every seed it started an exchange against, reachable or not"
  );

  // The reachable seed still merges — the fire-and-forget join is real, not a no-op.
  converge(&a, &b).await;

  // A dispatch on a left node is refused.
  a.leave().await.expect("A leaves the cluster");
  let err = a
    .dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(b_addr)])
    .await
    .expect_err("a left node cannot dispatch a join");
  assert!(
    matches!(err, serf_reactor::SerfError::NotRunning),
    "a post-leave dispatch_join reports NotRunning, got {err:?}"
  );

  a.shutdown().await.expect("q-dj-a shuts down");
  b.shutdown().await.expect("q-dj-b shuts down");
}

/// Every SWIM override the QUIC transport options carry is threaded into the
/// coordinator the driver builds. Two nodes configured with the FULL override set
/// — including the reclaim window and the suspicion ceiling that no other scenario
/// sets — still join, converge, and detect an abrupt kill, so no override is
/// dropped or mis-wired on the way through `Transport::run`.
async fn full_swim_override_set_is_threaded_into_the_coordinator<R>()
where
  R: Runtime,
{
  async fn spawn_tuned<R>(id: &str) -> Node<R>
  where
    R: Runtime,
  {
    Serf::<SmolStr, SocketAddr, R>::quic(
      QuicTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
        .with_quic_config(test_quic_options())
        .with_probe_interval(Duration::from_millis(200))
        .with_probe_timeout(Duration::from_millis(150))
        .with_gossip_interval(Duration::from_millis(20))
        .with_suspicion_mult(3)
        .with_suspicion_max_timeout_mult(4)
        .with_dead_node_reclaim_time(Duration::from_millis(1))
        .with_push_pull_interval(Duration::from_millis(500)),
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(VoidKeyringDelegate),
    )
    .await
    .expect("spawn a fully-tuned serf quic node")
  }

  let b = spawn_tuned::<R>("q-tuned-b").await;
  let a = spawn_tuned::<R>("q-tuned-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("q-tuned-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("the fully-tuned nodes still join");
  converge(&a, &b).await;

  // The tuned failure detection still fires: an abrupt kill is detected.
  b.shutdown().await.expect("q-tuned-b shuts down abruptly");
  R::timeout(Duration::from_secs(30), async {
    loop {
      let failed = a
        .members()
        .iter()
        .any(|m| m.node().id_ref() == &b_id && m.status() != MemberStatus::Alive);
      if failed {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the tuned SWIM overrides still detect the killed peer");

  a.shutdown().await.expect("q-tuned-a shuts down");
}

/// A resolver that resolves the advertise address to NO candidate fails
/// construction rather than booting a QUIC node with no reachable contact.
async fn advertise_resolution_failure_fails_construction<R>()
where
  R: Runtime,
{
  /// Resolves nothing — a bootstrap outage the advertise picker must refuse.
  struct EmptyResolver;
  impl serf_reactor::Resolver for EmptyResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;
    fn resolve(
      &self,
      _addr: &SocketAddr,
    ) -> impl core::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + '_ {
      // The candidate set is empty whatever the input, so the future borrows nothing.
      let out = Vec::new();
      async move { Ok(out) }
    }
  }

  let err = Serf::<SmolStr, SocketAddr, R>::quic(
    QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("q-unresolvable"))
      .with_advertise_addr(MaybeResolved::Unresolved(ephemeral_bind()))
      .with_quic_config(test_quic_options()),
    &EmptyResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(VoidKeyringDelegate),
  )
  .await
  .err()
  .expect("an advertise address that resolves to nothing cannot boot a node");
  assert!(
    matches!(err, serf_reactor::SerfError::Resolve(_)),
    "an empty candidate set is a resolution failure, got {err:?}"
  );
}

/// A key-management request on a node with NO keyring is REFUSED, not silently
/// applied: the responder answers `result = false` with an explanatory message and
/// leaves the wire untouched, so the originator's collected response carries the
/// error rather than a false success.
#[cfg(encryption)]
async fn key_op_without_a_keyring_is_refused<R>()
where
  R: Runtime,
{
  // Two PLAINTEXT nodes — neither carries a gossip keyring.
  let b = spawn_node::<R>("q-nokey-b").await;
  let a = spawn_node::<R>("q-nokey-a").await;
  let b_addr = b.advertise_address();

  let mut a_events = a.events();
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B");
  converge(&a, &b).await;

  a.install_key(test_secret_key(0x78))
    .await
    .expect("the request itself dispatches");

  let kr = R::timeout(Duration::from_secs(30), async {
    loop {
      match a_events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("the event stream ended before the key response"),
      }
    }
  })
  .await
  .expect("A collects the key response within the query window");

  assert!(
    kr.num_err > 0,
    "a node with no keyring must REFUSE the key op, not report success (num_err={}, num_resp={})",
    kr.num_err,
    kr.num_resp
  );
  assert!(
    !kr.messages.is_empty(),
    "the refusal carries an explanatory message"
  );
  assert!(
    !a.encryption_enabled() && !b.encryption_enabled(),
    "the refused op left both nodes plaintext"
  );

  a.shutdown().await.expect("q-nokey-a shuts down");
  b.shutdown().await.expect("q-nokey-b shuts down");
}

// The tokio cells: the runtime-generic scenarios driven on tokio's multi-thread
// runtime. Gated on the `tokio` feature so the `--test quic -- smol` build (which
// enables only `smol`) can drop the `agnostic/tokio` code path.
#[cfg(feature = "tokio")]
mod tokio_cells {
  use agnostic::tokio::TokioRuntime;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn two_node_quic_join_converges() {
    super::two_node_quic_join_converges::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn concurrent_leaves_share_one_in_flight_leave() {
    super::concurrent_leaves_share_one_in_flight_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn ignore_old_join_suppresses_the_replay() {
    super::ignore_old_join_suppresses_the_replay::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn malformed_gossip_datagram_is_ignored() {
    super::malformed_gossip_datagram_is_ignored::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn slow_subscriber_sheds_events_and_counts_them() {
    super::slow_subscriber_sheds_events_and_counts_them::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn stalling_delegate_sheds_observations_and_counts_them() {
    super::stalling_delegate_sheds_observations_and_counts_them::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn shutdown_racing_an_inflight_join_resolves_it() {
    super::shutdown_racing_an_inflight_join_resolves_it::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn second_shutdown_awaits_teardown_completion() {
    super::second_shutdown_awaits_teardown_completion::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn key_op_without_a_keyring_is_refused() {
    super::key_op_without_a_keyring_is_refused::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn dispatch_join_reports_the_dispatched_seed_count() {
    super::dispatch_join_reports_the_dispatched_seed_count::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn full_swim_override_set_is_threaded_into_the_coordinator() {
    super::full_swim_override_set_is_threaded_into_the_coordinator::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn advertise_resolution_failure_fails_construction() {
    super::advertise_resolution_failure_fails_construction::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_events_failed() {
    super::serf_events_failed::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_events_leave() {
    super::serf_events_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_reconnect() {
    super::serf_reconnect::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_force_leave_failed() {
    super::serf_force_leave_failed::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_remove_failed_node_prune_erases() {
    super::serf_remove_failed_node_prune_erases::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_set_tags_propagates() {
    super::serf_set_tags_propagates::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_join_leave() {
    super::serf_join_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn join_many_returns_only_reached_seeds() {
    super::join_many_returns_only_reached_seeds::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn remove_failed_node_absent_is_a_noop() {
    super::remove_failed_node_absent_is_a_noop::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn post_leave_operations_report_not_running() {
    super::post_leave_operations_report_not_running::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn post_leave_key_operations_report_not_running() {
    super::post_leave_key_operations_report_not_running::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn key_rotation_lifecycle() {
    super::key_rotation_lifecycle::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn udp_mode_gossip_bypasses_the_datagram_plane() {
    super::udp_mode_gossip_bypasses_the_datagram_plane::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn construction_requires_id_advertise_and_quic_config() {
    super::construction_requires_id_advertise_and_quic_config::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn wildcard_advertise_is_refused_and_releases_the_bind() {
    super::wildcard_advertise_is_refused_and_releases_the_bind::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn unresolved_advertise_addr_is_resolved_at_construction() {
    super::unresolved_advertise_addr_is_resolved_at_construction::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn merge_delegate_is_consulted_on_join() {
    super::merge_delegate_is_consulted_on_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_forwarders_reflect_joined_cluster() {
    super::snapshot_forwarders_reflect_joined_cluster::<TokioRuntime>().await;
  }

  #[cfg(feature = "coordinates")]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn coordinates_surface_on_the_handle() {
    super::coordinates_surface_on_the_handle::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_leave_gate_controls_rejoin() {
    super::snapshot_leave_gate_controls_rejoin::<TokioRuntime>().await;
  }

  #[cfg(all(unix, encryption))]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn file_backed_rotation_gates_the_response_on_persistence() {
    super::file_backed_rotation_gates_the_response_on_persistence::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn user_event_delivered() {
    super::user_event_delivered::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn datagram_mode_gossip_rides_quic_datagrams() {
    super::datagram_mode_gossip_rides_quic_datagrams::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn query_round_trip() {
    super::query_round_trip::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn leave_emits_left_cluster() {
    super::leave_emits_left_cluster::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn leave_with_racing_shutdown_reaches_peer_as_leave() {
    super::leave_with_racing_shutdown_reaches_peer_as_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn quic_shutdown_releases_bound_address_for_rebind() {
    super::quic_shutdown_releases_bound_address_for_rebind::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn two_node_quic_gossip_convergence_encrypted() {
    super::two_node_quic_gossip_convergence_encrypted::<TokioRuntime>().await;
  }

  #[cfg(encryption)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn mismatched_keyring_gossip_does_not_cross() {
    super::mismatched_keyring_gossip_does_not_cross::<TokioRuntime>().await;
  }
}

// The smol cells: the identical scenarios instantiated over `SmolRuntime` and
// driven by smol's `block_on`. The reactor QUIC poll task runs on smol's global
// executor, so the same scenario bodies verify the driver under a second runtime.
// `cargo test --test quic -- smol` selects exactly these.
#[cfg(feature = "smol")]
mod smol_cells {
  use agnostic::{RuntimeLite, smol::SmolRuntime};

  #[test]
  fn snapshot_leave_gate_controls_rejoin_smol() {
    SmolRuntime::block_on(super::snapshot_leave_gate_controls_rejoin::<SmolRuntime>());
  }

  #[cfg(all(unix, encryption))]
  #[test]
  fn file_backed_rotation_gates_the_response_on_persistence_smol() {
    SmolRuntime::block_on(
      super::file_backed_rotation_gates_the_response_on_persistence::<SmolRuntime>(),
    );
  }

  #[test]
  fn two_node_quic_join_converges_smol() {
    SmolRuntime::block_on(super::two_node_quic_join_converges::<SmolRuntime>());
  }

  #[test]
  fn user_event_delivered_smol() {
    SmolRuntime::block_on(super::user_event_delivered::<SmolRuntime>());
  }

  #[test]
  fn datagram_mode_gossip_rides_quic_datagrams_smol() {
    SmolRuntime::block_on(super::datagram_mode_gossip_rides_quic_datagrams::<
      SmolRuntime,
    >());
  }

  #[test]
  fn query_round_trip_smol() {
    SmolRuntime::block_on(super::query_round_trip::<SmolRuntime>());
  }

  #[test]
  fn leave_emits_left_cluster_smol() {
    SmolRuntime::block_on(super::leave_emits_left_cluster::<SmolRuntime>());
  }

  #[test]
  fn leave_with_racing_shutdown_reaches_peer_as_leave_smol() {
    SmolRuntime::block_on(super::leave_with_racing_shutdown_reaches_peer_as_leave::<
      SmolRuntime,
    >());
  }

  #[test]
  fn quic_shutdown_releases_bound_address_for_rebind_smol() {
    SmolRuntime::block_on(super::quic_shutdown_releases_bound_address_for_rebind::<
      SmolRuntime,
    >());
  }

  #[cfg(encryption)]
  #[test]
  fn two_node_quic_gossip_convergence_encrypted_smol() {
    SmolRuntime::block_on(super::two_node_quic_gossip_convergence_encrypted::<
      SmolRuntime,
    >());
  }

  #[cfg(encryption)]
  #[test]
  fn mismatched_keyring_gossip_does_not_cross_smol() {
    SmolRuntime::block_on(super::mismatched_keyring_gossip_does_not_cross::<SmolRuntime>());
  }

  #[test]
  fn serf_events_failed_smol() {
    SmolRuntime::block_on(super::serf_events_failed::<SmolRuntime>());
  }

  #[test]
  fn serf_events_leave_smol() {
    SmolRuntime::block_on(super::serf_events_leave::<SmolRuntime>());
  }

  #[test]
  fn serf_reconnect_smol() {
    SmolRuntime::block_on(super::serf_reconnect::<SmolRuntime>());
  }

  #[test]
  fn serf_force_leave_failed_smol() {
    SmolRuntime::block_on(super::serf_force_leave_failed::<SmolRuntime>());
  }

  #[test]
  fn serf_remove_failed_node_prune_erases_smol() {
    SmolRuntime::block_on(super::serf_remove_failed_node_prune_erases::<SmolRuntime>());
  }

  #[test]
  fn serf_set_tags_propagates_smol() {
    SmolRuntime::block_on(super::serf_set_tags_propagates::<SmolRuntime>());
  }

  #[test]
  fn serf_join_leave_smol() {
    SmolRuntime::block_on(super::serf_join_leave::<SmolRuntime>());
  }

  #[test]
  fn join_many_returns_only_reached_seeds_smol() {
    SmolRuntime::block_on(super::join_many_returns_only_reached_seeds::<SmolRuntime>());
  }

  #[test]
  fn remove_failed_node_absent_is_a_noop_smol() {
    SmolRuntime::block_on(super::remove_failed_node_absent_is_a_noop::<SmolRuntime>());
  }

  #[test]
  fn post_leave_operations_report_not_running_smol() {
    SmolRuntime::block_on(super::post_leave_operations_report_not_running::<SmolRuntime>());
  }

  #[cfg(encryption)]
  #[test]
  fn post_leave_key_operations_report_not_running_smol() {
    SmolRuntime::block_on(super::post_leave_key_operations_report_not_running::<
      SmolRuntime,
    >());
  }

  #[cfg(encryption)]
  #[test]
  fn key_rotation_lifecycle_smol() {
    SmolRuntime::block_on(super::key_rotation_lifecycle::<SmolRuntime>());
  }

  #[test]
  fn udp_mode_gossip_bypasses_the_datagram_plane_smol() {
    SmolRuntime::block_on(super::udp_mode_gossip_bypasses_the_datagram_plane::<
      SmolRuntime,
    >());
  }

  #[test]
  fn construction_requires_id_advertise_and_quic_config_smol() {
    SmolRuntime::block_on(super::construction_requires_id_advertise_and_quic_config::<
      SmolRuntime,
    >());
  }

  #[test]
  fn wildcard_advertise_is_refused_and_releases_the_bind_smol() {
    SmolRuntime::block_on(
      super::wildcard_advertise_is_refused_and_releases_the_bind::<SmolRuntime>(),
    );
  }

  #[test]
  fn unresolved_advertise_addr_is_resolved_at_construction_smol() {
    SmolRuntime::block_on(
      super::unresolved_advertise_addr_is_resolved_at_construction::<SmolRuntime>(),
    );
  }

  #[test]
  fn merge_delegate_is_consulted_on_join_smol() {
    SmolRuntime::block_on(super::merge_delegate_is_consulted_on_join::<SmolRuntime>());
  }

  #[test]
  fn snapshot_forwarders_reflect_joined_cluster_smol() {
    SmolRuntime::block_on(super::snapshot_forwarders_reflect_joined_cluster::<
      SmolRuntime,
    >());
  }

  #[cfg(feature = "coordinates")]
  #[test]
  fn coordinates_surface_on_the_handle_smol() {
    SmolRuntime::block_on(super::coordinates_surface_on_the_handle::<SmolRuntime>());
  }

  #[test]
  fn concurrent_leaves_share_one_in_flight_leave_smol() {
    SmolRuntime::block_on(super::concurrent_leaves_share_one_in_flight_leave::<
      SmolRuntime,
    >());
  }

  #[test]
  fn ignore_old_join_suppresses_the_replay_smol() {
    SmolRuntime::block_on(super::ignore_old_join_suppresses_the_replay::<SmolRuntime>());
  }

  #[test]
  fn malformed_gossip_datagram_is_ignored_smol() {
    SmolRuntime::block_on(super::malformed_gossip_datagram_is_ignored::<SmolRuntime>());
  }

  #[test]
  fn slow_subscriber_sheds_events_and_counts_them_smol() {
    SmolRuntime::block_on(super::slow_subscriber_sheds_events_and_counts_them::<
      SmolRuntime,
    >());
  }

  #[test]
  fn stalling_delegate_sheds_observations_and_counts_them_smol() {
    SmolRuntime::block_on(
      super::stalling_delegate_sheds_observations_and_counts_them::<SmolRuntime>(),
    );
  }

  #[test]
  fn shutdown_racing_an_inflight_join_resolves_it_smol() {
    SmolRuntime::block_on(super::shutdown_racing_an_inflight_join_resolves_it::<
      SmolRuntime,
    >());
  }

  #[test]
  fn second_shutdown_awaits_teardown_completion_smol() {
    SmolRuntime::block_on(super::second_shutdown_awaits_teardown_completion::<
      SmolRuntime,
    >());
  }

  #[cfg(encryption)]
  #[test]
  fn key_op_without_a_keyring_is_refused_smol() {
    SmolRuntime::block_on(super::key_op_without_a_keyring_is_refused::<SmolRuntime>());
  }

  #[test]
  fn dispatch_join_reports_the_dispatched_seed_count_smol() {
    SmolRuntime::block_on(super::dispatch_join_reports_the_dispatched_seed_count::<
      SmolRuntime,
    >());
  }

  #[test]
  fn full_swim_override_set_is_threaded_into_the_coordinator_smol() {
    SmolRuntime::block_on(
      super::full_swim_override_set_is_threaded_into_the_coordinator::<SmolRuntime>(),
    );
  }

  #[test]
  fn advertise_resolution_failure_fails_construction_smol() {
    SmolRuntime::block_on(super::advertise_resolution_failure_fails_construction::<
      SmolRuntime,
    >());
  }
}
