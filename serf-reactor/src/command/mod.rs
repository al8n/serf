//! Internal command queue — the user-facing Serf handle pushes commands; the
//! driver task drains and dispatches them.

use crate::error::Result;
use bytes::Bytes;
use futures_channel::oneshot::Sender;
use smol_str::SmolStr;

#[cfg(any(feature = "tcp", feature = "quic"))]
use memberlist_proto::Instant;
#[cfg(encryption)]
use memberlist_proto::SecretKey;
#[cfg(any(feature = "tcp", feature = "quic"))]
use smallvec::SmallVec;
#[cfg(any(feature = "tcp", feature = "quic"))]
use std::net::SocketAddr;

#[cfg(any(feature = "tcp", feature = "quic"))]
use crate::error::SerfError;
#[cfg(any(feature = "tcp", feature = "quic"))]
use serf_proto::{
  endpoint::{QueryId, QueryParams},
  event::QueryEvent,
  typed::Tags,
};

/// Address-set reply for [`Command::Join`].
///
/// Both join kinds reply through this single type (mirroring how the memberlist
/// driver unifies its join reply into one channel type): `Ok(set)` carries the
/// dispatched set ([`JoinKind::Dispatch`]) or the contacted set
/// ([`JoinKind::WaitForCompletion`] success); `Err((set, err))` is the legacy
/// partial-success tuple, surfacing the reached-so-far set alongside the error.
/// Every error this driver produces — `NotRunning`, `Shutdown`, and
/// `JoinAllFailed` — resolves before any contact is accumulated, so the tuple's
/// set is empty in practice; it is carried for the legacy `join_many` shape.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) type JoinReply =
  core::result::Result<SmallVec<[SocketAddr; 1]>, (SmallVec<[SocketAddr; 1]>, SerfError)>;

/// Payload for [`JoinKind::WaitForCompletion`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct WaitForCompletionArgs {
  /// Wall-clock instant past which the driver replies with whatever contacted
  /// set it has accumulated (an empty set surfaces as `JoinAllFailed`).
  pub(crate) deadline: Instant,
}

/// Semantic of a [`Command::Join`] dispatch.
///
/// Both kinds share the same `start_push_pull` fan-out (one outbound exchange
/// per resolved seed); the kind only affects WHEN the reply fires and WHAT it
/// carries:
/// - `Dispatch`: reply immediately with the dispatched seed set
///   (fire-and-forget; the caller does not wait for any exchange to terminate).
/// - `WaitForCompletion`: reply once every dispatched exchange has terminated
///   (an [`Event::ExchangeCompleted`](serf_proto::event::Event) with
///   `kind == ExchangeKind::PushPull` for each) OR the deadline elapses,
///   whichever comes first; the reply carries the contacted set (an empty set
///   surfaces as `JoinAllFailed`).
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) enum JoinKind {
  /// Reply immediately with the dispatched seed set.
  Dispatch,
  /// Reply once every dispatched exchange has terminated OR the deadline expires.
  WaitForCompletion(WaitForCompletionArgs),
}

/// Payload for [`Command::Join`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct JoinCmd {
  /// Pre-resolved socket addresses of the seed peers to contact. The handle
  /// resolves every `MaybeResolved` seed through the caller's resolver before
  /// sending the command, so the driver only ever sees concrete addresses.
  pub(crate) seeds: Vec<SocketAddr>,
  /// Dispatch semantic — see [`JoinKind`].
  pub(crate) kind: JoinKind,
  /// When `true`, each seed's join push/pull is started as an `ignore_old` join
  /// so the machine records that exchange's `StreamId` and suppresses replay of
  /// the seed's pre-join user events. Keyed per-EXCHANGE and one-shot — see
  /// `serf_proto::StreamEndpoint::start_join_push_pull`.
  pub(crate) ignore_old: bool,
  /// One-shot reply channel delivering the address-set result. See [`JoinReply`].
  pub(crate) reply: Sender<JoinReply>,
}

