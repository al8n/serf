//! The driver layer: the generic-free tuning knobs.
//!
//! [`options`] holds the generic-free driver tuning knobs shared by every
//! transport backend. The reactor substrate (the shared `Arc<Shared>` state) and
//! the per-backend driver loops live behind the transport features.

pub(crate) mod options;
