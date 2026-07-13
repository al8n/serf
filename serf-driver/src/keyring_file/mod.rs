//! [`KeyringFilePersistence`] — the shared file-persistence engine behind the
//! per-runtime `FileKeyringDelegate` types.

use std::{
  io,
  path::{Path, PathBuf},
  sync::mpsc,
};

use memberlist_proto::{Keyring, SecretKey};
use serf_proto::{secret_key_from_bytes, secret_key_to_bytes};

use crate::keyring::KeyringPersistError;

/// The file-persistence engine each runtime's `FileKeyringDelegate` wraps: it
/// persists every enqueued keyring rotation to a file and acknowledges each
/// write's durability, plus a [`load`](Self::load) to rebuild the ring at
/// construction — the turnkey replacement for the legacy keyring-file option,
/// with the persistence app-owned like any other delegate.
///
/// FORMAT: one lowercase-hex line per key, the PRIMARY key first, each line
/// decoding to `[algorithm_tag][raw_key_bytes]`. The leading tag byte keeps
/// the encoding self-describing, so two ciphers sharing a key length (AES-256
/// and ChaCha20-Poly1305 are both 32 bytes) stay distinguishable. The file
/// holds raw key material by design — protect it with filesystem permissions
/// exactly as the reference implementation's keyring file required.
///
/// Rotations are handed to a dedicated persistence thread — `keyring_updated`
/// runs inline on the driver pump, which must never block on storage, so the
/// callback only clones the ring into an unbounded channel (rotations are
/// rare and small) and the worker does the file I/O, acknowledging each write
/// back through [`KeyringPersistence::Pending`](crate::keyring::KeyringPersistence::Pending)
/// so the driver can gate the
/// key response on durability. Each write goes through an exclusively-created,
/// owner-only (`0600` on Unix), unpredictably-named sibling temp file, synced,
/// then atomically renamed over the destination: a crash mid-write never
/// truncates the previous ring, a restrictive mode on the key file is never
/// widened by a rotation, and key bytes can never land in a pre-existing
/// inode or behind a planted symlink. Dropping the delegate joins the
/// worker after it drains every queued rotation, so a shutdown cannot discard
/// a write that was already acknowledged toward the wire.
///
/// UNIX-ONLY: the acknowledgement contract is rename durability — the
/// containing directory is synced before a rotation reports success — and no
/// safe standard API can flush a directory entry on Windows, so this turnkey
/// delegate does not exist there rather than acknowledge a rotation a power
/// loss could revert. A Windows application implements its runtime's
/// `KeyringDelegate` trait itself with a platform-durable strategy (a
/// write-through rename via the platform APIs, or storage with its own
/// durability contract).
pub struct KeyringFilePersistence {
  path: PathBuf,
  /// Hand-off to the persistence thread; `None` only during drop, which hangs
  /// up first so the worker drains and exits.
  worker: Option<mpsc::Sender<PersistJob>>,
  /// The persistence thread, joined on drop after the hang-up.
  handle: Option<std::thread::JoinHandle<()>>,
}

/// One queued rotation: the ring to write and the acknowledgement sender the
/// pump's parked key response polls.
struct PersistJob {
  ring: Keyring,
  ack: mpsc::Sender<Result<(), KeyringPersistError>>,
}

impl KeyringFilePersistence {
  /// A delegate persisting to `path`.
  ///
  /// Construction sweeps stale sibling temp files — the fixed-name temp
  /// earlier releases wrote (whose permissions predate the owner-only
  /// guarantee) and abandoned temps from crashed rotations — before the
  /// first write can race one.
  pub fn new(path: impl Into<PathBuf>) -> Self {
    let path: PathBuf = path.into();
    sweep_stale_temps(&path);
    let (worker, jobs) = mpsc::channel::<PersistJob>();
    let worker_path = path.clone();
    let handle = std::thread::spawn(move || {
      while let Ok(job) = jobs.recv() {
        let res = persist(&worker_path, &job.ring);
        if let Err(_err) = &res {
          #[cfg(feature = "tracing")]
          tracing::warn!(
            path = %worker_path.display(),
            error = %_err,
            "serf keyring rotation could not be persisted; the wire keeps the rotated ring"
          );
        }
        // Ignoring Err: the pump dropped this rotation's receiver (teardown,
        // or the requester's deadline passed) — the outcome has nowhere to go.
        let _ = job
          .ack
          .send(res.map_err(|e| Box::new(e) as KeyringPersistError));
      }
    });
    Self {
      path,
      worker: Some(worker),
      handle: Some(handle),
    }
  }