/// Payload for [`Command::Leave`].
pub(crate) struct LeaveCmd {
  /// One-shot reply channel delivering `()` once the graceful leave completes.
  pub(crate) reply: Sender<Result<()>>,
}

/// Payload for [`Command::ForceLeave`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct ForceLeaveCmd<I> {
  /// The node id to force-remove.
  pub(crate) id: I,
  /// When `true`, the node is pruned immediately rather than after the
  /// tombstone timeout — mirrors `Endpoint::force_leave(prune = true)`.
  pub(crate) prune: bool,
  /// Wall-clock instant passed to the machine's `force_leave` call.
  pub(crate) now: Instant,
  /// One-shot reply channel for the operation result.
  pub(crate) reply: Sender<Result<()>>,
}

/// Payload for [`Command::UserEvent`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct UserEventCmd {
  /// Event name — must not exceed the configured `max_user_event_size`.
  name: SmolStr,
  /// Arbitrary application payload bytes.
  payload: Bytes,
  /// When `true`, the machine deduplicates events with the same name (coalesce mode).
  pub(crate) coalesce: bool,
  /// One-shot reply channel for the enqueue result.
  pub(crate) reply: Sender<Result<()>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl UserEventCmd {
  /// Construct from event fields and a reply channel.
  pub(crate) fn new(
    name: SmolStr,
    payload: Bytes,
    coalesce: bool,
    reply: Sender<Result<()>>,
  ) -> Self {
    Self {
      name,
      payload,
      coalesce,
      reply,
    }
  }

  /// The event name.
  pub(crate) fn name(&self) -> &SmolStr {
    &self.name
  }

  /// The event payload bytes.
  pub(crate) const fn payload(&self) -> &Bytes {
    &self.payload
  }
}

/// Payload for [`Command::Query`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct QueryCmd<I> {
  /// Query name forwarded to responders.
  name: SmolStr,
  /// Query payload forwarded to responders.
  payload: Bytes,
  /// Routing and timing parameters (filters, relay factor, ack, timeout).
  pub(crate) params: QueryParams<I>,
  /// Wall-clock instant passed to the machine's `query` call.
  pub(crate) now: Instant,
  /// One-shot reply channel delivering the [`QueryId`] identifying this query.
  pub(crate) reply: Sender<Result<QueryId>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I> QueryCmd<I> {
  /// Construct from query fields and a reply channel.
  pub(crate) fn new(
    name: SmolStr,
    payload: Bytes,
    params: QueryParams<I>,
    now: Instant,
    reply: Sender<Result<QueryId>>,
  ) -> Self {
    Self {
      name,
      payload,
      params,
      now,
      reply,
    }
  }

  /// The query name.
  pub(crate) fn name(&self) -> &SmolStr {
    &self.name
  }

  /// The query payload bytes.
  pub(crate) const fn payload(&self) -> &Bytes {
    &self.payload
  }
}

/// Payload for [`Command::Respond`].
///
/// The driver receives a [`serf_proto::event::Event::Query`] carrying a
/// [`QueryEvent`]; the application constructs a `RespondCmd` from that token
/// and its reply bytes, then sends it through the command queue.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct RespondCmd<I, A> {
  /// The query token received in `Event::Query` — carries the routing fields
  /// (`ltime`, `id`, `from`, `relay_factor`, `deadline`) `Endpoint::respond`
  /// needs to dispatch the reply.
  pub(crate) token: QueryEvent<I, A>,
  /// The application's response payload bytes.
  payload: Bytes,
  /// Wall-clock instant at which the driver dispatches the respond call.
  pub(crate) now: Instant,
  /// One-shot reply channel delivering `Ok(())` on success.
  pub(crate) reply: Sender<Result<()>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> RespondCmd<I, A> {
  /// Construct from a query token, payload, now instant, and reply channel.
  pub(crate) fn new(
    token: QueryEvent<I, A>,
    payload: Bytes,
    now: Instant,
    reply: Sender<Result<()>>,
  ) -> Self {
    Self {
      token,
      payload,
      now,
      reply,
    }
  }

  /// The response payload bytes.
  pub(crate) const fn payload(&self) -> &Bytes {
    &self.payload
  }
}

