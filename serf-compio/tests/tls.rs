//! Real-node TLS serf tests: loopback nodes exercising the compio stream driver
//! over a real rustls record layer (a self-signed localhost-SAN cert plus an
//! accept-any verifier). TLS rides the SAME stream driver as plain TCP and
//! differs only in the reliable record layer, so this suite pins the
//! TLS-specific construction surface — the options guards, the SNI provider, the
//! unresolved-advertise resolution, and the merge-delegate / snapshot wiring the
//! TLS `run` performs — plus an end-to-end cluster over the encrypted reliable
//! plane.
//!
//! serf-compio declares no `tls-rustls-*` backend feature of its own: the rustls
//! crypto provider is supplied by the CALLER inside the `TlsOptions` bundle
//! (`ServerConfig` / `ClientConfig`), which is exactly what these tests do.

#![cfg(feature = "tls")]

use core::time::Duration;
use std::{
  net::SocketAddr,
  rc::Rc,
  sync::{Arc, Mutex},
};

use bytes::Bytes;
use futures_util::{StreamExt, future};
use memberlist_proto::MaybeResolved;
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  crypto::CryptoProvider,
  pki_types::CertificateDer,
  version::TLS13,
};
use serf_compio::{
  FirstAddrResolver, Ipv4PreferringResolver, MergeDelegate, Resolver, RuntimeOptions, Serf,
  SerfError, SnapshotOptions, SocketAddrResolver, TlsOptions, TlsTransport, TlsTransportOptions,
  Transport, VoidDelegate,
};
use serf_proto::{
  event::{Event, MemberEventKind, QueryEvent},
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

/// Bound on every convergence / delivery poll in this file.
const WINDOW: Duration = Duration::from_secs(45);

/// An ephemeral loopback bind address.
fn loopback_ephemeral() -> SocketAddr {
  "127.0.0.1:0".parse().expect("loopback addr")
}

/// Accept-any server-cert verifier for the loopback suite. The nodes present
/// self-signed localhost-SAN certs; the client side accepts whatever the server
/// presents so the handshake completes without a real trust anchor. NEVER use
/// this outside a test.
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

/// The rustls crypto provider the caller supplies to the TLS bundle: the
/// process default if one is installed, otherwise `ring`.
fn crypto_provider() -> Arc<CryptoProvider> {
  CryptoProvider::get_default()
    .cloned()
    .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// A self-signed localhost-SAN `ServerConfig` + accept-any `ClientConfig`
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

/// The peers every consulted push/pull merge carried. `MergeDelegate` is the
/// machine's `Send + Sync` predicate, so the record is shared through an `Arc`
/// even on the `!Send` compio driver.
type MergedPeers = Arc<Mutex<Vec<SmolStr>>>;

/// A merge predicate that admits every exchange while recording the peers each
/// push/pull carried, so a test can prove the constructor installed it.
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

/// Build a TLS node on an ephemeral loopback port with the fixture's cert bundle.
async fn spawn_node(id: &str) -> Serf<SmolStr, SocketAddr> {
  spawn_node_with(id, None, None)
    .await
    .expect("spawn serf tls node")
}

/// Build a TLS node with an optional merge delegate and snapshot file.
async fn spawn_node_with(
  id: &str,
  merge: Option<Box<dyn MergeDelegate<SmolStr, SocketAddr>>>,
  snapshot: Option<SnapshotOptions>,
) -> Result<Serf<SmolStr, SocketAddr>, SerfError> {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral()))
    .with_tls_options(test_tls_options());
  Serf::tls(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    merge,
    snapshot,
    #[cfg(encryption)]
    Rc::new(serf_compio::VoidKeyringDelegate),
  )
  .await
}

