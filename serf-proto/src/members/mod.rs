//! Membership types for the serf state machine.
//!
//! [`MemberStatus`] is the per-node status within a serf cluster.
//! [`SerfState`] is the lifecycle state of the local serf endpoint (distinct
//! from any single node's status).  [`MemberState`] pairs a [`Member`] with
//! its lamport clock timestamp and optional leave wall-time. `Members` is
//! the in-memory store that the endpoint mutates as intents and inner
//! memberlist events arrive.
//!
//! Design: `MemberState.leave_time` and `NodeIntent.wall_time` carry a
//! `memberlist_proto::Instant` threaded in from the driver — no wall-clock
//! reads occur inside the pure machine.

use std::vec::Vec;

use crate::FxHashMap;

/// Hard cardinality cap on `Members::recent_intents`.
///
/// Unlike `states`, `left_members`, and `failed_members` (which are bounded by
/// the real cluster membership), `recent_intents` holds intents for nodes whose
/// inner memberlist `NodeJoined`/`NodeLeft` events have not yet arrived — and
/// those nodes may be entirely unknown to the local machine.  A flooder can
/// therefore grow `recent_intents` without bound by sending join/leave intents
/// for an unlimited stream of distinct unknown ids.
///
/// When the cap is reached the entry with the oldest `wall_time` is evicted
/// before inserting the new one.  Oldest-first eviction preserves the most
/// recently observed intents, which are the ones most likely to still be
/// relevant when the corresponding inner event fires.  Normal clusters will
/// never approach this limit; it is only reachable under adversarial flood.
pub(crate) const MAX_RECENT_INTENTS: usize = 8192;

use memberlist_proto::Instant;

use crate::LamportTime;

// ── MemberStatus ─────────────────────────────────────────────────────────────

/// The status of a node in the serf cluster.
///
/// Variants mirror the Go serf `MemberStatus` constants.  `None` is the
/// zero/default and indicates the node has never been seen.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Hash, Default, derive_more::IsVariant, derive_more::Display,
)]
#[display("{}", self.as_str())]
pub enum MemberStatus {
  /// No status (zero value).
  #[default]
  None,
  /// Node is alive and participating in the cluster.
  Alive,
  /// Node has announced that it is leaving.
  Leaving,
  /// Node has completed a graceful leave.
  Left,
  /// Node appears to have failed (no graceful leave observed).
  Failed,
}

impl MemberStatus {
  /// Returns a `'static` string representation of the status.
  ///
  /// Consistent with Go serf's `String()` on `MemberStatus`.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::None => "none",
      Self::Alive => "alive",
      Self::Leaving => "leaving",
      Self::Left => "left",
      Self::Failed => "failed",
    }
  }
}

// ── SerfState ─────────────────────────────────────────────────────────────────

/// The lifecycle state of the local serf endpoint.
///
/// Distinct from any single node's [`MemberStatus`].  Transitions:
/// `Alive → Leaving → Left` (graceful) or `Alive/Leaving → Shutdown` (forced).
/// Modelled after `SerfState` in Go serf `base.go`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, derive_more::IsVariant, derive_more::Display)]
#[display("{}", self.as_str())]
pub enum SerfState {
  /// Endpoint is alive and participating in the cluster.
  Alive,
  /// A graceful leave has been initiated but not yet completed.
  Leaving,
  /// The endpoint has fully left the cluster.
  Left,
  /// The endpoint has been shut down (abnormal or forced).
  ///
  /// The machine performs this forced transition itself when it loses an
  /// id-conflict vote (mirroring Go serf, whose conflict-loss branch calls
  /// `shutdown()`).  Once `Shutdown` the machine is terminal: it refuses every
  /// command that would originate cluster work (with [`Error::Shutdown`]), goes
  /// inert on ingress, and quiets its timers.  Only the already-buffered
  /// [`Event::Shutdown`] still drains via `poll_event`; the driver owns stopping
  /// I/O and delivering that event.
  ///
  /// [`Error::Shutdown`]: crate::endpoint::Error::Shutdown
  /// [`Event::Shutdown`]: crate::event::Event::Shutdown
  Shutdown,
}

impl SerfState {
  /// Returns a `'static` string representation of the state.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Alive => "alive",
      Self::Leaving => "leaving",
      Self::Left => "left",
      Self::Shutdown => "shutdown",
    }
  }
}

// ── Member ────────────────────────────────────────────────────────────────────

/// A single member of the serf cluster as seen by the local node.
///
/// Holds the node identity + address, the latest advertised tags, and the
/// current [`MemberStatus`].
#[derive(Debug, Clone)]
pub struct Member<I, A> {
  /// The underlying memberlist node (id + address).
  node: memberlist_proto::Node<I, A>,
  /// Key/value metadata advertised by this node.
  tags: crate::Tags,
  /// Current cluster status of this node.
  status: MemberStatus,
}

