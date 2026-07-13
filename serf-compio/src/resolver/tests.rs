//! Unit tests for the built-in resolvers: the advertise-candidate pickers (the
//! policy `Transport::new` applies when an unresolved advertise address resolves
//! to more than one candidate) and the address resolvers themselves.

use std::net::SocketAddr;

use super::{
  AdvertiseAddrResolver, AdvertiseResolutionError, FirstAddrResolver, Ipv4PreferringResolver,
  Ipv6PreferringResolver,
};

fn addr(s: &str) -> SocketAddr {
  s.parse().expect("socket addr")
}

/// The default picker takes the candidate set's FIRST address, whatever its
/// family — the order the resolver returned is the policy.
#[test]
fn first_addr_resolver_takes_the_head_of_the_candidate_set() {
  let picked = FirstAddrResolver
    .pick(vec![addr("[::1]:7946"), addr("127.0.0.1:7946")])
    .expect("a non-empty candidate set resolves");
  assert_eq!(picked, addr("[::1]:7946"), "the head candidate is picked");

  let picked = FirstAddrResolver
    .pick(vec![addr("10.0.0.1:1"), addr("10.0.0.2:2")])
    .expect("a non-empty candidate set resolves");
  assert_eq!(picked, addr("10.0.0.1:1"));
}

/// The IPv4-preferring picker skips past IPv6 candidates to the first IPv4 one,
/// and falls back to the head of the set when the resolution returned no IPv4
/// address at all (an IPv6-only host still gets a contact).
#[test]
fn ipv4_preferring_resolver_prefers_v4_then_falls_back() {
  let picked = Ipv4PreferringResolver
    .pick(vec![
      addr("[::1]:7946"),
      addr("[2001:db8::1]:7946"),
      addr("127.0.0.1:7946"),
      addr("10.0.0.1:7946"),
    ])
    .expect("a non-empty candidate set resolves");
  assert_eq!(
    picked,
    addr("127.0.0.1:7946"),
    "the FIRST IPv4 candidate wins over any IPv6 candidate ahead of it"
  );

  let picked = Ipv4PreferringResolver
    .pick(vec![addr("[::1]:7946"), addr("[2001:db8::1]:7946")])
    .expect("an IPv6-only candidate set still resolves");
  assert_eq!(
    picked,
    addr("[::1]:7946"),
    "with no IPv4 candidate the preference falls back to the head of the set"
  );
}

/// The IPv6-preferring picker is the mirror image: the first IPv6 candidate
/// wins, and an IPv4-only set falls back to the head.
#[test]
fn ipv6_preferring_resolver_prefers_v6_then_falls_back() {
  let picked = Ipv6PreferringResolver
    .pick(vec![
      addr("127.0.0.1:7946"),
      addr("10.0.0.1:7946"),
      addr("[2001:db8::1]:7946"),
      addr("[::1]:7946"),
    ])
    .expect("a non-empty candidate set resolves");
  assert_eq!(
    picked,
    addr("[2001:db8::1]:7946"),
    "the FIRST IPv6 candidate wins over any IPv4 candidate ahead of it"
  );

  let picked = Ipv6PreferringResolver
    .pick(vec![addr("127.0.0.1:7946"), addr("10.0.0.1:7946")])
    .expect("an IPv4-only candidate set still resolves");
  assert_eq!(
    picked,
    addr("127.0.0.1:7946"),
    "with no IPv6 candidate the preference falls back to the head of the set"
  );
}

/// An empty candidate set is a resolution FAILURE on every picker, never a
/// silent default: a node with no resolvable advertise address must not boot.
#[test]
fn every_picker_rejects_an_empty_candidate_set() {
  assert!(matches!(
    FirstAddrResolver.pick(Vec::new()),
    Err(AdvertiseResolutionError::Empty)
  ));
  assert!(matches!(
    Ipv4PreferringResolver.pick(Vec::new()),
    Err(AdvertiseResolutionError::Empty)
  ));
  assert!(matches!(
    Ipv6PreferringResolver.pick(Vec::new()),
    Err(AdvertiseResolutionError::Empty)
  ));
}

/// The identity resolver returns its already-concrete input verbatim as the sole
/// candidate — the pass-through the `SocketAddr`-addressed node type relies on.
#[compio::test]
async fn socket_addr_resolver_passes_its_input_through() {
  use super::{Resolver, SocketAddrResolver};

  let input = addr("192.0.2.7:7946");
  let out = SocketAddrResolver
    .resolve(&input)
    .await
    .expect("the identity resolver cannot fail");
  assert_eq!(
    out,
    vec![input],
    "the identity resolver yields exactly its input"
  );
}

/// The OS resolver resolves a literal-IP host without a DNS round-trip, keeps
/// the port, and reports a lookup failure for an unresolvable name rather than
/// yielding an empty candidate set.
#[compio::test]
async fn os_resolver_resolves_a_literal_ip_host() {
  use hostaddr::HostAddr;

  use super::{OsResolver, Resolver};

  let host: HostAddr<smol_str::SmolStr> = "127.0.0.1:7946".parse().expect("literal-IP host addr");
  let out = OsResolver
    .resolve(&host)
    .await
    .expect("a literal IP resolves without DNS");
  assert_eq!(
    out,
    vec![addr("127.0.0.1:7946")],
    "the literal IP and its port pass through"
  );

  let bad: HostAddr<smol_str::SmolStr> = "no-such-host.invalid:7946"
    .parse()
    .expect("domain host addr");
  assert!(
    OsResolver.resolve(&bad).await.is_err(),
    "an unresolvable name is a resolution error, never an empty candidate set"
  );
}
