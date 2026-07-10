//! Pure, timer-driven event coalescing for the serf [`crate::endpoint::Endpoint`].
//!
//! Go/legacy serf batches rapid membership and user events into time windows
//! before delivering them to the application, collapsing flapping into a single
//! grouped observation.  The reference implementation
//! (`serf-core/src/coalesce.rs` + `coalesce/{member,user}.rs`) runs the window as
//! an async task fed by a channel; this port re-expresses the exact same
//! algorithm as a synchronous, allocation-backed state machine owned by the
//! endpoint, so `now` is threaded in (no clock reads) and every driver inherits
//! coalescing without spawning a task.
//!
//! Two deadlines drive a flush (mirroring the reference `coalesceLoop`):
//! * the **coalesce** window — the maximum batch delay, armed once on the first
//!   buffered event of a batch and never pushed forward; and
//! * the **quiescent** window — the flush-after-quiet delay, re-armed on every
//!   buffered event.
//!
//! Whichever fires first flushes the coalesced batch and disarms the window.
//! The endpoint folds [`MemberEventCoalescer::flush_deadline`] /
//! [`UserEventCoalescer::flush_deadline`] into its `serf_poll_timeout` and calls
//! `flush` from `after_inner_timeout` when the deadline is due.

use core::{hash::Hash, time::Duration};

use std::{collections::VecDeque, vec::Vec};

use memberlist_proto::Instant;
use smol_str::SmolStr;

use crate::{
  FxHashMap, LamportTime, UserEventMessage,
  event::{Event, MemberEvent, MemberEventKind},
  members::Member,
};

// ── CoalesceWindow ────────────────────────────────────────────────────────────

/// The two-deadline flush timing shared by the member and user coalescers.
///
/// * `coalesce_deadline` is the maximum batch window: set once on the first
///   buffered event of a batch and never advanced, so a steady stream of events
///   still flushes at most `coalesce_period` after the batch opened.
/// * `quiescent_deadline` is the flush-after-quiet window: re-armed on every
///   buffered event, so a batch flushes `quiescent_period` after the last event
///   once the stream goes quiet.
///
/// The binding flush deadline is the minimum of the two; both are `None` while
/// the window holds no buffered events.
struct CoalesceWindow {
  coalesce_period: Duration,
  quiescent_period: Duration,
  coalesce_deadline: Option<Instant>,
  quiescent_deadline: Option<Instant>,
}

impl CoalesceWindow {
  const fn new(coalesce_period: Duration, quiescent_period: Duration) -> Self {
    Self {
      coalesce_period,
      quiescent_period,
      coalesce_deadline: None,
      quiescent_deadline: None,
    }
  }

  /// Arm the window for a newly buffered event: open the max-window on the first
  /// event of a batch, and (re)start the quiescent timer on every event.
  fn arm(&mut self, now: Instant) {
    if self.coalesce_deadline.is_none() {
      self.coalesce_deadline = Some(now + self.coalesce_period);
    }
    self.quiescent_deadline = Some(now + self.quiescent_period);
  }

  /// The binding flush deadline (`min(coalesce, quiescent)`), or `None` when the
  /// window is disarmed (no buffered events).
  fn deadline(&self) -> Option<Instant> {
    match (self.coalesce_deadline, self.quiescent_deadline) {
      (Some(c), Some(q)) => Some(c.min(q)),
      (c, None) => c,
      (None, q) => q,
    }
  }

  /// Whether the flush deadline has elapsed at `now`.
  fn due(&self, now: Instant) -> bool {
    self.deadline().is_some_and(|d| now >= d)
  }

  /// Disarm the window after a flush (or drop).
  fn reset(&mut self) {
    self.coalesce_deadline = None;
    self.quiescent_deadline = None;
  }
}

// ── MemberEventCoalescer ────────────────────────────────────────────────────────

/// The latest buffered observation of a node within an open coalesce window.
struct LatestMember<I, A> {
  kind: MemberEventKind,
  member: Member<I, A>,
}

