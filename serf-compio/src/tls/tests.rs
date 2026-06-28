//! End-to-end smoke test: two TLS serf nodes on the loopback interface, one
//! joining the other over a real rustls handshake, asserting the membership
//! event propagates through the full pump (Join command → push-pull dial → TLS
//! handshake → coordinator merge → serf `Member` event → `EventStream`).

use core::time::Duration;
use std::{io::ErrorKind, net::SocketAddr, sync::Arc};

use futures_util::StreamExt;
use memberlist_proto::MaybeResolved;
use rustls::{
  client::danger::{HandshakeSignatureValid, ServerCertVerified},
  crypto::CryptoProvider,
  pki_types::CertificateDer,
  version::TLS13,
};
use serf_proto::{
  event::{Event, MemberEventKind},
  options::Options as SerfOptions,
};
use smol_str::SmolStr;

use crate::{
  Channel, FirstAddrResolver, RuntimeOptions, Serf, SerfError, SocketAddrResolver,
  StreamTransportOptions, TlsOptions, TlsTransport, TlsTransportOptions, Transport, VoidDelegate,
  gossip_rng,
};

#[cfg(encryption)]
use crate::{EncryptionOptions, Keyring, SecretKey, VoidKeyringDelegate};

/// Accept-any server-cert verifier for the loopback smoke test.
///
/// The two nodes share a self-signed localhost-SAN cert; the client side
/// accepts whatever the server presents so the handshake completes without a
/// real trust anchor. NEVER use this outside a test.
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

/// Build and spawn a TLS serf node bound to an ephemeral loopback port. The
/// default SNI provider (`Some("localhost")`) matches the self-signed cert SAN.
async fn spawn_node(id: &str) -> Serf<SmolStr> {
  try_spawn_node_at(id, "127.0.0.1:0".parse().expect("loopback addr"))
    .await
    .expect("spawn serf node")
}

/// Build a TLS serf node bound to a specific advertise address, returning the
/// construction result so the same-address rebind regression can assert a freed
/// port accepts an immediate rebind. A fresh self-signed bundle is built per
/// node, matching `spawn_node`.
async fn try_spawn_node_at(id: &str, bind: SocketAddr) -> Result<Serf<SmolStr>, SerfError> {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_tls_options(test_tls_options());
  Serf::new::<TlsTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    #[cfg(encryption)]
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
}

/// TLS rides the same stream driver as plain TCP, so it inherits the same
/// bound-port release guarantee: `shutdown().await` must close the TCP listener
/// and UDP gossip socket before it resolves, so a second TLS node binding the
/// SAME advertise address the instant the first shuts down must construct
/// successfully, not fail with `AddrInUse`.
#[compio::test]
async fn tls_shutdown_releases_bound_address_for_rebind() {
  let first = spawn_node("rebind-first").await;
  let addr = first.advertise_address();
  first.shutdown().await.expect("first node shuts down");

  let second = try_spawn_node_at("rebind-second", addr)
    .await
    .expect("rebinding the freed address must succeed, not AddrInUse");
  assert_eq!(
    second.advertise_address(),
    addr,
    "the second node rebinds the exact freed address"
  );
  second.shutdown().await.expect("second node shuts down");
}

/// A `Bounded(0)` observation channel is rejected by the TLS stream driver's
/// `Serf::new` with [`SerfError::InvalidOption`] — before binding a socket or
/// spawning the detached driver — rather than panicking the driver task. TLS
/// rides the same stream driver as plain TCP; a real `tls_options` is supplied so
/// the runtime-option rejection, not the missing-config guard, is what fires.
#[compio::test]
async fn tls_new_rejects_zero_observation_channel() {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("bad-opt-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_tls_options(test_tls_options());
  let res =
    Serf::new::<TlsTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
      opts,
      &SocketAddrResolver,
      &FirstAddrResolver,
      VoidDelegate::<SmolStr, SocketAddr>::new(),
      RuntimeOptions::new().with_observation_channel(Channel::Bounded(0)),
      SerfOptions::new(),
      gossip_rng().expect("seed gossip rng"),
      #[cfg(encryption)]
      std::rc::Rc::new(VoidKeyringDelegate),
    )
    .await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero-capacity observation channel must be rejected at construction"),
  }
}

