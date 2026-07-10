//! The transport-agnostic serf driving core: construction, accessors, the serf
//! command API, and the link-layer-independent `pump`.
//!
//! [`SerfEngine`] owns serf's super-machine ([`StreamEndpoint`]) — serf logic
//! composed over the memberlist reliable stream coordinator on the plain-TCP
//! [`RawRecords`] path — and the reused reliable-plane connection state machine
//! ([`ReliablePlane`]), the gossip scratch buffer, and the join-seed queue. It
//! performs NO socket I/O: a driver supplies the link-layer stack tick plus a
//! [`GossipIo`] and a [`StreamIo`], and [`SerfEngine::pump`] drives the machine
//! over them. A driver wraps the engine, owning the actual sockets/interface; the
//! future `serf-smoltcp` / `serf-embassy` drivers are built on it.
//!
//! This is the serf port of memberlist-embedded's `Engine`: the reliable-plane
//! glue, the [`GossipIo`] / [`StreamIo`] seams, and the transform pipeline are
//! reused from [`memberlist-embedded`](https://docs.rs/memberlist-embedded)
//! directly; only the [`SerfEngine`] here differs, driving serf's richer machine
//! (folding serf's query / user-event / key-management events into the pump on
//! top of membership) instead of the membership-only memberlist machine.

use core::{hash::Hash, net::SocketAddr, num::NonZeroU8};

// Under `no_std + alloc` the prelude does not bring `Box` / `Vec` / `VecDeque`
// into scope; import them explicitly from the aliased `std` (which is `alloc` in
// that build).
#[cfg(feature = "std")]
use std::collections::VecDeque;
#[cfg(not(feature = "std"))]
use std::{boxed::Box, collections::VecDeque, vec::Vec};

use std::sync::Arc;

use bytes::Bytes;
use hashbrown::{HashMap, HashSet};
use memberlist_proto::{
  AliveDelegate, Endpoint, EndpointOptions, Instant, LabelOptions, RawRecords, Rng, SeedableRng,
  StreamId, Transmit,
  codec::{
    DecodeOptions, EncodeOptions, decode_incoming, encode_outgoing, encode_outgoing_compound,
    parse_messages,
  },
  streams::{ExchangeId, StreamAction, StreamEndpoint as Coordinator},
  typed::NodeState,
};
use smallvec_wrapper::{MediumVec, OneOrMore};

use serf_proto::{
  ExchangeKind, ExchangeStatus, StreamEndpoint,
  endpoint::{Error as SerfError, QueryId, QueryParams},
  event::{Event, QueryEvent},
  members::{Member, SerfState},
  options::Options as SerfOptions,
  typed::Tags,
};

#[cfg(encryption)]
use memberlist_proto::Keyring;
#[cfg(encryption)]
use serf_proto::{
  SecretKey,
  event::{KeyRequest, KeyRequestOperation, KeyResponseArgs},
};

use memberlist_embedded::{
  GossipIo, InitError, Options, StreamIo, TransformOptions,
  reliable::{ConnState, Connection, ReliablePlane},
  socket_addr_is_routable, validate_runtime_config,
};

use crate::cidr::{CidrFilter, cidr_blocks};

/// The largest the encrypted wrapper can inflate a gossip datagram, or `0` when
/// no encryption backend is built in. serf's gossip plane carries only the
/// encryption wrapper (no checksum / compression), so this is the whole on-wire
/// inflation the receive scratch must accommodate.
#[cfg(encryption)]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = memberlist_proto::ENCRYPTED_WRAPPER_OVERHEAD;
#[cfg(not(encryption))]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = 0;

/// Size the inbound-gossip receive scratch from the effective gossip MTU.
///
/// The machine caps an outbound gossip datagram's PLAINTEXT at the configured
/// [`EndpointOptions`] `gossip_mtu`; the on-wire datagram can then exceed that by
/// up to `ENCRYPTED_WRAPPER_OVERHEAD` (the AEAD wrapper header, nonce, and tag)
/// when encryption is enabled. The buffer must hold the largest such datagram, so
/// it is sized to `gossip_mtu + ENCRYPTED_WRAPPER_OVERHEAD`, floored at 1500 (the
/// common Ethernet payload) so a sub-1500 MTU never under-sizes it. A driver's
/// datagram receive may POP a datagram before checking the caller's slice length,
/// so a datagram larger than this buffer is consumed and lost; sizing from the
/// same knob the machine bounds outbound gossip with means a correctly-configured
/// cluster never truncates an in-budget datagram.
fn gossip_recv_buf_size(gossip_mtu: usize) -> usize {
  (gossip_mtu + ENCRYPTED_WRAPPER_OVERHEAD).max(1500)
}

/// Cap on the application-event backlog awaiting
/// [`poll_event`](SerfEngine::poll_event).
///
/// A driver using the supported pump-then-[`poll_join`](SerfEngine::poll_join)
/// flow that never drains [`poll_event`](SerfEngine::poll_event) must not grow that
/// queue without bound. At the cap the OLDEST buffered event is dropped
/// (best-effort, freshest-wins) and counted in
/// [`events_dropped`](SerfEngine::events_dropped), so app-event delivery is lossy
/// under sustained overload while join accounting — folded BEFORE buffering — is
/// never affected. Mirrors memberlist-embassy's bounded `app_events` queue (same
/// cap) and the serf std drivers' load-shed counters.
pub const DEFAULT_EVENT_BUFFER_CAP: usize = 1024;

/// An [`AliveDelegate`] that admits a peer only when its advertised address is a
/// routable destination ([`socket_addr_is_routable`]).
///
/// The machine consults `notify_alive` inline for EVERY admitted Alive — gossip
/// and join push/pull alike — so this one filter drops a non-routable address at
/// admission on both planes. The bad address is never stored as a member and so
/// is never re-gossiped, stopping cluster-wide propagation of a member address no
/// node could ever send a useful packet to.
struct RoutableAddrFilter;

impl<I> AliveDelegate<I, SocketAddr> for RoutableAddrFilter
where
  I: memberlist_proto::Id,
{
  fn notify_alive(&self, peer: &NodeState<I, SocketAddr>) -> bool {
    socket_addr_is_routable(peer.address_ref())
  }
}

/// An [`AliveDelegate`] that admits a peer only when BOTH the built-in routable
/// filter and an inner delegate accept it.
///
/// The routable filter is load-bearing on the no_std core, so a configured CIDR
/// policy composes with it (logical AND) rather than replacing it: a peer must
/// pass routable AND the policy.
#[cfg(feature = "cidr")]
struct RoutableAnd<D>(D);

#[cfg(feature = "cidr")]
impl<I, D> AliveDelegate<I, SocketAddr> for RoutableAnd<D>
where
  I: memberlist_proto::Id,
  D: AliveDelegate<I, SocketAddr>,
{
  fn notify_alive(&self, peer: &NodeState<I, SocketAddr>) -> bool {
    socket_addr_is_routable(peer.address_ref()) && self.0.notify_alive(peer)
  }
}

/// Opaque handle for one in-flight await-result [`join`](SerfEngine::join),
/// returned by `join` and polled via [`poll_join`](SerfEngine::poll_join).
///
/// A driver keys its own per-join waiter (a smoltcp poll flag, an embassy signal)
/// on this handle. Two concurrent joins mint distinct handles, so their outcomes
/// never cross-resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JoinId(u64);

impl JoinId {
  /// The underlying monotonically-allocated sequence number.
  #[inline]
  pub const fn get(&self) -> u64 {
    self.0
  }
}

/// The address set an await-result [`join`](SerfEngine::join) reached: one entry
/// per outbound push/pull exchange that completed [`ExchangeStatus::Succeeded`].
/// Duplicate seeds contribute one entry per successful exchange.
pub type ReachedSet = OneOrMore<SocketAddr>;

/// The terminal outcome of a fully-resolved await-result join that reached no
/// seed: it dispatched a push/pull to one or more routable seeds but none
/// completed `Succeeded` before every exchange terminated.
///
/// `contacted` is always `0` for this payload — a non-zero contact count resolves
/// the join `Ok(ReachedSet)` instead. Mirrors serf's `JoinFailed` shape (a no_std
/// twin of `serf-driver`'s, so the embedded core need not pull the std-only
/// driver error surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinFailed {
  requested: usize,
  contacted: usize,
}

impl JoinFailed {
  /// Build a payload from the requested-seed count and the contacted count.
  #[inline]
  pub const fn new(requested: usize, contacted: usize) -> Self {
    Self {
      requested,
      contacted,
    }
  }

  /// The number of routable seed addresses the join dispatched a push/pull to.
  #[inline]
  pub const fn requested(&self) -> usize {
    self.requested
  }

  /// The number of seeds actually contacted before the join resolved. Always `0`
  /// for this payload — a non-zero contact count resolves the join `Ok`.
  #[inline]
  pub const fn contacted(&self) -> usize {
    self.contacted
  }
}

impl core::fmt::Display for JoinFailed {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(
      f,
      "join reached {} of {} seed(s)",
      self.contacted, self.requested
    )
  }
}

impl core::error::Error for JoinFailed {}

/// One seed queued by [`join`](SerfEngine::join), awaiting its per-tick
/// `start_join_push_pull` dispatch in the pump. Carries the owning join handle so
/// the resulting exchange folds back into the right waiter, and the join's
/// `ignore_old` flag so the dispatch records the ignore-join stream when set.
struct QueuedSeed {
  join: JoinId,
  seed: SocketAddr,
  ignore_old: bool,
}

/// The caller-reply lifecycle of an await-result join, kept SEPARATE from the
/// `ignore_old` stream cleanup so the two terminals resolve independently.
///
/// The reply is resolved once — from all-exchanges-done or from
/// [`leave`](SerfEngine::leave) — and delivered to the caller exactly once via
/// [`poll_join`](SerfEngine::poll_join); the ignore-stream cleanup lives on
/// [`PendingJoin`] and stays gated on the exchange terminal. Decoupling them lets
/// `leave` hand the caller its result while a still-in-flight `ignore_old`
/// push/pull keeps its ignore token until it actually merges — mirroring
/// serf-reactor's `PendingJoin`, whose reply and ignore-cleanup terminals are
/// likewise distinct.
enum JoinReply {
  /// Not yet resolved; `poll_join` yields `None`.
  Pending,
  /// Resolved and awaiting the caller; `poll_join` delivers it once, then flips
  /// this to `Delivered`.
  Ready(Result<ReachedSet, JoinFailed>),
  /// Consumed — either handed to the caller by
  /// [`poll_join`](SerfEngine::poll_join) or forgotten via
  /// [`cancel_join`](SerfEngine::cancel_join); further `poll_join` calls yield
  /// `None`.
  Delivered,
}

/// Core-owned state for one in-flight await-result join.
///
/// The sync analogue of serf-reactor's `PendingJoin`, adapted to the driver-poll
/// core: contact accounting is strictly per-OUTBOUND-EXCHANGE, observed via the
/// machine's `Event::ExchangeCompleted` filtered to [`ExchangeKind::PushPull`].
/// The correlation token that spans dispatch → completion is the START
/// [`StreamId`] captured at [`join`](SerfEngine::join)'s per-seed dispatch and
/// bound to the exchange's [`ExchangeId`] when its `Connect` action surfaces; the
/// completion then matches by that bound `ExchangeId`.
///
/// The caller [`reply`](Self::reply) and the `ignore_old` cleanup are DISTINCT
/// terminals: the reply resolves and is delivered once (on all-exchanges-done or
/// on `leave`), while the ignore streams are cleared only when every dispatched
/// exchange has terminated (`pending` empty). Keeping them separate is what lets a
/// `leave` abandon the join for the caller without pulling the ignore token out
/// from under a push/pull that can still merge.
struct PendingJoin {
  /// `StreamId`s this join's `start_join_push_pull` calls returned. Matched
  /// against each surfaced `Connect`'s `stream_id()` to bind the exchange, and —
  /// for an `ignore_old` join — cleared from the machine's ignore set on the
  /// exchange terminal.
  started: HashSet<StreamId>,
  /// Number of this join's seeds still queued in `pending_seeds`, undispatched.
  /// Decremented as each seed's push/pull is started in the pump; the join is
  /// "fully dispatched" once this reaches zero. `leave` forces it to zero — no
  /// seed dispatches once the node is leaving.
  unstarted: usize,
  /// Outbound exchange ids bound at `Connect` and still awaiting a terminal
  /// `ExchangeCompleted`.
  pending: HashSet<ExchangeId>,
  /// Peer addresses of the dispatched exchanges that terminated `Succeeded`.
  contacted: ReachedSet,
  /// Total routable-seed count this join dispatched — the `JoinFailed` denominator.
  requested: usize,
  /// Whether this is an `ignore_old` join (its `started` streams are recorded in
  /// the machine's ignore set and must be cleared on the exchange terminal).
  ignore_old: bool,
  /// Whether the `ignore_old` streams have already been cleared from the machine's
  /// ignore set, so the cleanup runs exactly once when the last exchange
  /// terminates.
  ignore_cleared: bool,
  /// The caller-reply lifecycle, resolved and delivered once, independent of the
  /// ignore-stream cleanup above.
  reply: JoinReply,
}

