//! Key-management REFUSALS end-to-end: the ops a node must decline, and the one it
//! must accept as a no-op, with the LIVE wire keyring left untouched either way.
//!
//! The happy-path rotation is covered by the `key_rotation` suite. What is pinned
//! here is the other half of the contract — every refusal reaches the originator as
//! a counted, message-carrying error rather than a silent success, and no refused
//! op re-keys the wire:
//!
//! - promoting the CURRENT primary is a trivial success that installs nothing,
//! - promoting or removing a key that is not installed is refused,
//! - removing the primary is refused (an operator promotes a secondary first),
//! - `list_keys` reports the live census without changing it,
//! - a key op on an UNENCRYPTED cluster is refused, never silently accepted.
//!
//! Each test issues at most two key-management queries, because every one of them
//! runs to its own multi-second deadline and the suite shares one wall-clock cap.

// The whole suite exercises key management, so without an AEAD backend the file
// compiles to nothing — gating item-by-item would leave the shared harness
// helpers dead in a plaintext build.
#![cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#![allow(clippy::collapsible_if)]

mod support;

use core::net::SocketAddr;

use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{EncryptionOptions, Event, Keyring, SecretKey, Serf, TransformOptions, now};
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

/// The keyring every encrypted node in this suite starts from: K1 as the sole
/// primary, no secondaries.
fn keyed(k1: SecretKey) -> TransformOptions {
  TransformOptions::default()
    .with_encryption(EncryptionOptions::new().with_keyring(Keyring::new(k1)))
}

/// One closed key-management query: the aggregate counts, every responding node's
/// failure message, and the key census the responses reported.
struct KeyOutcome {
  num_resp: usize,
  num_err: usize,
  messages: std::vec::Vec<SmolStr>,
  keys: std::vec::Vec<SecretKey>,
  primary_keys: std::vec::Vec<SecretKey>,
}

/// Poll A until its in-flight key query closes, draining B so neither node's
/// backlog stalls the gossip.
async fn drive_to_key_response(
  a: &Serf<SmolStr, SocketAddr>,
  b: &Serf<SmolStr, SocketAddr>,
) -> KeyOutcome {
  loop {
    let mut seen = None;
    while let Some(ev) = a.poll_event() {
      if let Event::KeyResponse(kr) = ev {
        seen = Some(KeyOutcome {
          num_resp: kr.num_resp,
          num_err: kr.num_err,
          messages: kr.messages.values().cloned().collect(),
          keys: kr.keys.keys().copied().collect(),
          primary_keys: kr.primary_keys.keys().copied().collect(),
        });
      }
    }
    while b.poll_event().is_some() {}
    if let Some(outcome) = seen {
      return outcome;
    }
    Timer::after(Duration::from_millis(5)).await;
  }
}

/// Both nodes' live keyrings still hold `primary` alone — no refused or trivial op
/// may re-key the wire or install a secondary behind it.
fn assert_ring_untouched(
  a: &Serf<SmolStr, SocketAddr>,
  b: &Serf<SmolStr, SocketAddr>,
  primary: &SecretKey,
) {
  for (name, node) in [("a", a), ("b", b)] {
    let kr = node
      .keyring()
      .unwrap_or_else(|| panic!("{name} must be encrypted"));
    assert_eq!(
      kr.primary_ref(),
      primary,
      "{name}: the live primary must be unchanged"
    );
    assert!(
      kr.secondaries().is_empty(),
      "{name}: nothing may be installed on the live keyring"
    );
  }
}

/// Promoting the key that is ALREADY primary is a trivial success on every node:
/// reported as a success, and the ring is not re-keyed behind it.
#[test]
fn promoting_the_current_primary_installs_nothing() {
  let k1 = secret_key(0x11);

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
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, keyed(k1));
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, keyed(k1));

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      a.use_key(k1).expect("use_key from a running node");
      let out = drive_to_key_response(&a, &b).await;

      assert!(
        out.num_resp >= 2,
        "both nodes must answer (num_resp={})",
        out.num_resp
      );
      assert_eq!(
        out.num_err, 0,
        "promoting the current primary is a no-op success, not a failure: {:?}",
        out.messages
      );
      assert_ring_untouched(&a, &b, &k1);
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}

