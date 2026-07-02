//! Address resolution for serf-reactor.
//!
//! [`Resolver`] is the generic address-to-[`SocketAddr`] conversion trait;
//! [`AdvertiseAddrResolver`] picks the single advertise address from a
//! multi-candidate resolution result. Both are the `Send`/`Sync` siblings of
//! serf-compio's `!Send` resolvers — mirroring memberlist-reactor's
//! `AddressResolver`, `resolve` returns `-> impl Future + Send` (not `async fn`)
//! so resolution can run on a multi-threaded agnostic runtime.
//!
//! Built-in resolvers:
//! - [`OsResolver`]: `getaddrinfo`-backed via the runtime's blocking pool.
//! - [`SocketAddrResolver`]: identity pass-through for already-resolved addrs.
//! - [`DnsResolver`] (`dns` feature): TCP-first `hickory`-backed DNS over the
//!   agnostic runtime's TCP stream.
//! - [`LocalAddrResolver`] (`getifs` feature): auto-detect the advertise address
//!   from the host's own network interfaces.

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

use core::future::Future;
use std::net::SocketAddr;

/// Resolve a user-facing address into one or more concrete [`SocketAddr`]s.
///
/// The input address type is the implementor's choice: [`SocketAddrResolver`]
/// takes [`SocketAddr`] (identity pass-through); [`OsResolver`] takes
/// [`hostaddr::HostAddr<SmolStr>`](hostaddr::HostAddr). Custom resolvers may take
/// any type (e.g. a service-discovery handle).
///
/// Invoked only at the boundary (bootstrap + `join`). The returned future is
/// `Send` so resolution can run on a multi-threaded runtime; it is written
/// `-> impl Future + Send` rather than `async fn` so the `Send` bound is part of
/// the trait contract.
pub trait Resolver: Send + Sync + 'static {
  /// The user-facing address type this resolver consumes.
  type Address: Send + Sync + 'static;

  /// The error type returned on resolution failure.
  type Error: core::error::Error + Send + Sync + 'static;

  /// Resolve `addr` to its concrete socket addresses.
  fn resolve(
    &self,
    addr: &Self::Address,
  ) -> impl Future<Output = Result<Vec<SocketAddr>, Self::Error>> + Send + '_;
}
