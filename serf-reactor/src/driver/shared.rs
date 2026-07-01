//! Driver helpers shared by the reactor's backend driver pump.
//!
//! The observation / event hand-off helpers independent of the reliable plane:
//! the [`Delegate`](crate::delegate::Delegate) hook dispatcher, the
//! coordinator-allocated exchange-id alias, and the observation byte-backstop
//! weight. Unlike serf-compio's `driver/shared`, there is **no** `yield_once`
//! and **no** `drain_past_due_udp`: the reactor pump is readiness-based, so it
//! recv-loops the gossip socket to kernel-empty and fires `handle_timeout` inline
//! — there is no completion-backend past-due drain to build.

/// Coordinator-allocated handle for one in-flight reliable exchange.
///
/// Shared by the stream driver and the per-bridge task so they agree on the same
/// opaque id without the rest of the crate naming the machine's streams module.
#[cfg(feature = "tcp")]
pub(crate) type ExchangeId = memberlist_proto::event::ExchangeId;

/// Byte-backstop weight of a serf event. Delegates to
/// [`serf_driver::observation_payload_bytes`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use serf_driver::observation_payload_bytes;

/// Dispatch the matching [`Delegate`](crate::delegate::Delegate) hook for one
/// drained serf [`Event`](serf_proto::event::Event).
///
/// Member hooks run once per affected member in the batch; user-event and query
/// hooks run once per event. The observation delegate observes transitions the
/// FSM has already applied — it is NOT an admission gate. Returns a `Send` future
/// (the delegate hooks are `Send`) so the observation task can drive it on a
/// multi-threaded runtime.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) async fn dispatch_event_delegate<I, A, D>(
  delegate: &D,
  ev: &serf_proto::event::Event<I, A>,
) where
  D: crate::delegate::Delegate<Id = I, Address = A>,
  I: Clone,
  A: Clone,
{
  use serf_proto::event::{Event, MemberEventKind};
  use std::sync::Arc;

  match ev {
    Event::Member(me) => {
      for m in me.members() {
        let arc = Arc::new(m.clone());
        match me.kind() {
          MemberEventKind::Join => delegate.notify_join(arc).await,
          MemberEventKind::Leave => delegate.notify_leave(arc).await,
          MemberEventKind::Failed => delegate.notify_failed(arc).await,
          MemberEventKind::Update => delegate.notify_update(arc).await,
          MemberEventKind::Reap => delegate.notify_reap(arc).await,
        }
      }
    }
    Event::User(msg) => delegate.notify_user_event(msg).await,
    Event::Query(ev) => delegate.notify_query(ev).await,
    // Other variants (QueryResponse, QueryAck, Shutdown, LeftCluster,
    // RelayDropped, DialRequested, KeyResponse, KeyRequest) carry no observation
    // hook — the driver surfaces them through the EventStream.
    _ => {}
  }
}