/// Payload for [`Command::SetTags`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct SetTagsCmd {
  /// The new tag map to advertise on the local node.
  pub(crate) tags: Tags,
  /// One-shot reply channel for the update result.
  pub(crate) reply: Sender<Result<()>>,
}

/// Payload for [`Command::InstallKey`], [`Command::UseKey`], and
/// [`Command::RemoveKey`].
///
/// All three carry a single [`SecretKey`] and reply with the [`QueryId`] of the
/// issued cluster-wide key query.
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
pub(crate) struct KeyCmd {
  /// The secret key to install, promote, or remove.
  pub(crate) key: SecretKey,
  /// Wall-clock instant passed to the machine's key-op call.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) now: Instant,
  /// One-shot reply channel delivering the [`QueryId`] of the issued query.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) reply: Sender<Result<QueryId>>,
}

/// Payload for [`Command::ListKeys`].
///
/// Requires the `aes-gcm` or `chacha20-poly1305` feature.
#[cfg(encryption)]
pub(crate) struct ListKeysCmd {
  /// Wall-clock instant passed to `Endpoint::list_keys`.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) now: Instant,
  /// One-shot reply channel delivering the [`QueryId`] of the issued query.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) reply: Sender<Result<QueryId>>,
}

/// Payload for [`Command::CachedCoordinate`].
///
/// Requires the `coordinates` feature.
#[cfg(feature = "coordinates")]
pub(crate) struct CachedCoordinateCmd<I> {
  /// The peer whose most-recently-observed coordinate is requested.
  pub(crate) id: I,
  /// One-shot reply channel delivering the peer's cached coordinate, `None`
  /// when coordinates are disabled or no RTT sample has arrived from the peer.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  pub(crate) reply: Sender<Result<Option<serf_proto::typed::Coordinate>>>,
}

/// Payload for [`Command::Shutdown`].
pub(crate) struct ShutdownCmd {
  /// One-shot reply channel for the shutdown acknowledgement.
  pub(crate) reply: Sender<Result<()>>,
}

/// Commands sent from the public `Serf` handle to the driver task.
///
/// `I` is the node-id type; `A` is the resolved peer-address type (typically
/// `std::net::SocketAddr`). All variants are newtype-over-payload-struct.
pub(crate) enum Command<I, A> {
  /// Initiate joins to the given resolved seeds. The reply carries the
  /// address-set result: a dispatched or contacted set on success, or the
  /// legacy partial-success tuple on failure (see [`JoinReply`]).
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  Join(JoinCmd),

  /// Begin a graceful leave from the cluster.
  Leave(LeaveCmd),

  /// Force-remove a node from the cluster membership.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  ForceLeave(ForceLeaveCmd<I>),

  /// Broadcast a user-defined event cluster-wide via gossip.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  UserEvent(UserEventCmd),

  /// Issue a cluster-wide query and collect responses; reply carries the
  /// [`QueryId`] identifying the in-flight query.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  Query(QueryCmd<I>),

  /// Send a response to an inbound query received via `Event::Query`.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  Respond(RespondCmd<I, A>),

  /// Update the local node's advertised tags.
  #[cfg(any(feature = "tcp", feature = "quic"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
  SetTags(SetTagsCmd),

  /// Issue a cluster-wide install-key query.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  InstallKey(KeyCmd),

  /// Issue a cluster-wide use-key query to promote a key to primary.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  UseKey(KeyCmd),

  /// Issue a cluster-wide remove-key query.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  RemoveKey(KeyCmd),

  /// Issue a cluster-wide list-keys query to enumerate installed keys.
  ///
  /// Requires the `aes-gcm` or `chacha20-poly1305` feature.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  ListKeys(ListKeysCmd),

  /// Read a peer's most-recently-observed Vivaldi coordinate.
  ///
  /// Requires the `coordinates` feature.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  CachedCoordinate(CachedCoordinateCmd<I>),

  /// Signal the driver task to shut down gracefully.
  Shutdown(ShutdownCmd),
}

#[cfg(test)]
mod tests;
