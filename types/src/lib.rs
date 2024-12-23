//! Types used by the [`serf`](https://crates.io/crates/serf) crate.
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/memberlist/main/art/logo_72x72.png")]
#![forbid(unsafe_code)]
#![deny(warnings, missing_docs)]
#![allow(clippy::type_complexity, unexpected_cfgs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

pub use memberlist_types::{
  DelegateVersion as MemberlistDelegateVersion, Node, NodeAddress, NodeAddressError, NodeId,
  NodeIdTransformError, NodeTransformError, ProtocolVersion as MemberlistProtocolVersion,
  UnknownDelegateVersion as UnknownMemberlistDelegateVersion,
  UnknownProtocolVersion as UnknownMemberlistProtocolVersion,
};
pub use transformable::{Encodable, Transformable};

mod clock;
pub use clock::*;

mod filter;
pub use filter::*;

mod leave;
pub use leave::*;

mod member;
pub use member::*;

mod message;
pub use message::*;

mod join;
pub use join::*;

mod tags;
pub use tags::*;

mod push_pull;
pub use push_pull::*;

mod user_event;
pub use user_event::*;

mod query;
pub use query::*;

mod version;
pub use version::*;

#[cfg(feature = "encryption")]
mod key;
#[cfg(feature = "encryption")]
#[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
pub use key::*;