/// Poll both nodes until each reports the full two-member cluster.
async fn converge(a: &Serf<SmolStr, SocketAddr>, b: &Serf<SmolStr, SocketAddr>) {
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

/// Two nodes over a real TLS handshake: A joins B, both converge, a user event
/// crosses the encrypted reliable/gossip planes, a query round-trips through
/// `respond`, and A leaves gracefully.
#[compio::test]
async fn a_tls_cluster_forms_and_carries_events_queries_and_a_leave() {
  let b = spawn_node("tls-b").await;
  let a = spawn_node("tls-a").await;

  let mut a_events = a.events();
  let mut b_events = b.events();

  let reached = a
    .join(
      &SocketAddrResolver,
      MaybeResolved::Resolved(b.advertise_address()),
      false,
    )
    .await
    .expect("join reaches node B over TLS");
  assert_eq!(
    reached,
    b.advertise_address(),
    "join returns the reached seed address"
  );
  converge(&a, &b).await;

  // A `Member(Join)` for B surfaces on A's stream.
  let joined = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::Member(me)) if me.kind() == MemberEventKind::Join => {
          if me.members().iter().any(|m| m.node().id_ref() == "tls-b") {
            break true;
          }
        }
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("A observes B joining within the window");
  assert!(joined, "A must surface a Member(Join) naming B");

  // A user event crosses the TLS cluster.
  let payload = Bytes::from_static(b"tls-payload");
  b.user_event("tls-evt", payload.clone(), false)
    .await
    .expect("B broadcasts a user event");
  let got = compio::time::timeout(WINDOW, async {
    loop {
      match a_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "tls-evt" => break u.payload.clone(),
        Some(_) => {}
        None => panic!("A's event stream closed before the user event arrived"),
      }
    }
  })
  .await
  .expect("A receives B's user event within the window");
  assert_eq!(got, payload, "the payload survives the TLS cluster");

  // A query round-trips through B's `respond`.
  let want = Bytes::from_static(b"tls-pong");
  a.query("tls-ping", Bytes::new(), a.default_query_param())
    .await
    .expect("query issued");
  let responder = async {
    let token: QueryEvent<SmolStr, SocketAddr> = loop {
      match b_events.next().await {
        Some(Event::Query(qe)) if qe.name() == "tls-ping" => break qe,
        Some(_) => {}
        None => panic!("B's event stream closed before the query arrived"),
      }
    };
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
  let answered = compio::time::timeout(WINDOW, async {
    let (_, got) = future::join(responder, collector).await;
    got
  })
  .await
  .expect("the TLS query round-trip completes within the window");
  assert!(answered, "A must receive B's query response over TLS");

  a.leave().await.expect("A leaves the TLS cluster");
  a.shutdown().await.expect("tls-a shuts down");
  b.shutdown().await.expect("tls-b shuts down");
}

/// The constructor-supplied merge delegate reaches the TLS endpoint: A's join
/// push/pull drives at least one `notify_merge` on B carrying A's node state.
/// The snapshot file supplied alongside it is opened and written, proving both
/// constructor arguments are threaded through the TLS `run`.
#[compio::test]
async fn the_tls_run_installs_the_merge_delegate_and_the_snapshot() {
  let mut path = std::env::temp_dir();
  path.push(format!("serf-compio-tls-snap-{}", std::process::id()));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&path);

  let peers: MergedPeers = Arc::new(Mutex::new(Vec::new()));
  let b = spawn_node_with(
    "tlsm-b",
    Some(Box::new(RecordingMerge {
      peers: peers.clone(),
    })),
    Some(SnapshotOptions::new(&path)),
  )
  .await
  .expect("spawn a merge-recording, snapshot-backed TLS node");
  let a = spawn_node("tlsm-a").await;

  a.join(
    &SocketAddrResolver,
    MaybeResolved::Resolved(b.advertise_address()),
    false,
  )
  .await
  .expect("join reaches node B");
  converge(&a, &b).await;

  let merged = peers.lock().expect("merge record lock").clone();
  assert!(
    merged.iter().any(|id| id == "tlsm-a"),
    "the TLS push/pull consulted the installed merge delegate with the joining peer's state \
     (saw {merged:?})"
  );

  a.shutdown().await.expect("tlsm-a shuts down");
  b.shutdown().await.expect("tlsm-b shuts down");

  let bytes = std::fs::read(&path).expect("the snapshot the TLS run installed exists");
  assert!(
    !bytes.is_empty(),
    "the TLS run wrote membership records to the constructor-supplied snapshot"
  );
  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// `TlsTransport::new` refuses each required field it cannot default, naming the