impl<I, A> Member<I, A> {
  /// Constructs a new `Member`.
  pub fn new(node: memberlist_proto::Node<I, A>, tags: crate::Tags, status: MemberStatus) -> Self {
    Self { node, tags, status }
  }

  /// The underlying memberlist node.
  pub const fn node(&self) -> &memberlist_proto::Node<I, A> {
    &self.node
  }

  /// The advertised tags.
  pub const fn tags(&self) -> &crate::Tags {
    &self.tags
  }

  /// The current status of this node.
  pub const fn status(&self) -> MemberStatus {
    self.status
  }
}

// ── MemberState ───────────────────────────────────────────────────────────────

/// Tracks the full per-node state used by the serf membership FSM.
///
/// `status_time` is the lamport clock value at which the last status change
/// was witnessed.  `leave_time` is the driver-threaded `Instant` at which the
/// node was observed leaving or failing; it is `None` while the node is alive.
#[derive(Debug, Clone)]
pub struct MemberState<I, A> {
  member: Member<I, A>,
  /// Lamport clock time of last received status-change message.
  status_time: LamportTime,
  /// Wall-clock (driver-threaded `Instant`) at which the leave/failure was
  /// observed.  `None` while the node is alive.
  leave_time: Option<Instant>,
}

impl<I, A> MemberState<I, A> {
  /// Constructs a new `MemberState`.
  pub fn new(member: Member<I, A>, status_time: LamportTime, leave_time: Option<Instant>) -> Self {
    Self {
      member,
      status_time,
      leave_time,
    }
  }

  /// The underlying `Member`.
  pub const fn member(&self) -> &Member<I, A> {
    &self.member
  }

  /// Mutable access to the underlying `Member` (used by the FSM to update tags/status).
  pub fn member_mut(&mut self) -> &mut Member<I, A> {
    &mut self.member
  }

  /// Current status (shortcut accessor).
  pub const fn status(&self) -> MemberStatus {
    self.member.status
  }

  /// Lamport clock time of the last status change.
  pub const fn status_time(&self) -> LamportTime {
    self.status_time
  }

  /// Sets the status_time.
  pub fn set_status_time(&mut self, t: LamportTime) {
    self.status_time = t;
  }

  /// The leave time, if the node is no longer alive.
  pub const fn leave_time(&self) -> Option<Instant> {
    self.leave_time
  }

  /// Sets the leave time.
  pub fn set_leave_time(&mut self, t: Option<Instant>) {
    self.leave_time = t;
  }

  /// Sets the status on the inner Member.
  pub fn set_status(&mut self, s: MemberStatus) {
    self.member.status = s;
  }
}

// ── IntentKind + NodeIntent ───────────────────────────────────────────────────

/// Discriminates between join and leave intents buffered in `recent_intents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentKind {
  /// A join intent (node announcing it is joining).
  Join,
  /// A leave intent (node announcing it is leaving).
  Leave,
}

/// A buffered join/leave intent for a node whose inner memberlist event has
/// not yet arrived.
///
/// Intents arrive over gossip before the underlying memberlist `NodeJoined` /
/// `NodeLeft` events; `recent_intents` holds the newest one per node so the
/// FSM can apply it when the inner event eventually fires.
///
/// `wall_time` is a driver-threaded `Instant` — no wall-clock reads in the
/// machine (Go serf used `time.Now()` here; the Sans-I/O design threads it in).
///
/// `sequence` is a monotonically increasing counter assigned by `Members` at
/// insertion/update time (see `Members::recent_intent_seq`).  It breaks ties in
/// the cap-eviction comparator when two entries share the same `wall_time` so
/// that eviction order is deterministic regardless of `HashMap` iteration order.
/// Oldest-inserted entry (smallest sequence) is evicted first.
#[derive(Debug, Clone, Copy)]
pub struct NodeIntent {
  kind: IntentKind,
  ltime: LamportTime,
  /// Driver-threaded instant at which this intent was received.
  wall_time: Instant,
  /// Monotonic insertion counter — breaks `wall_time` ties in eviction.
  /// Assigned by `Members::upsert_intent`; NOT drawn from the RNG.
  sequence: u64,
}

impl NodeIntent {
  /// Constructs a new `NodeIntent`.
  pub fn new(kind: IntentKind, ltime: LamportTime, wall_time: Instant, sequence: u64) -> Self {
    Self {
      kind,
      ltime,
      wall_time,
      sequence,
    }
  }

  /// The kind of intent (join or leave).
  pub const fn kind(&self) -> IntentKind {
    self.kind
  }

  /// The lamport clock value carried by this intent.
  pub const fn ltime(&self) -> LamportTime {
    self.ltime
  }

  /// The driver-threaded instant at which this intent was received.
  pub const fn wall_time(&self) -> Instant {
    self.wall_time
  }

