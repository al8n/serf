//! OS-based resolver — delegates to the runtime's `getaddrinfo` equivalent.

use core::{future::Future, marker::PhantomData};
use std::{io, net::SocketAddr};

use agnostic::net::ToSocketAddrs;
use agnostic_lite::RuntimeLite;
use hostaddr::Host;
use smol_str::SmolStr;

use crate::resolver::Resolver;

/// OS-based resolver. Runs `getaddrinfo` on the runtime's blocking pool via the
/// agnostic [`ToSocketAddrs`] abstraction, so it stays runtime-agnostic yet never
/// blocks the async worker.
///
/// Generic over the runtime `R` (the same `agnostic::Runtime` the node's driver
/// is spawned on). Suitable for typical hostname lookups; large DNS records that
/// may be truncated over UDP are better served by the `dns`-feature resolver.
pub struct OsResolver<R>(PhantomData<fn() -> R>);

impl<R> OsResolver<R> {
  /// Construct an OS resolver for the runtime `R`.
  #[inline]
  pub const fn new() -> Self {
    Self(PhantomData)
  }
}

impl<R> Default for OsResolver<R> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl<R> Resolver for OsResolver<R>
where
  R: RuntimeLite,
{
  type Address = hostaddr::HostAddr<SmolStr>;
  type Error = io::Error;

  fn resolve(
    &self,
    addr: &Self::Address,
  ) -> impl Future<Output = Result<Vec<SocketAddr>, Self::Error>> + Send + '_ {
    let port = addr.port().unwrap_or(0);
    let host = match addr.host() {
      Host::Ip(ip) => ip.to_string(),
      Host::Domain(name) => name.to_string(),
    };
    async move {
      let iter = ToSocketAddrs::<R>::to_socket_addrs(&(host, port)).await?;
      Ok(iter.collect())
    }
  }
}