impl PendingJoin {
  /// Compute the terminal outcome from the accumulated contact set.
  fn outcome(&self) -> Result<ReachedSet, JoinFailed> {
    if self.contacted.is_empty() {
      Err(JoinFailed::new(self.requested, 0))
    } else {
      Ok(self.contacted.clone())
    }
  }

  /// Deliver the resolved outcome to the caller exactly once: transition
  /// `Ready → Delivered` and hand back the outcome, or `None` while still
  /// `Pending` (unresolved) or once already `Delivered`.
  fn take_ready_reply(&mut self) -> Option<Result<ReachedSet, JoinFailed>> {
    if !matches!(self.reply, JoinReply::Ready(_)) {
      return None;
    }
    let JoinReply::Ready(outcome) = core::mem::replace(&mut self.reply, JoinReply::Delivered)
    else {
      unreachable!("reply matched Ready immediately above");
    };
    Some(outcome)
  }

  /// Every dispatched exchange has terminated: no seed still queued (`unstarted`)
  /// and no exchange still in flight (`pending`).
  ///
  /// This is the EXCHANGE-terminal predicate. It gates the `ignore_old` /
  /// machine-ignore cleanup ([`try_resolve_join`]) — run the instant the work is
  /// done, INDEPENDENT of caller polling, so the machine's ignore set never leaks.
  /// It is NOT sufficient on its own to reap the caller-result entry: that reap
  /// additionally requires the reply to have been `Delivered` (polled or
  /// cancelled), so a resolved result is retained until the caller retrieves it.
  fn exchange_work_done(&self) -> bool {
    self.unstarted == 0 && self.pending.is_empty()
  }
}

/// Advance `pj` toward its terminals once every dispatched exchange has terminated
/// (`unstarted == 0` and `pending` empty): clear any still-recorded `ignore_old`
/// streams from the machine's ignore set (once), then resolve the caller reply if
/// it is still pending.
///
/// This is the SOLE site that clears a join's ignore streams, and it is gated on
/// the exchange terminal — never on `leave`, never lazily in `poll_join` — so a
/// still-in-flight `ignore_old` push/pull keeps its ignore token until it actually
/// merges (or its exchange is torn down), and the machine's ignore set never leaks
/// even if the driver drops the handle. Driven on the completion path (an
/// `ExchangeCompleted` emptied `pending`) and the end-of-pump sweep (a join whose
/// seeds all retired before a `Connect`, or an empty/all-non-routable seed set,
/// never accumulates any `pending`).
fn try_resolve_join<I, G, SR>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RawRecords, G, SR>,
  pj: &mut PendingJoin,
) where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  if pj.unstarted != 0 || !pj.pending.is_empty() {
    return;
  }
  if pj.ignore_old && !pj.ignore_cleared {
    for sid in &pj.started {
      endpoint.clear_ignore_join_stream(*sid);
    }
    pj.ignore_cleared = true;
  }
  if matches!(pj.reply, JoinReply::Pending) {
    pj.reply = JoinReply::Ready(pj.outcome());
  }
}

/// The transport-agnostic serf driving core.
///
/// Composes serf's super-machine with the reused pooled-stream reliable plane,
/// driving both through the [`GossipIo`] / [`StreamIo`] traits a driver supplies
/// to [`pump`](SerfEngine::pump). The engine holds NO sockets — the driver owns
/// the link-layer stack and its UDP/stream sockets — so the same core runs under
/// a caller-driven poll loop (smoltcp) or an async executor (embassy-net).
///
/// `I` is the node identifier type (e.g. `SmolStr`); the address is pinned to
/// [`core::net::SocketAddr`] and the record layer to the plain-TCP
/// [`RawRecords`]. `C` is the driver's opaque connection handle
/// ([`StreamIo::Conn`]). `G` is the memberlist gossip RNG (peer selection, timing
/// jitter) and `SR` is serf's OWN core RNG (query IDs, relay/reconnect selection);
/// the two are seeded independently, mirroring serf's `StreamEndpoint<.., G, SR>`.
/// A production driver seeds BOTH from its entropy source via
/// [`try_new_at_with_rng`](Self::try_new_at_with_rng) so that fresh nodes do not
/// share a `(ltime, id)` query-id sequence.
pub struct SerfEngine<I, C, G = memberlist_proto::SmallRng, SR = memberlist_proto::SmallRng>
where
  // Mandated by serf's `StreamEndpoint` field, which keys its membership store by
  // `I`. Every impl bounds `I: Id`, which implies these, so no impl restates them.
  I: Eq + Hash,
{
  /// serf's super-machine: serf logic over the memberlist reliable coordinator on
  /// the plain-TCP `RawRecords` path, carrying BOTH the injected gossip RNG `G`
  /// and serf's own injected core RNG `SR`.
  endpoint: StreamEndpoint<I, SocketAddr, RawRecords, G, SR>,
  /// Sizing / port configuration; retained for the reliable-plane paths.
  cfg: Options,
  /// Reused pooled connection handles and the exchange-to-handle map for the
  /// reliable plane.
  plane: ReliablePlane<C>,
  /// Heap scratch for one inbound gossip datagram, sized once at construction
  /// from the configured gossip MTU (see [`gossip_recv_buf_size`]) and reused
  /// every pump. Heap-resident so a large MTU does not blow a constrained stack
  /// and the allocation happens exactly once.
  gossip_recv: std::vec::Vec<u8>,
  /// Seeds queued by [`join`](Self::join) that have not yet been handed to the
  /// machine. Drained in the machine-pump phase of each `pump` tick: one
  /// `start_join_push_pull(seed, ignore_old, now)` per entry, which queues a
  /// `Connect` the machine services into a dial consumed later that same tick. Each
  /// entry carries its owning [`JoinId`] so the resulting exchange folds back into
  /// the right await-result waiter.
  pending_seeds: VecDeque<QueuedSeed>,
  /// In-flight await-result joins keyed by handle. Each accumulates its reached
  /// set from the terminal `ExchangeCompleted` of the push/pulls it dispatched;
  /// [`poll_join`](Self::poll_join) drains a resolved one.
  pending_joins: HashMap<JoinId, PendingJoin>,
  /// Mandatory, driver-actioned control events the pump (and [`leave`](Self::leave))
  /// drained — the NON-LOSSY delivery path. Holds exactly the events a driver must
  /// take a side effect on beyond observing them ([`is_mandatory_event`]): the
  /// conflict [`Event::Shutdown`] (the driver must STOP), an [`Event::KeyRequest`]
  /// (the driver must apply the op and `respond_key`), and an
  /// [`Event::DialRequested`] (the driver must dial and report back).
  ///
  /// UNLIKE `buffered_events` this queue is never dropped WHILE LIVE: evicting a
  /// live mandatory event would leave a conflict loser running or an answerable key
  /// op unhandled. [`poll_event`](Self::poll_event) drains it FIRST, so a burst of
  /// observations can neither evict nor postpone a mandatory action.
  ///
  /// It is still bounded, by LIVENESS rather than drop-oldest. An encrypted peer can
  /// flood distinct inbound [`Event::KeyRequest`]s (each pinning raw key material),
  /// so every pump prunes the ones past their response deadline — dead and
  /// unanswerable, losing nothing real — via
  /// [`prune_expired_control_events`](Self::prune_expired_control_events). That
  /// mirrors the `now < deadline` retain the serf endpoint applies to its own
  /// `received_queries`, and because the endpoint caps the live key queries it emits
  /// at a fixed inbound maximum, the live `KeyRequest`s held here are transitively
  /// bounded by that same cap. `Shutdown` is idempotent-terminal (deduped to one);
  /// `DialRequested` carries no deadline and does not flood.
  control_events: VecDeque<Event<I, SocketAddr>>,
  /// Machine events the pump (and [`leave`](Self::leave)) drained and folded into
  /// the pending joins, held so [`poll_event`](Self::poll_event) hands them to the
  /// driver in order — already folded, never re-folded — the LOSSY, best-effort
  /// delivery path for PASSIVE observations only (membership changes, user events,
  /// queries and their responses / acks, relay-drop notices, key-query results,
  /// exchange completions, and the `LeftCluster` notice). Mandatory driver-actioned
  /// events are routed to `control_events` instead; see
  /// [`route_drained_event`](Self::route_drained_event). Every `pump` drains the
  /// machine's event queue, folding each `ExchangeCompleted` into its join BEFORE
  /// buffering the observation, so join accounting is pump-driven and never waits on
  /// the app draining events.
  ///
  /// Bounded at [`DEFAULT_EVENT_BUFFER_CAP`]: at the cap the oldest observation is
  /// dropped and counted in `events_dropped`, so a driver that never drains
  /// [`poll_event`](Self::poll_event) cannot grow this queue without limit (which
  /// would exhaust memory on a long-running embedded node).
  buffered_events: VecDeque<Event<I, SocketAddr>>,
  /// Count of PASSIVE observation events shed from `buffered_events` because the
  /// driver never drained [`poll_event`](Self::poll_event) fast enough and the
  /// backlog hit [`DEFAULT_EVENT_BUFFER_CAP`]. Surfaced via
  /// [`events_dropped`](Self::events_dropped) so the best-effort loss is observable
  /// (mirroring the serf std drivers' load-shed counter). Mandatory control events
  /// are never dropped (they take the non-lossy `control_events` path) and so are
  /// never counted here. Join completions are folded before buffering, so a shed
  /// observation never affects join resolution.
  events_dropped: u64,
  /// Monotonic allocator for [`JoinId`]s, so two concurrent joins never collide.
  next_join_id: u64,
  /// Cluster label applied to the gossip codec on both encode and decode. When
  /// `Some`, the gossip codec stamps a label prefix onto every outbound datagram
  /// and rejects any inbound datagram whose label does not match. `None` disables
  /// labeling.
  label: Option<Bytes>,
  /// CIDR transport filter: a gossip datagram from a blocked source IP (recv) or
  /// a reliable connection from a blocked peer IP (accept/dial) is dropped before
  /// the machine sees it. `()` when the `cidr` feature is off.
  cidr_policy: CidrFilter,
}

