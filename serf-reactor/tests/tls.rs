//! Real-node TLS serf tests: loopback nodes exercising the reactor stream driver
//! with the rustls record layer end-to-end. Each test spins up ephemeral
//! `127.0.0.1:0` nodes via the ergonomic [`Serf::tls`] constructor and drives the
//! full pump — TLS handshake-on-dial, join push/pull, coordinator merge, gossip,
//! user events, queries, membership fault injection, and graceful leave/shutdown.
//!
//! The scenario bodies are runtime-generic `async fn <R: Runtime>` helpers, so the
//! SAME scenario runs as a `#[tokio::test]` cell over `TokioRuntime` and as a
//! `_smol` cell driven by `SmolRuntime::block_on` — mirroring memberlist-reactor's
//! runtime-parameterized suite. The multi-node fault-injection scenarios drive the
//! shared `cluster` fixture through a TLS [`cluster::Backend`], so the same bodies
//! that pin the TCP membership lifecycle pin it over the TLS record layer.
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
  RootCertStore,
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  crypto::CryptoProvider,
  pki_types::CertificateDer,
  version::TLS13,
};
use serf_proto::{
  event::{Event, MemberEventKind},
  members::{MemberStatus, SerfState},
  options::Options as SerfOptions,
};
use serf_reactor::{
  FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver, TlsOptions,
  TlsTransportOptions, VoidDelegate,
};
use smol_str::SmolStr;

/// The reusable multi-node fault-injection fixture, shared with the TCP and QUIC
/// suites.
mod cluster;

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

/// A fresh self-signed localhost-SAN cert + key, and the DER the client side needs
/// to pin it as a trust anchor.
fn self_signed() -> (
  Vec<CertificateDer<'static>>,
  rustls::pki_types::PrivateKeyDer<'static>,
) {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()])
    .expect("rcgen generate_simple_self_signed");
  let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
  let key = rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
  (chain, key)
}

/// Build a self-signed localhost-SAN `ServerConfig` + accept-any `ClientConfig`
/// bundle. A fresh bundle is built per node so each owns its own cert.
fn test_tls_options() -> TlsOptions {
  let (chain, key) = self_signed();
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

/// A bundle whose CLIENT side trusts exactly one root: its OWN self-signed cert.
/// A node built from it can serve peers, but can only complete an outbound
/// handshake against a peer presenting that same cert — the trust-anchor boundary
/// the accept-any bundle above deliberately waives.
fn self_trusting_tls_options() -> TlsOptions {
  let (chain, key) = self_signed();
  let provider = crypto_provider();

  let server_cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_no_client_auth()
    .with_single_cert(chain.clone(), key)
    .expect("valid self-signed cert");

  let mut roots = RootCertStore::empty();
  roots
    .add(chain[0].clone())
    .expect("its own cert is a valid root");
  let client_cfg = rustls::ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_root_certificates(roots)
    .with_no_client_auth();

  TlsOptions::new(server_cfg, client_cfg)
}

/// An ephemeral loopback bind (`127.0.0.1:0`).
fn ephemeral_bind() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

/// The fixture's TLS backend: the fast-SWIM timing mapped onto a
/// [`TlsTransportOptions`] block carrying a fresh per-node cert bundle.
struct Tls;

impl<R> cluster::Backend<R> for Tls
where
  R: Runtime,
{
  async fn build(
    id: &str,
    bind: SocketAddr,
    timing: &cluster::ClusterTiming,
  ) -> serf_reactor::Result<cluster::Node<R>> {
    let mut opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(bind))
      .with_tls_options(test_tls_options())
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
    Serf::<SmolStr, SocketAddr, R>::tls(
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
      std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
    )
    .await
  }
}

/// The TLS fault-injection cluster.
type TlsCluster<R> = cluster::Cluster<R, Tls>;

