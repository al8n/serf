//! Key-management end-to-end over the reactor stream driver: two encrypted nodes
//! rotate their keyring across the cluster and BOTH nodes' LIVE wire keyrings
//! follow.
//!
//! The regression this guards: a driver that applies key ops to a caller-held
//! shadow reports a completed rotation the wire never sees. Node A drives
//! `install_key` -> `use_key` -> `remove_key` through the public commands; each op
//! propagates as a cluster query and every node applies it to the coordinator's
//! LIVE `EncryptionOptions` keyring, then answers from that post-op state.
//!
//! The rotation is observed two independent ways, both reading live state rather
//! than a shadow: each node's [`KeyringDelegate`] records the exact ring the driver
//! published to its endpoint, and a final `list_keys` tallies every node's live
//! ring cluster-wide. Both must show primary == K2 with K1 gone — unreachable under
//! the pre-fix shadow model, where the coordinator keyring stayed frozen at
//! construction K1. A post-rotation user event then still crosses the wire, proving
//! both planes now run under K2, and the observer's fire count proves it fired
//! exactly once per successful mutation and never for a `list` or a refused op.
//!
//! The scenario body is a runtime-generic `async fn <R: Runtime>` helper, so the
//! same test runs as a `#[tokio::test]` cell over `TokioRuntime` and as a `_smol`
//! cell driven by `SmolRuntime::block_on`, mirroring the reactor's other real-node
//! suites.

#![cfg(all(
  feature = "tcp",
  any(feature = "aes-gcm", feature = "chacha20-poly1305")
))]

use core::time::Duration;
use std::{
  net::SocketAddr,
  sync::{Arc, Mutex},
};

use agnostic::Runtime;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serf_proto::{
  event::{Event, KeyResponse},
  options::Options as SerfOptions,
};
use serf_reactor::{
  EncryptionOptions, FirstAddrResolver, Keyring, KeyringDelegate, MaybeResolved, RuntimeOptions,
  SecretKey, Serf, SocketAddrResolver, TcpTransportOptions, VoidDelegate,
};
use smol_str::SmolStr;

/// A reactor TCP node handle over the agnostic runtime `R`.
type Node<R> = Serf<SmolStr, SocketAddr, R>;

/// A [`KeyringDelegate`] that records, in order, every live keyring the driver
/// publishes through `keyring_updated`. The driver fires it only after it has
/// pushed the rotated ring to the endpoint via `set_encryption_options`, so the
/// recorded ring is exactly the ring the gossip and reliable planes now encrypt
/// under — the live-wire observable this suite asserts on.
#[derive(Default)]
struct RecordingKeyring {
  rings: Mutex<Vec<Keyring>>,
}

impl RecordingKeyring {
  /// The keyrings observed so far, oldest first.
  fn rings(&self) -> Vec<Keyring> {
    self.rings.lock().expect("keyring log not poisoned").clone()
  }
}

impl KeyringDelegate for RecordingKeyring {
  fn keyring_updated(&self, keyring: &Keyring) {
    self
      .rings
      .lock()
      .expect("keyring log not poisoned")
      .push(keyring.clone());
  }
}

/// A deterministic test secret key, selecting whichever AEAD cipher this build
/// compiled so the test works under either backend.
fn secret_key(fill: u8) -> SecretKey {
  #[cfg(feature = "aes-gcm")]
  let key = SecretKey::Aes256([fill; 32]);
  #[cfg(all(not(feature = "aes-gcm"), feature = "chacha20-poly1305"))]
  let key = SecretKey::ChaCha20Poly1305([fill; 32]);
  key
}

/// Build and spawn a reactor TCP node on an ephemeral loopback port with
/// `encryption` as its keyring policy and `keyring` as its rotation observer.
async fn spawn_encrypted_node<R>(
  id: &str,
  encryption: EncryptionOptions,
  keyring: Arc<dyn KeyringDelegate>,
) -> Node<R>
where
  R: Runtime,
{
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_encryption(encryption);
  Serf::<SmolStr, SocketAddr, R>::tcp(
    opts,
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    keyring,
  )
  .await
  .expect("spawn encrypted serf tcp node")
}

