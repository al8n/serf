//! The serf `QuicEndpoint` super-machine — serf logic composed with the
//! memberlist QUIC coordinator.
//!
//! `QuicEndpoint` is the QUIC sibling of the stream super-machine `StreamEndpoint`:
//! it owns the same serf-logic [`Endpoint`] core but pairs it with the memberlist
//! QUIC coordinator ([`memberlist_proto::QuicEndpoint`]) instead of the reliable
//! stream coordinator.  The two live as **disjoint fields**, and the core is
//! driven over `&mut transport` through the `Reliable`
//! seam — identical to the stream super-machine; only the transport surface
//! differs.
//!
//! The QUIC coordinator pins `A = SocketAddr` (quinn dials and accepts wire
//! addresses), so this super-machine pins serf's address type to `SocketAddr`
//! as well.  Its driver surface is the QUIC coordinator's: one UDP ingress
//! ([`handle_udp`](Self::handle_udp)), one combined egress
//! ([`poll_transmit`](Self::poll_transmit)), and the memberlist gossip-plane
//! ingress / egress accessors — plus serf's own commands and events.
//!
//! The composed [`handle_timeout`](Self::handle_timeout) is where the
//! load-bearing tick ordering lives: the coordinator's SWIM timer fires between
//! serf's pre-tick snapshot resync and serf's post-tick drain + deadline pass,
//! so no per-runtime driver has to re-establish the order.
//!
//! The coordinator owns the QUIC reliable lifecycle internally: it dials peers,
//! drives the quinn handshake, opens per-peer bidi streams, and exchanges the
//! membership state blob in both directions.  Crucially it also sieves its own
//! `DialRequested` events into a private dial queue and dials itself (it *is*
//! the driver), so serf never routes a dial — it observes only the merged
//! outcome as [`memberlist_proto::RemoteStateReceived`] on the core's drain over
//! `poll_inner_event`.

use bytes::Bytes;
use core::net::SocketAddr;
use memberlist_proto::{
  Data, Id, Instant, QuicEndpoint as Coordinator, Rng, SeedableRng, SmallRng, Transmit,
  parse_message,
};
use smol_str::SmolStr;

use crate::{
  endpoint::{Endpoint, Error, QueryId, QueryParams},
  event::{Event, QueryEvent},
  members::SerfState,
  options::Options,
};

#[cfg(all(test, feature = "tag-regex"))]
use crate::typed::Tags;
#[cfg(test)]
use crate::{
  LamportTime,
  members::{IntentKind, MemberStatus},
  typed::{QueryMessage, QueryResponseMessage, RelayMessage, UserEventMessage},
};
#[cfg(test)]
use memberlist_proto::Node;

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::event::KeyResponseArgs;

/// The serf `QuicEndpoint` super-machine.
///
/// Composes the serf-logic [`Endpoint`] `core` with the memberlist QUIC
/// coordinator ([`memberlist_proto::QuicEndpoint`]) as the `transport`.  The
/// driver pumps **one** machine: it feeds the UDP ingress, ticks
/// `handle_timeout`, and drains the serf and transport poll surfaces.
///
/// The serf-logic core carries its **own** injected RNG `R`, distinct from the
/// coordinator's RNG `G`; the two are seeded independently so serf's gossip
/// choices and memberlist's probe choices do not share a stream.  The QUIC
/// coordinator pins `A = SocketAddr`, so this super-machine pins serf's address
/// type to `SocketAddr` too.
#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
pub struct QuicEndpoint<I, G = SmallRng, R = SmallRng>
where
  I: Eq + core::hash::Hash,
{
  /// The serf-logic core, holding all serf state and no transport reference.
  core: Endpoint<I, SocketAddr, R>,
  /// The memberlist QUIC coordinator serf drives through the `Reliable` seam.
  /// Holds the single membership `Endpoint` and the quinn endpoint.
  transport: Coordinator<I, G>,
}

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
impl<I, G, R> QuicEndpoint<I, G, R>
where
  I: Clone + Eq + core::hash::Hash,
  R: SeedableRng,
{
  /// Construct a `QuicEndpoint` from a memberlist QUIC coordinator `transport`,
  /// serf `opts`, and serf's own injected `rng`.
  ///
  /// `rng` is **separate** from the coordinator's RNG `G`; seed it from the
  /// driver's own entropy source.
  pub fn new_with_rng(transport: Coordinator<I, G>, opts: Options, rng: R) -> Self {
    Self {
      core: Endpoint::new_with_rng(opts, rng),
      transport,
    }
  }

  /// Convenience constructor that seeds serf's `R` with a zero seed.
  ///
  /// Suitable for tests and deterministic environments.  Production drivers
  /// should use `new_with_rng` and seed from a cryptographically-secure source.
  pub fn new(transport: Coordinator<I, G>, opts: Options) -> Self {
    Self::new_with_rng(transport, opts, R::seed_from_u64(0))
  }
}

