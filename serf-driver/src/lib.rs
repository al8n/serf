#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/serf/main/art/logo_72x72.png")]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

pub mod error;
#[cfg(all(encryption, any(feature = "tcp", feature = "quic")))]
mod keyring;
#[cfg(all(encryption, any(feature = "tcp", feature = "quic"), unix))]
mod keyring_file;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod observation;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod snapshot;
mod snapshotter;

#[cfg(all(encryption, any(feature = "tcp", feature = "quic")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(
    any(feature = "aes-gcm", feature = "chacha20-poly1305"),
    any(feature = "tcp", feature = "quic")
  )))
)]
pub use keyring::{
  AppliedKeyRequest, KEYRING_PERSIST_POLL_INTERVAL, KeyApplyOutcome, KeyringPersistError,
  KeyringPersistRx, KeyringPersistence, PendingKeyResponse, apply_key_request,
  keyring_carries_cross_cipher_twin, settle_parked_key_response,
};
#[cfg(all(encryption, any(feature = "tcp", feature = "quic"), unix))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(
    any(feature = "aes-gcm", feature = "chacha20-poly1305"),
    any(feature = "tcp", feature = "quic"),
    unix
  )))
)]
pub use keyring_file::{KeyringFileError, KeyringFilePersistence};
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use observation::observation_payload_bytes;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use snapshot::{SerfSnapshot, SerfStats};
/// Whether this build of the shared engines compiles their `tracing`
/// telemetry — the persistence-failure warnings in the snapshotter and the
/// keyring-file engine. The runtime crates' `tracing` features must forward
/// here; their wiring tests assert this constant so a dropped forward fails
/// loudly instead of silencing operator diagnostics.
pub const TRACING_WIRED: bool = cfg!(feature = "tracing");

pub use snapshotter::{
  DEFAULT_SNAPSHOT_COMPACT_THRESHOLD, OpenedSnapshot, SnapshotOpenError, Snapshotter,
};
