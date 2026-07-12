use super::*;

use memberlist_proto::SecretKey;

fn tmp_path(name: &str) -> PathBuf {
  let mut p = std::env::temp_dir();
  p.push(format!(
    "serf-reactor-keyring-{name}-{}",
    std::process::id()
  ));
  // Ignoring Err: a leftover file from a previous run is fine to lose.
  let _ = std::fs::remove_file(&p);
  p
}

fn test_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([fill; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// A rotation handed through the DELEGATE trait persists and acknowledges:
/// `keyring_updated` returns a pending acknowledgement that resolves once the
/// write is durable, and `load` rebuilds the persisted ring.
#[test]
fn rotation_round_trips_through_the_delegate() {
  let path = tmp_path("roundtrip");
  let delegate = FileKeyringDelegate::new(&path);
  assert_eq!(delegate.path(), path.as_path());

  let ring = Keyring::new(test_key(1));
  let KeyringPersistence::Pending(ack) = delegate.keyring_updated(&ring) else {
    panic!("a file-backed rotation is never durable inline");
  };
  ack
    .recv_timeout(std::time::Duration::from_secs(5))
    .expect("the persistence worker acknowledges within the bound")
    .expect("the rotation persists");

  let loaded = delegate
    .load()
    .expect("an acknowledged write parses")
    .expect("an acknowledged write exists");
  assert_eq!(loaded.primary_ref(), ring.primary_ref());

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Dropping the delegate flushes the queue through the engine's worker join:
/// a rotation enqueued immediately before the drop is on disk when `drop`
/// returns.
#[test]
fn drop_flushes_queued_rotations() {
  let path = tmp_path("drop-flush");
  let delegate = FileKeyringDelegate::new(&path);
  let key = test_key(2);
  // The acknowledgement is intentionally not awaited: the drop is the barrier.
  let _pending = delegate.keyring_updated(&Keyring::new(key));
  drop(delegate);

  let loaded = FileKeyringDelegate::new(&path)
    .load()
    .expect("the flushed write parses")
    .expect("the flushed write exists");
  assert_eq!(loaded.primary_ref(), &key);

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// First boot: a missing file loads as `None`, not an error.
#[test]
fn missing_file_loads_as_none() {
  let delegate = FileKeyringDelegate::new(tmp_path("missing"));
  assert!(
    delegate
      .load()
      .expect("not-found is not an error")
      .is_none()
  );
}
