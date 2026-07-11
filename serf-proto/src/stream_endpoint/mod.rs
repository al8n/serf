//! The serf `StreamEndpoint` super-machine — serf logic composed with the
//! memberlist reliable stream coordinator.
//!
//! `StreamEndpoint` owns the serf-logic [`Endpoint`] core and the memberlist
//! reliable coordinator ([`memberlist_proto::streams::StreamEndpoint`]) as
//! **two disjoint fields**, and drives the core over `&mut transport` through
//! the `Reliable` seam.  It exposes the
//! coordinator's transport-facing driver surface (`handle_packet`,
//! `handle_gossip`, `accept_connection`, `handle_transport_data`,
//! `poll_action`, `poll_transport_transmit`, `handle_timeout`, …) plus serf's
//! own commands and events (`poll_event`, `join`, `leave`, `user_event`,
//! `query`, key ops, …), forwarding each to the right field.
//!
//! The composed `handle_timeout` is where the load-bearing tick ordering lives:
//! the coordinator's SWIM timer fires between serf's pre-tick snapshot resync
//! and serf's post-tick drain + deadline pass, so no per-runtime driver has to
//! re-establish the order.
//!
//! The coordinator owns the reliable stream lifecycle internally: it dials
//! peers, runs the label / record-layer handshake, and exchanges the membership
//! state blob in both directions, surfacing only its transport I/O intents
//! (`poll_action` → `Connect`, `poll_transport_transmit`) to the driver.  Serf
//! observes the merged outcome as [`memberlist_proto::RemoteStateReceived`] on
//! the core's drain over `poll_inner_event`, and never reaches into the stream
//! lifecycle itself.

use std::{sync::Arc, vec::Vec};

use bytes::Bytes;
use memberlist_proto::{
  CheapClone, Data, Id, Instant, PushPullKind, Rng, SeedableRng, SmallRng, Transmit,
  event::StreamId,
  parse_message,
  streams::{ExchangeId, StreamAction, StreamEndpoint as Coordinator, StreamTransport},
  typed::Message,
};
use smol_str::SmolStr;

use crate::{
  DropCounter,
  endpoint::{Endpoint, Error, QueryId, QueryParams},
  event::{Event, QueryEvent},
  members::{Member, SerfState},
  options::Options,
};

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

/// The serf `StreamEndpoint` super-machine.
///
/// Composes the serf-logic [`Endpoint`] `core` with the memberlist reliable
/// stream coordinator ([`memberlist_proto::streams::StreamEndpoint`]) as the
/// `transport`.  The driver pumps **one** machine: it feeds the transport
/// ingress, ticks `handle_timeout`, and drains the serf and transport poll
/// surfaces.
///
/// The serf-logic core carries its **own** injected RNG `R`, distinct from the
/// coordinator's RNG `G`; the two are seeded independently so serf's gossip
/// choices and memberlist's probe choices do not share a stream.  `RT` is the
/// record-layer ([`StreamTransport`]) — `RawRecords` for plain-TCP,
/// `Labeled<TlsRecords>` for TLS.
#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub struct StreamEndpoint<I, A, RT, G = SmallRng, R = SmallRng, D = u64>
where
  I: Eq + core::hash::Hash,
  RT: StreamTransport,
  D: DropCounter,
{
  /// The serf-logic core, holding all serf state and no transport reference.
  core: Endpoint<I, A, R, D>,
  /// The memberlist reliable coordinator serf drives through the `Reliable`
  /// seam.  Holds the single membership `Endpoint`.
  transport: Coordinator<I, A, RT, G>,
}

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
impl<I, A, RT, G, R, D> StreamEndpoint<I, A, RT, G, R, D>
where
  I: Clone + Eq + core::hash::Hash,
  RT: StreamTransport,
  R: SeedableRng,
  D: DropCounter,
{
  /// Construct a `StreamEndpoint` from a memberlist reliable coordinator
  /// `transport`, serf `opts`, and serf's own injected `rng`.
  ///
  /// `rng` is **separate** from the coordinator's RNG `G`; seed it from the
  /// driver's own entropy source.
  pub fn new_with_rng(transport: Coordinator<I, A, RT, G>, opts: Options, rng: R) -> Self
  where
    D: Default,
  {
    Self {
      core: Endpoint::new_with_rng(opts, rng),
      transport,
    }
  }

  /// Construct a `StreamEndpoint` injecting the two coalescer shed counters, for
  /// a driver that shares them with a detached handle.
  ///
  /// Forwards `user_drop` / `member_drop` into
  /// [`Endpoint::new_with_rng_in`](crate::endpoint::Endpoint::new_with_rng_in).
  pub fn new_with_rng_in(
    transport: Coordinator<I, A, RT, G>,
    opts: Options,
    rng: R,
    user_drop: D,
    member_drop: D,
  ) -> Self {
    Self {
      core: Endpoint::new_with_rng_in(opts, rng, user_drop, member_drop),
      transport,
    }
  }

  /// Convenience constructor that seeds serf's `R` with a zero seed.
  ///
  /// Suitable for tests and deterministic environments.  Production drivers
  /// should use `new_with_rng` and seed from a cryptographically-secure source.
  pub fn new(transport: Coordinator<I, A, RT, G>, opts: Options) -> Self
  where
    D: Default,
  {
    Self::new_with_rng(transport, opts, R::seed_from_u64(0))
  }
}

