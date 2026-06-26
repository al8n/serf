//! Narrow serf-facing interface to a memberlist reliable coordinator.
//!
//! [`Reliable`] is the only serf-to-coordinator boundary: every call serf's
//! `Endpoint` makes into memberlist passes through one of these methods.
//! It hides the concrete coordinator type (`StreamEndpoint` vs `QuicEndpoint`)
//! so the serf-logic core wires against a generic `&mut impl Reliable<I, A>`
//! without knowing which transport is in use.
//!
//! # Method classification
//!
//! **Read-only accessors** (`local_id_ref`, `advertise_ref`,
//! `local_state_snapshot_bytes`, `user_broadcast_queue_len`) reach the inner
//! membership [`memberlist_proto::Endpoint`] directly through
//! [`Reliable::endpoint_ref`].  They are not duplicated as trait methods.
//!
//! **Mutating operations** are trait methods; each has a single, unambiguous
//! entry point whose contract is documented below.
//!
//! **Coordinator-internal surfaces** (`handle_packet`, `handle_gossip`,
//! `handle_transport_data`, `accept_connection`, `handle_timeout`,
//! `poll_timeout`, `poll_action`, `poll_transport_transmit`,
//! `poll_memberlist_transmit`) are NOT part of this trait — the coordinators own
//! the stream / transport lifecycle, and serf should not reach into it.

use bytes::Bytes;
use memberlist_proto::{Endpoint, Instant, PushPullKind, Rng, StreamId};

/// Serf-facing interface to a memberlist reliable coordinator.
///
/// Implemented by the two concrete coordinators serf composes with:
/// - [`memberlist_proto::streams::StreamEndpoint`] (plain-TCP or TLS record layer)
/// - [`memberlist_proto::QuicEndpoint`] (QUIC)
///
/// The trait is `pub(crate)` because it is a serf-internal composition
/// boundary, not part of the public API.
pub(crate) trait Reliable<I, A>
where
  I: Eq + core::hash::Hash,
{
  /// The RNG type used by the inner membership [`Endpoint`].
  type Rng: memberlist_proto::Rng;

  /// Borrow the inner membership [`Endpoint`] for read-only access.
  ///
  /// Callers use this to reach the read-only accessors that are not
  /// duplicated as trait methods: `local_id_ref`, `advertise_ref`,
  /// `local_state_snapshot_bytes`, `user_broadcast_queue_len`.
  fn endpoint_ref(&self) -> &Endpoint<I, A, Self::Rng>;

  /// Drain one pending event from the coordinator's event queue.
  ///
  /// Serf calls this in a loop (`while let Some(ev) = t.poll_inner_event()`)
  /// to intercept every memberlist event (join/leave/suspect/failed/user-data
  /// push-pull / stream lifecycle) and fold it into serf's own FSM.
  ///
  /// Maps to `poll_event` on the coordinator.
  fn poll_inner_event(&mut self) -> Option<memberlist_proto::Event<I, A>>;

  /// Broadcast `data` on the gossip plane at priority `rank` (`0` = highest).
  ///
  /// Serf uses rank 0 for leave/join messages and rank 1 for user-event and
  /// query messages so that membership churn travels faster than application
  /// traffic.
  ///
  /// # Errors
  ///
  /// Propagates [`memberlist_proto::Error`] from the inner broadcast queue
  /// (e.g. the queue is full or the coordinator is not running).
  fn queue_user_broadcast_ranked(
    &mut self,
    rank: u8,
    data: Bytes,
  ) -> Result<(), memberlist_proto::Error>;

  /// Enqueue a directed unreliable (gossip-plane) packet to `to`.
  ///
  /// Used for targeted serf messages that must reach a specific peer rather
  /// than being disseminated to the cluster (e.g. conflict-query responses,
  /// directed user-event packets).
  ///
  /// # Errors
  ///
  /// Propagates [`memberlist_proto::Error`] if the coordinator is not running
  /// or the packet would exceed the gossip MTU.
  fn send_user_packet(&mut self, to: A, data: Bytes) -> Result<(), memberlist_proto::Error>;

  /// Replace the local push-pull state snapshot.
  ///
  /// Serf serializes its own membership state (lamport clocks + serf member
  /// list) into a `Bytes` blob and stores it here so it is shipped to any
  /// peer that initiates a push-pull anti-entropy exchange.  The snapshot is
  /// re-computed whenever `local_state_dirty` is set (e.g. after a member
  /// joins, leaves, or updates tags).
  ///
  /// # Errors
  ///
  /// Returns [`memberlist_proto::Error::LocalStateExceedsFrame`] if the
  /// snapshot would not fit inside a reliable-stream frame.
  fn set_local_state_snapshot(&mut self, bytes: Bytes) -> Result<(), memberlist_proto::Error>;

  /// Initiate an outbound push-pull anti-entropy exchange with `peer`.
  ///
  /// The coordinator dials `peer`, performs a label handshake, and exchanges
  /// the membership state blob in both directions.  Serf calls this on join
  /// and on periodic anti-entropy ticks.
  ///
  /// Returns the [`StreamId`] allocated by the inner endpoint for this
  /// exchange; serf currently discards it (the exchange outcome arrives via
  /// `poll_inner_event`).
  fn start_push_pull(&mut self, peer: A, kind: PushPullKind, now: Instant) -> StreamId;

  /// Signal that the local node intends to leave the cluster gracefully.
  ///
  /// The coordinator disseminates a Leave message, transitions the inner
  /// membership state to `Left`, and cancels pending reliable exchanges that
  /// have not yet been written to the wire.  After this call, `poll_timeout`
  /// and `poll_transmit` drain remaining output; no new exchanges may be
  /// initiated.
  ///
  /// # Errors
  ///
  /// Returns [`memberlist_proto::Error`] if the coordinator is already in a
  /// terminal state.
  fn leave(&mut self, now: Instant) -> Result<(), memberlist_proto::Error>;

  /// Attach an application payload to outbound probe Ack messages.
  ///
  /// The payload is included verbatim in the `AckResponse` the coordinator
  /// sends when it is probed by another node.  Serf uses this slot to carry
  /// its per-node coordinate so peers can compute network-distance estimates
  /// without a separate round-trip.
  ///
  /// Only the `coordinates` feature drives this seam (serf piggybacks its
  /// Vivaldi coordinate on probe acks); it is compiled out otherwise.
  ///
  /// # Errors
  ///
  /// Returns [`memberlist_proto::Error::AckPayloadExceedsMtu`] if the framed
  /// Ack would not fit the gossip packet budget.
  #[cfg(feature = "coordinates")]
  fn set_ack_payload(&mut self, payload: Bytes) -> Result<(), memberlist_proto::Error>;
}