// ── transport-level driver surface + serf commands ─────────────────────────────
//
// The driver-surface methods reach the coordinator (`transport`) directly — the
// `Reliable` seam deliberately excludes transport ingress / timer / poll
// operations — then drive the serf-logic sieve over the coordinator.  The serf
// commands forward to the matching `Endpoint` method, threading `&mut transport`
// through the ones that reach the coordinator (the `Reliable` methods).

#[cfg(feature = "quic")]
#[cfg_attr(docsrs, doc(cfg(feature = "quic")))]
impl<I, G, R> QuicEndpoint<I, G, R>
where
  I: Id + Clone,
  G: Rng,
  R: Rng + SeedableRng,
{
  /// Feed one inbound UDP datagram from `from` into the coordinator.
  ///
  /// The single conceptual socket carries both QUIC packets and plain-UDP
  /// gossip; the coordinator's first-byte demux routes each datagram.  A QUIC
  /// packet is fully processed in-band (handshake, stream data, datagram); a
  /// gossip frame is buffered for the codec-owning driver to drain via
  /// [`Self::poll_memberlist_ingress`], decode, and feed back through
  /// [`Self::handle_packet`].  After the datagram is handled the resulting inner
  /// events are sieved into serf.
  pub fn handle_udp(&mut self, from: SocketAddr, datagram: &[u8], now: Instant) {
    self.transport.handle_udp(from, datagram, now);
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Feed one decoded unreliable memberlist `Message<I, SocketAddr>` into the
  /// coordinator, then sieve the resulting inner events into serf.
  ///
  /// The composed unit's unreliable ingress is `handle_udp` →
  /// `poll_memberlist_ingress` → (codec decode) → `handle_packet`.  This method
  /// is the decode-then-feed convenience: it parses the memberlist wire frame
  /// and hands the typed message to the coordinator.  Malformed or unrecognised
  /// bytes are silently dropped — the machine must not panic on bad input from
  /// the network.
  pub fn handle_packet(&mut self, from: SocketAddr, data: Bytes, now: Instant) {
    // Malformed frame or unrecognised tag: drop silently. The coordinator logs
    // its own decode errors; serf takes no serf-level action here.
    if let Ok(msg) = parse_message::<I, SocketAddr>(data) {
      self.transport.handle_packet(from, msg, now);
    }
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Advance time and fire any expired serf or coordinator deadlines.
  ///
  /// The composed tick order (the structural win): the coordinator's SWIM timer
  /// fires between the serf pre-tick snapshot resync and the serf post-tick
  /// drain + deadline pass, so the load-bearing
  /// `resync → inner timer → drain → serf deadlines` sequence is established
  /// here, once, rather than in each runtime driver.
  ///
  /// The coordinator's own `handle_timeout` services its dial queue, quinn
  /// connections, and bridge schedule internally; any `DialRequested` the inner
  /// endpoint emits is sieved into the coordinator's private dial queue (the
  /// coordinator dials itself), so it never reaches serf's drain.
  pub fn handle_timeout(&mut self, now: Instant) {
    // Pre-inner-timer: latch `now` and resync the push-pull snapshot if dirty,
    // so the coordinator ships current serf state on this tick's anti-entropy.
    self.core.before_inner_timeout(&mut self.transport, now);
    // Inner timer: the coordinator's SWIM gossip / probe / push-pull scheduler
    // plus its quinn connection + bridge servicing.
    self.transport.handle_timeout(now);
    // Post-inner-timer: sieve the inner events this tick produced, then fire
    // serf's own deadlines (reap / reconnect / queue-check / query-close / …).
    self.core.after_inner_timeout(&mut self.transport, now);
  }

  /// Drain one outbound UDP datagram `(to, bytes)` from the coordinator (a quinn
  /// packet or an encoded memberlist gossip frame); the driver writes `bytes` to
  /// `to` on the UDP socket.
  pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Bytes)> {
    self.transport.poll_transmit()
  }

  /// Drain one raw inbound gossip datagram `(from, bytes)` the coordinator
  /// buffered from [`Self::handle_udp`]; the codec layer decodes it and feeds
  /// the typed messages back through [`Self::handle_packet`].
  pub fn poll_memberlist_ingress(&mut self) -> Option<(SocketAddr, Bytes)> {
    self.transport.poll_memberlist_ingress()
  }

  /// Drain one outgoing unreliable (gossip-plane) memberlist [`Transmit`] from
  /// the coordinator; the driver encodes and sends it on the UDP socket.
  pub fn poll_memberlist_transmit(&mut self) -> Option<Transmit<I, SocketAddr>> {
    self.transport.poll_memberlist_transmit()
  }

  /// The earliest deadline requiring a `handle_timeout` call.
  ///
  /// The minimum of the coordinator's own deadline and serf's periodic
  /// deadlines.  Takes `&mut self` because the coordinator folds in
  /// immediate-due dial wakes that it tracks mutably.
  pub fn poll_timeout(&mut self) -> Option<Instant> {
    let inner = self.transport.poll_timeout();
    let serf = self.core.serf_poll_timeout();
    match (inner, serf) {
      (Some(a), Some(b)) => Some(a.min(b)),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    }
  }

  /// Number of unsent items in the coordinator's user broadcast queue.
  ///
  /// The driver may poll this during a graceful leave to detect when the
  /// leave-intent broadcast has been flushed without waiting the full
  /// `broadcast_timeout`.
  pub fn user_broadcast_queue_len(&self) -> usize {
    self.transport.endpoint_ref().user_broadcast_queue_len()
  }

  // ── serf-logic + serf-command forwarders ────────────────────────────────────

  /// Announce the local node's join intent to the cluster.
  ///
  /// Forwards to [`Endpoint::join`].
  pub fn join(&mut self) -> Result<(), Error>
  where
    I: Clone,
  {
    self.core.join(&mut self.transport)
  }

  /// Issue a cluster-wide `use_key` query to promote `key` to primary.
  ///
  /// Forwards to [`Endpoint::use_key`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn use_key(
    &mut self,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    I: Clone + Data,
  {
    self.core.use_key(&mut self.transport, key, now)
  }

  /// Issue a cluster-wide `remove_key` query to remove `key` from all nodes.
  ///
  /// Forwards to [`Endpoint::remove_key`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn remove_key(
    &mut self,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    I: Clone + Data,
  {
    self.core.remove_key(&mut self.transport, key, now)
  }

  /// Issue a cluster-wide `list_keys` query to enumerate installed keys.
  ///
  /// Forwards to [`Endpoint::list_keys`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn list_keys(&mut self, now: Instant) -> Result<QueryId, Error>
  where
    I: Clone + Data,
  {
    self.core.list_keys(&mut self.transport, now)
  }

  /// Install a new encryption key into the local keyring.
  ///
  /// Forwards to [`Endpoint::install_key`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn install_key(
    &mut self,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    I: Clone + Data,
  {
    self.core.install_key(&mut self.transport, key, now)
  }

  /// Forwards to [`Endpoint::state`].
  pub const fn state(&self) -> SerfState {
    self.core.state()
  }

  /// Forwards to [`Endpoint::member_time`].
  pub const fn member_time(&self) -> u64 {
    self.core.member_time()
  }

  /// Forwards to [`Endpoint::event_time`].
  pub const fn event_time(&self) -> u64 {
    self.core.event_time()
  }

  /// Forwards to [`Endpoint::query_time`].
  pub const fn query_time(&self) -> u64 {
    self.core.query_time()
  }

  /// Forwards to [`Endpoint::num_members`].
  pub fn num_members(&self) -> usize {
    self.core.num_members()
  }

  /// Forwards to [`Endpoint::poll_event`].
  pub fn poll_event(&mut self) -> Option<Event<I, SocketAddr>> {
    self.core.poll_event(&mut self.transport)
  }

  /// Forwards to [`Endpoint::resync_local_state`].
  pub fn resync_local_state(&mut self)
  where
    I: Clone + Data,
  {
    self.core.resync_local_state(&mut self.transport)
  }

  /// Forwards to [`Endpoint::set_event_join_ignore`].
  pub fn set_event_join_ignore(&mut self, v: bool) {
    self.core.set_event_join_ignore(v)
  }

  /// Forwards to [`Endpoint::leave`].
  pub fn leave(&mut self, now: Instant) -> Result<(), Error>
  where
    I: Clone,
  {
    self.core.leave(&mut self.transport, now)
  }

  /// Forwards to [`Endpoint::force_leave`].
  pub fn force_leave(&mut self, id: I, prune: bool, now: Instant) -> Result<(), Error>
  where
    I: Clone,
  {
    self.core.force_leave(&mut self.transport, id, prune, now)
  }

  /// Forwards to [`Endpoint::user_event`].
  pub fn user_event(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    coalesce: bool,
  ) -> Result<(), Error> {
    self
      .core
      .user_event(&mut self.transport, name, payload, coalesce)
  }

  /// Forwards to [`Endpoint::query`].
  pub fn query(
    &mut self,
    name: impl Into<SmolStr>,
    payload: Bytes,
    params: QueryParams<I>,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    I: Clone + Data,
  {
    self
      .core
      .query(&mut self.transport, name, payload, params, now)
  }

  /// Forwards to [`Endpoint::respond`].
  pub fn respond(
    &mut self,
    token: &QueryEvent<I, SocketAddr>,
    payload: Bytes,
    now: Instant,
  ) -> Result<(), Error>
  where
    I: Clone + Data,
  {
    self.core.respond(&mut self.transport, token, payload, now)
  }

  /// Forwards to [`Endpoint::respond_key`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn respond_key(
    &mut self,
    req: &crate::event::KeyRequest<I, SocketAddr>,
    resp: KeyResponseArgs,
    now: Instant,
  ) -> Result<(), Error>
  where
    I: Clone + Data,
  {
    self.core.respond_key(&mut self.transport, req, resp, now)
  }

  /// Forwards to [`Endpoint::leave_broadcast_deadline`].
  pub const fn leave_broadcast_deadline(&self) -> Option<Instant> {
    self.core.leave_broadcast_deadline()
  }

  /// Forwards to [`Endpoint::leave_complete_deadline`].
  pub const fn leave_complete_deadline(&self) -> Option<Instant> {
    self.core.leave_complete_deadline()
  }

  /// Forwards to [`Endpoint::load_snapshot`].
  pub fn load_snapshot(
    &mut self,
    replay: crate::snapshot::ReplayResult<I, SocketAddr>,
    now: Instant,
  ) {
    self.core.load_snapshot(&mut self.transport, replay, now)
  }

  /// Forwards to [`Endpoint::get_coordinate`].
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub fn get_coordinate(&self) -> Option<crate::typed::Coordinate> {
    self.core.get_coordinate()
  }

  /// Forwards to [`Endpoint::cached_coordinate`].
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub fn cached_coordinate(&self, node: &I) -> Option<crate::typed::Coordinate> {
    self.core.cached_coordinate(node)
  }
}

// ── test-only forwarders ──────────────────────────────────────────────────────
//
// These mirror `StreamEndpoint`'s test surface so the migrated endpoint suite
// can drive serf logic through either super-machine.  The QUIC unit tests in
// `tests.rs` exercise a subset; the block-level `allow(dead_code)` admits the
// rest until the full suite drives `QuicEndpoint` directly.

#[cfg(all(test, feature = "quic"))]
#[allow(dead_code)]
impl<I, G, R> QuicEndpoint<I, G, R>
where
  I: Id + Clone,
  G: Rng,
  R: Rng + SeedableRng,
{
  /// Forwards to [`Endpoint::handle_node_join_intent`].
  pub(crate) fn handle_node_join_intent(&mut self, ltime: LamportTime, id: &I, now: Instant) -> bool
  where
    I: Clone,
  {
    self.core.handle_node_join_intent(ltime, id, now)
  }

  /// Forwards to [`Endpoint::handle_node_leave_intent`].
  pub(crate) fn handle_node_leave_intent(
    &mut self,
    ltime: LamportTime,
    id: &I,
    prune: bool,
    now: Instant,
  ) -> bool
  where
    I: Clone,
  {
    self
      .core
      .handle_node_leave_intent(&mut self.transport, ltime, id, prune, now)
  }

  /// Forwards to [`Endpoint::handle_user_event`].
  pub(crate) fn handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    self.core.handle_user_event(msg)
  }

  /// Forwards to [`Endpoint::test_member_status`].
  pub(crate) fn test_member_status(&self, id: I) -> Option<MemberStatus>
  where
    I: Clone,
  {
    self.core.test_member_status(id)
  }

  /// Forwards to [`Endpoint::test_member_status_time`].
  pub(crate) fn test_member_status_time(&self, id: I) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.core.test_member_status_time(id)
  }

  /// Forwards to [`Endpoint::test_seed_member`].
  pub(crate) fn test_seed_member(&mut self, id: I, status: MemberStatus, status_time: LamportTime)
  where
    I: Clone,
  {
    self.core.test_seed_member(id, status, status_time)
  }

  /// Forwards to [`Endpoint::test_seed_member_with_tags`].
  #[cfg(feature = "tag-regex")]
  pub(crate) fn test_seed_member_with_tags(
    &mut self,
    id: I,
    tags: Tags,
    status: MemberStatus,
    status_time: LamportTime,
  ) where
    I: Clone,
  {
    self
      .core
      .test_seed_member_with_tags(id, tags, status, status_time)
  }

  /// Forwards to [`Endpoint::test_seed_failed_member_by_status`].
  pub(crate) fn test_seed_failed_member_by_status(
    &mut self,
    id: I,
    status_time: LamportTime,
    now: Instant,
  ) where
    I: Clone,
  {
    self
      .core
      .test_seed_failed_member_by_status(id, status_time, now)
  }

  /// Forwards to [`Endpoint::test_seed_left_member_by_status`].
  pub(crate) fn test_seed_left_member_by_status(
    &mut self,
    id: I,
    status_time: LamportTime,
    now: Instant,
  ) where
    I: Clone,
  {
    self
      .core
      .test_seed_left_member_by_status(id, status_time, now)
  }

  /// Forwards to [`Endpoint::test_handle_join_intent`].
  pub(crate) fn test_handle_join_intent(&mut self, id: I, ltime: LamportTime, now: Instant) -> bool
  where
    I: Clone,
  {
    self.core.test_handle_join_intent(id, ltime, now)
  }

  /// Forwards to [`Endpoint::test_handle_leave_intent`].
  pub(crate) fn test_handle_leave_intent(&mut self, id: I, ltime: LamportTime, now: Instant) -> bool
  where
    I: Clone,
  {
    self
      .core
      .test_handle_leave_intent(&mut self.transport, id, ltime, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_joined`].
  pub(crate) fn test_inner_node_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
  {
    self.core.test_inner_node_joined(id, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_left`].
  pub(crate) fn test_inner_node_left(&mut self, id: I, now: Instant)
  where
    I: Clone,
  {
    self.core.test_inner_node_left(id, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_updated`].
  pub(crate) fn test_inner_node_updated(&mut self, id: I, now: Instant)
  where
    I: Clone,
  {
    self.core.test_inner_node_updated(id, now)
  }

  /// Forwards to [`Endpoint::test_in_failed_members`].
  pub(crate) fn test_in_failed_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.core.test_in_failed_members(id)
  }

  /// Forwards to [`Endpoint::test_in_left_members`].
  pub(crate) fn test_in_left_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.core.test_in_left_members(id)
  }

  /// Forwards to [`Endpoint::test_inner_left_cluster`].
  pub(crate) fn test_inner_left_cluster(&mut self) {
    self.core.test_inner_left_cluster(&mut self.transport)
  }

  /// Forwards to [`Endpoint::test_seed_failed_member`].
  pub(crate) fn test_seed_failed_member(&mut self, id: I, addr: SocketAddr, now: Instant)
  where
    I: Clone,
  {
    self.core.test_seed_failed_member(id, addr, now)
  }

  /// Forwards to [`Endpoint::test_fire_reconnect`].
  pub(crate) fn test_fire_reconnect(&mut self, now: Instant) {
    self.core.test_fire_reconnect(&mut self.transport, now)
  }

  /// Forwards to [`Endpoint::test_fire_reap`].
  pub(crate) fn test_fire_reap(&mut self, now: Instant)
  where
    I: Clone,
  {
    self.core.test_fire_reap(now)
  }

  /// Forwards to [`Endpoint::test_last_dial_addr`].
  pub(crate) fn test_last_dial_addr(&self) -> Option<SocketAddr> {
    self.core.test_last_dial_addr()
  }

  /// Forwards to [`Endpoint::test_handle_user_event`].
  pub(crate) fn test_handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    self.core.test_handle_user_event(msg)
  }

  /// Forwards to [`Endpoint::test_set_event_min_time`].
  pub(crate) fn test_set_event_min_time(&mut self, t: u64) {
    self.core.test_set_event_min_time(t)
  }

  /// Forwards to [`Endpoint::test_set_event_clock`].
  pub(crate) fn test_set_event_clock(&mut self, t: u64) {
    self.core.test_set_event_clock(t)
  }

  /// Forwards to [`Endpoint::test_event_slot_len`].
  pub(crate) fn test_event_slot_len(&self, ltime: u64) -> usize {
    self.core.test_event_slot_len(ltime)
  }

  /// Forwards to [`Endpoint::test_inject_user_packet`].
  pub(crate) fn test_inject_user_packet(&mut self, from: SocketAddr, data: Bytes, now: Instant)
  where
    I: Clone + Data,
  {
    self
      .core
      .test_inject_user_packet(&mut self.transport, from, data, now)
  }

  /// Forwards to [`Endpoint::test_set_clocks`].
  pub(crate) fn test_set_clocks(&mut self, member: u64, event: u64, query: u64) {
    self.core.test_set_clocks(member, event, query)
  }

  /// Forwards to [`Endpoint::test_seed_left_member`].
  pub(crate) fn test_seed_left_member(&mut self, id: I, status_time: LamportTime)
  where
    I: Clone,
  {
    self.core.test_seed_left_member(id, status_time)
  }

  /// Forwards to [`Endpoint::test_inner_local_state_snapshot`].
  pub(crate) fn test_inner_local_state_snapshot(&self) -> Bytes {
    self.core.test_inner_local_state_snapshot(&self.transport)
  }

  /// Forwards to [`Endpoint::test_decode_pushpull`].
  pub(crate) fn test_decode_pushpull(&self, bytes: &Bytes) -> crate::typed::PushPullMessage<I>
  where
    I: Clone + Data,
  {
    self.core.test_decode_pushpull(bytes)
  }

  /// Forwards to [`Endpoint::test_clear_dirty`].
  pub(crate) fn test_clear_dirty(&mut self) {
    self.core.test_clear_dirty()
  }

  /// Forwards to [`Endpoint::test_is_dirty`].
  pub(crate) fn test_is_dirty(&self) -> bool {
    self.core.test_is_dirty()
  }

  /// Forwards to [`Endpoint::test_set_event_join_ignore`].
  pub(crate) fn test_set_event_join_ignore(&mut self, v: bool) {
    self.core.test_set_event_join_ignore(v)
  }

  /// Forwards to [`Endpoint::test_event_min_time`].
  pub(crate) fn test_event_min_time(&self) -> u64 {
    self.core.test_event_min_time()
  }

  /// Forwards to [`Endpoint::test_merge_remote_state`].
  pub(crate) fn test_merge_remote_state(&mut self, user_data: Bytes, is_join: bool)
  where
    I: Clone + Data,
  {
    self
      .core
      .test_merge_remote_state(&mut self.transport, user_data, is_join)
  }

  /// Forwards to [`Endpoint::test_intent_ltime`].
  pub(crate) fn test_intent_ltime(&self, id: I, kind: IntentKind) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.core.test_intent_ltime(id, kind)
  }

  /// Forwards to [`Endpoint::test_handle_query`].
  pub(crate) fn test_handle_query(&mut self, msg: QueryMessage<I, SocketAddr>) -> bool
  where
    I: Clone + Data,
  {
    self.core.test_handle_query(&mut self.transport, msg)
  }

  /// Forwards to [`Endpoint::test_last_query_id`].
  pub(crate) fn test_last_query_id(&self) -> Option<QueryId> {
    self.core.test_last_query_id()
  }

  /// Forwards to [`Endpoint::test_pending_query_count`].
  pub(crate) fn test_pending_query_count(&self) -> usize {
    self.core.test_pending_query_count()
  }

  /// Forwards to [`Endpoint::test_received_queries_len`].
  pub(crate) fn test_received_queries_len(&self) -> usize {
    self.core.test_received_queries_len()
  }

  /// Forwards to [`Endpoint::test_query_min_time`].
  pub(crate) fn test_query_min_time(&self) -> u64 {
    self.core.test_query_min_time()
  }

  /// Forwards to [`Endpoint::test_register_received_query`].
  pub(crate) fn test_register_received_query(
    &mut self,
    query_id: QueryId,
    querier: SocketAddr,
    deadline: Instant,
  ) -> QueryEvent<I, SocketAddr>
  where
    I: Default + Clone,
  {
    self
      .core
      .test_register_received_query(query_id, querier, deadline)
  }

  /// Forwards to [`Endpoint::test_handle_query_response`].
  pub(crate) fn test_handle_query_response(&mut self, msg: QueryResponseMessage<I, SocketAddr>)
  where
    I: Clone,
  {
    self
      .core
      .test_handle_query_response(&mut self.transport, msg)
  }

  /// Forwards to [`Endpoint::test_is_responded`].
  pub(crate) fn test_is_responded(&self, query_id: QueryId) -> bool {
    self.core.test_is_responded(query_id)
  }

  /// Forwards to [`Endpoint::test_recent_intents_len`].
  pub(crate) fn test_recent_intents_len(&self) -> usize {
    self.core.test_recent_intents_len()
  }

  /// Forwards to [`Endpoint::test_pending_query_conflict_matching`].
  pub(crate) fn test_pending_query_conflict_matching(&self, query_id: QueryId) -> Option<usize> {
    self.core.test_pending_query_conflict_matching(query_id)
  }

  /// Forwards to [`Endpoint::test_relay_response`].
  pub(crate) fn test_relay_response(
    &mut self,
    querier: Node<I, SocketAddr>,
    frame: Bytes,
    relay_factor: u8,
  ) where
    I: Clone + Data,
  {
    self
      .core
      .test_relay_response(&mut self.transport, querier, frame, relay_factor)
  }

  /// Forwards to [`Endpoint::test_handle_relay`].
  pub(crate) fn test_handle_relay(&mut self, relay: RelayMessage<I, SocketAddr>)
  where
    I: Clone,
  {
    self.core.test_handle_relay(&mut self.transport, relay)
  }

  /// Forwards to [`Endpoint::test_last_directed_send`].
  pub(crate) fn test_last_directed_send(&self) -> Option<(SocketAddr, Bytes)> {
    self.core.test_last_directed_send()
  }

  /// Forwards to [`Endpoint::test_register_conflict_query`].
  pub(crate) fn test_register_conflict_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
  {
    self.core.test_register_conflict_query(deadline)
  }

  /// Forwards to [`Endpoint::test_register_key_query`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  pub(crate) fn test_register_key_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
  {
    self.core.test_register_key_query(deadline)
  }

  /// Forwards to [`Endpoint::test_fold_conflict_response`].
  pub(crate) fn test_fold_conflict_response(
    &mut self,
    query_id: QueryId,
    responder_id: I,
    agrees: bool,
  ) where
    I: Clone,
  {
    self
      .core
      .test_fold_conflict_response(query_id, responder_id, agrees)
  }

  /// Forwards to [`Endpoint::test_pending_query_response_count`].
  pub(crate) fn test_pending_query_response_count(&self, query_id: QueryId) -> usize {
    self.core.test_pending_query_response_count(query_id)
  }

  /// Forwards to [`Endpoint::test_query_slot_len`].
  pub(crate) fn test_query_slot_len(&self, ltime: u64) -> usize {
    self.core.test_query_slot_len(ltime)
  }

  /// Forwards to [`Endpoint::test_fire_due_query_closes`].
  pub(crate) fn test_fire_due_query_closes(&mut self, now: Instant)
  where
    I: Clone,
  {
    self.core.test_fire_due_query_closes(now)
  }

  /// Forwards to [`Endpoint::test_inject_inner_joined`].
  pub(crate) fn test_inject_inner_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
  {
    self.core.test_inject_inner_joined(id, now)
  }

  /// Forwards to [`Endpoint::test_enqueue_intent_broadcast`].
  pub(crate) fn test_enqueue_intent_broadcast(&mut self, bytes: Bytes) {
    self
      .core
      .test_enqueue_intent_broadcast(&mut self.transport, bytes)
  }

  /// Forwards to [`Endpoint::test_enqueue_query_broadcast`].
  pub(crate) fn test_enqueue_query_broadcast(&mut self, bytes: Bytes) {
    self
      .core
      .test_enqueue_query_broadcast(&mut self.transport, bytes)
  }

  /// Forwards to [`Endpoint::test_rejoin_dials`].
  pub(crate) fn test_rejoin_dials(&self) -> Vec<SocketAddr> {
    self.core.test_rejoin_dials()
  }

  /// Forwards to [`Endpoint::test_ping_completed`].
  #[cfg(feature = "coordinates")]
  pub(crate) fn test_ping_completed(
    &mut self,
    node_id: I,
    rtt: std::time::Duration,
    payload: Bytes,
  ) {
    self
      .core
      .test_ping_completed(&mut self.transport, node_id, rtt, payload)
  }

  /// Forwards to [`Endpoint::test_seed_member_at`].
  pub(crate) fn test_seed_member_at(
    &mut self,
    id: I,
    addr: SocketAddr,
    status: MemberStatus,
    status_time: LamportTime,
  ) where
    I: Clone,
  {
    self.core.test_seed_member_at(id, addr, status, status_time)
  }

  /// Forwards to [`Endpoint::test_relay_all_directed_sends`].
  pub(crate) fn test_relay_all_directed_sends(&self) -> &[(SocketAddr, Bytes)] {
    self.core.test_relay_all_directed_sends()
  }

  /// Forwards to [`Endpoint::test_last_pending_query_num_nodes`].
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  pub(crate) fn test_last_pending_query_num_nodes(&self) -> Option<usize> {
    self.core.test_last_pending_query_num_nodes()
  }

  /// Forwards to [`Endpoint::test_peek_received_query_deadlines`].
  pub(crate) fn test_peek_received_query_deadlines(&self) -> Vec<Instant> {
    self.core.test_peek_received_query_deadlines()
  }

  /// Mutable access to the serf-logic core, for tests that manipulate its
  /// private state directly.
  pub(crate) fn core_mut(&mut self) -> &mut Endpoint<I, SocketAddr, R> {
    &mut self.core
  }

  /// Mutable access to the memberlist QUIC coordinator, for tests that drive a
  /// two-endpoint loopback (relay one side's `poll_transmit` UDP datagrams into
  /// the other's `handle_udp`).
  pub(crate) fn transport_mut(&mut self) -> &mut Coordinator<I, G> {
    &mut self.transport
  }
}

#[cfg(all(test, feature = "quic"))]
mod tests;
