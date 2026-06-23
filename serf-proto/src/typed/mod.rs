//! Typed serf message shapes — the canonical in-memory representations.
//!
//! These types are what the serf state machine works with directly. The
//! `bridge` module converts them to/from the buffa-generated codec types.

use std::collections::HashMap;

use bytes::Bytes;
use smol_str::SmolStr;

use crate::LamportTime;
use memberlist_proto::Node;

/// A user-generated event broadcast through the serf cluster.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UserEventMessage {
  /// The lamport clock value at the time the event was emitted.
  pub ltime: LamportTime,
  /// "Can Coalesce" — whether the event may be merged with later identical events.
  pub cc: bool,
  /// The event name.
  pub name: SmolStr,
  /// The event payload.
  pub payload: Bytes,
}

// ── QueryFlag ────────────────────────────────────────────────────────────────

bitflags::bitflags! {
  /// Control flags for a serf query message.
  ///
  /// Rides the wire as a `uint32`; the bit positions are identical to the
  /// legacy `serf-core` constants so tooling that inspects raw integers stays
  /// compatible.
  #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
  pub struct QueryFlag: u32 {
    /// Ack — force the receiver to send an acknowledgement back.
    const ACK          = 1 << 0;
    /// NoBroadcast — suppress re-broadcast of the query; useful for targeted
    /// unicast queries to individual members.
    const NO_BROADCAST = 1 << 1;
  }
}

// ── Coordinate ───────────────────────────────────────────────────────────────

/// A Vivaldi network-coordinate wire point.
///
/// Holds the serialisable fields of the Vivaldi coordinate; the Vivaldi
/// client engine (`CoordinateClient`) is not part of the wire protocol.
/// All values are in units of seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Coordinate {
  /// Euclidean portion of the coordinate (variable-length f64 vector).
  pub vec: Vec<f64>,
  /// Confidence in the coordinate estimate (dimensionless).
  pub error: f64,
  /// Distance offset derived from peer observations (seconds).
  pub adjustment: f64,
  /// Non-Euclidean height term modelling access-link latency (seconds).
  pub height: f64,
}

impl Default for Coordinate {
  fn default() -> Self {
    Self {
      vec: Vec::new(),
      error: 0.0,
      adjustment: 0.0,
      height: 0.0,
    }
  }
}

// ── Tags ─────────────────────────────────────────────────────────────────────

/// Node metadata: a string→string map gossiped via node meta.
///
/// Thin newtype over [`HashMap<SmolStr, SmolStr>`] so the rest of the crate
/// can name the concept without spelling out the full map type.
/// Proto3 `map` wire encoding does not guarantee key order, so encoded bytes
/// are not canonical for a given set of tags.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tags(pub HashMap<SmolStr, SmolStr>);

impl Tags {
  /// Creates an empty `Tags` map.
  pub fn new() -> Self {
    Self(HashMap::new())
  }

  /// Creates a `Tags` map with the given initial capacity.
  pub fn with_capacity(cap: usize) -> Self {
    Self(HashMap::with_capacity(cap))
  }

  /// Returns the number of tag entries.
  pub fn len(&self) -> usize {
    self.0.len()
  }

  /// Returns `true` if no tags are set.
  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }
}

impl<K, V> FromIterator<(K, V)> for Tags
where
  K: Into<SmolStr>,
  V: Into<SmolStr>,
{
  fn from_iter<T>(iter: T) -> Self
  where
    T: IntoIterator<Item = (K, V)>,
  {
    Self(iter.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
  }
}

// ── Filter ───────────────────────────────────────────────────────────────────

/// A tag-name / optional-regex pair for matching nodes by their tags.
///
/// When `expr` is `None` the filter matches any node that has the named tag,
/// regardless of its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagFilter {
  /// The tag key to match on.
  pub tag: SmolStr,
  /// Optional regex that the tag value must satisfy.  `None` = match any value.
  pub expr: Option<SmolStr>,
}

/// A single query-scoping predicate.
///
/// Exactly one variant is active per `Filter`.  The `Id` variant restricts
/// the query to the listed node ids; the `Tag` variant restricts it to nodes
/// whose tag satisfies the [`TagFilter`].
///
/// Generic over `I`: the node-id type.  When `I = SmolStr` this mirrors the
/// legacy hand-rolled codec; when `I` is a custom type it encodes each id via
/// `memberlist_proto::Data` (opaque `bytes` on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter<I> {
  /// Restrict responses to the listed node ids.
  Id(Vec<I>),
  /// Restrict responses to nodes whose tag value satisfies the filter.
  Tag(TagFilter),
}

