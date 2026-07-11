//! The serf `Endpoint` — the transport-agnostic serf-logic core of a
//! Sans-I/O super-machine.
//!
//! Holds serf's membership FSM, three Lamport clocks, user events,
//! queries/responses/relays, push-pull anti-entropy, and network coordinates,
//! but **no** transport reference.  It reaches a memberlist reliable
//! coordinator only through the narrow `Reliable` seam:
//! every serf-logic method that touches the membership transport takes a
//! `&mut impl Reliable<I, A>` (named `t`).  The composing super-machine
//! (`StreamEndpoint` or `QuicEndpoint`) owns both the core and the coordinator
//! as separate fields and threads the latter into the former on each call.
//!
//! The composing super-machine moves opaque `Bytes` in (`handle_packet`) and
//! `Transmit`/`Bytes` out (`poll_transmit`) and ticks `poll_timeout(now)`;
//! this core carries zero transport I/O.  All wall-clock reads are threaded in
//! as a `now: Instant` parameter — no clock reads occur inside this module.
//!
//! # Threat model
//!
//! The machine sits **inside** the keyring trust boundary: cluster members are
//! mutually trusted through a shared AEAD keyring (joining requires possession
//! of the shared key).
//!
//! **In scope:** well-formed reordered, duplicate, or delayed gossip from
//! honest peers; malformed or truncated bytes (the machine never panics on bad
//! input — it drops); bounded memory under honest-but-pathological message
//! volume (floods of distinct event/query/intent ids from honest peers).
//!
//! **Out of scope:** Byzantine or compromised key-holding members (forged
//! responder identities in query acks, crafted Lamport values from a peer
//! that holds the keyring secret).  This is consistent with Go serf, which
//! performs no such defenses.
//!
//! The [`LTIME_MAX`] watermark is an **integrity floor**: any Lamport time at
//! or above it is rejected at ingress as out-of-range, so the local clocks
//! can never be driven to `u64::MAX` via ordinary `+1` advancement.
//! `saturating_add` is the no-UB backstop for the rare cases where a stored
//! clock is already at `u64::MAX`.  A value at or above `LTIME_MAX` cannot
//! arise organically — a clock starting at 0 and advancing by at most 1 per
//! event needs 2^63 events to reach `LTIME_MAX`, which is unreachable in any
//! finite cluster lifetime.  A clock driven near or above `LTIME_MAX` by a
//! corrupt snapshot or crafted peer value parks there: its local emissions
//! are `>= LTIME_MAX` and are rejected by peers, a degraded-but-safe state
//! (no panic, hang, or wrap).  Full functional recovery from such a state is
//! out of scope, consistent with upstream Go serf.  The internal
//! conflict/key-tally membership gate (checking that a response comes from a
//! known cluster member before counting it) protects the self-shutdown control
//! decision; forged responder ids in application-query acks are out of threat
//! model and left to the driver.

//! **Directed transmit output** (ACKs, relay sends, directed query responses)
//! is bounded per operation: at most one ACK plus at most `relay_factor`
//! (≤ 255) relay sends per received query.  The cross-operation transmit
//! backlog — the inner `Endpoint`'s `pending_transmits` queue — is drained by
//! the driver via `poll_transmit` on every tick; that is the driver's
//! Sans-I/O contract.  Per-source rate-limiting of a flooding peer is likewise
//! driver-side responsibility.

use std::{collections::VecDeque, vec::Vec};

use bytes::Bytes;
use memberlist_proto::{
  CheapClone, Data, Id, Instant, Node, PushPullKind, Rng, SeedableRng, SmallRng, StreamId,
  typed::{Meta, NodeState},
};

use self::reliable::Reliable;
use rand::RngExt;
use smol_str::SmolStr;

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::KeyRequestMessage;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use crate::event::{KeyRequest as KeyRequestEvent, KeyRequestOperation, KeyResponseArgs};
use crate::{
  AnyMessage, ConflictResponseMessage, EncodeError, LamportTime, MessageType, ReconnectDelegate,
  bridge::{tags_from_pb, tags_to_pb, user_event_to_pb},
  coalesce::{DropCounter, MemberEventCoalescer, UserEventCoalescer},
  event::{
    DialPassthrough, Event, MemberEvent, MemberEventKind, QueryAck, QueryEvent,
    QueryResponse as QueryResponseEvent,
  },
  framing::{encode_message, peek_frame_header},
  members::{IntentKind, Member, MemberState, MemberStatus, Members, SerfState, remove_old_member},
  options::Options,
  typed::{
    Filter, JoinMessage, LeaveMessage, PushPullMessage, QueryFlag, QueryMessage,
    QueryResponseMessage, RelayMessage, Tags, UserEvent, UserEventMessage, UserEvents,
  },
};

// ── EventBuffer ──────────────────────────────────────────────────────────────

/// Ring-buffer dedup store for received user events.
///
/// Mirrors Go serf `types.go` `eventCore` (buffer + min_time).
/// The ring is indexed by `ltime % buffer.len()`.  Each slot holds the
/// batch of `UserEvent` values that arrived at that lamport time, allowing
/// the machine to detect and discard exact duplicates (same name + payload)
/// at the same ltime without re-sending them.
struct EventBuffer {
  /// Slots indexed by `ltime % len`. `None` = empty / never used.
  buffer: Vec<Option<UserEvents>>,
  /// Events with `ltime < min_time` are unconditionally dropped.
  ///
  /// Set to `old_event_clock + 1` when replaying a snapshot (H8/G5 recovery),
  /// or to the current event-clock's value bumped by 1 after a push-pull that
  /// carries an `eventJoinIgnore` flag (H8/G4).  `0` means no floor is active.
  min_time: u64,
}

impl EventBuffer {
  /// Allocate a ring of `size` empty slots with `min_time = 0`.
  ///
  /// A `size` of 0 is clamped to 1 — the ring modulus must never be zero.
  /// Mirrors `QueryBuffer::new`'s `.max(1)` guard.
  fn new(size: usize) -> Self {
    Self {
      buffer: vec![None; size.max(1)],
      min_time: 0,
    }
  }

  /// Record and deduplicate one user event.
  ///
  /// Returns `true` if this is the **first sight** of `(ltime, ev)` (and
  /// therefore the event should be rebroadcast and emitted locally).
  ///
  /// Returns `false` (drop) when:
  /// - `ltime < self.min_time` (below the recovery floor).
  /// - `ltime` is older than the entire ring relative to `cur_time`
  ///   (`cur_time > buffer_len && ltime < cur_time - buffer_len`).
  /// - The exact `(name, payload)` pair already exists in the slot for this ltime.
  ///
  /// `cur_time` is the event clock value **after** it has been witnessed.
  fn witness_event(&mut self, cur_time: u64, ltime: u64, ev: UserEvent) -> bool {
    // Below min_time floor: drop.
    if ltime < self.min_time {
      return false;
    }

    let bltime = self.buffer.len() as u64;
    // Too old relative to the ring (mirrors Go serf `handle_user_event` check):
    //   if cur_time > bltime && ltime < cur_time - bltime { drop }
    if cur_time > bltime && ltime < cur_time - bltime {
      return false;
    }

    let idx = (ltime % bltime) as usize;

    if let Some(slot) = &mut self.buffer[idx] {
      if slot.ltime.0 == ltime {
        // Same ltime: dedup against existing events.
        for prev in slot.events.iter() {
          if *prev == ev {
            return false; // exact duplicate
          }
        }
        // Per-slot cap: once a slot is saturated, further events at this ltime are
        // treated as already-seen.  This bounds memory under a flood of distinct
        // (name, payload) pairs at the same ltime.
        if slot.events.len() >= MAX_EVENTS_PER_LTIME {
          return false;
        }
        slot.events.push(ev);
      } else {
        // Stale entry from a different ltime wrapped onto this index: replace.
        *slot = UserEvents {
          ltime: LamportTime(ltime),
          events: vec![ev],
        };
      }
    } else {
      self.buffer[idx] = Some(UserEvents {
        ltime: LamportTime(ltime),
        events: vec![ev],
      });
    }
    true
  }
}

// ── QueryBuffer ───────────────────────────────────────────────────────────────

/// Maximum number of distinct `(name, payload)` user events recorded per ring slot (per Lamport time).
///
/// A single ltime can accumulate many coalesced or distinct events from different senders.
/// Without a cap a flooder can send unlimited distinct `(name, payload)` pairs at the same
/// accepted ltime and grow the `Vec<UserEvent>` in the slot without bound — a memory DoS.
/// Once a slot is saturated every subsequent event at that ltime is treated as already-seen
/// (return `false`) so it is not rebroadcast or emitted locally.
///
/// Mirrors the same DoS-bound rationale as `MAX_QUERY_IDS_PER_LTIME`.  Normal clusters never
/// approach this limit.
const MAX_EVENTS_PER_LTIME: usize = 256;

/// Maximum number of distinct query ids recorded per ring slot (per Lamport time).
///
/// A ring slot accumulates all `id` values seen at one `ltime`.  Without a cap,
/// a flooder can send unlimited unique ids at the same accepted `ltime` (local
/// queries do not advance `query_clock`, so a single ltime can stay current for
/// many rounds) and grow the `Vec<u32>` without bound — a memory DoS.  Once a
/// slot reaches this limit every subsequent id at that ltime is treated as
/// already-seen (return `false`) so it is not rebroadcast or delivered locally.
///
/// This is a port-specific per-slot DoS hardening.  Go/oracle lack a per-slot
/// id cap; their `QueryBufferSize` (default 512) is the ring *length* (number
/// of ltime slots), not a per-slot limit.  The ring length is configured
/// separately via `Options::with_query_buffer_size`.  Normal clusters never
/// issue more than a handful of queries per Lamport tick, so this cap is only
/// reachable under adversarial conditions.
const MAX_QUERY_IDS_PER_LTIME: usize = 2048;

/// Maximum cardinality of `received_queries`.
///
/// A flood of distinct `(ltime, id)` queries with huge timeouts would grow
/// `received_queries` without bound: the deadline-pruning in `handle_timeout`
/// only fires periodically, and a fast flooder can insert faster than the
/// pruner evicts.  When the map is already at this cap, the incoming query is
/// DROPPED before any state mutation — no clock witness, no dedup write, no
/// ACK, no event emission, no rebroadcast.  This preserves every already-surfaced
/// token (which may still be waiting on `respond` / `respond_key`) and keeps
/// memory bounded.  Normal clusters never approach this limit; it is reachable
/// only under adversarial flood.
const MAX_RECEIVED_QUERIES: usize = 2048;

/// Maximum inbound query timeout accepted from a peer.
///
/// A peer-supplied `msg.timeout` is clamped to this value before computing
/// the response deadline, preventing a flooder from pinning `received_queries`
/// entries open for an arbitrarily long time.  The bound mirrors the originator's
/// own default-timeout heuristic ceiling (several minutes at cluster scale).
const MAX_QUERY_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(600);

/// Distinguishes the two call sites of `handle_query`.
///
/// The inbound overflow cap (`MAX_RECEIVED_QUERIES`) applies only to
/// `Inbound` queries so that peer flooding cannot silently suppress a locally-
/// originated query from self-processing on the initiating node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryOrigin {
  /// The query arrived from a peer via `handle_user_packet`.  Subject to the
  /// inbound overflow cap.
  Inbound,
  /// The query was originated locally via `query()` or `internal_query()`.
  /// The cap is bypassed; the initiating node MUST always self-process.
  Local,
}

/// Per-ltime slot stored in the query ring buffer.
///
/// Holds all `id` values (random u32 per query) seen at a given Lamport time.
/// Multiple queries may share the same `ltime` if they were issued concurrently;
/// they are distinguished by their random `id`.
#[derive(Clone)]
struct Queries {
  ltime: LamportTime,
  /// All query ids seen at this ltime.
  query_ids: Vec<u32>,
}

/// Ring-buffer dedup store for received queries.
///
/// Mirrors Go serf `types.go` `queryCore` / `handleQuery` dedup logic.
/// The ring is indexed by `ltime % buffer.len()`.  Each slot holds a `Queries`
/// value recording all `id`s seen at that Lamport time.
///
/// Queries older than the entire ring (`cur_time > ring_len && ltime < cur_time
/// - ring_len`) are also rejected as "too old" to prevent stale re-delivery.
struct QueryBuffer {
  /// Slots indexed by `ltime % len`. `None` = never used.
  buffer: Vec<Option<Queries>>,
  /// Queries with `ltime < min_time` are unconditionally dropped.
  ///
  /// Raised to `old_query_clock + 1` when replaying a snapshot (G5
  /// clock-recovery analog for queries).  `0` means no floor is active.
  min_time: u64,
}

impl QueryBuffer {
  /// Allocate a ring of `size` empty slots with `min_time = 0`.
  fn new(size: usize) -> Self {
    Self {
      buffer: vec![None; size.max(1)],
      min_time: 0,
    }
  }

  /// Record and deduplicate one query by `(ltime, id)`.
  ///
  /// Returns `true` if this is the **first sight** of this `(ltime, id)` pair
  /// (and therefore the query should be rebroadcast and dispatched locally).
  ///
  /// Returns `false` (drop) when:
  /// - `ltime < self.min_time` (below the recovery floor).
  /// - `ltime` is older than the entire ring relative to `cur_time`
  ///   (`cur_time > ring_len && ltime < cur_time - ring_len`).
  /// - The same `(ltime, id)` pair already exists in the slot for this ltime.
  ///
  /// `cur_time` is the query clock value **after** it has been witnessed.
  fn witness_query(&mut self, cur_time: u64, ltime: u64, id: u32) -> bool {
    // Below min_time floor: drop.
    if ltime < self.min_time {
      return false;
    }

    let bltime = self.buffer.len() as u64;
    // Too old relative to the ring (mirrors Go serf `handleQuery` check).
    if cur_time > bltime && ltime < cur_time - bltime {
      return false;
    }

    let idx = (ltime % bltime) as usize;

    match self.buffer[idx].as_mut() {
      // Slot holds this ltime: dedup by id, then record this id.
      Some(seen) if seen.ltime.0 == ltime => {
        for &prev in &seen.query_ids {
          if prev == id {
            return false; // exact (ltime, id) duplicate
          }
        }
        // Per-slot cap: once a slot is saturated, further ids at this ltime are
        // treated as already-seen.  This bounds memory use under a flood of
        // unique ids at the same ltime (the slot stays owned by the current
        // ltime but new ids are silently rejected — not rebroadcast, not
        // delivered locally).  Normal clusters never approach this limit.
        if seen.query_ids.len() >= MAX_QUERY_IDS_PER_LTIME {
          return false;
        }
        seen.query_ids.push(id);
        // The slot already existed for this ltime, so this id is a *new* query
        // at the same ltime (two concurrent queries with same ltime, different
        // random ids).  Return true so it is processed.
        true
      }
      // Empty slot, or stale entry from a different ltime that wrapped onto this
      // ring index: start a fresh record.  Replacing a stale entry is correct —
      // keeping it would cause the new ltime's dedup to miss re-arrivals.
      _ => {
        self.buffer[idx] = Some(Queries {
          ltime: LamportTime(ltime),
          query_ids: vec![id],
        });
        true
      }
    }
  }
}

// ── QueryId ───────────────────────────────────────────────────────────────────

/// Opaque identifier for an issued query, keyed by `(ltime, id)`.
///
/// `ltime` is the query-clock value stamped at issue time (READ, not
/// incremented — G8 / H8).  `id` is a random `u32` drawn from `self.rng`
/// so that multiple queries issued at the same Lamport time are distinguishable
/// by the dedup ring.  Together `(ltime, id)` is the canonical composite key
/// used by the `QueryBuffer` and `PendingQuery` registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryId {
  /// The query-clock value at query-issue time (read, not incremented).
  pub ltime: LamportTime,
  /// A random u32 drawn from `self.rng` at query-issue time.
  pub id: u32,
}

// ── QueryParams ───────────────────────────────────────────────────────────────

/// Parameters for issuing a query via [`Endpoint::query`].
///
/// `Default` provides the "no filters, no relay, no ack, zero timeout"
/// baseline; callers supply a non-zero `timeout` (or the machine computes one
/// via `default_query_timeout` if zero is given).
#[derive(Debug, Clone, Default)]
pub struct QueryParams<I> {
  /// Optional node-id / tag-filter list.  Empty means "broadcast to all".
  pub filters: Vec<Filter<I>>,
  /// Number of relay hops to request from responders.
  pub relay_factor: u8,
  /// If `true`, the machine sets the ACK flag and waits for per-hop acks.
  pub request_ack: bool,
  /// Maximum time allowed for responses.
  ///
  /// A value of `Duration::ZERO` causes the machine to substitute
  /// `gossip_interval * query_timeout_mult * log10(n_members + 1)`.
  pub timeout: core::time::Duration,
}

// ── PendingQuery ──────────────────────────────────────────────────────────────

/// Purpose of a pending query, used by the internal-query interceptor.
///
/// `App` queries surface responses as `Event::QueryResponse`; `Conflict` and
/// `Key` queries (encryption-gated) are folded internally into `Event::KeyResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryPurpose {
  /// An application-issued query; responses surface as `Event::QueryResponse`.
  App,
  /// An internal conflict-resolution query; responses tally into a vote.
  Conflict,
  /// An internal key-management query; responses aggregate into a `KeyResponse`.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  Key,
}

// ── KeyResponseTally ──────────────────────────────────────────────────────────

/// Accumulator for folding per-node `KeyResponseMessage`s into a `KeyResponse<I>`.
///
/// Created inside `PendingQuery.key_tally` when `kind == Key`.
/// Folded per response in `handle_key_response_fold`; materialized into a
/// `KeyResponse<I>` and emitted as `Event::KeyResponse` in `close_key_query`.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
pub(crate) struct KeyResponseTally<I> {
  /// Number of nodes that responded with `result = false`.
  pub(crate) num_err: usize,
  /// Key → count of nodes reporting that key.
  pub(crate) keys: crate::FxHashMap<memberlist_proto::SecretKey, usize>,
  /// Primary key → count of nodes reporting it as primary.
  pub(crate) primary_keys: crate::FxHashMap<memberlist_proto::SecretKey, usize>,
  /// Per-node error message for nodes that reported result=false.
  pub(crate) messages: crate::FxHashMap<I, smol_str::SmolStr>,
}

/// Bookkeeping for an in-flight query this node originated.
///
/// Created in `query()` and kept alive until the `deadline` elapses (in
/// `handle_timeout`).  On expiry, `App` queries are silently closed; internal
/// query closes trigger tallying / emitting the result event.
pub(crate) struct PendingQuery<I> {
  /// Why this query was issued — controls how responses are folded.
  pub(crate) kind: QueryPurpose,
  /// Wall-clock deadline after which no more responses are useful.
  ///
  /// Armed at `now + timeout` in `query()`.
  pub(crate) deadline: Instant,
  /// Tracks which responders have already replied (dedup by id).
  ///
  /// The value is `()` — membership is the only information we need.
  pub(crate) responses: crate::FxHashMap<I, ()>,
  /// Tracks which peers have already acknowledged (dedup by id).
  ///
  /// Kept separate from `responses` because a single peer may send both an ack
  /// (on receipt) and a later response; deduping them in the same set would
  /// drop the response after the ack.  Non-empty only for `request_ack` queries.
  pub(crate) acks: crate::FxHashMap<I, ()>,
  /// The composite `(ltime, id)` key of the query.
  pub(crate) query_id: QueryId,
  /// Whether the originating `query()` requested acks from responders.
  ///
  /// Only queries issued with `params.request_ack = true` set the ACK flag in
  /// the wire message.  Acks received for queries that did NOT request acks are
  /// silently dropped here (after the ingress gate in `handle_query_response`).
  pub(crate) request_ack: bool,
  /// Conflict-resolution vote tally: count of responses where the peer's
  /// reported address matched the local advertise address.
  ///
  /// Non-zero only for `kind == Conflict`; always 0 for `App` and `Key`.
  pub(crate) conflict_matching: usize,
  /// Number of nodes the query was targeted at, captured at issue time.
  ///
  /// For `Key` queries this is `members.states.len()` at the moment
  /// `internal_query()` fires (mirrors Go serf `key_manager.go`
  /// `streamKeyResponse` which initialises `resp.num_nodes` from
  /// `this.num_members()`).  Set to 0 for `App` and `Conflict` queries where
  /// `num_nodes` is not surfaced.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  pub(crate) num_nodes: usize,
  /// Key aggregation state; `Some` only when `kind == Key`.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  pub(crate) key_tally: Option<KeyResponseTally<I>>,
}

// ── ReceivedQuery ─────────────────────────────────────────────────────────────

/// Bookkeeping for a query this node received (surfaced as [`Event::Query`]).
///
/// Kept until `respond()` fires (entry removed on success) or the deadline
/// passes (pruned by `handle_timeout`).  The once-only constraint (G7 guard 2)
/// is enforced by removing the entry on the first successful `respond()`; a
/// second call then falls through the `.ok_or(Error::AlreadyResponded)` guard.
pub(crate) struct ReceivedQuery<A> {
  /// Address of the originating node (the `respond()` directed-send target).
  pub(crate) from: A,
  /// Deadline after which a response is no longer useful (G7 guard 3).
  pub(crate) deadline: Instant,
}

// ── endpoint errors ───────────────────────────────────────────────────────────

