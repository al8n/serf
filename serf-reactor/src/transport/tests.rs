//! Unit tests for the shared pre-flight gates every backend's `Transport::new`
//! runs: the advertise-address admission check (which rejects a contact peers
//! could not dial or the wire could not carry) and the construction-time
//! cross-cipher keyring refusal.

use std::net::SocketAddr;

use super::validate_advertise_addr;

/// A routable loopback / unicast contact with a concrete port is admitted, in
/// both address families.
#[test]
fn unicast_advertise_addr_is_admitted() {
  for addr in [
    "127.0.0.1:7946",
    "192.168.1.10:7946",
    "8.8.8.8:1",
    "[::1]:7946",
    "[2001:db8::1]:7946",
  ] {
    let addr: SocketAddr = addr.parse().expect("advertise addr");
    assert!(
      validate_advertise_addr(&addr).is_ok(),
      "{addr} is a routable unicast contact and must be admitted"
    );
  }
}

/// The wildcard-bind address is not a contact: its `local_addr()` readback keeps
/// the unspecified IP, so peers would learn a member they cannot dial.
#[test]
fn unspecified_advertise_addr_is_rejected() {
  for addr in ["0.0.0.0:7946", "[::]:7946"] {
    let addr: SocketAddr = addr.parse().expect("advertise addr");
    let err = validate_advertise_addr(&addr).expect_err("an unspecified IP is not a contact");
    assert!(
      matches!(err, crate::SerfError::InvalidAdvertiseAddr(_)),
      "{addr} must be refused as an invalid advertise address, got {err:?}"
    );
  }
}

/// A multicast group address and the IPv4 broadcast address name a group, not a
/// single peer's unicast contact.
#[test]
fn group_advertise_addr_is_rejected() {
  for addr in ["224.0.0.1:7946", "[ff02::1]:7946", "255.255.255.255:7946"] {
    let addr: SocketAddr = addr.parse().expect("advertise addr");
    let err = validate_advertise_addr(&addr).expect_err("a group address is not a unicast contact");
    assert!(
      matches!(err, crate::SerfError::InvalidAdvertiseAddr(_)),
      "{addr} must be refused as an invalid advertise address, got {err:?}"
    );
  }
}

/// A zero port is undialable — the bound socket's readback must have resolved an
/// ephemeral `:0` to a concrete port before this gate runs.
#[test]
fn zero_port_advertise_addr_is_rejected() {
  let addr: SocketAddr = "127.0.0.1:0".parse().expect("advertise addr");
  let err = validate_advertise_addr(&addr).expect_err("a zero port is undialable");
  assert!(
    matches!(err, crate::SerfError::InvalidAdvertiseAddr(_)),
    "a zero port must be refused, got {err:?}"
  );
}

/// The compact `[16B IP][2B port]` wire layout carries neither the IPv6 scope id
/// nor the flow label, so a scoped/flow-labelled address is not representable as
/// a peer-decodable contact and is refused rather than silently truncated.
#[test]
fn scoped_or_flow_labelled_ipv6_advertise_addr_is_rejected() {
  use std::net::{Ipv6Addr, SocketAddrV6};

  let scoped = SocketAddr::V6(SocketAddrV6::new(
    Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
    7946,
    0,
    3,
  ));
  let err = validate_advertise_addr(&scoped).expect_err("a scoped IPv6 addr is not wire-carryable");
  assert!(
    matches!(err, crate::SerfError::InvalidAdvertiseAddr(_)),
    "a nonzero scope_id must be refused, got {err:?}"
  );

  let flow_labelled = SocketAddr::V6(SocketAddrV6::new(
    Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
    7946,
    7,
    0,
  ));
  let err =
    validate_advertise_addr(&flow_labelled).expect_err("a flow label is not wire-carryable");
  assert!(
    matches!(err, crate::SerfError::InvalidAdvertiseAddr(_)),
    "a nonzero flowinfo must be refused, got {err:?}"
  );
}

/// The rejection carries the offending address and a non-empty reason, so an
/// operator can tell WHICH address was refused and why.
#[test]
fn rejection_reports_the_offending_address() {
  let addr: SocketAddr = "0.0.0.0:7946".parse().expect("advertise addr");
  match validate_advertise_addr(&addr) {
    Err(crate::SerfError::InvalidAdvertiseAddr(e)) => {
      assert_eq!(e.addr(), addr, "the error names the refused address");
      assert!(!e.reason().is_empty(), "the error carries a reason");
    }
    other => panic!("expected InvalidAdvertiseAddr, got {other:?}"),
  }
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
