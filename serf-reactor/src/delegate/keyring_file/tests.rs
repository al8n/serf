use super::*;

use std::cell::RefCell;

/// This thread's mid-check hook, when a test installed one.
type ObservationGap = RefCell<Option<Box<dyn FnMut(&Path)>>>;

thread_local! {
  /// Per-test hook fired between `sweepable`'s two identity observations —
  /// the window a concurrent rotation's rename can land in. Thread-local so
  /// parallel tests never see each other's hooks.
  static BETWEEN_OBSERVATIONS: ObservationGap = const { RefCell::new(None) };
}

/// Called by `sweepable` between its two `metadata` observations.
pub(super) fn between_identity_observations(path: &Path) {
  BETWEEN_OBSERVATIONS.with(|hook| {
    if let Some(f) = hook.borrow_mut().as_mut() {
      f(path);
    }
  });
}

/// Install `f` as this thread's mid-check hook for the duration of `run`.
fn with_observation_gap(f: impl FnMut(&Path) + 'static, run: impl FnOnce()) {
  BETWEEN_OBSERVATIONS.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
  run();
  BETWEEN_OBSERVATIONS.with(|hook| *hook.borrow_mut() = None);
}

/// Wait for one rotation's persistence acknowledgement.
fn acked(p: KeyringPersistence) -> Result<(), KeyringPersistError> {
  match p {
    KeyringPersistence::Durable => Ok(()),
    KeyringPersistence::Pending(rx) => rx
      .recv_timeout(std::time::Duration::from_secs(5))
      .expect("the persistence worker acknowledges within the bound"),
  }
}

fn tmp_path(name: &str) -> PathBuf {
  let mut p = std::env::temp_dir();
  p.push(format!("serf-keyring-file-{name}-{}", std::process::id()));
  p
}

/// Any sibling temp file this delegate could have produced for `path`: the
/// legacy fixed-name temp or a random-suffix one.
fn temp_residue(path: &Path) -> Vec<PathBuf> {
  let mut residue = Vec::new();
  let legacy = path.with_extension("tmp");
  if legacy.symlink_metadata().is_ok() {
    residue.push(legacy);
  }
  let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
    return residue;
  };
  let prefix = format!(".{name}.");
  for entry in std::fs::read_dir(dir).expect("temp dir listable").flatten() {
    if let Some(f) = entry.file_name().to_str()
      && f.starts_with(&prefix)
      && f.ends_with(".tmp")
    {
      residue.push(entry.path());
    }
  }
  residue
}