// Construction — needing node identity and serf's core RNG being seedable (both
// the two-RNG production path and the deterministic single-RNG convenience wrap
// `StreamEndpoint::new_with_rng`, which bounds serf's `SR: SeedableRng`).
impl<I, C, G, SR> SerfEngine<I, C, G, SR>
where
  I: memberlist_proto::Id + Clone,
  SR: SeedableRng,
{
  /// Construct an engine seeding BOTH RNGs, panicking on a misconfiguration.
  ///
  /// The convenience wrapper over
  /// [`try_new_at_with_rng`](Self::try_new_at_with_rng); use it only when the
  /// configuration is a static constant known to be valid.
  ///
  /// # Panics
  ///
  /// Panics if [`try_new_at_with_rng`](Self::try_new_at_with_rng) returns an
  /// [`InitError`].
  pub fn new_at_with_rng(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, SocketAddr>,
    serf_opts: SerfOptions,
    now: Instant,
    gossip_rng: G,
    serf_rng: SR,
  ) -> Self {
    Self::try_new_at_with_rng(cfg, transform, ep_cfg, serf_opts, now, gossip_rng, serf_rng).expect(
      "SerfEngine::new_at_with_rng: invalid configuration; use try_new_at_with_rng to handle",
    )
  }

  /// Fallibly construct an engine, injecting BOTH the memberlist gossip RNG `G`
  /// and serf's own core RNG `SR`.
  ///
  /// This is the production constructor: serf's core RNG drives query-id
  /// generation and relay/reconnect selection, so a driver MUST seed it from its
  /// entropy source (getrandom on embedded, exactly as memberlist-smoltcp /
  /// memberlist-embassy seed their gossip RNG). Seeding it distinctly per node is
  /// what keeps fresh nodes from emitting identical `(ltime, id)` query-id
  /// sequences that a real cluster would drop or mis-correlate.
  ///
  /// Wires serf's super-machine over the memberlist coordinator and sizes the
  /// gossip receive scratch. No sockets are bound — the driver owns the gossip and
  /// reliable-stream sockets — and no I/O occurs here.
  ///
  /// # Parameters
  ///
  /// - `cfg`: engine port / timeout configuration.
  /// - `transform`: cross-transport gossip + reliable-plane encryption plus the
  ///   cluster label. serf's gossip plane carries no compression / checksum, so
  ///   only the encryption and label fields of `transform` take effect. A
  ///   configured encryption keyring is probed here (see Errors).
  /// - `ep_cfg`: memberlist machine identity (`id`, `advertise`, timing knobs).
  ///   The user-broadcast tier count is forced to 3 — serf ranks its intent /
  ///   event / query broadcasts on three tiers.
  /// - `serf_opts`: serf-level configuration (reap / reconnect / coalescing /
  ///   query timing).
  /// - `now`: the driver's clock reading at construction.
  /// - `gossip_rng`: the memberlist gossip RNG, already seeded by the driver.
  /// - `serf_rng`: serf's own core RNG, seeded distinctly by the driver.
  ///
  /// # Errors
  ///
  /// Returns [`InitError`] instead of panicking when the configuration is
  /// invalid: a zero/over-ceiling gossip MTU, a zero port or close timeout, a
  /// non-routable or port-mismatched advertise address, a machine-endpoint init
  /// failure, or (with an encryption backend built in) an unusable keyring.
  pub fn try_new_at_with_rng(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, SocketAddr>,
    serf_opts: SerfOptions,
    now: Instant,
    gossip_rng: G,
    serf_rng: SR,
  ) -> Result<Self, InitError> {
    // Validate every advertise-independent config field (port, gossip-MTU
    // ceiling, close timeout, and the encryption keyring) up front, sharing the
    // reused preflight so the deterministic checks live in ONE place.
    validate_runtime_config(&cfg, &transform, ep_cfg.gossip_mtu())?;

    // Capture the advertise-dependent values before `ep_cfg` is moved.
    let gossip_mtu = ep_cfg.gossip_mtu();
    let advertise = *ep_cfg.advertise_addr_ref();

    // Reject a non-routable advertise address before the endpoint exists: a node
    // must advertise an address its peers can route a reply to.
    if !socket_addr_is_routable(&advertise) {
      return Err(InitError::NonRoutableAdvertiseAddr(advertise));
    }
    // The advertised port must match the single bound port (one port serves both
    // the gossip and reliable planes; a direct embedded interface has no NAT).
    if advertise.port() != cfg.port {
      return Err(InitError::AdvertisePortMismatch);
    }

    // Size the inbound-gossip scratch from the configured gossip MTU, keeping the
    // driver's ingress in lockstep with the machine's egress bound.
    let gossip_recv = std::vec![0u8; gossip_recv_buf_size(gossip_mtu)];

    // serf ranks its user broadcasts on three tiers (intent / event / query →
    // ranks 0 / 1 / 2), so the inner memberlist endpoint needs at least three
    // broadcast tiers.
    let ep_cfg = ep_cfg.with_user_broadcast_tiers(NonZeroU8::new(3).expect("3 is nonzero"));

    // The CIDR policy gates the alive delegate (composed below) and the
    // transport-boundary recv/accept guards (stored on the engine).
    #[cfg(feature = "cidr")]
    let cidr_policy: CidrFilter = cfg.cidr_policy.clone();
    #[cfg(not(feature = "cidr"))]
    let cidr_policy: CidrFilter = ();

    // Build the inner memberlist `Endpoint` (the SWIM machine serf sits on) with
    // the injected gossip RNG. `try_new_at` maps a machine init failure to
    // `InitError::Endpoint` and starts its timers from a consistent origin.
    let mut ep = Endpoint::try_new_at(ep_cfg, now, gossip_rng).map_err(InitError::Endpoint)?;

    // Install the routable-address admission filter on the raw `Endpoint` BEFORE
    // it is wrapped: the machine consults it inline for every inbound Alive, so a
    // peer advertising a non-routable address is dropped at admission. When a CIDR
    // policy is set, the routable filter wraps it (routable AND in-policy).
    #[cfg(feature = "cidr")]
    match cidr_policy.clone() {
      Some(policy) => ep.set_alive_delegate(RoutableAnd(policy)),
      None => ep.set_alive_delegate(RoutableAddrFilter),
    }
    #[cfg(not(feature = "cidr"))]
    ep.set_alive_delegate(RoutableAddrFilter);

    // Build the reliable-plane label options from the single validated source
    // (already validated at the `TransformOptions` setter, so `new_in` is
    // infallible here). Plain TCP has no SNI (`|_| None`) and a membership
    // address that IS the transport socket (`|addr| *addr`).
    let mut label_opts = LabelOptions::new_in(transform.label().map(|b| b.to_vec()), ());
    if transform.skip_inbound_label_check() {
      label_opts = label_opts.skip_inbound_label_check();
    }
    // Retain the validated label for the gossip codec (same source, both planes
    // share one label so they cannot diverge).
    let label = transform.label().map(Bytes::copy_from_slice);

    #[allow(unused_mut)]
    let mut coord = Coordinator::new(
      ep,
      label_opts,
      Box::new(|_: &SocketAddr| -> Option<std::string::String> { None }),
      Box::new(|addr: &SocketAddr| *addr),
    );
    // Install the gossip-and-reliable encryption keyring; a no-keyring policy is
    // the identity transform, so an unencrypted node is unaffected. serf's gossip
    // plane applies only this transform (no compression / checksum on gossip).
    #[cfg(encryption)]
    coord.set_encryption_options(transform.encryption);

    // Wrap the coordinator in serf's super-machine, injecting serf's own core RNG
    // via `new_with_rng` — the fix for the zero-seeded-core-RNG footgun: serf's
    // query IDs / relay selection now draw from the driver-seeded `serf_rng`,
    // independent of the injected memberlist gossip RNG `G`.
    let endpoint =
      StreamEndpoint::<I, SocketAddr, RawRecords, G, SR>::new_with_rng(coord, serf_opts, serf_rng);

    Ok(Self {
      endpoint,
      cfg,
      plane: ReliablePlane::new(),
      gossip_recv,
      pending_seeds: VecDeque::new(),
      pending_joins: HashMap::new(),
      control_events: VecDeque::new(),
      buffered_events: VecDeque::new(),
      events_dropped: 0,
      next_join_id: 0,
      label,
      cidr_policy,
    })
  }

  /// Construct an engine with serf's core RNG ZERO-SEEDED, panicking on a
  /// misconfiguration.
  ///
  /// DETERMINISTIC-ONLY convenience: use it only for tests / reproducible
  /// fixtures. See [`try_new_at`](Self::try_new_at) for the zero-seed caveat.
  ///
  /// # Panics
  ///
  /// Panics if [`try_new_at`](Self::try_new_at) returns an [`InitError`].
  pub fn new_at(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, SocketAddr>,
    serf_opts: SerfOptions,
    now: Instant,
    gossip_rng: G,
  ) -> Self {
    Self::try_new_at(cfg, transform, ep_cfg, serf_opts, now, gossip_rng)
      .expect("SerfEngine::new_at: invalid configuration; use try_new_at to handle")
  }

  /// Fallibly construct an engine with serf's core RNG ZERO-SEEDED.
  ///
  /// DETERMINISTIC-ONLY: serf's core RNG is seeded from `0`, so every engine built
  /// this way emits the SAME `(ltime, id)` query-id sequence. That is fine for
  /// tests and reproducible fixtures but WRONG for a real cluster, where distinct
  /// nodes must not collide their query ids — production drivers construct via
  /// [`try_new_at_with_rng`](Self::try_new_at_with_rng), seeding serf's RNG from
  /// entropy. Takes only the gossip RNG; the serf RNG is `SR::seed_from_u64(0)`.
  ///
  /// # Errors
  ///
  /// As [`try_new_at_with_rng`](Self::try_new_at_with_rng).
  pub fn try_new_at(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, SocketAddr>,
    serf_opts: SerfOptions,
    now: Instant,
    gossip_rng: G,
  ) -> Result<Self, InitError> {
    Self::try_new_at_with_rng(
      cfg,
      transform,
      ep_cfg,
      serf_opts,
      now,
      gossip_rng,
      SR::seed_from_u64(0),
    )
  }
}

// Pure reliable-plane accessors — needing node identity but neither the
// connection-handle key nor either RNG.
impl<I, C, G, SR> SerfEngine<I, C, G, SR>
where
  I: memberlist_proto::Id + Clone,
{
  /// Mutable access to the reliable plane's pool, for a driver to push its
  /// pre-created connection handles and install the initial listener at
  /// construction.
  #[inline]
  pub fn plane_mut(&mut self) -> &mut ReliablePlane<C> {
    &mut self.plane
  }

  /// Install the initial passive-open listener handle, set by the driver after it
  /// has `listen`ed on that connection slot at construction.
  #[inline]
  pub fn set_listener(&mut self, c: C) {
    self.plane.listener = Some(c);
  }

  /// The configured local port (gossip + reliable listener both bind it).
  #[inline]
  pub fn port(&self) -> u16 {
    self.cfg.port
  }

  /// Number of inbound reliable connections accepted on the listener since
  /// construction.
  #[inline]
  pub fn accepted_inbound_count(&self) -> u64 {
    self.plane.accepted_inbound
  }

  /// Number of pooled connection slots currently free.
  #[inline]
  pub fn pool_free_count(&self) -> usize {
    self.plane.pool.free_len()
  }

  /// Number of connection slots currently parked mid-close.
  #[inline]
  pub fn closing_count(&self) -> usize {
    self.plane.closing.len()
  }

  /// Whether a passive-open listener slot is currently installed.
  #[inline]
  pub fn listener_present(&self) -> bool {
    self.plane.listener.is_some()
  }

  /// Number of reliable exchanges currently half-closed (local FIN emitted, still
  /// mapped awaiting the peer's reply and/or FIN).
  #[inline]
  pub fn half_closed_count(&self) -> usize {
    self.plane.half_closed_count()
  }

  /// Number of reliable exchanges still in `PendingDial` (dial requested, pool
  /// exhausted, no slot assigned yet).
  #[inline]
  pub fn pending_dial_count(&self) -> usize {
    self.plane.pending_dial_count()
  }

  /// Number of await-result joins currently tracked (in-flight plus resolved but
  /// not yet drained via [`poll_join`](Self::poll_join)) — a diagnostic proving
  /// the join table does not leak once every join resolves and is polled.
  #[inline]
  pub fn pending_join_count(&self) -> usize {
    self.pending_joins.len()
  }
}

