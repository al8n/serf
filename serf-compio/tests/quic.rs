//! Real-node QUIC serf tests: loopback nodes exercising the compio QUIC driver
//! end-to-end over a quinn-proto config bundle.
//!
//! The QUIC pump owns exactly one UDP socket (quinn multiplexes the reliable
//! push/pull streams over it, and serf's datagram gossip rides the same socket),
//! so it has no bridge table and its own command dispatch, key-request handling,
//! snapshot persistence, and teardown drain. This suite drives that whole
//! surface: join/converge, user events, a query round-trip through `respond`,
//! `set_tags`, graceful leave, the not-running gate every command honours after a
//! leave, live-keyring rotation (install/use/remove/list), snapshot persistence
//! and replay, observation-channel backpressure, and the command queue the
//! teardown must answer rather than drop.

#![cfg(feature = "quic")]

use core::{future::Future, pin::Pin, time::Duration};
use std::{cell::RefCell, net::SocketAddr, rc::Rc, sync::Arc};

use bytes::Bytes;
use futures_util::{StreamExt, future};
use memberlist_proto::{MaybeResolved, UnreliableTransport};
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  version::TLS13,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serf_compio::{
  Channel, Delegate, FirstAddrResolver, Ipv4PreferringResolver, MemberDelegate, MergeDelegate,
  QueryDelegate, QuicOptions, QuicTransport, QuicTransportOptions, Resolver, RuntimeOptions, Serf,
  SerfError, SnapshotOptions, SocketAddrResolver, Transport, UserEventDelegate, VoidDelegate,
  gossip_rng,
};
use serf_proto::{
  Tags, UserEventMessage,
  event::{Event, QueryEvent},
  members::SerfState,
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

#[cfg(encryption)]
use serf_compio::{EncryptionOptions, Keyring, KeyringDelegate, SecretKey, VoidKeyringDelegate};

/// Bound on every convergence / delivery poll in this file.
const WINDOW: Duration = Duration::from_secs(45);

/// An ephemeral loopback bind address.
fn loopback_ephemeral() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

// ── the quinn config bundle ───────────────────────────────────────────────────

/// A self-signed cert + key for `localhost`.
fn self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()])
    .expect("rcgen generate_simple_self_signed");
  let cert = CertificateDer::from(ck.cert.der().to_vec());
  let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
  (vec![cert], key)
}

fn test_endpoint_config() -> quinn_proto::EndpointConfig {
  let hmac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[0x5au8; 32]);
  quinn_proto::EndpointConfig::new(Arc::new(hmac))
}

fn test_server() -> quinn_proto::ServerConfig {
  let (chain, key) = self_signed();
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let rustls_server = rustls::ServerConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .expect("valid self-signed cert");
  let qsc = quinn_proto::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))
    .expect("a TLS 1.3 server config is a valid QUIC server config");
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
    .expect("TLS 1.3 supported")
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyServer))
    .with_no_client_auth();
  let qcc = quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(cfg))
    .expect("a TLS 1.3 client config is a valid QUIC client config");
  quinn_proto::ClientConfig::new(Arc::new(qcc))
}

/// A QUIC bundle with an idle timeout well past a localhost handshake and
/// datagram-mode unreliable transport. A fresh bundle is built per node so each
/// owns its own cert and quinn endpoint config.
fn test_quic_options() -> QuicOptions {
  let mut transport = quinn_proto::TransportConfig::default();
  transport.max_idle_timeout(Some(
    quinn_proto::IdleTimeout::try_from(Duration::from_secs(20)).expect("a valid idle timeout"),
  ));
  QuicOptions::new(
    test_endpoint_config(),
    test_server(),
    test_client(),
    transport,
    "localhost",
    UnreliableTransport::Datagram,
  )
}

// ── fixtures ──────────────────────────────────────────────────────────────────

/// A resolver that answers with a dual-stack candidate set (IPv6 first, then
/// IPv4) on the port it was asked for — enough to drive the
/// `MaybeResolved::Unresolved` advertise path AND the advertise picker's
/// narrowing, without depending on the host's name resolution.
struct DualStackResolver;

impl Resolver for DualStackResolver {
  type Address = SocketAddr;
  type Error = std::io::Error;

  async fn resolve(&self, addr: &SocketAddr) -> Result<Vec<SocketAddr>, std::io::Error> {
    Ok(vec![
      SocketAddr::new("::1".parse().expect("v6 loopback"), addr.port()),
      SocketAddr::new("127.0.0.1".parse().expect("v4 loopback"), addr.port()),
    ])
  }
}

/// Recorded observation-hook fan-out, shared between a [`RecordingDelegate`]
/// handed to the driver and the test that asserts on it.
#[derive(Default)]
struct Observed {
  updated: RefCell<Vec<SmolStr>>,
  queries: RefCell<Vec<SmolStr>>,
  user_events: RefCell<Vec<SmolStr>>,
}

/// A [`Delegate`] that records which observation hooks the QUIC driver fired.
struct RecordingDelegate(Rc<Observed>);

impl MemberDelegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;

  async fn notify_update(&self, member: Arc<serf_proto::members::Member<SmolStr, SocketAddr>>) {
    self
      .0
      .updated
      .borrow_mut()
      .push(member.node().id_ref().clone());
  }
}

impl UserEventDelegate for RecordingDelegate {
  async fn notify_user_event(&self, event: &UserEventMessage) {
    self.0.user_events.borrow_mut().push(event.name.clone());
  }
}

impl QueryDelegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;

  async fn notify_query(&self, event: &QueryEvent<SmolStr, SocketAddr>) {
    self.0.queries.borrow_mut().push(SmolStr::new(event.name()));
  }
}

impl Delegate for RecordingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

/// A [`Delegate`] whose user-event hook parks for `stall`, so the driver's
/// observation task cannot drain its queue while the pump keeps enqueueing.
struct StallingDelegate {
  stall: Duration,
}

impl MemberDelegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

impl UserEventDelegate for StallingDelegate {
  async fn notify_user_event(&self, _event: &UserEventMessage) {
    compio::time::sleep(self.stall).await;
  }
}

