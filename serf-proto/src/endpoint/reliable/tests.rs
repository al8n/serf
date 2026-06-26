use super::Reliable;
use bytes::Bytes;
use memberlist_proto::{EndpointOptions, Instant, PushPullKind, SeedableRng, SmallRng};

/// Smoke-test a `Reliable` implementation through a generic function so that
/// the impls are exercised at the trait boundary, not just as concrete calls.
///
/// Exercises `endpoint_ref` (read-only accessors), `queue_user_broadcast_ranked`,
/// `set_ack_payload` (coordinates only), `set_local_state_snapshot`,
/// `poll_inner_event`, and `start_push_pull` — methods that run on a fresh,
/// un-started endpoint without requiring a live network peer.
fn drive<I, A>(t: &mut impl Reliable<I, A>, addr: A)
where
  I: Eq + core::hash::Hash + Clone,
  A: Clone,
{
  // Read-only accessor via endpoint_ref.
  let _ = t.endpoint_ref().local_state_snapshot_bytes();
  let _ = t.endpoint_ref().user_broadcast_queue_len();

  // Mutating: queue a broadcast at rank 0 (highest priority).
  let result = t.queue_user_broadcast_ranked(0, Bytes::from_static(b"hello"));
  assert!(
    result.is_ok(),
    "queue_user_broadcast_ranked failed: {result:?}"
  );

  // Mutating: set ack payload (the coordinates-only seam serf uses to
  // piggyback its Vivaldi coordinate on probe acks).
  #[cfg(feature = "coordinates")]
  {
    let result = t.set_ack_payload(Bytes::from_static(b"coord"));
    assert!(result.is_ok(), "set_ack_payload failed: {result:?}");
  }

  // Mutating: set local state snapshot.
  let result = t.set_local_state_snapshot(Bytes::from_static(b"state"));
  assert!(
    result.is_ok(),
    "set_local_state_snapshot failed: {result:?}"
  );

  // poll_inner_event drains without panicking on a fresh endpoint.
  while t.poll_inner_event().is_some() {}

  // start_push_pull enqueues an outbound exchange; returns a StreamId we discard.
  let _ = t.start_push_pull(addr, PushPullKind::Join, Instant::ORIGIN);
}

// ── raw memberlist_proto::Endpoint ───────────────────────────────────────────

#[test]
fn raw_endpoint_impl_compiles_and_wires_up() {
  let addr: core::net::SocketAddr = "127.0.0.1:7946".parse().unwrap();
  let opts = EndpointOptions::new(1u32, addr);
  let mut ep =
    memberlist_proto::Endpoint::new_at(opts, Instant::ORIGIN, SmallRng::seed_from_u64(0));
  ep.start_scheduling(Instant::ORIGIN);
  drive(&mut ep, addr);
}

// ── memberlist_proto::streams::StreamEndpoint (plain TCP) ────────────────────

#[cfg(feature = "tcp")]
#[test]
fn tcp_stream_endpoint_impl_compiles_and_wires_up() {
  use memberlist_proto::{
    RawRecords,
    streams::{LabelOptions, StreamEndpoint},
  };

  let addr: core::net::SocketAddr = "127.0.0.1:7947".parse().unwrap();
  let opts = EndpointOptions::new(1u32, addr);
  let mut inner =
    memberlist_proto::Endpoint::new_at(opts, Instant::ORIGIN, SmallRng::seed_from_u64(1));
  inner.start_scheduling(Instant::ORIGIN);

  // Plain-TCP label options: no cluster label, Passthrough inner transport.
  let cfg: LabelOptions<()> = LabelOptions::new_in(None, ());
  let sni_provider: Box<dyn Fn(&core::net::SocketAddr) -> Option<String> + Send + Sync> =
    Box::new(|_| Some("localhost".to_string()));
  let peer_to_socket: Box<dyn Fn(&core::net::SocketAddr) -> core::net::SocketAddr + Send + Sync> =
    Box::new(|a| *a);

  let mut coord: StreamEndpoint<u32, core::net::SocketAddr, RawRecords> =
    StreamEndpoint::new(inner, cfg, sni_provider, peer_to_socket);

  drive(&mut coord, addr);
}
