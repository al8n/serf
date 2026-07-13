//! Key-management end-to-end: two encrypted nodes rotate their keyring across the
//! cluster and BOTH nodes' LIVE wire keyrings follow.
//!
//! The regression this guards: a driver that applies key ops to a private shadow
//! copy reports a completed rotation the wire never sees. Here A drives
//! `install_key` -> `use_key` -> `remove_key`; each node's runner drain routes the
//! resulting `Event::KeyRequest` into the engine's `handle_key_request`, mutating
//! the coordinator's LIVE keyring. The fail-on-revert assertions read both nodes'
//! `keyring()` and require primary == K2 with K1 absent, then a post-rotation user
//! event still crosses the wire, proving both planes now run under K2.

// The whole suite exercises key rotation, so without an AEAD backend the file
// compiles to nothing — gating item-by-item would leave the shared harness
// helpers dead in a plaintext build.
#![cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#![allow(clippy::collapsible_if)]

mod support;

use core::net::SocketAddr;

use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{
  Bytes, EncryptionOptions, Event, Keyring, SecretKey, Serf, TransformOptions, now,
};
use smol_str::SmolStr;

use support::cluster::{
  NodeBufs, POOL, build_node, build_sockets, build_stack, devices, drive, join_and_converge,
};

/// A fixed AEAD key filled with `fill`, in whichever backend is compiled.
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

/// Poll A until it observes a `KeyResponse`, returning its `(num_resp, num_err)`.
/// B's events are drained too so neither node's backlog stalls the gossip. The
/// self-addressed response is looped back by the driver, so the originator counts
/// itself among the responders.
async fn drive_to_key_response(
  a: &Serf<SmolStr, SocketAddr>,
  b: &Serf<SmolStr, SocketAddr>,
) -> (usize, usize) {
  loop {
    let mut seen = None;
    while let Some(ev) = a.poll_event() {
      if let Event::KeyResponse(kr) = ev {
        seen = Some((kr.num_resp, kr.num_err));
      }
    }
    while b.poll_event().is_some() {}
    if let Some(counts) = seen {
      return counts;
    }
    Timer::after(Duration::from_millis(5)).await;
  }
}

/// Two encrypted nodes share primary K1, then A rotates the cluster to K2 via
/// `install_key` -> `use_key` -> `remove_key`. Each op propagates and every node's
/// drain applies it to the LIVE wire keyring. The fail-on-revert check reads both
/// nodes' `keyring()` and requires primary == K2 with K1 gone; finally a user event
/// still propagates A -> B, proving the wire now runs under K2 on both planes.
#[test]
fn key_rotation_across_two_nodes_rotates_both_live_keyrings() {
  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);

  let transform_a = TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(k1)));
  let transform_b = TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(k1)));

  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now = now();
  let (ml_a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, transform_a);
  let (ml_b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, transform_b);

  block_on(async {
    let op = async {
      // Converge on a 2-member view under K1 (a real encrypted push/pull join).
      join_and_converge(&ml_a, &ml_b).await;

      // install K2 across the cluster: the query collects a response from BOTH
      // nodes, none in error, and each node's live keyring gains K2 as a secondary.
      ml_a.install_key(k2).expect("install_key");
      let (num_resp, num_err) = drive_to_key_response(&ml_a, &ml_b).await;
      assert!(
        num_resp >= 2,
        "install_key must collect a response from BOTH nodes (num_resp={num_resp})"
      );
      assert_eq!(num_err, 0, "install_key must succeed on every node");
      assert!(
        ml_a
          .keyring()
          .expect("a encrypted")
          .secondaries()
          .contains(&k2),
        "A's live keyring must gain K2 as a secondary"
      );
      assert!(
        ml_b
          .keyring()
          .expect("b encrypted")
          .secondaries()
          .contains(&k2),
        "B's live keyring must gain K2 as a secondary"
      );

      // use K2 across the cluster: both nodes promote K2 to primary.
      ml_a.use_key(k2).expect("use_key");
      let (_, num_err) = drive_to_key_response(&ml_a, &ml_b).await;
      assert_eq!(num_err, 0, "use_key must succeed on every node");
      assert_eq!(
        ml_a.keyring().expect("a encrypted").primary_ref(),
        &k2,
        "A must promote K2"
      );
      assert_eq!(
        ml_b.keyring().expect("b encrypted").primary_ref(),
        &k2,
        "B must promote K2"
      );

      // remove K1 across the cluster: both nodes drop the old key.
      ml_a.remove_key(k1).expect("remove_key");
      let (_, num_err) = drive_to_key_response(&ml_a, &ml_b).await;
      assert_eq!(num_err, 0, "remove_key must succeed on every node");

      // FAIL-ON-REVERT: BOTH nodes' LIVE keyrings show primary == K2 and K1 absent.
      for (name, kr) in [
        ("a", ml_a.keyring().expect("a encrypted")),
        ("b", ml_b.keyring().expect("b encrypted")),
      ] {
        assert_eq!(
          kr.primary_ref(),
          &k2,
          "{name}: primary must be the promoted K2"
        );
        assert!(
          !kr.secondaries().contains(&k1),
          "{name}: the removed K1 must be absent from the live keyring"
        );
      }

      // Post-rotation traffic proof: a user event still crosses the wire, which now
      // runs under K2 on both nodes.
      ml_a
        .user_event("after-rotation", Bytes::from_static(b"payload"), false)
        .expect("user_event from a running node");
      loop {
        if let Some(ev) = ml_b.poll_event() {
          if matches!(ev, Event::User(_)) {
            return true;
          }
        } else {
          Timer::after(Duration::from_millis(5)).await;
        }
      }
    };
    let ok = drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
    assert!(
      ok,
      "a user event must still propagate A -> B after the rotation"
    );
  });
}
