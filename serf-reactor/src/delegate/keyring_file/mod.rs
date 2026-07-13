//! [`FileKeyringDelegate`] — turnkey file persistence for keyring rotations,
//! backed by the shared [`serf_driver::KeyringFilePersistence`] engine.

use std::path::{Path, PathBuf};

use memberlist_proto::Keyring;
pub use serf_driver::KeyringFileError;
use serf_driver::KeyringFilePersistence;

use super::{KeyringDelegate, KeyringPersistence};

/// A [`KeyringDelegate`] that persists every keyring rotation to a file, plus
/// a [`load`](Self::load) to rebuild the ring at construction — the turnkey
/// replacement for the legacy keyring-file option, with the persistence
/// app-owned like any other delegate.
///
/// The file mechanics — the self-describing hex format, the exclusively
/// created owner-only temps, the identity-gated stale-temp sweep, and the
/// directory-synced rename the acknowledgement waits on — live in the shared
/// [`KeyringFilePersistence`] engine; see its documentation for the format
/// and durability contract (including why the engine, and therefore this
/// delegate, is Unix-only).
pub struct FileKeyringDelegate {
  engine: KeyringFilePersistence,
}

impl FileKeyringDelegate {
  /// A delegate persisting to `path`.
  ///
  /// Construction sweeps stale sibling temp files — the fixed-name temp
  /// earlier releases wrote (whose permissions predate the owner-only
  /// guarantee) and abandoned temps from crashed rotations — before the
  /// first write can race one.
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self {
      engine: KeyringFilePersistence::new(path),
    }
  }

  /// The persistence path.
  #[must_use]
  pub fn path(&self) -> &Path {
    self.engine.path()
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
    self.engine.load()
  }
}

impl KeyringDelegate for FileKeyringDelegate {
  fn keyring_updated(&self, keyring: &Keyring) -> KeyringPersistence {
    KeyringPersistence::Pending(self.engine.enqueue(keyring))
  }
}

#[cfg(test)]
mod tests;
