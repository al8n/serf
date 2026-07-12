//! [`SerfSnapshot`] — the observable membership view both serf driver crates publish.

use std::sync::Arc;

use serf_proto::{
  LamportTime,
  members::{Member, MemberStatus, SerfState},
};

/// An immutable snapshot of the serf cluster's observable membership at one instant.
///
/// A driver republishes this after every membership change; a `Serf` handle reads the
/// latest one with no coordination. It carries member identity, tags, status, the local
/// endpoint's lifecycle state, and the three Lamport clocks the serf protocol uses to
/// order events across the cluster. Application payloads (`User` events, `Query` /
/// `QueryResponse`) are delivered through the observation channel, not retained here.
///
/// Generic over the wire id / address types `<I, A>`, mirroring the underlying
/// [`Member<I, A>`]; a handle instantiates it as `SerfSnapshot<I, SocketAddr>`.
#[derive(Debug, Clone)]
pub struct SerfSnapshot<I, A> {
  members: Vec<Arc<Member<I, A>>>,
  /// Index into `members` of the local node's entry.
  ///
  /// Storing an index rather than a separate `Arc` guarantees that `local()` /
  /// `local_ref()` always return the exact same `Arc` that lives in `members`,
  /// so the local handle can never contradict the member view.
  local_index: usize,
  state: SerfState,
  member_clock: LamportTime,
  event_clock: LamportTime,
  query_clock: LamportTime,
  alive_count: usize,
  member_count: usize,
  /// The local node's Vivaldi coordinate at the instant of the snapshot, set by
  /// the publishing driver via [`with_coordinate`](Self::with_coordinate);
  /// `None` when coordinates are disabled or no coordinate exists yet.
  #[cfg(feature = "coordinates")]
  coordinate: Option<serf_proto::typed::Coordinate>,
  /// The node-awareness health score at the instant of the snapshot (`0` =
  /// healthy; higher stretches the failure-detection timeouts). Set by the
  /// publishing driver via [`with_ops_stats`](Self::with_ops_stats).
  health_score: usize,
  /// Depth of the gossip broadcast queue (the total across the intent, event,
  /// and query tiers) at the instant of the snapshot. Set by the publishing
  /// driver via [`with_ops_stats`](Self::with_ops_stats).
  broadcast_queue_depth: usize,
  /// Whether a gossip/reliable encryption keyring is configured on the node.
  /// Set by the publishing driver via [`with_ops_stats`](Self::with_ops_stats).
  encrypted: bool,
  /// The number of times the local Vivaldi coordinate was reset after
  /// degenerating; `None` when coordinates are disabled. Set by the publishing
  /// driver via [`with_coordinate_resets`](Self::with_coordinate_resets).
  #[cfg(feature = "coordinates")]
  coordinate_resets: Option<usize>,
}

impl<I, A> SerfSnapshot<I, A> {
  /// Builds a snapshot from the membership view. Called by a driver each time it republishes.
  ///
  /// `local_id` identifies the local node; the index into `members` is derived by
  /// finding the first member whose id equals `local_id`.  `local()` / `local_ref()`
  /// always return the element already in `members` and can never contradict the view.
  ///
  /// # Panics
  ///
  /// Panics if no member in `members` has an id equal to `local_id`.  The local node
  /// is always a member of its own cluster view; a driver that omits it has a logic
  /// error, and failing at construction makes the invariant violation visible immediately
  /// rather than allowing a silent mismatch to propagate to callers of `local()`.
  ///
  /// `member_count` and `alive_count` are derived from `members` and can never
  /// contradict the view.
  #[must_use]
  pub fn new(
    members: Vec<Arc<Member<I, A>>>,
    local_id: &I,
    state: SerfState,
    member_clock: LamportTime,
    event_clock: LamportTime,
    query_clock: LamportTime,
  ) -> Self
  where
    I: PartialEq,
  {
    let local_index = members
      .iter()
      .position(|m| m.node().id_ref() == local_id)
      .expect("local node must be present in members");
    let member_count = members.len();
    let alive_count = members
      .iter()
      .filter(|m| m.status() == MemberStatus::Alive)
      .count();
    Self {
      members,
      local_index,
      state,
      member_clock,
      event_clock,
      query_clock,
      alive_count,
      member_count,
      #[cfg(feature = "coordinates")]
      coordinate: None,
      health_score: 0,
      broadcast_queue_depth: 0,
      encrypted: false,
      #[cfg(feature = "coordinates")]
      coordinate_resets: None,
    }
  }

