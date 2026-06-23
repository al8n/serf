//! The serf wire codec — pure, no-I/O message types shared by the serf driver crates.
//!
//! Depends on `memberlist-proto` for the `Data`/`DataRef` codec primitives; defines serf's
//! own message set and framing on top of them.
#![deny(missing_docs)]
