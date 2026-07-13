#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/serf/main/art/logo_72x72.png")]
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

extern crate alloc;

/// The runtime-agnostic protocol core: the serf state machine, the wire codec,
/// options, typed messages, events, and delegates.
pub use serf_proto as proto;

#[cfg(feature = "tokio")]
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
pub mod tokio;

#[cfg(feature = "smol")]
#[cfg_attr(docsrs, doc(cfg(feature = "smol")))]
pub mod smol;

#[cfg(feature = "reactor")]
#[cfg_attr(docsrs, doc(cfg(feature = "reactor")))]
pub mod reactor;

#[cfg(feature = "compio")]
#[cfg_attr(docsrs, doc(cfg(feature = "compio")))]
pub mod compio;

#[cfg(feature = "smoltcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "smoltcp")))]
pub mod smoltcp;

#[cfg(feature = "embassy")]
#[cfg_attr(docsrs, doc(cfg(feature = "embassy")))]
pub mod embassy;

#[cfg(feature = "embedded")]
#[cfg_attr(docsrs, doc(cfg(feature = "embedded")))]
pub mod embedded;
