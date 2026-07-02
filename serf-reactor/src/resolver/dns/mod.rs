//! TCP-first DNS resolver — hickory-proto codec over the agnostic runtime's
//! `R`-generic TCP stream. Mirrors Go memberlist's `tcpLookupIP` algorithm
//! (`hashicorp/memberlist/memberlist.go:308-417`).
//!
//! Why TCP-first: UDP DNS responses are capped at 512 bytes (without EDNS), which
//! can truncate the answer list for cluster-discovery hostnames resolving to many
//! A/AAAA records. TCP-DNS has no such cap, so it gives the largest possible join
//! set on a single query.
//!
//! This is the `Send`/`agnostic` sibling of serf-compio's `!Send`, compio-bound
//! `DnsResolver`: the transport is `<R::Net as Net>::TcpStream` and the per-query
//! deadline is armed with `R::sleep`, so resolution runs on any `agnostic::Runtime`.

#![cfg(feature = "dns")]

use core::future::Future;
use std::{
  io::{self, Read},
  net::{IpAddr, SocketAddr},
  path::Path,
  time::Duration,
};

use agnostic::{
  Runtime,
  net::{Net, TcpStream},
};
use futures_util::{AsyncReadExt, AsyncWriteExt, FutureExt, pin_mut, select_biased};
use hickory_proto::{
  ProtoError,
  op::{Message, Query},
  rr::{Name, RData, RecordType},
  serialize::binary::{BinEncodable, BinEncoder, DecodeError},
};
use hostaddr::{Host, HostAddr};
use smol_str::SmolStr;

use crate::resolver::{OsResolver, Resolver};

/// Default wall-clock upper bound on a single TCP-DNS query (connect + write +
/// read length-prefix + read response). Matches the default DNS query timeout used
/// by most stub resolvers (Go's `net.Resolver` uses 5s, glibc's resolver uses 5s
/// per attempt). Configured per-resolver via [`DnsResolver::with_timeout`]; without
/// a bound the query inherits the kernel's TCP timeouts (~3 minutes connect,
/// infinite read), which would let a slow or hostile nameserver hang the caller's
/// `join` future indefinitely.
pub const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP-first DNS resolver — queries configured nameservers over TCP and falls back
/// to the OS resolver (which is UDP-first with TCP retry on truncation) if TCP
/// returns nothing.
///
/// Constructed from a resolv.conf-format file. For hostnames that lack a `.` (short
/// names, likely resolved via the host's search-domain list) the TCP path is
/// skipped entirely and the OS resolver is used directly, matching the upstream
/// behavior.
///
/// Generic over the runtime `R` (the same `agnostic::Runtime` the node's driver is
/// spawned on): the TCP transport and the per-query timer are both `R`'s.
pub struct DnsResolver<R> {
  servers: Vec<SocketAddr>,
  fallback: OsResolver<R>,
  timeout: Duration,
}

