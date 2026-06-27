//! The driver layer: shared options and observation helpers.
//!
//! [`shared`] holds the observation / event hand-off helpers used by every
//! transport backend; [`options`] holds the generic-free driver tuning knobs.
//! Per-backend driver loops live in the transport module (e.g. `tcp::run`).

pub(crate) mod options;
#[cfg(feature = "quic")]
pub(crate) mod quic;
pub(crate) mod shared;
#[cfg(feature = "tcp")]
pub(crate) mod stream;
