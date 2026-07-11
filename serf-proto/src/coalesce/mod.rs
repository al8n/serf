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

pub(crate) mod drop_counter;

pub use drop_counter::DropCounter;

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

/// Hard cap on the number of distinct nodes buffered in a single member-coalesce
/// window.
///
/// The `latest` map holds one entry per node id observed while the window is
/// open; a burst of membership churn across many distinct ids would otherwise
/// grow it without bound before the window closes.  At the cap a change for a
/// NEW id is dropped (and counted), while an id already buffered still updates in
/// place, so the collapse of an in-progress node stays exact.  A normal cluster
/// never approaches the limit — only an adversarial fan-out of distinct ids is
/// bounded.  Mirrors the inbound-query overflow cap on the endpoint.
const MAX_COALESCED_MEMBER_EVENTS: usize = 2048;

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
/// * `last` remembers the last EMITTED `(kind, address)` per node id ACROSS
///   flushes, so a repeated identical observation is suppressed — except
///   [`MemberEventKind::Update`], which always re-emits because a node's tags may
///   have changed.  Pairing the address alongside the kind lets a rejoin at a NEW
///   address (same id, same kind) re-emit, so consumers learn the node moved
///   rather than retaining the stale address.
///
/// A flush groups the surviving observations by kind and emits one
/// [`Event::Member`] batch per kind.
pub(crate) struct MemberEventCoalescer<I, A>
where
  I: Eq + Hash,
{
  window: CoalesceWindow,
  latest: FxHashMap<I, LatestMember<I, A>>,
  last: FxHashMap<I, (MemberEventKind, A)>,
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
  ///
  /// A change for a not-yet-buffered id dropped by the cardinality cap increments
  /// `drops` (the endpoint-owned member shed counter); the counter is never read
  /// back by any protocol path.
  pub(crate) fn feed<D>(
    &mut self,
    kind: MemberEventKind,
    members: Vec<Member<I, A>>,
    now: Instant,
    drops: &mut D,
  ) where
    D: DropCounter,
  {
    let mut admitted = false;
    for member in members {
      let id = member.node().id_ref().clone();
      // A terminal Reap forgets the node's last-emitted status at feed time: a
      // Reap overwritten by a rejoin Join later in the same window would never
      // reach a flush-time eviction, leaving a stale `last[id]` that suppresses
      // the genuinely-new Join.  This eviction is UNCONDITIONAL and runs before
      // the cardinality gate below: a Reap dropped by an overflowing window must
      // still forget the id, or the stale suppression entry outlives it.
      if kind == MemberEventKind::Reap {
        self.last.remove(&id);
      }
      // Bound the per-window map: a change for a not-yet-buffered id is dropped
      // once the map is at capacity, while an id already buffered still updates
      // in place (its collapse must stay exact).  Only genuinely-new ids can grow
      // the map, so only they are gated.
      if !self.latest.contains_key(&id) && self.latest.len() >= MAX_COALESCED_MEMBER_EVENTS {
        drops.incr_saturating();
        continue;
      }
      self.latest.insert(id, LatestMember { kind, member });
      admitted = true;
    }
    // Arm only when at least one member was buffered this call: a fully-rejected
    // batch must not extend the quiescent window.
    if admitted {
      self.window.arm(now);
    }
  }

  /// The next flush deadline while the window is open, else `None`.
  pub(crate) fn flush_deadline(&self) -> Option<Instant> {
    self.window.deadline()
  }

  /// Whether the flush deadline has elapsed at `now`.
  pub(crate) fn due(&self, now: Instant) -> bool {
    self.window.due(now)
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

  /// The number of distinct ids currently buffered in the open window.
  #[cfg(test)]
  pub(crate) fn latest_len(&self) -> usize {
    self.latest.len()
  }

  /// Whether the cross-flush suppression map currently holds `id`.
  #[cfg(test)]
  pub(crate) fn last_contains(&self, id: &I) -> bool {
    self.last.contains_key(id)
  }
}

impl<I, A> MemberEventCoalescer<I, A>
where
  I: Eq + Hash + Clone,
  A: Clone + PartialEq,
{
  /// Emit the coalesced member events into `out` and disarm the window.
  ///
  /// Groups the surviving observations by kind, suppressing a node whose kind AND
  /// address are both unchanged since the last flush (except `Update`), and emits
  /// one [`Event::Member`] batch per surviving kind.  The `last` map persists
  /// across flushes so the suppression is stateful.
  ///
  /// **Eviction rule (bounds `last` to live membership):** a node's id is
  /// forgotten from `last` at FEED time the instant a [`MemberEventKind::Reap`]
  /// is buffered for it, and a `Reap` is never recorded on flush.  `Reap` is the
  /// terminal removal-from-membership signal — the node is gone from the
  /// endpoint's `states`, so retaining it would grow `last` without bound as
  /// distinct ids churn through join → … → reap (the reference serf
  /// implementation leaks here).  `last` stays keyed by id — one entry per id —
  /// so pairing the address into the value leaves that live-membership bound
  /// intact.  Evicting at feed time (rather than on flush) also keeps a `Reap`
  /// that is overwritten by a rejoin `Join` within the same window from leaving a
  /// stale `last[id]` that would wrongly suppress the genuinely-new `Join`.
  /// Forgetting a reaped id is correct regardless: its later re-join is a
  /// genuinely new member, and a `Join` differs from the absent entry so it
  /// re-emits.  Every non-`Reap` kind can still transition, so it is retained to
  /// suppress a repeated identical observation.
  pub(crate) fn flush(&mut self, out: &mut VecDeque<Event<I, A>>) {
    // At most five kinds, so a linear-probed Vec is cheaper than a hash map and
    // avoids requiring `Hash` on the public `MemberEventKind`.
    let mut grouped: Vec<(MemberEventKind, Vec<Member<I, A>>)> = Vec::new();
    for (id, latest) in self.latest.drain() {
      let addr = latest.member.node().addr_ref().clone();
      if let Some((prev_kind, prev_addr)) = self.last.get(&id) {
        // Suppress only a genuinely unchanged observation — same kind AND same
        // address. A rejoin at a new address (same id, same kind) must still emit
        // so consumers learn the node moved. An Update always re-emits: its tags
        // may differ even at an unchanged kind and address.
        if *prev_kind == latest.kind && *prev_addr == addr && latest.kind != MemberEventKind::Update
        {
          continue;
        }
      }
      // A Reap is never recorded in `last`: `feed` already evicted the id, and
      // storing it would grow `last` without bound as ids churn through
      // join → … → reap. A forgotten reaped id re-emits its later Join correctly
      // (Join differs from the absent entry). Every non-Reap kind is retained to
      // suppress a repeated identical observation.
      if latest.kind != MemberEventKind::Reap {
        self.last.insert(id, (latest.kind, addr));
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
  /// Upper bound on `buffered` (the total buffered event volume).  `None`
  /// disables the bound.
  cap: Option<core::num::NonZeroUsize>,
  /// Running sum of every buffered event count across `events`.  A flush emits
  /// one [`Event::User`] per buffered event, so this equals both the true
  /// buffered volume and the maximum flush burst — the quantity the `cap`
  /// bounds (not the per-name key count).
  buffered: usize,
}

impl UserEventCoalescer {
  /// Construct a user coalescer over the given windows, bounding the total
  /// buffered event volume to `cap` (`None` disables the bound).
  pub(crate) fn new(
    coalesce_period: Duration,
    quiescent_period: Duration,
    cap: Option<core::num::NonZeroUsize>,
  ) -> Self {
    Self {
      window: CoalesceWindow::new(coalesce_period, quiescent_period),
      events: FxHashMap::default(),
      cap,
      buffered: 0,
    }
  }

  /// Buffer a coalescing user event (dedup by name, newest ltime wins) and arm
  /// the flush window.
  ///
  /// The total buffered event volume is bounded by `cap`: an admission that
  /// would grow it past the cap is dropped and counted, EXCEPT a newer
  /// generation for a buffered name, which clears that name's older buffer first
  /// (net change `<= 0`) and so is always admitted.  An older generation is
  /// dropped as normal dedup and is NOT counted.  The window is armed only when
  /// an event is admitted, so a rejected or dropped-older event never extends
  /// the quiescent timer.
  ///
  /// A volume-cap drop increments `drops` (the endpoint-owned user shed counter);
  /// an older-generation dedup drop does not.  The counter is never read back by
  /// any protocol path.
  pub(crate) fn feed<D>(&mut self, event: UserEventMessage, now: Instant, drops: &mut D)
  where
    D: DropCounter,
  {
    let cap = self.cap.map_or(usize::MAX, core::num::NonZeroUsize::get);
    let ltime = event.ltime;
    let admitted = match self.events.get_mut(&event.name) {
      None => {
        if self.buffered >= cap {
          drops.incr_saturating();
          false
        } else {
          self.events.insert(
            event.name.clone(),
            LatestUserEvents {
              ltime,
              events: std::vec![event],
            },
          );
          self.buffered += 1;
          true
        }
      }
      Some(latest) => {
        if latest.ltime < ltime {
          // A newer generation supersedes the buffered one, so the net change is
          // `1 - old_len <= 0` and this is always admitted regardless of the cap.
          // Replace the buffer with a fresh single-element vector rather than
          // clearing it in place: `Vec::clear` retains the old capacity, so a name
          // repeatedly filled toward the cap and then superseded would keep a
          // large allocation while `buffered` reads low — retained memory the
          // count-based cap cannot see (a per-name `Theta(cap)` leak). Assigning a
          // new vector frees the old allocation.
          self.buffered -= latest.events.len();
          latest.ltime = ltime;
          latest.events = std::vec![event];
          self.buffered += 1;
          true
        } else if latest.ltime == ltime {
          // Same generation: keep both (e.g. distinct payloads at one ltime),
          // subject to the cap.
          if self.buffered >= cap {
            drops.incr_saturating();
            false
          } else {
            latest.events.push(event);
            self.buffered += 1;
            true
          }
        } else {
          // Older generation: drop (normal dedup, not counted).
          false
        }
      }
    };
    if admitted {
      self.window.arm(now);
    }
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
    // The map is now empty, so the buffered-volume invariant resets to zero
    // (a live-content reset, not a decrement).
    self.buffered = 0;
    self.window.reset();
  }

  /// Drop the buffered batch and disarm the window without emitting anything
  /// (see [`MemberEventCoalescer::reset`]).
  pub(crate) fn reset(&mut self) {
    self.events.clear();
    self.buffered = 0;
    self.window.reset();
  }

  /// The total buffered event volume (the running sum bounded by `cap`).
  #[cfg(test)]
  pub(crate) fn buffered(&self) -> usize {
    self.buffered
  }

  /// The number of distinct event names currently buffered.
  #[cfg(test)]
  pub(crate) fn distinct_names(&self) -> usize {
    self.events.len()
  }

  /// The true sum of buffered payload counts across every name — the invariant
  /// `buffered` must equal.
  #[cfg(test)]
  pub(crate) fn live_payload_count(&self) -> usize {
    self.events.values().map(|l| l.events.len()).sum()
  }

  /// The total ALLOCATED capacity retained across every name's buffer — bounded
  /// alongside `buffered`, since a superseded buffer must free its allocation
  /// rather than retain it.
  #[cfg(test)]
  pub(crate) fn retained_capacity(&self) -> usize {
    self.events.values().map(|l| l.events.capacity()).sum()
  }
}

#[cfg(test)]
mod tests;