/// Errors returned by [`DnsResolver`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DnsError {
  /// I/O error from the TCP transport or the OS-resolver fallback.
  #[error("I/O error: {0}")]
  Io(#[from] io::Error),

  /// hickory-proto encoding error (malformed query construction).
  #[error("DNS encode error: {0}")]
  Encode(#[from] ProtoError),

  /// hickory-proto decoding error (malformed response from the server).
  #[error("DNS decode error: {0}")]
  Decode(#[from] DecodeError),

  /// The hostname could not be parsed into a wire-format DNS name.
  /// Carries the hickory `ProtoError` via `#[source]` rather than `#[from]`, since
  /// [`Self::Encode`] already owns the `From<ProtoError>` conversion.
  #[error("hostname parse error: {0}")]
  Hostname(#[source] ProtoError),
}

impl From<DnsError> for io::Error {
  fn from(e: DnsError) -> Self {
    Self::other(e)
  }
}

impl<R> DnsResolver<R> {
  /// Construct from a resolv.conf-format file path. Reads the file, parses the
  /// nameserver list (each pinned to port 53), and stores the OS resolver as the
  /// fallback.
  pub fn from_resolv_conf(path: impl AsRef<Path>) -> Result<Self, io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let cfg = resolv_conf::Config::parse(&buf)
      .map_err(|e| io::Error::other(format!("resolv.conf parse: {e}")))?;

    let servers: Vec<SocketAddr> = cfg
      .nameservers
      .iter()
      .map(|ns| SocketAddr::new(IpAddr::from(ns), 53))
      .collect();

    Ok(Self {
      servers,
      fallback: OsResolver::new(),
      timeout: DEFAULT_DNS_TIMEOUT,
    })
  }

  /// Construct from an explicit nameserver list. The OS resolver is used as the
  /// fallback path.
  pub fn from_servers(servers: Vec<SocketAddr>) -> Self {
    Self {
      servers,
      fallback: OsResolver::new(),
      timeout: DEFAULT_DNS_TIMEOUT,
    }
  }

  /// Builder: override the per-query timeout. Defaults to [`DEFAULT_DNS_TIMEOUT`].
  #[must_use]
  #[inline]
  pub const fn with_timeout(mut self, d: Duration) -> Self {
    self.timeout = d;
    self
  }

  /// The configured per-query timeout.
  #[inline]
  pub const fn timeout(&self) -> Duration {
    self.timeout
  }
}

impl<R> DnsResolver<R>
where
  R: Runtime,
{
  /// Send a single TCP-DNS query for TYPE ANY against the first configured
  /// nameserver and return the collected A + AAAA records. Returns an empty vec
  /// when no servers are configured.
  ///
  /// Bounded by `self.timeout` (default [`DEFAULT_DNS_TIMEOUT`]): a slow or hostile
  /// nameserver cannot hang the caller's `join` future beyond this wall-clock
  /// budget. On timeout returns [`DnsError::Io`]`(io::ErrorKind::TimedOut)`, which
  /// [`Resolver::resolve`] surfaces WITHOUT the OS fallback — that fallback runs
  /// outside this deadline, so escalating a timeout into it would defeat the bound.
  /// A genuine unavailability (connect refused, unreachable nameserver, malformed
  /// response) or an empty answer does fall through to the OS resolver.
  async fn tcp_query(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, DnsError> {
    let query = self.tcp_query_inner(host, port).fuse();
    let timeout = R::sleep(self.timeout).fuse();
    pin_mut!(query, timeout);
    select_biased! {
      res = query => res,
      _ = timeout => Err(DnsError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        "TCP-DNS query exceeded the configured timeout",
      ))),
    }
  }

  /// Inner unbounded TCP-DNS query — invoked by [`Self::tcp_query`] inside the
  /// deadline select. Kept separate so the deadline wrapper owns the timer arm
  /// without complicating the protocol logic.
  async fn tcp_query_inner(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, DnsError> {
    let Some(&server) = self.servers.first() else {
      return Ok(Vec::new());
    };

    // Build a TYPE ANY query message. `Message::query()` initializes a fresh ID
    // with the standard query flags; we add the question.
    let name = Name::from_ascii(host).map_err(DnsError::Hostname)?;
    let mut msg = Message::query();
    msg.add_query(Query::query(name, RecordType::ANY));

    // Encode to bytes via BinEncoder over an owned Vec.
    let mut payload: Vec<u8> = Vec::with_capacity(512);
    {
      let mut encoder = BinEncoder::new(&mut payload);
      msg.emit(&mut encoder)?;
    }

    // TCP-DNS (RFC 1035 §4.2.2) prepends a 2-byte big-endian length.
    let payload_len = u16::try_from(payload.len())
      .map_err(|_| DnsError::Io(io::Error::other("DNS query exceeds 65535 bytes")))?;
    let mut framed = Vec::with_capacity(2 + payload.len());
    framed.extend_from_slice(&payload_len.to_be_bytes());
    framed.extend_from_slice(&payload);

    // Connect and send the framed query. The agnostic stream reads/writes into
    // borrowed buffers via the futures `AsyncRead`/`AsyncWrite` ext traits.
    let mut stream = <R::Net as Net>::TcpStream::connect(server).await?;
    stream.write_all(&framed).await?;

    // Read the 2-byte length prefix, then the body of exactly that length.
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let response_len = u16::from_be_bytes(len_buf) as usize;

    let mut resp_buf = vec![0u8; response_len];
    stream.read_exact(&mut resp_buf).await?;

    // Decode and collect A + AAAA answers. CNAME and other RR types are ignored to
    // match the upstream behavior (see Go reference above).
    let response = Message::from_vec(&resp_buf)?;
    let mut addrs = Vec::new();
    for record in &response.answers {
      // `Record` exposes the rdata as a public field `data`; the same-named
      // accessor is shadowed when the field is the same name.
      match &record.data {
        RData::A(ipv4) => addrs.push(SocketAddr::new(IpAddr::V4(ipv4.0), port)),
        RData::AAAA(ipv6) => addrs.push(SocketAddr::new(IpAddr::V6(ipv6.0), port)),
        _ => {}
      }
    }
    Ok(addrs)
  }
}

impl<R> Resolver for DnsResolver<R>
where
  R: Runtime,
{
  type Address = HostAddr<SmolStr>;
  type Error = DnsError;

  fn resolve(
    &self,
    addr: &Self::Address,
  ) -> impl Future<Output = Result<Vec<SocketAddr>, Self::Error>> + Send + '_ {
    // Clone the input up front so the returned future borrows only `self`, not the
    // shorter-lived `addr` param (the `Resolver` contract binds the future to
    // `&self`'s lifetime; the fallback resolves the owned clone, which lives inside
    // the future).
    let addr = addr.clone();
    async move {
      let port = addr.port().unwrap_or(0);

      // IP literal: short-circuit, no DNS at all.
      if let Host::Ip(ip) = addr.host() {
        return Ok(vec![SocketAddr::new(*ip, port)]);
      }

      let host_str: &str = match addr.host() {
        Host::Domain(d) => d.as_ref(),
        Host::Ip(_) => unreachable!("handled above"),
      };

      // TCP-first only for names that look fully qualified (contain a `.`) and only
      // when we have at least one nameserver configured. Short names will be
      // resolved through the OS resolver's search-domain list.
      if host_str.contains('.') && !self.servers.is_empty() {
        // TCP-first is best-effort per the upstream spec ("If this fails it's not
        // fatal since this isn't a standard way to query DNS, and we have a fallback
        // below.", memberlist.go:404), so a genuine unavailability (connect refused,
        // unreachable nameserver, malformed response) or an empty answer falls
        // through to the OS resolver below.
        match self.tcp_query(host_str, port).await {
          // A productive TCP answer wins outright; no fallback needed.
          Ok(addrs) if !addrs.is_empty() => return Ok(addrs),
          // A configured-resolver TIMEOUT must NOT escalate into the OS resolver: the
          // OS path runs OUTSIDE `self.timeout` (its DNS is unbounded /
          // runtime-dependent), so falling through would let a slow or hostile
          // nameserver burn the TCP deadline and THEN hang bootstrap in unbounded OS
          // DNS — contradicting the timeout contract. Surface the timeout so the
          // configured budget bounds the whole resolution.
          Err(DnsError::Io(err)) if err.kind() == io::ErrorKind::TimedOut => {
            return Err(DnsError::Io(err));
          }
          // Empty answer or a genuine unavailability: fall through to the OS resolver.
          Ok(_) | Err(_) => {}
        }
      }

      self.fallback.resolve(&addr).await.map_err(DnsError::Io)
    }
  }
}

#[cfg(all(test, feature = "tokio"))]
mod tests;
