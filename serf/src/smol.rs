//! serf on the smol runtime (via the runtime-agnostic reactor driver).
//!
//! Re-exports the full [`serf_reactor`] surface with the runtime pinned to smol,
//! so callers never name `R`. Build a node with the inherent constructors on the
//! runtime-pinned `Serf` alias — `tcp`, `tls`, `quic`, and their `*_with_rng`
//! variants. The unpinned three-parameter handle stays available as
//! [`crate::reactor`].
pub use serf_reactor::*;

/// The runtime these handles bind.
pub type Runtime = agnostic::smol::SmolRuntime;

/// A smol-backed serf handle — [`serf_reactor::Serf`] with its runtime pinned to
/// smol, so callers never name `R`.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub type Serf<I, A> = serf_reactor::Serf<I, A, Runtime>;