/// Two nodes on loopback: A joins B over a real TLS push-pull exchange; A must
/// observe B joining the cluster through its event stream, then both shut down
/// cleanly.
#[compio::test]
async fn two_node_tls_join_observes_membership() {
  let b = spawn_node("node-b").await;
  let a = spawn_node("node-a").await;
  let b_addr = b.advertise_address();
  let b_id = SmolStr::new("node-b");

  // Subscribe BEFORE the join so a `Member` event cannot race ahead of the
  // subscription (the channel buffers either way, but this is the clean order).
  let mut a_events = a.events();

  // Node A dials node B as its seed.
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

/// The `TlsTransportOptions` getters reflect what the builders set, including
/// the SNI provider closure and the TLS options bundle. The pre-build state is
/// `None` for the required fields the `new()` `ok_or_else` checks arm.
#[test]
fn options_accessors_reflect_builders() {
  let addr: SocketAddr = "127.0.0.1:7946".parse().unwrap();
  // Default options: required fields unset, but the SNI provider has a default
  // (`localhost`) so `sni_provider()` returns a live closure even before build.
  let empty = TlsTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(empty.local_id().is_none());
  assert!(empty.advertise_addr().is_none());
  assert!(empty.tls_options().is_none());
  assert!(empty.stream().validate().is_ok());
  assert_eq!((empty.sni_provider())(&addr), Some("localhost".to_string()));

  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("acc-node"))
    .with_advertise_addr(MaybeResolved::Resolved(addr))
    .with_tls_options(test_tls_options())
    .with_sni_provider(Box::new(|_| Some("peer.example".to_string())));
  assert_eq!(opts.local_id().map(|s| s.as_str()), Some("acc-node"));
  match opts.advertise_addr() {
    Some(MaybeResolved::Resolved(s)) => assert_eq!(*s, addr),
    other => panic!("expected a resolved advertise addr, got {other:?}"),
  }
  assert!(opts.tls_options().is_some());
  // The custom SNI provider overrides the default for every peer.
  assert_eq!(
    (opts.sni_provider())(&addr),
    Some("peer.example".to_string())
  );
}

/// `Default` is the `new()` state: required fields `None`, default SNI provider
/// installed.
#[test]
fn default_matches_new() {
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let d = TlsTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(d.local_id().is_none());
  assert!(d.advertise_addr().is_none());
  assert!(d.tls_options().is_none());
  assert_eq!((d.sni_provider())(&addr), Some("localhost".to_string()));
}

