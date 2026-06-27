//! Internal command channel — user-facing Serf handle sends;
//! driver task receives and dispatches.

use crate::error::Result;
use bytes::Bytes;
use futures_channel::oneshot::Sender;
use smol_str::SmolStr;

#[cfg(any(feature = "tcp", feature = "quic"))]
use memberlist_proto::Instant;
#[cfg(encryption)]
use memberlist_proto::SecretKey;
#[cfg(any(feature = "tcp", feature = "quic"))]
use serf_proto::{
  endpoint::{QueryId, QueryParams},
  event::QueryEvent,
  typed::Tags,
};

/// Payload for [`Command::Join`].
pub(crate) struct JoinCmd {
  /// Pre-resolved socket addresses of the seed peers to contact.
  pub(crate) seeds: Vec<std::net::SocketAddr>,
  /// One-shot reply channel delivering the count of seeds the driver dispatched
  /// a push-pull to. Join is dispatch-only: the reply reports how many exchanges
  /// were initiated, not how many seeds were reached — actual joins surface as
  /// membership [`Event`](serf_proto::event::Event)s.
  pub(crate) reply: Sender<Result<usize>>,
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
/// and its reply bytes, then sends it through the command channel.
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

/// Payload for [`Command::SetEventJoinIgnore`].
pub(crate) struct SetEventJoinIgnoreCmd {
  /// When `true`, the machine suppresses `Event::Member(Join)` events.
  pub(crate) ignore: bool,
  /// One-shot reply channel (always `Ok(())`; the setter never fails).
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
  /// Initiate joins to the given seed peers (addresses already resolved). The
  /// reply carries the count of seeds the driver dispatched a push-pull to, not
  /// the count reached; actual joins surface as membership events.
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

  /// Enable or disable suppression of member-join events in the driver's
  /// observation stream.
  SetEventJoinIgnore(SetEventJoinIgnoreCmd),

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

  /// Signal the driver task to shut down gracefully.
  Shutdown(ShutdownCmd),
}

#[cfg(test)]
mod tests;
