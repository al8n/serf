//! Address resolution for serf-compio.
//!
//! [`Resolver`] is the generic address-to-[`SocketAddr`] conversion trait.
//! [`AdvertiseAddrResolver`] picks the single advertise address from a
//! multi-candidate resolution result.
//!
//! Built-in resolvers:
//! - [`OsResolver`]: `getaddrinfo`-backed (compio `ToSocketAddrsAsync`).
//! - [`SocketAddrResolver`]: identity pass-through for already-resolved addrs.
//!
//! Optional resolvers (feature-gated):
//! - `dns` — [`DnsResolver`]: TCP-first DNS via `hickory-proto`.
//! - `getifs` — [`LocalAddrResolver`]: auto-detect the advertise address from
//!   the host's own interfaces.

mod advertise;
mod os;
mod socket_addr;

#[cfg(feature = "dns")]
mod dns;

#[cfg(feature = "getifs")]
mod getifs;

pub use advertise::{
  AdvertiseAddrResolver, AdvertiseResolutionError, FirstAddrResolver, Ipv4PreferringResolver,
  Ipv6PreferringResolver,
};
pub use os::OsResolver;
pub use socket_addr::SocketAddrResolver;

#[cfg(feature = "dns")]
#[cfg_attr(docsrs, doc(cfg(feature = "dns")))]
pub use dns::{DEFAULT_DNS_TIMEOUT, DnsError, DnsResolver};

#[cfg(feature = "getifs")]
#[cfg_attr(docsrs, doc(cfg(feature = "getifs")))]
pub use getifs::{LocalAddrResolver, LocalAddrScope, local_advertise};

use std::net::SocketAddr;

/// Resolve a user-facing address into one or more concrete [`SocketAddr`]s.
///
/// The input address type is the implementor's choice: [`SocketAddrResolver`]
/// takes [`SocketAddr`] (identity pass-through); [`OsResolver`] takes
/// [`hostaddr::HostAddr<SmolStr>`](hostaddr::HostAddr). Custom resolvers may
/// take any type (e.g. a service-discovery handle).
///
/// AFIT (no `async-trait`) — compio is `!Send`-first so the trait has no
/// `Send`/`Sync` bound. Pass an instance per call; the caller owns its
/// lifetime and can reuse it across multiple joins.
#[allow(async_fn_in_trait)]
pub trait Resolver: 'static {
  /// The user-facing address type this resolver consumes.
  type Address;

  /// The error type returned on resolution failure.
  type Error: core::error::Error + 'static;

  /// Resolve `addr` to its concrete socket addresses.
  async fn resolve(&self, addr: &Self::Address) -> Result<Vec<SocketAddr>, Self::Error>;
}

#[cfg(test)]
mod tests;
