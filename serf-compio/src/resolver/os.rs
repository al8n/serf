//! OS-based resolver — delegates to compio's `getaddrinfo` equivalent.

use crate::resolver::Resolver;
use compio::net::ToSocketAddrsAsync;
use hostaddr::Host;
use smol_str::SmolStr;
use std::{io, net::SocketAddr};

/// OS-based resolver. Uses compio's `ToSocketAddrsAsync` (`getaddrinfo`).
///
/// Suitable for typical hostname lookups. For large DNS records that may be
/// truncated over UDP, use the `DnsResolver` (feature `dns`), which performs
/// TCP-first queries.
pub struct OsResolver;

impl Resolver for OsResolver {
  type Address = hostaddr::HostAddr<SmolStr>;
  type Error = io::Error;

  async fn resolve(&self, addr: &Self::Address) -> Result<Vec<SocketAddr>, Self::Error> {
    let port = addr.port().unwrap_or(0);
    let host_str = match addr.host() {
      Host::Ip(ip) => ip.to_string(),
      Host::Domain(name) => name.to_string(),
    };
    let resolved: Vec<SocketAddr> = (host_str.as_str(), port)
      .to_socket_addrs_async()
      .await?
      .collect();
    Ok(resolved)
  }
}