/// Binding the wildcard `0.0.0.0:0` reads an unspecified IP back from the
/// socket; gossiping it would publish an undialable contact, so construction
/// must reject it with `InvalidAdvertiseAddr` (the `tls_options` are supplied so
/// the advertise check, not the missing-config guard, is what fires).
#[compio::test]
async fn new_rejects_wildcard_advertise() {
  let wildcard: SocketAddr = "0.0.0.0:0".parse().expect("wildcard addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("wild-node"))
    .with_advertise_addr(MaybeResolved::Resolved(wildcard))
    .with_tls_options(test_tls_options());
  let res =
    TlsTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
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

/// A construction failure AFTER the sockets are bound must close them (awaited)
/// before returning `Err`, or the bound port leaks and a same-address rebind
/// races into `AddrInUse` (a plain drop is not a synchronous fd release on
/// compio/Windows-IOCP). TLS rides the same stream constructor as plain TCP: a
/// wildcard `0.0.0.0:0` advertise binds a concrete OS-assigned port (free for
/// both the TCP listener and the UDP socket) but is then rejected by
/// `validate_advertise_addr` for its unspecified IP; the exact freed
/// `0.0.0.0:<port>` must immediately re-accept the SAME listener + UDP socket,
/// proving neither leaked on the error path. A real `tls_options` is supplied so
/// the advertise rejection — not the missing-config guard — is what fires.
#[compio::test]
async fn new_failure_closes_bound_sockets_for_rebind() {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("rebind-fail"))
    .with_advertise_addr(MaybeResolved::Resolved(
      "0.0.0.0:0".parse().expect("wildcard addr"),
    ))
    .with_tls_options(test_tls_options());
  let res =
    TlsTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  let freed = match res {
    Err(SerfError::InvalidAdvertiseAddr(e)) => e.addr(),
    Err(other) => panic!("expected a post-bind InvalidAdvertiseAddr failure, got {other:?}"),
    Ok(_) => panic!("a post-bind failure must reject construction, but it succeeded"),
  };

  let listener = compio::net::TcpListener::bind(freed)
    .await
    .expect("the freed TCP port must rebind, not AddrInUse");
  let gossip = compio::net::UdpSocket::bind(freed)
    .await
    .expect("the freed UDP port must rebind, not AddrInUse");
  // Ignoring Err: test cleanup of the probe sockets.
  let _ = listener.close().await;
  let _ = gossip.close().await;
}

/// A zero `dial_timeout` is rejected by `TlsTransport::new` with `InvalidOption`.
/// `options.stream.validate()` runs at the top of `new` — before any socket bind
/// — so a real `tls_options` is supplied to show the dial-timeout rejection is
/// what fires, not a missing-config guard. TLS rides the same stream driver as
/// plain TCP, so the same zero-dial footgun applies.
#[compio::test]
async fn new_rejects_zero_dial_timeout() {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("zero-dial-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_tls_options(test_tls_options())
    .with_stream(StreamTransportOptions::new().with_dial_timeout(Duration::ZERO));
  let res =
    TlsTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero dial_timeout must be rejected at construction"),
  }
}

/// `new` rejects a missing `tls_options` with `InvalidInput`.
#[compio::test]
async fn new_without_tls_options_errors() {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("no-tls"))
    .with_advertise_addr(MaybeResolved::Resolved("127.0.0.1:0".parse().unwrap()));
  let res =
    TlsTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  match res {
    Err(SerfError::Io(e)) => {
      assert_eq!(e.kind(), ErrorKind::InvalidInput);
      assert!(e.to_string().contains("tls_options"));
    }
    Err(other) => panic!("expected InvalidInput(tls_options), got {other:?}"),
    Ok(_) => panic!("a missing tls_options must be rejected, but construction succeeded"),
  }
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

/// Build and spawn a TLS serf node on an ephemeral loopback port with
/// `encryption` installed as its gossip keyring policy.
#[cfg(encryption)]
async fn spawn_encrypted_node(id: &str, encryption: EncryptionOptions) -> Serf<SmolStr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_tls_options(test_tls_options())
    .with_encryption(encryption);
  Serf::new::<TlsTransport<SmolStr, SocketAddr>, SocketAddrResolver, FirstAddrResolver, _, _>(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    gossip_rng().expect("seed gossip rng"),
    std::rc::Rc::new(VoidKeyringDelegate),
  )
  .await
  .expect("spawn serf node")
}

/// Two TLS nodes sharing one gossip keyring: A joins B and must observe B
/// joining through its event stream. The reliable push-pull rides the TLS
/// session, while the gossip datagrams are AEAD-sealed by the configured
/// keyring — proving the keyring reaches the TLS coordinator and that an
/// encrypted TLS cluster forms and interoperates end-to-end.
#[cfg(encryption)]
#[compio::test]
async fn two_node_tls_join_observes_membership_encrypted() {
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
    "node A should observe node B joining the encrypted TLS cluster within the timeout"
  );

  a.shutdown().await.expect("node A shuts down");
  b.shutdown().await.expect("node B shuts down");
}
