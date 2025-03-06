#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/serf/main/art/logo_72x72.png")]
#![forbid(unsafe_code)]
// #![deny(warnings, missing_docs)]
#![allow(clippy::type_complexity)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

pub use serf_core::*;

pub use memberlist::{agnostic, transport};

#[cfg(feature = "net")]
pub use memberlist::net;

#[cfg(feature = "quic")]
pub use memberlist::quic;

/// [`Serf`] for `tokio` runtime.
#[cfg(feature = "tokio")]
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
pub mod tokio;

/// [`Serf`] for `async-std` runtime.
#[cfg(feature = "async-std")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-std")))]
pub mod async_std;

/// [`Serf`] for `smol` runtime.
#[cfg(feature = "smol")]
#[cfg_attr(docsrs, doc(cfg(feature = "smol")))]
pub mod smol;

/// A simple agent of [`Serf`].
#[cfg(feature = "agent")]
#[cfg_attr(docsrs, doc(cfg(feature = "agent")))]
pub mod agent;

/// Bultin command line tool for `Serf`.
#[cfg(all(
  feature = "cli",
  any(feature = "tokio", feature = "async-std", feature = "smol")
))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(
    feature = "cli",
    any(feature = "tokio", feature = "async-std", feature = "smol")
  )))
)]
pub mod cli;