/// Coalesces membership changes over a time window, collapsing rapid transitions
/// of a node to its latest observed status.
///
/// Ports serf-core `coalesce/member.rs` `MemberEventCoalescer`:
/// * `latest` holds the LATEST `(kind, member)` per node id within the open
///   window, so a rapid join → leave → join for one node collapses to its final
///   observation.
/// * `last` remembers the last EMITTED kind per node id ACROSS flushes, so a
///   repeated identical status is suppressed — except [`MemberEventKind::Update`],
///   which always re-emits because a node's tags may have changed.
///
/// A flush groups the surviving observations by kind and emits one
/// [`Event::Member`] batch per kind.
pub(crate) struct MemberEventCoalescer<I, A>
where
  I: Eq + Hash,
{
  window: CoalesceWindow,
  latest: FxHashMap<I, LatestMember<I, A>>,
  last: FxHashMap<I, MemberEventKind>,
}

impl<I, A> MemberEventCoalescer<I, A>
where
  I: Eq + Hash + Clone,
{
  /// Construct a member coalescer over the given windows.
  pub(crate) fn new(coalesce_period: Duration, quiescent_period: Duration) -> Self {
    Self {
      window: CoalesceWindow::new(coalesce_period, quiescent_period),
      latest: FxHashMap::default(),
      last: FxHashMap::default(),
    }
  }

  /// Buffer a batch of member changes of one `kind`, keyed by node id (latest
  /// wins), and arm the flush window.
  pub(crate) fn feed(&mut self, kind: MemberEventKind, members: Vec<Member<I, A>>, now: Instant) {
    for member in members {
      let id = member.node().id_ref().clone();
      self.latest.insert(id, LatestMember { kind, member });
    }
    self.window.arm(now);
  }

  /// The next flush deadline while the window is open, else `None`.
  pub(crate) fn flush_deadline(&self) -> Option<Instant> {
    self.window.deadline()
  }

  /// Whether the flush deadline has elapsed at `now`.
  pub(crate) fn due(&self, now: Instant) -> bool {
    self.window.due(now)
  }

  /// Emit the coalesced member events into `out` and disarm the window.
  ///
  /// Groups the surviving observations by kind, suppressing a node whose kind is
  /// unchanged since the last flush (except `Update`), and emits one
  /// [`Event::Member`] batch per surviving kind.  The `last` map persists across
  /// flushes so the suppression is stateful.
  ///
  /// **Eviction rule (bounds `last` to live membership):** when a node's flushed
  /// event is [`MemberEventKind::Reap`] its id is REMOVED from `last` rather than
  /// recorded.  `Reap` is the terminal removal-from-membership signal — the node
  /// is gone from the endpoint's `states`, so retaining it would grow `last`
  /// without bound as distinct ids churn through join → … → reap (the reference
  /// serf implementation leaks here).  Forgetting a reaped id is also correct:
  /// its later re-join is a genuinely new member, and a `Join` differs from the
  /// forgotten `Reap` so it re-emits regardless.  Every non-`Reap` kind can still
  /// transition, so it is retained to suppress a repeated identical status.
  pub(crate) fn flush(&mut self, out: &mut VecDeque<Event<I, A>>) {
    // At most five kinds, so a linear-probed Vec is cheaper than a hash map and
    // avoids requiring `Hash` on the public `MemberEventKind`.
    let mut grouped: Vec<(MemberEventKind, Vec<Member<I, A>>)> = Vec::new();
    for (id, latest) in self.latest.drain() {
      if let Some(&previous) = self.last.get(&id) {
        // Same status as last delivered, and not an Update → suppress. An Update
        // always re-emits: its payload (tags) may differ even at the same kind.
        if previous == latest.kind && latest.kind != MemberEventKind::Update {
          continue;
        }
      }
      if latest.kind == MemberEventKind::Reap {
        // Terminal: the node left membership for good — forget it so `last`
        // tracks only live members.
        self.last.remove(&id);
      } else {
        self.last.insert(id, latest.kind);
      }
      match grouped.iter_mut().find(|(k, _)| *k == latest.kind) {
        Some((_, members)) => members.push(latest.member),
        None => grouped.push((latest.kind, std::vec![latest.member])),
      }
    }
    for (kind, members) in grouped {
      out.push_back(Event::Member(MemberEvent::new(kind, members)));
    }
    self.window.reset();
  }

  /// Drop the buffered batch and disarm the window without emitting anything.
  ///
  /// Used when the machine transitions to `Shutdown` (a lost id-conflict vote):
  /// the reference implementation abandons its coalescer goroutine on shutdown,
  /// so the not-yet-flushed batch is discarded rather than delivered.  The `last`
  /// suppression history is retained (it is harmless once the machine is dead).
  pub(crate) fn reset(&mut self) {
    self.latest.clear();
    self.window.reset();
  }

  /// The number of ids currently held in the cross-flush suppression map.
  ///
  /// Bounded by live membership because `flush` evicts a node's id on its
  /// terminal `Reap`.
  #[cfg(test)]
  pub(crate) fn last_len(&self) -> usize {
    self.last.len()
  }
}

