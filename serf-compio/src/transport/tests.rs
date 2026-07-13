//! The construction-time advertise-address gate.
//!
//! Every transport reads its advertise `SocketAddr` back from the bound socket
//! and gossips it as the local node's contact identity, so an address peers
//! cannot dial — or cannot even decode off the compact `[16B IP][2B port]` wire
//! layout — must be refused at construction rather than published to the
//! cluster. The classes here are unreachable through a successful bind on a
//! normal host (a listener never hands back a multicast, broadcast, or
//! zero-port address), so they are pinned directly against the validator.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use super::validate_advertise_addr;
use crate::SerfError;

/// The reason string the validator attached, or a panic naming what it returned
/// instead of the expected `InvalidAdvertiseAddr` rejection.
fn rejection_reason(addr: SocketAddr) -> String {
  match validate_advertise_addr(&addr) {
    Err(SerfError::InvalidAdvertiseAddr(e)) => {
      assert_eq!(
        e.addr(),
        addr,
        "the rejection carries the address that was refused"
      );
      e.to_string()
    }
    Err(other) => panic!("expected InvalidAdvertiseAddr for {addr}, got {other:?}"),
    Ok(()) => panic!("{addr} must be refused as an advertise address, but it was accepted"),
  }
}

/// A routable unicast contact — loopback, private, or global — is accepted: the
/// gate refuses undialable classes, it does not narrow the deployment surface.
#[test]
fn a_routable_unicast_contact_is_accepted() {
  for addr in [
    "127.0.0.1:7946",
    "192.168.1.10:7946",
    "8.8.8.8:7946",
    "[::1]:7946",
    "[2001:db8::1]:7946",
  ] {
    let addr: SocketAddr = addr.parse().expect("a valid socket address");
    validate_advertise_addr(&addr)
      .unwrap_or_else(|e| panic!("{addr} is a routable unicast contact but was refused: {e}"));
  }
}

/// The wildcard bind address is the classic footgun: its `local_addr()` readback
/// keeps the unspecified IP, which peers cannot dial, so the node would join as a
/// member no peer can reach and be suspected and reaped.
#[test]
fn an_unspecified_ip_is_refused() {
  assert!(
    rejection_reason("0.0.0.0:7946".parse().expect("v4 wildcard")).contains("unspecified"),
    "the IPv4 wildcard is refused as the wildcard-bind address"
  );
  assert!(
    rejection_reason("[::]:7946".parse().expect("v6 wildcard")).contains("unspecified"),
    "the IPv6 wildcard is refused as the wildcard-bind address"
  );
}

/// A multicast IP names a GROUP, not a single peer's unicast contact, so it is
/// not a usable identity to gossip.
#[test]
fn a_multicast_ip_is_refused() {
  assert!(
    rejection_reason("224.0.0.1:7946".parse().expect("v4 multicast")).contains("multicast"),
    "an IPv4 multicast group is not a unicast contact"
  );
  assert!(
    rejection_reason("[ff02::1]:7946".parse().expect("v6 multicast")).contains("multicast"),
    "an IPv6 multicast group is not a unicast contact"
  );
}

/// The IPv4 broadcast address is a v4-only concept the multicast check does not
/// catch, so it has its own rejection.
#[test]
fn the_ipv4_broadcast_ip_is_refused() {
  let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 7946));
  assert!(
    rejection_reason(addr).contains("broadcast"),
    "255.255.255.255 is not a unicast contact"
  );
}

/// A zero port is undialable: the bound socket's `local_addr()` readback must
/// carry the concrete OS-assigned port, so a zero here means the readback was
/// skipped.
#[test]
fn a_zero_port_is_refused() {
  assert!(
    rejection_reason("127.0.0.1:0".parse().expect("zero-port v4")).contains("zero port"),
    "a zero port is undialable"
  );
  assert!(
    rejection_reason("[::1]:0".parse().expect("zero-port v6")).contains("zero port"),
    "a zero port is undialable"
  );
}

/// The compact `[16B IP][2B port]` wire layout carries neither `scope_id` nor
/// `flowinfo`, so a scoped or flow-labelled IPv6 advertise address would decode
/// on a peer as a DIFFERENT, unroutable contact. Both fields are rejected
/// independently.
#[test]
fn a_scoped_or_flow_labelled_ipv6_is_refused() {
  let scoped = SocketAddr::V6(SocketAddrV6::new(
    "fe80::1".parse::<Ipv6Addr>().expect("link-local v6"),
    7946,
    0,
    3,
  ));
  assert!(
    rejection_reason(scoped).contains("scope_id"),
    "a nonzero scope_id is not representable on the wire layout"
  );

  let flow_labelled = SocketAddr::V6(SocketAddrV6::new(
    "2001:db8::1".parse::<Ipv6Addr>().expect("global v6"),
    7946,
    0x1234,
    0,
  ));
  assert!(
    rejection_reason(flow_labelled).contains("flowinfo"),
    "a nonzero flowinfo is not representable on the wire layout"
  );

  // The same address with both fields zeroed is a perfectly good contact — the
  // gate keys on the fields, not on the address family or its link-local scope.
  let plain = SocketAddr::V6(SocketAddrV6::new(
    "2001:db8::1".parse::<Ipv6Addr>().expect("global v6"),
    7946,
    0,
    0,
  ));
  validate_advertise_addr(&plain).expect("an unscoped IPv6 unicast contact is accepted");
}

/// A seed keyring whose keys collide across ciphers — the same raw bytes under
/// two cipher variants — is refused at construction: the coordinator's rotation
/// ops match on bytes alone, so such a ring would let a later `use`/`remove`
/// promote or drop the wrong cipher's key. A same-cipher multi-key ring, and a
/// cross-cipher ring with DISTINCT bytes, both stay admissible.
#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn cross_cipher_twin_keyring_is_rejected_at_construction() {
  use memberlist_proto::{EncryptionOptions, Keyring, SecretKey};

  use super::reject_cross_cipher_keyring;

  let aes = |b: u8| SecretKey::Aes256([b; 32]);
  let chacha = |b: u8| SecretKey::ChaCha20Poly1305([b; 32]);

  let mut twinned = Keyring::new(aes(1));
  twinned.insert_secondary(chacha(1));
  let err = reject_cross_cipher_keyring(&EncryptionOptions::new().with_keyring(twinned))
    .expect_err("a cross-cipher byte twin makes every later key op ambiguous");
  assert!(
    matches!(err, crate::SerfError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput),
    "the twin refusal is an InvalidInput, got {err:?}"
  );

  let clean = Keyring::with_secondaries(aes(1), [aes(2), chacha(3)]);
  assert!(
    reject_cross_cipher_keyring(&EncryptionOptions::new().with_keyring(clean)).is_ok(),
    "distinct key bytes across ciphers are unambiguous and stay admissible"
  );

  assert!(
    reject_cross_cipher_keyring(&EncryptionOptions::new()).is_ok(),
    "a node with no keyring configured has nothing to refuse"
  );
}
