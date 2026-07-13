//! Unit tests for the TLS transport options builder — the pure accessor / builder
//! wiring that feeds `TlsTransport::new`. Real-node construction (the rustls
//! record-layer handshake, join/converge, freed-port rebind) is exercised
//! end-to-end by the tokio suite in `tests/tls.rs`.

use core::time::Duration;
use std::{net::SocketAddr, sync::Arc};

use memberlist_proto::MaybeResolved;
use rustls::{ClientConfig, RootCertStore, ServerConfig, crypto::CryptoProvider, version::TLS13};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use smol_str::SmolStr;

use super::{TlsOptions, TlsTransportOptions};
use crate::driver::options::StreamTransportOptions;

/// The process-default crypto provider, falling back to ring for the dev build.
fn crypto_provider() -> Arc<CryptoProvider> {
  CryptoProvider::get_default()
    .cloned()
    .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// Build a self-signed localhost-SAN `ServerConfig` + a root-verifying
/// `ClientConfig` bundle. The option-accessor tests only need a valid
/// [`TlsOptions`] to prove the builder round-trips it; the real-node handshake
/// (which uses an accept-any client verifier) lives in `tests/tls.rs`.
fn test_tls_options() -> TlsOptions {
  let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()])
    .expect("rcgen generate_simple_self_signed");
  let cert = CertificateDer::from(ck.cert.der().to_vec());
  let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
  let provider = crypto_provider();

  let server = ServerConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key)
    .expect("valid self-signed cert");

  let mut roots = RootCertStore::empty();
  roots.add(cert).expect("add root cert");
  let client = ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&TLS13])
    .expect("TLS 1.3 supported")
    .with_root_certificates(roots)
    .with_no_client_auth();

  TlsOptions::new(server, client)
}

/// The `TlsTransportOptions` getters reflect what the builders set, including the
/// SNI provider closure and the TLS options bundle. The pre-build state is `None`
/// for the required fields the `new()` `ok_or_else` checks arm.
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