// serf-command and read forwarders — reach serf's super-machine only (not the
// reliable plane), so they need node identity and BOTH RNGs (the machine driver
// surface bounds the gossip `G: Rng` and serf's `SR: Rng + SeedableRng`) but no
// connection-handle key.
impl<I, C, G, SR> SerfEngine<I, C, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Arm serf's periodic probe / gossip / push-pull schedulers. Call once before
  /// the first `pump`; without it failure detection, dissemination, and
  /// anti-entropy never run.
  pub fn start(&mut self, now: Instant) {
    self.endpoint.start_scheduling(now);
  }

  /// `Ok` only while the node is running (serf state `Alive`). After `leave()`
  /// the schedulers stop and the machine merges no remote state, so the
  /// operations that gate on this reject rather than queue work no peer would
  /// observe.
  pub fn ensure_running(&self) -> Result<(), SerfError> {
    if self.is_running() {
      Ok(())
    } else {
      Err(SerfError::BadJoinState(self.endpoint.state()))
    }
  }

  /// Whether serf's endpoint is in the running (`Alive`) state.
  #[inline]
  fn is_running(&self) -> bool {
    self.endpoint.state() == SerfState::Alive
  }

  /// serf's current lifecycle state.
  #[inline]
  pub fn state(&self) -> SerfState {
    self.endpoint.state()
  }

  /// Number of serf members currently tracked.
  #[inline]
  pub fn num_members(&self) -> usize {
    self.endpoint.num_members()
  }

  /// The local node's serf member Lamport clock.
  #[inline]
  pub fn member_time(&self) -> u64 {
    self.endpoint.member_time()
  }

  /// The local node's serf event Lamport clock.
  #[inline]
  pub fn event_time(&self) -> u64 {
    self.endpoint.event_time()
  }

  /// The local node's serf query Lamport clock.
  #[inline]
  pub fn query_time(&self) -> u64 {
    self.endpoint.query_time()
  }

  /// The local node's id.
  #[inline]
  pub fn local_id(&self) -> &I {
    self.endpoint.local_id()
  }

  /// A snapshot of every serf member currently tracked (alive, leaving, left, or
  /// failed within the reap window), for the observable membership view a driver
  /// publishes after each membership change.
  #[inline]
  pub fn members_snapshot(&self) -> Vec<Arc<Member<I, SocketAddr>>> {
    self.endpoint.members_snapshot()
  }

  /// Drain one serf event the last `pump` delivered, if any — mandatory
  /// driver-actioned control events FIRST, then passive observations.
  ///
  /// Each [`pump`](Self::pump) drains the machine's event queue to quiescence,
  /// folding every push/pull `ExchangeCompleted` into its await-result join and
  /// routing each event — the full serf surface: membership changes, user events,
  /// queries, query responses / acks, key-management requests / responses,
  /// reliable-exchange completions, and the lifecycle signals (`LeftCluster`,
  /// conflict `Shutdown`) — by class ([`route_drained_event`](Self::route_drained_event)).
  /// This call hands those events to the driver exactly once; it does NOT re-fold
  /// (the pump already folded). Returns `None` when both queues are empty; call
  /// again after the next `pump` tick.
  ///
  /// A MANDATORY control event ([`Event::Shutdown`], [`Event::KeyRequest`],
  /// [`Event::DialRequested`]) is delivered ahead of any queued observation and is
  /// NEVER dropped: promoting it changes no serf semantics — `Shutdown` is terminal,
  /// and a `KeyRequest` / `DialRequested` side effect is independent of
  /// membership-observation order — while guaranteeing an observation flood can
  /// neither evict nor delay a driver action.
  ///
  /// Join resolution and ignore cleanup are driven by the pump on the exchange
  /// terminal, so a driver need NOT drain events here before
  /// [`poll_join`](Self::poll_join) — `poll_join` resolves off the pump-folded
  /// state. The contract is the reactor-faithful "pump, then poll_join / poll_event".
  ///
  /// PASSIVE observation delivery is BEST-EFFORT: that backlog is bounded at
  /// [`DEFAULT_EVENT_BUFFER_CAP`], so a driver that pumps and resolves joins but
  /// stops draining `poll_event` sheds the oldest surplus observations (counted in
  /// [`events_dropped`](Self::events_dropped)) rather than growing memory without
  /// bound — matching the serf std drivers' lossy observation channel. Mandatory
  /// control events are exempt (non-lossy), and join resolution is unaffected
  /// (completions are folded before buffering).
  #[inline]
  pub fn poll_event(&mut self) -> Option<Event<I, SocketAddr>> {
    // Mandatory control signals first: a burst of buffered observations must never
    // starve or delay a Shutdown / KeyRequest / DialRequested. They live on a
    // separate non-lossy queue, so this ordering also makes eviction impossible.
    if let Some(ev) = self.control_events.pop_front() {
      return Some(ev);
    }
    self.buffered_events.pop_front()
  }

  /// The number of PASSIVE observation events shed from the `poll_event` backlog
  /// because it reached [`DEFAULT_EVENT_BUFFER_CAP`] before the driver drained them.
  ///
  /// Observation delivery is BEST-EFFORT under sustained overload: a driver that
  /// pumps and resolves joins but never drains [`poll_event`](Self::poll_event)
  /// sheds the oldest surplus observations rather than growing memory without bound,
  /// and each shed increments this counter (mirroring the serf std drivers'
  /// `events_dropped`). MANDATORY driver-actioned events ([`Event::Shutdown`],
  /// [`Event::KeyRequest`], [`Event::DialRequested`]) take the non-lossy control
  /// path and are never shed WHILE LIVE, so they are never counted here — a
  /// past-deadline `KeyRequest` pruned by
  /// [`prune_expired_control_events`](Self::prune_expired_control_events) is dead, not
  /// a dropped live event, and is likewise uncounted. Join resolution is unaffected —
  /// completions are folded before buffering — so a nonzero count means only that
  /// some observations were not delivered, never that a join was mis-resolved or a
  /// mandatory action was lost.
  #[inline]
  pub fn events_dropped(&self) -> u64 {
    self.events_dropped
  }

  /// Fold one machine event into the await-result join it terminates, if any.
  ///
  /// A push/pull `ExchangeCompleted` whose `eid` was bound to a join at its
  /// `Connect` (via the START `StreamId`) is removed from that join's `pending`
  /// and — on `Succeeded` — accumulates the peer into `contacted`; then
  /// [`try_resolve_join`] resolves the join (clearing any ignore streams) the
  /// instant `pending` empties. A no-op for every other event. Called by
  /// [`drain_fold_events`](Self::drain_fold_events) for every drained event, so the
  /// pump and [`leave`](Self::leave) fold identically.
  fn fold_join_completion(&mut self, ev: &Event<I, SocketAddr>) {
    let Event::ExchangeCompleted(ec) = ev else {
      return;
    };
    if ec.kind() != ExchangeKind::PushPull {
      return;
    }
    let Self {
      endpoint,
      pending_joins,
      ..
    } = self;
    if let Some(pj) = pending_joins
      .values_mut()
      .find(|pj| pj.pending.contains(&ec.eid()))
    {
      pj.pending.remove(&ec.eid());
      if matches!(ec.outcome(), ExchangeStatus::Succeeded) {
        pj.contacted.push(*ec.peer());
      }
      try_resolve_join(endpoint, pj);
    }
  }

  /// Buffer one PASSIVE observation for [`poll_event`](Self::poll_event), bounding the
  /// backlog at [`DEFAULT_EVENT_BUFFER_CAP`].
  ///
  /// At the cap the OLDEST buffered observation is dropped (freshest-wins, so a
  /// never-draining driver keeps the most recent surface) and counted in
  /// `events_dropped`, so the queue cannot grow without bound and the loss stays
  /// observable. Only observations reach here — a mandatory driver-actioned event
  /// takes the non-lossy [`push_control_event`](Self::push_control_event) path — and
  /// the dropped copy is purely the app-delivery one: the event's join completion
  /// was already folded by [`route_drained_event`](Self::route_drained_event) BEFORE
  /// this call, so a drop never affects join resolution. Mirrors memberlist-embassy's
  /// bounded `app_events` (drop-oldest on overflow).
  fn push_app_event(&mut self, ev: Event<I, SocketAddr>) {
    if self.buffered_events.len() >= DEFAULT_EVENT_BUFFER_CAP {
      self.buffered_events.pop_front();
      self.events_dropped += 1;
    }
    self.buffered_events.push_back(ev);
  }

  /// Enqueue one MANDATORY driver-actioned event on the non-lossy `control_events`
  /// queue, deduplicating the terminal [`Event::Shutdown`].
  ///
  /// Mandatory events ([`is_mandatory_event`]) carry a side effect the driver MUST
  /// perform — stop on `Shutdown`, apply + `respond_key` a `KeyRequest`, dial a
  /// `DialRequested` — so a LIVE one is NEVER dropped (dropping one is the bug the
  /// control/observation split fixes: a burst over the observation cap could
  /// otherwise evict a `Shutdown` before any driver acted on it).
  ///
  /// The queue is NOT drop-oldest, but it is still bounded — by LIVENESS, not
  /// cardinality. An encrypted peer can flood distinct inbound `KeyRequest`s
  /// (each pinning raw key material), so
  /// [`prune_expired_control_events`](Self::prune_expired_control_events) sheds every
  /// past-deadline (dead, unanswerable) `KeyRequest` on each pump; this method only
  /// appends. `Shutdown` is idempotent-terminal, so at most one is ever queued.
  fn push_control_event(&mut self, ev: Event<I, SocketAddr>) {
    if matches!(ev, Event::Shutdown)
      && self
        .control_events
        .iter()
        .any(|e| matches!(e, Event::Shutdown))
    {
      return;
    }
    self.control_events.push_back(ev);
  }

  /// Shed every strictly-past-deadline `KeyRequest` from the non-lossy
  /// `control_events` queue, mirroring the endpoint's own `received_queries`
  /// liveness prune.
  ///
  /// A `KeyRequest` is retained while `now <= deadline` and dropped only once
  /// strictly past its deadline (`now > deadline`) — the exact point at which
  /// `respond_key` stops accepting a response for it, since its deadline guard
  /// rejects only `now > deadline` and a `respond_key` at the exact instant
  /// `now == deadline` is still valid. Retaining through the inclusive boundary
  /// therefore never sheds a request a pending `respond_key` could still answer,
  /// while a strictly-past request is dead: dropping it loses nothing real,
  /// promptly releases the raw key material pinned in its payload, and bounds
  /// `control_events` to the LIVE mandatory set — the same `now <= deadline`
  /// retain the serf endpoint applies to its `received_queries` on every
  /// `handle_timeout`. Because the endpoint caps its
  /// LIVE `received_queries` at a fixed inbound maximum and emits exactly one
  /// `Event::KeyRequest` per kept entry (sharing this deadline), the live
  /// `KeyRequest`s retained here are transitively bounded by that same cap — no
  /// separate engine-side count cap is needed. `Shutdown` / `DialRequested` carry no
  /// deadline and are always kept; a repeated `Shutdown` stays deduped by
  /// [`push_control_event`](Self::push_control_event).
  #[cfg(encryption)]
  fn prune_expired_control_events(&mut self, now: Instant) {
    // Keep everything except a strictly-past-deadline KeyRequest — mirroring the
    // endpoint's `retain(|_, rq| now <= rq.deadline)` over received_queries.
    self
      .control_events
      .retain(|ev| !matches!(ev, Event::KeyRequest(kr) if now > kr.deadline()));
  }

  /// Fold one drained machine event into its await-result join, then route it to the
  /// correct delivery queue by class.
  ///
  /// The SOLE per-event routing site, shared by
  /// [`drain_fold_events`](Self::drain_fold_events) (the pump and
  /// [`leave`](Self::leave)), so the classification is single-sourced. The join
  /// completion is folded FIRST ([`fold_join_completion`](Self::fold_join_completion)),
  /// so join accounting is pump-driven and never gated on the app polling events;
  /// then a MANDATORY event ([`is_mandatory_event`]) goes to the non-lossy
  /// `control_events` and a PASSIVE observation to the bounded, drop-oldest
  /// `buffered_events`.
  fn route_drained_event(&mut self, ev: Event<I, SocketAddr>) {
    self.fold_join_completion(&ev);
    if is_mandatory_event(&ev) {
      self.push_control_event(ev);
    } else {
      self.push_app_event(ev);
    }
  }

  /// Drain the machine's event queue to quiescence, folding + routing each event via
  /// [`route_drained_event`](Self::route_drained_event).
  ///
  /// The SOLE drain of the endpoint's event queue. The pump calls it every tick, so
  /// join accounting is pump-driven and never gated on the app polling events; and
  /// [`leave`](Self::leave) calls it before resolving its abandonment, so an
  /// already-succeeded push/pull lands in the reached set. Because it empties the
  /// endpoint queue, a later call folds only the events enqueued since — never
  /// re-folding one already delivered.
  fn drain_fold_events(&mut self) {
    while let Some(ev) = self.endpoint.poll_event() {
      self.route_drained_event(ev);
    }
  }

  /// Announce the local node's join intent and begin an await-result join to
  /// these seeds, returning a [`JoinId`] the driver polls via
  /// [`poll_join`](Self::poll_join).
  ///
  /// Returns immediately; the pump initiates a push/pull to each routable seed on
  /// the next tick. serf's own `join()` rejects a non-Alive endpoint
  /// ([`SerfError::BadJoinState`]); on that rejection nothing is queued and no
  /// handle is minted. When `ignore_old` is set, each seed's push/pull records its
  /// ignore-join stream so the resulting merge suppresses replay of the peer's
  /// pre-join user events; the engine clears any such stream that fails to merge
  /// when the join terminates (no machine leak).
  ///
  /// The join RESOLVES — `poll_join` yields `Some` — once every dispatched
  /// push/pull has terminated (each `ExchangeCompleted` folded in): `Ok` with the
  /// reached-address set if any seed was contacted, else `Err(JoinFailed)`. A join
  /// to an unreachable seed resolves `Err` after that exchange's own stream
  /// timeout; the core imposes no separate caller deadline (a driver that wants an
  /// earlier give-up drops the handle).
  pub fn join(
    &mut self,
    seeds: &[SocketAddr],
    ignore_old: bool,
    now: Instant,
  ) -> Result<JoinId, SerfError> {
    // Ignoring `now`: the per-seed push/pulls are dispatched (with the tick's
    // `now`) in the pump, not here — `join` only announces intent and queues. The
    // parameter is kept for API parity with the reactor / a future synchronous
    // dispatch.
    let _ = now;
    // Announce the serf-level join intent first (this is where the running-state
    // gate lives); only mint the handle and queue seeds once it is accepted.
    self.endpoint.join()?;

    let id = JoinId(self.next_join_id);
    self.next_join_id += 1;

    let mut requested = 0usize;
    for s in seeds {
      // Drop a non-routable seed: it could only produce a doomed dial. Queue only
      // seeds a dial can actually complete, and count them as the `JoinFailed`
      // denominator.
      if socket_addr_is_routable(s) {
        requested += 1;
        self.pending_seeds.push_back(QueuedSeed {
          join: id,
          seed: *s,
          ignore_old,
        });
      }
    }

    self.pending_joins.insert(
      id,
      PendingJoin {
        started: HashSet::new(),
        unstarted: requested,
        pending: HashSet::new(),
        contacted: ReachedSet::new(),
        requested,
        ignore_old,
        ignore_cleared: false,
        reply: JoinReply::Pending,
      },
    );
    Ok(id)
  }

  /// Drain the terminal outcome of an await-result [`join`](Self::join), or `None`
  /// while it is still in flight.
  ///
  /// Returns `Some(Ok(reached))` with the address set the join contacted, or
  /// `Some(Err(JoinFailed))` if every dispatched push/pull terminated without
  /// contacting a seed. `None` means the join has not yet resolved — poll again
  /// after the next `pump` (which folds the terminal `ExchangeCompleted` into the
  /// join; no [`poll_event`](Self::poll_event) drain is required first). The outcome
  /// is delivered EXACTLY ONCE: a second poll of the same handle yields `None`.
  /// This call is the DELIVERY that lets the entry be reaped: once the caller has
  /// retrieved the result AND every dispatched exchange has terminated the entry is
  /// dropped (here, or by the next pump's sweep). A resolved result is therefore
  /// RETAINED until the caller polls it — a pump between resolution and this call
  /// never discards it. The machine's ignore set is cleared on the exchange
  /// terminal regardless of polling, so a dropped handle leaks no machine state; it
  /// leaks only the small unretrieved result entry until a poll or
  /// [`cancel_join`](Self::cancel_join) reaps it, so a driver MUST poll or cancel
  /// every join it starts. An unknown or already-consumed handle yields `None`.
  pub fn poll_join(&mut self, handle: JoinId) -> Option<Result<ReachedSet, JoinFailed>> {
    // Deliver the resolved outcome once (`Ready → Delivered`); yield `None` for an
    // unknown handle, one still in flight, or one already consumed.
    let pj = self.pending_joins.get_mut(&handle)?;
    let outcome = pj.take_ready_reply()?;
    // The caller has consumed the reply. Reap the waiter now if every exchange has
    // terminated; otherwise keep it so the pump can still fold the outstanding
    // `ExchangeCompleted`s and clear the ignore streams on the exchange terminal.
    if pj.exchange_work_done() {
      self.pending_joins.remove(&handle);
    }
    Some(outcome)
  }

  /// Give up an await-result [`join`](Self::join), dropping any of its seeds still
  /// queued for dispatch and forgetting its caller reply, leak-free.
  ///
  /// A driver's supported give-up path (a dropped handle, or a driver-imposed
  /// timeout). Every one of this join's seeds still queued for its per-tick
  /// dispatch is removed, so the pump initiates NO further push/pull on its behalf —
  /// a cancel BEFORE the first pump therefore has zero network side effect. If no
  /// exchange ever started, the entry is reaped immediately. If a push/pull is
  /// still in flight the reply is forgotten but that exchange's ignore token is
  /// RETAINED until its terminal (a late merge still suppresses the seed's pre-join
  /// user events), then the pump — which folds the terminal `ExchangeCompleted` —
  /// reaps the entry and clears the token. So only actually-started exchanges keep
  /// an ignore token, and neither the join table nor the machine's ignore set leaks.
  /// Like [`poll_join`](Self::poll_join) this reflects the pump-folded state (the
  /// reactor-faithful "pump, then cancel_join" contract) and does not itself drain
  /// the machine. An unknown or already-consumed handle is a no-op.
  pub fn cancel_join(&mut self, handle: JoinId) {
    let Self {
      endpoint,
      pending_seeds,
      pending_joins,
      ..
    } = self;
    let Some(pj) = pending_joins.get_mut(&handle) else {
      return;
    };
    // Drop this join's still-queued seeds so the next pump dispatches no push/pull
    // for a cancelled join, and zero its undispatched count to match. Only seeds
    // whose exchange ALREADY started (recorded in `started`, bound into `pending`)
    // survive, scoping the ignore-token retention below to in-flight exchanges.
    pending_seeds.retain(|qs| qs.join != handle);
    pj.unstarted = 0;
    // Clear the ignore streams + resolve if every started exchange is already done
    // (a cancel-before-pump has none), so an immediate removal never strands an
    // ignore-set entry.
    try_resolve_join(endpoint, pj);
    if pj.exchange_work_done() {
      pending_joins.remove(&handle);
    } else {
      // A started push/pull is still in flight: forget the caller reply (so
      // `poll_join` yields nothing) while the ignore token stays until the exchange
      // terminal, where the pump's fold clears it and reaps the entry.
      pj.reply = JoinReply::Delivered;
    }
  }

  /// Begin leaving the cluster.
  ///
  /// Calls serf's graceful-leave path FIRST, so a refused leave that leaves the
  /// node `Alive` (e.g. `LeaveClockExhausted`, the `LTIME_MAX` watermark guard)
  /// returns its error with the in-flight join state untouched — `?` returns
  /// before any of the abandonment below runs.
  ///
  /// On an ACCEPTED leave — which gossips the departure and ultimately emits
  /// [`Event::LeftCluster`] via [`poll_event`](Self::poll_event) — the pump
  /// initiates no further push/pull, so any queued seeds are dropped and every
  /// in-flight await-result join is handed its caller reply once from its reached
  /// set. Any push/pull `ExchangeCompleted` still queued in the endpoint that the
  /// pump has not already folded — a `leave` without a fresh `pump`, or a completion
  /// `endpoint.leave()` itself enqueues — is folded into that reached set FIRST (via
  /// [`drain_fold_events`](Self::drain_fold_events)), so a successful join resolves
  /// `Ok(reached)` rather than a stale `JoinFailed`; the drained events are buffered
  /// and still delivered, in order, by the next `poll_event`. The
  /// `ignore_old` cleanup is deliberately NOT run here: it stays gated on the
  /// exchange terminal ([`try_resolve_join`], driven by the pump fold), so a late
  /// successful push/pull that merges after leave still finds its ignore token in
  /// place and suppresses the seed's pre-join user events — the same
  /// reply-vs-cleanup decoupling serf-reactor uses.
  pub fn leave(&mut self, now: Instant) -> Result<(), SerfError> {
    // Machine leave FIRST: a refused leave returns without mutating serf state, so
    // the join state must stay untouched too.
    self.endpoint.leave(now)?;

    // Fold every still-queued machine completion into its await-result join BEFORE
    // computing any abandonment outcome, so a push/pull that already succeeded — its
    // `ExchangeCompleted` not yet folded by a pump — lands in the reached set rather
    // than being frozen out as a stale `JoinFailed`. The pump normally folds these
    // each tick; this covers a `leave` without a fresh `pump` and any completion
    // `endpoint.leave()` just enqueued. The drained events are buffered for
    // `poll_event` (already folded, delivered in order), preserving delivery.
    // Mirrors serf-reactor's shutdown/leave, which drains-and-folds before it reaps.
    self.drain_fold_events();

    // Shed any now past-deadline KeyRequests the drain just routed, on the same
    // liveness rule the pump applies, so a leave without a following pump still bounds
    // the control queue.
    #[cfg(encryption)]
    self.prune_expired_control_events(now);

    // Accepted: no seed dispatches once leaving, so drop the queue and mark every
    // still-pending join fully dispatched, then deliver each still-pending caller
    // reply once from its NOW-folded reached set. Ignore-stream cleanup stays with
    // `try_resolve_join` on the exchange terminal (never here), so a late push/pull
    // that merges after leave keeps its ignore token.
    self.pending_seeds.clear();
    for pj in self.pending_joins.values_mut() {
      pj.unstarted = 0;
      if matches!(pj.reply, JoinReply::Pending) {
        pj.reply = JoinReply::Ready(pj.outcome());
      }
    }
    Ok(())
  }

  /// Force a named node out of the cluster (an operator-driven removal).
  pub fn force_leave(&mut self, id: I, prune: bool, now: Instant) -> Result<(), SerfError> {
    self.endpoint.force_leave(id, prune, now)
  }

  /// Broadcast an application user event to the cluster.
  ///
  /// `coalesce` requests that identical events be coalesced by name over the
  /// user-coalesce window. Peers observe it as [`Event::User`] via `poll_event`.
  pub fn user_event(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: Bytes,
    coalesce: bool,
  ) -> Result<(), SerfError> {
    self.endpoint.user_event(name, payload, coalesce)
  }

  /// Issue a cluster-wide query, returning its [`QueryId`].
  ///
  /// Responders observe the query as [`Event::Query`] and answer via
  /// [`respond`](Self::respond); responses surface on this node as
  /// [`Event::QueryResponse`].
  pub fn query(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: Bytes,
    params: QueryParams<I>,
    now: Instant,
  ) -> Result<QueryId, SerfError> {
    self.endpoint.query(name, payload, params, now)
  }

  /// Answer a received query. `token` is the [`QueryEvent`] delivered via
  /// [`Event::Query`].
  pub fn respond(
    &mut self,
    token: &QueryEvent<I, SocketAddr>,
    payload: Bytes,
    now: Instant,
  ) -> Result<(), SerfError> {
    self.endpoint.respond(token, payload, now)
  }

  /// Replace the local node's tags, re-advertising them via the coordinator and
  /// refreshing the local member in the membership store.
  pub fn set_tags(&mut self, tags: Tags) -> Result<(), SerfError> {
    self.endpoint.set_tags(tags)
  }

  /// Issue a cluster-wide `install_key` query to add `key` to every node's
  /// keyring.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn install_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.endpoint.install_key(key, now)
  }

  /// Issue a cluster-wide `use_key` query to promote `key` to primary.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn use_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.endpoint.use_key(key, now)
  }

  /// Issue a cluster-wide `remove_key` query to remove `key` from all nodes.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn remove_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.endpoint.remove_key(key, now)
  }

  /// Issue a cluster-wide `list_keys` query to enumerate installed keys.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn list_keys(&mut self, now: Instant) -> Result<QueryId, SerfError> {
    self.endpoint.list_keys(now)
  }

  /// Answer an inbound key-management request. `req` is the [`KeyRequest`]
  /// delivered via [`Event::KeyRequest`]; the driver applies the requested op to
  /// its keyring and passes the outcome as `resp`.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn respond_key(
    &mut self,
    req: &KeyRequest<I, SocketAddr>,
    resp: KeyResponseArgs,
    now: Instant,
  ) -> Result<(), SerfError> {
    self.endpoint.respond_key(req, resp, now)
  }

  /// Apply one inbound key-management [`KeyRequest`] to the engine's LIVE wire
  /// keyring and answer the originator in the same call.
  ///
  /// The live keyring is the coordinator's cross-transport [`EncryptionOptions`]
  /// keyring — the single source of truth both the gossip and the reliable plane
  /// encrypt under. This method reads it, applies the requested op, and pushes the
  /// result back through the coordinator ([`set_encryption_options`]), so a
  /// completed rotation actually re-keys the wire instead of updating a driver-held
  /// shadow the wire never sees. It is the ONLY post-construction keyring mutation
  /// path, so the reported key state and the on-wire AEAD cannot diverge:
  ///
  /// - `install` inserts the key as a secondary (idempotent),
  /// - `use` promotes the key to primary,
  /// - `remove` drops a secondary — refusing the current primary,
  /// - `list` snapshots the keys and primary from the post-op live state.
  ///
  /// A node with no keyring configured answers `result = false` and makes no wire
  /// change. A failed op (unknown key, or removing the primary) answers
  /// `result = false` with a message and leaves the keyring untouched.
  ///
  /// Apply-then-respond: the op is applied to the wire keyring first; the
  /// [`respond_key`](Self::respond_key) that follows is best-effort. The cluster-wide
  /// op has already happened on this node even if the response is past its deadline
  /// or cannot be routed, so an `Err` here means only that the acknowledgement was
  /// not queued — never that the op was skipped. Returns `Ok(())` when a response
  /// was queued (a driver re-egresses it within the tick).
  ///
  /// [`EncryptionOptions`]: memberlist_proto::EncryptionOptions
  /// [`set_encryption_options`]: serf_proto::StreamEndpoint::set_encryption_options
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn handle_key_request(
    &mut self,
    req: &KeyRequest<I, SocketAddr>,
    now: Instant,
  ) -> Result<(), SerfError> {
    let resp = self.apply_key_request(req);
    self.respond_key(req, resp, now)
  }

  /// Read-modify-write the coordinator's live keyring for one [`KeyRequest`],
  /// returning the answer built from the post-op live state. The single keyring
  /// mutation chokepoint behind [`handle_key_request`](Self::handle_key_request).
  #[cfg(encryption)]
  fn apply_key_request(&mut self, req: &KeyRequest<I, SocketAddr>) -> KeyResponseArgs {
    let mut encryption = self.endpoint.encryption_options().clone();
    let Some(current) = encryption.keyring() else {
      return KeyResponseArgs {
        result: false,
        message: "no keyring configured on this node".into(),
        ..Default::default()
      };
    };
    let mut keyring = current.clone();
    let (resp, mutated) = match (req.op(), req.key()) {
      (KeyRequestOperation::Install, Some(key)) => {
        keyring.insert_secondary(*key);
        (
          KeyResponseArgs {
            result: true,
            ..Default::default()
          },
          true,
        )
      }
      (KeyRequestOperation::Use, Some(key)) => match keyring.promote(key.as_bytes()) {
        Ok(()) => (
          KeyResponseArgs {
            result: true,
            ..Default::default()
          },
          true,
        ),
        Err(_) => (
          KeyResponseArgs {
            result: false,
            message: "requested primary key is not installed".into(),
            ..Default::default()
          },
          false,
        ),
      },
      (KeyRequestOperation::Remove, Some(key)) => match keyring.remove_secondary(key.as_bytes()) {
        Ok(()) => (
          KeyResponseArgs {
            result: true,
            ..Default::default()
          },
          true,
        ),
        Err(_) => (
          KeyResponseArgs {
            result: false,
            message: "key is not a removable secondary".into(),
            ..Default::default()
          },
          false,
        ),
      },
      (KeyRequestOperation::List, _) => {
        let mut keys = Vec::with_capacity(1 + keyring.secondaries().len());
        keys.push(*keyring.primary_ref());
        keys.extend(keyring.secondaries().iter().copied());
        (
          KeyResponseArgs {
            result: true,
            primary_key: Some(*keyring.primary_ref()),
            keys,
            ..Default::default()
          },
          false,
        )
      }
      (_, None) => (
        KeyResponseArgs {
          result: false,
          message: "key-management request missing its required key".into(),
          ..Default::default()
        },
        false,
      ),
    };
    // Push the rotated keyring back to the coordinator so the gossip and reliable
    // planes re-key in lockstep. A read-only or failed op leaves the wire unchanged.
    if mutated {
      encryption.set_keyring(keyring);
      self.endpoint.set_encryption_options(encryption);
    }
    resp
  }

  /// The engine's LIVE wire keyring — the coordinator's current keyring, the same
  /// state [`handle_key_request`](Self::handle_key_request) mutates and the gossip
  /// and reliable planes encrypt under. `None` when the node is unencrypted.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn keyring(&self) -> Option<&Keyring> {
    self.endpoint.encryption_options().keyring()
  }
}

