use super::{apply_key_request, keyring_carries_cross_cipher_twin};
use memberlist_proto::{Keyring, SecretKey};
use serf_proto::KeyRequestOperation;

#[cfg(feature = "aes-gcm")]
fn aes(b: u8) -> SecretKey {
  SecretKey::Aes256([b; 32])
}

#[cfg(feature = "chacha20-poly1305")]
fn chacha(b: u8) -> SecretKey {
  SecretKey::ChaCha20Poly1305([b; 32])
}

// ── single-backend happy-path (aes-gcm) ───────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn install_adds_secondary_and_rotates() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&aes(2)));
  assert!(out.response().result);
  let rotated = out.rotated().expect("install mutates the ring");
  assert_eq!(rotated.primary_ref(), &aes(1));
  assert!(rotated.secondaries().contains(&aes(2)));
}

#[cfg(feature = "aes-gcm")]
#[test]
fn install_existing_key_is_idempotent_success() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&aes(1)));
  assert!(out.response().result);
  // The dup insert is dropped, so the rotated ring equals the original.
  assert_eq!(out.rotated().expect("install reports a mutation"), &ring);
}

#[cfg(feature = "aes-gcm")]
#[test]
fn use_promotes_exact_secondary() {
  let mut ring = Keyring::new(aes(1));
  ring.insert_secondary(aes(2));
  let out = apply_key_request(&ring, KeyRequestOperation::Use, Some(&aes(2)));
  assert!(out.response().result);
  assert_eq!(
    out
      .rotated()
      .expect("promote mutates the ring")
      .primary_ref(),
    &aes(2)
  );
}

#[cfg(feature = "aes-gcm")]
#[test]
fn use_current_primary_is_trivial_success_without_rotation() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Use, Some(&aes(1)));
  assert!(out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(feature = "aes-gcm")]
#[test]
fn use_absent_key_refused_and_unchanged() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Use, Some(&aes(9)));
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(feature = "aes-gcm")]
#[test]
fn remove_drops_exact_secondary() {
  let mut ring = Keyring::new(aes(1));
  ring.insert_secondary(aes(2));
  let out = apply_key_request(&ring, KeyRequestOperation::Remove, Some(&aes(2)));
  assert!(out.response().result);
  let rotated = out.rotated().expect("remove mutates the ring");
  assert_eq!(rotated.primary_ref(), &aes(1));
  assert!(!rotated.secondaries().contains(&aes(2)));
}

#[cfg(feature = "aes-gcm")]
#[test]
fn remove_primary_refused_and_unchanged() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Remove, Some(&aes(1)));
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(feature = "aes-gcm")]
#[test]
fn remove_absent_key_refused_and_unchanged() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Remove, Some(&aes(9)));
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(feature = "aes-gcm")]
#[test]
fn list_snapshots_state_without_rotation() {
  let mut ring = Keyring::new(aes(1));
  ring.insert_secondary(aes(2));
  let out = apply_key_request(&ring, KeyRequestOperation::List, None);
  assert!(out.response().result);
  assert!(out.rotated().is_none());
  assert_eq!(out.response().primary_key, Some(aes(1)));
  assert!(out.response().keys.contains(&aes(1)));
  assert!(out.response().keys.contains(&aes(2)));
}

#[cfg(feature = "aes-gcm")]
#[test]
fn keyed_op_missing_key_refused() {
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, None);
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

// ── dual-backend variant-exact + cross-cipher-collision regressions ────────────

#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn use_wrong_variant_refused_ring_unchanged() {
  // Ring holds an AES-256 key; a `use` of the SAME BYTES under a different cipher
  // is not exact (variant + bytes) membership and is refused with no mutation.
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Use, Some(&chacha(1)));
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn remove_wrong_variant_refused_ring_unchanged() {
  let mut ring = Keyring::new(aes(1));
  ring.insert_secondary(aes(2));
  // A remove of a ChaCha key byte-twinning the AES secondary is not exact
  // membership: refused, ring intact.
  let out = apply_key_request(&ring, KeyRequestOperation::Remove, Some(&chacha(2)));
  assert!(!out.response().result);
  assert!(out.rotated().is_none());
}

#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn install_cross_cipher_twin_refused() {
  // Installing a ChaCha key whose bytes twin the AES primary is refused so the
  // coordinator's byte-keyed rotation ops stay unambiguous.
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&chacha(1)));
  assert!(!out.response().result);
  assert_eq!(
    out.response().message.as_str(),
    "cross-cipher key collision"
  );
  assert!(out.rotated().is_none());
}

#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn install_distinct_cross_cipher_key_allowed() {
  // A ChaCha key with bytes distinct from every AES key is a legitimate
  // mixed-cipher migration secondary.
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&chacha(2)));
  assert!(out.response().result);
  assert!(
    out
      .rotated()
      .expect("install mutates the ring")
      .secondaries()
      .contains(&chacha(2))
  );
}

#[cfg(all(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn twin_detector_flags_cross_cipher_seed_ring() {
  let mut twinned = Keyring::new(aes(1));
  twinned.insert_secondary(chacha(1));
  assert!(keyring_carries_cross_cipher_twin(&twinned));

  let clean = Keyring::with_secondaries(aes(1), [aes(2), chacha(3)]);
  assert!(!keyring_carries_cross_cipher_twin(&clean));
}