  /// The persistence path.
  #[must_use]
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Load a keyring previously persisted by this delegate.
  ///
  /// Returns `Ok(None)` when the file does not exist (first boot). The first
  /// line is the primary key; the rest are secondaries in decrypt-trial order.
  ///
  /// # Errors
  ///
  /// [`KeyringFileError::Io`] on a read failure other than not-found;
  /// [`KeyringFileError::Parse`] on a malformed line (bad hex, an unknown
  /// algorithm tag, a key length not matching its tag, or an empty file).
  pub fn load(&self) -> Result<Option<Keyring>, KeyringFileError> {
    let raw = match std::fs::read_to_string(&self.path) {
      Ok(s) => s,
      Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
      Err(e) => return Err(KeyringFileError::Io(e)),
    };
    let mut keys = raw
      .lines()
      .map(str::trim)
      .filter(|l| !l.is_empty())
      .map(parse_key_line);
    let primary = keys
      .next()
      .transpose()?
      .ok_or_else(|| KeyringFileError::Parse("the keyring file holds no keys".into()))?;
    let secondaries = keys.collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Keyring::with_secondaries(primary, secondaries)))
  }
}

/// Serialize `keyring` into the file format (primary first, one hex line per
/// key) and write it via an exclusively-created owner-only temp file, sync,
/// and atomic rename.
fn persist(path: &Path, keyring: &Keyring) -> Result<(), KeyringFileError> {
  use zeroize::Zeroize as _;
  let mut out = String::new();
  push_key_line(&mut out, keyring.primary_ref());
  for key in keyring.secondaries() {
    push_key_line(&mut out, key);
  }
  let res = write_via_exclusive_temp(path, out.as_bytes()).map_err(KeyringFileError::Io);
  out.zeroize();
  res
}

/// Write `contents` to `path` through an exclusively-created, owner-only,
/// unpredictably-named sibling temp file, synced then atomically renamed over
/// the destination.
///
/// `create_new` (`O_CREAT | O_EXCL`) never reuses an existing inode and never
/// follows a symlink — a file or link already sitting at the temp path fails
/// the attempt instead of receiving the key bytes — and the OS-entropy name
/// keeps such a path from being plantable ahead of time. The inode is born
/// `0600` on Unix and re-asserted on the open handle, so raw key material
/// only ever lands in a fresh owner-only inode this process created; a crash
/// mid-write never truncates the previous ring.
fn write_via_exclusive_temp(path: &Path, contents: &[u8]) -> io::Result<()> {
  use std::io::Write as _;
  let name = path
    .file_name()
    .and_then(|n| n.to_str())
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "keyring path has no file name"))?;
  let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
  let mut attempts = 0u8;
  let (tmp, mut file) = loop {
    let tmp_name = format!(".{name}.{:016x}.tmp", temp_nonce()?);
    let tmp = match dir {
      Some(d) => d.join(&tmp_name),
      None => PathBuf::from(&tmp_name),
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    {
      use std::os::unix::fs::OpenOptionsExt as _;
      opts.mode(0o600);
    }
    match opts.open(&tmp) {
      Ok(file) => break (tmp, file),
      // A 64-bit OS-entropy collision is practically a squatted path; a
      // bounded retry with a fresh nonce outlasts any accidental leftover
      // without spinning against a directory an attacker keeps filling.
      Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 16 => attempts += 1,
      Err(e) => return Err(e),
    }
  };
  let res = (|| {
    {
      use std::os::unix::fs::PermissionsExt as _;
      file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    // The rename is not durable until the DIRECTORY entry is: a crash after
    // returning `Ok` here must not revert the destination to the old file —
    // the acknowledgement built on this return is what releases a successful
    // key response to the cluster.
    sync_dir(dir.unwrap_or_else(|| Path::new(".")))
  })();
  if res.is_err() {
    // Ignoring Err: removing the failed temp (it holds key bytes) is
    // best-effort hygiene; the write error itself is what propagates. A
    // failure after the rename consumed the temp removes nothing.
    let _ = std::fs::remove_file(&tmp);
  }
  res
}

/// Sync a directory so a completed rename of an entry inside it survives a
/// crash.
fn sync_dir(dir: &Path) -> io::Result<()> {
  std::fs::File::open(dir)?.sync_all()
}

/// OS-entropy nonce for a temp-file name: unpredictable, so a
/// directory-writing attacker cannot pre-plant a file or symlink at the next
/// temp path.
fn temp_nonce() -> io::Result<u64> {
  use rand::TryRng as _;
  rand::rngs::SysRng.try_next_u64().map_err(io::Error::other)
}

/// Remove leftovers a rotation can no longer reuse: the fixed-name sibling
/// temp earlier releases wrote (whose permissions predate the owner-only
/// guarantee and may already hold key material), and the exact-shape
/// `.{name}.{16 hex}.tmp` temps a crashed rotation abandoned. Every removal
/// is gated on FILESYSTEM IDENTITY against the destination — a candidate
/// that resolves to the destination's storage (lexical identity, a
/// case-folding filesystem, or a symlink on either side) is never touched,
/// so sweeping hygiene can never delete the persisted keyring. Removing a
/// planted symlink unlinks the LINK, never its target. Best-effort: a sweep
/// failure never blocks construction — `create_new` already keeps every
/// future write off any path that survives.
fn sweep_stale_temps(path: &Path) {
  if legacy_temp_provably_distinct(path) && sweepable(path, &path.with_extension("tmp")) {
    // Ignoring Err: nothing to sweep, or no permission — both non-fatal.
    let _ = std::fs::remove_file(path.with_extension("tmp"));
  }
  let (Some(dir), Some(name)) = (
    path.parent().filter(|d| !d.as_os_str().is_empty()),
    path.file_name().and_then(|n| n.to_str()),
  ) else {
    return;
  };
  let prefix = format!(".{name}.");
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let file_name = entry.file_name();
    let Some(f) = file_name.to_str() else {
      continue;
    };
    // Exact-shape match only — a sibling file that merely shares the prefix
    // and suffix (an operator's own backup, say) is not this delegate's to
    // delete.
    let matches_temp_shape = f
      .strip_prefix(&prefix)
      .and_then(|rest| rest.strip_suffix(".tmp"))
      .is_some_and(|mid| mid.len() == 16 && mid.bytes().all(|b| b.is_ascii_hexdigit()));
    if matches_temp_shape && sweepable(path, &entry.path()) {
      // Ignoring Err: best-effort sweep of abandoned temps.
      let _ = std::fs::remove_file(entry.path());
    }
  }
}