// ── impl for the raw memberlist_proto::Endpoint ───────────────────────────────
//
// The transitional `StreamEndpoint` super-machine uses the raw packet
// `memberlist_proto::Endpoint` as its reliable transport, so the serf-logic
// core can drive it through this seam before the full stream/QUIC coordinators
// are wired in as the transport.

impl<I, A, R> Reliable<I, A> for Endpoint<I, A, R>
where
  I: memberlist_proto::Id,
  A: memberlist_proto::CheapClone + memberlist_proto::Data + PartialEq + 'static,
  R: Rng,
{
  type Rng = R;

  #[inline]
  fn endpoint_ref(&self) -> &Endpoint<I, A, R> {
    self
  }

  #[inline]
  fn poll_inner_event(&mut self) -> Option<memberlist_proto::Event<I, A>> {
    // Fully-qualified call to reach the inherent method; the trait method has
    // the same name and would recurse without the explicit path.
    Endpoint::poll_event(self)
  }

  #[inline]
  fn queue_user_broadcast_ranked(
    &mut self,
    rank: u8,
    data: Bytes,
  ) -> Result<(), memberlist_proto::Error> {
    Endpoint::queue_user_broadcast_ranked(self, rank, data)
  }

  #[inline]
  fn send_user_packet(&mut self, to: A, data: Bytes) -> Result<(), memberlist_proto::Error> {
    Endpoint::send_user_packet(self, to, data)
  }

  #[inline]
  fn set_local_state_snapshot(&mut self, bytes: Bytes) -> Result<(), memberlist_proto::Error> {
    Endpoint::set_local_state_snapshot(self, bytes)
  }

  #[inline]
  fn start_push_pull(&mut self, peer: A, kind: PushPullKind, now: Instant) -> StreamId {
    Endpoint::start_push_pull(self, peer, kind, now)
  }

  #[inline]
  fn leave(&mut self, now: Instant) -> Result<(), memberlist_proto::Error> {
    Endpoint::leave(self, now)
  }

  #[cfg(feature = "coordinates")]
  #[inline]
  fn set_ack_payload(&mut self, payload: Bytes) -> Result<(), memberlist_proto::Error> {
    Endpoint::set_ack_payload(self, payload)
  }
}