// Reliable-plane lifecycle helpers that move the connection handle `C` by value
// (into the pool, the listener slot, and the `StreamIo` socket calls) and reach
// serf's machine, so they need `C: Copy + Eq + Hash` and both RNGs.
impl<I, C, G, SR> SerfEngine<I, C, G, SR>
where
  I: memberlist_proto::Id + Clone,
  C: Copy + Eq + Hash,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Advance serf's state machine once over the driver's already-ticked sockets.
  /// Returns the next wakeup deadline: the minimum of the machine's next timer
  /// AND the soonest gracefully-closing connection's force-abort instant. The
  /// driver folds in its own link-layer next-event deadline.
  ///
  /// The driver owns the super-loop: it ticks its link-layer stack, calls `pump`,
  /// then sleeps until `min(driver_stack_next, pump_result)`, advances the clock,
  /// and loops.
  ///
  /// # Order
  ///
  /// `pump` runs the same ordered phases as memberlist-embedded's `Engine::pump`,
  /// driving serf's super-machine (which threads its own serf pre/post-tick logic
  /// through the composed `handle_timeout`):
  ///
  /// 1a. Reap gracefully-closed (or close-timed-out) connections.
  /// 1b. Accept an inbound connection completed on the listener; replenish it.
  /// 1c. Rebalance: self-heal a missing listener, then assign free slots to
  ///     deferred dials (listener-first).
  /// 2a/2b. Gossip ingress: drain datagrams into `handle_gossip`, then decrypt +
  ///     label-strip + decode each buffered frame and feed the typed messages
  ///     back via `handle_message`.
  /// 3. Reliable ingress: drain each connection's rx into `handle_transport_data`;
  ///     deliver a one-shot EOF on peer FIN.
  /// 5. Join-seed drain: `start_join_push_pull(seed, ignore_old, now)` per queued
  ///     seed, capturing its `StreamId` for await-result correlation.
  /// 6. Machine tick: `handle_timeout` fires due serf + coordinator timers.
  /// 7a–7e. Drain `poll_action`, promote, pump outbound, flush deferred FINs,
  ///     complete `Closing` drains, re-rebalance, then drain + send outbound
  ///     gossip.
  /// 7f. Drain + fold machine events: fold every push/pull `ExchangeCompleted`
  ///     into its await-result join and buffer every event for `poll_event`.
  /// 7g. Resolve + reap await-result joins on the exchange terminal.
  /// 8. Deadline: `min(machine_next, closing_next)`.
  pub fn pump<GI, S>(&mut self, now: Instant, gossip: &mut GI, stream: &mut S) -> Option<Instant>
  where
    GI: GossipIo,
    S: StreamIo<Conn = C>,
  {
    // 1a. Reap gracefully-closing connections the driver's stack tick advanced to
    // completion, so the freed handles back new dials/accepts this same tick.
    self.reap_closing(now, stream);

    // 1b/1c. Accept-and-replenish first (listener-first), then self-heal a missing
    // listener and assign remaining free slots to deferred dials. Running the
    // rebalance BEFORE the machine tick lets a prior-tick `PendingDial` be dialed
    // before its bridge can time out.
    self.check_listener(now, stream);
    self.rebalance_pool(now, stream);

    // 2a. Drain inbound gossip datagrams into the machine's raw ingress buffer.
    {
      let buf = self.gossip_recv.as_mut_slice();
      let endpoint = &mut self.endpoint;
      let cidr_policy = &self.cidr_policy;
      while let Some((src, n)) = gossip.recv(buf) {
        // Drop a gossip datagram from a CIDR-blocked source before the machine
        // sees it.
        if !cidr_blocks(cidr_policy, src.ip()) {
          endpoint.handle_gossip(src, &buf[..n], now);
        }
      }
    }

    // 2b. Decrypt + label-strip + decode each raw gossip frame and feed typed
    // messages back. serf's gossip plane carries only the encryption wrapper (no
    // checksum / compression), so a plaintext build feeds the raw bytes straight
    // to the label check.
    while let Some((src, raw)) = self.endpoint.poll_memberlist_ingress() {
      // With an encryption backend built in, strip (and authenticate) the wrapper
      // first — identity when no keyring is configured, an `Err` (dropped) for a
      // frame the keyring cannot decrypt. Gossip is lossy and self-healing.
      #[cfg(encryption)]
      let plain = match self.endpoint.decrypt_gossip(&raw) {
        Ok(p) => Bytes::from(p),
        Err(_) => continue,
      };
      #[cfg(not(encryption))]
      let plain = raw;
      let opts = DecodeOptions::new(self.label.clone());
      // Drop malformed inbound datagrams silently — bad network input must not
      // panic the node; SWIM is self-healing.
      if let Ok(inner) = decode_incoming(plain, &opts) {
        if let Ok(msgs) = parse_messages::<I, SocketAddr>(inner) {
          for msg in msgs {
            self.endpoint.handle_message(src, msg, now);
          }
        }
      }
    }

    // 3. Reliable ingress pump: drain each active exchange's socket rx into the
    // machine (including the peer-FIN EOF) before the machine tick.
    self.pump_inbound_reliable(now, stream);

    // 5. Drain join seeds: each queued seed starts a join push/pull now. Skipped
    // once leaving/left — a left node initiates no join push/pull. The returned
    // `StreamId` is the await-result correlation token: it is recorded on the
    // owning join and matched against the resulting `Connect`'s `stream_id()`
    // (phase 7a) to bind that exchange's `ExchangeId` into the join's pending set.
    if self.is_running() {
      while let Some(qs) = self.pending_seeds.pop_front() {
        let sid = self
          .endpoint
          .start_join_push_pull(qs.seed, qs.ignore_old, now);
        if let Some(pj) = self.pending_joins.get_mut(&qs.join) {
          pj.started.insert(sid);
          pj.unstarted = pj.unstarted.saturating_sub(1);
        }
      }
    }

    // 6. Machine tick: fire due serf + coordinator timers.
    self.endpoint.handle_timeout(now);

    // 7a. Drain stream actions: open dials, half-close, or tear down exchanges.
    self.drain_stream_actions(now, stream);
    // 7b. Promote dialing connections whose handshake completed this tick.
    self.promote_established(stream);
    // 7c. Reliable egress pump: append new transmits, then flush each queue.
    self.pump_outbound_reliable(stream);
    // 7d. Emit deferred graceful write-half FINs now fully drained.
    self.flush_pending_shutdowns(stream);
    // 7d'. Complete deferred terminal closes of connections draining in `Closing`.
    self.flush_closing(now, stream);
    // 7d''. Re-run the listener/dial rebalance over every slot the machine tick
    // and teardown just freed back to the pool THIS tick.
    self.rebalance_pool(now, stream);

    debug_assert!(
      !self.plane.pool.any_where(|&c| stream.reuse_ready(c))
        || (self.plane.listener.is_some() && self.plane.pending_dial_count() == 0),
      "end-of-tick: a reuse-ready reliable slot left a listener missing or a PendingDial unserviced"
    );

    // 7e. Egress: drain outbound gossip transmits, encode + encrypt, and send.
    self.drain_gossip_transmits(gossip);

    // 7f. Drain the machine's event queue to quiescence, folding every push/pull
    // `ExchangeCompleted` into its await-result join (reducing `pending`,
    // accumulating the reached peer on `Succeeded`) and buffering every drained
    // event for later `poll_event` delivery. Folding HERE — in the pump, after this
    // tick's `Connect`s bound their exchanges (phase 7a) — is what makes join
    // resolution and ignore cleanup terminal-gated and INDEPENDENT of whether the
    // app ever drains `poll_event`, matching serf-reactor (which folds in its poll
    // loop before the observation hand-off).
    self.drain_fold_events();

    // 7f'. Shed past-deadline (dead, unanswerable) KeyRequests from the non-lossy
    // control queue so an encrypted peer flooding distinct key queries cannot grow it
    // without bound or pin their key material — bounding it to the live mandatory set.
    #[cfg(encryption)]
    self.prune_expired_control_events(now);

    // 7g. Resolve await-result joins whose exchanges all terminated, then reap only
    // those whose result the caller has already retrieved. [`try_resolve_join`]
    // clears the machine's ignore set and resolves the caller reply on the exchange
    // terminal — INDEPENDENT of caller polling, so the machine never leaks — which
    // also catches the joins that never accumulate a pending exchange at all (an
    // empty/all-non-routable seed set, or seeds that retired before a `Connect`) now
    // that this tick's `Connect`s have been captured (phase 7a) and folded (phase
    // 7f). The entry itself is reaped ONLY once its reply is `Delivered` (the caller
    // polled it, or `cancel_join` forgot it) AND every exchange has terminated, so a
    // resolved-but-unpolled result is retained until the caller retrieves it rather
    // than dropped out from under a slow/async waiter. A dropped-without-cancel
    // handle then lingers as the small result entry alone — its ignore set already
    // cleared — so a driver must poll or cancel every join it starts.
    {
      let Self {
        endpoint,
        pending_joins,
        ..
      } = self;
      pending_joins.retain(|_, pj| {
        try_resolve_join(endpoint, pj);
        !(matches!(pj.reply, JoinReply::Delivered) && pj.exchange_work_done())
      });
    }

    // 8. Next deadline = min(machine, closing).
    let machine = self.endpoint.poll_timeout();
    let closing = self
      .plane
      .closing
      .values()
      .chain(
        self
          .plane
          .connections
          .values()
          .filter_map(|c| c.close_deadline.as_ref()),
      )
      .min()
      .copied();
    min_opt(machine, closing)
  }

  /// Reclaim gracefully-closing connections that have finished closing or whose
  /// close has exceeded `cfg.close_timeout`.
  fn reap_closing<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let pool = &mut self.plane.pool;
    self.plane.closing.retain(|&c, &mut deadline| {
      if !stream.is_open(c) {
        pool.give(c);
        return false;
      }
      if now >= deadline {
        // Peer vanished mid-FIN: force the socket Closed and reclaim so the pool
        // (and the listener replenished from it) recover.
        stream.abort(c);
        pool.give(c);
        return false;
      }
      true
    });
  }

  /// Consume an accept-ready listener, hand its exchange to the machine, and
  /// replenish a fresh listener from the pool.
  ///
  /// The accept gate is `accepted_peer(c).is_some()`, which a driver reports only
  /// once the socket is at/after Established with a known remote — never a
  /// not-yet-established handshake an RST could revert.
  fn check_listener<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let c = match self.plane.listener {
      Some(c) => c,
      None => return,
    };
    let Some(peer) = stream.accepted_peer(c) else {
      return;
    };

    // Reject a reliable connection from a CIDR-blocked peer at the transport
    // boundary: abort the connected socket and reclaim it WITHOUT registering the
    // exchange, then re-arm a fresh listener.
    if cidr_blocks(&self.cidr_policy, peer.ip()) {
      stream.abort(c);
      self.plane.pool.give(c);
      self.plane.listener = None;
      self.ensure_listener(stream);
      return;
    }

    match self.endpoint.accept_connection(peer, now) {
      Some(eid) => {
        self
          .plane
          .connections
          .insert(eid, Connection::accepted(peer, c));
        self.plane.accepted_inbound += 1;
      }
      // Not admitted (leaving, the inbound-stream cap, or a record-layer config
      // error): abort the socket AND return its handle to the pool so a rejection
      // does not shrink the finite pool one slot at a time.
      None => {
        stream.abort(c);
        self.plane.pool.give(c);
      }
    }

    self.plane.listener = None;
    // Replenish immediately if a slot is free — the same self-heal the poll-phase
    // rebalance uses, giving the listener first claim on a free slot.
    self.ensure_listener(stream);
  }

  /// Re-establish the passive-open listener if it is missing and the pool can
  /// supply a reuse-ready slot. A no-op when a listener already exists or the pool
  /// has no reuse-ready slot.
  fn ensure_listener<S>(&mut self, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    if self.plane.listener.is_some() {
      return;
    }
    if let Some(c) = self.plane.pool.take_where(|&c| stream.reuse_ready(c)) {
      // `listen()` only fails on port 0 or an already-open socket; a pooled socket
      // is Closed and `cfg.port` is the user-supplied non-zero port.
      // Ignoring Err: both failure modes are unreachable for a pooled slot here.
      let _ = stream.listen(c, self.cfg.port);
      self.plane.listener = Some(c);
    }
  }

  /// Give the listener and any deferred dials first claim on whatever is currently
  /// in the pool: self-heal a missing listener, then assign the rest to
  /// `PendingDial` connections oldest-first (listener-first).
  fn rebalance_pool<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    self.ensure_listener(stream);
    self.drain_pending_dials(now, stream);
  }

  /// Assign a freed slot to each connection still waiting in `PendingDial`,
  /// oldest-first by ascending `ExchangeId`, and dial it. Stops the moment the
  /// pool empties again so the rest stay parked for a later tick.
  fn drain_pending_dials<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let mut waiting: MediumVec<(ExchangeId, SocketAddr)> = self
      .plane
      .connections
      .iter()
      .filter(|(_, c)| c.state == ConnState::PendingDial)
      .map(|(&eid, c)| (eid, c.peer))
      .collect();
    if waiting.is_empty() {
      return;
    }
    waiting.sort_by_key(|(eid, _)| eid.get());

    for (eid, peer) in waiting {
      let Some(c) = self.plane.pool.take_where(|&c| stream.reuse_ready(c)) else {
        break;
      };
      if let Some(conn) = self.plane.connections.get_mut(&eid) {
        conn.assign_socket(c);
      }
      self.dial(eid, peer, c, now, stream);
    }
  }

  /// Open a TCP dial for the `Dialing` connection `eid` on its assigned slot `c`.
  ///
  /// A CIDR-blocked or non-routable peer, or a `connect` rejection, reclaims the
  /// socket and terminalizes the exchange as a dial FAILURE via
  /// `handle_dial_failed` — never a benign EOF that a one-way exchange would read
  /// as success.
  fn dial<S>(&mut self, eid: ExchangeId, peer: SocketAddr, c: C, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    if cidr_blocks(&self.cidr_policy, peer.ip()) || !socket_addr_is_routable(&peer) {
      stream.abort(c);
      self.plane.pool.give(c);
      self.plane.connections.remove(&eid);
      self.endpoint.handle_dial_failed(eid, now);
      return;
    }

    // Derive an ephemeral local port from the ExchangeId so each dial uses a
    // distinct port within the IANA ephemeral range (49152–65535).
    let local_port = 49152u16 + (eid.get() as u16 % 16384);
    if stream.connect(c, peer, local_port).is_err() {
      stream.abort(c);
      self.plane.pool.give(c);
      self.plane.connections.remove(&eid);
      self.endpoint.handle_dial_failed(eid, now);
    }
  }

  /// Drain all `StreamAction`s emitted by the machine this tick: open dials
  /// (`Connect`), defer a graceful write-half FIN (`Shutdown`), tear down
  /// gracefully (`Close`), or hard-abort a FAILED exchange (`Abort`).
  fn drain_stream_actions<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    while let Some(action) = self.endpoint.poll_action() {
      match action {
        StreamAction::Connect(info) => {
          let eid = info.id();
          let peer = info.peer();
          // Bind this exchange to its await-result join, if any: the `Connect`'s
          // `stream_id()` is the START `StreamId` a join's `start_join_push_pull`
          // returned (phase 5). Matching it here records the machine-allocated
          // `ExchangeId` into that join's pending set, so the terminal
          // `ExchangeCompleted` (which reports by `eid`) folds back to the right
          // waiter — never by the ambiguous peer address.
          let sid = info.stream_id();
          for pj in self.pending_joins.values_mut() {
            if pj.started.contains(&sid) {
              pj.pending.insert(eid);
              break;
            }
          }
          // Only a reset, reuse-ready slot may back a fresh dial; a freed-but-
          // still-resetting slot defers to `PendingDial` until its worker resets.
          match self.plane.pool.take_where(|&c| stream.reuse_ready(c)) {
            Some(c) => {
              self
                .plane
                .connections
                .insert(eid, Connection::dialing(peer, c));
              self.dial(eid, peer, c, now, stream);
            }
            // Pool exhausted: record a `PendingDial` (no slot) so the dial intent
            // is not lost; `drain_pending_dials` assigns a slot once one frees.
            None => {
              self
                .plane
                .connections
                .insert(eid, Connection::pending_dial(peer));
            }
          }
        }
        StreamAction::Shutdown(r) => {
          // Deferred write-half FIN: set the flag; `flush_pending_shutdowns`
          // emits it once the socket is Established and its tx ring has drained.
          if let Some(conn) = self.plane.connections.get_mut(&r.id()) {
            conn.fin_pending = true;
          }
        }
        StreamAction::Close(r) => {
          self.teardown(r.id(), now, stream);
        }
        StreamAction::Abort(r) => {
          self.abort_exchange(r.id(), stream);
        }
      }
    }
  }

  /// Abort a FAILED exchange (dial failure, label/encryption rejection, or an
  /// elapsed deadline): discard its buffered `out`, hard-reset the socket, and
  /// reclaim the slot straight to the pool.
  fn abort_exchange<S>(&mut self, eid: ExchangeId, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let Some(conn) = self.plane.connections.remove(&eid) else {
      return;
    };
    if let Some(c) = conn.socket {
      stream.abort(c);
      self.plane.pool.give(c);
    }
  }

  /// Tear down a GRACEFULLY completed exchange (`StreamAction::Close`), draining
  /// any undelivered outbound bytes before the terminal FIN and reclaiming the
  /// slot by socket state.
  ///
  /// The one case that does NOT remove the connection on the spot is a graceful
  /// close whose send-capable socket still holds undelivered bytes: it parks in
  /// [`ConnState::Closing`] so the egress pump keeps flushing them, and
  /// `flush_closing` FINs + detaches once they are delivered (or the close
  /// deadline forces an abort). A graceful close never discards undelivered bytes.
  fn teardown<S>(&mut self, eid: ExchangeId, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let Some(conn) = self.plane.connections.get(&eid) else {
      return;
    };
    let Some(c) = conn.socket else {
      // PendingDial: no socket, nothing to reclaim. Removing it is the whole
      // teardown, so a retired exchange is never later dialed.
      self.plane.connections.remove(&eid);
      return;
    };

    let was_half_closed = conn.state == ConnState::HalfClosed;
    let out_pending = !conn.out_is_empty();
    let is_open = stream.is_open(c);
    let may_send = stream.may_send(c);
    let tx_unacked = stream.send_queue(c);

    if !is_open {
      // `Closed | TimeWait`: both FINs already exchanged. Reclaim directly.
      self.plane.connections.remove(&eid);
      self.plane.pool.give(c);
    } else if was_half_closed {
      // Our FIN is in flight and the tx half is closed, so any `out` remainder is
      // undeliverable. Park for the reap backstop.
      self.plane.connections.remove(&eid);
      self.plane.closing.insert(c, now + self.cfg.close_timeout);
    } else if may_send && (out_pending || tx_unacked != 0) {
      // Send-capable with outbound bytes the peer has NOT received. FIN-ing now
      // would truncate the reply; defer via `Closing` so the egress pump keeps
      // draining `out` into the tx ring.
      if let Some(conn) = self.plane.connections.get_mut(&eid) {
        conn.state = ConnState::Closing;
        conn.close_deadline = Some(now + self.cfg.close_timeout);
        conn.close_drain_mark = conn.out_bytes() + tx_unacked;
        conn.fin_pending = false;
      }
    } else if may_send {
      // Send-capable with nothing left to deliver: emit the graceful FIN now and
      // park the handle for the reap backstop.
      self.plane.connections.remove(&eid);
      stream.close(c);
      self.plane.closing.insert(c, now + self.cfg.close_timeout);
    } else {
      // Abrupt teardown of a socket the peer never established: RST and reclaim.
      self.plane.connections.remove(&eid);
      stream.abort(c);
      self.plane.pool.give(c);
    }
  }

  /// Complete the deferred terminal close of every connection draining in
  /// [`ConnState::Closing`]: FIN once `out` and the tx ring are fully drained, or
  /// force-abort one past its (no-progress) close deadline.
  fn flush_closing<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    enum ClosingAction<C> {
      Fin(C),
      Abort(C),
      Progress(usize),
    }

    let mut actions: MediumVec<(ExchangeId, ClosingAction<C>)> = MediumVec::new();
    for (&eid, conn) in self.plane.connections.iter() {
      if conn.state != ConnState::Closing {
        continue;
      }
      let Some(c) = conn.socket else { continue };
      // Undelivered shrinks ONLY when the peer acks, so a shrink is the
      // peer-liveness signal: `close_timeout` bounds a STALL, not the total drain.
      let undelivered = conn.out_bytes() + stream.send_queue(c);
      if undelivered == 0 {
        actions.push((eid, ClosingAction::Fin(c)));
      } else if undelivered < conn.close_drain_mark {
        actions.push((eid, ClosingAction::Progress(undelivered)));
      } else if conn.close_deadline.is_some_and(|d| now >= d) {
        actions.push((eid, ClosingAction::Abort(c)));
      }
    }

    for (eid, outcome) in actions {
      match outcome {
        ClosingAction::Fin(c) => {
          self.plane.connections.remove(&eid);
          stream.close(c);
          self.plane.closing.insert(c, now + self.cfg.close_timeout);
        }
        ClosingAction::Abort(c) => {
          self.plane.connections.remove(&eid);
          stream.abort(c);
          self.plane.pool.give(c);
        }
        ClosingAction::Progress(mark) => {
          if let Some(conn) = self.plane.connections.get_mut(&eid) {
            conn.close_drain_mark = mark;
            conn.close_deadline = Some(now + self.cfg.close_timeout);
          }
        }
      }
    }
  }

  /// Flush partially-written outbound bytes and drain new transport transmits from
  /// the machine into each connection's tx ring, preserving per-connection byte
  /// order under partial-write backpressure.
  fn pump_outbound_reliable<S>(&mut self, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    // Pass 1: append new transmits to their connection's out queue (in order,
    // regardless of state — a Dialing / PendingDial connection holds them until
    // its socket is writable). Bytes for a torn-down exchange are dropped.
    while let Some((eid, _peer, bytes)) = self.endpoint.poll_transport_transmit() {
      if let Some(conn) = self.plane.connections.get_mut(&eid) {
        conn.out.push_back(bytes);
      }
    }

    // Pass 2: flush each connection's out queue to its socket.
    let pairs: MediumVec<_> = self
      .plane
      .connections
      .iter()
      .filter_map(|(&eid, c)| {
        if c.out.is_empty() {
          return None;
        }
        c.socket.map(|h| (eid, h))
      })
      .collect();

    for (eid, c) in pairs {
      // A still-handshaking socket is `!may_send`; leave the queue parked and
      // retry once Established, so a push/pull half is never dropped mid-open.
      if !stream.may_send(c) {
        continue;
      }
      while let Some(front) = self
        .plane
        .connections
        .get(&eid)
        .and_then(|conn| conn.out.front().cloned())
      {
        let sent = stream.send(c, &front);
        if sent >= front.len() {
          if let Some(conn) = self.plane.connections.get_mut(&eid) {
            conn.out.pop_front();
          }
        } else {
          // Partial write: replace the front with its unsent tail and stop, so the
          // tail stays at the front and later entries are not reordered.
          if let Some(conn) = self.plane.connections.get_mut(&eid) {
            if let Some(slot) = conn.out.front_mut() {
              *slot = front.slice(sent..);
            }
          }
          break;
        }
      }
    }
  }

  /// Promote each `Dialing` connection whose TCP handshake has completed to
  /// `Established`.
  fn promote_established<S>(&mut self, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let promote: MediumVec<ExchangeId> = self
      .plane
      .connections
      .iter()
      .filter(|(_, c)| c.state == ConnState::Dialing)
      .filter_map(|(&eid, c)| c.socket.map(|h| (eid, h)))
      .filter(|&(_, h)| stream.may_send(h))
      .map(|(eid, _)| eid)
      .collect();
    for eid in promote {
      if let Some(conn) = self.plane.connections.get_mut(&eid) {
        conn.state = ConnState::Established;
      }
    }
  }

  /// Emit deferred graceful write-half FINs for connections whose socket can now
  /// carry one losslessly — KEEPING the connection mapped so its inbound reply
  /// still pumps. The socket is reclaimed only later, by the machine's `Close`.
  fn flush_pending_shutdowns<S>(&mut self, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    let ready: MediumVec<_> = self
      .plane
      .connections
      .iter()
      .filter(|(_, c)| c.fin_pending && c.state == ConnState::Established && c.out_is_empty())
      .filter_map(|(&eid, c)| c.socket.map(|h| (eid, h)))
      // …and the tx ring is fully drained and acknowledged, so every byte reached
      // the peer before the FIN.
      .filter(|&(_, h)| stream.may_send(h) && stream.send_queue(h) == 0)
      .collect();

    for (eid, c) in ready {
      stream.close(c);
      if let Some(conn) = self.plane.connections.get_mut(&eid) {
        conn.fin_pending = false;
        conn.state = ConnState::HalfClosed;
      }
    }
  }

  /// Drain each active connection's socket rx into the machine, delivering a
  /// one-shot EOF once the peer's FIN has been received AND the rx buffer is fully
  /// drained ([`StreamIo::recv_finished`]).
  fn pump_inbound_reliable<S>(&mut self, now: Instant, stream: &mut S)
  where
    S: StreamIo<Conn = C>,
  {
    const READ_BUF: usize = 4096;
    let mut buf = [0u8; READ_BUF];

    let pairs: MediumVec<_> = self
      .plane
      .connections
      .iter()
      .filter_map(|(&eid, c)| c.socket.map(|h| (eid, h)))
      .collect();

    for (eid, c) in pairs {
      loop {
        match stream.recv(c, &mut buf) {
          Some(n) if n > 0 => {
            self
              .endpoint
              .handle_transport_data(eid, &buf[..n], false, now);
          }
          _ => {
            // No data this tick. Deliver the peer FIN exactly once when the receive
            // half is gracefully closed and drained.
            if stream.recv_finished(c) {
              if let Some(conn) = self.plane.connections.get_mut(&eid) {
                if !conn.eof_delivered {
                  conn.eof_delivered = true;
                  self.endpoint.handle_transport_data(eid, &[], true, now);
                }
              }
            }
            break;
          }
        }
      }
    }
  }

  /// Drain all outbound gossip transmits from the machine, encode each, apply the
  /// encryption wrapper (when a backend is built in), and write it to the gossip
  /// socket.
  ///
  /// serf's gossip plane carries no compression / checksum wrappers — only the
  /// label frame and, under an encryption backend, the AEAD wrapper. Encoding
  /// errors and a full tx ring both silently drop the datagram; gossip is
  /// best-effort and SWIM recovers on the next round.
  fn drain_gossip_transmits<GI>(&mut self, gossip: &mut GI)
  where
    GI: GossipIo,
  {
    let enc = EncodeOptions::new(self.label.clone());
    while let Some(transmit) = self.endpoint.poll_memberlist_transmit() {
      let (dest, bytes) = match encode_transmit::<I>(transmit, &enc) {
        Some(pair) => pair,
        None => continue,
      };
      // Apply the encryption wrapper before the wire (identity when no keyring is
      // configured). Drop rather than emit plaintext on an encrypted-cluster path
      // if the backend rejects the request.
      #[allow(unused_mut)]
      let mut on_wire: Vec<u8> = bytes.to_vec();
      #[cfg(encryption)]
      {
        on_wire = match self.endpoint.encrypt_gossip(&on_wire) {
          Ok(b) => b,
          Err(_) => continue,
        };
      }
      // Last-line egress screens: never emit to a non-routable destination or one
      // our own CIDR policy excludes.
      if !socket_addr_is_routable(&dest) || cidr_blocks(&self.cidr_policy, dest.ip()) {
        continue;
      }
      gossip.send(&on_wire, dest);
    }
  }
}

