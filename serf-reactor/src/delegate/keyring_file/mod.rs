//! [`FileKeyringDelegate`] — turnkey file persistence for keyring rotations.

use std::{
  io,
  path::{Path, PathBuf},
  sync::mpsc,
};

use memberlist_proto::{Keyring, SecretKey};
use serf_proto::{secret_key_from_bytes, secret_key_to_bytes};

use super::KeyringDelegate;

/// A [`KeyringDelegate`] that persists every keyring rotation to a file, plus
/// a [`load`](Self::load) to rebuild the ring at construction — the turnkey
/// replacement for the legacy keyring-file option, with the persistence
/// app-owned like any other delegate.
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
/// rare and small) and the worker does the file I/O. Each write goes to a
/// sibling temp file created with owner-only permissions (`0600` on Unix),
/// synced, then atomically renamed over the destination, so a crash mid-write
/// never truncates the previous ring and a restrictive mode on the key file
/// is never widened by a rotation. `keyring_updated` has no error channel; a
/// failed write is surfaced through `tracing` (a no-op without the `tracing`
/// feature) and the wire keeps the rotated ring regardless — the file is the
/// durable copy, not the live one.
pub struct FileKeyringDelegate {
  path: PathBuf,
  /// Hand-off to the persistence thread; dropping the delegate drops the
  /// sender, and the worker exits after draining what was queued.
  worker: mpsc::Sender<Keyring>,
}

impl FileKeyringDelegate {
  /// A delegate persisting to `path`.
  pub fn new(path: impl Into<PathBuf>) -> Self {
    let path: PathBuf = path.into();
    let (worker, jobs) = mpsc::channel::<Keyring>();
    let worker_path = path.clone();
    std::thread::spawn(move || {
      while let Ok(ring) = jobs.recv() {
        if let Err(_err) = persist(&worker_path, &ring) {
          #[cfg(feature = "tracing")]
          tracing::warn!(
            path = %worker_path.display(),
            error = %_err,
            "serf keyring rotation could not be persisted; the wire keeps the rotated ring"
          );
        }
      }
    });
    Self { path, worker }
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
/// key) and write it via an owner-only temp file, sync, and atomic rename.
fn persist(path: &Path, keyring: &Keyring) -> Result<(), KeyringFileError> {
  use std::io::Write as _;
  use zeroize::Zeroize as _;
  let mut out = String::new();
  push_key_line(&mut out, keyring.primary_ref());
  for key in keyring.secondaries() {
    push_key_line(&mut out, key);
  }
  let tmp = path.with_extension("tmp");
  let mut opts = std::fs::OpenOptions::new();
  opts.create(true).truncate(true).write(true);
  // The file holds raw key material: the temp inode that will REPLACE the
  // destination is born owner-only, so a rotation can never widen a
  // restrictive mode on the key file (`fs::write` would inherit the umask).
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt as _;
    opts.mode(0o600);
  }
  let res = opts
    .open(&tmp)
    .and_then(|mut file| {
      file.write_all(out.as_bytes())?;
      file.sync_all()
    })
    .and_then(|()| std::fs::rename(&tmp, path))
    .map_err(KeyringFileError::Io);
  out.zeroize();
  res
}

impl KeyringDelegate for FileKeyringDelegate {
  fn keyring_updated(&self, keyring: &Keyring) {
    // Non-blocking hand-off: the pump must never wait on storage. Ignoring
    // Err: the worker exits only when this sender is dropped, so a send can
    // only fail during teardown, where losing the final rotation write is
    // acceptable (the wire state is authoritative).
    let _ = self.worker.send(keyring.clone());
  }
}

/// Errors from [`FileKeyringDelegate::load`].
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
