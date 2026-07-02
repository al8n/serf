//! Runtime-agnostic glue shared by the serf async driver crates.
//!
//! A driver binds `serf-proto`'s Sans-I/O endpoint to a real async runtime. This crate holds
//! the runtime-independent pieces both the compio and reactor drivers need — the observable
//! [`SerfSnapshot`] (when a transport feature is enabled), the common driver error payloads,
//! and small pure helpers — so they live in one place instead of being copied per runtime.
//! The run loops, the channel and cell substrate, and the delegate dispatch stay in each
//! runtime crate.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

pub mod error;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod observation;
#[cfg(any(feature = "tcp", feature = "quic"))]
mod snapshot;

#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use observation::observation_payload_bytes;
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub use snapshot::SerfSnapshot;