/// Classify a drained machine [`Event`] as a MANDATORY driver-actioned control
/// signal (the driver must take a side effect beyond observing it) versus a PASSIVE
/// observation (which [`SerfEngine`] has already accounted for, or which is pure
/// app-level information).
///
/// The MANDATORY set is exactly what serf's reliable-stream drivers act on in their
/// synchronous drain — no more, no less — mirroring serf-reactor's `account_event`
/// and serf-compio's `drain_events`:
///
/// - [`Event::Shutdown`] — the local node lost an id-conflict vote and the driver
///   MUST stop (serf-reactor flags `begin_shutdown`; serf-compio sets its terminal
///   flag), then still delivers the event to subscribers.
/// - [`Event::KeyRequest`] — the driver MUST apply the key op and answer the
///   originator via `respond_key` (both reference drivers do so ahead of the
///   observation hand-off); without it the inbound key op silently times out.
/// - [`Event::DialRequested`] — the driver MUST dial the peer and report back via
///   `dial_succeeded` / `dial_failed`. On the reliable-stream path the coordinator
///   sieves the inner dial request into its own dial queue (surfaced as a
///   `poll_action` `Connect` the pump already services), so this event never
///   actually reaches the drain here; it is classified mandatory so that, were any
///   transport to surface it, a driver-owned dial could never be silently evicted.
///
/// Every other variant is a PASSIVE observation, delivered best-effort:
/// [`Event::ExchangeCompleted`] (its await-result join is folded non-lossily by
/// [`fold_join_completion`](SerfEngine::fold_join_completion) BEFORE buffering),
/// [`Event::LeftCluster`] (the engine's [`leave`](SerfEngine::leave) resolves its
/// join replies synchronously, so nothing resolves off the event here),
/// [`Event::Member`] / [`Event::User`] / [`Event::Query`] (app observations, a
/// `Query` response being optional), and [`Event::QueryResponse`] /
/// [`Event::QueryAck`] / [`Event::KeyResponse`] / [`Event::RelayDropped`]
/// (correlated internally by the machine, forwarded to the app only).
fn is_mandatory_event<I, A>(ev: &Event<I, A>) -> bool {
  match ev {
    Event::Shutdown | Event::DialRequested(_) => true,
    #[cfg(encryption)]
    Event::KeyRequest(_) => true,
    Event::Member(_)
    | Event::User(_)
    | Event::Query(_)
    | Event::QueryResponse(_)
    | Event::QueryAck(_)
    | Event::RelayDropped(_)
    | Event::LeftCluster
    | Event::ExchangeCompleted(_) => false,
    #[cfg(encryption)]
    Event::KeyResponse(_) => false,
    // `Event` is `#[non_exhaustive]`, so an exhaustive match is impossible from a
    // downstream crate: an unknown future variant defaults to a best-effort
    // observation. Add it to the mandatory arm above if a new driver-actioned event
    // is ever introduced upstream.
    _ => false,
  }
}

/// Returns the earlier of two optional deadlines. If only one is `Some`, that
/// deadline wins; if both are `None` the result is `None`.
fn min_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
  match (a, b) {
    (Some(x), Some(y)) => Some(core::cmp::min(x, y)),
    (x, y) => x.or(y),
  }
}

/// Encode one outbound gossip transmit using the shared no-std codec.
///
/// Returns `(dest, encoded_bytes)` on success, or `None` if encoding fails (the
/// caller silently skips the datagram — gossip is lossy).
fn encode_transmit<I>(
  t: Transmit<I, SocketAddr>,
  enc: &EncodeOptions,
) -> Option<(SocketAddr, Bytes)>
where
  I: memberlist_proto::Data,
{
  match t {
    Transmit::Packet(pkt) => {
      let (to, msg) = pkt.into_parts();
      let bytes = encode_outgoing(&msg, enc).ok()?;
      Some((to, bytes))
    }
    Transmit::Compound(cmp) => {
      let (to, msgs) = cmp.into_parts();
      let bytes = encode_outgoing_compound(&msgs, enc).ok()?;
      Some((to, bytes))
    }
  }
}

#[cfg(test)]
mod tests;