// ── transport-level driver surface ────────────────────────────────────────────
//
// These reach the coordinator (`transport`) directly — the `Reliable` seam
// deliberately excludes transport ingress / timer / poll operations — then drive
// the serf-logic sieve over the coordinator.

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
impl<I, A, RT, G, R, D> StreamEndpoint<I, A, RT, G, R, D>
where
  I: Id + Clone,
  A: CheapClone + Data + PartialEq + Clone + 'static,
  RT: StreamTransport,
  G: Rng,
  R: Rng + SeedableRng,
  D: DropCounter,
{
  /// Feed one decoded unreliable memberlist `Message<I, A>` into the
  /// coordinator, then sieve the resulting inner events into serf.
  ///
  /// The composed unit's unreliable ingress is `handle_gossip` →
  /// `poll_memberlist_ingress` → (codec decode) → `handle_packet`.  This method
  /// is the decode-then-feed convenience: it parses the memberlist wire frame
  /// and hands the typed message to the coordinator.  Malformed or unrecognised
  /// bytes are silently dropped — the machine must not panic on bad input from
  /// the network.
  pub fn handle_packet(&mut self, from: A, data: Bytes, now: Instant) {
    // Malformed frame or unrecognised tag: drop silently. The coordinator logs
    // its own decode errors; serf takes no serf-level action here.
    if let Ok(msg) = parse_message::<I, A>(data) {
      self.transport.handle_packet(from, msg, now);
    }
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Buffer one inbound gossip datagram on the coordinator's ingress queue.
  ///
  /// The codec-owning driver drains the raw frames via
  /// [`Self::poll_memberlist_ingress`], decodes each, and feeds the typed
  /// messages back through [`Self::handle_packet`] before ticking
  /// [`Self::handle_timeout`].
  pub fn handle_gossip(&mut self, from: A, datagram: &[u8], now: Instant) {
    self.transport.handle_gossip(from, datagram, now);
  }

  /// Admit an inbound reliable stream connection from `from`.
  ///
  /// Returns the [`ExchangeId`] the coordinator allocated for the accepted
  /// exchange, or `None` if the connection was rejected (e.g. the inbound
  /// stream cap is exceeded or the node is leaving).  The driver feeds the
  /// connection's bytes back through [`Self::handle_transport_data`] under this
  /// id.
  pub fn accept_connection(&mut self, from: A, now: Instant) -> Option<ExchangeId> {
    self.transport.accept_connection(from, now)
  }

  /// Deliver inbound transport bytes for the reliable exchange `id`, then sieve
  /// the resulting inner events into serf.
  ///
  /// `eof` signals the peer half-closed the connection (a transport read of
  /// zero).  A completed push-pull exchange surfaces as
  /// [`memberlist_proto::RemoteStateReceived`] on the core's drain, which serf
  /// folds into its membership via `merge_remote_state`.
  pub fn handle_transport_data(&mut self, id: ExchangeId, bytes: &[u8], eof: bool, now: Instant) {
    self.transport.handle_transport_data(id, bytes, eof, now);
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Report that the driver's outbound dial for reliable exchange `id` failed,
  /// then sieve the resulting inner events into serf.
  ///
  /// The coordinator retires the exchange (no bridge is opened) and may emit a
  /// terminal `ExchangeCompleted`; serf takes no action on it.
  pub fn handle_dial_failed(&mut self, id: ExchangeId, now: Instant) {
    self.transport.handle_dial_failed(id, now);
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Report a transport-level error on reliable exchange `id`, then sieve the
  /// resulting inner events into serf.
  pub fn handle_transport_error(&mut self, id: ExchangeId, now: Instant) {
    self.transport.handle_transport_error(id, now);
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
  /// The coordinator's own `handle_timeout` services its dial queue and bridge
  /// schedule internally; any `DialRequested` the inner endpoint emits is
  /// sieved into the coordinator's private dial queue (surfaced to the driver as
  /// a `poll_action` → `Connect`), so it never reaches serf's drain.
  pub fn handle_timeout(&mut self, now: Instant) {
    // Pre-inner-timer: latch `now` and resync the push-pull snapshot if dirty,
    // so the coordinator ships current serf state on this tick's anti-entropy.
    self.core.before_inner_timeout(&mut self.transport, now);
    // Inner timer: the coordinator's SWIM gossip / probe / push-pull scheduler.
    self.transport.handle_timeout(now);
    // Post-inner-timer: sieve the inner events this tick produced, then fire
    // serf's own deadlines (reap / reconnect / queue-check / query-close / …).
    self.core.after_inner_timeout(&mut self.transport, now);
  }

  /// Drain one outbound transport directive ([`StreamAction`]) from the
  /// coordinator — a `Connect` to dial a peer, or a `Shutdown` / `Close` /
  /// `Abort` to tear an exchange's connection down.
  pub fn poll_action(&mut self) -> Option<StreamAction> {
    self.transport.poll_action()
  }

  /// Drain one outbound per-exchange transport chunk `(exchange, peer, bytes)`
  /// from the coordinator; the driver writes `bytes` on `exchange`'s connection.
  pub fn poll_transport_transmit(&mut self) -> Option<(ExchangeId, core::net::SocketAddr, Bytes)> {
    self.transport.poll_transport_transmit()
  }

  /// Drain one raw inbound gossip datagram `(from, bytes)` the coordinator
  /// buffered from [`Self::handle_gossip`]; the codec layer decodes it and feeds
  /// the typed messages back through [`Self::handle_packet`].
  pub fn poll_memberlist_ingress(&mut self) -> Option<(A, Bytes)> {
    self.transport.poll_memberlist_ingress()
  }

  /// Drain one outgoing unreliable (gossip-plane) memberlist [`Transmit`] from
  /// the coordinator; the driver encodes and sends it on the UDP socket.
  pub fn poll_memberlist_transmit(&mut self) -> Option<Transmit<I, A>> {
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

  // ── driver-owned transport surface (additive forwarders) ────────────────────
  //
  // These reach the memberlist coordinator's already-public driver methods. The
  // serf `Reliable` seam deliberately excludes them (scheduling / outbound-dial /
  // wire-sizing / membership read), so the per-runtime driver forwards through
  // here rather than naming the coordinator directly.

  /// Arm the coordinator's periodic probe / gossip / push-pull schedulers.
  ///
  /// The driver calls this once at loop entry; without it the coordinator's
  /// `next_probe` / `next_gossip` / `next_pushpull` stay unset and failure
  /// detection, dissemination, and anti-entropy never run.
  ///
  /// Also folds the inner events the coordinator queued during construction —
  /// the local self-join in particular — into serf state under the driver's
  /// live `now`. Deferring that drain to a later un-latched `poll_event` would
  /// process the self-join at the machine's origin instant, so a coalescing
  /// window it opens would be armed already-overdue and flush immediately
  /// instead of batching the startup membership changes.
  pub fn start_scheduling(&mut self, now: Instant) {
    self.transport.start_scheduling(now);
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// Initiate an outbound push-pull dial to `peer`, then sieve the resulting
  /// inner events into serf.
  ///
  /// The driver owns the inner-memberlist join: serf's [`Self::join`] only
  /// announces the local join intent, while contacting each seed is a
  /// driver-issued push-pull through the coordinator.  Returns the
  /// coordinator's [`StreamId`] for the dial; the queued `Connect` surfaces on
  /// the next [`Self::poll_action`].
  pub fn start_push_pull(&mut self, peer: A, kind: PushPullKind, now: Instant) -> StreamId {
    let id = self.transport.start_push_pull(peer, kind, now);
    self.core.drain_after_ingress(&mut self.transport, now);
    id
  }

  /// Initiate an outbound **join** push-pull dial to `peer`, returning the
  /// exchange's [`StreamId`].
  ///
  /// Like [`Self::start_push_pull`] with [`PushPullKind::Join`], but when
  /// `ignore_old` is set it records the returned `StreamId` as a per-EXCHANGE
  /// ignore-join target on the serf core, so the resulting join merge (whose
  /// `originating_stream_id` equals this `StreamId`) suppresses replay of the
  /// peer's pre-join user events (H8/G4). The driver uses this for the seed joins
  /// of an `ignore_old` join, and must hand the returned `StreamId` to
  /// [`Self::clear_ignore_join_stream`] if the join terminates without merging.
  /// Reconnect-driven joins go through the coordinator directly and never ignore
  /// old events.
  pub fn start_join_push_pull(&mut self, peer: A, ignore_old: bool, now: Instant) -> StreamId {
    let id = self.start_push_pull(peer, PushPullKind::Join, now);
    if ignore_old {
      self.core.note_ignore_join_stream(id);
    }
    id
  }

  /// Remove a terminated `ignore_old` join's exchange `id` from the serf core's
  /// ignore set.
  ///
  /// The driver calls this when an `ignore_old` join reaches its terminal without
  /// a merge having consumed the entry (dial failure, timeout, empty push/pull
  /// body, or a dropped join future). Idempotent: a `StreamId` the success-path
  /// merge already consumed is simply absent. Required because
  /// [`memberlist_proto::event::ExchangeCompleted`]'s `eid` is a different domain
  /// from the `StreamId` on the stream backend, so the machine cannot self-clean
  /// the no-merge case — the driver owns the per-join terminal bookkeeping.
  pub fn clear_ignore_join_stream(&mut self, id: StreamId) {
    self.core.clear_ignore_join_stream(id);
  }

  /// Feed one already-decoded gossip [`Message`] into the coordinator, then
  /// sieve the resulting inner events into serf.
  ///
  /// The compound-aware counterpart to [`Self::handle_packet`]: a codec-owning
  /// driver that has split a compound datagram into individual messages feeds
  /// each typed message here, skipping the per-call single-message
  /// `parse_message` that `handle_packet` performs.
  pub fn handle_message(&mut self, from: A, msg: Message<I, A>, now: Instant) {
    self.transport.handle_packet(from, msg, now);
    self.core.drain_after_ingress(&mut self.transport, now);
  }

  /// The coordinator's configured gossip MTU — the driver sizes its UDP recv
  /// buffer and bounds inbound transform stripping by this value.
  pub fn gossip_mtu(&self) -> usize {
    self.transport.gossip_mtu()
  }

  /// Encrypt one outbound gossip datagram for the wire, applying the
  /// coordinator's configured encryption keyring.
  ///
  /// Forwards to [`memberlist_proto::streams::StreamEndpoint::encrypt_gossip`].
  /// The codec-owning driver calls this on the label-framed gossip bytes before
  /// handing them to the UDP socket; when no keyring is configured the bytes are
  /// returned unchanged. Returns `Err` when encryption is configured but the
  /// backend rejects the request — the driver MUST drop the datagram rather than
  /// emit plaintext on an encrypted-cluster path.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn encrypt_gossip(
    &self,
    datagram: &[u8],
  ) -> Result<Vec<u8>, memberlist_proto::EncryptionError> {
    self.transport.encrypt_gossip(datagram)
  }

  /// Decrypt and unwrap one inbound gossip datagram, reversing the wire
  /// transform stack the peer applied before decoding.
  ///
  /// Forwards to [`memberlist_proto::streams::StreamEndpoint::decrypt_gossip`].
  /// The codec-owning driver calls this on the raw bytes from
  /// [`Self::poll_memberlist_ingress`] before stripping the cluster label; a
  /// datagram with no encryption wrapper is returned unchanged when no keyring
  /// is configured, and a frame the keyring cannot decrypt is an `Err` (the
  /// driver drops it, gossip being lossy and self-healing).
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn decrypt_gossip(&self, datagram: &[u8]) -> Result<Vec<u8>, memberlist_proto::FrameError> {
    self.transport.decrypt_gossip(datagram)
  }

  /// The coordinator's maximum reliable-stream frame size — the driver uses it
  /// to bound the observation byte-backstop budget.
  pub fn max_stream_frame_size(&self) -> usize {
    self.transport.max_stream_frame_size()
  }

  /// The local node's id (the coordinator's membership-endpoint local id).
  pub fn local_id(&self) -> &I {
    self.transport.endpoint_ref().local_id_ref()
  }

  /// A snapshot of every serf member currently tracked (alive, leaving, left,
  /// or failed within the reap window), for the observable membership view a
  /// driver publishes after each membership change.
  pub fn members_snapshot(&self) -> Vec<Arc<Member<I, A>>>
  where
    I: Clone,
    A: Clone,
  {
    self.core.members_snapshot()
  }

  // ── serf-logic + serf-command forwarders ────────────────────────────────────
  // Each forwards to the matching `Endpoint` method, threading `&mut transport`
  // through the ones that reach the coordinator (the `Reliable` methods).

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
    A: Clone + Data,
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
    A: Clone + Data,
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
    A: Clone + Data,
  {
    self.core.list_keys(&mut self.transport, now)
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

  /// Forwards to [`Endpoint::coalesced_user_events_dropped`].
  pub fn coalesced_user_events_dropped(&self) -> u64 {
    self.core.coalesced_user_events_dropped()
  }

  /// Forwards to [`Endpoint::coalesced_member_events_dropped`].
  pub fn coalesced_member_events_dropped(&self) -> u64 {
    self.core.coalesced_member_events_dropped()
  }

  /// Forwards to [`Endpoint::pending_events_len`].
  pub fn pending_events_len(&self) -> usize {
    self.core.pending_events_len()
  }

  /// Forwards to [`Endpoint::poll_event`].
  pub fn poll_event(&mut self) -> Option<Event<I, A>> {
    self.core.poll_event(&mut self.transport)
  }

  /// Forwards to [`Endpoint::resync_local_state`].
  pub fn resync_local_state(&mut self)
  where
    I: Clone + Data,
    A: Data,
  {
    self.core.resync_local_state(&mut self.transport)
  }

  /// Update the local node's tags, re-advertise them via the coordinator, and
  /// synchronously refresh the local member in the membership store.
  ///
  /// # Errors
  ///
  /// Returns [`Error::SetTagsMeta`] if the encoded tags exceed the metadata cap.
  pub fn set_tags(&mut self, tags: Tags, now: Instant) -> Result<(), Error>
  where
    I: Clone,
    A: Clone,
  {
    self.core.set_tags(&mut self.transport, tags, now)
  }

  /// Forwards to [`Endpoint::handle_node_join_intent`].
  #[cfg(test)]
  pub(crate) fn handle_node_join_intent(&mut self, ltime: LamportTime, id: &I, now: Instant) -> bool
  where
    I: Clone,
  {
    self.core.handle_node_join_intent(ltime, id, now)
  }

  /// Forwards to [`Endpoint::handle_node_leave_intent`].
  #[cfg(test)]
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

  /// Forwards to [`Endpoint::leave`].
  pub fn leave(&mut self, now: Instant) -> Result<(), Error>
  where
    I: Clone,
    A: Clone,
  {
    self.core.leave(&mut self.transport, now)
  }

  /// Forwards to [`Endpoint::force_leave`].
  pub fn force_leave(&mut self, id: I, prune: bool, now: Instant) -> Result<(), Error>
  where
    I: Clone,
    A: Clone,
  {
    self.core.force_leave(&mut self.transport, id, prune, now)
  }

  /// Forwards to [`Endpoint::user_event`].
  pub fn user_event(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    coalesce: bool,
    now: Instant,
  ) -> Result<(), Error> {
    self
      .core
      .user_event(&mut self.transport, name, payload, coalesce, now)
  }

  /// Forwards to [`Endpoint::handle_user_event`].
  #[cfg(test)]
  pub(crate) fn handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    self.core.handle_user_event(msg)
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
    A: Clone + Data,
  {
    self
      .core
      .query(&mut self.transport, name, payload, params, now)
  }

  /// Forwards to [`Endpoint::respond`].
  pub fn respond(
    &mut self,
    token: &QueryEvent<I, A>,
    payload: Bytes,
    now: Instant,
  ) -> Result<(), Error>
  where
    I: Clone + Data,
    A: Clone + Data,
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
    req: &crate::event::KeyRequest<I, A>,
    resp: KeyResponseArgs,
    now: Instant,
  ) -> Result<(), Error>
  where
    I: Clone + Data,
    A: Clone + Data,
  {
    self.core.respond_key(&mut self.transport, req, resp, now)
  }

  /// The coordinator's live cross-transport [`memberlist_proto::EncryptionOptions`]
  /// — the single source of truth for the gossip and reliable-plane AEAD keyring.
  ///
  /// A driver applying a key-management op reads this, mutates the keyring, and
  /// pushes it back via [`set_encryption_options`](Self::set_encryption_options),
  /// so the reported key state and the bytes on the wire cannot diverge.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn encryption_options(&self) -> &memberlist_proto::EncryptionOptions {
    self.transport.encryption_options()
  }

  /// Replace the coordinator's live encryption options, re-keying the gossip and
  /// reliable planes in lockstep.
  ///
  /// The post-construction counterpart to the construction-time policy: applying a
  /// completed key rotation here rotates the actual AEAD both planes encrypt
  /// under, rather than leaving a driver-held shadow to drift from the wire.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn set_encryption_options(&mut self, encryption: memberlist_proto::EncryptionOptions) {
    self.transport.set_encryption_options(encryption)
  }

  /// Forwards to [`Endpoint::leave_broadcast_deadline`].
  pub const fn leave_broadcast_deadline(&self) -> Option<Instant> {
    self.core.leave_broadcast_deadline()
  }

  /// Forwards to [`Endpoint::leave_complete_deadline`].
  pub const fn leave_complete_deadline(&self) -> Option<Instant> {
    self.core.leave_complete_deadline()
  }

  /// Forwards to [`Endpoint::test_member_status`].
  #[cfg(test)]
  pub(crate) fn test_member_status(&self, id: I) -> Option<MemberStatus>
  where
    I: Clone,
  {
    self.core.test_member_status(id)
  }

  /// Forwards to [`Endpoint::test_queue_max`].
  #[cfg(test)]
  pub(crate) fn test_queue_max(&self) -> usize {
    self.core.test_queue_max()
  }

  /// Forwards to [`Endpoint::test_member_status_time`].
  #[cfg(test)]
  pub(crate) fn test_member_status_time(&self, id: I) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.core.test_member_status_time(id)
  }

  /// Forwards to [`Endpoint::test_seed_member`].
  #[cfg(test)]
  pub(crate) fn test_seed_member(&mut self, id: I, status: MemberStatus, status_time: LamportTime)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_seed_member(id, status, status_time)
  }

  /// Forwards to [`Endpoint::test_seed_member_with_tags`].
  #[cfg(all(test, feature = "tag-regex"))]
  pub(crate) fn test_seed_member_with_tags(
    &mut self,
    id: I,
    tags: Tags,
    status: MemberStatus,
    status_time: LamportTime,
  ) where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self
      .core
      .test_seed_member_with_tags(id, tags, status, status_time)
  }

  /// Forwards to [`Endpoint::test_seed_failed_member_by_status`].
  #[cfg(test)]
  pub(crate) fn test_seed_failed_member_by_status(
    &mut self,
    id: I,
    status_time: LamportTime,
    now: Instant,
  ) where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self
      .core
      .test_seed_failed_member_by_status(id, status_time, now)
  }

  /// Forwards to [`Endpoint::test_seed_left_member_by_status`].
  #[cfg(test)]
  pub(crate) fn test_seed_left_member_by_status(
    &mut self,
    id: I,
    status_time: LamportTime,
    now: Instant,
  ) where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self
      .core
      .test_seed_left_member_by_status(id, status_time, now)
  }

  /// Forwards to [`Endpoint::test_handle_join_intent`].
  #[cfg(test)]
  pub(crate) fn test_handle_join_intent(&mut self, id: I, ltime: LamportTime, now: Instant) -> bool
  where
    I: Clone,
  {
    self.core.test_handle_join_intent(id, ltime, now)
  }

  /// Forwards to [`Endpoint::test_handle_leave_intent`].
  #[cfg(test)]
  pub(crate) fn test_handle_leave_intent(&mut self, id: I, ltime: LamportTime, now: Instant) -> bool
  where
    I: Clone,
  {
    self
      .core
      .test_handle_leave_intent(&mut self.transport, id, ltime, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_joined`].
  #[cfg(test)]
  pub(crate) fn test_inner_node_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_inner_node_joined(id, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_left`].
  #[cfg(test)]
  pub(crate) fn test_inner_node_left(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_inner_node_left(id, now)
  }

  /// Forwards to [`Endpoint::test_inner_node_updated`].
  #[cfg(test)]
  pub(crate) fn test_inner_node_updated(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_inner_node_updated(id, now)
  }

  /// Forwards to [`Endpoint::test_in_failed_members`].
  #[cfg(test)]
  pub(crate) fn test_in_failed_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.core.test_in_failed_members(id)
  }

  /// Forwards to [`Endpoint::test_in_left_members`].
  #[cfg(test)]
  pub(crate) fn test_in_left_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.core.test_in_left_members(id)
  }

  /// Forwards to [`Endpoint::test_inner_left_cluster`].
  #[cfg(test)]
  pub(crate) fn test_inner_left_cluster(&mut self) {
    self.core.test_inner_left_cluster(&mut self.transport)
  }

  /// Forwards to [`Endpoint::test_seed_failed_member`].
  #[cfg(test)]
  pub(crate) fn test_seed_failed_member(&mut self, id: I, addr: A, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_seed_failed_member(id, addr, now)
  }

  /// Forwards to [`Endpoint::test_fire_reconnect`].
  #[cfg(test)]
  pub(crate) fn test_fire_reconnect(&mut self, now: Instant)
  where
    A: Clone,
  {
    self.core.test_fire_reconnect(&mut self.transport, now)
  }

  /// Forwards to [`Endpoint::test_fire_reap`].
  #[cfg(test)]
  pub(crate) fn test_fire_reap(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_fire_reap(now)
  }

  /// Forwards to [`Endpoint::test_last_dial_addr`].
  #[cfg(test)]
  pub(crate) fn test_last_dial_addr(&self) -> Option<A>
  where
    A: Clone,
  {
    self.core.test_last_dial_addr()
  }

  /// Forwards to [`Endpoint::test_handle_user_event`].
  #[cfg(test)]
  pub(crate) fn test_handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    self.core.test_handle_user_event(msg)
  }

  /// Forwards to [`Endpoint::test_set_event_min_time`].
  #[cfg(test)]
  pub(crate) fn test_set_event_min_time(&mut self, t: u64) {
    self.core.test_set_event_min_time(t)
  }

  /// Forwards to [`Endpoint::test_set_event_clock`].
  #[cfg(test)]
  pub(crate) fn test_set_event_clock(&mut self, t: u64) {
    self.core.test_set_event_clock(t)
  }

  /// Forwards to [`Endpoint::test_event_slot_len`].
  #[cfg(test)]
  pub(crate) fn test_event_slot_len(&self, ltime: u64) -> usize {
    self.core.test_event_slot_len(ltime)
  }

  /// Forwards to [`Endpoint::test_inject_user_packet`].
  #[cfg(test)]
  pub(crate) fn test_inject_user_packet(&mut self, from: A, data: Bytes, now: Instant)
  where
    I: Clone + Data,
    A: Clone + Data,
  {
    self
      .core
      .test_inject_user_packet(&mut self.transport, from, data, now)
  }

  /// Overwrite all three Lamport clocks in one call.
  ///
  /// A test-support seam gated behind the non-default `test-support` feature (NOT
  /// exposed by a production `tcp` build) that a downstream crate's tests use to
  /// drive the member clock to the `LTIME_MAX` integrity floor and exercise the
  /// refused-leave (`LeaveClockExhausted`) path without 2^63 real membership
  /// events. Forwards to [`Endpoint::test_set_clocks`].
  #[cfg(any(test, feature = "test-support"))]
  #[cfg_attr(docsrs, doc(cfg(feature = "test-support")))]
  pub fn test_set_clocks(&mut self, member: u64, event: u64, query: u64) {
    self.core.test_set_clocks(member, event, query)
  }

  /// Forwards to [`Endpoint::test_seed_left_member`].
  #[cfg(test)]
  pub(crate) fn test_seed_left_member(&mut self, id: I, status_time: LamportTime)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_seed_left_member(id, status_time)
  }

  /// Forwards to [`Endpoint::test_inner_local_state_snapshot`].
  #[cfg(test)]
  pub(crate) fn test_inner_local_state_snapshot(&self) -> Bytes {
    self.core.test_inner_local_state_snapshot(&self.transport)
  }

  /// Forwards to [`Endpoint::test_decode_pushpull`].
  #[cfg(test)]
  pub(crate) fn test_decode_pushpull(&self, bytes: &Bytes) -> crate::typed::PushPullMessage<I>
  where
    I: Clone + Data,
    A: Data,
  {
    self.core.test_decode_pushpull(bytes)
  }

  /// Forwards to [`Endpoint::test_clear_dirty`].
  #[cfg(test)]
  pub(crate) fn test_clear_dirty(&mut self) {
    self.core.test_clear_dirty()
  }

  /// Forwards to [`Endpoint::test_is_dirty`].
  #[cfg(test)]
  pub(crate) fn test_is_dirty(&self) -> bool {
    self.core.test_is_dirty()
  }

  /// Return the local node's serf-side tags from `members.states` (test
  /// adapter for `set_tags` observability assertions).
  ///
  /// Returns `None` when the local node is not yet in the serf membership store.
  #[cfg(test)]
  pub(crate) fn test_local_tags(&self) -> Option<Tags> {
    self
      .core
      .test_local_tags_in(self.transport.endpoint_ref().local_id_ref())
  }

  /// Return the local node's advertised meta from the coordinator's inner
  /// membership store (test adapter for `set_tags` round-trip assertions).
  ///
  /// Returns `None` when the local node is not tracked by the coordinator
  /// (should not happen after construction).
  #[cfg(test)]
  pub(crate) fn test_local_meta(&self) -> Option<memberlist_proto::typed::Meta> {
    let local_id = self.transport.endpoint_ref().local_id_ref();
    self
      .transport
      .endpoint_ref()
      .member(local_id)
      .map(|ns| ns.meta_ref().cheap_clone())
  }

  /// Forwards to [`Endpoint::test_note_ignore_join_stream`].
  #[cfg(test)]
  pub(crate) fn test_note_ignore_join_stream(&mut self, id: StreamId) {
    self.core.test_note_ignore_join_stream(id)
  }

  /// Whether exchange `id` is still recorded as an `ignore_old` join target.
  ///
  /// A test-support seam gated behind the non-default `test-support` feature (NOT
  /// exposed by a production `tcp` build) that a downstream crate's tests use to
  /// assert a join's ignore token survives `leave` until the exchange terminal.
  /// Forwards to [`Endpoint::test_has_ignore_join_stream`].
  #[cfg(any(test, feature = "test-support"))]
  #[cfg_attr(docsrs, doc(cfg(feature = "test-support")))]
  pub fn test_has_ignore_join_stream(&self, id: StreamId) -> bool {
    self.core.test_has_ignore_join_stream(id)
  }

  /// Forwards to [`Endpoint::test_clear_ignore_join_stream`].
  #[cfg(test)]
  pub(crate) fn test_clear_ignore_join_stream(&mut self, id: StreamId) {
    self.core.test_clear_ignore_join_stream(id)
  }

  /// Forwards to [`Endpoint::test_event_min_time`].
  #[cfg(test)]
  pub(crate) fn test_event_min_time(&self) -> u64 {
    self.core.test_event_min_time()
  }

  /// Forwards to [`Endpoint::test_merge_remote_state`].
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state(&mut self, user_data: Bytes)
  where
    I: Clone + Data,
    A: Data,
  {
    self
      .core
      .test_merge_remote_state(&mut self.transport, user_data)
  }

  /// Forwards to [`Endpoint::test_merge_remote_state_suppressed`].
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state_suppressed(&mut self, user_data: Bytes)
  where
    I: Clone + Data,
    A: Data,
  {
    self
      .core
      .test_merge_remote_state_suppressed(&mut self.transport, user_data)
  }

  /// Forwards to [`Endpoint::test_merge_remote_state_with_stream`].
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state_with_stream(
    &mut self,
    user_data: Bytes,
    is_join: bool,
    sid: StreamId,
  ) where
    I: Clone + Data,
    A: Data,
  {
    self
      .core
      .test_merge_remote_state_with_stream(&mut self.transport, user_data, is_join, sid)
  }

  /// Forwards to [`Endpoint::test_intent_ltime`].
  #[cfg(test)]
  pub(crate) fn test_intent_ltime(&self, id: I, kind: IntentKind) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.core.test_intent_ltime(id, kind)
  }

  /// Forwards to [`Endpoint::test_handle_query`].
  #[cfg(test)]
  pub(crate) fn test_handle_query(&mut self, msg: QueryMessage<I, A>) -> bool
  where
    I: Clone + Data,
    A: Clone + Data,
  {
    self.core.test_handle_query(&mut self.transport, msg)
  }

  /// Forwards to [`Endpoint::test_set_drain_now`].
  #[cfg(test)]
  pub(crate) fn test_set_drain_now(&mut self, now: Instant) {
    self.core.test_set_drain_now(now)
  }

  /// Forwards to [`Endpoint::drain_after_ingress`], latching `now` and sieving
  /// the coordinator's pending inner events — the interposed-ingress seam for a
  /// test that asserts a command's effect is not re-timed by a later drain.
  #[cfg(test)]
  pub(crate) fn test_drain_after_ingress(&mut self, now: Instant) {
    self.core.drain_after_ingress(&mut self.transport, now)
  }

  /// Forwards to [`Endpoint::test_last_query_id`].
  #[cfg(test)]
  pub(crate) fn test_last_query_id(&self) -> Option<QueryId> {
    self.core.test_last_query_id()
  }

  /// Forwards to [`Endpoint::test_pending_query_count`].
  #[cfg(test)]
  pub(crate) fn test_pending_query_count(&self) -> usize {
    self.core.test_pending_query_count()
  }

  /// Forwards to [`Endpoint::test_received_queries_len`].
  #[cfg(test)]
  pub(crate) fn test_received_queries_len(&self) -> usize {
    self.core.test_received_queries_len()
  }

  /// Forwards to [`Endpoint::test_query_min_time`].
  #[cfg(test)]
  pub(crate) fn test_query_min_time(&self) -> u64 {
    self.core.test_query_min_time()
  }

  /// Forwards to [`Endpoint::test_register_received_query`].
  #[cfg(test)]
  pub(crate) fn test_register_received_query(
    &mut self,
    query_id: QueryId,
    querier: A,
    deadline: Instant,
  ) -> QueryEvent<I, A>
  where
    I: Default + Clone,
    A: Clone,
  {
    self
      .core
      .test_register_received_query(query_id, querier, deadline)
  }

  /// Forwards to [`Endpoint::test_handle_query_response`].
  #[cfg(test)]
  pub(crate) fn test_handle_query_response(&mut self, msg: QueryResponseMessage<I, A>)
  where
    I: Clone,
    A: Clone,
  {
    self
      .core
      .test_handle_query_response(&mut self.transport, msg)
  }

  /// Forwards to [`Endpoint::test_is_responded`].
  #[cfg(test)]
  pub(crate) fn test_is_responded(&self, query_id: QueryId) -> bool {
    self.core.test_is_responded(query_id)
  }

  /// Forwards to [`Endpoint::test_recent_intents_len`].
  #[cfg(test)]
  pub(crate) fn test_recent_intents_len(&self) -> usize {
    self.core.test_recent_intents_len()
  }

  /// Forwards to [`Endpoint::test_pending_query_conflict_matching`].
  #[cfg(test)]
  pub(crate) fn test_pending_query_conflict_matching(&self, query_id: QueryId) -> Option<usize> {
    self.core.test_pending_query_conflict_matching(query_id)
  }

  /// Forwards to [`Endpoint::test_relay_response`].
  #[cfg(test)]
  pub(crate) fn test_relay_response(&mut self, querier: Node<I, A>, frame: Bytes, relay_factor: u8)
  where
    I: Clone + Data,
    A: Clone + Data,
  {
    self
      .core
      .test_relay_response(&mut self.transport, querier, frame, relay_factor)
  }

  /// Forwards to [`Endpoint::test_handle_relay`].
  #[cfg(test)]
  pub(crate) fn test_handle_relay(&mut self, relay: RelayMessage<I, A>)
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_handle_relay(&mut self.transport, relay)
  }

  /// Forwards to [`Endpoint::test_last_directed_send`].
  #[cfg(test)]
  pub(crate) fn test_last_directed_send(&self) -> Option<(A, Bytes)>
  where
    A: Clone,
  {
    self.core.test_last_directed_send()
  }

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
    A: Clone + Data,
  {
    self.core.install_key(&mut self.transport, key, now)
  }

  /// Forwards to [`Endpoint::test_register_conflict_query`].
  #[cfg(test)]
  pub(crate) fn test_register_conflict_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_register_conflict_query(deadline)
  }

  /// Forwards to [`Endpoint::test_register_key_query`].
  #[cfg(all(test, any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
  pub(crate) fn test_register_key_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_register_key_query(deadline)
  }

  /// Forwards to [`Endpoint::test_fold_conflict_response`].
  #[cfg(test)]
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
  #[cfg(test)]
  pub(crate) fn test_pending_query_response_count(&self, query_id: QueryId) -> usize {
    self.core.test_pending_query_response_count(query_id)
  }

  /// Forwards to [`Endpoint::test_query_slot_len`].
  #[cfg(test)]
  pub(crate) fn test_query_slot_len(&self, ltime: u64) -> usize {
    self.core.test_query_slot_len(ltime)
  }

  /// Forwards to [`Endpoint::test_fire_due_query_closes`].
  #[cfg(test)]
  pub(crate) fn test_fire_due_query_closes(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    self.core.test_fire_due_query_closes(now)
  }

  /// Forwards to [`Endpoint::test_inject_inner_joined`].
  #[cfg(test)]
  pub(crate) fn test_inject_inner_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    self.core.test_inject_inner_joined(id, now)
  }

  /// Forwards to [`Endpoint::test_enqueue_intent_broadcast`].
  #[cfg(test)]
  pub(crate) fn test_enqueue_intent_broadcast(&mut self, bytes: Bytes) {
    self
      .core
      .test_enqueue_intent_broadcast(&mut self.transport, bytes)
  }

  /// Forwards to [`Endpoint::test_enqueue_query_broadcast`].
  #[cfg(test)]
  pub(crate) fn test_enqueue_query_broadcast(&mut self, bytes: Bytes) {
    self
      .core
      .test_enqueue_query_broadcast(&mut self.transport, bytes)
  }

  /// Forwards to [`Endpoint::load_snapshot`].
  ///
  /// Refuses with [`Error::Shutdown`] on a machine that lost an id-conflict vote.
  pub fn load_snapshot(
    &mut self,
    replay: crate::snapshot::ReplayResult<I, A>,
    now: Instant,
  ) -> Result<(), Error>
  where
    A: Clone,
  {
    self.core.load_snapshot(&mut self.transport, replay, now)
  }

  /// Forwards to [`Endpoint::test_rejoin_dials`].
  #[cfg(test)]
  pub(crate) fn test_rejoin_dials(&self) -> Vec<A>
  where
    A: Clone,
  {
    self.core.test_rejoin_dials()
  }

  /// Forwards to [`Endpoint::test_ping_completed`].
  #[cfg(all(feature = "coordinates", test))]
  pub(crate) fn test_ping_completed(
    &mut self,
    node_id: I,
    rtt: core::time::Duration,
    payload: Bytes,
  ) {
    self
      .core
      .test_ping_completed(&mut self.transport, node_id, rtt, payload)
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

  /// Forwards to [`Endpoint::test_seed_member_at`].
  #[cfg(test)]
  pub(crate) fn test_seed_member_at(
    &mut self,
    id: I,
    addr: A,
    status: MemberStatus,
    status_time: LamportTime,
  ) where
    I: Clone,
    A: Clone,
  {
    self.core.test_seed_member_at(id, addr, status, status_time)
  }

  /// Forwards to [`Endpoint::test_relay_all_directed_sends`].
  #[cfg(test)]
  pub(crate) fn test_relay_all_directed_sends(&self) -> &[(A, Bytes)] {
    self.core.test_relay_all_directed_sends()
  }

  /// Forwards to [`Endpoint::test_last_pending_query_num_nodes`].
  #[cfg(all(test, any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
  pub(crate) fn test_last_pending_query_num_nodes(&self) -> Option<usize> {
    self.core.test_last_pending_query_num_nodes()
  }

  /// Forwards to [`Endpoint::test_peek_received_query_deadlines`].
  #[cfg(test)]
  pub(crate) fn test_peek_received_query_deadlines(&self) -> Vec<Instant> {
    self.core.test_peek_received_query_deadlines()
  }

  /// Mutable access to the serf-logic core, for tests that manipulate its
  /// private state directly.
  #[cfg(test)]
  pub(crate) fn core_mut(&mut self) -> &mut Endpoint<I, A, R, D> {
    &mut self.core
  }

  /// Mutable access to the memberlist reliable coordinator, for tests that
  /// drive a two-endpoint loopback (relay one side's transport transmits into
  /// the other's `handle_transport_data`).
  #[cfg(test)]
  pub(crate) fn transport_mut(&mut self) -> &mut Coordinator<I, A, RT, G> {
    &mut self.transport
  }
}

#[cfg(all(test, feature = "tcp"))]
mod tests;