/// Whether the fixed-name legacy temp's name is PROVABLY distinct from the
/// destination's under every supported filename-alias relation.
///
/// The legacy image differs from the destination only in its extension, so
/// the alias question reduces to the extension replacement. Sweeping is
/// allowed only when the destination's extension is pure ASCII and does not
/// ASCII-case-fold to `tmp` — case-insensitive HFS+ additionally IGNORES
/// certain Unicode scalars when comparing names, so an extension carrying
/// any non-ASCII scalar could fold the two names together and is
/// conservatively refused. No extension at all is safe: the image then
/// APPENDS `.tmp`, four non-ignorable ASCII characters no folding can
/// absorb. Random-suffix candidates need no such classifier — their names
/// carry a dot prefix and a 16-hex infix the destination's name does not,
/// an excess of non-ignorable ASCII no alias relation can erase.
fn legacy_temp_provably_distinct(path: &Path) -> bool {
  match path.extension() {
    None => true,
    Some(e) => e
      .to_str()
      .is_some_and(|e| e.is_ascii() && !e.eq_ignore_ascii_case("tmp")),
  }
}

/// Whether removing `candidate` cannot touch the keyring the destination
/// `path` reaches.
///
/// `remove_file` unlinks a NAME, so the guards are layered by what a name
/// can do: a candidate whose name can itself name the destination —
/// lexically, or equal under the ASCII case folding that aliases names on
/// case-insensitive filesystems — is NEVER sweepable, because a rotation's
/// rename can land between any identity observation and the unlink, and the
/// unlink would then remove whatever the destination name holds (the freshly
/// persisted keyring). Only for genuinely distinct names — where the unlink
/// cannot remove the destination's entry — is resolved filesystem identity
/// consulted: `metadata` FOLLOWS symlinks, so each side resolves to the file
/// a reader would actually open, and a candidate reaching the destination's
/// storage through a symlink or hard link is skipped.
fn sweepable(path: &Path, candidate: &Path) -> bool {
  use std::os::unix::fs::MetadataExt as _;
  if candidate == path {
    return false;
  }
  match (
    candidate.file_name().and_then(|n| n.to_str()),
    path.file_name().and_then(|n| n.to_str()),
  ) {
    (Some(c), Some(p)) if c.eq_ignore_ascii_case(p) => return false,
    (Some(_), Some(_)) => {}
    // Un-inspectable names: never delete on uncertainty.
    _ => return false,
  }
  if std::fs::symlink_metadata(candidate).is_err() {
    // Nothing at the candidate name; removal would be a no-op.
    return false;
  }
  let candidate_meta = std::fs::metadata(candidate);
  // The window between the two observations is where a concurrent rename
  // lands; the tests widen it deterministically to prove the name guards
  // above — not luck — are what keep a mid-check rename safe.
  #[cfg(test)]
  tests::between_identity_observations(path);
  let destination_meta = std::fs::metadata(path);
  match (candidate_meta, destination_meta) {
    // A dangling symlink at the candidate name reaches no storage at all.
    (Err(e), _) if e.kind() == io::ErrorKind::NotFound => true,
    (Ok(c), Ok(d)) => (c.dev(), c.ino()) != (d.dev(), d.ino()),
    // The destination resolves to nothing: with the name-aliasing shapes
    // already excluded above, an existing candidate cannot BE the missing
    // destination — the two are genuinely distinct.
    (Ok(_), Err(e)) if e.kind() == io::ErrorKind::NotFound => true,
    // Identity cannot be established: never delete on uncertainty.
    _ => false,
  }
}

