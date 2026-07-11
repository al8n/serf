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
use serf_proto::{event::Event, members::SerfState, options::Options as SerfOptions};
#[cfg(encryption)]
use serf_reactor::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, QuicOptions, QuicTransportOptions, RuntimeOptions, Serf,
  SocketAddrResolver, VoidDelegate,
};
use smol_str::SmolStr;

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
/// handshake) and datagram-mode unreliable transport. A fresh bundle is built per
/// node so each owns its own cert and quinn endpoint config.
fn test_quic_options() -> QuicOptions {
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
    UnreliableTransport::Datagram,
  )
}

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
}