// ── impl for memberlist_proto::streams::StreamEndpoint ────────────────────────
//
// Enabled when the `tcp` or `tls` feature is active (which enables
// `memberlist-proto/tcp` or `memberlist-proto/tls`; both expose
// `streams::StreamEndpoint`).

#[cfg(any(feature = "tcp", feature = "tls"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "tls"))))]
impl<I, A, R, G> Reliable<I, A> for memberlist_proto::streams::StreamEndpoint<I, A, R, G>
where
  I: memberlist_proto::Id,
  A: memberlist_proto::CheapClone + memberlist_proto::Data + PartialEq + 'static,
  R: memberlist_proto::streams::StreamTransport,
  G: Rng,
{
  type Rng = G;

  #[inline]
  fn endpoint_ref(&self) -> &Endpoint<I, A, G> {
    self.endpoint_ref()
  }

  #[inline]
  fn poll_inner_event(&mut self) -> Option<memberlist_proto::Event<I, A>> {
    self.poll_event()
  }

  #[inline]
  fn queue_user_broadcast_ranked(
    &mut self,
    rank: u8,
    data: Bytes,
  ) -> Result<(), memberlist_proto::Error> {
    self.queue_user_broadcast_ranked(rank, data)
  }

  #[inline]
  fn send_user_packet(&mut self, to: A, data: Bytes) -> Result<(), memberlist_proto::Error> {
    self.send_user_packet(to, data)
  }

  #[inline]
  fn set_local_state_snapshot(&mut self, bytes: Bytes) -> Result<(), memberlist_proto::Error> {
    self.set_local_state_snapshot(bytes)
  }

  #[inline]
  fn start_push_pull(&mut self, peer: A, kind: PushPullKind, now: Instant) -> StreamId {
    self.start_push_pull(peer, kind, now)
  }

  #[inline]
  fn leave(&mut self, now: Instant) -> Result<(), memberlist_proto::Error> {
    self.leave(now)
  }

  #[cfg(feature = "coordinates")]
  #[inline]
  fn set_ack_payload(&mut self, payload: Bytes) -> Result<(), memberlist_proto::Error> {
    self.set_ack_payload(payload)
  }
}

// ── impl for memberlist_proto::QuicEndpoint ───────────────────────────────────
//
// Enabled when the `quic` feature is active (which enables
// `memberlist-proto/quic`).  The QUIC coordinator pins `A = SocketAddr`.

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
impl<I, R> Reliable<I, core::net::SocketAddr> for memberlist_proto::QuicEndpoint<I, R>
where
  I: memberlist_proto::Id,
  R: Rng,
{
  type Rng = R;

  #[inline]
  fn endpoint_ref(&self) -> &Endpoint<I, core::net::SocketAddr, R> {
    self.endpoint_ref()
  }

  #[inline]
  fn poll_inner_event(&mut self) -> Option<memberlist_proto::Event<I, core::net::SocketAddr>> {
    self.poll_event()
  }

  #[inline]
  fn queue_user_broadcast_ranked(
    &mut self,
    rank: u8,
    data: Bytes,
  ) -> Result<(), memberlist_proto::Error> {
    self.queue_user_broadcast_ranked(rank, data)
  }

  #[inline]
  fn send_user_packet(
    &mut self,
    to: core::net::SocketAddr,
    data: Bytes,
  ) -> Result<(), memberlist_proto::Error> {
    self.send_user_packet(to, data)
  }

  #[inline]
  fn set_local_state_snapshot(&mut self, bytes: Bytes) -> Result<(), memberlist_proto::Error> {
    self.set_local_state_snapshot(bytes)
  }

  #[inline]
  fn start_push_pull(
    &mut self,
    peer: core::net::SocketAddr,
    kind: PushPullKind,
    now: Instant,
  ) -> StreamId {
    self.start_push_pull(peer, kind, now)
  }

  #[inline]
  fn leave(&mut self, now: Instant) -> Result<(), memberlist_proto::Error> {
    self.leave(now)
  }

  #[cfg(feature = "coordinates")]
  #[inline]
  fn set_ack_payload(&mut self, payload: Bytes) -> Result<(), memberlist_proto::Error> {
    self.set_ack_payload(payload)
  }
}

#[cfg(test)]
mod tests;