impl KeyringFilePersistence {
  /// Hand one rotation to the persistence worker, returning the receiver its
  /// durability acknowledgement resolves — the value a `keyring_updated`
  /// implementation wraps in `KeyringPersistence::Pending`.
  ///
  /// Non-blocking: the pump must never wait on storage. A failed hand-off
  /// (the worker already hung up) drops the job — and with it the ack sender
  /// — so the returned receiver disconnects and the pump reports the rotation
  /// unpersisted rather than silently acknowledged.
  pub fn enqueue(&self, keyring: &Keyring) -> crate::keyring::KeyringPersistRx {
    let (ack, rx) = mpsc::channel();
    if let Some(worker) = &self.worker {
      // Ignoring Err: see above — the dropped job's disconnected receiver IS
      // the failure signal.
      let _ = worker.send(PersistJob {
        ring: keyring.clone(),
        ack,
      });
    }
    rx
  }
}

impl Drop for KeyringFilePersistence {
  fn drop(&mut self) {
    // Hang up, then join: the worker drains every queued rotation before it
    // exits, so a shutdown cannot discard a write the wire already carries.
    self.worker = None;
    if let Some(handle) = self.handle.take() {
      // Ignoring Err: a panicked worker already surfaced its failure through
      // the acknowledgement channel; there is nothing to unwind into here.
      let _ = handle.join();
    }
  }
}

/// Errors from [`KeyringFilePersistence::load`].
#[derive(Debug, thiserror::Error)]
pub enum KeyringFileError {
  /// Reading or writing the keyring file failed.
  #[error(transparent)]
  Io(#[from] io::Error),
  /// The file's contents are not a valid keyring serialization.
  #[error("malformed keyring file: {0}")]
  Parse(String),
}

/// Append one key as a lowercase-hex `[tag][bytes]` line. The transient
/// tagged buffer is zeroed before it is freed.
fn push_key_line(out: &mut String, key: &SecretKey) {
  use core::fmt::Write as _;
  let tagged = secret_key_to_bytes(key);
  for b in tagged.iter() {
    // Ignoring Err: writing hex digits into a String cannot fail.
    let _ = write!(out, "{b:02x}");
  }
  out.push('\n');
}

/// Parse one lowercase-hex `[tag][bytes]` line into a [`SecretKey`].
fn parse_key_line(line: &str) -> Result<SecretKey, KeyringFileError> {
  use zeroize::Zeroize as _;
  if line.len() % 2 != 0 {
    return Err(KeyringFileError::Parse("odd-length hex key line".into()));
  }
  let mut buf = Vec::with_capacity(line.len() / 2);
  for pair in line.as_bytes().chunks_exact(2) {
    let hi = hex_val(pair[0]);
    let lo = hex_val(pair[1]);
    match (hi, lo) {
      (Some(h), Some(l)) => buf.push((h << 4) | l),
      _ => {
        buf.zeroize();
        return Err(KeyringFileError::Parse(
          "non-hex character in key line".into(),
        ));
      }
    }
  }
  let bytes = bytes::Bytes::copy_from_slice(&buf);
  buf.zeroize();
  secret_key_from_bytes(&bytes).map_err(|e| KeyringFileError::Parse(e.to_string()))
}

/// The value of one lowercase/uppercase hex digit.
fn hex_val(c: u8) -> Option<u8> {
  match c {
    b'0'..=b'9' => Some(c - b'0'),
    b'a'..=b'f' => Some(c - b'a' + 10),
    b'A'..=b'F' => Some(c - b'A' + 10),
    _ => None,
  }
}

#[cfg(test)]
mod tests;
