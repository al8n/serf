//! Umbrella facade smoke test: `serf::tokio` hides the runtime.
//!
//! `serf::tokio::Serf<I, A>` is the reactor handle with `R = TokioRuntime` already
//! applied, so nothing below names a runtime type. The driver types come from
//! `serf::tokio` and the protocol types from `serf::proto`, which is the whole
//! point of the facade — two loopback nodes join over TCP and converge on a
//! two-member view without the caller depending on `serf-reactor` directly.

#![cfg(all(feature = "tokio", feature = "tcp"))]

use std::{net::SocketAddr, time::Duration};

use serf::{
  proto::options::Options as SerfOptions,
  tokio::{
    FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver,
    TcpTransportOptions, VoidDelegate,
  },
};
use smol_str::SmolStr;

/// Fast SWIM timing, so the two nodes converge well inside the poll loop below.
fn transport_opts(id: &str) -> TcpTransportOptions<SmolStr, SocketAddr> {
  TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(
      "127.0.0.1:0".parse().expect("loopback addr"),
    ))
    .with_probe_interval(Duration::from_millis(50))
    .with_probe_timeout(Duration::from_millis(100))
    .with_gossip_interval(Duration::from_millis(20))
}

async fn node(id: &str) -> Serf<SmolStr, SocketAddr> {
  Serf::<SmolStr, SocketAddr>::tcp(
    transport_opts(id),
    &SocketAddrResolver,
    &FirstAddrResolver,
    VoidDelegate::<SmolStr, SocketAddr>::new(),
    RuntimeOptions::new(),
    SerfOptions::new(),
    None,
    None,
    None,
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    std::sync::Arc::new(serf::tokio::VoidKeyringDelegate),
  )
  .await
  .expect("node builds on the tokio facade")
}

#[tokio::test]
async fn umbrella_tokio_facade_joins_over_tcp() {
  let a = node("a").await;
  let b = node("b").await;

  let seed = a.advertise_address();
  b.join(&SocketAddrResolver, MaybeResolved::Resolved(seed), false)
    .await
    .expect("b joins a");

  for _ in 0..100 {
    if a.num_members() == 2 && b.num_members() == 2 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  assert_eq!(a.num_members(), 2, "a converges on the two-member view");
  assert_eq!(b.num_members(), 2, "b converges on the two-member view");

  b.leave().await.expect("b leaves gracefully");
  b.shutdown().await.expect("b shuts down");
  a.shutdown().await.expect("a shuts down");
}