// ── QueryMessage ──────────────────────────────────────────────────────────────

/// A query broadcast through the cluster, optionally scoped by filters.
///
/// Generic over `I` (node-id) and `A` (node-address); both must implement
/// `memberlist_proto::Data` so the embedded `Node<I,A>` and any `Filter<I>`
/// node-ids can be encoded as opaque `bytes` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryMessage<I, A> {
  /// The lamport clock value when the query was issued.
  pub ltime: LamportTime,
  /// Randomly generated query identifier used to correlate responses.
  pub id: u32,
  /// The node that originated the query.
  pub from: memberlist_proto::Node<I, A>,
  /// Optional list of node-id / tag predicates that scope which nodes respond.
  pub filters: Vec<Filter<I>>,
  /// Control flags (ACK, NO_BROADCAST, …).
  pub flags: QueryFlag,
  /// Number of relayed duplicate responses requested.
  pub relay_factor: u8,
  /// Maximum time allowed between delivery and response.
  pub timeout: std::time::Duration,
  /// Query name.
  pub name: SmolStr,
  /// Query payload.
  pub payload: Bytes,
}

impl<I, A> QueryMessage<I, A> {
  /// Returns `true` if the ACK flag is set.
  pub fn ack(&self) -> bool {
    self.flags.contains(QueryFlag::ACK)
  }

  /// Returns `true` if the NO_BROADCAST flag is set.
  pub fn no_broadcast(&self) -> bool {
    self.flags.contains(QueryFlag::NO_BROADCAST)
  }
}

// ── QueryResponseMessage ──────────────────────────────────────────────────────

/// A response to a [`QueryMessage`], sent back to the originator.
///
/// Generic over `I` (node-id) and `A` (node-address); both must implement
/// `memberlist_proto::Data` so the embedded `Node<I,A>` can be encoded as
/// opaque `bytes` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResponseMessage<I, A> {
  /// The lamport clock value when the response was emitted.
  pub ltime: LamportTime,
  /// Identifier of the query being responded to.
  pub id: u32,
  /// The node sending this response.
  pub from: memberlist_proto::Node<I, A>,
  /// Control flags (e.g. ACK to acknowledge the query).
  pub flags: QueryFlag,
  /// Optional response payload.
  pub payload: Bytes,
}

impl<I, A> QueryResponseMessage<I, A> {
  /// Returns `true` if the ACK flag is set (this message is an acknowledgement).
  pub fn ack(&self) -> bool {
    self.flags.contains(QueryFlag::ACK)
  }
}

// ── Membership messages ───────────────────────────────────────────────────────

/// Broadcast after a node joins the cluster to associate it with a lamport clock.
///
/// Generic over `I`: the node-id type, which must implement
/// `memberlist_proto::Data` so it can be encoded as opaque proto `bytes` in
/// the bridge layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinMessage<I> {
  /// The lamport clock value at the time the node joined.
  pub ltime: crate::LamportTime,
  /// The joining node's identifier.
  pub id: I,
}

impl<I> JoinMessage<I> {
  /// Construct a new `JoinMessage`.
  pub fn new(ltime: crate::LamportTime, id: I) -> Self {
    Self { ltime, id }
  }
}

/// Broadcast to signal the intent to leave the cluster.
///
/// Generic over `I`: the node-id type, which must implement
/// `memberlist_proto::Data` so it can be encoded as opaque proto `bytes` in
/// the bridge layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveMessage<I> {
  /// The lamport clock value at the time the leave was emitted.
  pub ltime: crate::LamportTime,
  /// The leaving node's identifier.
  pub id: I,
  /// Whether the leave is a prune (permanent removal) rather than a graceful leave.
  pub prune: bool,
}

impl<I> LeaveMessage<I> {
  /// Construct a new `LeaveMessage`.
  pub fn new(ltime: crate::LamportTime, id: I, prune: bool) -> Self {
    Self { ltime, id, prune }
  }
}

/// Carries the winning node in a node-name conflict tie-breaker.
///
/// Generic over `I` and `A`: the node-id and address types, which must
/// implement `memberlist_proto::Data` so the embedded `Node<I,A>` can be
/// encoded as opaque proto `bytes` in the bridge layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResponseMessage<I, A> {
  /// The winning node in the conflict resolution.
  pub member: Node<I, A>,
}

impl<I, A> ConflictResponseMessage<I, A> {
  /// Construct a new `ConflictResponseMessage`.
  pub fn new(member: Node<I, A>) -> Self {
    Self { member }
  }
}
