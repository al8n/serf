//! Inbound reliable-plane ADMISSION end-to-end: which TCP connections a node lets
//! into its serf machine, and what happens to the ones it refuses.
//!
//! A node's listener accepts a socket long before serf sees it, so refusal has to
//! happen at the driver's accept gate. Two refusals are pinned here over a real TCP
//! handshake across the paired device:
//!
//! - a peer the node's CIDR policy excludes is aborted at the transport boundary and
//!   never registered as an exchange, and
//! - a peer that dials a node which has already LEFT is refused by the machine.
//!
//! In both cases the refused socket must be returned to the pool and a fresh listener
//! re-armed — otherwise every refusal would shrink the finite socket pool by one and
//! a node under a hostile or stale peer would eventually have no reliable plane at
//! all. The joiner, for its part, must see a FAILED join rather than hang.

mod harness;

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use serf_smoltcp::{
  EndpointOptions, MaybeResolved, Options, Serf, SerfOptions, SocketAddrResolver, TransformOptions,
};
use smol_str::SmolStr;

fn addr(ip: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// Build a node at `10.0.0.{ip}` over `device`, with an optional CIDR policy.
fn node(
  ip: u8,
  cfg: Options,
  device: &mut harness::PairedDevice,
  now: serf_smoltcp::Instant,
) -> Serf<SmolStr, SocketAddr, harness::PairedDevice> {
  let id = if ip == 1 { "a" } else { "b" };
  Serf::new(
    cfg,
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip))),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new(id), addr(ip, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    device,
    now,
  )
}

/// A policy admitting only `allow`, blocking every other address.
#[cfg(feature = "cidr")]
fn only(allow: &str) -> serf_smoltcp::CidrPolicy {
  let mut policy = serf_smoltcp::CidrPolicy::block_all();
  policy.add(allow.parse().expect("a well-formed CIDR parses"));
  policy
}

/// A's CIDR policy excludes B. B dials A over real TCP; A's accept gate aborts the
/// connected socket WITHOUT registering an exchange, returns it to the pool, and
/// re-arms its listener — so the refusal costs A nothing and B's join fails rather
/// than hanging, and A never admits B as a member.
#[cfg(feature = "cidr")]
#[test]
fn a_cidr_blocked_peer_is_refused_at_the_accept_gate() {
  const BUDGET: u32 = 2000;

  let (mut da, mut db) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

  // A admits only itself; B (10.0.0.2) is outside the policy.
  let mut a = node(
    1,
    Options::new().with_cidr_policy(only("10.0.0.1/32")),
    &mut da,
    now,
  );
  let mut b = node(2, Options::new(), &mut db, now);
  a.start(now);
  b.start(now);

  let free_before = a.pool_free_count();
  assert!(a.listener_present(), "A starts with an armed listener");

  let jid = b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, 7946))],
      false,
      now,
    )
    .expect("join from a running node");

  // Drive both until B's join resolves. `poll_join` delivers exactly once, so the
  // outcome is captured as it lands.
  let mut outcome = None;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    while a.poll_event().is_some() {}
    while b.poll_event().is_some() {}
    if let Some(o) = b.poll_join(jid) {
      outcome = Some(o);
      break;
    }
    clk.advance_ms(10);
  }

  let Some(Err(failed)) = outcome else {
    panic!("a join into a policy that excludes the joiner must FAIL, got {outcome:?}");
  };
  assert_eq!(failed.contacted(), 0, "the blocked join reached no seed");

  assert_eq!(
    a.num_members(),
    1,
    "A must never admit the excluded peer as a member"
  );
  assert!(
    a.listener_present(),
    "A must re-arm its listener after refusing the connection"
  );
  assert_eq!(
    a.pool_free_count(),
    free_before,
    "a refused connection must return its socket to the pool, not shrink it"
  );
  assert_eq!(
    a.accepted_inbound_count(),
    0,
    "a refused connection is never counted as an accepted exchange"
  );
}

/// A has LEFT the cluster when B dials it. The machine refuses the inbound exchange,
/// and the driver must still return the connected socket to the pool and re-arm the
/// listener — a rejection must not shrink the finite pool one slot at a time.
#[test]
fn a_peer_dialing_a_departed_node_is_refused_without_leaking_its_socket() {
  const BUDGET: u32 = 2000;

  let (mut da, mut db) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

  let mut a = node(1, Options::new(), &mut da, now);
  let mut b = node(2, Options::new(), &mut db, now);
  a.start(now);
  b.start(now);

  let free_before = a.pool_free_count();

  // A leaves before B ever contacts it, so the inbound exchange arrives at a node
  // that is no longer an admissible member of the cluster.
  a.leave(clk.now()).expect("leave from a running node");
  for _ in 0..50 {
    let _ = a.poll(clk.now(), &mut da);
    while a.poll_event().is_some() {}
    clk.advance_ms(10);
  }

  let jid = b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, 7946))],
      false,
      now,
    )
    .expect("join from a running node");

  let mut outcome = None;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    while a.poll_event().is_some() {}
    while b.poll_event().is_some() {}
    if let Some(o) = b.poll_join(jid) {
      outcome = Some(o);
      break;
    }
    clk.advance_ms(10);
  }

  assert!(
    outcome.is_some(),
    "a join into a departed node must resolve, never hang"
  );
  assert!(
    a.listener_present(),
    "A must re-arm its listener after refusing the exchange"
  );
  assert_eq!(
    a.pool_free_count(),
    free_before,
    "a refused exchange must return its socket to the pool, not shrink it"
  );
}