/// A fresh block carries no SWIM override — every knob is `None`, which is what
/// makes `Transport::run` keep the coordinator's own defaults.
#[test]
fn new_starts_with_no_swim_override() {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new();
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
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
    .with_stream(StreamTransportOptions::new().with_close_timeout(Duration::from_millis(12)))
    .with_push_pull_interval(Duration::from_millis(1))
    .with_probe_interval(Duration::from_millis(2))
    .with_probe_timeout(Duration::from_millis(3))
    .with_gossip_interval(Duration::from_millis(4))
    .with_suspicion_mult(5)
    .with_dead_node_reclaim_time(Duration::from_millis(6))
    .with_suspicion_max_timeout_mult(7);

  assert_eq!(opts.stream().close_timeout(), Duration::from_millis(12));
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
/// anti-entropy), so it must round-trip as `Some(ZERO)` — never collapse back to
/// the `None` that means "keep the coordinator default".
#[test]
fn zero_push_pull_interval_is_set_not_unset() {
  let opts =
    TlsTransportOptions::<SmolStr, SocketAddr>::new().with_push_pull_interval(Duration::ZERO);
  assert_eq!(opts.push_pull_interval(), Some(Duration::ZERO));
}

/// The SNI provider is consulted PER PEER, so a provider that maps each dialed
/// address to its own name must be carried unflattened.
#[test]
fn sni_provider_is_consulted_per_peer() {
  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new().with_sni_provider(Box::new(|a| {
    (a.port() != 9).then(|| format!("peer-{}.example", a.port()))
  }));
  let sni = opts.sni_provider();
  assert_eq!(
    sni(&"127.0.0.1:1".parse().unwrap()),
    Some("peer-1.example".to_string())
  );
  assert_eq!(
    sni(&"127.0.0.1:2".parse().unwrap()),
    Some("peer-2.example".to_string())
  );
  assert_eq!(
    sni(&"127.0.0.1:9".parse().unwrap()),
    None,
    "a provider may refuse a peer, which aborts the dial before the handshake"
  );
}

/// The gossip keyring reaches the block through the builder. (On TLS it seals
/// only the gossip datagrams — the reliable plane rides the TLS session.)
#[cfg(encryption)]
#[test]
fn encryption_policy_round_trips() {
  use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([0x31; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([0x31; 32]);

  let opts = TlsTransportOptions::<SmolStr, SocketAddr>::new()
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

/// `Default` is the `new()` state: required fields `None`, default SNI provider
/// installed.
#[test]
fn default_matches_new() {
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let d = TlsTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(d.local_id().is_none());
  assert!(d.advertise_addr().is_none());
  assert!(d.tls_options().is_none());
  assert!(d.probe_interval().is_none());
  assert_eq!((d.sni_provider())(&addr), Some("localhost".to_string()));
}

/// A constructed transport reports the identity it was built with: the local id,
/// the advertise input in the ORIGINAL form the caller supplied (an unresolved
/// input stays unresolved — resolution happens for the bind, not for this
/// accessor), and the concrete bound contact the node will gossip.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn transport_reports_its_identity_and_bound_contact() {
  use agnostic::tokio::TokioRuntime;

  use super::TlsTransport;
  use crate::{FirstAddrResolver, SocketAddrResolver, transport::Transport};

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let transport =
    <TlsTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
      TlsTransportOptions::new()
        .with_local_id(SmolStr::new("ident"))
        .with_advertise_addr(MaybeResolved::Unresolved(bind))
        .with_tls_options(test_tls_options()),
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

/// The TLS transport needs BOTH planes: the reliable TLS-over-TCP listener and the
/// plain-UDP gossip socket on the same port. When the gossip port is already taken,
/// construction FAILS rather than coming up with a reliable plane and no gossip.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn taken_gossip_port_fails_construction() {
  use agnostic::tokio::TokioRuntime;

  use super::TlsTransport;
  use crate::{FirstAddrResolver, SocketAddrResolver, transport::Transport};

  let squatter = std::net::UdpSocket::bind("127.0.0.1:0").expect("squat a UDP port");
  let taken = squatter.local_addr().expect("the squatted address");

  let err = <TlsTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
    TlsTransportOptions::new()
      .with_local_id(SmolStr::new("squatted"))
      .with_advertise_addr(MaybeResolved::Resolved(taken))
      .with_tls_options(test_tls_options()),
    &SocketAddrResolver,
    &FirstAddrResolver,
  )
  .await
  .err()
  .expect("a node cannot come up without its gossip plane");
  assert!(
    matches!(err, crate::SerfError::Io(_)),
    "a taken gossip port is an I/O failure, got {err:?}"
  );

  // Freeing the port makes the very same construction succeed — the failure was
  // the squatter, not the address.
  drop(squatter);
  let transport =
    <TlsTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
      TlsTransportOptions::new()
        .with_local_id(SmolStr::new("unsquatted"))
        .with_advertise_addr(MaybeResolved::Resolved(taken))
        .with_tls_options(test_tls_options()),
      &SocketAddrResolver,
      &FirstAddrResolver,
    )
    .await
    .expect("the released gossip port lets the transport bind");
  assert_eq!(*transport.advertise_address(), taken);
}

/// A resolver that resolves the advertise address to NO candidate fails
/// construction rather than booting a node with no reachable contact.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn advertise_resolution_failure_fails_construction() {
  use agnostic::tokio::TokioRuntime;

  use super::TlsTransport;
  use crate::{FirstAddrResolver, Resolver, transport::Transport};

  /// Resolves nothing — a bootstrap outage the advertise picker must refuse.
  struct EmptyResolver;
  impl Resolver for EmptyResolver {
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

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let err = <TlsTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
    TlsTransportOptions::new()
      .with_local_id(SmolStr::new("unresolvable"))
      .with_advertise_addr(MaybeResolved::Unresolved(bind))
      .with_tls_options(test_tls_options()),
    &EmptyResolver,
    &FirstAddrResolver,
  )
  .await
  .err()
  .expect("an advertise address that resolves to nothing cannot boot a node");
  assert!(
    matches!(err, crate::SerfError::Resolve(_)),
    "an empty candidate set is a resolution failure, got {err:?}"
  );
}