/// Build and spawn a reactor TLS node on an ephemeral loopback port through the
/// ergonomic `Serf::tls` constructor. The default SNI provider (`Some("localhost")`)
/// matches the self-signed cert SAN.
async fn spawn_node<R>(id: &str) -> Node<R>
where
  R: Runtime,
{
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
    .with_tls_options(test_tls_options());
  Serf::<SmolStr, SocketAddr, R>::tls(
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

/// A tls node with snapshot persistence configured.
async fn spawn_node_with_snapshot<R>(
  id: &str,
  snapshot: serf_reactor::SnapshotOptions,
  rejoin_after_leave: bool,
) -> Node<R>
where
  R: Runtime,
{
  Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_tls_options(test_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new().with_rejoin_after_leave(rejoin_after_leave),
    None,
    None,
    Some(snapshot),
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn snapshot-backed serf tls node")
}

/// The clean-leave gate over the tls transport: the graceful leave persists,
/// the default posture starts fresh on restart, the opt-in posture rejoins.
async fn snapshot_leave_gate_controls_rejoin<R>()
where
  R: Runtime,
{
  let mut path = std::env::temp_dir();
  // Keyed by runtime as well as pid: the tokio and smol cells run
  // concurrently in one test binary and must not share a snapshot file.
  path.push(format!(
    "serf-e2e-tls-snap-leave-gate-{}-{}",
    std::process::id(),
    core::any::type_name::<R>().replace("::", "-"),
  ));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&path);

  let a = spawn_node::<R>("tgate-a").await;
  let b =
    spawn_node_with_snapshot::<R>("tgate-b", serf_reactor::SnapshotOptions::new(&path), false)
      .await;
  let a_addr = a.advertise_address();

  b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
    .await
    .expect("join reaches node A");
  converge(&a, &b).await;

  b.leave().await.expect("tgate-b leaves gracefully");
  b.shutdown().await.expect("tgate-b shuts down");

  // Default posture: the leave clears the recovered state — no auto-rejoin.
  let b2 =
    spawn_node_with_snapshot::<R>("tgate-b", serf_reactor::SnapshotOptions::new(&path), false)
      .await;
  R::sleep(Duration::from_millis(1500)).await;
  assert_eq!(
    b2.num_members(),
    1,
    "a cleanly-left node must not auto-rejoin unless opted in"
  );
  b2.shutdown().await.expect("tgate-b2 shuts down");

  // Opt-in posture: the Leave marker is ignored and the membership recovers.
  let b3 =
    spawn_node_with_snapshot::<R>("tgate-b", serf_reactor::SnapshotOptions::new(&path), true).await;
  converge(&a, &b3).await;

  a.shutdown().await.expect("tgate-a shuts down");
  b3.shutdown().await.expect("tgate-b3 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Over TLS: node A joins node B, then B is abruptly killed. A must observe
/// Join → Failed → Reap about B — a Failed (not a Leave), proving the kill
/// discards the graceful-leave datagram, followed by the reaper removing the
/// failed member under the shortened reconnect timeout. The TLS reliable plane
/// carries the join; the SWIM failure detection that follows rides the plain-UDP
/// gossip plane the TLS transport binds alongside it.
async fn serf_events_failed<R>()
where
  R: Runtime,
{
  let mut cluster = TlsCluster::<R>::spawn(
    &["tls-failed-a", "tls-failed-b"],
    cluster::ClusterTiming::fast(),
  )
  .await;
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

/// Over TLS: a graceful leave reaches the peer as a Leave (never a Failed) and
/// lands the leaver in the observer's Left tombstone view.
async fn serf_events_leave<R>()
where
  R: Runtime,
{
  let mut cluster = TlsCluster::<R>::spawn(
    &["tls-leave-a", "tls-leave-b"],
    cluster::ClusterTiming::fast().with_tombstone_timeout(Duration::from_secs(30)),
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

/// Over TLS: a failed node that returns at the SAME address reconnects rather
/// than being reaped — the survivor's reconnect re-dial re-establishes the TLS
/// session and the member revives (Join → Failed → Join).
async fn serf_reconnect<R>()
where
  R: Runtime,
{
  let mut cluster = TlsCluster::<R>::spawn(
    &["tls-recon-a", "tls-recon-b"],
    cluster::ClusterTiming::fast().with_reconnect_timeout(Duration::from_secs(30)),
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

/// Over TLS: an operator force-leaving a FAILED member transitions it to Left on
/// every surviving node rather than leaving it to linger Failed until the reap.
async fn serf_force_leave_failed<R>()
where
  R: Runtime,
{
  let mut cluster = TlsCluster::<R>::spawn(
    &["tls-fl-a", "tls-fl-b", "tls-fl-c"],
    cluster::ClusterTiming::fast()
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

/// Over TLS: a tag change propagates in both directions — the observer records an
/// Update member event for the setter and its member view carries the new value.
async fn serf_set_tags_propagates<R>()
where
  R: Runtime,
{
  let mut cluster = TlsCluster::<R>::spawn(
    &["tls-tags-a", "tls-tags-b"],
    cluster::ClusterTiming::fast(),
  )
  .await;
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

/// Over TLS: after a peer leaves gracefully, the departure settles on BOTH sides
/// under the default tombstone timeout — the observer reaps it while the leaver
/// holds its own self `Left` tombstone (never a self Reap) beside the still-Alive
/// peer.
async fn serf_join_leave<R>()
where
  R: Runtime,
{
  let mut cluster =
    TlsCluster::<R>::spawn(&["tls-jl-a", "tls-jl-b"], cluster::ClusterTiming::fast()).await;
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

/// The constructor-supplied merge delegate is the predicate the machine consults
/// on the TLS join push/pull: with a recording accept-all delegate installed on B,
/// A's join drives at least one `notify_merge` on B carrying A's node state.
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
      if peers.iter().any(|p| p.id_ref().as_str() == "tls-merge-a") {
        self.saw_peer.fetch_add(1, Ordering::Relaxed);
      }
      true
    }
  }

  let hits = Arc::new(AtomicUsize::new(0));
  let saw_peer = Arc::new(AtomicUsize::new(0));

  let b = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("tls-merge-b"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_tls_options(test_tls_options()),
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
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn tls-merge-b");
  let a = spawn_node::<R>("tls-merge-a").await;
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

  a.shutdown().await.expect("tls-merge-a shuts down");
  b.shutdown().await.expect("tls-merge-b shuts down");
}

/// An SNI provider that refuses a peer (`None`) aborts the outbound dial BEFORE
/// the handshake, so the await-result join fails and no membership merges. The
/// server side is untouched — B never sees a completed exchange — which is the
/// discriminator against a provider that merely supplied the wrong name (that
/// would fail INSIDE the handshake instead).
async fn sni_provider_refusal_aborts_the_dial<R>()
where
  R: Runtime,
{
  let b = spawn_node::<R>("sni-b").await;
  let b_addr = b.advertise_address();

  let a = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("sni-a"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_tls_options(test_tls_options())
      .with_sni_provider(Box::new(|_| None)),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("a refusing SNI provider still builds a node");

  let outcome = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await;
  assert!(
    outcome.is_err(),
    "an SNI refusal must fail the join, not silently merge (got {outcome:?})"
  );
  assert_eq!(
    a.num_members(),
    1,
    "no peer is admitted when the dial never handshakes"
  );

  a.shutdown().await.expect("sni-a shuts down");
  b.shutdown().await.expect("sni-b shuts down");
}

/// The TLS trust anchor IS the reliable-plane boundary for the DIALER: a node whose
/// client config trusts only its OWN self-signed cert cannot complete a handshake
/// against a peer presenting a different one, so its join fails and neither side
/// merges membership. The paired accept-any nodes (every other scenario here) prove
/// the same join SUCCEEDS once the verifier accepts the peer's cert, so this
/// failure is real verification, not a broken bundle.
async fn untrusted_peer_cert_fails_the_join<R>()
where
  R: Runtime,
{
  // B presents its own fresh self-signed cert; A trusts only A's cert.
  let b = spawn_node::<R>("trust-b").await;
  let b_addr = b.advertise_address();

  let a = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("trust-a"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_tls_options(self_trusting_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("spawn trust-a");

  let outcome = a
    .join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await;
  assert!(
    outcome.is_err(),
    "a peer outside the trust anchor must fail the handshake, not merge (got {outcome:?})"
  );
  assert_eq!(a.num_members(), 1, "the dialer admitted nothing");
  assert_eq!(
    b.num_members(),
    1,
    "the seed never completed an exchange either"
  );

  a.shutdown().await.expect("trust-a shuts down");
  b.shutdown().await.expect("trust-b shuts down");
}

/// The TLS transport's construction gate: each field `TlsTransport::new` requires
/// is refused when absent, so a half-built options block can never bind a socket.
async fn construction_requires_id_advertise_and_tls_options<R>()
where
  R: Runtime,
{
  async fn build<R>(opts: TlsTransportOptions<SmolStr, SocketAddr>) -> serf_reactor::Result<Node<R>>
  where
    R: Runtime,
  {
    Serf::<SmolStr, SocketAddr, R>::tls(
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
      std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
    )
    .await
  }

  // No local id.
  let err = build::<R>(
    TlsTransportOptions::new()
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
      .with_tls_options(test_tls_options()),
  )
  .await
  .err()
  .expect("a node with no id cannot be built");
  assert!(
    matches!(err, serf_reactor::SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "a missing local_id is an InvalidInput, got {err:?}"
  );

  // No advertise address.
  let err = build::<R>(
    TlsTransportOptions::new()
      .with_local_id(SmolStr::new("no-addr"))
      .with_tls_options(test_tls_options()),
  )
  .await
  .err()
  .expect("a node with no advertise address cannot be built");
  assert!(
    matches!(err, serf_reactor::SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "a missing advertise_addr is an InvalidInput, got {err:?}"
  );

  // No TLS bundle: the record layer has no cert/key to run.
  let err = build::<R>(
    TlsTransportOptions::new()
      .with_local_id(SmolStr::new("no-tls"))
      .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind())),
  )
  .await
  .err()
  .expect("a TLS node with no cert bundle cannot be built");
  assert!(
    matches!(err, serf_reactor::SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "a missing tls_options is an InvalidInput, got {err:?}"
  );
}

/// A wildcard advertise address is refused AFTER the bind: the readback keeps the
/// unspecified IP, which peers could not route back to, so construction fails and
/// the bound sockets are released rather than the node joining as an undialable
/// member. The released port is proven free by an immediate successful rebind of
/// the very port the failed attempt had claimed.
async fn wildcard_advertise_is_refused_and_releases_the_bind<R>()
where
  R: Runtime,
{
  // Claim a concrete port through a successful node, then free it, so the wildcard
  // attempt below binds a KNOWN port we can prove was released.
  let probe = spawn_node::<R>("wild-probe").await;
  let port = probe.advertise_address().port();
  probe.shutdown().await.expect("probe shuts down");

  let wildcard: SocketAddr = format!("0.0.0.0:{port}").parse().expect("wildcard addr");
  let err = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("wild"))
      .with_advertise_addr(MaybeResolved::Resolved(wildcard))
      .with_tls_options(test_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .err()
  .expect("a wildcard advertise address is not a routable contact");
  assert!(
    matches!(err, serf_reactor::SerfError::InvalidAdvertiseAddr(_)),
    "a wildcard bind must be refused as an invalid advertise address, got {err:?}"
  );

  // The refused construction released BOTH bound sockets: the same port rebinds.
  let after = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("wild-after"))
      .with_advertise_addr(MaybeResolved::Resolved(
        format!("127.0.0.1:{port}").parse().expect("loopback addr"),
      ))
      .with_tls_options(test_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
  )
  .await
  .expect("the refused construction released the port it had bound");
  assert_eq!(after.advertise_address().port(), port);
  after.shutdown().await.expect("wild-after shuts down");
}

/// Every SWIM override the TLS transport options carry is threaded into the
/// coordinator the driver builds. Two nodes configured with the FULL override set
/// — including the reclaim window and the suspicion ceiling that no other scenario
/// sets — still handshake, join, and converge, so no override is dropped or
/// mis-wired on the way through `Transport::run`.
async fn full_swim_override_set_is_threaded_into_the_coordinator<R>()
where
  R: Runtime,
{
  async fn spawn_tuned<R>(id: &str) -> Node<R>
  where
    R: Runtime,
  {
    Serf::<SmolStr, SocketAddr, R>::tls(
      TlsTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new(id))
        .with_advertise_addr(MaybeResolved::Resolved(ephemeral_bind()))
        .with_tls_options(test_tls_options())
        .with_probe_interval(Duration::from_millis(100))
        .with_probe_timeout(Duration::from_millis(50))
        .with_gossip_interval(Duration::from_millis(20))
        .with_suspicion_mult(3)
        .with_suspicion_max_timeout_mult(4)
        .with_dead_node_reclaim_time(Duration::from_millis(1))
        .with_push_pull_interval(Duration::from_millis(500))
        .with_stream(
          serf_reactor::StreamTransportOptions::new().with_dial_timeout(Duration::from_secs(5)),
        ),
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new(),
      SerfOptions::new(),
      None,
      None,
      None,
      #[cfg(encryption)]
      std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
    )
    .await
    .expect("spawn a fully-tuned serf tls node")
  }

  let b = spawn_tuned::<R>("tls-tuned-b").await;
  let a = spawn_tuned::<R>("tls-tuned-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("tls-tuned-b");

  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("the fully-tuned nodes still handshake and join");
  converge(&a, &b).await;

  // The tuned failure detection still fires: an abrupt kill is detected.
  b.shutdown().await.expect("tls-tuned-b shuts down abruptly");
  R::timeout(Duration::from_secs(20), async {
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

  a.shutdown().await.expect("tls-tuned-a shuts down");
}

/// An UNRESOLVED advertise address is resolved at construction through the
/// caller's resolvers, and the node comes up on the resolved contact.
async fn unresolved_advertise_addr_is_resolved_at_construction<R>()
where
  R: Runtime,
{
  let node = Serf::<SmolStr, SocketAddr, R>::tls(
    TlsTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new("resolve-me"))
      .with_advertise_addr(MaybeResolved::Unresolved(ephemeral_bind()))
      .with_tls_options(test_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(encryption)]
    std::sync::Arc::new(serf_reactor::VoidKeyringDelegate),
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
  node.shutdown().await.expect("resolve-me shuts down");
}

// The tokio cells: the runtime-generic scenarios driven on tokio's multi-thread
// runtime. Gated on the `tokio` feature so the `--test tls -- smol` build (which
// enables only `smol`) can drop the `agnostic/tokio` code path.
#[cfg(feature = "tokio")]
mod tokio_cells {
  use agnostic::tokio::TokioRuntime;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn snapshot_leave_gate_controls_rejoin() {
    super::snapshot_leave_gate_controls_rejoin::<TokioRuntime>().await;
  }

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
  async fn serf_set_tags_propagates() {
    super::serf_set_tags_propagates::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn serf_join_leave() {
    super::serf_join_leave::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn merge_delegate_is_consulted_on_join() {
    super::merge_delegate_is_consulted_on_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn sni_provider_refusal_aborts_the_dial() {
    super::sni_provider_refusal_aborts_the_dial::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn untrusted_peer_cert_fails_the_join() {
    super::untrusted_peer_cert_fails_the_join::<TokioRuntime>().await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn construction_requires_id_advertise_and_tls_options() {
    super::construction_requires_id_advertise_and_tls_options::<TokioRuntime>().await;
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
  async fn full_swim_override_set_is_threaded_into_the_coordinator() {
    super::full_swim_override_set_is_threaded_into_the_coordinator::<TokioRuntime>().await;
  }
}

// The smol cells: the identical scenarios instantiated over `SmolRuntime` and
// driven by smol's `block_on`. `cargo test --test tls -- smol` selects exactly
// these.
#[cfg(feature = "smol")]
mod smol_cells {
  use agnostic::{RuntimeLite, smol::SmolRuntime};

  #[test]
  fn snapshot_leave_gate_controls_rejoin_smol() {
    SmolRuntime::block_on(super::snapshot_leave_gate_controls_rejoin::<SmolRuntime>());
  }

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
  fn serf_set_tags_propagates_smol() {
    SmolRuntime::block_on(super::serf_set_tags_propagates::<SmolRuntime>());
  }

  #[test]
  fn serf_join_leave_smol() {
    SmolRuntime::block_on(super::serf_join_leave::<SmolRuntime>());
  }

  #[test]
  fn merge_delegate_is_consulted_on_join_smol() {
    SmolRuntime::block_on(super::merge_delegate_is_consulted_on_join::<SmolRuntime>());
  }

  #[test]
  fn sni_provider_refusal_aborts_the_dial_smol() {
    SmolRuntime::block_on(super::sni_provider_refusal_aborts_the_dial::<SmolRuntime>());
  }

  #[test]
  fn untrusted_peer_cert_fails_the_join_smol() {
    SmolRuntime::block_on(super::untrusted_peer_cert_fails_the_join::<SmolRuntime>());
  }

  #[test]
  fn construction_requires_id_advertise_and_tls_options_smol() {
    SmolRuntime::block_on(super::construction_requires_id_advertise_and_tls_options::<
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
  fn full_swim_override_set_is_threaded_into_the_coordinator_smol() {
    SmolRuntime::block_on(
      super::full_swim_override_set_is_threaded_into_the_coordinator::<SmolRuntime>(),
    );
  }
}