  /// The monotonic insertion sequence number (for deterministic tie-breaking in eviction).
  pub const fn sequence(&self) -> u64 {
    self.sequence
  }
}

// ── Members ───────────────────────────────────────────────────────────────────

/// The in-memory membership store for the serf endpoint.
///
/// Mutated by the FSM handlers (`handle_node_join`, `handle_node_leave`, etc.)
/// and the intent reconciler.  All lookups are by node id `I`.
///
/// `left_members` and `failed_members` are index lists of ids for the reaper;
/// the full state lives in `states`.
pub(crate) struct Members<I, A>
where
  I: Eq + core::hash::Hash,
{
  /// Full state for every known node (alive, leaving, left, or failed).
  pub(crate) states: FxHashMap<I, MemberState<I, A>>,
  /// Buffered join/leave intents whose inner memberlist event has not yet
  /// arrived.  Newest ltime wins (upsert_intent).
  pub(crate) recent_intents: FxHashMap<I, NodeIntent>,
  /// Ids of nodes in the `Left` state, for tombstone reaping.
  pub(crate) left_members: Vec<I>,
  /// Ids of nodes in the `Failed` state, for reconnect and reaping.
  pub(crate) failed_members: Vec<I>,
  /// Monotonically increasing counter incremented on every `recent_intents`
  /// insert or update.  Assigned to `NodeIntent::sequence` so that cap-eviction
  /// breaks `wall_time` ties deterministically (oldest-inserted, smallest
  /// sequence, evicted first) regardless of `HashMap` iteration order.
  /// This is a plain counter — NOT derived from the RNG.
  pub(crate) recent_intent_seq: u64,
}

impl<I, A> Default for Members<I, A>
where
  I: Eq + core::hash::Hash,
{
  fn default() -> Self {
    Self {
      states: FxHashMap::default(),
      recent_intents: FxHashMap::default(),
      left_members: Vec::new(),
      failed_members: Vec::new(),
      recent_intent_seq: 0,
    }
  }
}

impl<I, A> Members<I, A>
where
  I: Eq + core::hash::Hash + Clone,
{
  /// Returns the most recent intent for a node + kind, if any.
  ///
  /// Used by `handle_node_join` to check whether a Leave intent arrived before
  /// the inner `NodeJoined` event (Go serf `base.go` `getRecentIntent`).
  pub(crate) fn recent_intent(&self, id: &I, kind: IntentKind) -> Option<LamportTime> {
    self
      .recent_intents
      .get(id)
      .filter(|i| i.kind == kind)
      .map(|i| i.ltime)
  }

  /// Inserts or updates the intent for a node, keeping the newest ltime.
  ///
  /// Returns `true` if the intent was inserted or updated (i.e. it was newer).
  /// Mirrors Go serf `base.go` `upsertIntent`.
  ///
  /// **Bounded memory**: when the map is at `MAX_RECENT_INTENTS` and a new
  /// entry would be inserted, the entry with the oldest `(wall_time, sequence)`
  /// is evicted first.  The composite key breaks `wall_time` ties by insertion
  /// order so eviction is deterministic regardless of `HashMap` iteration order.
  /// Oldest-inserted (smallest sequence) is evicted first among equal timestamps.
  ///
  /// Every insert AND every update (refresh of an existing node's intent)
  /// assigns a fresh sequence number so a re-observed intent is treated as
  /// "more recent" and won't be the next eviction candidate.
  pub(crate) fn upsert_intent(
    &mut self,
    id: &I,
    kind: IntentKind,
    ltime: LamportTime,
    wall_time: Instant,
  ) -> bool {
    match self.recent_intents.get_mut(id) {
      Some(existing) if ltime <= existing.ltime => false,
      Some(existing) => {
        existing.kind = kind;
        existing.ltime = ltime;
        existing.wall_time = wall_time;
        existing.sequence = self.recent_intent_seq;
        self.recent_intent_seq += 1;
        true
      }
      None => {
        // Enforce the cap before inserting a new entry.  Evict the entry with
        // the smallest (wall_time, sequence) to preserve the most recently
        // observed intents — those are most likely to still be relevant when
        // their corresponding inner memberlist event arrives.  The sequence
        // tie-break makes eviction order deterministic regardless of HashMap
        // iteration order (two entries with identical wall_time always evict
        // the older-inserted one first).
        if self.recent_intents.len() >= MAX_RECENT_INTENTS {
          if let Some(oldest_id) = self
            .recent_intents
            .iter()
            .min_by_key(|(_, intent)| (intent.wall_time(), intent.sequence()))
            .map(|(k, _)| k.clone())
          {
            self.recent_intents.remove(&oldest_id);
          }
        }
        let seq = self.recent_intent_seq;
        self.recent_intent_seq += 1;
        self
          .recent_intents
          .insert(id.clone(), NodeIntent::new(kind, ltime, wall_time, seq));
        true
      }
    }
  }
}

#[cfg(test)]
mod tests;
