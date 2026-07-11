//! Real-node TLS serf tests: two loopback nodes exercising the reactor stream
//! driver with the rustls record layer end-to-end. Each test spins up ephemeral
//! `127.0.0.1:0` nodes via the ergonomic [`Serf::tls`] constructor and drives the
//! full pump — TLS handshake-on-dial, join push/pull, coordinator merge, gossip,
//! user events, queries, and graceful leave/shutdown.
//!
//! The scenario bodies are runtime-generic `async fn <R: Runtime>` helpers, so the
//! SAME scenario runs as a `#[tokio::test]` cell over `TokioRuntime` and as a
//! `_smol` cell driven by `SmolRuntime::block_on` — mirroring memberlist-reactor's
//! runtime-parameterized suite.
//!
//! Mirrors `tests/tcp.rs` (TLS rides the same stream driver as plain TCP, differing
//! only in the record layer) and serf-compio's / memberlist-reactor's TLS harness:
//! each node presents a fresh self-signed localhost-SAN cert and the client side
//! accepts whatever the server presents, so the handshake completes without a real
//! trust anchor. The default SNI provider (`Some("localhost")`) matches the cert
//! SAN.

#![cfg(feature = "tls")]

use core::time::Duration;
use std::{net::SocketAddr, sync::Arc};

use agnostic::Runtime;
use bytes::Bytes;
use futures_util::{StreamExt, future};
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  crypto::CryptoProvider,
  pki_types::CertificateDer,
  version::TLS13,
};
use serf_proto::{event::Event, members::SerfState, options::Options as SerfOptions};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TlsOptions,
  TlsTransportOptions, VoidDelegate,
};
use smol_str::SmolStr;

/// A reactor TLS node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// Accept-any server-cert verifier for the loopback tests.
///
/// Each node presents its own fresh self-signed localhost-SAN cert; the client side
/// accepts whatever the server presents so the handshake completes without a real
/// trust anchor. NEVER use this outside a test.
#[derive(Debug)]
struct AcceptAnyServer(Arc<CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
  fn verify_server_cert(
    &self,
    _e: &CertificateDer<'_>,
    _i: &[CertificateDer<'_>],
    _n: &rustls::pki_types::ServerName<'_>,
    _o: &[u8],
    _t: rustls::pki_types::UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }
  fn verify_tls12_signature(
    &self,
    _m: &[u8],
    _c: &CertificateDer<'_>,
    _d: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }
  fn verify_tls13_signature(
    &self,
    _m: &[u8],
    _c: &CertificateDer<'_>,
    _d: &rustls::DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    Ok(HandshakeSignatureValid::assertion())
  }
  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    self.0.signature_verification_algorithms.supported_schemes()
  }
}

fn crypto_provider() -> Arc<CryptoProvider> {
  CryptoProvider::get_default()
    .cloned()
    .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// Build a self-signed localhost-SAN `ServerConfig` + accept-any `ClientConfig`
/// bundle. A fresh bundle is built per node so each owns its own cert.
fn test_tls_options() -> TlsOptions {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()])
    .expect("rcgen generate_simple_self_signed");
  let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
  let key = rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());

  let provider = crypto_provider();

  let server_cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .expect("valid self-signed cert");

  let client_cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnyServer(provider)))
    .with_no_client_auth();

  TlsOptions::new(server_cfg, client_cfg)
}

/// Build and spawn a reactor TLS node on an ephemeral loopback port through the
/// ergonomic `Serf::tls` constructor. The default SNI provider (`Some("localhost")`)
/// matches the self-signed cert SAN.
async fn spawn_node<R>(id: &str) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_tls_options(test_tls_options());
  Serf::<SmolStr, SocketAddr, R>::tls(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf tls node")
}

/// Poll both nodes until each reports the full two-member cluster, or fail on a
/// generous timeout so a convergence regression surfaces as a timeout, not a hang.
async fn converge<R>(a: &Node<R>, b: &Node<R>)
where
  R: Runtime,
{
  R::timeout(Duration::from_secs(20), async {
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

/// Two nodes on loopback: A joins B over a real TLS push-pull exchange, then BOTH
/// converge to a two-member cluster and shut down cleanly.
async fn two_node_tls_join_converges<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("conv-b").await;
  let a = spawn_node::<R>("conv-a").await;
  let b_addr = b.advertise_address();

  let reached = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over TLS");
  assert_eq!(reached, b_addr, "join returns the reached seed address");

  converge(&a, &b).await;
  assert_eq!(a.num_members(), 2, "A sees the 2-member cluster");
  assert_eq!(b.num_members(), 2, "B sees the 2-member cluster");

  a.shutdown().await.expect("conv-a shuts down");
  b.shutdown().await.expect("conv-b shuts down");
}

/// After a two-node TLS join, a user event broadcast by B is delivered to A's event
/// stream carrying the original name and payload.
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

  let got = R::timeout(Duration::from_secs(20), async {
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

/// After a two-node TLS join, a query issued by A round-trips: B receives the
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

  let got = R::timeout(Duration::from_secs(20), async {
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
  let saw = R::timeout(Duration::from_secs(20), async {
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

// The tokio cells: the runtime-generic scenarios driven on tokio's multi-thread
// runtime. Gated on the `tokio` feature so the `--test tls -- smol` build (which
// enables only `smol`) can drop the `agnostic/tokio` code path.
#[cfg(feature = "tokio")]
mod tokio_cells {
  use agnostic::tokio::TokioRuntime;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn two_node_tls_join_converges() {
    super::two_node_tls_join_converges::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn user_event_delivered() {
    super::user_event_delivered::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn query_round_trip() {
    super::query_round_trip::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn leave_emits_left_cluster() {
    super::leave_emits_left_cluster::<TokioRuntime>().await;
  }
}

// The smol cells: the identical scenarios instantiated over `SmolRuntime` and
// driven by smol's `block_on`. `cargo test --test tls -- smol` selects exactly
// these.
#[cfg(feature = "smol")]
mod smol_cells {
  use agnostic::{RuntimeLite, smol::SmolRuntime};

  #[test]
  fn two_node_tls_join_converges_smol() {
    SmolRuntime::block_on(super::two_node_tls_join_converges::<SmolRuntime>());
  }

  #[test]
  fn user_event_delivered_smol() {
    SmolRuntime::block_on(super::user_event_delivered::<SmolRuntime>());
  }

  #[test]
  fn query_round_trip_smol() {
    SmolRuntime::block_on(super::query_round_trip::<SmolRuntime>());
  }

  #[test]
  fn leave_emits_left_cluster_smol() {
    SmolRuntime::block_on(super::leave_emits_left_cluster::<SmolRuntime>());
  }
}
