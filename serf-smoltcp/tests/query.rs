//! Query-plane end-to-end over the smoltcp gossip self-delivery loopback.
//!
//! serf directs a node's response to its OWN locally-originated query / key-request
//! to the originator's advertise address = THIS node's address. A real OS UDP socket
//! loops such a self-addressed datagram back into recv; smoltcp does not, so the
//! driver must emulate the OS self-delivery. These tests exercise that path: the
//! query round-trip proves the originator observes its OWN (local, self-addressed)
//! response alongside the remote peer's, and the key-response test proves a
//! locally-originated key op counts the local node in `num_resp` by the query
//! deadline.

mod harness;

use core::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  time::Duration,
};

use serf_smoltcp::{
  Bytes, EndpointOptions, Event, Instant, MaybeResolved, Options, QueryEvent, QueryParams, Serf,
  SerfOptions, SocketAddrResolver, TransformOptions,
};
use smol_str::SmolStr;

fn addr(ip: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// Advance the shared clock the way a real event loop sleeps: a delivered frame
/// wakes its receiver at once, otherwise sleep to the soonest returned deadline.
/// Never stalls — a `now`-valued or absent deadline steps a single millisecond.
fn advance(clk: &mut harness::Clock, targets: &[Option<Instant>], woke: bool) {
  if woke {
    clk.advance_ms(1);
    return;
  }
  let target = targets.iter().copied().flatten().min();
  match target {
    Some(t) if t > clk.now() => clk.advance_to(t),
    _ => clk.advance_ms(1),
  }
}

/// Drain a node's buffered events: answer every observed `Event::Query` (directing
/// the reply to the query's originator), and record every `Event::QueryResponse`'s
/// responder id. Draining also keeps the node's app-event buffer from shedding.
fn service_queries(
  node: &mut Serf<SmolStr, SocketAddr, harness::PairedDevice>,
  now: Instant,
  responders: &mut Vec<SmolStr>,
) {
  let mut pending: Vec<QueryEvent<SmolStr, SocketAddr>> = Vec::new();
  while let Some(ev) = node.poll_event() {
    match ev {
      Event::Query(qe) => pending.push(qe),
      Event::QueryResponse(qr) => responders.push(qr.from().id_ref().clone()),
      _ => {}
    }
  }
  for qe in pending {
    // Ignoring Err: a duplicate / past-deadline respond is a no-op the test tolerates.
    let _ = node.respond(&qe, Bytes::from_static(b"pong"), now);
  }
}

/// Two converged nodes; one issues a query; the originator must collect responses
/// from BOTH the remote peer AND itself. The local node self-processes its own
/// query and directs its reply to its own advertise address — a self-addressed
/// gossip datagram that smoltcp does not loop back on its own. Without the driver's
/// self-delivery loopback the originator never observes its OWN response, so
/// `a_resp` would hold only `"b"` and this test fails.
#[test]
fn query_collects_local_and_remote_responses() {
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

  // Converge on a 2-member view first (proven fixed cadence).
  let mut joined = false;
  for _ in 0..BUDGET {
    let _ = a.poll(clk.now(), &mut da);
    let _ = b.poll(clk.now(), &mut db);
    let _ = b.poll_join(jid);
    while a.poll_event().is_some() {}
    while b.poll_event().is_some() {}
    if a.num_members() == 2 && b.num_members() == 2 {
      joined = true;
      break;
    }
    clk.advance_ms(10);
  }
  assert!(joined, "nodes did not converge before the query");

  // A issues a query; both nodes answer every Event::Query they observe.
  a.query(
    "ping",
    Bytes::from_static(b"q"),
    QueryParams {
      timeout: Duration::from_secs(5),
      ..Default::default()
    },
    clk.now(),
  )
  .expect("query from a running node");

  // Drive to the query deadline via the returned poll deadlines (not a fixed
  // cadence). A must collect responses from BOTH itself ("a", via the loopback) and
  // B ("b", over the wire).
  let mut a_resp: Vec<SmolStr> = Vec::new();
  let mut b_resp: Vec<SmolStr> = Vec::new();
  let mut done = false;
  for _ in 0..BUDGET {
    let a_next = a.poll(clk.now(), &mut da);
    let b_next = b.poll(clk.now(), &mut db);
    service_queries(&mut a, clk.now(), &mut a_resp);
    service_queries(&mut b, clk.now(), &mut b_resp);
    if a_resp.iter().any(|id| id == "a") && a_resp.iter().any(|id| id == "b") {
      done = true;
      break;
    }
    let woke = da.inbound_pending() || db.inbound_pending();
    advance(&mut clk, &[a_next, b_next], woke);
  }

  assert!(
    done,
    "originator did not collect both responses in time: a_resp={a_resp:?}"
  );
  assert!(
    a_resp.iter().any(|id| id == "a"),
    "originator did not observe its OWN self-addressed, looped-back response: a_resp={a_resp:?}"
  );
  assert!(
    a_resp.iter().any(|id| id == "b"),
    "originator did not observe the remote peer's response: a_resp={a_resp:?}"
  );
}

/// A locally-originated key query self-processes and directs its response to the
/// node's own advertise address. Driven only to the returned poll deadlines, the
/// key op must complete with the local node counted in `num_resp` — proving the
/// self-addressed key response is looped back, ingested, and collected within the
/// tick rather than stranded past the query deadline. Without the loopback the
/// encrypted self-response is dropped on the wire and `num_resp` is 0.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_query_collects_local_response_within_deadline() {
  use serf_smoltcp::{EncryptionOptions, Keyring, SecretKey};

  const BUDGET: u32 = 4000;

  let (mut dev, _peer) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

  // Either AEAD backend gates this test; pick whichever key variant is compiled.
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([0x42; 32]);
  #[cfg(all(feature = "chacha20-poly1305", not(feature = "aes-gcm")))]
  let key = SecretKey::ChaCha20Poly1305([0x42; 32]);
  let transform = TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(key)));

  let mut a: Serf<SmolStr, SocketAddr, _> = Serf::new(
    Options::new(),
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
    transform,
    EndpointOptions::new(SmolStr::new("a"), addr(1, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut dev,
    now,
  );

  a.start(now);

  // Let the local node register its own membership: an internal (key) query counts
  // a response only from a known member, and the self-response is from this node.
  let mut registered = false;
  for _ in 0..200 {
    let next = a.poll(clk.now(), &mut dev);
    while a.poll_event().is_some() {}
    if a.num_members() >= 1 {
      registered = true;
      break;
    }
    let woke = dev.inbound_pending();
    advance(&mut clk, &[next], woke);
  }
  assert!(registered, "local node did not register its own membership");

  a.list_keys(clk.now())
    .expect("list_keys from a running node");

  // Drive only to the returned poll deadlines until the key op closes (at the query
  // deadline). num_resp must include the local node's own looped-back response.
  let mut num_resp = None;
  for _ in 0..BUDGET {
    let next = a.poll(clk.now(), &mut dev);
    while let Some(ev) = a.poll_event() {
      if let Event::KeyResponse(kr) = ev {
        num_resp = Some(kr.num_resp);
      }
    }
    if num_resp.is_some() {
      break;
    }
    let woke = dev.inbound_pending();
    advance(&mut clk, &[next], woke);
  }

  let n = num_resp.expect("the list_keys query must complete with a KeyResponse by its deadline");
  assert!(
    n >= 1,
    "the local node's own key response must be collected via the self-delivery loopback (num_resp={n})"
  );
}