  /// Attach the operator statistics a driver reads live from its endpoint
  /// (builder form, called by the publishing driver after [`new`](Self::new)).
  #[must_use]
  pub fn with_ops_stats(
    mut self,
    health_score: usize,
    broadcast_queue_depth: usize,
    encrypted: bool,
  ) -> Self {
    self.health_score = health_score;
    self.broadcast_queue_depth = broadcast_queue_depth;
    self.encrypted = encrypted;
    self
  }

  /// Attach the coordinate-reset counter (builder form).
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub fn with_coordinate_resets(mut self, resets: Option<usize>) -> Self {
    self.coordinate_resets = resets;
    self
  }

  /// The node-awareness health score at the instant of the snapshot (`0` =
  /// healthy; higher stretches the failure-detection timeouts).
  #[must_use]
  pub const fn health_score(&self) -> usize {
    self.health_score
  }

  /// Depth of the gossip broadcast queue (the total across the intent, event,
  /// and query tiers) at the instant of the snapshot.
  #[must_use]
  pub const fn broadcast_queue_depth(&self) -> usize {
    self.broadcast_queue_depth
  }

  /// Whether a gossip/reliable encryption keyring is configured on the node.
  #[must_use]
  pub const fn encrypted(&self) -> bool {
    self.encrypted
  }

  /// The number of times the local Vivaldi coordinate was reset after
  /// degenerating; `None` when coordinates are disabled.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub const fn coordinate_resets(&self) -> Option<usize> {
    self.coordinate_resets
  }

  /// Assemble the aggregate operator statistics from this snapshot.
  #[must_use]
  pub fn stats(&self) -> SerfStats {
    let failed = self
      .members
      .iter()
      .filter(|m| m.status() == MemberStatus::Failed)
      .count();
    let left = self
      .members
      .iter()
      .filter(|m| m.status() == MemberStatus::Left)
      .count();
    SerfStats {
      members: self.member_count,
      failed,
      left,
      health_score: self.health_score,
      member_clock: self.member_clock,
      event_clock: self.event_clock,
      query_clock: self.query_clock,
      broadcast_queue_depth: self.broadcast_queue_depth,
      encrypted: self.encrypted,
      #[cfg(feature = "coordinates")]
      coordinate_resets: self.coordinate_resets,
    }
  }

  /// Attach the local node's Vivaldi coordinate to the snapshot (builder form,
  /// called by the publishing driver after [`new`](Self::new)).
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub fn with_coordinate(mut self, coordinate: Option<serf_proto::typed::Coordinate>) -> Self {
    self.coordinate = coordinate;
    self
  }

  /// The local node's Vivaldi coordinate at the instant of the snapshot.
  ///
  /// `None` when coordinates are disabled
  /// (`Options::with_disable_coordinates(true)`), when the publishing driver
  /// does not forward them, or when no coordinate exists yet.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub const fn coordinate(&self) -> Option<&serf_proto::typed::Coordinate> {
    self.coordinate.as_ref()
  }

  /// All known members (full [`Member`], carrying tags and status) — alive, leaving, left,
  /// and failed within the reap window.
  #[must_use]
  pub fn members(&self) -> &[Arc<Member<I, A>>] {
    self.members.as_slice()
  }

  /// All known members. Alias for [`Self::members`].
  #[must_use]
  pub fn members_slice(&self) -> &[Arc<Member<I, A>>] {
    self.members.as_slice()
  }

  /// The local node's full [`Member`] view (cheap Arc clone).
  ///
  /// Returns a clone of the same `Arc` that lives at `local_index` in `members()`,
  /// so this is always consistent with the member view.
  /// For a borrowed view use [`Self::local_ref`].
  #[must_use]
  pub fn local(&self) -> Arc<Member<I, A>> {
    Arc::clone(&self.members[self.local_index])
  }

  /// Borrow the local node's full [`Member`] view.
  ///
  /// Returns a reference to the `Arc` at `local_index` in `members()`.
  #[must_use]
  pub fn local_ref(&self) -> &Arc<Member<I, A>> {
    &self.members[self.local_index]
  }

  /// The lifecycle state of the local serf endpoint at the instant the snapshot was taken.
  #[must_use]
  pub const fn state(&self) -> SerfState {
    self.state
  }

  /// The member Lamport clock at the time of the snapshot.
  #[must_use]
  pub const fn member_clock(&self) -> LamportTime {
    self.member_clock
  }

  /// The event Lamport clock at the time of the snapshot.
  #[must_use]
  pub const fn event_clock(&self) -> LamportTime {
    self.event_clock
  }

  /// The query Lamport clock at the time of the snapshot.
  #[must_use]
  pub const fn query_clock(&self) -> LamportTime {
    self.query_clock
  }

  /// The count of members currently in the alive state.
  #[must_use]
  pub const fn alive_count(&self) -> usize {
    self.alive_count
  }

  /// The count of all known members (alive + leaving + left + failed).
  #[must_use]
  pub const fn member_count(&self) -> usize {
    self.member_count
  }

  /// The count of all known members. Alias for [`Self::member_count`].
  #[must_use]
  pub const fn num_members(&self) -> usize {
    self.member_count
  }

  /// Look up a member by id. Returns `None` if the id is not in the snapshot.
  #[must_use]
  #[inline]
  pub fn by_id(&self, id: &I) -> Option<&Arc<Member<I, A>>>
  where
    I: PartialEq,
  {
    self.members.iter().find(|m| m.node().id_ref() == id)
  }

  /// Iterate members currently in the alive state.
  #[inline]
  pub fn online_members(&self) -> impl Iterator<Item = &Arc<Member<I, A>>> {
    self
      .members
      .iter()
      .filter(|m| m.status() == MemberStatus::Alive)
  }

  /// Iterate members matching `pred`.
  #[inline]
  pub fn members_by<'a>(
    &'a self,
    mut pred: impl FnMut(&Member<I, A>) -> bool + 'a,
  ) -> impl Iterator<Item = &'a Arc<Member<I, A>>> {
    self.members.iter().filter(move |m| pred(m))
  }

  /// Count members matching `pred`.
  #[inline]
  pub fn num_members_by(&self, mut pred: impl FnMut(&Member<I, A>) -> bool) -> usize {
    self.members.iter().filter(|m| pred(m)).count()
  }

  /// Map-filter members, collecting all `Some` results into a `Vec`.
  #[inline]
  pub fn members_map_by<O>(&self, mut f: impl FnMut(&Member<I, A>) -> Option<O>) -> Vec<O> {
    self.members.iter().filter_map(|m| f(m)).collect()
  }
}

