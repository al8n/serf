//! Gossip-plane end-to-end: two serf nodes converge over a real TCP push/pull
//! join, then one broadcasts a user event that the other observes through its
//! driver-buffered `poll_event`. Exercises the `Serf` handle's outbound gossip
//! path and the mandatory-event drain's app-event buffering.

mod harness;

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use serf_smoltcp::{
  Bytes, EndpointOptions, Event, MaybeResolved, Options, Serf, SerfOptions, SocketAddrResolver,
  TransformOptions,
};
use smol_str::SmolStr;

fn addr(ip: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// After a converged join, A broadcasts a user event; B observes `Event::User`.
#[test]
fn user_event_propagates_across_the_gossip_plane() {
  const BUDGET: u32 = 4000;

  let (mut da, mut db) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

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

  let jid = b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, 7946))],
      false,
      now,
    )
    .expect("join from a running node");

  // Converge on a 2-member view first.
  let mut joined = false;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    let _ = b.poll_join(jid);
    // Keep the event backlogs drained so neither sheds while converging.
    while a.poll_event().is_some() {}
    while b.poll_event().is_some() {}
    if a.num_members() == 2 && b.num_members() == 2 {
      joined = true;
      break;
    }
    clk.advance_ms(10);
  }
  assert!(joined, "nodes did not converge before the user event");

  // A broadcasts a user event; drive gossip until B observes it.
  a.user_event("greeting", Bytes::from_static(b"hello"), false, clk.now())
    .expect("queue user event from a running node");

  let mut b_saw_user = false;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    while let Some(ev) = b.poll_event() {
      if matches!(ev, Event::User(_)) {
        b_saw_user = true;
      }
    }
    while a.poll_event().is_some() {}
    if b_saw_user {
      break;
    }
    clk.advance_ms(10);
  }
  assert!(b_saw_user, "b did not observe the user event A broadcast");
}
