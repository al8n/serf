//! Machine-output event types for the serf [`crate::endpoint::Endpoint`].
//!
//! The serf `Endpoint` surfaces cluster changes, user events, queries, and
//! control signals through this enum rather than callbacks, mirroring the
//! quinn-proto / memberlist-proto pull-style event model.

use std::{sync::Arc, vec::Vec};

use bytes::Bytes;
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
use memberlist_proto::SecretKey;
use memberlist_proto::{Instant, Node, StreamId, event::ExchangeCompleted};
use smol_str::SmolStr;

use crate::{LamportTime, UserEventMessage, members::Member};

// ── MemberEventKind ──────────────────────────────────────────────────────────

/// The kind of membership change carried by a [`MemberEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::IsVariant, derive_more::Display)]
#[display("{}", self.as_str())]
pub enum MemberEventKind {
  /// A node joined the cluster.
  Join,
  /// A node gracefully left the cluster.
  Leave,
  /// A node was detected as failed (no graceful leave).
  Failed,
  /// A node's tags or metadata were updated.
  Update,
  /// A node was reaped from the membership store (tombstone expired).
  Reap,
}

impl MemberEventKind {
  /// Returns a `'static` string representation.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Join => "join",
      Self::Leave => "leave",
      Self::Failed => "failed",
      Self::Update => "update",
      Self::Reap => "reap",
    }
  }
}

// ── MemberEvent ───────────────────────────────────────────────────────────────

/// Payload for [`Event::Member`]: a batch of membership changes of the same
/// kind.
///
/// Members are `Arc`-wrapped so the event is cheap to clone and multiple
/// consumers can inspect the same snapshot without copying.
#[derive(Debug, Clone)]
pub struct MemberEvent<I, A> {
  kind: MemberEventKind,
  members: Arc<Vec<Member<I, A>>>,
}

impl<I, A> MemberEvent<I, A> {
  /// Constructs a new `MemberEvent`.
  pub(crate) fn new(kind: MemberEventKind, members: Vec<Member<I, A>>) -> Self {
    Self {
      kind,
      members: Arc::new(members),
    }
  }

  /// The kind of membership change.
  pub const fn kind(&self) -> MemberEventKind {
    self.kind
  }

  /// The affected members.
  pub fn members(&self) -> &[Member<I, A>] {
    &self.members
  }
}

// ── QueryEvent ────────────────────────────────────────────────────────────────

/// Payload for [`Event::Query`]: an incoming query that the application may
/// respond to.
///
/// The response token (`id` + `from`) must be passed to
/// `Endpoint::respond_to_query` to send a reply before the `deadline`.
#[derive(Debug, Clone)]
pub struct QueryEvent<I, A> {
  /// Opaque query identifier used to route responses back to the originator.
  pub(crate) id: u32,
  /// Lamport clock time carried by the query message.
  pub(crate) ltime: LamportTime,
  /// The node that originated the query.
  pub(crate) from: Node<I, A>,
  /// The query name (application-defined).
  pub(crate) name: SmolStr,
  /// The query payload.
  pub(crate) payload: Bytes,
  /// Number of relay hops requested.
  pub(crate) relay_factor: u8,
  /// Deadline by which a response is useful.
  pub(crate) deadline: Instant,
}

impl<I, A> QueryEvent<I, A> {
  /// Opaque query id (the response token).
  pub const fn id(&self) -> u32 {
    self.id
  }

  /// Lamport time of the query.
  pub const fn ltime(&self) -> LamportTime {
    self.ltime
  }

  /// The originating node.
  pub const fn from(&self) -> &Node<I, A> {
    &self.from
  }

  /// The query name.
  pub fn name(&self) -> &str {
    &self.name
  }

  /// The query payload bytes.
  pub const fn payload(&self) -> &Bytes {
    &self.payload
  }

  /// The relay factor requested.
  pub const fn relay_factor(&self) -> u8 {
    self.relay_factor
  }

  /// The deadline by which a response should be sent.
  pub const fn deadline(&self) -> Instant {
    self.deadline
  }
}

// ── QueryResponse ─────────────────────────────────────────────────────────────

/// Payload for [`Event::QueryResponse`]: a response to a query this node
/// originated, folded from an inbound `QueryResponseMessage`.
#[derive(Debug, Clone)]
pub struct QueryResponse<I, A> {
  /// The query this response is for.
  pub(crate) id: u32,
  /// The node that sent this response.
  pub(crate) from: Node<I, A>,
  /// The response payload.
  pub(crate) payload: Bytes,
}

