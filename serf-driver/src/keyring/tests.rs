use super::{
  KeyringPersistError, apply_key_request, keyring_carries_cross_cipher_twin,
  settle_parked_key_response,
};
use memberlist_proto::{Keyring, SecretKey};
use serf_proto::{KeyRequestOperation, KeyResponseArgs};

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
fn install_existing_key_is_success_without_rotation() {
  // Re-installing the current primary succeeds but mutates nothing, so no
  // rotated ring is published and the persistence observer never fires on a
  // retried install.
  let ring = Keyring::new(aes(1));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&aes(1)));
  assert!(out.response().result);
  assert!(out.rotated().is_none());

  // The same holds for a key already installed as a secondary.
  let mut ring = Keyring::new(aes(1));
  ring.insert_secondary(aes(2));
  let out = apply_key_request(&ring, KeyRequestOperation::Install, Some(&aes(2)));
  assert!(out.response().result);
  assert!(out.rotated().is_none());
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

// ── the applied outcome's parts ───────────────────────────────────────────────

#[cfg(feature = "aes-gcm")]
#[test]
fn into_parts_yields_the_response_and_the_rotated_ring() {
  let ring = Keyring::new(aes(1));

  let (response, rotated) =
    apply_key_request(&ring, KeyRequestOperation::Install, Some(&aes(2))).into_parts();
  assert!(response.result);
  let rotated = rotated.expect("install mutates the ring");
  assert_eq!(rotated.primary_ref(), &aes(1));
  assert!(rotated.secondaries().contains(&aes(2)));

  // A read-only op splits into a response with no ring to publish, so the
  // caller never re-keys the wire on a `list`.
  let (response, rotated) = apply_key_request(&ring, KeyRequestOperation::List, None).into_parts();
  assert!(response.result);
  assert_eq!(response.primary_key, Some(aes(1)));
  assert!(rotated.is_none());
}

// ── parked key responses gated on persistence ─────────────────────────────────

/// A `list` answer: the shape whose carried key material must survive a
/// persistence downgrade untouched.
#[cfg(feature = "aes-gcm")]
fn listed() -> KeyResponseArgs {
  KeyResponseArgs {
    result: true,
    keys: vec![aes(1), aes(2)],
    primary_key: Some(aes(1)),
    ..Default::default()
  }
}

#[cfg(feature = "aes-gcm")]
#[test]
fn an_unacknowledged_rotation_leaves_the_response_parked() {
  // The sender is alive and silent: persistence is still in flight, so the
  // requester must hear nothing yet.
  let (_worker, rx) = std::sync::mpsc::channel::<Result<(), KeyringPersistError>>();
  assert!(settle_parked_key_response(&rx, &listed()).is_none());
}

#[cfg(feature = "aes-gcm")]
#[test]
fn a_persisted_rotation_releases_the_response_unchanged() {
  let (worker, rx) = std::sync::mpsc::channel();
  worker.send(Ok(())).expect("the receiver is alive");

  let parked = listed();
  let sent = settle_parked_key_response(&rx, &parked).expect("a durable rotation settles");
  assert!(sent.result);
  assert!(
    sent.message.is_empty(),
    "a durable rotation carries no failure message, got {}",
    sent.message
  );
  assert_eq!(sent.keys, parked.keys);
  assert_eq!(sent.primary_key, parked.primary_key);
}

#[cfg(feature = "aes-gcm")]
#[test]
fn a_failed_persistence_downgrades_the_response_and_carries_the_cause() {
  let (worker, rx) = std::sync::mpsc::channel();
  worker
    .send(Err(
      Box::new(std::io::Error::other("the volume is read-only")) as KeyringPersistError,
    ))
    .expect("the receiver is alive");

  let parked = listed();
  let sent = settle_parked_key_response(&rx, &parked).expect("a failed rotation settles");
  assert!(
    !sent.result,
    "a rotation the delegate could not persist must not report success"
  );
  assert!(
    sent.message.contains("not persisted") && sent.message.contains("the volume is read-only"),
    "the failure must name the cause, got {}",
    sent.message
  );
  // Only the verdict is downgraded: the state the apply reported is intact.
  assert_eq!(sent.keys, parked.keys);
  assert_eq!(sent.primary_key, parked.primary_key);
}

#[cfg(feature = "aes-gcm")]
#[test]
fn a_persistence_worker_that_exits_without_acknowledging_is_a_failure() {
  // A worker that vanished mid-rotation must never be read as a silent
  // success, and must never leave the response parked forever.
  let (worker, rx) = std::sync::mpsc::channel::<Result<(), KeyringPersistError>>();
  drop(worker);

  let parked = listed();
  let sent = settle_parked_key_response(&rx, &parked).expect("a disconnected ack settles");
  assert!(!sent.result);
  assert!(
    sent.message.contains("without acknowledging"),
    "the failure must name the vanished worker, got {}",
    sent.message
  );
  assert_eq!(sent.keys, parked.keys);
}

/// A queued acknowledgement is read even after the worker hung up: the value is
/// buffered in the channel, so a rotation that WAS persisted before the worker
/// exited still releases its response as a success.
#[cfg(feature = "aes-gcm")]
#[test]
fn an_acknowledgement_queued_before_the_worker_exited_still_succeeds() {
  let (worker, rx) = std::sync::mpsc::channel();
  worker.send(Ok(())).expect("the receiver is alive");
  drop(worker);

  let sent = settle_parked_key_response(&rx, &listed()).expect("the queued ack settles");
  assert!(
    sent.result,
    "a rotation persisted before the worker exited is durable"
  );
}
