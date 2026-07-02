//! Unit tests for the TLS transport options builder — the pure accessor / builder
//! wiring that feeds `TlsTransport::new`. Real-node construction (the rustls
//! record-layer handshake, join/converge, freed-port rebind) is exercised
//! end-to-end by the tokio suite in `tests/tls.rs`.

use std::{net::SocketAddr, sync::Arc};

use memberlist_proto::MaybeResolved;
use rustls::{ClientConfig, RootCertStore, ServerConfig, crypto::CryptoProvider, version::TLS13};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use smol_str::SmolStr;

use super::{TlsOptions, TlsTransportOptions};

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