// ── UserEventCoalescer ──────────────────────────────────────────────────────────

/// The newest Lamport generation of a named user event within an open window.
struct LatestUserEvents {
  ltime: LamportTime,
  events: Vec<UserEventMessage>,
}

/// Coalesces user events over a time window, keeping only the newest Lamport
/// generation per event name.
///
/// Ports serf-core `coalesce/user.rs` `UserEventCoalescer`: for each event
/// `name`, buffer the events carrying the highest `ltime` seen so far — a newer
/// ltime clears the older buffer, an equal ltime accumulates (distinct payloads
/// at the same generation are all delivered), and an older ltime is dropped.
///
/// Only events that opted into coalescing (`UserEventMessage::cc == true`) are
/// fed here; the endpoint passes non-coalescing user events straight through,
/// mirroring the reference coalescer's `handle` predicate.
pub(crate) struct UserEventCoalescer {
  window: CoalesceWindow,
  events: FxHashMap<SmolStr, LatestUserEvents>,
}

impl UserEventCoalescer {
  /// Construct a user coalescer over the given windows.
  pub(crate) fn new(coalesce_period: Duration, quiescent_period: Duration) -> Self {
    Self {
      window: CoalesceWindow::new(coalesce_period, quiescent_period),
      events: FxHashMap::default(),
    }
  }

  /// Buffer a coalescing user event (dedup by name, newest ltime wins) and arm
  /// the flush window.
  pub(crate) fn feed(&mut self, event: UserEventMessage, now: Instant) {
    let ltime = event.ltime;
    match self.events.get_mut(&event.name) {
      None => {
        self.events.insert(
          event.name.clone(),
          LatestUserEvents {
            ltime,
            events: std::vec![event],
          },
        );
      }
      Some(latest) => {
        if latest.ltime < ltime {
          // A newer generation supersedes the buffered one.
          latest.ltime = ltime;
          latest.events.clear();
          latest.events.push(event);
        } else if latest.ltime == ltime {
          // Same generation: keep both (e.g. distinct payloads at one ltime).
          latest.events.push(event);
        }
        // Older generation: drop.
      }
    }
    self.window.arm(now);
  }

  /// The next flush deadline while the window is open, else `None`.
  pub(crate) fn flush_deadline(&self) -> Option<Instant> {
    self.window.deadline()
  }

  /// Whether the flush deadline has elapsed at `now`.
  pub(crate) fn due(&self, now: Instant) -> bool {
    self.window.due(now)
  }

  /// Emit the coalesced user events into `out` and disarm the window.
  pub(crate) fn flush<I, A>(&mut self, out: &mut VecDeque<Event<I, A>>) {
    for (_, latest) in self.events.drain() {
      for event in latest.events {
        out.push_back(Event::User(event));
      }
    }
    self.window.reset();
  }

  /// Drop the buffered batch and disarm the window without emitting anything
  /// (see [`MemberEventCoalescer::reset`]).
  pub(crate) fn reset(&mut self) {
    self.events.clear();
    self.window.reset();
  }
}

#[cfg(test)]
mod tests;
