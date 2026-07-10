//! Key-management end-to-end: two encrypted nodes rotate their keyring across the
//! cluster and BOTH nodes' LIVE wire keyrings follow.
//!
//! The regression this guards: a driver that applies key ops to a private shadow
//! copy reports a completed rotation the wire never sees. Here A drives
//! `install_key` → `use_key` → `remove_key`; each node's poll drain routes the
//! resulting `Event::KeyRequest` into the engine's `handle_key_request`, mutating
//! the coordinator's LIVE keyring. The fail-on-revert assertions read both nodes'
//! `keyring()` and require primary == K2 with K1 absent — which the old shadow model
//! (live keyring frozen at construction K1) cannot satisfy — then a post-rotation
//! user event still crosses the wire, proving both planes now run under K2.

// The whole suite exercises key rotation, so without an AEAD backend the file
// compiles to nothing — gating item-by-item would leave the shared harness
// helpers dead in a plaintext build.
#![cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]

mod harness;

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use serf_smoltcp::{
  Bytes, EncryptionOptions, EndpointOptions, Event, Instant, Keyring, MaybeResolved, Options,
  SecretKey, Serf, SerfOptions, SocketAddrResolver, TransformOptions,
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

/// A fixed AEAD key filled with `fill`, in whichever backend is compiled.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  {
    SecretKey::Aes256([fill; 32])
  }
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  {
    SecretKey::ChaCha20Poly1305([fill; 32])
  }
}

/// Poll both nodes to their returned deadlines until A observes a `KeyResponse`,
/// returning its `(num_resp, num_err)`. All other events are drained so neither
/// node's backlog stalls the gossip. The self-addressed response is looped back by
/// the driver, so the originator counts itself among the responders.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
fn drive_to_key_response(
  a: &mut Serf<SmolStr, SocketAddr, harness::PairedDevice>,
  b: &mut Serf<SmolStr, SocketAddr, harness::PairedDevice>,
  da: &mut harness::PairedDevice,
  db: &mut harness::PairedDevice,
  clk: &mut harness::Clock,
  budget: u32,
) -> Option<(usize, usize)> {
  for _ in 0..budget {
    let na = a.poll(clk.now(), da);
    let nb = b.poll(clk.now(), db);
    let mut seen = None;
    while let Some(ev) = a.poll_event() {
      if let Event::KeyResponse(kr) = ev {
        seen = Some((kr.num_resp, kr.num_err));
      }
    }
    while b.poll_event().is_some() {}
    if let Some(counts) = seen {
      return Some(counts);
    }
    let woke = da.inbound_pending() || db.inbound_pending();
    advance(clk, &[na, nb], woke);
  }
  None
}

