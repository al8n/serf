//! Construction-time guards for the TCP transport: advertise-address validation
//! (a wildcard bind must not be gossiped as a contact) and the per-node serf
//! RNG seeding (two fresh nodes must draw independent entropy).

use core::time::Duration;
use std::net::SocketAddr;

use memberlist_proto::MaybeResolved;
use rand::Rng;
use smol_str::SmolStr;

use crate::{
  FirstAddrResolver, SerfError, SocketAddrResolver, StreamTransportOptions, TcpTransport,
  TcpTransportOptions, Transport,
};

/// Binding the wildcard `0.0.0.0:0` reads an unspecified IP back from the
/// socket; gossiping it would publish an undialable contact, so construction
/// must reject it with `InvalidAdvertiseAddr` rather than join as an unreachable
/// member.
#[compio::test]
async fn new_rejects_wildcard_advertise() {
  let wildcard: SocketAddr = "0.0.0.0:0".parse().expect("wildcard addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("wild-node"))
    .with_advertise_addr(MaybeResolved::Resolved(wildcard));
  let res =
    TcpTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  match res {
    Err(SerfError::InvalidAdvertiseAddr(e)) => {
      assert!(
        e.addr().ip().is_unspecified(),
        "the rejected address carries the unspecified IP read back from the wildcard bind"
      );
    }
    Err(other) => panic!("expected InvalidAdvertiseAddr, got {other:?}"),
    Ok(_) => panic!("a wildcard advertise must be rejected, but construction succeeded"),
  }
}

/// A zero `dial_timeout` makes every outbound dial resolve as an immediate
/// biased-select timeout — and since serf's `join` is dispatch-only it would
/// return `Ok` while no reliable exchange ever completes — so `TcpTransport::new`
/// rejects it with `InvalidOption` at the top of `new`, before binding any socket.
#[compio::test]
async fn new_rejects_zero_dial_timeout() {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("zero-dial-node"))
    .with_advertise_addr(MaybeResolved::Resolved(bind))
    .with_stream(StreamTransportOptions::new().with_dial_timeout(Duration::ZERO));
  let res =
    TcpTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  match res {
    Err(SerfError::InvalidOption(_)) => {}
    Err(other) => panic!("expected InvalidOption, got {other:?}"),
    Ok(_) => panic!("a zero dial_timeout must be rejected at construction"),
  }
}

/// Construct a TCP transport bound to an ephemeral loopback port (the per-node
/// construction path that draws the serf-core RNG).
async fn build_node(id: &str) -> TcpTransport<SmolStr, SocketAddr> {
  let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr");
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new(id))
    .with_advertise_addr(MaybeResolved::Resolved(bind));
  TcpTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver)
    .await
    .expect("construct transport")
}

/// A construction failure AFTER the sockets are bound must close them (awaited)
/// before returning `Err`, or the bound port leaks and a same-address rebind
/// races into `AddrInUse` (a plain drop is not a synchronous fd release on
/// compio/Windows-IOCP). A wildcard `0.0.0.0:0` advertise binds a concrete
/// OS-assigned port (free for both the TCP listener and the UDP socket) but is
/// then rejected by `validate_advertise_addr` for its unspecified IP; the exact
/// freed `0.0.0.0:<port>` must immediately re-accept the SAME listener + UDP
/// socket the transport bound, proving neither leaked on the error path.
#[compio::test]
async fn new_failure_closes_bound_sockets_for_rebind() {
  let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
    .with_local_id(SmolStr::new("rebind-fail"))
    .with_advertise_addr(MaybeResolved::Resolved(
      "0.0.0.0:0".parse().expect("wildcard addr"),
    ));
  let res =
    TcpTransport::<SmolStr, SocketAddr>::new(opts, &SocketAddrResolver, &FirstAddrResolver).await;
  let freed = match res {
    Err(SerfError::InvalidAdvertiseAddr(e)) => e.addr(),
    Err(other) => panic!("expected a post-bind InvalidAdvertiseAddr failure, got {other:?}"),
    Ok(_) => panic!("a post-bind failure must reject construction, but it succeeded"),
  };

  let listener = compio::net::TcpListener::bind(freed)
    .await
    .expect("the freed TCP port must rebind, not AddrInUse");
  let gossip = compio::net::UdpSocket::bind(freed)
    .await
    .expect("the freed UDP port must rebind, not AddrInUse");
  // Ignoring Err: test cleanup of the probe sockets.
  let _ = listener.close().await;
  let _ = gossip.close().await;
}

/// Two freshly-constructed nodes must seed their serf-core RNGs from independent
/// OS entropy, not a shared/zero seed: a shared seed makes two nodes with the
/// same op history emit identical `(ltime, id)` for concurrent queries, which
/// the dedup ring drops. Drawing several words from each and comparing the
/// streams asserts the seeds diverge (the field is reachable from this child
/// module; `Transport::run` consumes it into the endpoint via `new_with_rng`).
#[compio::test]
async fn freshly_constructed_nodes_have_independent_serf_rngs() {
  let mut a = build_node("rng-a").await;
  let mut b = build_node("rng-b").await;

  let sample_a: [u64; 4] = core::array::from_fn(|_| a.serf_rng.next_u64());
  let sample_b: [u64; 4] = core::array::from_fn(|_| b.serf_rng.next_u64());

  assert_ne!(
    sample_a, sample_b,
    "two fresh nodes must hold independently OS-seeded serf RNGs, not a shared stream"
  );
}