impl QueryDelegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

impl Delegate for StallingDelegate {
  type Id = SmolStr;
  type Address = SocketAddr;
}

/// A merge predicate that admits every exchange while recording the peers each
/// push/pull carried. `MergeDelegate` is the machine's `Send + Sync` predicate,
/// so the record is shared through an `Arc` even on the `!Send` compio driver.
type MergedPeers = Arc<std::sync::Mutex<Vec<SmolStr>>>;

struct RecordingMerge {
  peers: MergedPeers,
}

impl MergeDelegate<SmolStr, SocketAddr> for RecordingMerge {
  fn notify_merge(
    &self,
    peers: memberlist_proto::MaybeOwned<
      '_,
      [memberlist_proto::typed::NodeState<SmolStr, SocketAddr>],
    >,
  ) -> bool {
    let mut seen = self.peers.lock().expect("merge record lock");
    for p in peers.iter() {
      seen.push(p.id_ref().clone());
    }
    true
  }
}

/// Every knob a QUIC test node may vary, defaulted to the plain loopback node
/// most scenarios want.
struct NodeSpec<D>
where
  D: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
{
  delegate: D,
  runtime: RuntimeOptions,
  serf: SerfOptions,
  merge: Option<Box<dyn MergeDelegate<SmolStr, SocketAddr>>>,
  snapshot: Option<SnapshotOptions>,
  #[cfg(encryption)]
  encryption: EncryptionOptions,
  #[cfg(encryption)]
  keyring: Rc<dyn KeyringDelegate>,
}

impl NodeSpec<VoidDelegate<SmolStr, SocketAddr>> {
  fn new() -> Self {
    Self {
      delegate: VoidDelegate::new(),
      runtime: RuntimeOptions::new(),
      serf: SerfOptions::new(),
      merge: None,
      snapshot: None,
      #[cfg(encryption)]
      encryption: EncryptionOptions::new(),
      #[cfg(encryption)]
      keyring: Rc::new(VoidKeyringDelegate),
    }
  }
}

impl<D> NodeSpec<D>
where
  D: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
{
  fn with_delegate<E>(self, delegate: E) -> NodeSpec<E>
  where
    E: Delegate<Id = SmolStr, Address = SocketAddr> + 'static,
  {
    NodeSpec {
      delegate,
      runtime: self.runtime,
      serf: self.serf,
      merge: self.merge,
      snapshot: self.snapshot,
      #[cfg(encryption)]
      encryption: self.encryption,
      #[cfg(encryption)]
      keyring: self.keyring,
    }
  }

  fn with_runtime(mut self, runtime: RuntimeOptions) -> Self {
    self.runtime = runtime;
    self
  }

  fn with_merge(mut self, merge: Box<dyn MergeDelegate<SmolStr, SocketAddr>>) -> Self {
    self.merge = Some(merge);
    self
  }

  fn with_snapshot(mut self, snapshot: SnapshotOptions) -> Self {
    self.snapshot = Some(snapshot);
    self
  }

  #[cfg(encryption)]
  fn with_encryption(mut self, encryption: EncryptionOptions) -> Self {
    self.encryption = encryption;
    self
  }

  #[cfg(encryption)]
  fn with_keyring(mut self, keyring: Rc<dyn KeyringDelegate>) -> Self {
    self.keyring = keyring;
    self
  }

  /// Build and spawn the node on an ephemeral loopback UDP port.
  async fn spawn(self, id: &str) -> Serf<SmolStr> {
    #[allow(unused_mut)]
    let mut opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_local_id(SmolStr::new(id))
      .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral()))
      .with_quic_config(test_quic_options());
    #[cfg(encryption)]
    {
      opts = opts.with_encryption(self.encryption);
    }
    Serf::new::<QuicTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      self.delegate,
      self.runtime,
      self.serf,
      gossip_rng().expect("seed gossip rng"),
      None,
      self.merge,
      self.snapshot,
      #[cfg(encryption)]
      self.keyring,
    )
    .await
    .expect("spawn serf quic node")
  }
}

/// Spawn a plain loopback QUIC node.
async fn spawn_node(id: &str) -> Serf<SmolStr> {
  NodeSpec::new().spawn(id).await
}

/// Poll both nodes until each reports the full two-member cluster.
async fn converge(a: &Serf<SmolStr>, b: &Serf<SmolStr>) {
  compio::time::timeout(WINDOW, async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      compio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Join `joiner` to `seed` over QUIC and wait for both to converge.
async fn join_and_converge(joiner: &Serf<SmolStr>, seed: &Serf<SmolStr>) {
  joiner
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(seed.advertise_address()),
      false,
    )
    .await
    .expect("join reaches the seed over QUIC");
  converge(joiner, seed).await;
}

/// Drive `events` until the next inbound `Event::Query` named `name` arrives,
/// returning its response token.
async fn next_query_token<S>(events: &mut S, name: &str) -> QueryEvent<SmolStr, SocketAddr>
where
  S: futures_util::Stream<Item = Event<SmolStr, SocketAddr>> + Unpin,
{
  compio::time::timeout(WINDOW, async {
    loop {
      match events.next().await {
        Some(Event::Query(qe)) if qe.name() == name => break qe,
        Some(_) => {}
        None => panic!("the event stream closed before the query arrived"),
      }
    }
  })
  .await
  .expect("the query reaches the responder within the window")
}

/// Assert a command's reply is the teardown `Shutdown` error.
fn expect_shutdown<T>(res: Result<T, SerfError>)
where
  T: core::fmt::Debug,
{
  match res {
    Err(SerfError::Shutdown) => {}
    Err(other) => panic!("expected Shutdown, got {other:?}"),
    Ok(v) => panic!("expected Shutdown, got Ok({v:?})"),
  }
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled.
#[cfg(encryption)]
fn test_secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

// ── scenarios ─────────────────────────────────────────────────────────────────

/// A user event broadcast by B over the QUIC datagram gossip plane reaches A's
/// event stream with the original name and payload, and fires A's
/// `notify_user_event` observation hook.
#[compio::test]
async fn a_quic_user_event_reaches_the_peer_stream_and_delegate() {
  let seen = Rc::new(Observed::default());
  let a = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("que-a")
    .await;
  let b = spawn_node("que-b").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  let payload = Bytes::from_static(b"quic-deploy");
  b.user_event("deploy", payload.clone(), false)
    .await
    .expect("B broadcasts a user event");

  let got = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "deploy" => break u.payload.clone(),
        Some(_) => {}
        None => panic!("A's event stream closed before the user event arrived"),
      }
    }
  })
  .await
  .expect("A receives B's user event within the window");
  assert_eq!(got, payload, "the payload survives the QUIC broadcast");

  assert!(
    seen.user_events.borrow().iter().any(|n| n == "deploy"),
    "the QUIC driver fired A's notify_user_event hook"
  );

  a.shutdown().await.expect("que-a shuts down");
  b.shutdown().await.expect("que-b shuts down");
}