/// Two encrypted nodes share primary K1, then A rotates the cluster to K2 via
/// `install_key` → `use_key` → `remove_key`. Each op propagates and every node's
/// drain applies it to the engine's LIVE wire keyring. The install query collects a
/// response from BOTH nodes; the fail-on-revert check reads both nodes' `keyring()`
/// and requires primary == K2 with K1 gone (unsatisfiable under a per-driver shadow
/// keyring); finally a user event still propagates A → B, proving the wire now runs
/// under K2 on both planes.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[test]
fn key_rotation_across_two_nodes_rotates_both_live_keyrings() {
  const BUDGET: u32 = 8000;

  let (mut da, mut db) = harness::link(1500);
  let mut clk = harness::Clock::new();
  let now = clk.now();

  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);

  let transform_a = TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(k1)));
  let transform_b = TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(k1)));

  let mut a: Serf<SmolStr, SocketAddr, _> = Serf::new(
    Options::new(),
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
    transform_a,
    EndpointOptions::new(SmolStr::new("a"), addr(1, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut da,
    now,
  );
  let mut b: Serf<SmolStr, SocketAddr, _> = Serf::new(
    Options::new(),
    harness::ip_iface(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
    transform_b,
    EndpointOptions::new(SmolStr::new("b"), addr(2, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut db,
    now,
  );

  a.start(now);
  b.start(now);

  // Converge on a 2-member view under K1 (a real encrypted push/pull join).
  let jid = b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, 7946))],
      false,
      now,
    )
    .expect("join from a running node");
  let mut joined = false;
  for _ in 0..BUDGET {
    let na = a.poll(clk.now(), &mut da);
    let nb = b.poll(clk.now(), &mut db);
    let _ = b.poll_join(jid);
    while a.poll_event().is_some() {}
    while b.poll_event().is_some() {}
    if a.num_members() == 2 && b.num_members() == 2 {
      joined = true;
      break;
    }
    let woke = da.inbound_pending() || db.inbound_pending();
    advance(&mut clk, &[na, nb], woke);
  }
  assert!(joined, "the encrypted nodes did not converge under K1");

  // install K2 across the cluster: the query collects a response from BOTH nodes,
  // none in error, and each node's live keyring gains K2 as a secondary.
  a.install_key(k2, clk.now()).expect("install_key");
  let (num_resp, num_err) =
    drive_to_key_response(&mut a, &mut b, &mut da, &mut db, &mut clk, BUDGET)
      .expect("the install_key query must close with a KeyResponse");
  assert!(
    num_resp >= 2,
    "install_key must collect a response from BOTH nodes (num_resp={num_resp})"
  );
  assert_eq!(num_err, 0, "install_key must succeed on every node");
  assert!(
    a.keyring()
      .expect("a encrypted")
      .secondaries()
      .contains(&k2),
    "A's live keyring must gain K2 as a secondary"
  );
  assert!(
    b.keyring()
      .expect("b encrypted")
      .secondaries()
      .contains(&k2),
    "B's live keyring must gain K2 as a secondary"
  );

  // use K2 across the cluster: both nodes promote K2 to primary.
  a.use_key(k2, clk.now()).expect("use_key");
  let (_, num_err) = drive_to_key_response(&mut a, &mut b, &mut da, &mut db, &mut clk, BUDGET)
    .expect("the use_key query must close with a KeyResponse");
  assert_eq!(num_err, 0, "use_key must succeed on every node");
  assert_eq!(a.keyring().unwrap().primary_ref(), &k2, "A must promote K2");
  assert_eq!(b.keyring().unwrap().primary_ref(), &k2, "B must promote K2");

  // remove K1 across the cluster: both nodes drop the old key.
  a.remove_key(k1, clk.now()).expect("remove_key");
  let (_, num_err) = drive_to_key_response(&mut a, &mut b, &mut da, &mut db, &mut clk, BUDGET)
    .expect("the remove_key query must close with a KeyResponse");
  assert_eq!(num_err, 0, "remove_key must succeed on every node");

  // FAIL-ON-REVERT: BOTH nodes' LIVE keyrings show primary == K2 and K1 absent. On
  // the old shadow model the coordinator keyring never rotated, so B's live keyring
  // would still be K1-primary here and these assertions would fail.
  for (name, node) in [("a", &a), ("b", &b)] {
    let kr = node
      .keyring()
      .unwrap_or_else(|| panic!("{name} must be encrypted"));
    assert_eq!(
      kr.primary_ref(),
      &k2,
      "{name}: primary must be the promoted K2"
    );
    assert_ne!(kr.primary_ref(), &k1, "{name}: K1 must not be the primary");
    assert!(
      !kr.secondaries().contains(&k1),
      "{name}: the removed K1 must be absent from the live keyring"
    );
  }

  // Post-rotation traffic proof: a user event still crosses the wire, which now runs
  // under K2 on both nodes — the reliable and gossip planes rotated with the keyring.
  a.user_event("after-rotation", Bytes::from_static(b"payload"), false)
    .expect("user_event from a running node");
  let mut b_saw_user = false;
  for _ in 0..BUDGET {
    let na = a.poll(clk.now(), &mut da);
    let nb = b.poll(clk.now(), &mut db);
    while let Some(ev) = b.poll_event() {
      if matches!(ev, Event::User(_)) {
        b_saw_user = true;
      }
    }
    while a.poll_event().is_some() {}
    if b_saw_user {
      break;
    }
    let woke = da.inbound_pending() || db.inbound_pending();
    advance(&mut clk, &[na, nb], woke);
  }
  assert!(
    b_saw_user,
    "a user event must still propagate A -> B after the rotation (wire under K2)"
  );
}