impl<I, A> QueryResponse<I, A> {
  /// The query id this response belongs to.
  pub const fn id(&self) -> u32 {
    self.id
  }

  /// The node that sent this response.
  pub const fn from(&self) -> &Node<I, A> {
    &self.from
  }

  /// The response payload.
  pub const fn payload(&self) -> &Bytes {
    &self.payload
  }
}

// ── QueryAck ──────────────────────────────────────────────────────────────────

/// Payload for [`Event::QueryAck`]: a delivery acknowledgement for a query this
/// node originated, from a peer that received the query and passed its filters.
///
/// Emitted only for queries issued with `request_ack`.  An ack carries no
/// payload — it confirms receipt, distinct from a [`QueryResponse`] which
/// carries the responder's reply.  A single peer may emit both: an ack on
/// receipt and, later, a response if the application calls `respond`.  Mirrors
/// the per-query `ack_ch` channel in serf-core `query.rs`.
#[derive(Debug, Clone)]
pub struct QueryAck<I, A> {
  /// The query this ack is for.
  pub(crate) id: u32,
  /// The node that acknowledged the query.
  pub(crate) from: Node<I, A>,
}

impl<I, A> QueryAck<I, A> {
  /// The query id this ack belongs to.
  pub const fn id(&self) -> u32 {
    self.id
  }

  /// The node that acknowledged the query.
  pub const fn from(&self) -> &Node<I, A> {
    &self.from
  }
}

// ── DialPassthrough ───────────────────────────────────────────────────────────

/// Payload for [`Event::DialRequested`]: the inner memberlist Endpoint asks the
/// driver to open a TCP/QUIC stream to `peer`.
///
/// The driver calls `inner.dial_succeeded(id, now)` on success or
/// `inner.dial_failed(id, err, now)` on failure.  The `StreamId` is opaque to
/// serf and is forwarded verbatim from the inner [`memberlist_proto::DialRequested`].
///
/// serf emits this via `start_push_pull(addr, PushPullKind::Join, now)` in the
/// reconnector; the inner machine queues the `Event::DialRequested` and the
/// sieve passes it through as this type.  H3: serf does NO dial itself.
#[derive(Debug, Clone)]
pub struct DialPassthrough<A> {
  /// Opaque stream id the driver reports back to the inner endpoint.
  id: StreamId,
  /// The peer address to dial.
  peer: A,
}

impl<A> DialPassthrough<A> {
  /// Construct a new passthrough payload.
  pub(crate) fn new(id: StreamId, peer: A) -> Self {
    Self { id, peer }
  }

  /// The stream id (report back via `inner.dial_succeeded` / `dial_failed`).
  pub const fn id(&self) -> StreamId {
    self.id
  }

  /// The peer address to dial.
  pub const fn peer(&self) -> &A {
    &self.peer
  }
}

// ── RelayDropped ──────────────────────────────────────────────────────────────

/// Payload for [`Event::RelayDropped`]: a relay forward could not be delivered.
///
/// A relay is always directed (`send_user_packet`), never re-broadcast.  If the
/// directed send fails, the machine emits this event instead of silently
/// discarding the failure.
#[derive(Debug, Clone)]
pub struct RelayDropped<A> {
  /// The destination address the relay was directed to.
  pub(crate) destination: A,
}

impl<A> RelayDropped<A> {
  /// The destination that could not be reached.
  pub const fn destination(&self) -> &A {
    &self.destination
  }
}

// ── KeyResponse ───────────────────────────────────────────────────────────────

/// Aggregated result of a cluster-wide key-management query.
///
/// Produced after a `list_keys`, `install_key`, `use_key`, or `remove_key`
/// query's deadline fires.  All per-node `KeyResponseMessage`s received before
/// the deadline are folded into this summary.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
#[derive(Debug, Clone)]
pub struct KeyResponse<I> {
  /// Total number of nodes queried.
  pub num_nodes: usize,
  /// Number that responded before the deadline.
  pub num_resp: usize,
  /// Number with `result = false` (error) in their response.
  pub num_err: usize,
  /// Key → count of nodes that have that key installed.
  pub keys: crate::FxHashMap<SecretKey, usize>,
  /// Primary key → count of nodes using it as primary.
  pub primary_keys: crate::FxHashMap<SecretKey, usize>,
  /// Per-node error messages (only for nodes with `result = false`).
  pub messages: crate::FxHashMap<I, smol_str::SmolStr>,
}