/// A query issued by A over QUIC round-trips: B surfaces the inbound
/// `Event::Query` (firing its `notify_query` hook), answers through
/// `Serf::respond`, and A surfaces the matching `Event::QueryResponse`.
#[compio::test]
async fn a_quic_query_round_trips_through_respond() {
  let seen = Rc::new(Observed::default());
  let b = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("qq-b")
    .await;
  let a = spawn_node("qq-a").await;

  let mut b_events = b.events();
  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  let want = Bytes::from_static(b"quic-pong");
  a.query(
    "ping",
    Bytes::from_static(b"quic-ping"),
    a.default_query_param(),
  )
  .await
  .expect("query issued");

  let responder = async {
    let token = next_query_token(&mut b_events, "ping").await;
    assert_eq!(
      token.payload(),
      &Bytes::from_static(b"quic-ping"),
      "the inbound query carries the originator's payload"
    );
    b.respond(token, want.clone())
      .await
      .expect("B responds to the query");
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

  let got = compio::time::timeout(WINDOW, async {
    let (_, got) = future::join(responder, collector).await;
    got
  })
  .await
  .expect("the QUIC query round-trip completes within the window");
  assert!(got, "A must receive B's query response over QUIC");

  assert_eq!(
    seen.queries.borrow().as_slice(),
    &[SmolStr::new("ping")],
    "the QUIC driver fired B's notify_query hook exactly once"
  );

  a.shutdown().await.expect("qq-a shuts down");
  b.shutdown().await.expect("qq-b shuts down");
}

/// `set_tags` over QUIC re-tags the local node and propagates: the peer's view
/// carries the new tag and its `notify_update` hook fires.
#[compio::test]
async fn quic_set_tags_propagates_as_a_member_update() {
  let seen = Rc::new(Observed::default());
  let a = NodeSpec::new()
    .with_delegate(RecordingDelegate(seen.clone()))
    .spawn("qt-a")
    .await;
  let b = spawn_node("qt-b").await;

  join_and_converge(&a, &b).await;

  let mut tags = Tags::new();
  tags.0.insert(SmolStr::new("role"), SmolStr::new("worker"));
  b.set_tags(tags).await.expect("B re-tags itself");

  let got = compio::time::timeout(WINDOW, async {
    loop {
      let seen_tag = a
        .members()
        .iter()
        .find(|m| m.node().id_ref().as_str() == "qt-b")
        .and_then(|m| m.tags().0.get("role").cloned());
      if let Some(v) = seen_tag {
        break v;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A observes B's new tag within the window");
  assert_eq!(got.as_str(), "worker", "A's view of B carries the new tag");

  assert!(
    seen.updated.borrow().iter().any(|id| id == "qt-b"),
    "the QUIC driver fired A's notify_update hook for the re-tagged peer"
  );

  a.shutdown().await.expect("qt-a shuts down");
  b.shutdown().await.expect("qt-b shuts down");
}

/// A graceful `leave()` over QUIC resolves only once the machine's `LeftCluster`
/// fires: the event surfaces on the leaver's own stream and its endpoint settles
/// at `Left`. Two racing `leave()` callers share ONE in-flight operation — the
/// second joins the first's waiter rather than re-invoking the machine's
/// terminal `leave()` (which emits no second `LeftCluster`).
#[compio::test]
async fn a_quic_graceful_leave_emits_left_cluster_once_for_every_caller() {
  let b = spawn_node("qlv-b").await;
  let a = spawn_node("qlv-a").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  let (first, second) = compio::time::timeout(WINDOW, future::join(a.leave(), a.leave()))
    .await
    .expect("both racing leaves resolve within the window");
  first.expect("the initiating leave resolves Ok");
  second.expect("the leave that joined the in-flight operation resolves Ok");

  let saw = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::LeftCluster) => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("A observes LeftCluster within the window");
  assert!(saw, "A must surface Event::LeftCluster after leave()");

  compio::time::timeout(WINDOW, async {
    loop {
      if a.state() == SerfState::Left {
        break;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("A's endpoint state becomes Left");

  a.shutdown().await.expect("qlv-a shuts down");
  b.shutdown().await.expect("qlv-b shuts down");
}

/// Once a QUIC node has left, every mutating command it is handed reports
/// [`SerfError::NotRunning`] rather than being applied to a non-participating
/// endpoint — while a repeat `leave()` stays idempotent and the read-only
/// coordinate probe still answers.
#[compio::test]
async fn quic_commands_after_leave_report_not_running() {
  let a = spawn_node("qnr-a").await;
  let b = spawn_node("qnr-b").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  b.query("probe", Bytes::new(), b.default_query_param())
    .await
    .expect("B issues a query");
  let token = next_query_token(&mut a_events, "probe").await;

  a.leave().await.expect("A leaves the cluster");

  assert!(
    matches!(
      a.join(
        &SocketAddrResolver,
        MaybeResolved::Resolved(b.advertise_address()),
        false
      )
      .await,
      Err(SerfError::NotRunning)
    ),
    "join after leave must be refused"
  );
  assert!(
    matches!(
      a.force_leave(SmolStr::new("qnr-b"), false).await,
      Err(SerfError::NotRunning)
    ),
    "force_leave after leave must be refused"
  );
  assert!(
    matches!(
      a.user_event("evt", Bytes::new(), false).await,
      Err(SerfError::NotRunning)
    ),
    "user_event after leave must be refused"
  );
  assert!(
    matches!(
      a.query("q", Bytes::new(), a.default_query_param()).await,
      Err(SerfError::NotRunning)
    ),
    "query after leave must be refused"
  );
  assert!(
    matches!(
      a.respond(token, Bytes::new()).await,
      Err(SerfError::NotRunning)
    ),
    "respond after leave must be refused"
  );
  assert!(
    matches!(a.set_tags(Tags::new()).await, Err(SerfError::NotRunning)),
    "set_tags after leave must be refused"
  );
  #[cfg(encryption)]
  {
    let key = test_secret_key(0x5a);
    assert!(
      matches!(a.install_key(key).await, Err(SerfError::NotRunning)),
      "install_key after leave must be refused"
    );
    assert!(
      matches!(a.use_key(key).await, Err(SerfError::NotRunning)),
      "use_key after leave must be refused"
    );
    assert!(
      matches!(a.remove_key(key).await, Err(SerfError::NotRunning)),
      "remove_key after leave must be refused"
    );
    assert!(
      matches!(a.list_keys().await, Err(SerfError::NotRunning)),
      "list_keys after leave must be refused"
    );
  }

  compio::time::timeout(WINDOW, a.leave())
    .await
    .expect("the repeat leave resolves rather than parking")
    .expect("a repeat leave is idempotent");

  #[cfg(feature = "coordinates")]
  a.cached_coordinate(SmolStr::new("qnr-b"))
    .await
    .expect("the coordinate cache answers after leave");

  a.shutdown().await.expect("qnr-a shuts down");
  b.shutdown().await.expect("qnr-b shuts down");
}

/// Commands still queued behind a `Shutdown` when the QUIC pump breaks are
/// ANSWERED with [`SerfError::Shutdown`] at teardown, never dropped: a caller's
/// reply receiver must not hang forever because the driver exited between its
/// send and its dispatch. Every command variant is queued behind the shutdown in
/// one batch, so each teardown reply arm is exercised.
#[compio::test]
async fn quic_commands_queued_behind_a_shutdown_are_answered_not_dropped() {
  let a = spawn_node("qtd-a").await;
  let b = spawn_node("qtd-b").await;

  let mut b_events = b.events();
  join_and_converge(&a, &b).await;

  a.query("probe", Bytes::new(), a.default_query_param())
    .await
    .expect("A issues a query");
  let token = next_query_token(&mut b_events, "probe").await;

  let a_addr = a.advertise_address();
  // Declared ahead of `queued` so it outlives the boxed futures that borrow it.
  #[cfg(encryption)]
  let key = test_secret_key(0x6b);
  let mut queued: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
  // Polled first, so `Shutdown` is the head of the command queue and every
  // command pushed after it lands behind it.
  queued.push(Box::pin(async {
    b.shutdown().await.expect("the shutdown itself is acked");
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.user_event("evt", Bytes::new(), false).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.query("q", Bytes::new(), b.default_query_param()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.respond(token, Bytes::new()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.set_tags(Tags::new()).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.force_leave(SmolStr::new("qtd-a"), false).await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(b.leave().await);
  }));
  queued.push(Box::pin(async {
    expect_shutdown(
      b.join(&SocketAddrResolver, MaybeResolved::Resolved(a_addr), false)
        .await,
    );
  }));
  queued.push(Box::pin(async {
    expect_shutdown(
      b.dispatch_join(&SocketAddrResolver, &[MaybeResolved::Resolved(a_addr)])
        .await,
    );
  }));
  #[cfg(encryption)]
  {
    queued.push(Box::pin(async {
      expect_shutdown(b.install_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.use_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.remove_key(key).await);
    }));
    queued.push(Box::pin(async {
      expect_shutdown(b.list_keys().await);
    }));
  }
  #[cfg(feature = "coordinates")]
  queued.push(Box::pin(async {
    expect_shutdown(b.cached_coordinate(SmolStr::new("qtd-a")).await);
  }));

  compio::time::timeout(WINDOW, future::join_all(queued))
    .await
    .expect("every command queued behind the shutdown is answered, none hang");

  // A command issued AFTER the driver has torn down fails fast on the handle's
  // shutdown flag rather than queueing into a dead channel.
  assert!(
    matches!(
      b.user_event("late", Bytes::new(), false).await,
      Err(SerfError::Shutdown)
    ),
    "a post-teardown command fails fast with Shutdown"
  );

  a.shutdown().await.expect("qtd-a shuts down");
}

/// A burst of commands issued in one batch is all applied: the pump's iter-top
/// fairness drain picks up the commands queued behind the one that woke it and
/// flushes their outputs before it re-enters the select, so every event of the
/// burst still reaches a subscriber.
#[compio::test]
async fn a_quic_command_burst_is_drained_and_flushed_in_one_pass() {
  const BURST: usize = 16;
  let a = spawn_node("qb-a").await;
  let mut events = a.events();

  let mut batch: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::with_capacity(BURST);
  for i in 0..BURST {
    // Every clone shares the one driver task; cloning per future keeps each an
    // owned handle so the whole batch can be polled in a single pass.
    let handle = a.clone();
    batch.push(Box::pin(async move {
      handle
        .user_event(format!("burst-{i}"), Bytes::new(), false)
        .await
        .expect("user event dispatched");
    }));
  }
  compio::time::timeout(WINDOW, future::join_all(batch))
    .await
    .expect("every command of the burst is applied");

  let delivered = compio::time::timeout(WINDOW, async {
    let mut n = 0usize;
    while n < BURST {
      match events.next().await {
        Some(Event::User(_)) => n += 1,
        Some(_) => {}
        None => panic!("the event stream closed mid-burst"),
      }
    }
    n
  })
  .await
  .expect("every event of the command burst surfaces");
  assert_eq!(delivered, BURST, "no command of the burst is lost");

  a.shutdown().await.expect("qb-a shuts down");
}

/// With an UNBOUNDED observation channel the QUIC driver opts out of shedding
/// entirely: a stalling delegate cannot make the pump drop a single event.
#[compio::test]
async fn a_quic_unbounded_observation_channel_sheds_nothing() {
  const BURST: u32 = 24;
  let a = NodeSpec::new()
    .with_delegate(StallingDelegate {
      stall: Duration::from_millis(2),
    })
    .with_runtime(RuntimeOptions::new().with_observation_channel(Channel::Unbounded))
    .spawn("qunb-a")
    .await;

  let mut events = a.events();
  for i in 0..BURST {
    a.user_event(format!("burst-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  let delivered = compio::time::timeout(WINDOW, async {
    let mut n = 0u32;
    while n < BURST {
      match events.next().await {
        Some(Event::User(_)) => n += 1,
        Some(_) => {}
        None => panic!("the event stream closed mid-burst"),
      }
    }
    n
  })
  .await
  .expect("every event of the burst is delivered under an unbounded observation channel");

  assert_eq!(delivered, BURST, "no event of the burst is shed");
  assert_eq!(
    a.observation_dropped(),
    0,
    "an unbounded observation channel never drops"
  );

  a.shutdown().await.expect("qunb-a shuts down");
}

/// A cap-1 observation channel behind a delegate that parks on every user event
/// makes the QUIC pump shed: the enqueue retries once (yielding to the
/// observation task) and then drops and counts, rather than blocking the FSM on
/// a full queue.
#[compio::test]
async fn a_stalled_delegate_makes_the_quic_pump_shed() {
  const BURST: u32 = 32;
  let a = NodeSpec::new()
    .with_delegate(StallingDelegate {
      stall: Duration::from_secs(30),
    })
    .with_runtime(RuntimeOptions::new().with_observation_channel(Channel::Bounded(1)))
    .spawn("qshed-a")
    .await;

  for i in 0..BURST {
    a.user_event(format!("shed-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  let dropped = compio::time::timeout(WINDOW, async {
    loop {
      let n = a.observation_dropped();
      if n > 0 {
        break n;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("a stalled delegate on a cap-1 observation channel must make the QUIC pump shed");

  assert!(
    dropped > 0,
    "the pump drops and counts events the stalled observation task cannot take (got {dropped})"
  );

  a.shutdown().await.expect("qshed-a shuts down");
}

/// With a cap-1 event queue and nobody draining the stream, the `events_dropped`
/// counter on the handle becomes non-zero: the observation task's forward to
/// subscribers is best-effort and counts what it sheds rather than blocking.
#[compio::test]
async fn a_full_quic_event_queue_counts_what_it_sheds() {
  let a = NodeSpec::new()
    .with_runtime(RuntimeOptions::new().with_event_queue_cap(1))
    .spawn("qdrop-a")
    .await;

  // The stream is subscribed but never polled, so its cap-1 slot fills on the
  // first event and every later delivery is shed and counted.
  let _events = a.events();
  for i in 0..16u32 {
    a.user_event(format!("drop-{i}"), Bytes::new(), false)
      .await
      .expect("user event dispatched");
  }

  let dropped = compio::time::timeout(WINDOW, async {
    loop {
      let n = a.events_dropped();
      if n > 0 {
        break n;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("an undrained cap-1 event queue must shed and count");
  assert!(
    dropped > 0,
    "events_dropped counts every event the full stream channel rejected (got {dropped})"
  );

  a.shutdown().await.expect("qdrop-a shuts down");
}

/// A fire-and-forget `dispatch_join` parks no await-result waiter: its exchange
/// still terminalizes cleanly through the pump's completion path (no waiter to
/// resolve), the membership converges from the dispatched push/pull alone, and
/// the constructor-supplied merge delegate is consulted on the seed side.
#[compio::test]
async fn a_quic_dispatch_join_converges_without_a_join_waiter() {
  let peers: MergedPeers = Arc::new(std::sync::Mutex::new(Vec::new()));
  let b = NodeSpec::new()
    .with_merge(Box::new(RecordingMerge {
      peers: peers.clone(),
    }))
    .spawn("qdj-b")
    .await;
  let a = spawn_node("qdj-a").await;

  let dispatched = a
    .dispatch_join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(b.advertise_address())],
    )
    .await
    .expect("the QUIC dial is dispatched");
  assert_eq!(dispatched, 1, "exactly one seed was dispatched");

  converge(&a, &b).await;

  let merged = peers.lock().expect("merge record lock").clone();
  assert!(
    merged.iter().any(|id| id == "qdj-a"),
    "the QUIC push/pull consulted the installed merge delegate with the joining peer's state \
     (saw {merged:?})"
  );

  a.shutdown().await.expect("qdj-a shuts down");
  b.shutdown().await.expect("qdj-b shuts down");
}

/// Dropping the last handle of a quiet QUIC node tears the driver down through
/// the command-channel disconnect observed in the main select, releasing the
/// bound UDP port for an immediate rebind.
#[compio::test]
async fn dropping_the_last_quic_handle_releases_the_bound_port() {
  let a = spawn_node("qdrp-a").await;
  let addr = a.advertise_address();

  // Let the pump settle into its select before the disconnect, so the drop is
  // observed by the select's command arm rather than the iter-top drain.
  compio::time::sleep(Duration::from_millis(200)).await;
  drop(a);

  let gossip = compio::time::timeout(WINDOW, async {
    loop {
      if let Ok(sock) = compio::net::UdpSocket::bind(addr).await {
        break sock;
      }
      compio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the QUIC driver releases its bound UDP port after the last handle drops");
  // Ignoring Err: test cleanup of the rebind probe socket.
  let _ = gossip.close().await;
}

/// A QUIC node persisting to a snapshot writes its membership records, and a
/// fresh node replaying the SAME file rejoins the cluster with no explicit join
/// call — the constructor's snapshot argument is threaded through the QUIC `run`
/// into the endpoint's replay.
#[compio::test]
async fn a_quic_snapshot_replays_the_membership_on_restart() {
  let mut path = std::env::temp_dir();
  path.push(format!("serf-compio-quic-snap-{}", std::process::id()));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&path);

  let a = spawn_node("qsnap-a").await;
  let b = NodeSpec::new()
    .with_snapshot(SnapshotOptions::new(&path).with_compact_threshold(1))
    .spawn("qsnap-b")
    .await;

  join_and_converge(&b, &a).await;
  b.shutdown().await.expect("qsnap-b shuts down");
  drop(b);

  let bytes = std::fs::read(&path).expect("the QUIC run wrote the snapshot");
  assert!(
    !bytes.is_empty(),
    "compaction rewrites the live membership rather than truncating the file"
  );

  let b2 = NodeSpec::new()
    .with_snapshot(SnapshotOptions::new(&path).with_compact_threshold(1))
    .spawn("qsnap-b")
    .await;
  converge(&a, &b2).await;
  assert_eq!(
    b2.num_members(),
    2,
    "the restarted QUIC node recovers its membership from the snapshot"
  );

  a.shutdown().await.expect("qsnap-a shuts down");
  b2.shutdown().await.expect("qsnap-b2 shuts down");
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// An inbound key-management request on a node with NO keyring configured is
/// answered with a failure result rather than silently ignored: the originator's
/// `KeyResponse` counts the refusal, so an operator sees the plaintext node
/// instead of a hung key query.
#[cfg(encryption)]
#[compio::test]
async fn a_key_request_on_a_plaintext_quic_node_is_refused() {
  let b = spawn_node("qkp-b").await;
  let a = spawn_node("qkp-a").await;

  let mut a_events = a.events();
  join_and_converge(&a, &b).await;

  a.install_key(test_secret_key(0x77))
    .await
    .expect("install_key dispatched");
  let kr = next_key_response(&mut a_events).await;

  assert!(
    kr.num_err >= 1,
    "a node with no keyring must refuse the key op (num_err={}, messages={:?})",
    kr.num_err,
    kr.messages
  );
  assert!(
    kr.messages
      .values()
      .any(|m| m.contains("no keyring configured")),
    "the refusal names the missing keyring (messages={:?})",
    kr.messages
  );

  a.shutdown().await.expect("qkp-a shuts down");
  b.shutdown().await.expect("qkp-b shuts down");
}

/// Drive `events` until the originator's next `KeyResponse` surfaces (emitted
/// when the key query's deadline fires), draining any interleaved membership /
/// gossip events so a backlog cannot stall the stream.
#[cfg(encryption)]
async fn next_key_response<S>(events: &mut S) -> serf_proto::event::KeyResponse<SmolStr>
where
  S: futures_util::Stream<Item = Event<SmolStr, SocketAddr>> + Unpin,
{
  compio::time::timeout(WINDOW, async {
    loop {
      match events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("the event stream closed before a KeyResponse"),
      }
    }
  })
  .await
  .expect("a KeyResponse within the window")
}

/// A [`KeyringDelegate`] that records, in order, every live keyring the driver
/// publishes through `keyring_updated`. The driver fires it only after it has
/// pushed the rotated ring to the endpoint, so the recorded ring is exactly the
/// ring the gossip plane now encrypts under.
#[cfg(encryption)]
#[derive(Default)]
struct RecordingKeyring {
  rings: RefCell<Vec<Keyring>>,
}

#[cfg(encryption)]
impl RecordingKeyring {
  fn rings(&self) -> Vec<Keyring> {
    self.rings.borrow().clone()
  }
}

#[cfg(encryption)]
impl KeyringDelegate for RecordingKeyring {
  fn keyring_updated(&self, keyring: &Keyring) -> serf_driver::KeyringPersistence {
    self.rings.borrow_mut().push(keyring.clone());
    serf_driver::KeyringPersistence::Durable
  }
}

/// Two encrypted QUIC nodes share primary K1, then A rotates the cluster to K2
/// via install -> use -> remove. Each op propagates over the encrypted gossip
/// plane and every node applies it to its LIVE wire keyring: both observers
/// record the exact ring sequence, a cluster-wide `list_keys` confirms K2 is the
/// primary everywhere and K1 is installed nowhere, the read-only `list` and the
/// refused repeat-remove fire no observer, and a user event still crosses the
/// wire afterwards — proving the datagram plane now runs under K2.
#[cfg(encryption)]
#[compio::test]
async fn quic_key_rotation_rotates_both_live_keyrings() {
  let k1 = test_secret_key(0x11);
  let k2 = test_secret_key(0x22);

  let rec_a = Rc::new(RecordingKeyring::default());
  let rec_b = Rc::new(RecordingKeyring::default());
  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));

  let b = NodeSpec::new()
    .with_encryption(enc())
    .with_keyring(rec_b.clone())
    .spawn("qrot-b")
    .await;
  let a = NodeSpec::new()
    .with_encryption(enc())
    .with_keyring(rec_a.clone())
    .spawn("qrot-a")
    .await;

  join_and_converge(&a, &b).await;

  // The published snapshot carries the coordinator's live encryption flag, so a
  // keyring that never reached the QUIC coordinator would read back `false` here.
  assert!(
    a.encryption_enabled(),
    "the keyring reaches the QUIC coordinator and surfaces on the handle"
  );
  assert!(b.encryption_enabled(), "both nodes gossip under a keyring");

  let mut a_events = a.events();
  let mut b_events = b.events();

  a.install_key(k2).await.expect("install_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert!(
    kr.num_resp >= 2,
    "install_key must collect a response from BOTH nodes (num_resp={})",
    kr.num_resp
  );
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");

  a.use_key(k2).await.expect("use_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "use_key must succeed on every node");

  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "remove_key must succeed on every node");

  for (name, rec) in [("qrot-a", &rec_a), ("qrot-b", &rec_b)] {
    let rings = rec.rings();
    assert_eq!(
      rings.len(),
      3,
      "{name}: the observer fires exactly once per successful mutation"
    );
    assert_eq!(
      rings[0].primary_ref(),
      &k1,
      "{name}: install leaves K1 as primary"
    );
    assert!(
      rings[0].secondaries().contains(&k2),
      "{name}: install adds K2 as a secondary"
    );
    assert_eq!(rings[1].primary_ref(), &k2, "{name}: use promotes K2");
    assert_eq!(
      rings[2].primary_ref(),
      &k2,
      "{name}: K2 stays primary after the remove"
    );
    assert!(
      !rings[2].secondaries().contains(&k1),
      "{name}: the removed K1 is absent from the live keyring"
    );
  }

  a.list_keys().await.expect("list_keys dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert_eq!(
    kr.primary_keys.get(&k2).copied(),
    Some(2),
    "both nodes report K2 as their live primary"
  );
  assert_eq!(
    kr.keys.get(&k1),
    None,
    "the removed K1 is installed on no node"
  );
  assert_eq!(
    rec_a.rings().len(),
    3,
    "list_keys is read-only and does not fire the keyring observer"
  );

  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response(&mut a_events).await;
  assert!(
    kr.num_err >= 1,
    "removing an already-absent key is refused (num_err={})",
    kr.num_err
  );
  assert_eq!(
    rec_b.rings().len(),
    3,
    "a refused op does not fire the keyring observer"
  );

  // Post-rotation traffic proof: a user event still crosses the encrypted
  // datagram plane, which now runs under K2 on both nodes.
  a.user_event("after-rotation", Bytes::from_static(b"payload"), false)
    .await
    .expect("user_event from a running node");
  let saw = compio::time::timeout(WINDOW, async {
    loop {
      match b_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "after-rotation" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("B observes the post-rotation user event within the window");
  assert!(
    saw,
    "a user event must still propagate A -> B after the rotation (wire under K2)"
  );

  a.shutdown().await.expect("qrot-a shuts down");
  b.shutdown().await.expect("qrot-b shuts down");
}

/// The sending half of the driver's rotation-durability acknowledgement channel
/// ([`serf_driver::KeyringPersistRx`] is its receiver).
#[cfg(encryption)]
type PersistTx = std::sync::mpsc::Sender<Result<(), serf_driver::KeyringPersistError>>;

/// A keyring delegate whose persistence resolves OUT OF BAND: `keyring_updated`
/// hands back a pending receiver, and the test releases it after a delay. The
/// pump must park the key response until the acknowledgement lands and only then
/// route it — so the originator still collects BOTH nodes' successes.
#[cfg(encryption)]
#[derive(Default)]
struct DeferredKeyring {
  /// Acknowledgement senders for every rotation this delegate parked, in order.
  parked: RefCell<Vec<PersistTx>>,
}

#[cfg(encryption)]
impl DeferredKeyring {
  /// Acknowledge every parked rotation as durable.
  fn release_all(&self) {
    for tx in self.parked.borrow_mut().drain(..) {
      // Ignoring Err: the pump dropped the receiver (its key request already
      // timed out); nothing to acknowledge.
      let _ = tx.send(Ok(()));
    }
  }
}

#[cfg(encryption)]
impl KeyringDelegate for DeferredKeyring {
  fn keyring_updated(&self, _keyring: &Keyring) -> serf_driver::KeyringPersistence {
    let (tx, rx) = std::sync::mpsc::channel();
    self.parked.borrow_mut().push(tx);
    serf_driver::KeyringPersistence::Pending(rx)
  }
}

/// With node B parking its rotation on out-of-band persistence, an
/// `install_key` from A still collects BOTH nodes' successful responses: B's
/// response is held until the acknowledgement resolves and is then routed inside
/// the query window. A driver that answered ahead of the acknowledgement — or
/// dropped the parked response — would fail this.
#[cfg(encryption)]
#[compio::test]
async fn a_parked_key_response_is_routed_once_persistence_acknowledges() {
  let k1 = test_secret_key(0x33);
  let k2 = test_secret_key(0x44);
  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));

  let deferred = Rc::new(DeferredKeyring::default());
  let b = NodeSpec::new()
    .with_encryption(enc())
    .with_keyring(deferred.clone())
    .spawn("qdef-b")
    .await;
  // A keeps the DEFAULT keyring delegate: its own live-ring rotation still
  // applies, it simply persists nothing and answers durable-inline.
  let a = NodeSpec::new().with_encryption(enc()).spawn("qdef-a").await;

  join_and_converge(&a, &b).await;

  let mut a_events = a.events();
  a.install_key(k2).await.expect("install_key dispatched");

  // Release B's parked acknowledgement shortly after the rotation lands, well
  // inside the key query's response window.
  let releaser = deferred.clone();
  compio::runtime::spawn(async move {
    compio::time::sleep(Duration::from_millis(300)).await;
    releaser.release_all();
  })
  .detach();

  let kr = next_key_response(&mut a_events).await;
  assert!(
    kr.num_resp >= 2,
    "install_key must collect a response from BOTH nodes, including the one parked on \
     out-of-band persistence (num_resp={})",
    kr.num_resp
  );
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");

  a.shutdown().await.expect("qdef-a shuts down");
  b.shutdown().await.expect("qdef-b shuts down");
}

/// The `QuicTransportOptions` accessors reflect exactly what the builders set,
/// and `Default` is the `new()` state: every required field the constructor's
/// guards check is unset.
#[test]
fn quic_transport_options_accessors_reflect_builders() {
  let addr: SocketAddr = "127.0.0.1:7946".parse().expect("loopback addr");

  let empty = QuicTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(empty.local_id().is_none(), "Default leaves local_id unset");
  assert!(
    empty.advertise_addr().is_none(),
    "Default leaves advertise_addr unset"
  );
  assert!(
    empty.quic_config().is_none(),
    "Default leaves the quic config unset"
  );

  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("acc-node"))
    .with_advertise_addr(MaybeResolved::Resolved(addr))
    .with_quic_config(test_quic_options());
  assert_eq!(opts.local_id().map(SmolStr::as_str), Some("acc-node"));
  match opts.advertise_addr() {
    Some(MaybeResolved::Resolved(s)) => assert_eq!(*s, addr),
    other => panic!("expected a resolved advertise addr, got {other:?}"),
  }
  assert!(
    opts.quic_config().is_some(),
    "with_quic_config installs the caller's bundle"
  );

  #[cfg(encryption)]
  {
    assert!(
      opts.encryption().keyring().is_none(),
      "the default gossip-encryption policy carries no keyring"
    );
    let key = test_secret_key(0x31);
    let encrypted = QuicTransportOptions::<SmolStr, SocketAddr>::new()
      .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(key)));
    assert_eq!(
      encrypted
        .encryption()
        .keyring()
        .expect("the installed keyring is readable back")
        .primary_ref(),
      &key,
      "with_encryption installs the caller's gossip keyring"
    );
  }
}

/// `QuicTransport::new` refuses each required field it cannot default, naming
/// the missing one, before binding a socket.
#[compio::test]
async fn quic_new_requires_a_local_id_an_advertise_addr_and_a_config() {
  let no_id = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral()))
    .with_quic_config(test_quic_options());
  assert_missing_field(
    QuicTransport::<SmolStr, SocketAddr>::new(no_id, &SocketAddrResolver, &FirstAddrResolver).await,
    "local_id",
  );

  let no_addr = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("no-addr"))
    .with_quic_config(test_quic_options());
  assert_missing_field(
    QuicTransport::<SmolStr, SocketAddr>::new(no_addr, &SocketAddrResolver, &FirstAddrResolver)
      .await,
    "advertise_addr",
  );

  let no_cfg = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("no-cfg"))
    .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral()));
  assert_missing_field(
    QuicTransport::<SmolStr, SocketAddr>::new(no_cfg, &SocketAddrResolver, &FirstAddrResolver)
      .await,
    "quic_config",
  );
}

/// Assert a transport construction was refused with `InvalidInput` naming the
/// required field the caller left unset.
fn assert_missing_field<T>(res: Result<T, SerfError>, field: &str) {
  match res {
    Err(SerfError::Io(e)) => {
      assert_eq!(
        e.kind(),
        std::io::ErrorKind::InvalidInput,
        "a missing required field is an InvalidInput refusal"
      );
      assert!(
        e.to_string().contains(field),
        "the refusal names the missing field {field:?}, got {e}"
      );
    }
    Err(other) => panic!("expected InvalidInput({field}), got {other:?}"),
    Ok(_) => panic!("a missing {field} must be refused, but construction succeeded"),
  }
}

/// An advertise address the caller supplied UNRESOLVED must not silently bind a
/// wrong contact when resolution cannot answer: a resolver outage surfaces as
/// `SerfError::Resolve`, and a resolution that yields ZERO candidates is refused
/// by the advertise picker rather than defaulted.
#[compio::test]
async fn quic_new_refuses_an_advertise_address_it_cannot_resolve() {
  struct FailingResolver;

  impl Resolver for FailingResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;

    async fn resolve(&self, _addr: &SocketAddr) -> Result<Vec<SocketAddr>, std::io::Error> {
      Err(std::io::Error::other("discovery backend unavailable"))
    }
  }

  struct EmptyResolver;

  impl Resolver for EmptyResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;

    async fn resolve(&self, _addr: &SocketAddr) -> Result<Vec<SocketAddr>, std::io::Error> {
      Ok(Vec::new())
    }
  }

  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("quic-res-fail"))
    .with_advertise_addr(MaybeResolved::Unresolved(loopback_ephemeral()))
    .with_quic_config(test_quic_options());
  match QuicTransport::<SmolStr, SocketAddr>::new(opts, &FailingResolver, &FirstAddrResolver).await
  {
    Err(SerfError::Resolve(e)) => assert!(
      e.to_string().contains("discovery backend unavailable"),
      "the resolver's own error is surfaced, got {e}"
    ),
    Err(other) => panic!("expected Resolve, got {other:?}"),
    Ok(_) => panic!("a resolver outage must refuse construction"),
  }

  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("quic-res-empty"))
    .with_advertise_addr(MaybeResolved::Unresolved(loopback_ephemeral()))
    .with_quic_config(test_quic_options());
  match QuicTransport::<SmolStr, SocketAddr>::new(opts, &EmptyResolver, &FirstAddrResolver).await {
    Err(SerfError::Resolve(e)) => assert_eq!(
      e.kind(),
      std::io::ErrorKind::AddrNotAvailable,
      "a zero-candidate resolution is an unavailable advertise address"
    ),
    Err(other) => panic!("expected Resolve(AddrNotAvailable), got {other:?}"),
    Ok(_) => panic!("a zero-candidate resolution must refuse construction"),
  }
}

/// An UNRESOLVED advertise address is resolved through the caller's `Resolver`
/// and NARROWED by the `AdvertiseAddrResolver`: the resolver offers an IPv6 and
/// an IPv4 candidate, the IPv4-preferring picker chooses the IPv4 one, and the
/// QUIC transport binds THAT address and retains the unresolved input form.
#[compio::test]
async fn quic_new_resolves_and_narrows_an_unresolved_advertise_addr() {
  let input: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("quic-unres"))
    .with_advertise_addr(MaybeResolved::Unresolved(input))
    .with_quic_config(test_quic_options());
  let transport =
    QuicTransport::<SmolStr, SocketAddr>::new(opts, &DualStackResolver, &Ipv4PreferringResolver)
      .await
      .expect("an unresolved advertise address resolves through the resolver");

  assert_eq!(transport.local_id().as_str(), "quic-unres");
  let bound = *transport.advertise_address();
  assert!(
    bound.is_ipv4(),
    "the IPv4-preferring picker narrowed the dual-stack candidate set, got {bound}"
  );
  assert!(bound.ip().is_loopback(), "the picked candidate was bound");
  assert_ne!(
    bound.port(),
    0,
    "the ephemeral port is read back concretely"
  );
  match transport.local_address() {
    MaybeResolved::Unresolved(a) => {
      assert_eq!(*a, input, "the unresolved input form is retained")
    }
    other => panic!("expected the unresolved input form, got {other:?}"),
  }
}
