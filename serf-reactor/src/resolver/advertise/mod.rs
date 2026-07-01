//! [`AdvertiseAddrResolver`] — picks one [`SocketAddr`] from a candidate set
//! during local-node advertise resolution. Called once at `Transport::new` when
//! the configured advertise address is `MaybeResolved::Unresolved(addr)` and
//! [`Resolver::resolve`](crate::resolver::Resolver::resolve) returns multiple
//! candidates.

use std::net::SocketAddr;

/// Picks one [`SocketAddr`] from a candidate set.
///
/// The driver calls this immediately after
/// [`Resolver::resolve`](crate::resolver::Resolver::resolve) returns more than
/// one result for the configured advertise address. An implementor can express
/// simple policies (prefer IPv4, prefer IPv6, take the first) or consult external
/// state. `Send + Sync + 'static` so it can be held across the multi-threaded
/// construction path.
pub trait AdvertiseAddrResolver: Send + Sync + 'static {
  /// Error type returned by [`Self::pick`].
  type Error: core::error::Error + Send + Sync + 'static;

  /// Pick one candidate. Returns an error when `candidates` is empty.
  fn pick(&self, candidates: Vec<SocketAddr>) -> Result<SocketAddr, Self::Error>;
}

/// Error variants returned by the built-in [`AdvertiseAddrResolver`] impls.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdvertiseResolutionError {
  /// The candidate set was empty — resolution returned no addresses.
  #[error("advertise resolution: no candidate addresses returned")]
  Empty,
}

/// Default — returns the first candidate address in the set.
#[derive(Debug, Default, Clone, Copy)]
pub struct FirstAddrResolver;

impl AdvertiseAddrResolver for FirstAddrResolver {
  type Error = AdvertiseResolutionError;

  fn pick(&self, candidates: Vec<SocketAddr>) -> Result<SocketAddr, Self::Error> {
    candidates
      .into_iter()
      .next()
      .ok_or(AdvertiseResolutionError::Empty)
  }
}

/// Prefers the first IPv4 candidate; falls through to the first address of any
/// family if no IPv4 candidates are present.
#[derive(Debug, Default, Clone, Copy)]
pub struct Ipv4PreferringResolver;

impl AdvertiseAddrResolver for Ipv4PreferringResolver {
  type Error = AdvertiseResolutionError;

  fn pick(&self, candidates: Vec<SocketAddr>) -> Result<SocketAddr, Self::Error> {
    let v4 = candidates.iter().find(|s| s.is_ipv4()).copied();
    if let Some(s) = v4 {
      return Ok(s);
    }
    candidates
      .into_iter()
      .next()
      .ok_or(AdvertiseResolutionError::Empty)
  }
}

/// Prefers the first IPv6 candidate; falls through to the first address of any
/// family if no IPv6 candidates are present.
#[derive(Debug, Default, Clone, Copy)]
pub struct Ipv6PreferringResolver;

impl AdvertiseAddrResolver for Ipv6PreferringResolver {
  type Error = AdvertiseResolutionError;

  fn pick(&self, candidates: Vec<SocketAddr>) -> Result<SocketAddr, Self::Error> {
    let v6 = candidates.iter().find(|s| s.is_ipv6()).copied();
    if let Some(s) = v6 {
      return Ok(s);
    }
    candidates
      .into_iter()
      .next()
      .ok_or(AdvertiseResolutionError::Empty)
  }
}