// ── KeyRequestOperation ───────────────────────────────────────────────────────

/// The key-management operation requested by a [`KeyRequest`].
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRequestOperation {
  /// Install a new key into the keyring.
  Install,
  /// Promote a key to primary.
  Use,
  /// Remove a key from the keyring.
  Remove,
  /// List all installed keys.
  List,
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
impl KeyRequestOperation {
  /// Returns a `'static` string representation of the operation.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Install => "install",
      Self::Use => "use",
      Self::Remove => "remove",
      Self::List => "list",
    }
  }

  /// Returns `true` if the operation carries a key (all except [`List`](Self::List)).
  pub const fn has_key(self) -> bool {
    !matches!(self, Self::List)
  }
}

// ── KeyRequest ────────────────────────────────────────────────────────────────

/// An inbound key-management request that the driver must apply to its keyring
/// and then answer via [`crate::endpoint::Endpoint::respond_key`].
///
/// The machine emits this event instead of `Event::Query` for the four internal
/// key query names (`_serf_install_key`, `_serf_use_key`, `_serf_remove_key`,
/// `_serf_list_keys`).  The machine never holds or inspects live key material
/// beyond routing the already-typed [`memberlist_proto::SecretKey`] to the
/// driver.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
#[derive(Debug, Clone)]
pub struct KeyRequest<I, A> {
  /// The key-management operation.
  pub(crate) op: KeyRequestOperation,
  /// The key, if the operation requires one (`None` for [`KeyRequestOperation::List`]).
  ///
  /// `SecretKey`'s `Debug` implementation redacts key bytes; the derived `Debug`
  /// on `KeyRequest` therefore does NOT leak raw key material.
  pub(crate) key: Option<SecretKey>,
  /// Opaque query identifier (used by `respond_key` to route the response).
  pub(crate) id: u32,
  /// Lamport clock time of the originating query.
  pub(crate) ltime: LamportTime,
  /// The node that originated the query.
  pub(crate) from: Node<I, A>,
  /// Number of relay hops requested by the originator.
  pub(crate) relay_factor: u8,
  /// Deadline by which a response is useful.
  pub(crate) deadline: Instant,
}

#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
impl<I, A> KeyRequest<I, A> {
  /// The key-management operation.
  pub const fn op(&self) -> KeyRequestOperation {
    self.op
  }

  /// The key, if the operation carries one.
  pub fn key(&self) -> Option<&SecretKey> {
    self.key.as_ref()
  }

  /// The node that originated the query.
  pub const fn from(&self) -> &Node<I, A> {
    &self.from
  }

  /// Opaque query id (forwarded to `Endpoint::respond_key`).
  pub const fn id(&self) -> u32 {
    self.id
  }

  /// Deadline by which a response should be sent.
  pub const fn deadline(&self) -> Instant {
    self.deadline
  }

  /// Construct a `KeyRequest` with an explicit `op` and response `deadline`, for
  /// the downstream driver tests that exercise key-op application and
  /// deadline-based control-queue bounding (a driver holding these on a non-lossy
  /// queue must prune the past-deadline, unanswerable ones). Gated behind the
  /// non-default `test-support` feature; NOT a production build path and NOT part
  /// of the wire contract. Only `op`, `id`, `from`, `key`, and `deadline` are
  /// caller-chosen; the remaining wire fields are inert placeholders.
  #[cfg(any(test, feature = "test-support"))]
  #[cfg_attr(docsrs, doc(cfg(feature = "test-support")))]
  pub fn test_with_deadline(
    op: KeyRequestOperation,
    id: u32,
    from: Node<I, A>,
    key: Option<SecretKey>,
    deadline: Instant,
  ) -> Self {
    Self {
      op,
      key,
      id,
      ltime: LamportTime::ZERO,
      from,
      relay_factor: 0,
      deadline,
    }
  }
}

// ── KeyResponseArgs ───────────────────────────────────────────────────────────

