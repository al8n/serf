//! Unit tests for the QUIC transport options builder — the pure accessor / builder
//! wiring that feeds `QuicTransport::new`. Real-node construction (advertise
//! validation, single-socket bind, freed-port rebind) is exercised end-to-end by
//! the tokio suite in `tests/quic.rs`.

use super::QuicTransportOptions;
use core::time::Duration;
use memberlist_proto::MaybeResolved;
use smol_str::SmolStr;
use std::net::SocketAddr;

/// A fresh `QuicTransportOptions` carries no id / advertise / config until the
/// builder sets them, and no SWIM override — every knob is `None`, which is what
/// makes `Transport::run` keep the coordinator's own defaults.
#[test]
fn new_starts_empty() {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(opts.local_id().is_none());
  assert!(opts.advertise_addr().is_none());
  assert!(opts.quic_config().is_none());
  assert!(opts.push_pull_interval().is_none());
  assert!(opts.probe_interval().is_none());
  assert!(opts.probe_timeout().is_none());
  assert!(opts.gossip_interval().is_none());
  assert!(opts.suspicion_mult().is_none());
  assert!(opts.dead_node_reclaim_time().is_none());
  assert!(opts.suspicion_max_timeout_mult().is_none());
  #[cfg(encryption)]
  assert!(
    opts.encryption().keyring().is_none(),
    "the default policy leaves the gossip datagrams plaintext"
  );
}

/// Every builder writes its OWN field: the accessors read back exactly what was
/// set, with distinct values per knob so a crossed assignment surfaces.
#[test]
fn builders_round_trip_each_knob() {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
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
/// anti-entropy, isolating the gossip datagram plane), so it must round-trip as
/// `Some(ZERO)` — never collapse back to the `None` that means "keep the
/// coordinator default".
#[test]
fn zero_push_pull_interval_is_set_not_unset() {
  let opts =
    QuicTransportOptions::<SmolStr, SocketAddr>::new().with_push_pull_interval(Duration::ZERO);
  assert_eq!(opts.push_pull_interval(), Some(Duration::ZERO));
}

/// An unresolved advertise address is retained in its unresolved form — the
/// transport constructor is what resolves it, so the block must not resolve early.
#[test]
fn unresolved_advertise_addr_round_trips_unresolved() {
  let host: hostaddr::HostAddr<SmolStr> = "example.com:7946".parse().expect("host addr");
  let opts = QuicTransportOptions::<SmolStr, hostaddr::HostAddr<SmolStr>>::new()
    .with_advertise_addr(MaybeResolved::Unresolved(host.clone()));
  match opts.advertise_addr() {
    Some(MaybeResolved::Unresolved(h)) => assert_eq!(*h, host),
    other => panic!("expected an unresolved advertise addr, got {other:?}"),
  }
}

/// The gossip keyring reaches the block through the builder. (On QUIC it seals
/// only the datagram plane — the reliable plane rides quinn's own TLS.)
#[cfg(encryption)]
#[test]
fn encryption_policy_round_trips() {
  use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([0x41; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([0x41; 32]);

  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(key)));
  assert_eq!(
    opts
      .encryption()
      .keyring()
      .expect("the configured keyring reaches the options block")
      .primary_ref(),
    &key
  );
}

/// The builder setters round-trip through the accessors (the id and advertise the
/// transport constructor requires).
#[test]
fn builders_round_trip() {
  let addr: SocketAddr = "127.0.0.1:8300".parse().expect("addr");
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("node-a"))
    .with_advertise_addr(MaybeResolved::Resolved(addr));

  assert_eq!(opts.local_id(), Some(&SmolStr::new("node-a")));
  match opts.advertise_addr() {
    Some(MaybeResolved::Resolved(s)) => assert_eq!(*s, addr),
    other => panic!("expected a resolved advertise addr, got {other:?}"),
  }
}

/// `Default` delegates to `new` — the empty starting point.
#[test]
fn default_matches_new() {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(opts.local_id().is_none());
  assert!(opts.quic_config().is_none());
}

/// A constructed transport reports the identity it was built with: the local id,
/// the advertise input in the ORIGINAL form the caller supplied (an unresolved
/// input stays unresolved — resolution happens for the bind, not for this
/// accessor), and the concrete bound contact the node will gossip.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn transport_reports_its_identity_and_bound_contact() {
  use core::time::Duration;
  use std::sync::Arc;

  use agnostic::tokio::TokioRuntime;
  use memberlist_proto::UnreliableTransport;
  use rustls::version::TLS13;
  use rustls_pki_types::{CertificateDer, PrivateKeyDer};

  use super::{QuicOptions, QuicTransport};
  use crate::{FirstAddrResolver, SocketAddrResolver, transport::Transport};

  /// A minimal quinn bundle: a fresh self-signed localhost cert, an accept-any
  /// client, and datagram-mode gossip. Nothing dials in this test — the bundle
  /// only has to be well-formed enough for the transport to bind.
  fn test_quic_options() -> QuicOptions {
    let ck =
      rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("self-signed cert");
    let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let rustls_server = rustls::ServerConfig::builder_with_provider(provider.clone())
      .with_protocol_versions(&[&TLS13])
      .expect("tls13 server")
      .with_no_client_auth()
      .with_single_cert(chain, key)
      .expect("single cert");
    let server = quinn_proto::ServerConfig::with_crypto(Arc::new(
      quinn_proto::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_server))
        .expect("quic server config"),
    ));

    let roots = rustls::RootCertStore::empty();
    let rustls_client = rustls::ClientConfig::builder_with_provider(provider)
      .with_protocol_versions(&[&TLS13])
      .expect("tls13 client")
      .with_root_certificates(roots)
      .with_no_client_auth();
    let client = quinn_proto::ClientConfig::new(Arc::new(
      quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_client))
        .expect("quic client config"),
    ));

    let hmac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[0x5au8; 32]);
    let mut transport = quinn_proto::TransportConfig::default();
    transport.max_idle_timeout(Some(
      quinn_proto::IdleTimeout::try_from(Duration::from_secs(20)).expect("idle timeout"),
    ));
    QuicOptions::new(
      quinn_proto::EndpointConfig::new(Arc::new(hmac)),
      server,
      client,
      transport,
      "localhost",
      UnreliableTransport::Datagram,
    )
  }

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let transport =
    <QuicTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
      QuicTransportOptions::new()
        .with_local_id(SmolStr::new("ident"))
        .with_advertise_addr(MaybeResolved::Unresolved(bind))
        .with_quic_config(test_quic_options()),
      &SocketAddrResolver,
      &FirstAddrResolver,
    )
    .await
    .expect("the transport binds an ephemeral loopback port");

  assert_eq!(transport.local_id(), &SmolStr::new("ident"));
  match transport.local_address() {
    MaybeResolved::Unresolved(a) => assert_eq!(*a, bind),
    other => panic!("the advertise INPUT form must be retained, got {other:?}"),
  }
  let advertise = *transport.advertise_address();
  assert!(advertise.ip().is_loopback());
  assert_ne!(
    advertise.port(),
    0,
    "the bound contact carries the OS-assigned port, not the ephemeral `:0`"
  );
}
