//! Unit tests for the QUIC transport options builder — the pure accessor / builder
//! wiring that feeds `QuicTransport::new`. Real-node construction (advertise
//! validation, single-socket bind, freed-port rebind) is exercised end-to-end by
//! the tokio suite in `tests/quic.rs`.

use super::QuicTransportOptions;
use memberlist_proto::MaybeResolved;
use smol_str::SmolStr;
use std::net::SocketAddr;

/// A fresh `QuicTransportOptions` carries no id / advertise / config until the
/// builder sets them.
#[test]
fn new_starts_empty() {
  let opts = QuicTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(opts.local_id().is_none());
  assert!(opts.advertise_addr().is_none());
  assert!(opts.quic_config().is_none());
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