/// Errors returned by `Endpoint` operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
  /// A `leave()` or `force_leave()` was called from an invalid lifecycle state.
  ///
  /// `leave()` is idempotent when already `Left` (returns `Ok`), but rejected
  /// when already `Leaving` or `Shutdown`.
  #[error("leave called from invalid state: {0}")]
  BadLeaveState(SerfState),
  /// `leave()` could not stamp a leave intent because the member Lamport clock
  /// has reached the `LTIME_MAX` integrity floor.
  ///
  /// The post-incremented leave ltime would land at or above `LTIME_MAX`, which
  /// every node (including this one) rejects, so broadcasting it or starting the
  /// inner leave would desync local membership.  The endpoint parks
  /// degraded-but-safe: it stays `Alive` and consistent, emits no invalid
  /// intent, and starts no inner leave.  Unreachable in any finite cluster
  /// lifetime (2^63 membership events).
  #[error("leave clock exhausted: member clock reached the LTIME_MAX integrity floor")]
  LeaveClockExhausted,
  /// `join()` was called while the local endpoint is not `Alive`.
  ///
  /// serf only announces its own join intent from the `Alive` state; a
  /// `Leaving`, `Left`, or `Shutdown` endpoint rejects the call.  Mirrors the
  /// `bad_join_status` guard in serf-core `api.rs` `join()`.
  #[error("join called from invalid state: {0}")]
  BadJoinState(SerfState),
  /// A command was issued after the serf machine shut down.
  ///
  /// Losing an id-conflict vote forces the machine to [`SerfState::Shutdown`]
  /// (mirroring Go serf, whose conflict-loss branch calls `shutdown()`, which
  /// sets the state).  A shut-down machine is no longer a cluster participant,
  /// so every command that would originate new work — user events, queries, tag
  /// updates, query responses, and key-management issuance — is refused with
  /// this error.  The driver owns stopping I/O and delivering the buffered
  /// [`Event::Shutdown`]; the machine only refuses to originate.
  #[error("operation attempted after the serf machine shut down")]
  Shutdown,
  /// The inner memberlist `leave()` returned an error.
  #[error("inner leave error: {0}")]
  InnerLeave(#[from] memberlist_proto::Error),
  /// A user event or its name+payload exceeds the configured
  /// `max_user_event_size`.
  ///
  /// Carries `(actual_size, limit)`.
  #[error("user event too large: {0} bytes exceeds limit of {1}")]
  UserEventTooLarge(usize, usize),
  /// The wire codec failed to encode a user event for broadcast.
  #[error("user event encode error: {0}")]
  UserEventEncode(#[from] EncodeError),
  /// A query payload exceeds `query_size_limit`.
  ///
  /// Carries `(actual_encoded_size, limit)`.
  #[error("query too large: {0} bytes exceeds limit of {1}")]
  QueryTooLarge(usize, usize),
  /// The response payload exceeds `query_response_size_limit` (G7 guard 1).
  ///
  /// Carries `(actual_size, limit)`.
  #[error("query response too large: {0} bytes exceeds limit of {1}")]
  RespondTooLarge(usize, usize),
  /// `respond()` was called a second time for the same query token (G7 guard 2).
  ///
  /// Each received query may be responded to at most once.  The span is zeroed
  /// on the first successful `respond()`; subsequent calls return this error.
  /// Mirrors Go serf `event.go` `query_already_responsed`.
  #[error("query already responded")]
  AlreadyResponded,
  /// `respond()` was called after the query's deadline elapsed (G7 guard 3).
  ///
  /// Mirrors Go serf `event.go` `query_timeout`.
  #[error("query response deadline exceeded")]
  RespondAfterDeadline,
  /// The wire codec failed to encode a query response for sending.
  #[error("query response encode error: {0}")]
  RespondEncode(EncodeError),
  /// The directed send for a query response failed.
  ///
  /// The `responded` flag is not set; the caller may retry `respond()`.
  #[error("query response send error: {0}")]
  RespondSend(memberlist_proto::Error),
  /// A `Filter::Tag` in the query carries a regex pattern that fails to
  /// compile.  No side effects occur: no `PendingQuery` is registered, no
  /// broadcast is queued, no clock is advanced.
  ///
  /// Only reachable when the `tag-regex` feature is enabled; without it,
  /// tag-filter matching uses exact string equality, which has no compile step
  /// and cannot produce an invalid pattern.
  #[cfg(feature = "tag-regex")]
  #[cfg_attr(docsrs, doc(cfg(feature = "tag-regex")))]
  #[error("query filter contains an invalid tag regex")]
  InvalidQueryFilter,
  /// The coordinator's `update_meta()` call from `set_tags()` failed (e.g. the
  /// encoded tag map exceeds the metadata cap).
  #[error("set_tags update_meta error: {0}")]
  SetTagsMeta(memberlist_proto::Error),
}

// ── clock witness ─────────────────────────────────────────────────────────────

/// The ingress reject watermark for Lamport clock values.
///
/// Any Lamport time at or above this value is rejected on every ingress path
/// as out-of-range and untrustworthy.  The upper half `[LTIME_MAX, u64::MAX]`
/// can only be reached after 2^63 events — unreachable in any finite cluster
/// lifetime — so any wire value in that range signals corruption or a crafted
/// packet and is silently dropped before any state mutation.
///
/// Mirrors Go serf's naked-wrapping clock: no storage clamp is applied, but
/// out-of-range ingress values are unconditionally rejected.
pub(crate) const LTIME_MAX: u64 = 1u64 << 63;

/// Return `true` if `t` is a valid Lamport time that can be safely witnessed.
///
/// Accepts times in `[0, LTIME_MAX)`.  Times at or above `LTIME_MAX` are
/// rejected at every ingress gate as out-of-range.
///
/// Applied as the WHOLE-MESSAGE DROP gate at the entry of every Lamport
/// ingress handler (join intent, leave intent, user event, query, push-pull
/// top-level clocks, load_snapshot clocks), before any state mutation.
#[inline]
pub(crate) fn ltime_is_acceptable(t: u64) -> bool {
  t < LTIME_MAX
}

/// Advance `clock` to at least `t + 1`.
///
/// Witness semantics: if the observed time `t` is at or beyond the current
/// clock value, advance the clock past it so the next local event gets a
/// strictly greater timestamp.  Older observations are no-ops (no regression).
/// Mirrors Go serf `types/clock.go` `Witness`.
///
/// Only values that pass `ltime_is_acceptable` are ever witnessed; this
/// function is a defense-in-depth backstop that re-checks the gate so that a
/// site which bypasses the ingress check cannot drive the clock to `u64::MAX`.
/// `saturating_add` is the no-UB backstop: it parks a clock driven to
/// `u64::MAX` there rather than wrapping to 0.
#[inline]
pub(crate) fn witness(clock: &mut u64, t: u64) {
  // Defense-in-depth: refuse unacceptable Lamport times even if the ingress
  // gate was bypassed.  The primary gate is `ltime_is_acceptable` at each
  // ingress site; this check catches any site that was missed.
  if !ltime_is_acceptable(t) {
    return;
  }
  if t >= *clock {
    *clock = t.saturating_add(1);
  }
}

/// Stamp the current Lamport time from `clock` and advance the clock.
///
/// Reads the current clock value as the stamp, then advances the stored clock
/// by one using `saturating_add` (the no-UB backstop: a clock at `u64::MAX`
/// parks there rather than wrapping to 0).
///
/// All five local-emission sites (leave, force_leave, user_event, query,
/// internal_query) route through this helper.  Mirrors Go serf
/// `types/clock.go` `Time` + `Increment`.
#[inline]
fn next_ltime(clock: &mut u64) -> u64 {
  let stamped = *clock;
  *clock = stamped.saturating_add(1);
  stamped
}

// ── Endpoint ──────────────────────────────────────────────────────────────────

/// The serf-logic core of the Sans-I/O super-machine.
///
/// Holds all serf state — the three Lamport clocks, membership store, options,
/// event ring, query bookkeeping, deadlines, and (feature-gated) the coordinate
/// client — and **no** transport reference.  It reaches a memberlist reliable
/// coordinator only through the narrow `Reliable` seam:
/// every serf-logic method that must touch the membership transport takes a
/// `&mut impl Reliable<I, A>` (named `t`), borrowed disjointly from the core's
/// own state.  The composing super-machine (`StreamEndpoint` or `QuicEndpoint`)
/// owns both the core and the coordinator as separate fields and threads the
/// latter into the former.
///
/// `I` is the node-id type; `A` is the (resolved) address type; `R` is the
/// random number generator injected at construction time (default: `SmallRng`).
/// serf's own selection draws (relay picks, reconnect probabilistic gate,
/// coordinate jitter, query id generation) use `self.rng`; the inner
/// memberlist `Endpoint`'s gossip uses its own independently-seeded `R`.
///
/// `D` is the [`DropCounter`] storage for the two coalescer shed counts
/// (default: a plain `u64`, keeping the machine atomics-free and `Send + Sync`).
/// An async driver injects its own shared, read-observable backing via
/// [`new_with_rng_in`](Self::new_with_rng_in) so its detached handle reads the
/// shed count without the endpoint publishing a copy.
pub struct Endpoint<I, A, R = SmallRng, D = u64>
where
  I: Eq + core::hash::Hash,
  D: DropCounter,
{
  /// serf configuration knobs.
  opts: Options,
  /// Member (SWIM membership) Lamport clock — plain `u64`, no atomics.
  /// Single-threaded machine; no concurrent writers.
  clock: u64,
  /// User-event Lamport clock.
  event_clock: u64,
  /// Query Lamport clock.
  query_clock: u64,
  /// Serf membership store (states, intents, left/failed lists).
  members: Members<I, A>,
  /// Local lifecycle state of this serf endpoint.
  state: SerfState,
  /// serf's own injected RNG — separate from the inner Endpoint's `R`.
  /// Used for: query id generation, relay / k-random member picks,
  /// reconnect probabilistic gate, coordinate jitter.
  rng: R,
  /// Next deadline at which the reaper should run.
  next_reap: Option<Instant>,
  /// Next deadline at which the reconnector should attempt a dial.
  next_reconnect: Option<Instant>,
  /// Next deadline at which the broadcast queue depth is checked.
  next_queue_check: Option<Instant>,
  /// Deadline after which the `Leaving → Left` transition fires.
  ///
  /// Armed in the `LeftCluster` sieve arm at `now + leave_propagate_delay`.
  /// When this deadline elapses in `handle_timeout`, the state transitions to
  /// `Left` (unless already `Shutdown`) and `Event::LeftCluster` is emitted.
  /// `None` when not waiting for the propagation delay.
  leave_complete_deadline: Option<Instant>,
  /// Ring-buffer for user-event deduplication.
  ///
  /// Sized by `opts.event_buffer_size()` at construction time.
  /// Each slot holds the set of `(name, payload)` pairs seen at that lamport
  /// time so exact duplicates are suppressed.
  event_buffer: EventBuffer,
  /// Ring-buffer for query deduplication keyed by `(ltime, id)`.
  ///
  /// Sized by `opts.query_buffer_size()` at construction time.
  /// Each slot holds all `id` values seen at that lamport time.
  query_buffer: QueryBuffer,
  /// Registry of in-flight queries originated by this node.
  ///
  /// Keyed by `QueryId` (= `(ltime, id)` composite).  Entries are created in
  /// `query()` and removed when the `deadline` elapses in `handle_timeout`.
  pending_queries: Vec<PendingQuery<I>>,
  /// Registry of queries received by this node and surfaced to the app.
  ///
  /// Keyed by `QueryId` (= `(ltime, id)` composite).  Entries are created when
  /// a query passes filters and is emitted as `Event::Query`.  `respond()` looks
  /// up the entry to enforce the three guards (size, once-only, deadline) and
  /// to obtain the originator address for the directed send.
  received_queries: crate::FxHashMap<QueryId, ReceivedQuery<A>>,
  /// serf-level events queued for the driver to drain via `poll_event`.
  pending_events: VecDeque<Event<I, A>>,
  /// Member-event coalescer, or `None` when member coalescing is disabled
  /// (either period is zero — the default).
  ///
  /// When `Some`, membership events are fed here at their emission sites instead
  /// of pushed straight to `pending_events`; the batch is flushed into
  /// `pending_events` once the coalescer's window closes (`after_inner_timeout`).
  /// When `None`, every membership event passes straight through unchanged —
  /// the exact behaviour of a machine built with the default (disabled) options.
  member_coalescer: Option<MemberEventCoalescer<I, A>>,
  /// User-event coalescer, or `None` when user coalescing is disabled (either
  /// user period is zero — the default).
  ///
  /// Only user events that opted in (`UserEventMessage::cc == true`) are fed
  /// here; a non-coalescing user event passes straight through even when the
  /// coalescer is enabled (mirrors the legacy coalescer's `handle` predicate).
  user_coalescer: Option<UserEventCoalescer>,
  /// Cumulative user-coalescer shed count, incremented in `emit_user` when the
  /// user coalescer drops an event at its volume cap.  Held always-present (out
  /// of the `Option` coalescer) so the count survives `reset`/`flush` and a
  /// driver-injected backing stays wired for the endpoint's whole lifetime.
  user_drop: D,
  /// Cumulative member-coalescer shed count, incremented in `emit_member` when
  /// the member coalescer drops a change at its cardinality cap.
  member_drop: D,
  /// Optional per-member override for the reaper's reconnect / tombstone
  /// timeout (Go serf `ReconnectDelegate`).
  ///
  /// Consulted by `fire_reap` for every failed and left member it considers;
  /// `None` is the noop (the flat configured timeouts apply unchanged). Boxed
  /// `dyn` carries `Send + Sync` from the trait's supertraits, so the endpoint
  /// keeps its auto-traits for the multi-threaded drivers.
  reconnect_delegate: Option<std::boxed::Box<dyn ReconnectDelegate<I, A>>>,
  /// The most recent directed-send (address, bytes) produced by
  /// `handle_relay` or `relay_response`.
  ///
  /// Captured only in test builds so assertions can inspect the relay path
  /// without a live socket layer.
  #[cfg(test)]
  last_directed_send: Option<(A, Bytes)>,
  /// Addresses dialled by `load_snapshot` for rejoin (G10, skip self).
  ///
  /// Populated only in test builds so assertions can verify which peers were
  /// dialled after snapshot replay without a live socket layer.
  #[cfg(test)]
  rejoin_dials: Vec<A>,
  /// Vivaldi network-coordinate client (Vivaldi algorithm engine).
  ///
  /// `Some` when the `coordinates` feature is compiled in and
  /// `opts.disable_coordinates()` was `false` at construction.
  /// `None` when the feature is disabled at compile time or coordinates
  /// were explicitly disabled via `Options::with_disable_coordinates(true)`.
  #[cfg(feature = "coordinates")]
  coord_client: Option<crate::coordinate_client::CoordinateClient<I>>,
  /// Cache of the most-recently-seen coordinate for each known peer.
  ///
  /// Updated on every successful `PingCompleted` RTT feed.  Keyed by node id.
  /// Removed when a node is reaped from membership (G13).
  #[cfg(feature = "coordinates")]
  coord_cache: crate::FxHashMap<I, crate::typed::Coordinate>,
  /// The `now` instant threaded into the most recent poll/handle call.
  ///
  /// The inner `poll_event` loop (drain_inner) fires synchronously from
  /// within `handle_packet`/`handle_timeout`/etc., and inner events like
  /// `NodeLeft` need `now` for `leave_time`.  Rather than thread `now`
  /// through every inner-sieve dispatch, we latch it here before each drain.
  /// Callers that do not have a meaningful `now` (e.g., `poll_event` called
  /// after a prior handle call) use the last latched value.
  drain_now: Instant,
  /// Monotonically non-decreasing processing clock for coalescer window
  /// scheduling.  `drain_now` carries protocol arrival time, which is NOT
  /// monotonic — reliable ingress can be processed after a newer command yet
  /// carry an earlier `received_at`.  Feeding that raw value to the coalescer
  /// would move an active window's quiescent deadline backward and flush it
  /// prematurely, so the coalescer arms from this max-clamped clock instead.
  coalesce_now: Instant,
  /// Dirty flag for the push-pull local-state snapshot (H6).
  ///
  /// Set whenever any of the three Lamport clocks, member status-ltimes,
  /// `left_members`, or the event buffer changes.  `drain_inner` calls
  /// `resync_local_state()` lazily when this is `true` so the inner
  /// `Endpoint` always ships a current serf snapshot on the next push-pull
  /// egress — not the snapshot from the last explicit sync.
  local_state_dirty: bool,
  /// Outbound exchange [`StreamId`]s serf started as an `ignore_old` join
  /// push/pull.
  ///
  /// When the driver issues an `ignore_old` join, the composing super-machine
  /// records the exchange's `StreamId` here (via
  /// [`Endpoint::note_ignore_join_stream`]) — the same `StreamId` that
  /// `start_push_pull` returned. The matching merge arrives as
  /// `RemoteStateReceived` carrying that exchange's `originating_stream_id`; when
  /// it equals a recorded entry the entry is consumed (one-shot) and
  /// `event_buffer.min_time` is bumped to the remote `event_ltime`, dropping the
  /// peer's pre-join user events (H8/G4 / Go serf `eventJoinIgnore`). A failed,
  /// timed-out, or dropped `ignore_old` join never merges, so the driver removes
  /// its `StreamId` (via [`Endpoint::clear_ignore_join_stream`]) when the join
  /// reaches its terminal — a stale entry can never leak.
  ///
  /// Keyed per-EXCHANGE, not per-peer: two joins to the SAME seed (e.g. an
  /// `ignore_old` join racing a `dispatch_join`) get distinct `StreamId`s, so
  /// only the `ignore_old` join's own merge is suppressed — whichever merge lands
  /// first can no longer consume the wrong token. The set is tiny (the in-flight
  /// `ignore_old` exchanges), so a `Vec` with linear lookup is the right
  /// structure.
  ignore_join_streams: Vec<StreamId>,
  /// The address of the most recently reconnect-dialled peer.
  ///
  /// Set by `fire_reconnect` each time a dial is initiated.  Test helpers
  /// expose this via `test_last_dial_addr` to assert which peer was chosen.
  /// Only meaningful in test builds; production code ignores this field.
  #[cfg(test)]
  last_dial_addr: Option<A>,
  /// All directed sends produced by `relay_response` in insertion order.
  ///
  /// Accumulated across repeated calls so tests can assert the full set of
  /// relay peers chosen across a single relay invocation (relay_factor > 1).
  /// Reset to empty by `test_clear_relay_directed_sends`.  Only present in
  /// test builds; production code does not carry this allocation.
  #[cfg(test)]
  relay_all_directed_sends: Vec<(A, Bytes)>,
}

// ── construction + cheap accessors ────────────────────────────────────────────

impl<I, A, R, D> Endpoint<I, A, R, D>
where
  I: Clone + Eq + core::hash::Hash,
  R: SeedableRng,
  D: DropCounter,
{
  /// Construct a serf `Endpoint` core using `opts` for serf-level knobs and
  /// `rng` as serf's own injected selection entropy.
  ///
  /// The core holds no transport; the composing super-machine pairs it with a
  /// memberlist coordinator that must already have been configured with
  /// `EndpointOptions::with_user_broadcast_tiers(NonZeroU8::new(3))` so that
  /// serf's three broadcast tiers (intent=0, query=1, event=2) are available.
  ///
  /// `rng` is **separate** from the coordinator's `R`.  Seed it from the
  /// driver's own entropy source; do not share the same `R` instance.
  ///
  /// The two coalescer shed counters start at `D::default()` (`0` for the
  /// default `u64`).  A driver that must observe the counts from a detached
  /// handle injects a shared backing via
  /// [`new_with_rng_in`](Self::new_with_rng_in) instead.
  pub fn new_with_rng(opts: Options, rng: R) -> Self
  where
    D: Default,
  {
    Self::new_with_rng_in(opts, rng, D::default(), D::default())
  }

  /// Construct a serf `Endpoint` core injecting the two coalescer shed counters
  /// `user_drop` / `member_drop`.
  ///
  /// An async driver mints a shared, read-observable backing (an atomic or a
  /// `Cell`), keeps a read-only clone on its handle, and passes the write-capable
  /// clones here so the handle observes every coalescer shed WITHOUT the endpoint
  /// publishing a copy each pump iteration.  The single-owner drivers use the
  /// `u64` default via [`new_with_rng`](Self::new_with_rng).
  pub fn new_with_rng_in(opts: Options, rng: R, user_drop: D, member_drop: D) -> Self {
    // Arm the first reap/reconnect/queue-check deadlines relative to the ORIGIN instant.
    // The driver calls handle_timeout(now) and the deadlines fire when now >= deadline.
    let first_reap = Instant::ORIGIN + opts.reap_interval();
    let first_reconnect = Instant::ORIGIN + opts.reconnect_interval();
    let first_queue_check = Instant::ORIGIN + opts.queue_check_interval();
    let event_buf_size = opts.event_buffer_size();
    let query_buf_size = opts.query_buffer_size();

    // Enable each coalescer iff its (period > 0 && quiescent > 0), else None
    // (disabled) — the default, which yields exact passthrough at the emission
    // sites so a machine built with default options behaves identically to one
    // with no coalescer at all.
    let member_coalescer = opts
      .member_coalesce_enabled()
      .then(|| MemberEventCoalescer::new(opts.coalesce_period(), opts.quiescent_period()));
    let user_coalescer = opts.user_coalesce_enabled().then(|| {
      UserEventCoalescer::new(
        opts.user_coalesce_period(),
        opts.user_quiescent_period(),
        opts.max_coalesced_user_events(),
      )
    });

    #[cfg(feature = "coordinates")]
    let coord_client: Option<crate::coordinate_client::CoordinateClient<I>> =
      if opts.disable_coordinates() {
        None
      } else {
        Some(crate::coordinate_client::CoordinateClient::new(
          crate::coordinate_client::CoordinateOptions::default(),
        ))
      };

    Self {
      opts,
      clock: 0,
      event_clock: 0,
      query_clock: 0,
      members: Members::default(),
      state: SerfState::Alive,
      rng,
      next_reap: Some(first_reap),
      next_reconnect: Some(first_reconnect),
      next_queue_check: Some(first_queue_check),
      leave_complete_deadline: None,
      event_buffer: EventBuffer::new(event_buf_size),
      query_buffer: QueryBuffer::new(query_buf_size),
      pending_queries: Vec::new(),
      received_queries: crate::FxHashMap::default(),
      pending_events: VecDeque::new(),
      member_coalescer,
      user_coalescer,
      user_drop,
      member_drop,
      reconnect_delegate: None,
      drain_now: Instant::ORIGIN,
      coalesce_now: Instant::ORIGIN,
      // The snapshot starts dirty so the first push-pull always ships a fresh
      // body even if no explicit API call has been made yet.
      local_state_dirty: true,
      ignore_join_streams: Vec::new(),
      #[cfg(feature = "coordinates")]
      coord_client,
      #[cfg(feature = "coordinates")]
      coord_cache: crate::FxHashMap::default(),
      #[cfg(test)]
      last_dial_addr: None,
      #[cfg(test)]
      last_directed_send: None,
      #[cfg(test)]
      rejoin_dials: Vec::new(),
      #[cfg(test)]
      relay_all_directed_sends: Vec::new(),
    }
  }

  /// Convenience constructor that seed serf's `R` with a zero seed.
  ///
  /// Suitable for tests and environments where determinism or an explicit seed
  /// is acceptable.  Production drivers should use `new_with_rng` and seed from
  /// a cryptographically-secure source.
  pub fn new(opts: Options) -> Self
  where
    D: Default,
  {
    Self::new_with_rng(opts, R::seed_from_u64(0))
  }

  // ── read accessors ────────────────────────────────────────────────────────

  /// The current lifecycle state of this serf endpoint.
  pub const fn state(&self) -> SerfState {
    self.state
  }

  /// The single post-[`Shutdown`](SerfState::Shutdown) command guard: `Ok(())`
  /// while the machine is live, `Err(Error::Shutdown)` once it has shut down.
  ///
  /// Losing an id-conflict vote forces the machine to `SerfState::Shutdown`
  /// (mirroring Go serf's conflict-loss `shutdown()`), after which every command
  /// that originates new cluster work funnels its lifecycle check through here so
  /// the contract is named in one place — the memberlist post-leave
  /// `ensure_running` precedent applied to serf's terminal state.  A shut-down
  /// machine additionally goes inert on ingress and quiet on its timers; only the
  /// already-buffered [`Event::Shutdown`] still drains via `poll_event`.  The
  /// driver owns stopping I/O and delivering that event.
  ///
  /// Gates only on `Shutdown`, not on every non-`Alive` state: `Leaving` and
  /// `Left` keep originating (matching Go serf, where the layer keeps running
  /// until `shutdown()`); a driver that wants a stricter post-leave policy
  /// enforces it in its own command gate.
  const fn ensure_not_shutdown(&self) -> Result<(), Error> {
    if matches!(self.state, SerfState::Shutdown) {
      Err(Error::Shutdown)
    } else {
      Ok(())
    }
  }

  /// The current member (SWIM membership) Lamport clock value.
  pub const fn member_time(&self) -> u64 {
    self.clock
  }

  /// The current user-event Lamport clock value.
  pub const fn event_time(&self) -> u64 {
    self.event_clock
  }

  /// The current query Lamport clock value.
  pub const fn query_time(&self) -> u64 {
    self.query_clock
  }

  /// Number of nodes currently tracked in the membership store.
  pub fn num_members(&self) -> usize {
    self.members.states.len()
  }

  /// Cumulative count of coalescing user events dropped because the user
  /// coalescer's buffered volume was at its configured cap
  /// ([`Options::max_coalesced_user_events`](crate::options::Options::max_coalesced_user_events)).
  ///
  /// Lifetime total, saturating, and never cleared — a flush or a `reset` does
  /// not reset it.  Returns `0` when user coalescing is disabled.
  pub fn coalesced_user_events_dropped(&self) -> u64 {
    self.user_drop.get()
  }

  /// Cumulative count of member changes dropped because the member coalescer's
  /// per-window map was at its cardinality cap.
  ///
  /// Lifetime total, saturating, and never cleared.  Returns `0` when member
  /// coalescing is disabled.
  pub fn coalesced_member_events_dropped(&self) -> u64 {
    self.member_drop.get()
  }

  /// Number of coalesced events currently waiting in the flush queue to be
  /// drained by [`poll_event`](Self::poll_event).
  pub fn pending_events_len(&self) -> usize {
    self.pending_events.len()
  }

  /// A snapshot of every tracked member (alive, leaving, left, or failed within
  /// the reap window) as owned [`Member`](crate::members::Member) values, for a
  /// driver's observable membership view published after each membership change.
  pub fn members_snapshot(&self) -> Vec<std::sync::Arc<crate::members::Member<I, A>>>
  where
    I: Clone,
    A: Clone,
  {
    self
      .members
      .states
      .values()
      .map(|ms| std::sync::Arc::new(ms.member().clone()))
      .collect()
  }
}

// ── reconnect delegate (minimal bounds: a plain injected-field setter) ────────

impl<I, A, R, D> Endpoint<I, A, R, D>
where
  I: Eq + core::hash::Hash,
  D: DropCounter,
{
  /// Install (or clear) the per-member reconnect-timeout override
  /// [`ReconnectDelegate`], consuming builder form.
  ///
  /// `None` (the default) is the noop: the flat configured `reconnect_timeout`
  /// / `tombstone_timeout` apply to every member in the reaper.
  #[must_use]
  pub fn with_reconnect_delegate(
    mut self,
    delegate: Option<std::boxed::Box<dyn ReconnectDelegate<I, A>>>,
  ) -> Self {
    self.reconnect_delegate = delegate;
    self
  }

  /// Install (or clear) the per-member reconnect-timeout override
  /// [`ReconnectDelegate`].
  ///
  /// `None` restores the default flat timeouts.
  pub fn set_reconnect_delegate(
    &mut self,
    delegate: Option<std::boxed::Box<dyn ReconnectDelegate<I, A>>>,
  ) {
    self.reconnect_delegate = delegate;
  }
}

// ── poll API (requires full Id + Data bounds for inner delegation) ─────────────

impl<I, A, R, D> Endpoint<I, A, R, D>
where
  I: Id + Clone,
  A: CheapClone + Data + PartialEq + Clone + 'static,
  R: Rng + SeedableRng,
  D: DropCounter,
{
  /// Drain one serf event.
  ///
  /// Pumps the coordinator `t` to exhaustion, sieving each inner `Event`
  /// into serf state, then returns the next queued serf `Event`.  Call in a
  /// loop until `None` before blocking.
  pub(crate) fn poll_event<T>(&mut self, t: &mut T) -> Option<Event<I, A>>
  where
    T: Reliable<I, A>,
  {
    self.drain_inner(t);
    self.pending_events.pop_front()
  }

  /// The earliest serf-level deadline requiring a `handle_timeout` call.
  ///
  /// Returns the minimum of serf's own periodic deadlines (reap, reconnect,
  /// queue-check, leave-complete, and pending-query closes).
  /// The composing super-machine folds in the coordinator's own deadline.
  ///
  /// A shut-down machine (lost id-conflict vote) schedules no wakeups: its
  /// deadlines fire no work (`after_inner_timeout` is inert once Shutdown), so
  /// surfacing them would spin the driver.  Mirrors memberlist's `poll_timeout`
  /// returning `None` once not Running.
  pub fn serf_poll_timeout(&self) -> Option<Instant> {
    if self.state.is_shutdown() {
      return None;
    }
    let query_min = self.pending_queries.iter().map(|pq| pq.deadline).min();
    // Fold in each enabled coalescer's flush deadline so the driver wakes to
    // flush a buffered member/user batch on time.
    let member_flush = self
      .member_coalescer
      .as_ref()
      .and_then(|c| c.flush_deadline());
    let user_flush = self
      .user_coalescer
      .as_ref()
      .and_then(|c| c.flush_deadline());
    [
      self.next_reap,
      self.next_reconnect,
      self.next_queue_check,
      self.leave_complete_deadline,
      query_min,
      member_flush,
      user_flush,
    ]
    .into_iter()
    .flatten()
    .min()
  }

  /// Sieve the coordinator's inner events after a transport ingress.
  ///
  /// The composing super-machine hands inbound bytes to its coordinator (which
  /// runs the SWIM machine and emits inner events: `UserPacket`,
  /// `RemoteStateReceived`, `NodeJoined`, …), then calls this to fold those
  /// inner events into serf state.  Latches `now` so inner events that need a
  /// wall-clock reference (e.g. `NodeLeft` leave-time) use the ingress instant.
  pub(crate) fn drain_after_ingress<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
  {
    self.drain_now = now;
    self.drain_inner(t);
  }

  /// Pre-inner-timer phase of the composed tick (H6).
  ///
  /// Latches `now` and, if the local-state snapshot is dirty, resyncs it so the
  /// coordinator echoes a fresh serf-clock / member-status snapshot on this
  /// tick's anti-entropy exchange rather than a snapshot from a previous tick.
  /// The dirty flag is cleared inside `resync_local_state` on success.
  ///
  /// The composing super-machine calls this, then drives the coordinator's own
  /// `handle_timeout(now)` (the SWIM gossip / probe / push-pull scheduler), then
  /// [`Endpoint::after_inner_timeout`].  This three-phase ordering keeps the
  /// load-bearing sequence (resync → inner timer → drain → serf deadlines)
  /// structural; the trait deliberately excludes the inner timer so the core
  /// cannot drive it directly.
  pub(crate) fn before_inner_timeout<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    // A shut-down machine (lost id-conflict vote) is quiet on its timers: no
    // snapshot resync, and `after_inner_timeout` fires no deadlines.
    if self.state.is_shutdown() {
      return;
    }
    self.drain_now = now;
    if self.local_state_dirty {
      self.resync_local_state(t);
    }
  }

  /// Post-inner-timer phase of the composed tick: drain inner events then fire
  /// serf's own deadlines.
  ///
  /// Tick order:
  ///
  /// 1. Drain all inner events produced by the coordinator's timer via
  ///    `drain_inner`, processing NodeJoined / NodeLeft / UserPacket / etc.
  ///    through the serf sieve **before** any serf deadline fires.
  /// 2. Fire serf's own deadlines: reap → reconnect → queue-check →
  ///    query-closes → leave-complete.
  ///
  /// **Why this order:** firing serf deadlines after draining the inner's events
  /// prevents tombstoning a member whose `NodeJoined` event is still queued in
  /// the inner machine at the time the reap deadline would otherwise fire.
  /// Without this ordering, a member that reconnects exactly at the reap
  /// boundary could be incorrectly pruned.
  pub(crate) fn after_inner_timeout<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    // A shut-down machine (lost id-conflict vote) fires no serf deadlines: no
    // reap, reconnect, queue-check, query-close, or leave-completion, and its
    // ingress drain is inert.  Mirrors the memberlist post-leave `handle_timeout`
    // early-out.  The transition itself happens inside this method (a lost
    // conflict close), so the gate quiets every *subsequent* tick, not the one
    // that shut the machine down.
    if self.state.is_shutdown() {
      return;
    }
    self.drain_now = now;

    // Step 1: drain all inner events produced by the tick through the serf sieve.
    // NodeJoined / NodeLeft / etc. are processed NOW, before any serf deadline fires.
    self.drain_inner(t);

    // Step 2: fire serf's own deadlines in deterministic order.

    // Reaper: remove tombstoned left/failed nodes and stale intents.
    if let Some(dl) = self.next_reap {
      if now >= dl {
        self.fire_reap(now);
        self.next_reap = Some(now + self.opts.reap_interval());
      }
    }

    // Reconnector: probabilistically re-dial a random failed peer.
    if let Some(dl) = self.next_reconnect {
      if now >= dl {
        self.fire_reconnect(t, now);
        self.next_reconnect = Some(now + self.opts.reconnect_interval());
      }
    }

    // Queue-check: re-arm the deadline. The depth gate in rebroadcast() prevents
    // enqueue past queue_max; this periodic tick is retained for future telemetry.
    if let Some(dl) = self.next_queue_check {
      if now >= dl {
        self.next_queue_check = Some(now + self.opts.queue_check_interval());
      }
    }

    // Query-close: tally and expire any pending queries whose deadline elapsed.
    self.fire_due_query_closes(now);

    // A lost id-conflict vote in the query-close pass transitions the machine to
    // Shutdown. Nothing may follow the terminal Event::Shutdown, so skip the rest
    // of this pass's deadline work (received-query prune, leave-complete
    // transition): it would prune protocol state or transition after the terminal
    // event. `serf_poll_timeout` returns `None` once Shutdown, so the un-cleared
    // leave-complete deadline never respins the driver.
    if self.state.is_shutdown() {
      return;
    }

    // Prune expired received-query tokens.  Entries for which respond() succeeded
    // are removed there; this catches those whose deadline elapsed without a
    // respond() call (driver missed the response window).
    self.prune_expired_received_queries(now);

    // Leave-complete: Leaving → Left after inner LeftCluster + leave_propagate_delay.
    if let Some(dl) = self.leave_complete_deadline {
      if now >= dl {
        self.leave_complete_deadline = None;
        if self.state == SerfState::Leaving {
          self.state = SerfState::Left;
          self.pending_events.push_back(Event::LeftCluster);
        }
        // If already Shutdown: the leave chain was interrupted; no transition or event.
      }
    }

    // Flush any coalescer whose window has closed, delivering the coalesced batch
    // via `pending_events`.  A member/user event fed earlier this tick (during
    // `drain_inner` / `fire_reap`) arms a future deadline, so it is NOT flushed
    // now — only a window armed on a PRIOR tick that has since elapsed flushes
    // here.  Placed after the Shutdown gate above: a machine that lost its
    // conflict vote this tick has already dropped its buffered batch (in
    // `close_conflict_query`) and returned early, so nothing is flushed after the
    // terminal Event::Shutdown.
    self.flush_due_coalescers(now);
  }

  /// Flush each enabled coalescer whose window has closed at `now` into
  /// `pending_events`.
  fn flush_due_coalescers(&mut self, now: Instant) {
    let now = self.coalesce_now.max(now);
    self.coalesce_now = now;
    if let Some(c) = self.member_coalescer.as_mut() {
      if c.due(now) {
        c.flush(&mut self.pending_events);
      }
    }
    if let Some(c) = self.user_coalescer.as_mut() {
      if c.due(now) {
        c.flush(&mut self.pending_events);
      }
    }
  }

  /// Drop received-query tokens that are strictly past their response deadline.
  ///
  /// An entry is pruned only once `now > deadline` — exactly the point at which
  /// `respond` / `respond_key` stop accepting a response for it, since their
  /// deadline guard rejects only `now > deadline` and a response sent at the
  /// exact instant `now == deadline` is still valid.  Retaining while
  /// `now <= deadline` therefore never evicts a token a pending response could
  /// still answer, while a strictly-past token is unanswerable so freeing its
  /// inbound-cap slot loses nothing.  Single-sources the predicate shared by the
  /// periodic reclaim in `after_inner_timeout` and the inline reclaim in
  /// `handle_query` (which runs before the inbound overflow cap so the cap counts
  /// only answerable entries).
  fn prune_expired_received_queries(&mut self, now: Instant) {
    self.received_queries.retain(|_, rq| now <= rq.deadline);
  }

  // ── inner-event sieve ─────────────────────────────────────────────────────

  /// Pump the coordinator `t` to exhaustion, routing each inner event through
  /// the serf sieve.
  ///
  /// After all inner events are drained, if the local-state snapshot is dirty
  /// (H6), `resync_local_state` is called so the next push-pull egress ships
  /// the current serf clock / member-status state, not a stale snapshot.
  fn drain_inner<T>(&mut self, t: &mut T)
  where
    T: Reliable<I, A>,
  {
    while let Some(ev) = t.poll_inner_event() {
      self.on_inner_event(t, ev);
    }
    if self.local_state_dirty {
      self.resync_local_state(t);
    }
  }

  /// Dispatch a single inner `memberlist_proto::Event` to the matching serf
  /// handler.
  ///
  /// Every variant of the inner `Event` enum is covered (totality / H4).
  fn on_inner_event<T>(&mut self, t: &mut T, ev: memberlist_proto::Event<I, A>)
  where
    T: Reliable<I, A>,
  {
    // Ingress chokepoint: a shut-down machine (lost id-conflict vote) is inert on
    // ingress — no inbound inner event mutates serf state, rebroadcasts, or emits
    // any event beyond the already-buffered Event::Shutdown.  Gating the dispatch
    // itself (before the match, every caller) mirrors the memberlist post-leave
    // handle_packet gate, which returns early before its own message match once
    // not Running.
    if self.state.is_shutdown() {
      return;
    }
    use memberlist_proto::Event as IE;
    match ev {
      // ── membership ───────────────────────────────────────────────────────
      IE::NodeJoined(node) => {
        let now = self.drain_now;
        self.handle_node_join(&node, now);
      }
      IE::NodeLeft(node) => {
        // now is not available here; use the stored now from the most-recent
        // handle_timeout/handle_packet call. For correctness the sieve callers
        // thread `now` through drain_inner; at this sub-stage we carry it via
        // a field set before the drain.  See `drain_inner_at`.
        self.handle_node_leave(&node, self.drain_now);
      }
      IE::NodeUpdated(node) => {
        self.handle_node_update(&node);
      }
      IE::NodeConflict(_c) => {
        if self.opts.enable_id_conflict_resolution() {
          let now = self.drain_now;
          self.resolve_node_conflict(t, now);
        }
      }

      // ── user gossip ───────────────────────────────────────────────────────
      IE::UserPacket(p) => {
        // `drain_now` was latched by the ingress entry point (handle_packet /
        // handle_transport_data / handle_timeout) before drain_inner was called,
        // so it is always a fresh `now` for the current call site.
        let now = self.drain_now;
        let (from, data, _reliability) = p.into_parts();
        self.handle_user_packet(t, from, data, now);
      }
      IE::RemoteStateReceived(r) => {
        // Correlate the merge to the exchange that produced it by its
        // `originating_stream_id` — for an outbound join this is exactly the
        // `StreamId` `start_push_pull` returned and that the driver recorded.
        let sid = r.originating_stream_id();
        let (_peer, user_data, is_join) = r.into_parts();
        if !user_data.is_empty() {
          // Consume the one-shot ignore-join entry for THIS exchange iff this is
          // a join: the `ignore_old` join's own merge suppresses its pre-join
          // user events. A refresh, or any other exchange to the same peer (a
          // concurrent `dispatch_join`, or an inbound join we did not ignore),
          // carries a different `StreamId` and leaves the set untouched.
          let suppress = is_join && self.consume_ignore_join_stream(sid);
          self.merge_remote_state(t, user_data, suppress);
        }
      }

      // ── lifecycle ────────────────────────────────────────────────────────
      IE::LeftCluster => {
        // The inner memberlist has finished broadcasting the dead-self fan-out.
        // If we are in the middle of a graceful leave, arm the propagation
        // delay deadline.  The actual Leaving → Left transition fires in
        // handle_timeout when the deadline elapses.
        //
        // Use the minimum of any already-armed deadline and the new one so
        // that a repeated LeftCluster (e.g. from inner.handle_timeout during
        // the propagation window) does not push the deadline forward — the
        // first-fired deadline is the earliest and therefore the binding one.
        if self.state == SerfState::Leaving {
          let delay = self.opts.leave_propagate_delay();
          let deadline = self.drain_now + delay;
          self.leave_complete_deadline = Some(match self.leave_complete_deadline {
            Some(existing) => existing.min(deadline),
            None => deadline,
          });
        }
      }

      // ── coordinates ───────────────────────────────────────────────────────
      IE::PingCompleted(p) => {
        // G9: on every successful probe round-trip, feed the RTT and the
        // remote peer's piggybacked coordinate into the local Vivaldi model,
        // then refresh the ack payload so the next ack ships the updated
        // local coordinate.  No-op when the `coordinates` feature is disabled
        // or when coordinates were disabled at construction.
        #[cfg(feature = "coordinates")]
        {
          let node_id = p.node_ref().id_ref().clone();
          let rtt = p.rtt();
          let payload = p.payload_ref().clone();
          self.handle_ping_completed(t, &node_id, rtt, &payload);
        }
        // When the feature is disabled, suppress the unused-variable warning.
        #[cfg(not(feature = "coordinates"))]
        let _ = p;
      }

      // ── drop with no serf-level action ───────────────────────────────────
      // PingFailed: no serf action; the inner already records the failure.
      IE::PingFailed(_) => {}
      // Surface the terminal exchange outcome so drivers awaiting a Join
      // push/pull (or any other reliable exchange) can resolve without
      // inferring completion from membership-state side effects.
      IE::ExchangeCompleted(p) => {
        // An ignore-join's `StreamId` is NOT cleaned here: `ExchangeCompleted`
        // carries the coordinator-allocated `eid`, which on the stream backend
        // is a separate domain from the `StreamId` the ignore set is keyed by.
        // The driver clears a terminated `ignore_old` join's `StreamId` from its
        // own per-join bookkeeping instead (see `clear_ignore_join_stream`).
        self.pending_events.push_back(Event::ExchangeCompleted(p));
      }
      // DecodeError: the inner has already logged / tracked the error;
      // serf takes no action on undecodable inner messages.
      IE::DecodeError(_) => {}

      // ── reconnect / dial passthrough ─────────────────────────────────────
      // The reliable coordinator (`StreamEndpoint`/`QuicEndpoint`) sieves the
      // inner `DialRequested` into its own dial queue and dials itself, so this
      // event never reaches serf's drain over a real coordinator.  The arm
      // remains for totality over `memberlist_proto::Event` and to re-emit a
      // serf-level passthrough for any transport that does surface the dial to
      // serf (the driver then performs the dial and reports back via
      // dial_succeeded / dial_failed).
      IE::DialRequested(d) => {
        let (stream_id, peer, _deadline) = d.into_parts();
        // Track the dialled address for test assertions.
        #[cfg(test)]
        {
          self.last_dial_addr = Some(peer.clone());
        }
        self
          .pending_events
          .push_back(Event::DialRequested(DialPassthrough::new(stream_id, peer)));
      }
    }
  }

  // ── push-pull local-state synthesis (H6) ────────────────────────────────

  /// Mark the push-pull local-state snapshot as needing re-synthesis.
  ///
  /// Called at every site that mutates the three Lamport clocks, member
  /// status-ltimes, `left_members`, or the event buffer.  The actual
  /// re-synthesis is deferred to the end of `drain_inner` so that a burst
  /// of mutations within one call produces only one encode+push cycle.
  #[inline]
  fn mark_local_state_dirty(&mut self) {
    self.local_state_dirty = true;
  }

  /// Synthesise the push-pull body and push it to the inner Endpoint.
  ///
  /// Builds a `PushPullMessage` from the current machine state — three
  /// Lamport clocks, per-member `status_ltime`, `left_members` id list, and
  /// the event ring buffer — then encodes it via `AnyMessage::encode` and
  /// calls `inner.set_local_state_snapshot`.  On encode or snapshot-cap
  /// errors the dirty flag is left set so the next drain attempt retries
  /// (a transient alloc failure should not permanently corrupt the snapshot).
  ///
  /// Mirrors Go serf `delegate.go` `local_state` (~line 386).
  pub(crate) fn resync_local_state<T>(&mut self, t: &mut T)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Data,
  {
    // Egress chokepoint: a shut-down machine (lost id-conflict vote) synthesises
    // no push-pull snapshot.  `fire_reap` can mark the snapshot dirty in the same
    // tick that a lost conflict close shuts the machine down, so this guard keeps
    // the deferred resync (in `drain_inner`) from pushing a fresh snapshot to the
    // coordinator after shutdown; the driver owns tearing the coordinator down.
    if self.state.is_shutdown() {
      return;
    }

    // Gather status_ltimes from the membership store.
    // HashMap iteration order is arbitrary, so collect first then sort by the
    // stable encoded id bytes so two machines with identical membership always
    // produce byte-identical PushPullMessage wire output.
    let mut status_ltimes: Vec<(I, LamportTime)> = self
      .members
      .states
      .iter()
      .map(|(id, ms)| (id.clone(), ms.status_time()))
      .collect();
    status_ltimes.sort_unstable_by(|(a, _), (b, _)| {
      let a_bytes = a.encode_to_vec().unwrap_or_default();
      let b_bytes = b.encode_to_vec().unwrap_or_default();
      a_bytes.cmp(&b_bytes)
    });

    // Gather left_members id list (the oracle sends node ids, not MemberState).
    let left_members: Vec<I> = self
      .members
      .left_members
      .iter()
      .filter_map(|id| self.members.states.get(id).map(|_| id.clone()))
      .collect();

    // Collect the event ring buffer (non-None slots).
    let events: Vec<UserEvents> = self
      .event_buffer
      .buffer
      .iter()
      .filter_map(|slot| slot.clone())
      .collect();

    let pp = PushPullMessage::new(
      LamportTime(self.clock),
      status_ltimes,
      left_members,
      LamportTime(self.event_clock),
      events,
      LamportTime(self.query_clock),
    );

    // Encode via AnyMessage so the wire framing is applied consistently.
    // AnyMessage::PushPull does not depend on `A`, but the type parameter is
    // required for the enum variant; we supply `A` from the impl bound.
    let encoded = match AnyMessage::<I, A>::PushPull(pp).encode() {
      Ok(b) => b,
      Err(_) => {
        // Encoding failure: leave dirty so the next drain retries.
        // A snapshot that cannot be encoded should not crash the machine.
        return;
      }
    };

    // Push to the coordinator's inner Endpoint.  On cap-exceeded errors keep
    // dirty for retry; the operator must raise max_stream_frame_size if the serf
    // state is too large to fit in one push-pull frame.  The snapshot is stale
    // but the machine continues operating — the next drain attempt will retry.
    if t.set_local_state_snapshot(encoded).is_ok() {
      self.local_state_dirty = false;
    }
  }

  /// Record outbound exchange `id` as an `ignore_old` join.
  ///
  /// The composing super-machine calls this when the driver starts an
  /// `ignore_old` join push/pull, passing the `StreamId` `start_push_pull`
  /// returned, so the matching merge (whose `originating_stream_id` equals `id`)
  /// bumps `event_buffer.min_time` and suppresses replay of the peer's pre-join
  /// user events (H8/G4). The entry is one-shot: it is consumed at the join merge
  /// (see [`Self::consume_ignore_join_stream`]) or removed by the driver via
  /// [`Self::clear_ignore_join_stream`] when the join terminates without a merge.
  /// Idempotent — a duplicate `StreamId` is not stored twice.
  pub(crate) fn note_ignore_join_stream(&mut self, id: StreamId) {
    if !self.ignore_join_streams.contains(&id) {
      self.ignore_join_streams.push(id);
    }
  }

  /// Consume the one-shot ignore-join entry for exchange `id`, returning whether
  /// one was present.
  ///
  /// Called on a join push/pull merge whose `originating_stream_id` is `id`
  /// (suppression is applied iff this returns `true`). Removing the entry here is
  /// what makes the ignore per-exchange one-shot: a second merge on the same
  /// stream, or any other exchange to the same peer, is unaffected.
  fn consume_ignore_join_stream(&mut self, id: StreamId) -> bool {
    if let Some(idx) = self.ignore_join_streams.iter().position(|s| *s == id) {
      self.ignore_join_streams.swap_remove(idx);
      true
    } else {
      false
    }
  }

  /// Remove exchange `id` from the ignore-join set without applying suppression.
  ///
  /// The driver calls this when an `ignore_old` join reaches its terminal without
  /// a merge having consumed the entry (dial failure, timeout, empty push/pull
  /// body, or a dropped join future), so a stale `StreamId` can never linger.
  /// Idempotent: a `StreamId` the merge already consumed on the success path is
  /// simply absent, making this a no-op there. `ExchangeCompleted` cannot drive
  /// this on the stream backend because its `eid` is a different domain from the
  /// `StreamId`, so the cleanup is driver-side per-join bookkeeping.
  pub(crate) fn clear_ignore_join_stream(&mut self, id: StreamId) {
    self.consume_ignore_join_stream(id);
  }

  /// Update the local node's tags, re-advertise them via the coordinator, and
  /// synchronously refresh the local member in the membership store.
  ///
  /// The coordinator queues an Alive/NodeUpdated broadcast so peers learn the
  /// new metadata; the corresponding `NodeUpdated` event arrives later via
  /// `poll_event` and is idempotent (it re-applies the same tags via the normal
  /// `handle_node_update` path).  The synchronous refresh here means
  /// tag-filtered local queries (`Filter::Tag` in `should_process_query`) see
  /// the new tags immediately, without waiting for that event.
  ///
  /// If the local node is not yet in `members.states` (the local `NodeJoined`
  /// has not been drained yet), the in-place refresh is skipped.  Materializing
  /// a phantom entry here would cause the queued `NodeUpdated` to emit a
  /// `Member(Update)` event before the imminent `Member(Join)`, violating the
  /// Join-before-Update ordering consumers rely on.  When `NodeJoined` does
  /// arrive it creates the member with tags decoded from the `Meta` set by
  /// `update_meta`, and with the correct status/ltime from any buffered intent.
  /// `handle_node_update` already suppresses `NodeUpdated` for absent members,
  /// so no spurious event reaches the driver in this path.
  ///
  /// Tags are not part of the push-pull snapshot (only Lamport clocks,
  /// per-member status times, `left_members`, and the event ring are), so
  /// `set_tags` never marks the snapshot dirty.  Mirrors Go serf
  /// `base.go` `SetTags`.
  ///
  /// # Errors
  ///
  /// Returns [`Error::SetTagsMeta`] if the encoded tag map exceeds
  /// `Meta::MAX_SIZE` or the coordinator's configured `meta_max_size`.
  pub(crate) fn set_tags<T>(&mut self, t: &mut T, tags: Tags, now: Instant) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    use buffa::Message as _;

    // Latch the command's instant so the coordinator's resulting `NodeUpdated`,
    // drained synchronously at the end of this call, arms the member coalescer
    // from live `now` rather than a stale `drain_now`.
    self.drain_now = now;

    // Refuse once the machine has shut down (lost id-conflict vote).
    self.ensure_not_shutdown()?;

    let pb_tags = tags_to_pb(&tags);
    let encoded = pb_tags.encode_to_vec();
    // `Meta::try_from` only fails when the encoded size exceeds `Meta::MAX_SIZE`
    // (u16::MAX bytes). Surface this as `SetTagsMeta` rather than panicking so
    // the driver can log and retry with fewer or shorter tags.
    let n = encoded.len();
    let meta = Meta::try_from(encoded).map_err(|_| {
      Error::SetTagsMeta(memberlist_proto::Error::MetaExceedsCap(
        memberlist_proto::SizeExceeded::new(n, Meta::MAX_SIZE),
      ))
    })?;
    t.update_meta(meta).map_err(Error::SetTagsMeta)?;

    // Refresh the local member's tags in place if it is already present, so
    // tag-filtered local queries see the new tags before the coordinator's
    // NodeUpdated event is drained. If the local member is not yet present
    // (set_tags before the local NodeJoined), do nothing: materializing it here
    // would surface a Member(Update) before the Member(Join), and the join will
    // create the member with these tags (decoded from the meta set above) and the
    // correct status/ltime from any buffered intent.
    if let Some(ms) = self.members.states.get_mut(t.endpoint_ref().local_id_ref()) {
      let node = ms.member().node().clone();
      let status = ms.status();
      *ms.member_mut() = Member::new(node, tags, status);
    }

    // Emit the resulting NodeUpdated synchronously under the freshly latched
    // `now`, mirroring how `user_event` processes its event inline. Deferring it
    // to a later `poll_event` drain would let an intervening ingress or timeout
    // overwrite `drain_now`, arming the member coalescer from the wrong instant.
    self.drain_inner(t);

    Ok(())
  }

  // ── push-pull ingress replay (H8/G2, G3, G4) ────────────────────────────

  /// Replay a remote push-pull body received via `RemoteStateReceived`.
  ///
  /// Mirrors Go serf `delegate.go` `merge_remote_state` (~line 427).
  ///
  /// **G2 — witness all three clocks at `ltime - 1`, guarded `> 0`:**
  /// Each of the remote's three clocks is witnessed at `value - 1`.  The
  /// subtraction ensures that no message stamped at the remote's current
  /// value is treated as *already seen* — it would need to be received
  /// first before the local clock passes it.  Guard: only if `value > 0`
  /// (witnessing at `LamportTime::MAX` would overflow).
  ///
  /// **G3 — process `left_members` FIRST, then join intents, skipping lefts:**
  /// Left nodes must be installed before the join pass so that a node that
  /// appears in both `status_ltimes` and `left_members` ends up as `Left`,
  /// not `Alive`.  The synthetic leave ltime for a left node is
  /// `status_ltimes[id] + 1` (the leave is necessarily after the last known
  /// join time; using `+1` avoids a stale-intent rejection while keeping
  /// the causal ordering intact).
  ///
  /// **G4 — `eventJoinIgnore` bumps `event_buffer.min_time`:**
  /// If `suppress_pre_join_events` is set (the caller saw a join push/pull from
  /// a peer it started an `ignore_old` join against and consumed the one-shot
  /// per-peer entry), the `event_buffer.min_time` is raised to
  /// `max(min_time, event_ltime)`. This suppresses all buffered user events
  /// from the remote peer that pre-date the current event-clock (prevents
  /// re-emitting stale events on a fresh join exchange).
  ///
  /// After the intent passes, every buffered user event in the push-pull
  /// body is replayed via `handle_user_event`.  Events older than
  /// `min_time` or already deduped in the ring are silently dropped
  /// (handled inside `handle_user_event` / `EventBuffer::witness_event`).
  ///
  /// Decoding errors in the `user_data` bytes are silently dropped —
  /// the machine must not panic on bad network input.
  fn merge_remote_state<T>(&mut self, t: &mut T, user_data: Bytes, suppress_pre_join_events: bool)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Data,
  {
    // Exact-consumption decode: the buffer must hold exactly one serf frame.
    // A valid-prefix payload with trailing bytes (e.g. a PushPull frame followed
    // by junk) must be dropped before any state mutation — trailing junk would
    // otherwise allow clock witnessing, intent application, and dirty-marking
    // while bypassing the semantic validity of the whole frame.
    let pp = match AnyMessage::<I, A>::decode_with_consumed(&user_data) {
      Ok((AnyMessage::PushPull(pp), consumed)) if consumed == user_data.len() => pp,
      _ => return,
    };

    let ltime = pp.ltime.0;
    let event_ltime = pp.event_ltime.0;
    let query_ltime = pp.query_ltime.0;

    // Whole-message drop gate for push-pull: if ANY of the three top-level
    // Lamport clocks is unacceptable (u64::MAX or u64::MAX-1), drop the ENTIRE
    // push-pull message — no mark_local_state_dirty, no status_ltimes
    // processing, no left_members processing, no event replay.  A single
    // adversarial clock in a push-pull must not let the rest of the body apply.
    //
    // G2 witness uses `value - 1` so we need `value > 0` as an additional guard
    // to avoid underflow; non-zero is checked inside the witness blocks below.
    if !ltime_is_acceptable(ltime)
      || !ltime_is_acceptable(event_ltime)
      || !ltime_is_acceptable(query_ltime)
    {
      return;
    }

    // G2: witness all three clocks at remote_value - 1, guarded > 0.
    // Capture clock values before witnessing to detect whether any advanced.
    let clock_before = self.clock;
    let event_clock_before = self.event_clock;
    let query_clock_before = self.query_clock;
    if ltime > 0 {
      witness(&mut self.clock, ltime - 1);
    }
    if event_ltime > 0 {
      witness(&mut self.event_clock, event_ltime - 1);
    }
    if query_ltime > 0 {
      witness(&mut self.query_clock, query_ltime - 1);
    }
    // Mark dirty only if a clock actually advanced (stale push-pulls whose three
    // clocks are all <= current are no-ops and must not force a resync).
    if self.clock != clock_before
      || self.event_clock != event_clock_before
      || self.query_clock != query_clock_before
    {
      self.mark_local_state_dirty();
    }

    // Build a fast lookup map from the status_ltimes Vec so we can look up a
    // node's ltime when processing left_members (Vec is the wire type; HashMap
    // is the lookup we need — O(n) build, O(1) lookup per left entry).
    // Entries whose ltime is not acceptable are silently excluded — they would
    // write a permanent status_time tombstone that no finite join intent can
    // ever outrank.
    let status_map: crate::FxHashMap<&I, LamportTime> = pp
      .status_ltimes
      .iter()
      .filter(|(_, lt)| ltime_is_acceptable(lt.0))
      .map(|(id, lt)| (id, *lt))
      .collect();

    // Build a fast lookup set for left_members so the join pass can skip them.
    let left_set: crate::FxHashSet<&I> = pp.left_members.iter().collect();

    let now = self.drain_now;

    // G3a: process left_members first as synthetic leave intents.
    // The synthetic ltime = status_ltimes[id] + 1 (leave is causally after join).
    // Nodes absent from status_map (because their ltime was u64::MAX or missing)
    // are silently skipped — the oracle logs an error and skips; we do the same.
    // handle_node_leave_intent calls mark_local_state_dirty() when it applies a change.
    for node_id in &pp.left_members {
      if let Some(&status_ltime) = status_map.get(node_id) {
        let leave_ltime = LamportTime(status_ltime.0.saturating_add(1));
        self.handle_node_leave_intent(t, leave_ltime, node_id, false, now);
      }
    }

    // G3b: process status_ltimes as join intents, SKIPPING left nodes.
    // status_map already excludes u64::MAX entries, so handle_node_join_intent's
    // own u64::MAX gate is a redundant safety net here.
    // handle_node_join_intent calls mark_local_state_dirty() when it applies a change.
    for (node_id, ltime) in &pp.status_ltimes {
      if left_set.contains(node_id) {
        continue;
      }
      self.handle_node_join_intent(*ltime, node_id, now);
    }

    // G4: when this join push/pull came from a peer we started an `ignore_old`
    // join against (the caller consumed the one-shot per-peer entry and passed
    // `suppress_pre_join_events`), bump event_buffer.min_time to
    // max(min_time, remote event_ltime).  This prevents the peer's pre-join
    // events from being replayed on a fresh join exchange.
    if suppress_pre_join_events && event_ltime > self.event_buffer.min_time {
      self.event_buffer.min_time = event_ltime;
      self.mark_local_state_dirty();
    }

    // Replay every buffered user event from the remote push-pull body.
    // handle_user_event applies the min_time gate and dedup ring check,
    // so stale or already-seen events are silently discarded.
    // handle_user_event calls mark_local_state_dirty() when it accepts a new event.
    for user_events in pp.events {
      for ev in user_events.events {
        let msg = UserEventMessage {
          ltime: user_events.ltime,
          cc: false,
          name: ev.name,
          payload: ev.payload,
        };
        self.handle_user_event(msg);
      }
    }
  }

  // ── event emission (coalesce-or-passthrough) ──────────────────────────────

  /// Emit a batch of membership changes of one `kind`.
  ///
  /// When the member coalescer is enabled the batch is fed into it (buffered,
  /// deduped to the latest status per node, and flushed later once its window
  /// closes in `after_inner_timeout`); otherwise it is pushed straight to
  /// `pending_events` — the exact passthrough a machine with member coalescing
  /// disabled (the default) performs.  The feed is armed at `self.drain_now`,
  /// the freshest instant the machine has latched.
  fn emit_member(&mut self, kind: MemberEventKind, members: Vec<Member<I, A>>) {
    let now = self.coalesce_now.max(self.drain_now);
    self.coalesce_now = now;
    if let Some(c) = self.member_coalescer.as_mut() {
      // The window may have elapsed while the driver was busy and has not yet
      // fired the overdue flush timer. Flush the completed batch before the new
      // event mutates it — otherwise a feed after the deadline would overwrite a
      // due observation and extend the window, merging two separate windows and
      // dropping the earlier one.
      if c.due(now) {
        c.flush(&mut self.pending_events);
      }
      c.feed(kind, members, now, &mut self.member_drop);
    } else {
      self
        .pending_events
        .push_back(Event::Member(MemberEvent::new(kind, members)));
    }
  }

  /// Emit a user event.
  ///
  /// A coalescing user event (`cc == true`) is fed to the user coalescer when it
  /// is enabled; every other case (a non-coalescing event, or the coalescer
  /// disabled) passes straight through to `pending_events`.  Mirrors the legacy
  /// coalescer's `handle` predicate (`CrateEvent::User(e) => e.cc()`): only
  /// coalescable user events are buffered.
  fn emit_user(&mut self, msg: UserEventMessage) {
    let now = self.coalesce_now.max(self.drain_now);
    self.coalesce_now = now;
    if msg.cc {
      if let Some(c) = self.user_coalescer.as_mut() {
        // Flush an elapsed-but-not-yet-fired window before the new event mutates
        // it (see emit_member): a newer generation fed after the deadline would
        // otherwise supersede and drop a due earlier generation.
        if c.due(now) {
          c.flush(&mut self.pending_events);
        }
        c.feed(msg, now, &mut self.user_drop);
        return;
      }
    }
    self.pending_events.push_back(Event::User(msg));
  }

  // ── member-status FSM handlers ───────────────────────────────────────────

  /// Handle an inner `NodeJoined` event.
  ///
  /// If the node is already known and was previously Failed or Left, clears it
  /// from those lists, resets `leave_time`, and sets status to Alive.  For a
  /// brand-new node, consults the recent-intent buffer: a pending Leave intent
  /// sets initial status to `Leaving`; a Join intent (only) or no intent gives
  /// `Alive`.  Emits `Event::Member(Join)` in all cases.
  ///
  /// Mirrors Go serf `base.go` `handleNodeJoin` (lines 1213-1341).
  fn handle_node_join(&mut self, node: &NodeState<I, A>, _now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    // Decode tags from the node's meta bytes.  Go serf's oracle returns early
    // on a tag-decode failure, silently dropping the join — a footgun for
    // callers who injected a node they expect to appear.  The Sans-I/O machine
    // cannot skip the join without emitting the event, so on decode failure we
    // fall back to empty tags and proceed.
    let tags = if node.meta_ref().is_empty() {
      Tags::new()
    } else {
      // Tags is a HashMap<SmolStr, SmolStr> in serf-proto.  The memberlist
      // meta field carries the serialised tags; at this stage we decode them
      // via the protobuf framing helpers.  If decoding fails, fall back to
      // empty tags (forward-compat).
      decode_tags_from_meta(node.meta_ref().as_bytes()).unwrap_or_default()
    };

    let id = node.id_ref();
    let n = node.node();

    let (old_status, new_state) = if let Some(ms) = self.members.states.get_mut(id) {
      let old = ms.status();
      // Keep existing status_time when re-joining.
      let st = ms.status_time();
      ms.set_status(MemberStatus::Alive);
      ms.set_leave_time(None);
      *ms.member_mut() = Member::new(n.clone(), tags, MemberStatus::Alive);
      ms.set_status_time(st);
      (old, None)
    } else {
      // New node: check for a pending intent in recent_intents.
      // The Leave intent takes priority over Join if both somehow coexist
      // (the oracle checks Join then Leave, with Leave winning because it
      // is checked second and overwrites status_ltime).
      let (status, status_ltime) = {
        let mut s = MemberStatus::Alive;
        let mut lt = LamportTime::ZERO;
        if let Some(t) = self.members.recent_intent(id, IntentKind::Join) {
          lt = t;
        }
        if let Some(t) = self.members.recent_intent(id, IntentKind::Leave) {
          lt = t;
          s = MemberStatus::Leaving;
        }
        (s, lt)
      };
      let ms = MemberState::new(Member::new(n.clone(), tags, status), status_ltime, None);
      (MemberStatus::None, Some(ms))
    };

    if let Some(ms) = new_state {
      self.members.states.insert(id.clone(), ms);
    }

    // Clear from failed/left lists when re-joining after failure or leave.
    if matches!(old_status, MemberStatus::Failed | MemberStatus::Left) {
      remove_old_member(&mut self.members.failed_members, id);
      remove_old_member(&mut self.members.left_members, id);
    }

    // Membership changed — snapshot is stale.
    self.mark_local_state_dirty();

    // Always emit Member(Join).
    let member = self.members.states[id].member().clone();
    self.emit_member(MemberEventKind::Join, vec![member]);
  }

  /// Handle an inner `NodeLeft` event.
  ///
  /// `Leaving` → `Left` (pushes to `left_members`, emits Leave).
  /// `Alive` → `Failed` (pushes to `failed_members`, emits Failed).
  /// Any other status is a no-op (mirrors the oracle's `_` arm at base.go:1410).
  ///
  /// Mirrors Go serf `base.go` `handleNodeLeave` (lines 1382-1447).
  fn handle_node_leave(&mut self, node: &NodeState<I, A>, now: Instant)
  where
    I: Clone,
  {
    let id = node.id_ref();
    let Some(ms) = self.members.states.get_mut(id) else {
      return;
    };

    let current = ms.status();
    let (new_status, event_kind) = match current {
      MemberStatus::Leaving => (MemberStatus::Left, MemberEventKind::Leave),
      MemberStatus::Alive => (MemberStatus::Failed, MemberEventKind::Failed),
      // Any other status (None, Left, Failed) is a no-op per the oracle.
      _ => return,
    };

    ms.set_status(new_status);
    ms.set_leave_time(Some(now));

    let member = ms.member().clone();
    let id_clone = id.clone();

    match new_status {
      MemberStatus::Left => self.members.left_members.push(id_clone),
      MemberStatus::Failed => self.members.failed_members.push(id_clone),
      _ => {}
    }

    // Membership changed — snapshot is stale.
    self.mark_local_state_dirty();

    self.emit_member(event_kind, vec![member]);
  }

  /// Handle an inner `NodeUpdated` event.
  ///
  /// Refreshes the member's tags.  If the node is not tracked, this is a
  /// no-op (matches the oracle's `if let Some(ms) = members.states.get_mut`).
  /// Emits `Event::Member(Update)`.
  ///
  /// Mirrors Go serf `base.go` `handleNodeUpdate` (lines 1583-1631).
  fn handle_node_update(&mut self, node: &NodeState<I, A>)
  where
    I: Clone,
    A: Clone,
  {
    let id = node.id_ref();
    let Some(ms) = self.members.states.get_mut(id) else {
      return;
    };

    let tags = if node.meta_ref().is_empty() {
      Tags::new()
    } else {
      decode_tags_from_meta(node.meta_ref().as_bytes()).unwrap_or_default()
    };

    let n = node.node();
    let status = ms.status();
    *ms.member_mut() = Member::new(n, tags, status);
    let member = ms.member().clone();

    // No `mark_local_state_dirty` here: a NodeUpdated changes only the member's
    // tags and address, and the push-pull snapshot carries neither (only the
    // Lamport clocks, per-member status times, `left_members`, and the event
    // ring). Status and status time are unchanged, so the snapshot is
    // unaffected and a resync would be wasted work — and `set_tags` queues
    // exactly this event via `update_meta` on every local tag change.

    self.emit_member(MemberEventKind::Update, vec![member]);
  }

  /// Handle a gossiped join intent (`JoinMessage`).
  ///
  /// Witnesses the member clock.  If the node is already in the membership
  /// store and the intent is stale (`ltime <= status_time`), returns `false`.
  /// Otherwise, updates `status_time` and, if the member is currently
  /// `Leaving`, clears it back to `Alive`.  If the node is absent, buffers
  /// the intent via `upsert_intent`.
  ///
  /// Returns `true` if the intent should be rebroadcast.
  ///
  /// Mirrors Go serf `base.go` `handleNodeJoinIntent` (lines 1345-1380).
  pub(crate) fn handle_node_join_intent(&mut self, ltime: LamportTime, id: &I, now: Instant) -> bool
  where
    I: Clone,
  {
    // Whole-message drop gate: reject unacceptable Lamport times before any
    // state mutation, clock witness, dirty flag, or event emission.
    if !ltime_is_acceptable(ltime.0) {
      return false;
    }
    // Witness a potentially newer member clock.
    witness(&mut self.clock, ltime.0);

    if let Some(ms) = self.members.states.get_mut(id) {
      // Stale intent: ltime <= status_time — no state change, no dirty mark.
      if ltime <= ms.status_time() {
        return false;
      }
      ms.set_status_time(ltime);
      // If we are Leaving, a newer join intent clears it back to Alive
      // (the leave must have been for an older time).
      if ms.status() == MemberStatus::Leaving {
        ms.set_status(MemberStatus::Alive);
        *ms.member_mut() = {
          let m = ms.member();
          Member::new(m.node().clone(), m.tags().clone(), MemberStatus::Alive)
        };
      }
      self.mark_local_state_dirty();
      true
    } else {
      // Node not yet seen — buffer the intent.
      let buffered = self.members.upsert_intent(id, IntentKind::Join, ltime, now);
      if buffered {
        self.mark_local_state_dirty();
      }
      buffered
    }
  }

  /// Handle a gossiped leave intent (`LeaveMessage`).
  ///
  /// Witnesses the member clock.  If the node is absent, buffers via
  /// `upsert_intent`.  If the node is present and the intent is stale
  /// (`ltime <= status_time`), returns `false`.
  ///
  /// **Self-refute:** if the intent targets the local node id and the local
  /// `SerfState` is `Alive`, the node refutes the leave by enqueuing a join
  /// broadcast and returns `false`.
  ///
  /// **`status_time` update (consul#8179 / consul#7960):** `status_time` is
  /// updated unconditionally even when the current status is already `Leaving`
  /// or `Left`, preventing the infinite-rebroadcast bug.
  ///
  /// State transitions:
  /// - `Alive` → `Leaving`
  /// - `Failed` → `Left` (move to `left_members`, emit `Member(Leave)`)
  /// - `Leaving | Left` → stay (status_time updated)
  /// - `None` → `false`
  ///
  /// Returns `true` if the intent should be rebroadcast.
  ///
  /// Mirrors Go serf `base.go` `handleNodeLeaveIntent` (lines 1449-1579).
  pub(crate) fn handle_node_leave_intent<T>(
    &mut self,
    t: &mut T,
    ltime: LamportTime,
    id: &I,
    prune: bool,
    now: Instant,
  ) -> bool
  where
    T: Reliable<I, A>,
    I: Clone,
  {
    // Whole-message drop gate: reject unacceptable Lamport times before any
    // state mutation, clock witness, dirty flag, or event emission.
    if !ltime_is_acceptable(ltime.0) {
      return false;
    }
    // Witness a potentially newer member clock.
    witness(&mut self.clock, ltime.0);

    if !self.members.states.contains_key(id) {
      let buffered = self
        .members
        .upsert_intent(id, IntentKind::Leave, ltime, now);
      if buffered {
        self.mark_local_state_dirty();
      }
      return buffered;
    }

    // Stale intent: ltime <= status_time → no transition, no rebroadcast, no dirty.
    // This guard fires for ALL nodes, including the local node, before the
    // self-refute path.  A stale leave for self must not trigger broadcast_join.
    let ms = self.members.states.get(id).unwrap();
    if ltime <= ms.status_time() {
      return false;
    }

    // Self-refute: if this is a FRESH leave intent for the local node and the local
    // endpoint is Alive, push back with a join broadcast and suppress the rebroadcast.
    // We check this AFTER the stale guard so stale self-leaves do not trigger refutes.
    // broadcast_join calls handle_node_join_intent which marks dirty when it buffers.
    let is_local = id == t.endpoint_ref().local_id_ref();
    if is_local && self.state == SerfState::Alive {
      // Refute the leave by re-announcing our own Join intent so every peer
      // clears the spurious Leaving status; the rebroadcast of the leave itself
      // is suppressed.
      self.broadcast_join(t, LamportTime(self.clock));
      return false;
    }

    // Always update status_time even if the status doesn't change; the
    // infinite-rebroadcast bug (consul#8179 / consul#7960) fires when the
    // time is left stale on a repeated intent.
    let ms = self.members.states.get_mut(id).unwrap();
    ms.set_status_time(ltime);

    let current = ms.status();
    match current {
      MemberStatus::None => false,
      MemberStatus::Alive => {
        ms.set_status(MemberStatus::Leaving);
        *ms.member_mut() = {
          let m = ms.member();
          Member::new(m.node().clone(), m.tags().clone(), MemberStatus::Leaving)
        };
        self.mark_local_state_dirty();
        // prune forgets the member outright instead of leaving the Leaving
        // tombstone for the reaper.
        if prune {
          self.prune_member(id);
        }
        true
      }
      MemberStatus::Leaving | MemberStatus::Left => {
        self.mark_local_state_dirty();
        if prune {
          self.prune_member(id);
        }
        true
      }
      MemberStatus::Failed => {
        let id_clone = id.clone();
        ms.set_status(MemberStatus::Left);
        *ms.member_mut() = {
          let m = ms.member();
          Member::new(m.node().clone(), m.tags().clone(), MemberStatus::Left)
        };
        let member = ms.member().clone();
        // Move from failed_members to left_members.
        remove_old_member(&mut self.members.failed_members, &id_clone);
        self.members.left_members.push(id_clone);
        self.emit_member(MemberEventKind::Leave, vec![member]);
        self.mark_local_state_dirty();
        if prune {
          self.prune_member(id);
        }
        true
      }
    }
  }

  /// Forget a member entirely (the `prune` path of a forced leave).
  ///
  /// Removes the member from the state store, the left/failed index lists, and
  /// the recent-intent buffer, drops any cached Vivaldi coordinate, and emits a
  /// `Member(Reap)` event.  Unlike the reaper, this fires immediately rather
  /// than after the tombstone timeout — a `force_leave(prune = true)` purges the
  /// node now.  Mirrors serf-core `base.rs` `handle_prune` + `erase_node!`.
  ///
  /// Go serf's `handle_prune` only removes a `Leaving`/`Left` node from
  /// `left_members` (relying on the invariant that a node is never in both
  /// lists) and sleeps its broadcast timeout plus the leave-propagate delay for
  /// a `Leaving` member before erasing.  The Sans-I/O machine cannot sleep, so
  /// the prune is immediate; it also scrubs every index list and the
  /// recent-intent entry so no stale reference to the forgotten node survives
  /// in any structure.
  fn prune_member(&mut self, id: &I) {
    remove_old_member(&mut self.members.left_members, id);
    remove_old_member(&mut self.members.failed_members, id);
    self.members.recent_intents.remove(id);

    #[cfg(feature = "coordinates")]
    {
      if let Some(ref mut cc) = self.coord_client {
        cc.forget_node(id);
      }
      self.coord_cache.remove(id);
    }

    if let Some(ms) = self.members.states.remove(id) {
      self.emit_member(MemberEventKind::Reap, vec![ms.member().clone()]);
    }
    self.mark_local_state_dirty();
  }

  // ── reaper ────────────────────────────────────────────────────────────────

  /// Reap stale left/failed members and expired intents.
  ///
  /// Run order (mirrors Go serf `base.go` `Reaper.run`):
  /// 1. `reap_failed` — remove `Failed` members whose `leave_time` elapsed
  ///    past `reconnect_timeout` (24 h default).
  /// 2. `reap_left` — remove `Left` (tombstone) members whose `leave_time`
  ///    elapsed past `tombstone_timeout` (24 h default).
  /// 3. `reap_intents` — purge `recent_intents` entries older than
  ///    `recent_intent_timeout` (10 min default).
  ///
  /// Each reaped member is erased from `states` and its id-list, and a
  /// `Event::Member(Reap)` is emitted.
  ///
  /// `now` is the driver-threaded instant; no wall-clock reads occur here.
  fn fire_reap(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    let reconnect_timeout = self.opts.reconnect_timeout();
    let tombstone_timeout = self.opts.tombstone_timeout();
    let intent_timeout = self.opts.recent_intent_timeout();

    // Reap failed members whose leave_time > reconnect_timeout, honoring the
    // per-member override: Go serf's `reap!` (base.rs ~521-553) consults the
    // `ReconnectDelegate` here with `reconnect_timeout` as the base. The
    // delegate and the membership store are disjoint fields, so their shared
    // borrows compose.
    let mut i = 0;
    while i < self.members.failed_members.len() {
      let id = self.members.failed_members[i].clone();
      let expired = match self.members.states.get(&id) {
        Some(ms) => {
          let timeout = match &self.reconnect_delegate {
            Some(d) => d.reconnect_timeout(ms.member(), reconnect_timeout),
            None => reconnect_timeout,
          };
          ms.leave_time()
            .is_some_and(|lt| now.duration_since(lt) > timeout)
        }
        None => false,
      };
      if expired {
        self.members.failed_members.swap_remove(i);
        if let Some(ms) = self.members.states.remove(&id) {
          // G13: purge stale latency-filter history so a rejoining peer starts
          // with a clean Vivaldi model rather than inheriting stale samples.
          #[cfg(feature = "coordinates")]
          {
            if let Some(ref mut cc) = self.coord_client {
              cc.forget_node(&id);
            }
            self.coord_cache.remove(&id);
          }
          self.emit_member(MemberEventKind::Reap, vec![ms.member().clone()]);
        }
        // Do not increment i — swap_remove moved the last element here.
      } else {
        i += 1;
      }
    }

    // Reap left (tombstone) members whose leave_time > tombstone_timeout,
    // honoring the same per-member override: Go serf shares one `reap!` pass
    // for the left list, passing `tombstone_timeout` as the base to the very
    // same `ReconnectDelegate` (base.rs ~568-569).
    let mut i = 0;
    while i < self.members.left_members.len() {
      let id = self.members.left_members[i].clone();
      let expired = match self.members.states.get(&id) {
        Some(ms) => {
          let timeout = match &self.reconnect_delegate {
            Some(d) => d.reconnect_timeout(ms.member(), tombstone_timeout),
            None => tombstone_timeout,
          };
          ms.leave_time()
            .is_some_and(|lt| now.duration_since(lt) > timeout)
        }
        None => false,
      };
      if expired {
        self.members.left_members.swap_remove(i);
        if let Some(ms) = self.members.states.remove(&id) {
          // G13: purge stale latency-filter history so a rejoining peer starts
          // with a clean Vivaldi model rather than inheriting stale samples.
          #[cfg(feature = "coordinates")]
          {
            if let Some(ref mut cc) = self.coord_client {
              cc.forget_node(&id);
            }
            self.coord_cache.remove(&id);
          }
          self.emit_member(MemberEventKind::Reap, vec![ms.member().clone()]);
        }
      } else {
        i += 1;
      }
    }

    // Reap stale recent intents.
    self
      .members
      .recent_intents
      .retain(|_, intent| now.duration_since(intent.wall_time()) <= intent_timeout);

    // Membership may have changed — snapshot is stale.
    self.mark_local_state_dirty();
  }

  // ── reconnector ───────────────────────────────────────────────────────────

  /// Probabilistically re-dial a random failed peer (reconnect attempt).
  ///
  /// Mirrors Go serf `base.go` `Reconnector.run` (lines 628-680):
  ///
  /// 1. If `failed_members` is empty, return immediately (no-op).
  /// 2. Compute `prob = num_failed / max(num_alive, 1)` where `num_alive` is
  ///    the number of members that are neither failed nor left.
  /// 3. Draw `r: f32` from `self.rng`; if `r > prob`, skip (probabilistic
  ///    throttle so that each failed peer is retried roughly once per
  ///    `reconnect_interval` across the whole cluster).
  /// 4. Pick a random index into `failed_members` (uniform; draw from `self.rng`).
  /// 5. Call `inner.start_push_pull(addr, PushPullKind::Join, now)`.
  ///    The inner queues `Event::DialRequested` which the sieve passes through
  ///    to the driver as `Event::DialRequested(DialPassthrough { .. })`.
  ///
  /// H3: serf emits a dial request; it does NO dial itself.
  fn fire_reconnect<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
    A: Clone,
  {
    let num_failed = self.members.failed_members.len();
    if num_failed == 0 {
      return;
    }

    // num_alive = all members that are neither failed nor left (Go serf:
    // `(mu.states.len() - num_failed - mu.left_members.len()).max(1)`).
    let num_alive = self
      .members
      .states
      .len()
      .saturating_sub(num_failed)
      .saturating_sub(self.members.left_members.len())
      .max(1);

    let prob = num_failed as f32 / num_alive as f32;
    let r: f32 = self.rng.random();
    if r > prob {
      // Probabilistic throttle: skip this interval.
      return;
    }

    // Pick a random failed member (uniform distribution).
    let idx: usize = self.rng.random_range(0..num_failed);
    let id = self.members.failed_members[idx].clone();
    let Some(ms) = self.members.states.get(&id) else {
      return;
    };
    let addr = ms.member().node().addr_ref().clone();

    // Capture the dialled address for test assertions at the call site. The
    // reliable coordinator sieves the inner `DialRequested` into its own dial
    // queue (it IS the driver and dials itself), so the event never reaches the
    // serf sieve — the call site is the only place serf observes the choice.
    #[cfg(test)]
    {
      self.last_dial_addr = Some(addr.clone());
    }
    t.start_push_pull(addr, PushPullKind::Join, now);
    self.drain_inner(t);
  }

  // ── join ─────────────────────────────────────────────────────────────────

  /// Announce the local node's join intent to the cluster.
  ///
  /// The driver owns the inner-memberlist join (it dials the seed nodes and
  /// drives the push-pull exchange); once that succeeds it calls this method so
  /// serf gossips its own `Join` intent and peers learn the local node's join
  /// ltime without waiting for the next anti-entropy round.  Push-pull only
  /// backstops it.  Mirrors serf-core `api.rs` `join()`'s
  /// `broadcast_join(self.clock.time())` call on inner-join success.
  ///
  /// State gate: only `Alive` announces a join; any other lifecycle state
  /// returns [`Error::BadJoinState`].
  pub(crate) fn join<T>(&mut self, t: &mut T) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone,
  {
    if self.state != SerfState::Alive {
      return Err(Error::BadJoinState(self.state));
    }
    self.broadcast_join(t, LamportTime(self.clock));
    Ok(())
  }

  /// Broadcast a `Join` intent for the local node at `ltime`.
  ///
  /// Used both on join (announce) and to refute a stale leave intent targeting
  /// the local node.  Witnesses the member clock at `ltime`, applies the intent
  /// locally via `handle_node_join_intent`, then enqueues the encoded
  /// `JoinMessage` on the intent tier (rank 0, highest priority).  Mirrors
  /// serf-core `base.rs` `broadcast_join`.
  fn broadcast_join<T>(&mut self, t: &mut T, ltime: LamportTime)
  where
    T: Reliable<I, A>,
    I: Clone,
  {
    let local_id = t.endpoint_ref().local_id_ref().clone();

    // Witness the member clock, then apply the intent locally so the local node
    // is recorded at this ltime (handle_node_join_intent also witnesses, but the
    // explicit witness here mirrors the oracle and is a no-op on the second pass).
    witness(&mut self.clock, ltime.0);
    let now = self.drain_now;
    self.handle_node_join_intent(ltime, &local_id, now);

    // Encode and enqueue on the intent tier.  Ignoring Err: a `Join` carrying a
    // single id never approaches the gossip MTU, so the only error path is an
    // encode failure on a degenerate id type, which the driver surfaces at
    // construction; a dropped intent is re-announced by the next push-pull.
    let jm = JoinMessage::new(ltime, local_id);
    if let Ok(encoded) = AnyMessage::<I, A>::Join(jm).encode() {
      let _ = t.queue_user_broadcast_ranked(0, encoded);
    }
  }

  // ── leave chain ──────────────────────────────────────────────────────────

  /// Begin a graceful leave.
  ///
  /// State-machine gate (mirrors Go serf `api.go` `Leave()`):
  /// - `Left` → `Ok(())` (idempotent; leave already completed).
  /// - `Leaving` | `Shutdown` → `Err(BadLeaveState)`.
  /// - `Alive` → proceeds with the leave chain below.
  ///
  /// Leave chain (decision 5 / oracle `api.go` leave()):
  /// 1. Post-increment the member clock to stamp the leave ltime — but only
  ///    after the [`LTIME_MAX`] integrity-floor gate: if the stamp would land at
  ///    or above `LTIME_MAX` (a clock driven near the floor), return
  ///    [`Error::LeaveClockExhausted`] without mutating any state. No invalid
  ///    intent is emitted and no inner leave starts; the endpoint stays a
  ///    consistent `Alive` member (degraded-but-safe).
  /// 2. Set `state = Leaving`, then handle the local leave intent
  ///    (`handle_node_leave_intent` for the local id), which marks the local
  ///    node as `Leaving` in the membership store and queues a join-refute
  ///    suppression.
  /// 3. Enqueue the leave-intent broadcast on the intent tier (rank 0) so peers
  ///    learn the local node is leaving.
  /// 4. Call inner `leave(now)` synchronously. The inner packs the payloads
  ///    still queued on the user-broadcast tiers — the rank-0 intent just
  ///    enqueued — into its dead-self fan-out, user parts ahead of the death
  ///    notice, so every farewell recipient receives the intent ATOMICALLY with
  ///    the dead-self notice in one datagram and processes the intent first,
  ///    classifying the departure as intentional rather than a failure. The
  ///    inner emits `Event::LeftCluster` once all dead-self packets drain via
  ///    `poll_transmit`. No separate wait for the intent to flush exists: the
  ///    queued intent departs with the dead-self frame, not on its own schedule.
  ///
  /// The `Leaving → Left` transition happens later in `handle_timeout` when
  /// `leave_complete_deadline` (armed on inner `LeftCluster` + `leave_propagate_delay`)
  /// elapses.  `Event::LeftCluster` is emitted at that point.
  pub(crate) fn leave<T>(&mut self, t: &mut T, now: Instant) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    // Latch the command's instant so any coalesced member event this leave
    // reaches arms its window from live `now`, consistent with the ingress and
    // timeout paths and with `force_leave`.
    self.drain_now = now;

    match self.state {
      SerfState::Left => return Ok(()), // idempotent
      SerfState::Leaving | SerfState::Shutdown => {
        return Err(Error::BadLeaveState(self.state));
      }
      SerfState::Alive => {}
    }

    let local_id = t.endpoint_ref().local_id_ref().clone();

    // Post-increment: advance first, then stamp the new value. This matches
    // Go serf's clock.Increment() (returns the new value), and is required so
    // the leave ltime (1 on a fresh node) is strictly greater than the
    // self-join status_time (0), preventing a stale-intent rejection.
    //
    // Compute the prospective stamp WITHOUT committing the clock yet: near the
    // LTIME_MAX integrity floor (a member clock driven there by a corrupt
    // snapshot or crafted peer value), the post-incremented stamp lands at or
    // above LTIME_MAX, which handle_node_leave_intent and every peer reject.
    // Committing such a clock would also poison our push-pull snapshot — peers
    // drop a whole body whose top-level ltime is out of range — so we park
    // degraded-but-safe BEFORE any mutation: stay Alive and consistent, emit no
    // invalid intent, and start no inner leave. Unreachable in any finite
    // cluster lifetime (2^63 membership events).
    let stamp = self.clock.saturating_add(1);
    if !ltime_is_acceptable(stamp) {
      return Err(Error::LeaveClockExhausted);
    }

    // The stamp is acceptable: commit it and transition to Leaving.
    self.clock = stamp;
    let ltime = LamportTime(self.clock);
    self.mark_local_state_dirty();

    // 1. Transition to Leaving BEFORE applying the local intent so the
    //    self-refute guard in handle_node_leave_intent does NOT fire (it only
    //    fires when state == Alive).
    self.state = SerfState::Leaving;

    // 2. Local leave intent — marks the local node as Leaving in the store. The
    //    acceptability gate above guarantees this applies; defensively revert to
    //    a consistent Alive state and bail if it somehow did not, so we never
    //    broadcast an intent no node can apply nor start an inconsistent inner
    //    leave.
    if !self.handle_node_leave_intent(t, ltime, &local_id, false, now) {
      self.state = SerfState::Alive;
      return Err(Error::LeaveClockExhausted);
    }

    // 3. Encode the leave intent and call the inner leave synchronously,
    //    passing the intent as the explicit farewell payload. The coordinator
    //    RESERVES it into every dead-self farewell compound ahead of its
    //    ordinary queue drain — user parts before the death notice — so every
    //    farewell recipient receives the intent atomically with the dead-self
    //    notice and processes it first, reading the departure as intentional,
    //    regardless of what else is queued (an older, larger queued payload
    //    cannot crowd the reservation out). This queues the resulting fan-out
    //    packets for `poll_transmit`.
    //
    //    An encode failure (a degenerate id type — a construction-time concern
    //    the driver surfaces) degrades to a plain inner leave: peers then read
    //    the departure as a failure until the late-intent heal cannot help,
    //    matching the pre-farewell tolerance for the same degenerate case.
    let farewell = AnyMessage::<I, A>::Leave(LeaveMessage::new(ltime, local_id, false))
      .encode()
      .ok();
    t.leave(now, farewell)?;

    Ok(())
  }

  /// Forcibly remove another node from the cluster by broadcasting a leave intent.
  ///
  /// Mirrors Go serf `api.go` `force_leave()` / `remove_failed_node_prune()`.
  /// Broadcasts a `LeaveMessage` with `prune` set to `prune`; this causes
  /// peers to immediately remove the node from the membership store rather than
  /// waiting for tombstone timeout.
  ///
  /// Does not require the local endpoint to be `Alive` (callers may want to
  /// clean up failed nodes before leaving themselves), but rejects `Shutdown`.
  ///
  /// A leave intent queued here shortly before a local [`leave`](Self::leave)
  /// now rides that leave's dead-self fan-out — the inner `leave()` packs the
  /// still-pending user broadcasts into its farewell frames — instead of being
  /// dropped by the pre-fan-out queue reset.
  pub(crate) fn force_leave<T>(
    &mut self,
    t: &mut T,
    id: I,
    prune: bool,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    // Latch the command's instant so the coalesced Leave/Reap this force-leave
    // reaches arms its window from live `now`, not a stale `drain_now`.
    self.drain_now = now;

    if self.state == SerfState::Shutdown {
      return Err(Error::BadLeaveState(self.state));
    }

    // Compute the prospective stamp WITHOUT committing the clock yet: near the
    // LTIME_MAX integrity floor, the post-incremented stamp lands at or above
    // LTIME_MAX, which handle_node_leave_intent and every peer reject.
    // Committing such a clock would also poison push-pull snapshots, so we park
    // degraded-but-safe BEFORE any mutation: advance no clock, apply no local
    // intent, and broadcast nothing. Mirrors the identical guard in leave().
    let stamp = self.clock.saturating_add(1);
    if !ltime_is_acceptable(stamp) {
      return Err(Error::LeaveClockExhausted);
    }

    // The stamp is acceptable: commit it.
    self.clock = stamp;
    let ltime = LamportTime(self.clock);
    self.mark_local_state_dirty();

    // Apply the local leave intent; if for any reason it does not apply (e.g.
    // the target is unknown), skip the broadcast so we do not emit an intent
    // no node can apply.
    if !self.handle_node_leave_intent(t, ltime, &id, prune, now) {
      return Ok(());
    }

    // Broadcast the leave intent (carrying the prune flag) so peers apply the
    // same forced removal.
    self.broadcast_leave(t, ltime, id, prune);

    Ok(())
  }

  /// Encode and enqueue a `Leave` intent for `id` at `ltime` on the intent tier.
  ///
  /// Shared by the graceful `leave()` (local id, `prune = false`) and
  /// `force_leave()` (remote id, caller's `prune`).  Mirrors the
  /// `encode_message_to_bytes(&LeaveMessage) + broadcast` step in serf-core
  /// `api.rs` `leave()` and `base.rs` `force_leave()`.
  ///
  /// Go serf gates the broadcast on `has_alive_members()` (skipping the enqueue
  /// when the cluster is a singleton).  The Sans-I/O machine enqueues
  /// unconditionally — the inner gossip layer only transmits when peers exist,
  /// so an enqueue against an empty cluster is a harmless no-op rather than a
  /// special case.
  fn broadcast_leave<T>(&mut self, t: &mut T, ltime: LamportTime, id: I, prune: bool)
  where
    T: Reliable<I, A>,
  {
    let lm = LeaveMessage::new(ltime, id, prune);
    // Ignoring Err: a `Leave` carrying a single id never approaches the gossip
    // MTU, so the only error path is an encode failure on a degenerate id type
    // (a construction-time concern the driver surfaces); a dropped intent is
    // re-announced by the next anti-entropy round.
    if let Ok(encoded) = AnyMessage::<I, A>::Leave(lm).encode() {
      let _ = t.queue_user_broadcast_ranked(0, encoded);
    }
  }

  // ── user-event API ───────────────────────────────────────────────────────

  /// Fire a user event (mirrors Go serf `api.go` `UserEvent`).
  ///
  /// Sequence (oracle `api.go` `user_event`):
  /// 1. Pre-encode size check against `max_user_event_size` (name.len + payload.len).
  /// 2. Stamp `ltime = self.event_clock` (pre-increment value).
  /// 3. Build `UserEventMessage { ltime, name, payload, cc: coalesce }`.
  /// 4. Encode to bytes; post-encode size check.
  /// 5. Increment the event clock (`self.event_clock += 1`).
  /// 6. Process locally via `handle_user_event` (dedup + emit `Event::User`).
  /// 7. Enqueue encoded bytes on the **event tier** (rank 2) via
  ///    `inner.queue_user_broadcast_ranked(2, bytes)`.
  ///
  /// Go serf checks `max_user_event_size` in three places; this port
  /// consolidates to two (pre-name+payload-len, post-encoded-len), matching
  /// the oracle's intent without the redundant intermediate check.
  pub(crate) fn user_event<T>(
    &mut self,
    t: &mut T,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    coalesce: bool,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
  {
    // Latch the command's instant so the coalescer arms from live `now` when
    // this event feeds it (mirrors the ingress/timeout paths). Without this a
    // coalescing event issued after an idle gap would arm from a stale
    // `drain_now` and flush immediately, defeating the batching window.
    self.drain_now = now;

    // Refuse once the machine has shut down (lost id-conflict vote).
    self.ensure_not_shutdown()?;

    let name: smol_str::SmolStr = name.into();
    let max_size = self.opts.max_user_event_size();

    // Pre-encode size check: name.len() + payload.len() must not exceed the limit.
    let pre_len = name.len() + payload.len();
    if pre_len > max_size {
      return Err(Error::UserEventTooLarge(pre_len, max_size));
    }

    // Stamp ltime via next_ltime: clamps to < LTIME_MAX and advances the clock.
    let ltime = LamportTime(next_ltime(&mut self.event_clock));

    let msg = UserEventMessage {
      ltime,
      cc: coalesce,
      name: name.clone(),
      payload: payload.clone(),
    };

    // Encode to bytes for the broadcast queue.
    // UserEvent wire encoding does not depend on I or A — encode directly via
    // the bridge + framing helpers to avoid a phantom type parameter.
    let pb = user_event_to_pb(&msg);
    let encoded: bytes::Bytes = encode_message(MessageType::UserEvent, &pb)
      .map_err(EncodeError::Frame)
      .map(|v| v.into())?;

    // Post-encode size check.
    if encoded.len() > max_size {
      return Err(Error::UserEventTooLarge(encoded.len(), max_size));
    }

    self.mark_local_state_dirty();

    // Process locally (dedup + emit Event::User if first sight).
    // For a locally-originated event the dedup always accepts it (it was
    // not in the ring yet), so we can safely ignore the bool return.
    let _ = self.handle_user_event(msg);

    // Enqueue on the event tier (rank 2 = bottom priority after intent=0, query=1).
    // Ignoring Err: the inner rejects oversized frames; the pre-encode size
    // check above already guarantees we are within max_user_event_size, so
    // the only remaining path to an error is a frame larger than the inner's
    // gossip MTU — a configuration mismatch the driver should detect at
    // startup.  We propagate it back rather than silently drop.
    t.queue_user_broadcast_ranked(2, encoded)
      .map_err(Error::InnerLeave)?;

    Ok(())
  }

  /// Ingress handler for a received `UserEventMessage`.
  ///
  /// Called both on locally-originated events (from `user_event`) and on
  /// gossiped events decoded from `UserPacket`.
  ///
  /// Mirrors Go serf `base.go` `handleUserEvent`:
  /// 1. Witness the event clock.
  /// 2. Drop if `ltime < min_time`.
  /// 3. Drop if the event is too old relative to the ring size
  ///    (`cur_time > ring_len && ltime < cur_time - ring_len`).
  /// 4. Dedup against the ring slot for this ltime.
  /// 5. If new: emit `Event::User(msg)`.
  ///
  /// Returns `true` if the event was new (should be rebroadcast by the caller).
  pub(crate) fn handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    let ltime = msg.ltime.0;

    // Whole-message drop gate: reject unacceptable Lamport times before any
    // state mutation, clock witness, dirty flag, or event emission.
    if !ltime_is_acceptable(ltime) {
      return false;
    }

    // Reject inbound events that exceed the configured size limit.
    // Mirrors the pre-encode check in user_event() for locally-originated events,
    // applied here on the ingress path to bound rebroadcast amplification.
    let pre_len = msg.name.len() + msg.payload.len();
    if pre_len > self.opts.max_user_event_size() {
      return false;
    }

    // Witness a potentially newer event clock.
    witness(&mut self.event_clock, ltime);
    let cur_time = self.event_clock;

    let ev = UserEvent {
      name: msg.name.clone(),
      payload: msg.payload.clone(),
    };

    // Dedup ring: returns false for duplicates and stale events — no state change.
    // Mark dirty only when witness_event confirms this is a new event.
    if !self.event_buffer.witness_event(cur_time, ltime, ev) {
      return false;
    }

    self.mark_local_state_dirty();
    // First sight — emit to the driver (coalesced when enabled and the event
    // opted into coalescing).
    self.emit_user(msg);
    true
  }

  /// Decode a serf `AnyMessage` off a `UserPacket` and dispatch to the
  /// matching handler.
  ///
  /// The original `Bytes` buffer is retained (refcount-shared) for
  /// re-broadcast — the relay-retain invariant: no re-encode on the
  /// re-broadcast path.
  ///
  /// Dispatch table (oracle: `delegate.rs` `NotifyMsg`):
  /// - `UserEvent` → `handle_user_event`; if new, `rebroadcast` on event tier.
  /// - `Join`      → `handle_node_join_intent`; if rebroadcast, intent tier.
  /// - `Leave`     → `handle_node_leave_intent`; if rebroadcast, intent tier.
  /// - `Query`           → `handle_query`; if rebroadcast, query tier.
  /// - `QueryResponse`   → `handle_query_response`: fold into `PendingQuery`.
  /// - `Relay`           → `handle_relay`: verbatim forward to destination.
  /// - `ConflictResponse`→ dropped (conflict responses arrive as `QueryResponse` payloads).
  /// - `KeyRequest`/`KeyResponse` → dropped (key responses arrive as `QueryResponse` payloads).
  /// - `PushPull`  → not valid on a UserPacket; dropped.
  /// - Decode failure → silent drop (never panic on bad network input).
  ///
  /// H4: both `Reliability::Reliable` and `Reliability::Unreliable` dispatch
  /// identically; the reliability value affects delivery guarantees at the
  /// memberlist layer but not the serf handler logic.
  fn handle_user_packet<T>(&mut self, t: &mut T, _from: A, data: Bytes, now: Instant)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // Pre-decode size fence: peek the message-type tag and the total frame
    // length from the raw bytes before allocating or bridge-converting the body.
    // For size-bounded types (UserEvent, Query) reject frames whose encoded
    // length exceeds the configured limit before any decode work — memory cost
    // scales with the attacker-controlled body size, so the fence must come
    // first.  `data.len()` is the frame length the exact-consumption gate will
    // also enforce below; peeking from the raw bytes avoids body allocation.
    //
    // Relay and QueryResponse carry no configured size limit — the fence is a
    // no-op for them; their payloads are already limited by the inner
    // Endpoint's per-packet MTU contract and the query_size_limit applied at
    // the originating peer before the query was sent.
    match peek_frame_header(data.as_ref()) {
      Ok((MessageType::UserEvent, _)) if data.len() > self.opts.max_user_event_size() => {
        return;
      }
      Ok((MessageType::Query, _)) if data.len() > self.opts.query_size_limit() => {
        return;
      }
      // Incomplete / empty / varint-overflow frames are caught by the
      // decode_with_consumed call below; don't double-drop here.
      _ => {}
    }

    // Exact-consumption decode: the buffer must hold exactly one serf frame.
    // A packet whose decoded frame does not consume the entire buffer is
    // malformed (e.g. a valid small message followed by trailing junk bytes);
    // drop before any state mutation, clock witness, event emission, or
    // rebroadcast to prevent junk amplification and budget bypass.
    let msg = match AnyMessage::<I, A>::decode_with_consumed(&data) {
      Ok((m, consumed)) if consumed == data.len() => m,
      // Decode failure or trailing junk — drop silently; bad bytes must never
      // panic the machine.
      _ => return,
    };

    match msg {
      AnyMessage::UserEvent(ue) => {
        // Relay-retain: pass the original `data` bytes to rebroadcast; the
        // decoded `ue` is consumed by handle_user_event for dedup + emission.
        let is_new = self.handle_user_event(ue);
        if is_new {
          self.rebroadcast(t, MessageType::UserEvent, data);
        }
      }
      AnyMessage::Join(join) => {
        // handle_node_join_intent takes ltime + id reference.
        let rebroadcast = self.handle_node_join_intent(join.ltime, &join.id.clone(), now);
        if rebroadcast {
          self.rebroadcast(t, MessageType::Join, data);
        }
      }
      AnyMessage::Leave(leave) => {
        let id = leave.id.clone();
        let rebroadcast = self.handle_node_leave_intent(t, leave.ltime, &id, leave.prune, now);
        if rebroadcast {
          self.rebroadcast(t, MessageType::Leave, data);
        }
      }
      // Sub-stage 3: push-pull is not valid on a UserPacket; drop.
      AnyMessage::PushPull(_) => {}
      // Query handling: witness clock, dedup, filter, emit Event::Query.
      AnyMessage::Query(q) => {
        // Defense-in-depth: post-decode semantic size gate kept as a second
        // check after the pre-decode fence above already enforces the limit.
        if data.len() > self.opts.query_size_limit() {
          return;
        }
        let rebroadcast = self.handle_query(t, q, QueryOrigin::Inbound);
        if rebroadcast {
          self.rebroadcast(t, MessageType::Query, data);
        }
      }
      // Fold the query response into the matching PendingQuery.
      AnyMessage::QueryResponse(resp) => {
        self.handle_query_response(t, resp);
      }
      // Relay: forward the inner payload verbatim to the destination (decision 4).
      AnyMessage::Relay(relay) => {
        self.handle_relay(t, relay);
      }
      // ConflictResponse and Key* bare packets arrive only as payloads inside
      // QueryResponseMessage; bare arrivals here are unexpected — drop silently.
      AnyMessage::ConflictResponse(_) => {}
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      AnyMessage::KeyRequest(_) => {}
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      AnyMessage::KeyResponse(_) => {}
    }
  }

  /// Re-queue the original packet `Bytes` on the appropriate broadcast tier.
  ///
  /// The relay-retain rule: re-broadcast keeps the **original** `Bytes`
  /// (a cheap `Bytes::clone` — just a refcount bump, no copy).  No re-encode.
  ///
  /// Tier assignments (mirrors oracle `delegate.rs` `rebroadcast_queue`):
  /// - `Join` / `Leave` intents  → intent tier (rank 0, highest priority).
  /// - `UserEvent`               → event tier (rank 2, lowest priority).
  /// - All other types           → no-op (queries handled via their own tier
  ///   in sub-stage 4).
  ///
  /// Errors from `queue_user_broadcast_ranked` are silently dropped — the
  /// inner already applies its own back-pressure and queue-depth limits.
  fn rebroadcast<T>(&mut self, t: &mut T, ty: MessageType, original: Bytes)
  where
    T: Reliable<I, A>,
  {
    let rank: u8 = match ty {
      MessageType::Join | MessageType::Leave => 0, // intent tier
      MessageType::UserEvent => 2,                 // event tier
      MessageType::Query => 1,                     // query tier (sub-stage 4)
      _ => return,
    };
    // Depth gate: skip enqueue if the queue is already at or over the effective
    // cap.  This prevents unbounded memory growth under a flood of unique first-seen
    // messages.  The gate mirrors Go serf's `getQueueMax` / `checkQueueDepth`
    // logic applied inline at the rebroadcast site.
    if t.endpoint_ref().user_broadcast_queue_len() >= self.queue_max() {
      return;
    }
    // Ignoring Err: the inner applies its own MTU back-pressure; a rejected
    // broadcast is a flow-control decision, not a fatal error.
    let _ = t.queue_user_broadcast_ranked(rank, original);
  }

  /// Compute the effective broadcast queue depth cap (mirrors Go serf `getQueueMax`).
  ///
  /// When `min_queue_depth > 0`, the cap is `max(min_queue_depth, 2 * num_members)`,
  /// scaling with the cluster so larger clusters get a proportionally larger budget.
  /// Otherwise the flat `max_queue_depth` applies.
  fn queue_max(&self) -> usize {
    let min = self.opts.min_queue_depth();
    if min > 0 {
      min.max(2 * self.members.states.len())
    } else {
      self.opts.max_queue_depth()
    }
  }

  // ── Query issue + ingress (G8: read-not-increment) ────────────────────────

  /// Issue an application query.
  ///
  /// Steps (oracle: `base.rs` `query_in`):
  /// 1. Compute the default timeout if `params.timeout == 0`.
  /// 2. Stamp `ltime = query_clock` (**read, not incremented** — G8/H8).
  /// 3. Draw a random `id` from `self.rng`.
  /// 4. Size-check the encoded `QueryMessage` against `query_size_limit`.
  /// 5. Register a `PendingQuery` keyed by `(ltime, id)` with `deadline = now + timeout`.
  /// 6. Process the query locally via `handle_query` (the machine is always a
  ///    potential responder to its own queries, matching the oracle's "process
  ///    locally first" order).
  /// 7. Encode and enqueue on the **query tier** (rank 1).
  ///
  /// Returns the `QueryId` so the caller can correlate responses.
  pub(crate) fn query<T>(
    &mut self,
    t: &mut T,
    name: impl Into<SmolStr>,
    payload: Bytes,
    params: QueryParams<I>,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // Refuse once the machine has shut down (lost id-conflict vote), before any
    // RNG draw, clock read, or state mutation.
    self.ensure_not_shutdown()?;

    // Tag-regex pre-validation: compile-check every Filter::Tag pattern FIRST,
    // before any RNG draw, clock read, or state mutation.  A broken pattern
    // returns Err with zero side effects — no RNG advance, no ltime stamp, no
    // PendingQuery, no broadcast, no event.  Exact-string matching (no-regex
    // path) has no compile step and cannot produce an invalid pattern.
    #[cfg(feature = "tag-regex")]
    for filter in &params.filters {
      if let Filter::Tag(tf) = filter {
        if let Some(expr) = &tf.expr {
          if regex::Regex::new(expr.as_str()).is_err() {
            return Err(Error::InvalidQueryFilter);
          }
        }
      }
    }

    let name: SmolStr = name.into();

    // G8 / H8: stamp from the query clock; the clock is not incremented for
    // outbound queries — reads only, per G8.
    let ltime = LamportTime(self.query_clock);

    // Draw a random id from serf's own injected RNG.
    let id: u32 = self.rng.next_u32();

    // Resolve the timeout: use provided value or fall back to the oracle's
    // `gossip_interval * query_timeout_mult * log10(n + 1)` heuristic.
    // The inner Endpoint does not expose `gossip_interval` via a public
    // accessor, so when timeout is zero this uses `query_timeout_mult * 200ms`
    // (a rough constant).  Drivers that need precise timing should supply an
    // explicit timeout via `QueryParams::timeout`.
    let timeout = if params.timeout.is_zero() {
      let n = self.members.states.len();
      let mult = self.opts.query_timeout_mult();
      let log_factor = (crate::mathf::ceil(crate::mathf::log10(n as f64 + 1.0)) as u32).max(1);
      core::time::Duration::from_millis(200) * mult as u32 * log_factor
    } else {
      params.timeout
    };

    // Build the flags.
    let flags = if params.request_ack {
      QueryFlag::ACK
    } else {
      QueryFlag::empty()
    };

    let q = QueryMessage {
      ltime,
      id,
      from: memberlist_proto::Node::new(
        t.endpoint_ref().local_id_ref().clone(),
        t.endpoint_ref().advertise_ref().clone(),
      ),
      filters: params.filters,
      flags,
      relay_factor: params.relay_factor,
      timeout,
      name,
      payload,
    };

    // Size-check before encoding.
    let encoded = AnyMessage::<I, A>::Query(q.clone())
      .encode()
      .map_err(Error::UserEventEncode)?;
    if encoded.len() > self.opts.query_size_limit() {
      return Err(Error::QueryTooLarge(
        encoded.len(),
        self.opts.query_size_limit(),
      ));
    }

    let query_id = QueryId { ltime, id };
    let deadline = now + timeout;

    // Register the pending query BEFORE processing locally so that if the
    // local handle_query immediately generates a response (e.g. the local node
    // passes its own filter), the response fold path can find the pending entry.
    self.pending_queries.push(PendingQuery {
      kind: QueryPurpose::App,
      deadline,
      responses: crate::FxHashMap::default(),
      acks: crate::FxHashMap::default(),
      query_id,
      request_ack: params.request_ack,
      conflict_matching: 0,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      num_nodes: 0,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      key_tally: None,
    });

    // Process the query locally (oracle: "Process query locally" before broadcast).
    // QueryOrigin::Local bypasses the inbound cap so the initiating node always
    // self-processes its own query.
    self.drain_now = now;
    self.handle_query(t, q, QueryOrigin::Local);

    // Enqueue on the query tier (rank 1).
    // Ignoring Err: the inner applies queue-depth / MTU back-pressure; a rejected
    // broadcast is a flow-control decision, not a fatal error.
    let _ = t.queue_user_broadcast_ranked(1, encoded);

    Ok(query_id)
  }

  /// Handle a query, either received from a peer or locally originated.
  ///
  /// `origin` distinguishes the two call sites:
  /// - `QueryOrigin::Inbound`: called from `handle_user_packet` for a
  ///   peer-broadcast query.  The inbound overflow cap (`MAX_RECEIVED_QUERIES`)
  ///   applies here as a DoS defence against peer flooding.
  /// - `QueryOrigin::Local`: called from `query()` and `internal_query()` for
  ///   queries the local node originates.  The cap is bypassed — local ops are
  ///   app-rate-limited, never adversarial, and the initiating node MUST always
  ///   self-process its own query.
  ///
  /// Mirrors Go serf `base.go` `handleQuery`.
  ///
  /// Steps:
  /// 1. **Witness the query clock** at `msg.ltime`.
  /// 2. **Dedup** via `query_buffer.witness_query(cur_time, ltime, id)`.
  ///    Returns `false` (no rebroadcast, no emission) on duplicate or too-old.
  /// 3. **`NO_BROADCAST` flag**: if set, suppress rebroadcast.
  /// 4. **Filter check** (`should_process_query`): if the local node does not
  ///    match the filters, return `true` (G6 — still rebroadcast!), but do NOT
  ///    emit a local `Event::Query`.
  /// 5. **Emit** `Event::Query(QueryEvent { … })` for the driver/app to respond.
  ///
  /// Returns `true` if the query should be rebroadcast (i.e., first sight AND
  /// not `NO_BROADCAST`), even when the filter rejects local processing (G6).
  fn handle_query<T>(&mut self, t: &mut T, msg: QueryMessage<I, A>, origin: QueryOrigin) -> bool
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone,
  {
    let ltime = msg.ltime.0;

    // Whole-message drop gate: reject unacceptable Lamport times before any
    // state mutation, clock witness, dirty flag, or event emission.
    if !ltime_is_acceptable(ltime) {
      return false;
    }

    // Internal-query payload gate: for `_serf_conflict`, decode and
    // exact-consumption-validate the payload BEFORE any state mutation (before
    // the clock witness, dedup insert, received_queries insert, ACK send, and
    // rebroadcast).  A payload whose decoded id does not consume ALL bytes is
    // malformed: a valid-prefix id followed by trailing junk passes the outer
    // `AnyMessage::decode_with_consumed` gate but must be rejected here.
    // Dropping the whole query on malformed payload means: no clock advance, no
    // dedup entry, no received_queries entry, no ConflictResponse send.
    let pre_decoded_conflict_id: Option<I> = if msg.name.as_str() == "_serf_conflict" {
      match I::decode(msg.payload.as_ref()) {
        Ok((consumed, id)) if consumed == msg.payload.len() => Some(id),
        _ => return false, // Malformed payload: drop before any state mutation.
      }
    } else {
      None
    };

    // Key-management query gate: decode and exact-consumption-validate before
    // any state mutation.  Mirrors the _serf_conflict gate above.
    //
    // Op-shape enforcement: `_serf_install_key`, `_serf_use_key`, and
    // `_serf_remove_key` MUST carry `key = Some`; `_serf_list_keys` MUST carry
    // `key = None`.  A mismatch is treated as malformed and dropped before any
    // clock witness, dedup, received_queries insert, ACK, event, or rebroadcast.
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    let pre_decoded_key_op: Option<(KeyRequestOperation, Option<memberlist_proto::SecretKey>)> = {
      match msg.name.as_str() {
        "_serf_install_key" | "_serf_use_key" | "_serf_remove_key" | "_serf_list_keys" => {
          let op = match msg.name.as_str() {
            "_serf_install_key" => KeyRequestOperation::Install,
            "_serf_use_key" => KeyRequestOperation::Use,
            "_serf_remove_key" => KeyRequestOperation::Remove,
            _ => KeyRequestOperation::List,
          };
          match AnyMessage::<I, A>::decode_with_consumed(&msg.payload) {
            Ok((AnyMessage::KeyRequest(m), consumed)) if consumed == msg.payload.len() => {
              // Op-shape check: key presence must match the operation's expectation.
              // Install/Use/Remove require a key; List must have none.
              if op.has_key() != m.key.is_some() {
                return false; // Shape mismatch: drop before any state mutation.
              }
              Some((op, m.key))
            }
            _ => return false, // Malformed payload: drop before any state mutation.
          }
        }
        _ => None,
      }
    };

    // Tag-regex pre-validation gate: reject a query that carries any
    // `Filter::Tag` with an uncompilable regex BEFORE any side effect.  An
    // uninterpretable filter is malformed input: the query must be dropped with
    // zero side effects (no clock witness, no dedup insert, no
    // received_queries entry, no ACK, no event, no rebroadcast).  This is
    // distinct from a valid-but-non-matching filter, which still propagates
    // (witnesses the clock, inserts the dedup entry, rebroadcasts) while
    // suppressing the local `Event::Query` (G6).
    //
    // The gate runs only when `tag-regex` is enabled because exact-string
    // matching (the no-regex path) has no compile step and cannot produce an
    // invalid pattern.
    #[cfg(feature = "tag-regex")]
    for filter in &msg.filters {
      if let Filter::Tag(tf) = filter {
        if let Some(expr) = &tf.expr {
          if regex::Regex::new(expr.as_str()).is_err() {
            return false; // Uncompilable regex: drop before any state mutation.
          }
        }
      }
    }

    // Hard-cap (INBOUND only): when `received_queries` holds MAX_RECEIVED_QUERIES
    // LIVE tokens, drop the inbound query BEFORE any state mutation — no clock
    // witness, no dedup write, no ACK, no event emission, no rebroadcast.  Every
    // live entry was surfaced to the driver (Event::Query or Event::KeyRequest)
    // and its token must remain answerable via respond / respond_key until its
    // deadline or until the driver responds.
    //
    // Prune expired tokens inline first, using the ingress `now` (drain_now), so
    // the cap counts only LIVE entries.  Ingress precedes the periodic
    // deadline-prune in after_inner_timeout within a tick, so without this a
    // flood of stale past-deadline tokens — already unanswerable — could pin the
    // cap and drop a new live query.  Evicting a past-deadline token here is
    // safe: respond / respond_key would reject it via the G7 deadline guard.
    //
    // Local queries (QueryOrigin::Local) bypass this cap: the initiating node
    // MUST always self-process its own query regardless of inbound saturation.
    // Local query volume is app-controlled and not an adversarial flood vector.
    if origin == QueryOrigin::Inbound {
      let now = self.drain_now;
      self.prune_expired_received_queries(now);
      if self.received_queries.len() >= MAX_RECEIVED_QUERIES {
        return false;
      }
    }

    // Witness a potentially newer query clock.
    witness(&mut self.query_clock, ltime);
    let cur_time = self.query_clock;

    // Dedup by (ltime, id): returns false for duplicate (ltime, id) pairs.
    // Mark dirty only when witness_query confirms this is a new query.
    if !self.query_buffer.witness_query(cur_time, ltime, msg.id) {
      return false;
    }

    self.mark_local_state_dirty();

    // Check the NO_BROADCAST flag.
    let mut rebroadcast = true;
    if msg.no_broadcast() {
      rebroadcast = false;
    }

    // Filter check (G6): even if the local node is not targeted, still rebroadcast.
    if !self.should_process_query(t, &msg.filters) {
      return rebroadcast;
    }

    // Clamp the peer-supplied timeout to MAX_QUERY_TIMEOUT before computing the
    // deadline.  An unclamped timeout lets a flooder pin received_queries entries
    // open for an arbitrarily long time; the clamp bounds the worst-case TTL.
    let clamped_timeout = msg.timeout.min(MAX_QUERY_TIMEOUT);
    let deadline = self.drain_now + clamped_timeout;

    // Register this received query so respond() can enforce the three guards
    // (G7: size, once-only, deadline) and look up the originator address.
    let query_id = QueryId {
      ltime: msg.ltime,
      id: msg.id,
    };
    let from_addr = msg.from.addr_ref().clone();
    self.received_queries.insert(
      query_id,
      ReceivedQuery {
        from: from_addr,
        deadline,
      },
    );

    // If the querier requested an acknowledgement, send an immediate ACK before
    // emitting Event::Query.  The ACK carries no payload; it signals receipt.
    if msg.ack() {
      let local_node = memberlist_proto::Node::new(
        t.endpoint_ref().local_id_ref().clone(),
        t.endpoint_ref().advertise_ref().clone(),
      );
      let ack_resp = QueryResponseMessage {
        ltime: msg.ltime,
        id: msg.id,
        from: local_node,
        flags: QueryFlag::ACK,
        payload: Bytes::new(),
      };
      if let Ok(ack_encoded) = AnyMessage::<I, A>::QueryResponse(ack_resp).encode() {
        let from_addr = msg.from.addr_ref().clone();
        // Ignoring Err: directed ACK sends are best-effort; a missing ACK is
        // handled by the querier's timeout on its acks set.
        let _ = t.send_user_packet(from_addr.clone(), ack_encoded.clone());
        #[cfg(test)]
        {
          self.last_directed_send = Some((from_addr, ack_encoded.clone()));
        }
        // Relay the ACK through relay_factor random intermediary nodes when requested.
        if msg.relay_factor > 0 {
          self.relay_response(t, msg.from.clone(), ack_encoded, msg.relay_factor);
        }
      }
    }

    // Internal conflict query: respond autonomously without surfacing to the driver.
    // `pre_decoded_conflict_id` is always `Some` here because the
    // internal-query payload gate at the top returned `false` for any
    // `_serf_conflict` with a malformed payload; execution only reaches this
    // point when the id was already decoded and exactly consumed.
    if let Some(conflict_id) = pre_decoded_conflict_id {
      self.received_queries.remove(&query_id);
      self.handle_conflict_query(t, &msg, conflict_id);
      return rebroadcast;
    }

    // Key-management query: emit Event::KeyRequest and return without surfacing
    // to the app as Event::Query.  The received_queries entry is kept so
    // respond_key can enforce the G7 guards and look up the originator address.
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    if let Some((op, key)) = pre_decoded_key_op {
      self
        .pending_events
        .push_back(Event::KeyRequest(KeyRequestEvent {
          op,
          key,
          id: msg.id,
          ltime: msg.ltime,
          from: msg.from,
          relay_factor: msg.relay_factor,
          deadline,
        }));
      return rebroadcast;
    }

    let ev = QueryEvent {
      id: msg.id,
      ltime: msg.ltime,
      from: msg.from,
      name: msg.name,
      payload: msg.payload,
      relay_factor: msg.relay_factor,
      deadline,
    };
    self.pending_events.push_back(Event::Query(ev));

    rebroadcast
  }

  /// Check whether the local node passes the query's filter list.
  ///
  /// Mirrors Go serf `query.go` `shouldProcessQuery`.
  ///
  /// - An empty filter list means "broadcast to all" → always `true`.
  /// - `Filter::Id(ids)`: the local id must be present in `ids`.
  /// - `Filter::Tag(tag_filter)`: the local node's tags must contain the
  ///   key and the value must match the expression.
  ///
  /// When `tag-regex` is enabled, the expression is compiled as a regex and
  /// matched with partial (anywhere-in-value) semantics, mirroring Go serf
  /// `query.go` `regexp.MatchString(filt.Expr, tags[filt.Tag])`.  When
  /// `tag-regex` is disabled, the expression is compared by exact string
  /// equality.
  ///
  /// All `Filter::Tag` patterns with `tag-regex` enabled are guaranteed to be
  /// valid by the pre-witness validation gate in `handle_query`; this function
  /// is only called on queries that have already passed that gate.
  ///
  /// Returns `true` if the node should respond locally, `false` if filtered out.
  /// A `false` return suppresses `Event::Query` but does NOT affect rebroadcast
  /// (rebroadcast is decided by the caller, not by this function).
  fn should_process_query<T>(&self, t: &T, filters: &[Filter<I>]) -> bool
  where
    T: Reliable<I, A>,
    I: Clone,
  {
    let local_id = t.endpoint_ref().local_id_ref();
    for filter in filters {
      match filter {
        Filter::Id(ids) => {
          // The local node must appear in the id list.
          if !ids.iter().any(|n| n == local_id) {
            return false;
          }
        }
        Filter::Tag(tag_filter) => {
          // The local node's tags live in `members.states` under the local id.
          // If the local node is not yet in the store (before the first join
          // event has been processed), we conservatively return `false`.
          let local_id = t.endpoint_ref().local_id_ref();
          let empty_tags = Tags::new();
          let tags = self
            .members
            .states
            .get(local_id)
            .map(|ms| ms.member().tags())
            .unwrap_or(&empty_tags);
          match tags.0.get(tag_filter.tag.as_str()) {
            Some(val) => {
              if let Some(expr) = &tag_filter.expr {
                #[cfg(feature = "tag-regex")]
                {
                  // The pre-validation gate in handle_query ensures every
                  // pattern reaching here is a valid regex; unwrap is safe.
                  let re = regex::Regex::new(expr.as_str())
                    .expect("regex validated by handle_query pre-validation gate");
                  if !re.is_match(val.as_str()) {
                    return false;
                  }
                }
                #[cfg(not(feature = "tag-regex"))]
                {
                  if val.as_str() != expr.as_str() {
                    return false;
                  }
                }
              }
              // No expr → key presence is sufficient.
            }
            None => return false,
          }
        }
      }
    }
    true
  }

  // ── respond() + handle_query_response() ─────────────────────────────────────

  /// Respond to a received query (G7 / oracle: `event.go` `QueryContext.respond`).
  ///
  /// Three guards applied in order:
  /// 1. **Size** (`query_response_size_limit`): `payload.len() > limit` → `Err(RespondTooLarge)`.
  /// 2. **Once-only**: already responded to this token → `Err(AlreadyResponded)`.
  /// 3. **Deadline**: `now > deadline` → `Err(RespondAfterDeadline)`.
  ///
  /// On success: encodes a `QueryResponseMessage`, sends it via `send_user_packet`
  /// to the querier's address (directed send, never broadcast), marks the token
  /// as responded.  The relay path is a TODO stub.
  ///
  /// Go serf's `event.go` checks size first, then uses a combined
  /// already-responded+deadline guard (the mutex holding the span doubles as
  /// both checks).  This port uses three distinct guards in order — size,
  /// already-responded, deadline — which is strictly more informative to
  /// callers that need to distinguish the error cases.
  pub(crate) fn respond<T>(
    &mut self,
    t: &mut T,
    token: &QueryEvent<I, A>,
    payload: Bytes,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // Refuse once the machine has shut down (lost id-conflict vote): answering an
    // already-received query after shutdown is dead work.
    self.ensure_not_shutdown()?;

    // Look up the received-query entry for this token.
    let query_id = QueryId {
      ltime: token.ltime(),
      id: token.id(),
    };
    let entry = self
      .received_queries
      .get(&query_id)
      .ok_or(Error::AlreadyResponded)?;
    let deadline = entry.deadline;
    let to = entry.from.clone();
    let relay_factor = token.relay_factor();
    let relay_querier = token.from().clone();

    // Build and encode the `QueryResponseMessage`.
    let local_node = memberlist_proto::Node::new(
      t.endpoint_ref().local_id_ref().clone(),
      t.endpoint_ref().advertise_ref().clone(),
    );
    let resp = QueryResponseMessage {
      ltime: token.ltime(),
      id: token.id(),
      from: local_node,
      flags: QueryFlag::empty(),
      payload,
    };
    let encoded = AnyMessage::<I, A>::QueryResponse(resp)
      .encode()
      .map_err(Error::RespondEncode)?;

    self.respond_inner(
      t,
      query_id,
      to,
      relay_factor,
      relay_querier,
      encoded,
      deadline,
      now,
    )
  }

  /// Shared directed-send logic for `respond` and `respond_key`.
  ///
  /// Applies the G7 size guard, sends `encoded` directed to `to`, removes the
  /// `received_queries` entry on success, and relays when `relay_factor > 0`.
  ///
  /// The once-only guard is implemented by entry removal: the caller's
  /// `.ok_or(AlreadyResponded)` at lookup acts as the gate; `respond_inner`
  /// removes the entry on success so a subsequent lookup returns `None`.
  // All parameters are distinct routing / payload values with no natural sub-grouping;
  // a wrapper struct would add churn without clarity.
  #[allow(clippy::too_many_arguments)]
  fn respond_inner<T>(
    &mut self,
    t: &mut T,
    query_id: QueryId,
    to: A,
    relay_factor: u8,
    relay_querier: Node<I, A>,
    encoded: Bytes,
    deadline: Instant,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    // Guard 3: deadline.
    if now > deadline {
      return Err(Error::RespondAfterDeadline);
    }

    // Guard 1: encoded frame size against the limit.
    let limit = self.opts.query_response_size_limit();
    if encoded.len() > limit {
      return Err(Error::RespondTooLarge(encoded.len(), limit));
    }

    // Directed send — never broadcast.
    t.send_user_packet(to.clone(), encoded.clone())
      .map_err(Error::RespondSend)?;

    #[cfg(test)]
    {
      self.last_directed_send = Some((to, encoded.clone()));
    }

    // Send succeeded: remove the entry.
    self.received_queries.remove(&query_id);

    // Relay when requested.
    if relay_factor > 0 {
      self.relay_response(t, relay_querier, encoded, relay_factor);
    }

    Ok(())
  }

  /// Respond to a received key-management query.
  ///
  /// Builds the sealed wire `KeyResponseMessage` from `resp`, wraps it in a
  /// `QueryResponseMessage`, and sends it directed to the query originator via
  /// `respond_inner` (which enforces the G7 size, once-only, and deadline guards).
  ///
  /// The caller (driver) is responsible for applying the key operation to its
  /// keyring before calling this method.  The machine never mutates key material.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub(crate) fn respond_key<T>(
    &mut self,
    t: &mut T,
    req: &crate::event::KeyRequest<I, A>,
    resp: KeyResponseArgs,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // Refuse once the machine has shut down (lost id-conflict vote): answering an
    // already-received key query after shutdown is dead work.
    self.ensure_not_shutdown()?;

    let query_id = QueryId {
      ltime: req.ltime,
      id: req.id,
    };
    let entry = self
      .received_queries
      .get(&query_id)
      .ok_or(Error::AlreadyResponded)?;
    let deadline = entry.deadline;
    let to = entry.from.clone();
    let relay_factor = req.relay_factor;
    let relay_querier = req.from.clone();

    let key_resp = crate::KeyResponseMessage {
      result: resp.result,
      message: resp.message,
      keys: resp.keys,
      primary_key: resp.primary_key,
    };
    let inner_payload = AnyMessage::<I, A>::KeyResponse(key_resp)
      .encode()
      .map_err(Error::RespondEncode)?;

    let local_node = memberlist_proto::Node::new(
      t.endpoint_ref().local_id_ref().clone(),
      t.endpoint_ref().advertise_ref().clone(),
    );
    let qresp = QueryResponseMessage {
      ltime: req.ltime,
      id: req.id,
      from: local_node,
      flags: QueryFlag::empty(),
      payload: inner_payload,
    };
    let encoded = AnyMessage::<I, A>::QueryResponse(qresp)
      .encode()
      .map_err(Error::RespondEncode)?;

    self.respond_inner(
      t,
      query_id,
      to,
      relay_factor,
      relay_querier,
      encoded,
      deadline,
      now,
    )
  }

  /// Fold an incoming `QueryResponseMessage` into the matching `PendingQuery`.
  ///
  /// Mirrors Go serf `base.go` `handleQueryResponse` + `query.go`
  /// `handle_query_response`.
  ///
  /// Steps:
  /// 1. Look up `PendingQuery` by `(ltime, id)`.  If not found (stale / already
  ///    expired), silently drop — the oracle logs a warn, the Sans-I/O machine
  ///    has no logging layer.
  /// 2. Check if the `deadline` has elapsed; if so, silently drop.
  /// 3. **Ack** (`msg.ack()` set): dedup by responder id in the separate `acks`
  ///    set; on first sight emit `Event::QueryAck`.  An ack is a bare
  ///    delivery confirmation (no payload) and does not consume the responder's
  ///    response slot — the same peer may later send a real response.  Mirrors
  ///    the oracle's per-query `ack_ch` send.
  /// 4. **Response** (no ack flag): dedup by responder id in `responses`; on
  ///    first sight dispatch on `PendingQuery.kind`:
  ///    - `App` → emit `Event::QueryResponse { id, from, payload }`.
  ///    - `Conflict` → tally: decode `ConflictResponseMessage`, compare addr to local advertise.
  ///    - `Key` → tally: decode `KeyResponseMessage`, fold into the `KeyResponseTally`.
  fn handle_query_response<T>(&mut self, t: &mut T, msg: QueryResponseMessage<I, A>)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    let query_id = QueryId {
      ltime: msg.ltime,
      id: msg.id,
    };

    let drain_now = self.drain_now;

    // Steps 1–3: validate, then for acks dedup + emit and return.  The pending
    // borrow is scoped; for the response path we capture the kind and fall
    // through to dispatch after the borrow ends.
    let kind = {
      // Step 1: find matching PendingQuery.
      let pending = match self
        .pending_queries
        .iter_mut()
        .find(|pq| pq.query_id == query_id)
      {
        Some(pq) => pq,
        // Stale response (query already expired or was never registered): drop.
        None => return,
      };

      // Step 2: deadline elapsed — drop.
      if drain_now > pending.deadline {
        return;
      }

      // Step 3: ack path — only emit if the originating query requested acks
      // (`request_ack`); drop acks for non-ack queries (they may arrive as
      // stale retransmits when the ACK flag was set by a buggy peer).
      if msg.ack() {
        if !pending.request_ack {
          return; // This query did not request acks; drop the ack silently.
        }
        if pending.acks.contains_key(msg.from.id_ref()) {
          return;
        }
        pending.acks.insert(msg.from.id_ref().clone(), ());
        self.pending_events.push_back(Event::QueryAck(QueryAck {
          id: msg.id,
          from: msg.from,
        }));
        return;
      }

      // Step 4: response path — membership validation (internal queries only),
      // then dedup by responder id.
      //
      // For internal queries (Conflict, Key) the responder id MUST be a known
      // cluster member before counting the response in the tally denominator.
      // An unknown / forged id could inflate the conflict majority or key-op
      // success count and trigger false shutdown or false key-op confirmation.
      // For App queries the response is forwarded verbatim to the driver, which
      // makes its own trust decisions; the machine does not filter by membership.
      //
      // Transport-origin authentication is driver-side and out of scope for the
      // machine; this check is the membership-plausibility gate the machine CAN
      // enforce for its own internal tallies.
      let is_internal = !matches!(pending.kind, QueryPurpose::App);
      if is_internal && !self.members.states.contains_key(msg.from.id_ref()) {
        return;
      }

      // Dedup by responder id: the same responder cannot be counted twice.
      if pending.responses.contains_key(msg.from.id_ref()) {
        return;
      }

      // Validate the payload before counting in the denominator.  For every
      // internal-query purpose (Conflict, Key) a malformed or wrong-type
      // response must NOT increment `num_resp` — an inflated denominator skews
      // the majority/success threshold.
      //
      // `App` responses are surfaced verbatim to the driver (no internal decode
      // needed here) and are always counted.
      match pending.kind {
        QueryPurpose::Conflict => {
          // Exact-consumption decode: trailing bytes in an internal response
          // payload must reject the response before counting, not after.
          match AnyMessage::<I, A>::decode_with_consumed(&msg.payload) {
            Ok((AnyMessage::ConflictResponse(_), consumed)) if consumed == msg.payload.len() => {}
            _ => return, // Malformed, unexpected type, or trailing junk: drop without counting.
          }
        }
        #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
        QueryPurpose::Key => {
          // Exact-consumption decode: trailing bytes in an internal response
          // payload must reject the response before counting, not after.
          match AnyMessage::<I, A>::decode_with_consumed(&msg.payload) {
            Ok((AnyMessage::KeyResponse(_), consumed)) if consumed == msg.payload.len() => {}
            _ => return, // Malformed, unexpected type, or trailing junk: drop without counting.
          }
        }
        QueryPurpose::App => {}
      }

      pending.responses.insert(msg.from.id_ref().clone(), ());

      pending.kind
    };

    // Step 5: dispatch (pending_queries borrow has ended).
    match kind {
      QueryPurpose::App => {
        let ev = QueryResponseEvent {
          id: msg.id,
          from: msg.from,
          payload: msg.payload,
        };
        self.pending_events.push_back(Event::QueryResponse(ev));
      }
      QueryPurpose::Conflict => {
        self.handle_conflict_response_fold(t, query_id, msg.payload);
      }
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      QueryPurpose::Key => {
        let from_id = msg.from.id_ref().clone();
        self.handle_key_response_fold(query_id, from_id, msg.payload);
      }
    }
  }

  // ── Responder-side relay ─────────────────────────────────────────────────────

  /// Relay a query response through up to `relay_factor` random Alive members.
  ///
  /// Mirrors Go serf `query.rs` `relay_response` (lines 523-601):
  ///
  /// 1. **Early exit when `relay_factor == 0`**: no relay needed.
  /// 2. **Count guard**: requires at least `relay_factor + 1` total members in the
  ///    membership store (so there are enough other nodes to relay through).
  ///    If the cluster is too small, **silently return** — no event, no error.
  ///    This is a deliberate improvement over the oracle (which returns an `Ok(())`):
  ///    the machine cannot surface the skip as an error since it is not a failure.
  /// 3. **Size check**: if `relay_frame.len() > query_response_size_limit`, emit
  ///    `Event::RelayDropped` per destination (the response is too large to relay).
  /// 4. **Build the relay wrapper**: one `RelayMessage { destination: querier,
  ///    payload: relay_frame }` — the same frame is sent to every relay peer.
  /// 5. **Pick `relay_factor` random Alive non-self peers** from `self.rng` (the
  ///    oracle uses `random_members`; we implement the equivalent inline).
  /// 6. **Directed-send** the relay frame to each chosen peer via
  ///    `inner.send_user_packet`.  On send failure emit `Event::RelayDropped`.
  ///
  /// **Decision-4 FIX notes:**
  /// - Count-guard failure (`num_members < k + 1`) is a **silent no-op** (no
  ///   event), not an error.  The oracle also returns `Ok(())` here.
  /// - Size-overflow is an explicit `Event::RelayDropped`.
  /// - The destination querier IS reselectable as a relay peer (the relay dedup
  ///   is the query-response dedup at the querier, not here).
  /// - Only `Alive` non-self members are eligible relay peers.
  fn relay_response<T>(
    &mut self,
    t: &mut T,
    querier: Node<I, A>,
    relay_frame: Bytes,
    relay_factor: u8,
  ) where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    if relay_factor == 0 {
      return;
    }

    // Count guard: need at least relay_factor + 1 members (silent no-op if too few).
    let num_members = self.members.states.len();
    if num_members < relay_factor as usize + 1 {
      return;
    }

    // Size check: the relay wraps the full QueryResponse frame; its encoded size
    // must not exceed the response size limit.
    let limit = self.opts.query_response_size_limit();
    if relay_frame.len() > limit {
      // The frame is too large to relay; emit RelayDropped for each would-be
      // relay target (we don't know the exact addresses, so emit once for the
      // destination to surface the failure to the driver).
      self
        .pending_events
        .push_back(Event::RelayDropped(crate::event::RelayDropped {
          destination: querier.addr_ref().clone(),
        }));
      return;
    }

    // Build the relay wrapper.  The inner payload is the verbatim QueryResponse
    // frame — relay-retain: no re-encode.
    let relay_msg = RelayMessage::new(querier, relay_frame);
    let relay_encoded = match AnyMessage::<I, A>::Relay(relay_msg).encode() {
      Ok(b) => b,
      Err(_) => return, // Encode failure: silently drop (best-effort relay).
    };

    // Collect Alive non-self members as eligible relay candidates.
    //
    // Collected as (id_bytes, addr) pairs so we can sort by the encoded id
    // before the Fisher-Yates shuffle.  The sort is required for determinism:
    // HashMap::iter() returns keys in an arbitrary, per-instance order, so two
    // endpoints with identical membership and identical RNG seed would select
    // different relay peers without it.  Sorting by the id's encoded bytes
    // establishes a total, stable input order so the shuffle is a deterministic
    // function of the RNG state (Go serf `random_members` uses a slice with a
    // stable iteration order for the same reason).
    let local_id = t.endpoint_ref().local_id_ref();
    let mut candidates: Vec<(Vec<u8>, A)> = self
      .members
      .states
      .iter()
      .filter(|(id, ms)| *id != local_id && ms.status() == MemberStatus::Alive)
      .map(|(id, ms)| {
        let id_bytes = id.encode_to_vec().unwrap_or_default();
        (id_bytes, ms.member().node().addr_ref().clone())
      })
      .collect();

    if candidates.is_empty() {
      return;
    }

    // Sort by encoded id bytes for a stable, deterministic input ordering.
    candidates.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    // Pick relay_factor random peers using serf's own injected RNG (A2).
    // Reservoir-shuffle the first relay_factor positions (Fisher-Yates partial).
    let k = (relay_factor as usize).min(candidates.len());
    for i in 0..k {
      let j: usize = self.rng.random_range(i..candidates.len());
      candidates.swap(i, j);
    }
    candidates.truncate(k);

    // Directed-send to each chosen relay peer.  On failure emit RelayDropped.
    for (_, peer_addr) in candidates {
      let result = t.send_user_packet(peer_addr.clone(), relay_encoded.clone());
      #[cfg(test)]
      {
        // Track the last directed send for test assertions.
        self.last_directed_send = Some((peer_addr.clone(), relay_encoded.clone()));
        self
          .relay_all_directed_sends
          .push((peer_addr.clone(), relay_encoded.clone()));
      }
      if result.is_err() {
        self
          .pending_events
          .push_back(Event::RelayDropped(crate::event::RelayDropped {
            destination: peer_addr,
          }));
      }
    }
  }

  /// Handle a received `RelayMessage` (relay node B).
  ///
  /// The local node is acting as an intermediary: forward the inner payload
  /// verbatim to the wrapped destination via a single directed `send_user_packet`.
  ///
  /// Mirrors Go serf `delegate.rs` Relay arm (lines 264-310):
  /// 1. Decode the destination address from `relay.destination`.
  /// 2. **Self-destination guard**: if the destination id equals the local node id,
  ///    drop with `Event::RelayDropped` (forwarding to self is a no-op that hides
  ///    a configuration error; surfacing it helps debugging).
  /// 3. Forward `relay.payload` verbatim via `send_user_packet` — NON-recursive:
  ///    we NEVER parse the inner payload or re-relay.
  /// 4. On send failure emit `Event::RelayDropped`.
  ///
  /// **Global constraint:** the relay loop retains the original bytes — the inner
  /// payload is carried opaque, never re-encoded or re-parsed.  This function is
  /// the FINAL hop; it does NOT check whether the inner payload is itself a Relay
  /// message (the loop-guard is documented as an additive hardening that Go omits;
  /// we omit it as well to keep faithful oracle correspondence — if a relay chain
  /// were constructed, the recipient would decode an AnyMessage::Relay and call
  /// handle_relay again, naturally bounding by TTL at the network layer).
  fn handle_relay<T>(&mut self, t: &mut T, relay: RelayMessage<I, A>)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    let dest_id = relay.destination.id_ref();
    let local_id = t.endpoint_ref().local_id_ref();

    // Self-destination guard: relay to self is always a no-op failure.
    if dest_id == local_id {
      self
        .pending_events
        .push_back(Event::RelayDropped(crate::event::RelayDropped {
          destination: relay.destination.addr_ref().clone(),
        }));
      return;
    }

    let dest_addr = relay.destination.addr_ref().clone();
    let payload = relay.payload;

    let result = t.send_user_packet(dest_addr.clone(), payload.clone());
    #[cfg(test)]
    {
      self.last_directed_send = Some((dest_addr.clone(), payload.clone()));
    }
    if result.is_err() {
      self
        .pending_events
        .push_back(Event::RelayDropped(crate::event::RelayDropped {
          destination: dest_addr,
        }));
    }
  }

  /// Returns the `leave_complete_deadline`, if armed.
  ///
  /// `None` until the inner `LeftCluster` event has been received.
  pub const fn leave_complete_deadline(&self) -> Option<Instant> {
    self.leave_complete_deadline
  }

  // ── test helpers (test-only) ──────────────────────────────────────────────

  /// Return the current status of member `id`, or `None` if unknown.
  #[cfg(test)]
  pub(crate) fn test_member_status(&self, id: I) -> Option<MemberStatus>
  where
    I: Clone,
  {
    self.members.states.get(&id).map(|ms| ms.status())
  }

  /// Expose the effective broadcast queue depth cap for assertions.
  ///
  /// Gated like the unit suite that calls it (`mod tests` is `tcp`-gated), so a
  /// quic-only build does not carry an uncalled seam.
  #[cfg(all(test, feature = "tcp"))]
  pub(crate) fn test_queue_max(&self) -> usize {
    self.queue_max()
  }

  /// Return the `status_time` of member `id`, or `None` if unknown.
  #[cfg(test)]
  pub(crate) fn test_member_status_time(&self, id: I) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.members.states.get(&id).map(|ms| ms.status_time())
  }

  /// Seed a member directly into the membership store (test fixture).
  ///
  /// Uses a zero socket address for the node.  The caller is `Endpoint<u32, SocketAddr>`
  /// in all current tests, so we hard-code the sentinel address here.
  #[cfg(test)]
  pub(crate) fn test_seed_member(&mut self, id: I, status: MemberStatus, status_time: LamportTime)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), status);
    self
      .members
      .states
      .insert(id, MemberState::new(member, status_time, None));
  }

  /// Seed a member with explicit tags into the membership store (test fixture).
  ///
  /// Like `test_seed_member` but lets the caller supply a `Tags` map, enabling
  /// tag-filter unit tests to place a known value under a known key.  Only the
  /// `tag-regex` test module exercises tag filtering, so this is gated on it.
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
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, tags, status);
    self
      .members
      .states
      .insert(id, MemberState::new(member, status_time, None));
  }

  /// Seed a member as `Failed` into both `states` and `failed_members`.
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
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), MemberStatus::Failed);
    self
      .members
      .states
      .insert(id.clone(), MemberState::new(member, status_time, Some(now)));
    self.members.failed_members.push(id);
  }

  /// Seed a member as `Left` into both `states` and `left_members`.
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
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), MemberStatus::Left);
    self
      .members
      .states
      .insert(id.clone(), MemberState::new(member, status_time, Some(now)));
    self.members.left_members.push(id);
  }

  /// Invoke `handle_node_join_intent` with bare parameters (test adapter).
  #[cfg(test)]
  pub(crate) fn test_handle_join_intent(&mut self, id: I, ltime: LamportTime, now: Instant) -> bool
  where
    I: Clone,
  {
    self.handle_node_join_intent(ltime, &id, now)
  }

  /// Invoke `handle_node_leave_intent` with bare parameters (test adapter).
  #[cfg(test)]
  pub(crate) fn test_handle_leave_intent<T>(
    &mut self,
    t: &mut T,
    id: I,
    ltime: LamportTime,
    now: Instant,
  ) -> bool
  where
    T: Reliable<I, A>,
    I: Clone,
  {
    self.handle_node_leave_intent(t, ltime, &id, false, now)
  }

  /// Synthesise an inner `NodeJoined` event for node `id` and drive it
  /// through the sieve (test adapter).
  #[cfg(test)]
  pub(crate) fn test_inner_node_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    use std::sync::Arc;
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let ns = Arc::new(memberlist_proto::typed::NodeState::new(
      id,
      addr,
      memberlist_proto::typed::State::Alive,
    ));
    self.drain_now = now;
    self.handle_node_join(&ns, now);
  }

  /// Synthesise an inner `NodeLeft` event for node `id`.
  #[cfg(test)]
  pub(crate) fn test_inner_node_left(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    use std::sync::Arc;
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let ns = Arc::new(memberlist_proto::typed::NodeState::new(
      id,
      addr,
      memberlist_proto::typed::State::Dead,
    ));
    self.drain_now = now;
    self.handle_node_leave(&ns, now);
  }

  /// Synthesise an inner `NodeUpdated` event for node `id`.
  #[cfg(test)]
  pub(crate) fn test_inner_node_updated(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    use std::sync::Arc;
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let ns = Arc::new(memberlist_proto::typed::NodeState::new(
      id,
      addr,
      memberlist_proto::typed::State::Alive,
    ));
    self.drain_now = now;
    self.handle_node_update(&ns);
  }

  /// Check if `id` is in `failed_members`.
  #[cfg(test)]
  pub(crate) fn test_in_failed_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.members.failed_members.contains(&id)
  }

  /// Check if `id` is in `left_members`.
  #[cfg(test)]
  pub(crate) fn test_in_left_members(&self, id: I) -> bool
  where
    I: PartialEq,
  {
    self.members.left_members.contains(&id)
  }

  /// Simulate the inner `LeftCluster` event arriving (test adapter).
  ///
  /// Drives `on_inner_event(LeftCluster)` directly, as if the inner memberlist
  /// finished its dead-self fan-out.  Used by leave-chain tests that do not
  /// have a live inner endpoint to drive.
  #[cfg(test)]
  pub(crate) fn test_inner_left_cluster<T>(&mut self, t: &mut T)
  where
    T: Reliable<I, A>,
  {
    self.on_inner_event(t, memberlist_proto::Event::LeftCluster);
  }

  /// Seed a `Failed` member with an explicit address into both `states` and
  /// `failed_members` (test fixture for reconnect assertions).
  #[cfg(test)]
  pub(crate) fn test_seed_failed_member(&mut self, id: I, addr: A, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), MemberStatus::Failed);
    self.members.states.insert(
      id.clone(),
      MemberState::new(member, LamportTime::ZERO, Some(now)),
    );
    self.members.failed_members.push(id);
  }

  /// Directly invoke `fire_reconnect` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_fire_reconnect<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
    A: Clone,
  {
    self.fire_reconnect(t, now);
  }

  /// Directly invoke `fire_reap` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_fire_reap(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    self.fire_reap(now);
  }

  /// Return the address of the most recently reconnect-dialled peer.
  ///
  /// `None` if `fire_reconnect` has not yet produced a dial attempt.
  #[cfg(test)]
  pub(crate) fn test_last_dial_addr(&self) -> Option<A>
  where
    A: Clone,
  {
    self.last_dial_addr.clone()
  }

  // ── User-event test helpers ───────────────────────────────────────────────

  /// Directly invoke `handle_user_event` with a `UserEventMessage` (test adapter).
  ///
  /// Returns the same bool that `handle_user_event` returns (true = first sight).
  #[cfg(test)]
  pub(crate) fn test_handle_user_event(&mut self, msg: UserEventMessage) -> bool {
    self.handle_user_event(msg)
  }

  /// Set the event ring-buffer's `min_time` floor directly (test adapter).
  ///
  /// Used to simulate snapshot-recovery or `eventJoinIgnore` bump without
  /// driving a full push-pull.
  #[cfg(test)]
  pub(crate) fn test_set_event_min_time(&mut self, t: u64) {
    self.event_buffer.min_time = t;
  }

  /// Directly set the event clock value (test adapter for "too-old" ring checks).
  #[cfg(test)]
  pub(crate) fn test_set_event_clock(&mut self, t: u64) {
    self.event_clock = t;
  }

  /// Return the number of events recorded in the event ring slot for `ltime`.
  ///
  /// Used to verify the per-ltime cap (`MAX_EVENTS_PER_LTIME`) is enforced.
  #[cfg(test)]
  pub(crate) fn test_event_slot_len(&self, ltime: u64) -> usize {
    let bltime = self.event_buffer.buffer.len() as u64;
    if bltime == 0 {
      return 0;
    }
    let idx = (ltime % bltime) as usize;
    match &self.event_buffer.buffer[idx] {
      Some(slot) if slot.ltime.0 == ltime => slot.events.len(),
      _ => 0,
    }
  }

  /// Return the `num_nodes` stored on the most recent pending query (test adapter).
  #[cfg(all(test, any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
  pub(crate) fn test_last_pending_query_num_nodes(&self) -> Option<usize> {
    self.pending_queries.last().map(|pq| pq.num_nodes)
  }

  // ── Packet-ingress test helpers ───────────────────────────────────────────

  /// Inject a `UserPacket` with an explicit `from` address directly into the
  /// inner-event sieve (test adapter).
  ///
  /// Bypasses the memberlist packet-framing layer so tests can inject
  /// serf-encoded `Bytes` without wrapping them in a memberlist frame.
  /// Sets `drain_now = now` then calls `on_inner_event` with a synthetic
  /// `Event::UserPacket` carrying the given bytes and `Unreliable` reliability
  /// (mirrors the gossip-plane delivery path).
  #[cfg(test)]
  pub(crate) fn test_inject_user_packet<T>(&mut self, t: &mut T, from: A, data: Bytes, now: Instant)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    use memberlist_proto::{Reliability, UserPacket};
    self.drain_now = now;
    self.on_inner_event(
      t,
      memberlist_proto::Event::UserPacket(UserPacket::new(from, data, Reliability::Unreliable)),
    );
  }

  // ── Clock + intent test helpers ───────────────────────────────────────────

  /// Set all three Lamport clocks in one call (test fixture).
  ///
  /// Compiled under `test` or the non-default `test-support` feature: the
  /// `StreamEndpoint` test-support seam
  /// ([`StreamEndpoint::test_set_clocks`](crate::StreamEndpoint::test_set_clocks))
  /// forwards here so a downstream crate's tests can reach the member clock. Not a
  /// production build path — a plain `tcp` build never compiles this.
  #[cfg(any(test, feature = "test-support"))]
  pub(crate) fn test_set_clocks(&mut self, member: u64, event: u64, query: u64) {
    self.clock = member;
    self.event_clock = event;
    self.query_clock = query;
    self.mark_local_state_dirty();
  }

  /// Seed a member as `Left` into `states` and `left_members` using just
  /// a `status_time` (no wall-clock `now`).  The `leave_time` is set to
  /// `Instant::ORIGIN` so reaper tests that need a concrete timestamp can
  /// adjust independently.
  #[cfg(test)]
  pub(crate) fn test_seed_left_member(&mut self, id: I, status_time: LamportTime)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), MemberStatus::Left);
    self.members.states.insert(
      id.clone(),
      MemberState::new(member, status_time, Some(Instant::ORIGIN)),
    );
    self.members.left_members.push(id);
    self.mark_local_state_dirty();
  }

  /// Read back the bytes currently stored in the coordinator's inner Endpoint
  /// `local_state_snapshot` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_inner_local_state_snapshot<T>(&self, t: &T) -> Bytes
  where
    T: Reliable<I, A>,
  {
    t.endpoint_ref().local_state_snapshot_bytes()
  }

  /// Decode `bytes` as a `PushPullMessage<u32>` for assertion (test adapter).
  ///
  /// Decodes via the same `AnyMessage::decode` path that a remote peer would
  /// use, so the round-trip is exercised.
  #[cfg(test)]
  pub(crate) fn test_decode_pushpull(&self, bytes: &Bytes) -> crate::typed::PushPullMessage<I>
  where
    I: Clone + Data,
    A: Data,
  {
    match AnyMessage::<I, A>::decode(bytes).expect("decode should succeed") {
      AnyMessage::PushPull(pp) => pp,
      other => panic!("expected PushPull, got {:?}", other.message_type()),
    }
  }

  /// Return the local node's serf-side tags from `members.states` (test
  /// adapter for `set_tags` synchronous-observability assertions).
  ///
  /// Returns `None` when the local node is not yet tracked in the serf
  /// membership store.
  #[cfg(test)]
  pub(crate) fn test_local_tags_in(&self, local_id: &I) -> Option<Tags> {
    self
      .members
      .states
      .get(local_id)
      .map(|ms| ms.member().tags().clone())
  }

  /// Clear the dirty flag (test adapter for dirty-flag unit assertions).
  #[cfg(test)]
  pub(crate) fn test_clear_dirty(&mut self) {
    self.local_state_dirty = false;
  }

  /// Return the current dirty flag (test adapter).
  #[cfg(test)]
  pub(crate) fn test_is_dirty(&self) -> bool {
    self.local_state_dirty
  }

  /// Record outbound exchange `id` as an `ignore_old` join (test adapter).
  #[cfg(test)]
  pub(crate) fn test_note_ignore_join_stream(&mut self, id: StreamId) {
    self.note_ignore_join_stream(id);
  }

  /// Whether exchange `id` is currently a recorded `ignore_old` join (test
  /// adapter for cancellation / one-shot-consume / per-exchange assertions).
  ///
  /// Compiled under `test` or the non-default `test-support` feature: the
  /// `StreamEndpoint` test-support seam
  /// ([`StreamEndpoint::test_has_ignore_join_stream`](crate::StreamEndpoint::test_has_ignore_join_stream))
  /// forwards here so a downstream crate's tests can observe the ignore set. Not a
  /// production build path — a plain `tcp` build never compiles this.
  #[cfg(any(test, feature = "test-support"))]
  pub(crate) fn test_has_ignore_join_stream(&self, id: StreamId) -> bool {
    self.ignore_join_streams.contains(&id)
  }

  /// Mirror the driver's terminal cleanup (test adapter): drop the pending
  /// ignore-join entry for exchange `id`, exactly as
  /// [`Self::clear_ignore_join_stream`] does when a join terminates without a
  /// merge — a dial failure, timeout, or a dropped join future — so the
  /// cancellation-safety property can be asserted directly.
  #[cfg(test)]
  pub(crate) fn test_clear_ignore_join_stream(&mut self, id: StreamId) {
    self.clear_ignore_join_stream(id);
  }

  /// Read the current `event_buffer.min_time` (test adapter for G4 assertions).
  #[cfg(test)]
  pub(crate) fn test_event_min_time(&self) -> u64 {
    self.event_buffer.min_time
  }

  // ── Push-pull / merge test helpers ───────────────────────────────────────

  /// Directly invoke `merge_remote_state` with raw `user_data` bytes and no
  /// ignore-old suppression (test adapter).
  ///
  /// Sets `drain_now = Instant::ORIGIN` before the call so intent handlers
  /// receive a stable `now`.  Use `test_set_drain_now` to override the
  /// timestamp when wall-clock values matter.  For the ignore-old / G4 path use
  /// [`Self::test_merge_remote_state_with_stream`] (per-exchange consume) or
  /// [`Self::test_merge_remote_state_suppressed`] (suppression applied directly).
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state<T>(&mut self, t: &mut T, user_data: Bytes)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Data,
  {
    self.merge_remote_state(t, user_data, false);
  }

  /// Invoke `merge_remote_state` with suppression forced on (test adapter for the
  /// G4 `eventJoinIgnore` watermark): bumps `event_buffer.min_time` to the remote
  /// `event_ltime` and drops the body's pre-join user events, exactly as a
  /// consumed ignore-join merge does — without minting an exchange `StreamId`.
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state_suppressed<T>(&mut self, t: &mut T, user_data: Bytes)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Data,
  {
    self.merge_remote_state(t, user_data, true);
  }

  /// Drive the full `RemoteStateReceived` ignore-old path for a merge whose
  /// `originating_stream_id` is `sid` (test adapter): consume the one-shot
  /// ignore-join entry for `sid` iff `is_join`, then merge.  Mirrors
  /// `on_inner_event`'s per-exchange suppression logic.
  #[cfg(test)]
  pub(crate) fn test_merge_remote_state_with_stream<T>(
    &mut self,
    t: &mut T,
    user_data: Bytes,
    is_join: bool,
    sid: StreamId,
  ) where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Data,
  {
    let suppress = is_join && self.consume_ignore_join_stream(sid);
    self.merge_remote_state(t, user_data, suppress);
  }

  /// Return the `ltime` of the most recently buffered intent for `id` of `kind`,
  /// or `None` if no such intent exists (test adapter for G3 assertions).
  #[cfg(test)]
  pub(crate) fn test_intent_ltime(&self, id: I, kind: IntentKind) -> Option<LamportTime>
  where
    I: Clone,
  {
    self.members.recent_intent(&id, kind)
  }

  // ── Query ingress test helpers ────────────────────────────────────────────

  /// Directly invoke `handle_query` as an inbound peer query and return whether
  /// it should rebroadcast (test adapter).  Always passes `QueryOrigin::Inbound`
  /// so the inbound cap and all inbound semantics are exercised.
  #[cfg(test)]
  pub(crate) fn test_handle_query<T>(&mut self, t: &mut T, msg: QueryMessage<I, A>) -> bool
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    self.handle_query(t, msg, QueryOrigin::Inbound)
  }

  /// Overwrite `drain_now` (test adapter).
  ///
  /// Production ingress entry points latch `drain_now`; tests that drive a
  /// handler directly (e.g. `test_handle_query`) use this to advance the
  /// endpoint's current-time reference between calls without going through
  /// `handle_timeout`.
  ///
  /// Gated like the unit suite that calls it (`mod tests` is `tcp`-gated because
  /// it drives through `StreamEndpoint`), so a `quic`-only build carries no
  /// uncalled seam.
  #[cfg(all(test, feature = "tcp"))]
  pub(crate) fn test_set_drain_now(&mut self, now: Instant) {
    self.drain_now = now;
  }

  /// The member coalescer's current flush deadline (test adapter), unpolluted by
  /// the periodic serf deadlines that `serf_poll_timeout` folds in.
  #[cfg(all(test, feature = "tcp"))]
  pub(crate) fn test_member_flush_deadline(&self) -> Option<Instant> {
    self
      .member_coalescer
      .as_ref()
      .and_then(|c| c.flush_deadline())
  }

  /// The user coalescer's current flush deadline (test adapter).
  #[cfg(all(test, feature = "tcp"))]
  pub(crate) fn test_user_flush_deadline(&self) -> Option<Instant> {
    self
      .user_coalescer
      .as_ref()
      .and_then(|c| c.flush_deadline())
  }

  /// Return the `QueryId` of the last pending query entry (test adapter).
  #[cfg(test)]
  pub(crate) fn test_last_query_id(&self) -> Option<QueryId> {
    self.pending_queries.last().map(|pq| pq.query_id)
  }

  /// Return the number of pending queries (test adapter).
  #[cfg(test)]
  pub(crate) fn test_pending_query_count(&self) -> usize {
    self.pending_queries.len()
  }

  /// Return the number of entries in `received_queries` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_received_queries_len(&self) -> usize {
    self.received_queries.len()
  }

  /// Return all deadline values from `received_queries` (test adapter).
  ///
  /// Used to verify that inbound query timeouts are clamped to MAX_QUERY_TIMEOUT.
  #[cfg(test)]
  pub(crate) fn test_peek_received_query_deadlines(&self) -> Vec<Instant> {
    self
      .received_queries
      .values()
      .map(|rq| rq.deadline)
      .collect()
  }

  /// Return the current `query_buffer.min_time` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_query_min_time(&self) -> u64 {
    self.query_buffer.min_time
  }

  /// Register a synthetic received-query entry and return a matching `QueryEvent`
  /// token (test adapter for `respond()` tests).
  ///
  /// Inserts a `ReceivedQuery` keyed by `query_id` with `from = querier` and
  /// `deadline = deadline`, `responded = false`.  Returns a `QueryEvent`
  /// with matching `id`/`ltime` so tests can call `respond(&token, ...)`.
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
    self.received_queries.insert(
      query_id,
      ReceivedQuery {
        from: querier.clone(),
        deadline,
      },
    );
    QueryEvent {
      id: query_id.id,
      ltime: query_id.ltime,
      from: memberlist_proto::Node::new(I::default(), querier),
      name: SmolStr::new("test"),
      payload: Bytes::new(),
      relay_factor: 0,
      deadline,
    }
  }

  /// Call `handle_query_response` directly (test adapter).
  #[cfg(test)]
  pub(crate) fn test_handle_query_response<T>(&mut self, t: &mut T, msg: QueryResponseMessage<I, A>)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    self.handle_query_response(t, msg);
  }

  /// Return `true` if the received-query entry for `query_id` has been
  /// responded to (test adapter).
  ///
  /// `respond()` removes the entry on success, so this returns `true` when
  /// the entry is absent (either successfully responded or pruned by
  /// `handle_timeout`) and `false` while the entry is still present and
  /// awaiting a response.
  #[cfg(test)]
  pub(crate) fn test_is_responded(&self, query_id: QueryId) -> bool {
    !self.received_queries.contains_key(&query_id)
  }

  /// Return the number of entries currently in `members.recent_intents` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_recent_intents_len(&self) -> usize {
    self.members.recent_intents.len()
  }

  /// Return the conflict_matching counter for the pending query matching
  /// `query_id`, or `None` if the query is not found (test adapter).
  #[cfg(test)]
  pub(crate) fn test_pending_query_conflict_matching(&self, query_id: QueryId) -> Option<usize> {
    self
      .pending_queries
      .iter()
      .find(|pq| pq.query_id == query_id)
      .map(|pq| pq.conflict_matching)
  }

  // ── Relay test helpers ────────────────────────────────────────────────────

  /// Directly invoke `relay_response` (test adapter).
  ///
  /// Seeds a live Alive member into the store at `relay_port` so the count
  /// guard can pass when `relay_factor == 1`.  The caller is responsible for
  /// ensuring the membership store is populated to satisfy the guard.
  #[cfg(test)]
  pub(crate) fn test_relay_response<T>(
    &mut self,
    t: &mut T,
    querier: Node<I, A>,
    frame: Bytes,
    relay_factor: u8,
  ) where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    self.relay_response(t, querier, frame, relay_factor);
  }

  /// Directly invoke `handle_relay` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_handle_relay<T>(&mut self, t: &mut T, relay: RelayMessage<I, A>)
  where
    T: Reliable<I, A>,
    I: Clone,
    A: Clone,
  {
    self.handle_relay(t, relay);
  }

  /// Return the most recent `(address, bytes)` pair sent via a directed
  /// `send_user_packet` from the relay path (test adapter).
  ///
  /// Returns `None` if no directed send has occurred yet.
  #[cfg(test)]
  pub(crate) fn test_last_directed_send(&self) -> Option<(A, Bytes)>
  where
    A: Clone,
  {
    self.last_directed_send.clone()
  }

  /// Return all `(address, bytes)` pairs accumulated by `relay_response` since
  /// construction or the last `test_clear_relay_directed_sends` call.
  ///
  /// Unlike `test_last_directed_send`, this captures every send produced across
  /// a single `relay_response` invocation, enabling determinism assertions when
  /// `relay_factor > 1`.
  #[cfg(test)]
  pub(crate) fn test_relay_all_directed_sends(&self) -> &[(A, Bytes)] {
    &self.relay_all_directed_sends
  }

  /// Clear the accumulated relay directed-send log (test adapter).
  ///
  /// Call between invocations of `test_relay_response` to isolate per-call
  /// assertions.
  #[cfg(test)]
  #[allow(dead_code)]
  pub(crate) fn test_clear_relay_directed_sends(&mut self) {
    self.relay_all_directed_sends.clear();
  }

  /// Seed a member at an explicit socket address into the membership store (test fixture).
  ///
  /// Like `test_seed_member` but lets the caller supply an explicit address,
  /// enabling relay-determinism tests where members must have distinct addresses
  /// so relay-peer selection is observable.
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
    let node = memberlist_proto::Node::new(id.clone(), addr);
    let member = Member::new(node, Tags::new(), status);
    self
      .members
      .states
      .insert(id, MemberState::new(member, status_time, None));
  }

  // ── Conflict-resolution + key-management internal queries ─────────────────

  /// Issue an internal (non-app) query using `purpose` as the `PendingQuery.kind`.
  ///
  /// Mirrors `query()` but skips the App-specific flag and timeout handling;
  /// uses the oracle's `defaultQueryTimeout` heuristic unconditionally.  The
  /// registered `PendingQuery` will have `kind = purpose` so responses are
  /// routed to the appropriate fold path.
  fn internal_query<T>(
    &mut self,
    t: &mut T,
    name: SmolStr,
    payload: Bytes,
    purpose: QueryPurpose,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // Refuse once the machine has shut down (lost id-conflict vote): the single
    // chokepoint behind every key-management issuance (install / use / remove /
    // list) and the conflict-resolution query itself.
    self.ensure_not_shutdown()?;

    // G8 / H8: stamp from the query clock; queries read the clock but do not
    // increment it.
    let ltime = LamportTime(self.query_clock);
    let id: u32 = self.rng.next_u32();

    let n = self.members.states.len();
    let mult = self.opts.query_timeout_mult();
    let log_factor = (crate::mathf::ceil(crate::mathf::log10(n as f64 + 1.0)) as u32).max(1);
    let timeout = core::time::Duration::from_millis(200) * mult as u32 * log_factor;

    let q = QueryMessage {
      ltime,
      id,
      from: memberlist_proto::Node::new(
        t.endpoint_ref().local_id_ref().clone(),
        t.endpoint_ref().advertise_ref().clone(),
      ),
      filters: vec![],
      flags: QueryFlag::empty(),
      relay_factor: 0,
      timeout,
      name,
      payload,
    };

    let encoded = AnyMessage::<I, A>::Query(q.clone())
      .encode()
      .map_err(Error::UserEventEncode)?;
    if encoded.len() > self.opts.query_size_limit() {
      return Err(Error::QueryTooLarge(
        encoded.len(),
        self.opts.query_size_limit(),
      ));
    }

    let query_id = QueryId { ltime, id };
    let deadline = now + timeout;

    // Capture the current member count before we push the pending query.
    // For Key queries this becomes KeyResponse.num_nodes (mirrors Go serf
    // key_manager.go `streamKeyResponse` which reads `this.num_members()`
    // at the moment the query is issued).
    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    let num_nodes = self.members.states.len();

    #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
    let key_tally = if purpose == QueryPurpose::Key {
      Some(KeyResponseTally {
        num_err: 0,
        keys: crate::FxHashMap::default(),
        primary_keys: crate::FxHashMap::default(),
        messages: crate::FxHashMap::default(),
      })
    } else {
      None
    };

    self.pending_queries.push(PendingQuery {
      kind: purpose,
      deadline,
      responses: crate::FxHashMap::default(),
      acks: crate::FxHashMap::default(),
      query_id,
      request_ack: false, // Internal queries never request per-hop acks.
      conflict_matching: 0,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      num_nodes,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      key_tally,
    });

    // QueryOrigin::Local bypasses the inbound cap so the initiating node always
    // self-processes its own internal query.
    self.drain_now = now;
    self.handle_query(t, q, QueryOrigin::Local);

    // Ignoring Err: queue back-pressure is a flow-control decision, not fatal.
    let _ = t.queue_user_broadcast_ranked(1, encoded);

    Ok(query_id)
  }

  /// Issue a conflict-resolution query for the local node id.
  ///
  /// Called from `on_inner_event` when `IE::NodeConflict` fires and
  /// `enable_id_conflict_resolution` is set.  Broadcasts a `_serf_conflict`
  /// query carrying the local id; peers respond with their view of that id's
  /// address.  When the deadline fires, `close_conflict_query` tallies votes and
  /// emits `Event::Shutdown` if the local node lost the majority.
  fn resolve_node_conflict<T>(&mut self, t: &mut T, now: Instant)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    let local_id = t.endpoint_ref().local_id_ref().clone();
    let payload = match local_id.encode_to_bytes() {
      Ok(b) => b,
      Err(_) => return,
    };
    // Ignoring Err: encoding or queue failures are best-effort; if we cannot
    // broadcast the conflict query the cluster will simply time out the conflict.
    let _ = self.internal_query(
      t,
      SmolStr::new("_serf_conflict"),
      payload,
      QueryPurpose::Conflict,
      now,
    );
  }

  /// Respond autonomously to a received `_serf_conflict` query.
  ///
  /// The local node looks up the conflicting id in its membership store and
  /// sends a directed `QueryResponseMessage` carrying a `ConflictResponseMessage`
  /// back to the originator.  If the conflicting id is the local id itself, no
  /// response is sent (the originator does not vote in its own conflict).
  ///
  /// `conflict_id` is pre-decoded and exact-consumption-validated by the
  /// internal-query payload gate in `handle_query`; it is NOT re-decoded here.
  fn handle_conflict_query<T>(&mut self, t: &mut T, msg: &QueryMessage<I, A>, conflict_id: I)
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    // The originator does not respond to its own conflict query.
    if &conflict_id == t.endpoint_ref().local_id_ref() {
      return;
    }

    // Look up the conflicting node's info in the membership store.
    let member_node = match self.members.states.get(&conflict_id) {
      Some(ms) => ms.member().node().clone(),
      None => return, // Unknown node: no response.
    };

    // Build and encode the ConflictResponseMessage.
    let resp_msg = ConflictResponseMessage::new(member_node);
    let conflict_resp_bytes = match AnyMessage::<I, A>::ConflictResponse(resp_msg).encode() {
      Ok(b) => b,
      Err(_) => return,
    };

    // Build and encode the QueryResponseMessage to send to the originator.
    let local_node = memberlist_proto::Node::new(
      t.endpoint_ref().local_id_ref().clone(),
      t.endpoint_ref().advertise_ref().clone(),
    );
    let qresp = QueryResponseMessage {
      ltime: msg.ltime,
      id: msg.id,
      from: local_node,
      flags: QueryFlag::empty(),
      payload: conflict_resp_bytes,
    };
    let qresp_encoded = match AnyMessage::<I, A>::QueryResponse(qresp).encode() {
      Ok(b) => b,
      Err(_) => return,
    };

    // Directed send to the originator (no relay for internal queries).
    let dest_addr = msg.from.addr_ref().clone();
    // Ignoring Err: directed-send failure on the conflict-response path is
    // best-effort; the originator will simply count this node as non-responding.
    let _ = t.send_user_packet(dest_addr, qresp_encoded);
  }

  /// Fold a conflict-resolution response into the matching `PendingQuery`.
  ///
  /// The payload is a serf-framed `ConflictResponseMessage`.  If the reported
  /// member's address matches the local advertise address, `conflict_matching`
  /// is incremented.
  fn handle_conflict_response_fold<T>(&mut self, t: &mut T, query_id: QueryId, payload: Bytes)
  where
    T: Reliable<I, A>,
    A: PartialEq,
  {
    // Exact-consumption decode: the validation gate in handle_query_response
    // already enforced exact consumption before counting; re-check here for
    // defence-in-depth (the fold is called only after the gate passed, so this
    // is a redundant safety net that adds no overhead on the hot path).
    let msg = match AnyMessage::<I, A>::decode_with_consumed(&payload) {
      Ok((AnyMessage::ConflictResponse(m), consumed)) if consumed == payload.len() => m,
      _ => return,
    };

    let local_addr = t.endpoint_ref().advertise_ref().clone();
    let pending = match self
      .pending_queries
      .iter_mut()
      .find(|pq| pq.query_id == query_id)
    {
      Some(p) => p,
      None => return,
    };

    if msg.member.addr_ref() == &local_addr {
      pending.conflict_matching += 1;
    }
  }

  /// Fold a key-management response into the matching `PendingQuery`.
  ///
  /// The payload is a serf-framed `KeyResponseMessage`.  Errors, keys, and
  /// primary-key reports are merged into `key_tally`.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  fn handle_key_response_fold(&mut self, query_id: QueryId, responder_id: I, payload: Bytes)
  where
    I: Eq + core::hash::Hash,
  {
    // Exact-consumption decode: the validation gate in handle_query_response
    // already enforced exact consumption before counting; re-check here for
    // defence-in-depth.
    let key_msg = match AnyMessage::<I, A>::decode_with_consumed(&payload) {
      Ok((AnyMessage::KeyResponse(m), consumed)) if consumed == payload.len() => m,
      _ => return,
    };

    let pending = match self
      .pending_queries
      .iter_mut()
      .find(|pq| pq.query_id == query_id)
    {
      Some(p) => p,
      None => return,
    };

    let tally = match &mut pending.key_tally {
      Some(t) => t,
      None => return, // Not a Key query: ignore.
    };

    if !key_msg.result {
      tally.num_err += 1;
      if !key_msg.message.is_empty() {
        tally.messages.insert(responder_id, key_msg.message);
      }
    }

    for k in key_msg.keys {
      *tally.keys.entry(k).or_insert(0) += 1;
    }

    if let Some(pk) = key_msg.primary_key {
      *tally.primary_keys.entry(pk).or_insert(0) += 1;
    }
  }

  /// Close all pending queries whose deadline has elapsed.
  ///
  /// `App` queries expire silently; `Conflict` queries trigger vote tallying;
  /// `Key` queries (encryption-gated) emit `Event::KeyResponse`.
  fn fire_due_query_closes(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    let mut i = 0;
    while i < self.pending_queries.len() {
      if now >= self.pending_queries[i].deadline {
        let pq = self.pending_queries.swap_remove(i);
        match pq.kind {
          QueryPurpose::App => {
            // App queries expire silently — no event emitted.
          }
          QueryPurpose::Conflict => {
            self.close_conflict_query(&pq);
          }
          #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
          QueryPurpose::Key => {
            self.close_key_query(pq);
          }
        }
        // A lost conflict close transitions the machine to Shutdown mid-loop.
        // Stop before closing any remaining due query so nothing is emitted
        // after the terminal Event::Shutdown (e.g. a same-deadline KeyResponse):
        // the delivery contract is that nothing follows Event::Shutdown.
        if self.state.is_shutdown() {
          break;
        }
        // Do not advance i: swap_remove replaced index i with the last element.
      } else {
        i += 1;
      }
    }
  }

  /// Tally a closed conflict-resolution query and emit `Event::Shutdown` if
  /// the local node lost the majority vote.
  ///
  /// "Lost" means strictly fewer than `(num_responses / 2) + 1` respondents
  /// reported our local advertise address as the canonical address for our id.
  ///
  /// **FIX (zero-response guard):** when `num_resp == 0` (all responses were
  /// malformed and dropped before counting, or no peers responded at all), the
  /// outcome is inconclusive — we cannot compute a meaningful majority.  The
  /// local node keeps its name; do NOT emit `Event::Shutdown`.  Requiring
  /// `num_resp > 0` before comparing prevents the `majority = 1, matching = 0`
  /// false-shutdown that would otherwise occur.
  fn close_conflict_query(&mut self, pq: &PendingQuery<I>) {
    let num_resp = pq.responses.len();
    // Zero valid responses → inconclusive; the local node keeps its name.
    if num_resp == 0 {
      return;
    }
    let matching = pq.conflict_matching;
    let majority = (num_resp / 2) + 1;
    if matching >= majority {
      // Won — the local node is the canonical holder.
      return;
    }
    // We lost the vote.  Perform serf's documented forced Alive/Leaving →
    // Shutdown transition — Go serf's conflict-loss branch calls `shutdown()`,
    // which sets the state — BEFORE emitting, so the event is born from an
    // already-dead machine and the chokepoints (commands / ingress / timers)
    // observe Shutdown for the rest of this drain.  The driver remains
    // responsible for stopping I/O and delivering this buffered event.
    //
    // Drop each coalescer's not-yet-flushed batch here, before the terminal
    // Event::Shutdown: Go serf's `shutdown()` tears down the coalescer goroutine,
    // abandoning its buffered events rather than delivering them, and the
    // delivery contract forbids emitting anything after Event::Shutdown.  Ingress
    // is already inert post-Shutdown, so nothing can refill them.
    if let Some(c) = self.member_coalescer.as_mut() {
      c.reset();
    }
    if let Some(c) = self.user_coalescer.as_mut() {
      c.reset();
    }
    self.state = SerfState::Shutdown;
    self.pending_events.push_back(Event::Shutdown);
  }

  /// Materialize a closed key-management query into `Event::KeyResponse`.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  fn close_key_query(&mut self, pq: PendingQuery<I>)
  where
    I: Clone,
  {
    use crate::event::KeyResponse;
    let tally = pq.key_tally.unwrap_or_else(|| KeyResponseTally {
      num_err: 0,
      keys: crate::FxHashMap::default(),
      primary_keys: crate::FxHashMap::default(),
      messages: crate::FxHashMap::default(),
    });
    let num_resp = pq.responses.len();
    // num_nodes was captured at query-issue time from members.states.len()
    // (mirrors Go serf key_manager.go `streamKeyResponse` num_nodes init).
    let kr = KeyResponse {
      num_nodes: pq.num_nodes,
      num_resp,
      num_err: tally.num_err,
      keys: tally.keys,
      primary_keys: tally.primary_keys,
      messages: tally.messages,
    };
    self.pending_events.push_back(Event::KeyResponse(kr));
  }

  /// Issue a cluster-wide `install_key` query for `key`.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub(crate) fn install_key<T>(
    &mut self,
    t: &mut T,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    let payload = self.encode_key_request(Some(key))?;
    self.internal_query(
      t,
      SmolStr::new("_serf_install_key"),
      payload,
      QueryPurpose::Key,
      now,
    )
  }

  /// Issue a cluster-wide `use_key` query to promote `key` to primary.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub(crate) fn use_key<T>(
    &mut self,
    t: &mut T,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    let payload = self.encode_key_request(Some(key))?;
    self.internal_query(
      t,
      SmolStr::new("_serf_use_key"),
      payload,
      QueryPurpose::Key,
      now,
    )
  }

  /// Issue a cluster-wide `remove_key` query to remove `key` from all nodes.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub(crate) fn remove_key<T>(
    &mut self,
    t: &mut T,
    key: memberlist_proto::SecretKey,
    now: Instant,
  ) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    let payload = self.encode_key_request(Some(key))?;
    self.internal_query(
      t,
      SmolStr::new("_serf_remove_key"),
      payload,
      QueryPurpose::Key,
      now,
    )
  }

  /// Issue a cluster-wide `list_keys` query to enumerate installed keys.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub(crate) fn list_keys<T>(&mut self, t: &mut T, now: Instant) -> Result<QueryId, Error>
  where
    T: Reliable<I, A>,
    I: Clone + Data,
    A: Clone + Data,
  {
    let payload = self.encode_key_request(None)?;
    self.internal_query(
      t,
      SmolStr::new("_serf_list_keys"),
      payload,
      QueryPurpose::Key,
      now,
    )
  }

  /// Encode a `KeyRequestMessage` as serf-framed bytes.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  fn encode_key_request(&self, key: Option<memberlist_proto::SecretKey>) -> Result<Bytes, Error>
  where
    I: Data,
    A: Data,
  {
    let req = KeyRequestMessage::new(key);
    AnyMessage::<I, A>::KeyRequest(req)
      .encode()
      .map_err(Error::UserEventEncode)
  }

  // ── Conflict-resolution + key-management test helpers ─────────────────────

  /// Insert a synthetic `Conflict` `PendingQuery` into `pending_queries`.
  ///
  /// Returns the `QueryId` so the test can fold responses and fire the close.
  #[cfg(test)]
  pub(crate) fn test_register_conflict_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
    A: Clone,
  {
    let ltime = LamportTime(self.query_clock);
    let id: u32 = 99;
    let query_id = QueryId { ltime, id };
    self.pending_queries.push(PendingQuery {
      kind: QueryPurpose::Conflict,
      deadline,
      responses: crate::FxHashMap::default(),
      acks: crate::FxHashMap::default(),
      query_id,
      request_ack: false,
      conflict_matching: 0,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      num_nodes: 0,
      #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
      key_tally: None,
    });
    query_id
  }

  /// Insert a synthetic `Key` `PendingQuery` into `pending_queries`.
  ///
  /// Returns the `QueryId` so the test can inject responses and fire the close.
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(all(test, any(feature = "aes-gcm", feature = "chacha20-poly1305")))]
  pub(crate) fn test_register_key_query(&mut self, deadline: Instant) -> QueryId
  where
    I: Clone,
    A: Clone,
  {
    let ltime = LamportTime(self.query_clock);
    let id: u32 = 77;
    let query_id = QueryId { ltime, id };
    let num_nodes = self.members.states.len();
    self.pending_queries.push(PendingQuery {
      kind: QueryPurpose::Key,
      deadline,
      responses: crate::FxHashMap::default(),
      acks: crate::FxHashMap::default(),
      query_id,
      request_ack: false,
      conflict_matching: 0,
      num_nodes,
      key_tally: Some(KeyResponseTally {
        num_err: 0,
        keys: crate::FxHashMap::default(),
        primary_keys: crate::FxHashMap::default(),
        messages: crate::FxHashMap::default(),
      }),
    });
    query_id
  }

  /// Fold a synthetic conflict response for test purposes.
  ///
  /// `responder_id` is used as the dedup key; `agrees` controls whether the
  /// `conflict_matching` counter is bumped.
  #[cfg(test)]
  pub(crate) fn test_fold_conflict_response(
    &mut self,
    query_id: QueryId,
    responder_id: I,
    agrees: bool,
  ) where
    I: Clone,
  {
    let pending = match self
      .pending_queries
      .iter_mut()
      .find(|pq| pq.query_id == query_id)
    {
      Some(p) => p,
      None => return,
    };
    pending.responses.insert(responder_id, ());
    if agrees {
      pending.conflict_matching += 1;
    }
  }

  /// Return the number of valid responses counted for a pending query (test adapter).
  ///
  /// This is `pq.responses.len()` — the deduplicated set that forms the
  /// denominator in the majority calculation.
  #[cfg(test)]
  pub(crate) fn test_pending_query_response_count(&self, query_id: QueryId) -> usize {
    self
      .pending_queries
      .iter()
      .find(|pq| pq.query_id == query_id)
      .map_or(0, |pq| pq.responses.len())
  }

  /// Return the number of ids in the query-buffer slot for `ltime` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_query_slot_len(&self, ltime: u64) -> usize {
    let bltime = self.query_buffer.buffer.len() as u64;
    let idx = (ltime % bltime) as usize;
    match &self.query_buffer.buffer[idx] {
      Some(q) if q.ltime.0 == ltime => q.query_ids.len(),
      _ => 0,
    }
  }

  /// Directly invoke `fire_due_query_closes` (test adapter).
  #[cfg(test)]
  pub(crate) fn test_fire_due_query_closes(&mut self, now: Instant)
  where
    I: Clone,
    A: Clone,
  {
    self.fire_due_query_closes(now);
  }

  // ── Timeout tick-order test helpers ──────────────────────────────────────

  /// Inject a synthetic `NodeJoined` event for `id` directly through the sieve.
  ///
  /// Sets `drain_now = now` then calls `handle_node_join` on a synthetic
  /// `NodeState`, simulating an inner event that arrives in the same tick as
  /// a serf deadline.  Used to verify the H1b drain-before-deadline ordering.
  #[cfg(test)]
  pub(crate) fn test_inject_inner_joined(&mut self, id: I, now: Instant)
  where
    I: Clone,
    A: Clone + From<core::net::SocketAddr>,
  {
    use std::sync::Arc;
    let addr: A = core::net::SocketAddr::from(([127, 0, 0, 1], 0u16)).into();
    let ns = Arc::new(memberlist_proto::typed::NodeState::new(
      id,
      addr,
      memberlist_proto::typed::State::Alive,
    ));
    self.drain_now = now;
    self.handle_node_join(&ns, now);
  }

  /// Enqueue raw `bytes` on the intent broadcast tier (rank 0, highest priority).
  #[cfg(test)]
  pub(crate) fn test_enqueue_intent_broadcast<T>(&mut self, t: &mut T, bytes: Bytes)
  where
    T: Reliable<I, A>,
  {
    // Ignoring Err: test helper; queue back-pressure is not exercised here.
    let _ = t.queue_user_broadcast_ranked(0, bytes);
  }

  /// Enqueue raw `bytes` on the query broadcast tier (rank 1).
  #[cfg(test)]
  pub(crate) fn test_enqueue_query_broadcast<T>(&mut self, t: &mut T, bytes: Bytes)
  where
    T: Reliable<I, A>,
  {
    // Ignoring Err: test helper; queue back-pressure is not exercised here.
    let _ = t.queue_user_broadcast_ranked(1, bytes);
  }

  // ── Snapshot replay → Endpoint load (G5 + G10) ───────────────────────────

  /// Apply a [`ReplayResult`] to this endpoint after restart.
  ///
  /// Mirrors Go serf `serf.go` `handleRejoin` + the clock-recovery section of
  /// `open_and_replay_snapshot` (~lines 130-160).
  ///
  /// **G5 — clock floors:**
  /// - The member clock is advanced to at least `replay.last_clock`.
  /// - `event_buffer.min_time` is set to `last_event_clock + 1` so that any
  ///   buffered user events from before the snapshot are not replayed
  ///   (prevents duplicate `Event::User` deliveries after restart).
  /// - `query_buffer.min_time` is set to `last_query_clock + 1` for the same
  ///   reason on the query dedup path.
  ///
  /// **G10 — rejoin dials (skip self):**
  /// For each node in `replay.alive_nodes` whose id is NOT the local node id,
  /// the machine calls `inner.start_push_pull(addr, Join, now)`.  The inner
  /// then emits `Event::DialRequested` which the sieve passes through to the
  /// driver as `Event::DialRequested(DialPassthrough { … })`.  This mirrors
  /// Go serf `handleRejoin` which shuffles `AliveNodes` and skips
  /// `node.Name == s.config.NodeName` before dialling each peer.
  ///
  /// The self-skip check compares node ids (not addresses), consistent with
  /// the rest of serf's membership logic.
  ///
  /// The local state is marked dirty so the next push-pull egress ships the
  /// recovered clock state.
  ///
  /// Refuses with [`Error::Shutdown`] on a machine that lost an id-conflict vote,
  /// before any mutation: replay is a public origination path (it advances the
  /// Lamport clocks, dirties the snapshot, and dials every recorded peer), so a
  /// terminated node must not be able to resurrect itself through it.
  pub(crate) fn load_snapshot<T>(
    &mut self,
    t: &mut T,
    replay: crate::snapshot::ReplayResult<I, A>,
    now: Instant,
  ) -> Result<(), Error>
  where
    T: Reliable<I, A>,
    A: Clone,
  {
    // Terminal-state gate: refuse before any mutation so a Shutdown conflict
    // loser can neither advance its clocks nor originate rejoin dials.
    self.ensure_not_shutdown()?;

    // G5: advance the member clock to at least last_clock.
    // Whole-message drop gate: a corrupt or adversarially-crafted snapshot with
    // an unacceptable ltime must not advance the local clock.
    if ltime_is_acceptable(replay.last_clock.0) {
      witness(&mut self.clock, replay.last_clock.0);
    }
    self.mark_local_state_dirty();

    // G5: set event min_time so pre-snapshot events are not replayed.
    if ltime_is_acceptable(replay.last_event_clock.0) {
      let event_min = replay.last_event_clock.0.saturating_add(1);
      if event_min > self.event_buffer.min_time {
        self.event_buffer.min_time = event_min;
      }
      // Advance event_clock to at least the snapshot floor so that user_event()
      // stamps a ltime >= min_time.  Without this, event_clock stays at 0 and
      // every new event is dropped as "too old" by handle_user_event.
      witness(&mut self.event_clock, replay.last_event_clock.0);
    }

    // G5: set query min_time so pre-snapshot queries are not replayed.
    if ltime_is_acceptable(replay.last_query_clock.0) {
      let query_min = replay.last_query_clock.0.saturating_add(1);
      if query_min > self.query_buffer.min_time {
        self.query_buffer.min_time = query_min;
      }
      // Advance query_clock to at least the snapshot floor so that query()
      // stamps a ltime >= min_time.
      witness(&mut self.query_clock, replay.last_query_clock.0);
    }

    // G10: dial each alive peer (skip self) so the node re-joins the cluster.
    // The inner emits Event::DialRequested; the sieve passes it through to the
    // driver.  The driver owns the actual network dial.
    let local_id = t.endpoint_ref().local_id_ref().clone();
    for node in replay.alive_nodes {
      if node.id_ref() == &local_id {
        // Self-skip: the local node is already "alive" by definition.
        continue;
      }
      let addr = node.addr_ref().clone();
      // Capture for test assertions before calling start_push_pull.
      #[cfg(test)]
      self.rejoin_dials.push(addr.clone());
      t.start_push_pull(addr, PushPullKind::Join, now);
      self.drain_inner(t);
    }
    Ok(())
  }

  // ── PingCompleted handler (G9 both halves) ───────────────────────────────

  /// Handle a `PingCompleted` event from the inner memberlist Endpoint.
  ///
  /// Implements G9 (both halves):
  ///
  /// **Half 1 — update local Vivaldi model**: Decodes the remote peer's
  /// coordinate from `payload[1..]` (byte 0 is `PING_VERSION = 1`), then
  /// calls `coord_client.update(node_id, &remote_coord, rtt, &mut self.rng)`.
  /// Stores the remote peer's updated coordinate in `coord_cache[node_id]`.
  ///
  /// **Half 2 — refresh own ack payload**: After updating the local model,
  /// re-encodes the new local coordinate as `[PING_VERSION] ++ pb_bytes` and
  /// calls `inner.set_ack_payload` so the next probe ack piggybacks the fresh
  /// coordinate automatically.
  ///
  /// A `payload` that is empty, has an unexpected version byte, or fails to
  /// decode is dropped silently (mirrors Go serf `delegate.go` `notify_ping_complete`).
  ///
  /// No-op when `coord_client` is `None` (coordinates disabled at construction).
  #[cfg(feature = "coordinates")]
  fn handle_ping_completed<T>(
    &mut self,
    t: &mut T,
    node_id: &I,
    rtt: core::time::Duration,
    payload: &Bytes,
  ) where
    T: Reliable<I, A>,
  {
    use crate::{bridge::coordinate_from_pb, messages::serf::v1 as pb};
    use buffa::Message as _;

    // Guard: no-op when coordinates are disabled at construction.
    let cc = match self.coord_client.as_mut() {
      Some(c) => c,
      None => return,
    };

    // Validate payload version byte (mirrors delegate.go notify_ping_complete).
    if payload.is_empty() || payload[0] != PING_VERSION {
      return;
    }

    // Decode the remote peer's coordinate from payload[1..].
    let remote_coord = match pb::Coordinate::decode_from_slice(&payload[1..]) {
      Ok(pb_coord) => coordinate_from_pb(&pb_coord),
      Err(_) => return,
    };

    // Half 1: update the local Vivaldi model with (peer, remote_coord, rtt).
    let new_local_coord = match cc.update(node_id, &remote_coord, rtt, &mut self.rng) {
      Ok(c) => c,
      Err(_) => return,
    };

    // Cache the remote peer's coordinate for `cached_coordinate()` queries.
    self.coord_cache.insert(node_id.clone(), remote_coord);

    // Half 2: refresh own ack payload so future probe acks piggyback the
    // updated local coordinate.
    let ack_payload = coord_ack_payload(new_local_coord);
    // Ignoring Err: set_ack_payload only fails when Leaving/Left/Shutdown.
    // PingCompleted arrives on the probe path, active only while Alive.
    let _ = t.set_ack_payload(ack_payload);
  }

  // ── Snapshot replay test helpers ─────────────────────────────────────────

  /// Return the list of addresses dialled by `load_snapshot` for rejoin
  /// (test adapter for G10 assertions).
  ///
  /// Each call to `load_snapshot` appends to this list, so the test can
  /// inspect which peers were targeted for the rejoin dial without needing
  /// a live transport layer.
  #[cfg(test)]
  pub(crate) fn test_rejoin_dials(&self) -> Vec<A>
  where
    A: Clone,
  {
    self.rejoin_dials.clone()
  }

  /// Synthesise a `PingCompleted` update and drive it through
  /// `handle_ping_completed` (test adapter).
  ///
  /// Sets `drain_now = Instant::ORIGIN` before the call.
  #[cfg(all(feature = "coordinates", test))]
  pub(crate) fn test_ping_completed<T>(
    &mut self,
    t: &mut T,
    node_id: I,
    rtt: core::time::Duration,
    payload: Bytes,
  ) where
    T: Reliable<I, A>,
  {
    self.drain_now = memberlist_proto::Instant::ORIGIN;
    self.handle_ping_completed(t, &node_id, rtt, &payload);
  }
}

