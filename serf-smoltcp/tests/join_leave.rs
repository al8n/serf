//! Reliable-plane end-to-end: a serf node JOINS a seed via a real push/pull state
//! exchange over TCP, both converge on a 2-member view, then the joiner LEAVES
//! gracefully and observes the corresponding lifecycle event.
//!
//! This exercises the full reliable plane over the smoltcp stack: dial
//! (`StreamAction::Connect` → `tcp::Socket::connect`), the TCP three-way handshake
//! over the paired device, the listener accept, the bidirectional push/pull byte
//! pump, and graceful teardown — the proof that the `Serf` handle's dial / accept /
//! pump wiring carries a serf join to completion.

mod harness;

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use serf_smoltcp::{
  EndpointOptions, Event, MaybeResolved, Options, Serf, SerfOptions, SocketAddrResolver,
  TransformOptions,
};
use smol_str::SmolStr;

fn addr(ip: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// A real TCP push/pull serf join, a synced 2-member view, then a clean leave.
///
/// # Phase 1 — join
///
/// B dials A as a seed. Both nodes poll each tick so the SYN/SYN-ACK/ACK and the
/// push/pull frames flow through the paired device FIFOs. Convergence is
/// `num_members() == 2` on BOTH nodes — A learns B from the inbound push/pull, B
/// learns A from the reply. The 20 s virtual budget is generous headroom over the
/// zero-latency link, not an expected duration.
///
/// # Phase 2 — leave
///
/// B calls `leave`, which gossips the departure; B must observe `Event::LeftCluster`.
/// Both nodes keep polling so the leave broadcast and any reliable teardown frames
/// cross the link.
#[test]
fn join_from_seed_then_clean_leave() {
  const BUDGET: u32 = 2000;

  let (mut da, mut db) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

  // Node A is the seed; node B is the joiner.
  let mut a: Serf<SmolStr, SocketAddr, _> = Serf::new(
    Options::new(),
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("a"), addr(1, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut da,
    now,
  );
  let mut b: Serf<SmolStr, SocketAddr, _> = Serf::new(
    Options::new(),
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("b"), addr(2, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut db,
    now,
  );

  a.start(now);
  b.start(now);

  // B joins via A as a seed: a REAL push/pull over TCP.
  let jid = b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, 7946))],
      false,
      now,
    )
    .expect("join from a running node");

  // Drive both until both see 2 members (join push/pull synced state), bounded.
  let mut joined = false;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    // Drain the await-result join so its waiter does not linger once resolved.
    let _ = b.poll_join(jid);
    if a.num_members() == 2 && b.num_members() == 2 {
      joined = true;
      break;
    }
    clk.advance_ms(10);
  }
  assert!(
    joined,
    "join push/pull did not converge: a={} b={}",
    a.num_members(),
    b.num_members()
  );

  // B leaves; B must observe LeftCluster.
  b.leave(clk.now()).expect("leave");
  let mut b_left = false;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    while let Some(ev) = b.poll_event() {
      if matches!(ev, Event::LeftCluster) {
        b_left = true;
      }
    }
    // Drain A's events so its buffer does not shed under the leave gossip.
    while a.poll_event().is_some() {}
    if b_left {
      break;
    }
    clk.advance_ms(10);
  }
  assert!(b_left, "b did not complete leave (LeftCluster)");
}
