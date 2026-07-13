//! Unit tests for the TCP transport options builder — the pure accessor /
//! builder wiring that feeds `TcpTransport::new`. Real-node construction (bind,
//! join/converge, the SWIM overrides taking effect on the wire) is exercised
//! end-to-end by the suite in `tests/tcp.rs`.

use core::time::Duration;
use std::net::SocketAddr;

use memberlist_proto::MaybeResolved;
use smol_str::SmolStr;

use super::TcpTransportOptions;
use crate::driver::options::StreamTransportOptions;

/// A fresh block carries none of the required fields and no SWIM override — every
/// knob is `None`, which is what makes `Transport::run` keep the coordinator's own
/// defaults and `Transport::new` refuse a half-built block.
#[test]
fn new_starts_empty() {
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new();
  assert!(opts.local_id().is_none());
  assert!(opts.advertise_addr().is_none());
  assert!(opts.push_pull_interval().is_none());
  assert!(opts.probe_interval().is_none());
  assert!(opts.probe_timeout().is_none());
  assert!(opts.gossip_interval().is_none());
  assert!(opts.suspicion_mult().is_none());
  assert!(opts.dead_node_reclaim_time().is_none());
  assert!(opts.suspicion_max_timeout_mult().is_none());
  assert!(opts.stream().validate().is_ok());
  #[cfg(encryption)]
  assert!(
    opts.encryption().keyring().is_none(),
    "the default policy leaves both planes plaintext"
  );
}

/// Every builder writes its OWN field: the accessors read back exactly what was
/// set, with distinct values per knob so a crossed assignment surfaces.
#[test]
fn builders_round_trip_each_knob() {
  let addr: SocketAddr = "127.0.0.1:7946".parse().expect("advertise addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("acc-node"))
    .with_advertise_addr(MaybeResolved::Resolved(addr))
    .with_stream(StreamTransportOptions::new().with_dial_timeout(Duration::from_millis(11)))
    .with_push_pull_interval(Duration::from_millis(1))
    .with_probe_interval(Duration::from_millis(2))
    .with_probe_timeout(Duration::from_millis(3))
    .with_gossip_interval(Duration::from_millis(4))
    .with_suspicion_mult(5)
    .with_dead_node_reclaim_time(Duration::from_millis(6))
    .with_suspicion_max_timeout_mult(7);

  assert_eq!(opts.local_id(), Some(&SmolStr::new("acc-node")));
  match opts.advertise_addr() {
    Some(MaybeResolved::Resolved(s)) => assert_eq!(*s, addr),
    other => panic!("expected a resolved advertise addr, got {other:?}"),
  }
  assert_eq!(opts.stream().dial_timeout(), Duration::from_millis(11));
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
    TcpTransportOptions::<SmolStr, SocketAddr>::new().with_push_pull_interval(Duration::ZERO);
  assert_eq!(
    opts.push_pull_interval(),
    Some(Duration::ZERO),
    "a zero interval disables periodic push/pull; it is not the absent default"
  );
}

/// An unresolved advertise address is retained in its unresolved form — the
/// transport constructor is what resolves it, so the block must not resolve early.
#[test]
fn unresolved_advertise_addr_round_trips_unresolved() {
  let host: hostaddr::HostAddr<SmolStr> = "example.com:7946".parse().expect("host addr");
  let opts = TcpTransportOptions::<SmolStr, hostaddr::HostAddr<SmolStr>>::new()
    .with_advertise_addr(MaybeResolved::Unresolved(host.clone()));
  match opts.advertise_addr() {
    Some(MaybeResolved::Unresolved(h)) => assert_eq!(*h, host),
    other => panic!("expected an unresolved advertise addr, got {other:?}"),
  }
}

/// The gossip-and-reliable keyring reaches the block through the builder.
#[cfg(encryption)]
#[test]
fn encryption_policy_round_trips() {
  use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([0x21; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([0x21; 32]);

  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(key)));
  let keyring = opts
    .encryption()
    .keyring()
    .expect("the configured keyring reaches the options block");
  assert_eq!(
    keyring.primary_ref(),
    &key,
    "the primary key is the one that was configured"
  );
}

/// `Default` is the `new()` state.
#[test]
fn default_matches_new() {
  let d = TcpTransportOptions::<SmolStr, SocketAddr>::default();
  assert!(d.local_id().is_none());
  assert!(d.advertise_addr().is_none());
  assert!(d.probe_interval().is_none());
}

/// A constructed transport reports the identity it was built with: the local id,
/// the advertise input in the ORIGINAL form the caller supplied (an unresolved
/// input stays unresolved — resolution happens for the bind, not for this
/// accessor), and the concrete bound contact the node will gossip.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn transport_reports_its_identity_and_bound_contact() {
  use agnostic::tokio::TokioRuntime;

  use super::TcpTransport;
  use crate::{FirstAddrResolver, SocketAddrResolver, transport::Transport};

  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let transport =
    <TcpTransport<SmolStr, SocketAddr, TokioRuntime> as Transport<TokioRuntime>>::new(
      TcpTransportOptions::new()
        .with_local_id(SmolStr::new("ident"))
        .with_advertise_addr(MaybeResolved::Unresolved(bind)),
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