/// Poll both nodes until each reports the full two-member cluster, or fail on a
/// generous timeout so a convergence regression surfaces as a timeout, not a hang.
async fn converge<R>(a: &Node<R>, b: &Node<R>)
where
  R: Runtime,
{
  R::timeout(Duration::from_secs(20), async {
    loop {
      if a.num_members() == 2 && b.num_members() == 2 {
        break;
      }
      R::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("both nodes converge to a 2-member cluster");
}

/// Drive `events` until the originator's next `KeyResponse` surfaces (emitted when
/// the key query's deadline fires), draining any interleaved membership / gossip
/// events so a backlog cannot stall the stream.
async fn next_key_response<R, S>(events: &mut S) -> KeyResponse<SmolStr>
where
  R: Runtime,
  S: Stream<Item = Event<SmolStr, SocketAddr>> + Unpin + Send,
{
  R::timeout(Duration::from_secs(20), async {
    loop {
      match events.next().await {
        Some(Event::KeyResponse(kr)) => break kr,
        Some(_) => {}
        None => panic!("event stream closed before a KeyResponse"),
      }
    }
  })
  .await
  .expect("a KeyResponse within the timeout")
}

/// Two encrypted nodes share primary K1, then A rotates the cluster to K2 via
/// `install_key` -> `use_key` -> `remove_key`. Each op propagates and every node
/// applies it to its LIVE wire keyring. The fail-on-revert check reads both nodes'
/// recorded live rings (and cross-checks cluster-wide via `list_keys`) and requires
/// primary == K2 with K1 gone; the observer must have fired exactly once per
/// mutation and not for the read-only `list` or the refused final remove; finally a
/// user event still propagates A -> B, proving the wire now runs under K2.
async fn key_rotation_across_two_nodes_rotates_both_live_keyrings<R>()
where
  R: Runtime,
{
  let k1 = secret_key(0x11);
  let k2 = secret_key(0x22);

  let rec_a = Arc::new(RecordingKeyring::default());
  let rec_b = Arc::new(RecordingKeyring::default());
  let enc = || EncryptionOptions::new().with_keyring(Keyring::new(k1));

  let b = spawn_encrypted_node::<R>("rot-b", enc(), rec_b.clone()).await;
  let a = spawn_encrypted_node::<R>("rot-a", enc(), rec_a.clone()).await;
  let b_addr = b.advertise_address();

  // Converge on a 2-member view under K1 (a real encrypted push/pull join).
  a.join(&SocketAddrResolver, MaybeResolved::Resolved(b_addr), false)
    .await
    .expect("join reaches node B over the encrypted reliable plane");
  converge(&a, &b).await;

  // Subscribe before issuing any key op so no KeyResponse races the subscription.
  let mut a_events = a.events();
  let mut b_events = b.events();

  // install K2: both nodes gain it as a secondary in their live ring.
  a.install_key(k2).await.expect("install_key dispatched");
  let kr = next_key_response::<R, _>(&mut a_events).await;
  assert!(
    kr.num_resp >= 2,
    "install_key must collect a response from BOTH nodes (num_resp={})",
    kr.num_resp
  );
  assert_eq!(kr.num_err, 0, "install_key must succeed on every node");

  // use K2: both nodes promote it to primary.
  a.use_key(k2).await.expect("use_key dispatched");
  let kr = next_key_response::<R, _>(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "use_key must succeed on every node");

  // remove K1: both nodes drop the old key.
  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response::<R, _>(&mut a_events).await;
  assert_eq!(kr.num_err, 0, "remove_key must succeed on every node");

  // FAIL-ON-REVERT: each node's observer captured exactly the ring the driver
  // published to its endpoint at each mutation, in order — install adds K2 as a
  // secondary under K1, use promotes K2, remove drops K1. Under the pre-fix shadow
  // model the coordinator keyring never rotated, so this sequence is unreachable.
  for (name, rec) in [("rot-a", &rec_a), ("rot-b", &rec_b)] {
    let rings = rec.rings();
    assert_eq!(
      rings.len(),
      3,
      "{name}: the observer fires exactly once per successful mutation"
    );
    assert_eq!(
      rings[0].primary_ref(),
      &k1,
      "{name}: install leaves K1 as primary"
    );
    assert!(
      rings[0].secondaries().contains(&k2),
      "{name}: install adds K2 as a secondary"
    );
    assert_eq!(rings[1].primary_ref(), &k2, "{name}: use promotes K2");
    assert_eq!(
      rings[2].primary_ref(),
      &k2,
      "{name}: K2 stays primary after the remove"
    );
    assert!(
      !rings[2].secondaries().contains(&k1),
      "{name}: the removed K1 is absent from the live keyring"
    );
  }

  // Independent, endpoint-direct cluster cross-check: `list_keys` tallies every
  // node's LIVE ring — K2 primary on BOTH nodes and K1 installed on none.
  a.list_keys().await.expect("list_keys dispatched");
  let kr = next_key_response::<R, _>(&mut a_events).await;
  assert_eq!(
    kr.primary_keys.get(&k2).copied(),
    Some(2),
    "both nodes report K2 as their live primary"
  );
  assert_eq!(
    kr.keys.get(&k2).copied(),
    Some(2),
    "both nodes still hold K2 in their live ring"
  );
  assert_eq!(
    kr.keys.get(&k1),
    None,
    "the removed K1 is installed on no node"
  );

  // `list_keys` is read-only: it must NOT have fired the observer.
  assert_eq!(
    rec_a.rings().len(),
    3,
    "list_keys does not fire the keyring observer"
  );
  assert_eq!(
    rec_b.rings().len(),
    3,
    "list_keys does not fire the keyring observer"
  );

  // A refused op must not fire the observer either: removing the already-gone K1 is
  // refused on every node, leaving both live rings — and the observer — untouched.
  a.remove_key(k1).await.expect("remove_key dispatched");
  let kr = next_key_response::<R, _>(&mut a_events).await;
  assert!(
    kr.num_err >= 1,
    "removing an absent key is refused (num_err={})",
    kr.num_err
  );
  assert_eq!(
    rec_a.rings().len(),
    3,
    "a refused op does not fire the keyring observer"
  );
  assert_eq!(
    rec_b.rings().len(),
    3,
    "a refused op does not fire the keyring observer"
  );

  // Post-rotation traffic proof: a user event still crosses the wire, which now runs
  // under K2 on both nodes — the reliable and gossip planes rotated with the keyring.
  a.user_event("after-rotation", Bytes::from_static(b"payload"), false)
    .await
    .expect("user_event from a running node");
  let saw = R::timeout(Duration::from_secs(20), async {
    loop {
      match b_events.next().await {
        Some(Event::User(u)) if u.name.as_str() == "after-rotation" => break true,
        Some(_) => {}
        None => break false,
      }
    }
  })
  .await
  .expect("B observes the post-rotation user event within the timeout");
  assert!(
    saw,
    "a user event must still propagate A -> B after the rotation (wire under K2)"
  );

  a.shutdown().await.expect("rot-a shuts down");
  b.shutdown().await.expect("rot-b shuts down");
}

// The tokio cell: the runtime-generic scenario driven on tokio's multi-thread
// runtime. Gated on `tokio` so the `--test key_rotation -- smol` build can drop the
// `agnostic/tokio` code path.
#[cfg(feature = "tokio")]
mod tokio_cells {
  use agnostic::tokio::TokioRuntime;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn key_rotation_across_two_nodes_rotates_both_live_keyrings() {
    super::key_rotation_across_two_nodes_rotates_both_live_keyrings::<TokioRuntime>().await;
  }
}

// The smol cell: the identical scenario over `SmolRuntime`, driven by smol's
// `block_on`. `cargo test --test key_rotation -- smol` selects exactly this.
#[cfg(feature = "smol")]
mod smol_cells {
  use agnostic::{RuntimeLite, smol::SmolRuntime};

  #[test]
  fn key_rotation_across_two_nodes_rotates_both_live_keyrings_smol() {
    SmolRuntime::block_on(
      super::key_rotation_across_two_nodes_rotates_both_live_keyrings::<SmolRuntime>(),
    );
  }
}
