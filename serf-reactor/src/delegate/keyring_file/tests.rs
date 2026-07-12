use super::*;

/// Bounded wait for the persistence worker to materialize the file state
/// `pred` accepts (rotations hand off to a dedicated thread).
fn await_file<T>(mut probe: impl FnMut() -> Option<T>) -> T {
  for _ in 0..200 {
    if let Some(v) = probe() {
      return v;
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
  }
  panic!("the persistence worker did not materialize the expected file state");
}

fn tmp_path(name: &str) -> PathBuf {
  let mut p = std::env::temp_dir();
  p.push(format!("serf-keyring-file-{name}-{}", std::process::id()));
  p
}

/// A persisted rotation round-trips: `keyring_updated` writes the ring
/// (primary first), `load` rebuilds it with the same primary and secondaries.
#[test]
fn rotation_round_trips_through_the_file() {
  let path = tmp_path("roundtrip");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let (primary, secondary) = (SecretKey::Aes128([1u8; 16]), SecretKey::Aes256([2u8; 32]));
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let (primary, secondary) = (
    SecretKey::ChaCha20Poly1305([1u8; 32]),
    SecretKey::ChaCha20Poly1305([2u8; 32]),
  );

  let ring = Keyring::with_secondaries(primary, [secondary]);
  delegate.keyring_updated(&ring);

  let loaded = await_file(|| delegate.load().ok().flatten());
  assert_eq!(loaded.primary_ref(), ring.primary_ref());
  assert_eq!(loaded.secondaries(), ring.secondaries());

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

/// Malformed contents are a parse error, never a panic: odd-length hex,
/// non-hex characters, an unknown algorithm tag, and an empty file.
#[test]
fn malformed_files_are_parse_errors() {
  for (name, contents) in [
    ("odd", "abc\n"),
    ("nonhex", "zz\n"),
    ("badtag", "ff00112233445566778899aabbccddeeff\n"),
    ("empty", "\n"),
  ] {
    let path = tmp_path(name);
    std::fs::write(&path, contents).expect("write test file");
    let delegate = FileKeyringDelegate::new(&path);
    assert!(
      matches!(delegate.load(), Err(KeyringFileError::Parse(_))),
      "{name} must be a parse error"
    );
    // Ignoring Err: best-effort test-file cleanup.
    let _ = std::fs::remove_file(&path);
  }
}

/// A second rotation atomically replaces the file: the newest ring wins and
/// no temp-file residue remains.
#[test]
fn a_second_rotation_replaces_the_first() {
  let path = tmp_path("replace");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let (first, second) = (SecretKey::Aes128([3u8; 16]), SecretKey::Aes128([4u8; 16]));
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let (first, second) = (
    SecretKey::ChaCha20Poly1305([3u8; 32]),
    SecretKey::ChaCha20Poly1305([4u8; 32]),
  );

  delegate.keyring_updated(&Keyring::new(first));
  delegate.keyring_updated(&Keyring::new(second));

  let loaded = await_file(|| {
    delegate
      .load()
      .ok()
      .flatten()
      .filter(|r| r.primary_ref() == &second)
  });
  assert_eq!(loaded.primary_ref(), &second);
  assert!(loaded.secondaries().is_empty());
  assert!(
    !path.with_extension("tmp").exists(),
    "the atomic rename must consume the temp file"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// A rotation never widens the key file's mode: the replacing temp inode is
/// born owner-only, so a `0600` destination stays `0600` (and a fresh file is
/// created `0600`), even under a permissive umask.
#[cfg(unix)]
#[test]
fn rotation_preserves_owner_only_permissions() {
  use std::os::unix::fs::PermissionsExt as _;

  let path = tmp_path("perms");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let (first, second) = (SecretKey::Aes128([5u8; 16]), SecretKey::Aes128([6u8; 16]));
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let (first, second) = (
    SecretKey::ChaCha20Poly1305([5u8; 32]),
    SecretKey::ChaCha20Poly1305([6u8; 32]),
  );

  delegate.keyring_updated(&Keyring::new(first));
  await_file(|| path.exists().then_some(()));
  let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "a fresh key file is born owner-only");

  delegate.keyring_updated(&Keyring::new(second));
  await_file(|| {
    delegate
      .load()
      .ok()
      .flatten()
      .filter(|r| r.primary_ref() == &second)
  });
  let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "a rotation must not widen the key file's mode");

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}