/// missing one, before binding a socket.
#[compio::test]
async fn tls_new_requires_a_local_id_and_an_advertise_addr() {
  let no_id = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_advertise_addr(MaybeResolved::Resolved(loopback_ephemeral()))
    .with_tls_options(test_tls_options());
  assert_missing_field(
    TlsTransport::<SmolStr, SocketAddr>::new(no_id, &SocketAddrResolver, &FirstAddrResolver).await,
    "local_id",
  );

  let no_addr = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("no-addr"))
    .with_tls_options(test_tls_options());
  assert_missing_field(
    TlsTransport::<SmolStr, SocketAddr>::new(no_addr, &SocketAddrResolver, &FirstAddrResolver)
      .await,
    "advertise_addr",
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

/// An UNRESOLVED advertise address is resolved through the caller's `Resolver`
/// and NARROWED by the `AdvertiseAddrResolver`: the resolver offers an IPv6 and
/// an IPv4 candidate, the IPv4-preferring picker chooses the IPv4 one, and the
/// TLS transport binds THAT address and retains the unresolved input form.
#[compio::test]
async fn tls_new_resolves_and_narrows_an_unresolved_advertise_addr() {
  let input: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("tls-unres"))
    .with_advertise_addr(MaybeResolved::Unresolved(input))
    .with_tls_options(test_tls_options());
  let transport =
    TlsTransport::<SmolStr, SocketAddr>::new(opts, &DualStackResolver, &Ipv4PreferringResolver)
      .await
      .expect("an unresolved advertise address resolves through the resolver");

  assert_eq!(transport.local_id().as_str(), "tls-unres");
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

/// An advertise address the caller supplied UNRESOLVED must not silently bind a
/// wrong contact when resolution cannot answer: a resolver outage surfaces as
/// `SerfError::Resolve` rather than a bound-but-undialable node.
#[compio::test]
async fn tls_new_refuses_an_advertise_address_it_cannot_resolve() {
  struct FailingResolver;

  impl Resolver for FailingResolver {
    type Address = SocketAddr;
    type Error = std::io::Error;

    async fn resolve(&self, _addr: &SocketAddr) -> Result<Vec<SocketAddr>, std::io::Error> {
      Err(std::io::Error::other("discovery backend unavailable"))
    }
  }

  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("tls-res-fail"))
    .with_advertise_addr(MaybeResolved::Unresolved(loopback_ephemeral()))
    .with_tls_options(test_tls_options());
  match TlsTransport::<SmolStr, SocketAddr>::new(opts, &FailingResolver, &FirstAddrResolver).await {
    Err(SerfError::Resolve(e)) => assert!(
      e.to_string().contains("discovery backend unavailable"),
      "the resolver's own error is surfaced, got {e}"
    ),
    Err(other) => panic!("expected Resolve, got {other:?}"),
    Ok(_) => panic!("a resolver outage must refuse construction"),
  }
}

/// The gossip-encryption policy on the TLS options is a first-class accessor:
/// the default carries no keyring (plaintext gossip alongside the TLS-secured
/// reliable plane), and `with_encryption` installs one.
#[cfg(encryption)]
#[test]
fn tls_options_expose_the_gossip_encryption_policy() {
  use serf_compio::{EncryptionOptions, Keyring, SecretKey};

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([0x31; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([0x31; 32]);

  let plain = TlsTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(
    plain.encryption().keyring().is_none(),
    "the default TLS options leave the gossip plane plaintext"
  );

  let encrypted = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(key)));
  assert_eq!(
    encrypted
      .encryption()
      .keyring()
      .expect("the installed keyring is readable back")
      .primary_ref(),
    &key,
    "with_encryption installs the caller's keyring as the gossip policy"
  );
}
