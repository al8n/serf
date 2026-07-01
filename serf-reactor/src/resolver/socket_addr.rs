//! Identity pass-through resolver — input is already a [`SocketAddr`].

use crate::resolver::Resolver;
use core::future::Future;
use std::{io, net::SocketAddr};

/// Identity pass-through resolver. Declares
/// [`Resolver::Address`](crate::resolver::Resolver::Address)`= SocketAddr` and
/// returns the input verbatim.
///
/// Use when seed addresses are already concrete socket addresses — no DNS lookup
/// or hostname parsing is required.
#[derive(Debug, Default, Clone, Copy)]
pub struct SocketAddrResolver;

impl Resolver for SocketAddrResolver {
  type Address = SocketAddr;
  type Error = io::Error;

  fn resolve(
    &self,
    addr: &SocketAddr,
  ) -> impl Future<Output = Result<Vec<SocketAddr>, Self::Error>> + Send + '_ {
    let addr = *addr;
    async move { Ok(vec![addr]) }
  }
}