// ── Tags helper ──────────────────────────────────────────────────────────────

/// Attempt to decode serf `Tags` from raw meta bytes.
///
/// The meta field in a memberlist `NodeState` carries buffa-encoded `pb::Tags`
/// (a protobuf map of string→string entries).  On decode failure the caller
/// falls back to empty tags — FIX over the oracle: the oracle returns early
/// on tag-failure and silently skips the join/update, which is a footgun;
/// we prefer to join/update with empty tags instead.
///
/// Mirrors Go serf `types.go` `Tags` decode path:
/// `Tags.Decode(n.Meta)` (oracle: `base.go` line ~1227, `~1587`).
fn decode_tags_from_meta(bytes: &[u8]) -> Option<Tags> {
  use crate::messages::serf::v1 as pb;
  use buffa::Message as _;

  let pb_tags = pb::Tags::decode_from_slice(bytes).ok()?;
  Some(tags_from_pb(&pb_tags))
}

// ── Coordinates (G9 / G13 / A5) ──────────────────────────────────────────────

/// Wire version byte that prefixes every ack payload carrying a Vivaldi coordinate.
///
/// Mirrors Go serf `delegate.go` `PingVersion` constant (= 1).
/// The payload layout is `[PING_VERSION] ++ pb::Coordinate`.
#[cfg(feature = "coordinates")]
const PING_VERSION: u8 = 1;

