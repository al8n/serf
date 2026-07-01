//! The driver layer: the generic-free tuning knobs and the per-backend pumps.
//!
//! [`options`] holds the generic-free driver tuning knobs shared by every
//! transport backend. [`shared`] holds the driver-side observation helpers. The
//! per-backend driver loops (behind the transport features) each own a serf
//! `StreamEndpoint` / `QuicEndpoint` and pump it as a quinn-style `Future::poll`.

pub(crate) mod options;

#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) mod shared;

#[cfg(feature = "tcp")]
pub(crate) mod stream;