/// A persisted rotation round-trips: the acknowledged `keyring_updated` write
/// (primary first) is on disk, and `load` rebuilds it with the same primary
/// and secondaries.
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
  acked(delegate.keyring_updated(&ring)).expect("the rotation persists");

  let loaded = delegate
    .load()
    .expect("an acknowledged write parses")
    .expect("an acknowledged write exists");
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
/// no temp-file residue remains under either naming scheme.
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

  acked(delegate.keyring_updated(&Keyring::new(first))).expect("first rotation persists");
  acked(delegate.keyring_updated(&Keyring::new(second))).expect("second rotation persists");

  let loaded = delegate
    .load()
    .expect("parses")
    .expect("the file exists after two rotations");
  assert_eq!(loaded.primary_ref(), &second);
  assert!(loaded.secondaries().is_empty());
  assert!(
    temp_residue(&path).is_empty(),
    "the atomic rename must consume every temp file"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// A rotation never widens the key file's mode — and it NARROWS a permissive
/// one: the replacing temp inode is born owner-only, so a fresh file is
/// created `0600` and a pre-existing `0644` destination is `0600` after the
/// next rotation, even under a permissive umask.
#[test]
fn rotation_enforces_owner_only_permissions() {
  use std::os::unix::fs::PermissionsExt as _;

  let path = tmp_path("perms");
  // A permissive pre-existing destination (an operator's hand-created file).
  std::fs::write(&path, "junk\n").expect("pre-create the destination");
  std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
    .expect("widen the destination");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let (first, second) = (SecretKey::Aes128([5u8; 16]), SecretKey::Aes128([6u8; 16]));
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let (first, second) = (
    SecretKey::ChaCha20Poly1305([5u8; 32]),
    SecretKey::ChaCha20Poly1305([6u8; 32]),
  );

  acked(delegate.keyring_updated(&Keyring::new(first))).expect("first rotation persists");
  let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
  assert_eq!(
    mode, 0o600,
    "the first rotation narrows a permissive destination to owner-only"
  );

  acked(delegate.keyring_updated(&Keyring::new(second))).expect("second rotation persists");
  let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
  assert_eq!(mode, 0o600, "a rotation must not widen the key file's mode");

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// Construction sweeps both classes of stale sibling temps: the fixed-name
/// temp earlier releases wrote (possibly permissive and already holding key
/// material) and abandoned random-suffix temps from crashed rotations.
#[test]
fn construction_sweeps_stale_temps() {
  let path = tmp_path("sweep");
  let legacy = path.with_extension("tmp");
  std::fs::write(&legacy, "stale key bytes at permissive mode\n").expect("plant the legacy temp");
  let name = path.file_name().and_then(|n| n.to_str()).expect("name");
  let abandoned = path.with_file_name(format!(".{name}.00000000deadbeef.tmp"));
  std::fs::write(&abandoned, "abandoned partial write\n").expect("plant the abandoned temp");

  let _delegate = FileKeyringDelegate::new(&path);
  assert!(
    !legacy.exists(),
    "the legacy fixed-name temp must be swept at construction"
  );
  assert!(
    !abandoned.exists(),
    "an abandoned random-suffix temp must be swept at construction"
  );
}

/// A symlink planted at the legacy temp path is unlinked — the LINK, never
/// its target — and no rotation ever writes through it: the exclusive
/// creation refuses any pre-existing path, so key bytes cannot be redirected
/// into an attacker-chosen file.
#[test]
fn a_planted_symlink_never_receives_key_bytes() {
  let path = tmp_path("symlink");
  let victim = tmp_path("symlink-victim");
  std::fs::write(&victim, "victim contents\n").expect("create the victim");
  let planted = path.with_extension("tmp");
  // Ignoring Err: a leftover link from a previous run is about to be re-planted.
  let _ = std::fs::remove_file(&planted);
  std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");

  let delegate = FileKeyringDelegate::new(&path);
  assert!(
    planted.symlink_metadata().is_err(),
    "construction unlinks the planted symlink"
  );
  assert_eq!(
    std::fs::read_to_string(&victim).expect("victim readable"),
    "victim contents\n",
    "unlinking removes the LINK, never its target"
  );

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([7u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([7u8; 32]);
  acked(delegate.keyring_updated(&Keyring::new(key))).expect("rotation persists");
  assert_eq!(
    std::fs::read_to_string(&victim).expect("victim readable"),
    "victim contents\n",
    "no rotation writes through a planted path"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_file(&victim);
}

/// A destination whose own extension is `tmp` is NOT its legacy temp: the
/// construction sweep must preserve it — the previous implementation wrote
/// such a destination in place, so the file can hold the only copy of the
/// keyring.
#[test]
fn a_tmp_extension_destination_survives_construction() {
  let path = tmp_path("selfnamed").with_extension("tmp");
  // Ignoring Err: a leftover file from a previous run is about to be rewritten.
  let _ = std::fs::remove_file(&path);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([10u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([10u8; 32]);
  {
    let delegate = FileKeyringDelegate::new(&path);
    acked(delegate.keyring_updated(&Keyring::new(key))).expect("the rotation persists");
  }

  let reopened = FileKeyringDelegate::new(&path);
  let loaded = reopened
    .load()
    .expect("the persisted keyring parses")
    .expect("constructing a delegate must not sweep a .tmp-named destination");
  assert_eq!(loaded.primary_ref(), &key);

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// The alias guard is case-insensitive: a destination named with an
/// uppercase `TMP` extension lexically differs from its lowercase
/// `with_extension` image, yet the two alias the same file on the
/// case-insensitive filesystems that are the default on macOS — construction
/// must preserve it.
#[test]
fn an_uppercase_tmp_destination_survives_construction() {
  let path = tmp_path("selfnamed-upper").with_extension("TMP");
  // Ignoring Err: a leftover file from a previous run is about to be rewritten.
  let _ = std::fs::remove_file(&path);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([12u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([12u8; 32]);
  {
    let delegate = FileKeyringDelegate::new(&path);
    acked(delegate.keyring_updated(&Keyring::new(key))).expect("the rotation persists");
  }

  let reopened = FileKeyringDelegate::new(&path);
  let loaded = reopened
    .load()
    .expect("the persisted keyring parses")
    .expect("constructing a delegate must not sweep a case-aliased .TMP destination");
  assert_eq!(loaded.primary_ref(), &key);

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// A symlinked destination keeps its target through construction: with the
/// configured path pointing at a file that happens to live at the legacy
/// temp name, the sweep resolves filesystem IDENTITY — not names — and
/// leaves the keyring intact for the follow-up load.
#[test]
fn a_symlinked_destination_keeps_its_target_through_construction() {
  let target = tmp_path("linked").with_extension("tmp");
  let link = tmp_path("linked").with_extension("current");
  // Ignoring Err: leftovers from a previous run are about to be recreated.
  let _ = std::fs::remove_file(&link);
  let _ = std::fs::remove_file(&target);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([13u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([13u8; 32]);
  {
    // Persist a real ring at the target path (itself a tmp-named
    // destination, which construction must already preserve).
    let seed = FileKeyringDelegate::new(&target);
    acked(seed.keyring_updated(&Keyring::new(key))).expect("the seed rotation persists");
  }
  std::os::unix::fs::symlink(&target, &link).expect("plant the destination symlink");

  let delegate = FileKeyringDelegate::new(&link);
  let loaded = delegate
    .load()
    .expect("the symlinked keyring parses")
    .expect("construction must not sweep the storage a symlinked destination resolves to");
  assert_eq!(loaded.primary_ref(), &key);

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&link);
  let _ = std::fs::remove_file(&target);
}

/// The legacy-sweep classifier admits only extensions provably distinct
/// from `tmp` under every supported filename-alias relation: pure-ASCII
/// non-tmp extensions (or none, where the image appends characters no
/// folding can absorb) pass; any tmp case-fold or any non-ASCII scalar —
/// case-insensitive HFS+ ignores certain Unicode scalars when comparing
/// names, so `ring.t\u{200D}mp` aliases `ring.tmp` there — is refused.
#[test]
fn the_legacy_sweep_classifier_refuses_unprovable_extensions() {
  for (destination, distinct) in [
    ("ring.keys", true),
    ("ring", true),
    ("ring.tmp", false),
    ("ring.TMP", false),
    ("ring.Tmp", false),
    // An HFS+-ignorable scalar (ZERO WIDTH JOINER) inside the extension.
    ("ring.t\u{200D}mp", false),
    // Any non-ASCII scalar is unprovable, ignorable or not.
    ("ring.cl\u{00E9}s", false),
  ] {
    assert_eq!(
      legacy_temp_provably_distinct(Path::new(destination)),
      distinct,
      "{destination:?}"
    );
  }
}

/// A destination whose extension is not provably distinct keeps its legacy
/// image on EVERY filesystem: whether or not the running filesystem folds
/// the two names together, the conservative refusal leaves the sibling file
/// alone — the fold-aliasing filesystems are exactly where that sibling IS
/// the persisted keyring.
#[test]
fn an_unprovable_extension_keeps_the_legacy_sibling() {
  let path = tmp_path("ignorable").with_extension("t\u{200D}mp");
  let sibling = path.with_extension("tmp");
  std::fs::write(&sibling, "possibly the persisted keyring\n").expect("seed the sibling");

  let _delegate = FileKeyringDelegate::new(&path);
  assert!(
    sibling.exists(),
    "an unprovable extension must refuse the legacy sweep"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&sibling);
  let _ = std::fs::remove_file(&path);
}

/// A rename landing between the sweep's two identity observations must
/// never lose the destination: `remove_file` unlinks a NAME, so a candidate
/// whose name can itself name the destination is refused by the name guard
/// BEFORE any identity observation — the widened observation gap here
/// atomically replaces the destination exactly as a concurrent rotation
/// would, and construction must leave the replacement intact.
#[test]
fn a_rename_landing_mid_check_never_loses_the_destination() {
  let path = tmp_path("raced").with_extension("tmp");
  std::fs::write(&path, "pre-rotation contents\n").expect("seed the destination");

  with_observation_gap(
    |dest: &Path| {
      // The concurrent rotation: a fresh inode atomically renamed over the
      // destination, mid-check.
      let staged = dest.with_file_name(".raced-replacement");
      std::fs::write(&staged, "freshly persisted contents\n").expect("stage the replacement");
      std::fs::rename(&staged, dest).expect("land the replacement");
    },
    || {
      let _delegate = FileKeyringDelegate::new(&path);
    },
  );

  // With the name guard in place the observation gap is never reached for a
  // name-aliasing candidate, so the original contents remain; what must hold
  // in every world is that the destination NAME was not unlinked.
  let contents =
    std::fs::read_to_string(&path).expect("the destination survives construction un-unlinked");
  assert!(
    contents == "pre-rotation contents\n" || contents == "freshly persisted contents\n",
    "the destination holds one of the two written generations, never nothing"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}

/// The success acknowledgement is durability: a rotation whose parent
/// directory cannot be synced reports failure, because the completed rename
/// is not crash-durable until the directory entry is.
#[test]
fn an_unsyncable_directory_fails_the_acknowledgement() {
  use std::os::unix::fs::PermissionsExt as _;

  let mut dir = std::env::temp_dir();
  dir.push(format!("serf-keyring-unsync-{}", std::process::id()));
  // Ignoring Err: a leftover directory from a previous run is fine to reuse.
  let _ = std::fs::create_dir(&dir);
  std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("open the dir");
  let path = dir.join("ring");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([11u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([11u8; 32]);

  // Write+search without read: the temp creation, write, and rename all
  // succeed, but the directory handle needed for the durability sync cannot
  // be opened — the acknowledgement must report that as a failure.
  std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300))
    .expect("make the dir unsyncable");
  let outcome = acked(delegate.keyring_updated(&Keyring::new(key)));
  std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("restore the dir");
  assert!(
    outcome.is_err(),
    "an un-syncable rename must not acknowledge success"
  );

  // Ignoring Err: best-effort test-tree cleanup.
  let _ = std::fs::remove_dir_all(&dir);
}

/// A write failure is acknowledged as an error — the response gate's failure
/// signal — not silently swallowed.
#[test]
fn persistence_failure_is_acknowledged_as_an_error() {
  let mut path = std::env::temp_dir();
  path.push(format!("serf-keyring-no-such-dir-{}", std::process::id()));
  path.push("ring");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([8u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([8u8; 32]);
  assert!(
    acked(delegate.keyring_updated(&Keyring::new(key))).is_err(),
    "a write into a missing directory must acknowledge failure"
  );
}

/// Dropping the delegate joins the worker after it drains the queue: a
/// rotation handed off immediately before the drop is on disk when `drop`
/// returns, so a shutdown cannot discard a write the wire already carries.
#[test]
fn drop_joins_the_worker_and_flushes_queued_rotations() {
  let path = tmp_path("drop-flush");
  let delegate = FileKeyringDelegate::new(&path);

  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes128([9u8; 16]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([9u8; 32]);

  let ack = delegate.keyring_updated(&Keyring::new(key));
  drop(delegate);

  // No waiting: the join inside drop already flushed the queue.
  let loaded = FileKeyringDelegate::new(&path)
    .load()
    .expect("the flushed write parses")
    .expect("the flushed write exists");
  assert_eq!(loaded.primary_ref(), &key);
  assert!(
    matches!(ack, KeyringPersistence::Pending(rx) if matches!(rx.try_recv(), Ok(Ok(())))),
    "the queued rotation was acknowledged before the worker exited"
  );

  // Ignoring Err: best-effort test-file cleanup.
  let _ = std::fs::remove_file(&path);
}