/// Build the ack-payload bytes `[PING_VERSION] ++ encode(coord)`.
///
/// Called at construction and after each successful coordinate update so the
/// inner Endpoint's ack piggybacks the current local coordinate on every probe.
#[cfg(feature = "coordinates")]
pub(crate) fn coord_ack_payload(coord: crate::typed::Coordinate) -> Bytes {
  use crate::bridge::coordinate_to_pb;
  use buffa::Message as _;
  let pb = coordinate_to_pb(&coord);
  let encoded = pb.encode_to_vec();
  let mut buf = Vec::with_capacity(1 + encoded.len());
  buf.push(PING_VERSION);
  buf.extend_from_slice(&encoded);
  Bytes::from(buf)
}

// ── Coordinate public accessors ───────────────────────────────────────────────

impl<I, A, R, D> Endpoint<I, A, R, D>
where
  I: Clone + Eq + core::hash::Hash,
  D: DropCounter,
{
  /// Return the local node's current Vivaldi coordinate.
  ///
  /// `None` when the `coordinates` feature is compiled out or when
  /// `Options::with_disable_coordinates(true)` was set at construction.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub fn get_coordinate(&self) -> Option<crate::typed::Coordinate> {
    self.coord_client.as_ref().map(|cc| cc.get_coordinate())
  }

  /// Return the most-recently-observed Vivaldi coordinate of `node`.
  ///
  /// Updated on each successful `PingCompleted` RTT feed from that peer.
  /// Returns `None` when coordinates are disabled or when no RTT sample
  /// has been received from `node` yet.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  pub fn cached_coordinate(&self, node: &I) -> Option<crate::typed::Coordinate> {
    self.coord_cache.get(node).cloned()
  }
}

pub(crate) mod reliable;

// These suites drive the serf logic through `crate::StreamEndpoint`, which
// composes the plain-TCP reliable coordinator and is therefore `tcp`-gated.
#[cfg(all(test, feature = "tcp"))]
mod serf_parity_tests;
#[cfg(all(test, feature = "tcp"))]
mod tests;