#[cfg(test)]
mod tests;

/// Aggregate operator statistics assembled from one [`SerfSnapshot`].
///
/// The counts and clocks come from the snapshot's member view; the health
/// score, broadcast queue depth, and encryption flag are the live endpoint
/// readings the publishing driver attached at the same instant.
#[derive(Debug, Clone)]
pub struct SerfStats {
  members: usize,
  failed: usize,
  left: usize,
  health_score: usize,
  member_clock: LamportTime,
  event_clock: LamportTime,
  query_clock: LamportTime,
  broadcast_queue_depth: usize,
  encrypted: bool,
  #[cfg(feature = "coordinates")]
  coordinate_resets: Option<usize>,
}

impl SerfStats {
  /// The count of all known members (alive + leaving + left + failed).
  #[must_use]
  pub const fn members(&self) -> usize {
    self.members
  }

  /// The count of members currently in the failed state.
  #[must_use]
  pub const fn failed(&self) -> usize {
    self.failed
  }

  /// The count of members currently in the gracefully-left state.
  #[must_use]
  pub const fn left(&self) -> usize {
    self.left
  }

  /// The node-awareness health score (`0` = healthy; higher stretches the
  /// failure-detection timeouts).
  #[must_use]
  pub const fn health_score(&self) -> usize {
    self.health_score
  }

  /// The member Lamport clock.
  #[must_use]
  pub const fn member_clock(&self) -> LamportTime {
    self.member_clock
  }

  /// The event Lamport clock.
  #[must_use]
  pub const fn event_clock(&self) -> LamportTime {
    self.event_clock
  }

  /// The query Lamport clock.
  #[must_use]
  pub const fn query_clock(&self) -> LamportTime {
    self.query_clock
  }

  /// Depth of the gossip broadcast queue (the total across the intent, event,
  /// and query tiers).
  #[must_use]
  pub const fn broadcast_queue_depth(&self) -> usize {
    self.broadcast_queue_depth
  }

  /// Whether a gossip/reliable encryption keyring is configured on the node.
  #[must_use]
  pub const fn encrypted(&self) -> bool {
    self.encrypted
  }

  /// The number of times the local Vivaldi coordinate was reset after
  /// degenerating; `None` when coordinates are disabled.
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  #[must_use]
  pub const fn coordinate_resets(&self) -> Option<usize> {
    self.coordinate_resets
  }
}