/// A `use` or `remove` of a key NO node has installed is refused everywhere, with
/// the cause named, and leaves the live keyring untouched — so a key op can never
/// silently resolve to some other installed key.
#[test]
fn uninstalled_keys_are_refused_by_every_node() {
  let k1 = secret_key(0x11);
  let absent = secret_key(0x99);

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
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, keyed(k1));
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, keyed(k1));

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      a.use_key(absent).expect("use_key from a running node");
      let out = drive_to_key_response(&a, &b).await;
      assert_eq!(
        out.num_err, 2,
        "a `use` of an uninstalled key must be refused on BOTH nodes"
      );
      assert!(
        out
          .messages
          .iter()
          .all(|m| m.as_str() == "requested key is not installed"),
        "each refusal must name its cause: {:?}",
        out.messages
      );

      a.remove_key(absent)
        .expect("remove_key from a running node");
      let out = drive_to_key_response(&a, &b).await;
      assert_eq!(
        out.num_err, 2,
        "a `remove` of an uninstalled key must be refused on BOTH nodes"
      );
      assert!(
        out
          .messages
          .iter()
          .all(|m| m.as_str() == "requested key is not installed"),
        "each refusal must name its cause: {:?}",
        out.messages
      );

      assert_ring_untouched(&a, &b, &k1);
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}

/// Removing the PRIMARY is refused on every node — an operator promotes a secondary
/// first, or the cluster would be left with no key to encrypt under — and the
/// read-only `list_keys` census then still reports that primary as installed.
#[test]
fn the_primary_cannot_be_removed_and_the_census_still_reports_it() {
  let k1 = secret_key(0x11);

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
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, keyed(k1));
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, keyed(k1));

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      a.remove_key(k1).expect("remove_key from a running node");
      let out = drive_to_key_response(&a, &b).await;
      assert_eq!(
        out.num_err, 2,
        "removing the primary must be refused on BOTH nodes"
      );
      assert!(
        out
          .messages
          .iter()
          .all(|m| m.as_str() == "cannot remove the primary key; promote a secondary first"),
        "each refusal must direct the operator: {:?}",
        out.messages
      );
      assert_ring_untouched(&a, &b, &k1);

      // The census is read-only and still sees the key the refused remove targeted.
      a.list_keys().expect("list_keys from a running node");
      let out = drive_to_key_response(&a, &b).await;
      assert_eq!(
        out.num_err, 0,
        "listing keys never fails on an encrypted node: {:?}",
        out.messages
      );
      assert!(
        out.keys.contains(&k1),
        "the census must report the installed key: {:?}",
        out.keys
      );
      assert_eq!(
        out.primary_keys,
        std::vec![k1],
        "the census must report K1 as the only primary"
      );
      assert_ring_untouched(&a, &b, &k1);
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}

/// A key op issued on an UNENCRYPTED cluster is refused by every node with a message
/// naming the missing keyring — never silently reported as an applied rotation,
/// which would leave an operator believing the cluster is keyed.
#[test]
fn an_unencrypted_cluster_refuses_key_management() {
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
  let (a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, TransformOptions::default());
  let (b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, TransformOptions::default());

  assert!(
    a.keyring().is_none(),
    "a plaintext node carries no wire keyring"
  );

  block_on(async {
    let op = async {
      join_and_converge(&a, &b).await;

      a.install_key(secret_key(0x33))
        .expect("the query is accepted; the nodes refuse the op");
      let out = drive_to_key_response(&a, &b).await;

      assert!(
        out.num_resp >= 2,
        "both nodes must answer (num_resp={})",
        out.num_resp
      );
      assert_eq!(
        out.num_err, out.num_resp,
        "every unencrypted node must refuse the op"
      );
      assert!(
        out
          .messages
          .iter()
          .all(|m| m.as_str() == "no keyring configured on this node"),
        "each refusal must name the missing keyring: {:?}",
        out.messages
      );
      assert!(
        a.keyring().is_none() && b.keyring().is_none(),
        "a refused install must not conjure a keyring onto the wire"
      );
    };
    drive(op, run_a, run_b, &mut net_a, &mut net_b).await;
  });
}