/// The driver's per-node answer to a [`KeyRequest`].
///
/// This is a projection type for the driver to fill in; it is NOT the wire
/// `KeyResponseMessage` (which stays `pub(crate)`).  The driver applies the
/// requested operation to its keyring, then calls
/// [`crate::endpoint::Endpoint::respond_key`] with the result.
///
/// For `install`, `use`, and `remove` operations, `keys` and `primary_key`
/// are typically left empty; for `list`, the driver fills `keys` with all
/// installed keys and `primary_key` with the current primary.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
#[cfg_attr(
  docsrs,
  doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
)]
#[derive(Debug, Default, Clone)]
pub struct KeyResponseArgs {
  /// `true` if the operation succeeded on this node.
  pub result: bool,
  /// Human-readable result or error description.
  pub message: smol_str::SmolStr,
  /// Installed keys (used for `list` responses).
  pub keys: Vec<SecretKey>,
  /// The current primary key, if reporting it.
  pub primary_key: Option<SecretKey>,
}

// ── Event ─────────────────────────────────────────────────────────────────────

/// Machine-output events produced by the serf [`crate::endpoint::Endpoint`].
///
/// The driver calls `poll_event()` in a loop and dispatches on these variants.
/// The enum is `#[non_exhaustive]` so that adding new variants is a
/// backwards-compatible change (drivers must include a `_ => {}` arm).
#[derive(Debug, Clone, derive_more::IsVariant)]
#[non_exhaustive]
pub enum Event<I, A> {
  /// A batch of membership changes (join, leave, failed, update, or reap).
  Member(MemberEvent<I, A>),
  /// An application-level user event was received from the cluster.
  User(UserEventMessage),
  /// An incoming query that this node should handle (and optionally respond to).
  Query(QueryEvent<I, A>),
  /// A response to a query this node originated arrived from a peer.
  QueryResponse(QueryResponse<I, A>),
  /// A delivery acknowledgement for a query this node originated (only emitted
  /// for queries issued with `request_ack`).
  QueryAck(QueryAck<I, A>),
  /// The local node lost an id-conflict vote and must shut down.
  ///
  /// The machine emits this signal; the driver is responsible for actually
  /// stopping the process or reinitialising — the pure machine never exits.
  /// Corresponds to Go serf `serf.go` conflict-resolution shutdown path.
  Shutdown,
  /// A responder-side relay forward could not be delivered to its destination.
  RelayDropped(RelayDropped<A>),
  /// The graceful leave chain completed; the node has fully left the cluster.
  ///
  /// Emitted after the inner memberlist `LeftCluster` event is received and
  /// the `leave_propagate_delay` has elapsed.
  LeftCluster,
  /// The inner memberlist Endpoint requests that the driver open a stream to
  /// the given peer.
  ///
  /// Produced when the reconnector calls `inner.start_push_pull()` and the
  /// inner queues its own `Event::DialRequested`.  The driver must dial `peer`
  /// and report back via `inner.dial_succeeded` / `inner.dial_failed`.
  /// H3: serf does NO I/O itself — this is machine output, not machine I/O.
  DialRequested(DialPassthrough<A>),
  /// Aggregated key-management query results.
  ///
  /// Emitted after an `install_key`, `use_key`, `remove_key`, or `list_keys`
  /// query closes (deadline fires).  The driver uses this to surface results
  /// to the operator.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyResponse(KeyResponse<I>),
  /// An inbound key-management request that this node must handle.
  ///
  /// The driver applies the requested operation to its keyring and then calls
  /// `Endpoint::respond_key` with the result.  The machine never holds or
  /// inspects live key material — it only routes the typed fields.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305"))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  KeyRequest(KeyRequest<I, A>),
  /// The terminal outcome of a reliable exchange initiated by this node.
  ///
  /// Emitted when the coordinator's bridge-reap path fires for an outbound
  /// exchange.  The payload carries the opaque `eid` (correlates with the
  /// `ExchangeId` returned by the coordinator's `start_push_pull` /
  /// `accept_connection`), the `peer` address, the `outcome`
  /// ([`memberlist_proto::event::ExchangeStatus`]), and the `kind`
  /// ([`memberlist_proto::event::ExchangeKind`]) that identifies the
  /// initiator.  A driver awaiting a Join push/pull resolves when
  /// `kind() == ExchangeKind::PushPull`.  Inbound (peer-initiated)
  /// exchanges do NOT emit this event — only outbound ones do.
  ExchangeCompleted(ExchangeCompleted<A>),
}

#[cfg(test)]
mod tests;
