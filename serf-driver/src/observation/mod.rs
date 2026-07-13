//! Observation channel byte-backstop accounting shared by the serf async driver crates.

use serf_proto::{event::Event, members::Member};

/// Byte-backstop weight of an event for the observation channel queue.
///
/// The byte cap exists to bound the queue against **application flood vectors** — events whose
/// volume and size an application or peer controls — so those are weighted by their full
/// heap-bearing payload:
/// - `User` / `Query`: event name + payload bytes (serf bounds these against a `name + payload`
///   size limit, so a large name with an empty payload still counts).
/// - `QueryResponse`: payload bytes.
/// - `Member`: total tag key+value byte length across the affected members (peer tag maps are
///   decoded from node metadata and a peer can spam meta updates).
///
/// Every other variant returns `None` — it is bounded by the event-**count** cap alone, not the
/// byte cap. That covers control/acknowledgement signals (`Shutdown`, `LeftCluster`, `QueryAck`,
/// `RelayDropped`, `DialRequested`), the admin-triggered, cluster-size-bounded key-management
/// results (`KeyResponse` / `KeyRequest`), and any future `#[non_exhaustive]` variant. Key
/// management is matched by the wildcard rather than a feature-gated arm on purpose: a gated arm
/// would depend on serf-driver's own encryption features, which can skew from serf-proto's and
/// silently drop the arm.
#[must_use]
pub fn observation_payload_bytes<I, A>(ev: &Event<I, A>) -> Option<u64> {
  match ev {
    Event::User(u) => Some((u.name.len() as u64).saturating_add(u.payload.len() as u64)),
    Event::Query(q) => Some((q.name().len() as u64).saturating_add(q.payload().len() as u64)),
    Event::QueryResponse(r) => Some(r.payload().len() as u64),
    Event::Member(me) => Some(member_tag_bytes(me.members())),
    // Bounded by the event-count cap (no large application-controlled payload).
    Event::QueryAck(_)
    | Event::Shutdown
    | Event::RelayDropped(_)
    | Event::LeftCluster
    | Event::DialRequested(_) => None,
    // Key-management results/requests (admin-triggered, cluster-bounded) and any future variant.
    _ => None,
  }
}

/// Total tag key+value byte length across `members` — the heap weight a `Member` event
/// contributes to the observation byte-backstop. Tag maps are decoded from peer node metadata
/// and can grow up to the wire meta ceiling under large tag sets.
pub(crate) fn member_tag_bytes<I, A>(members: &[Member<I, A>]) -> u64 {
  members
    .iter()
    .flat_map(|m| m.tags().0.iter())
    .map(|(k, v)| (k.len() + v.len()) as u64)
    .sum()
}

#[cfg(test)]
mod tests;
