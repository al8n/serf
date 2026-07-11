//! Driver helpers shared by the reactor's backend driver pump.
//!
//! The observation / event hand-off helpers independent of the reliable plane:
//! the [`Delegate`](crate::delegate::Delegate) hook dispatcher, the
//! coordinator-allocated exchange-id alias, and the observation byte-backstop
//! weight. Unlike serf-compio's `driver/shared`, there is **no** `yield_once`
//! and **no** `drain_past_due_udp`: the reactor pump is readiness-based, so it
//! recv-loops the gossip socket to kernel-empty and fires `handle_timeout` inline
//! — there is no completion-backend past-due drain to build.

#[cfg(any(feature = "tcp", feature = "quic"))]
use agnostic::net::UdpSocket;
#[cfg(any(feature = "tcp", feature = "quic"))]
use core::task::{Context, Poll};
#[cfg(any(feature = "tcp", feature = "quic"))]
use std::{collections::VecDeque, io, net::SocketAddr, vec::Vec};

/// Coordinator-allocated handle for one in-flight reliable exchange.
///
/// Shared by the stream driver and the per-bridge task so they agree on the same
/// opaque id without the rest of the crate naming the machine's streams module; the
/// QUIC driver correlates its await-result joins on the same id domain.
#[cfg(any(feature = "tcp", feature = "quic"))]
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

// ── leave-drain retention ─────────────────────────────────────────────────────
//
// Periodic gossip is best-effort: a readiness-based UDP send that returns
// `Pending` (kernel buffer full) or `Ready(Err)` drops the datagram, and SWIM
// re-sends on the next round. The graceful-leave fan-out has NO next round — a
// dropped farewell leaves peers to classify the intentional departure as a
// failure while `leave()` reported success — so once `leave()` has been
// initiated the pump RETAINS the fan-out datagrams whose send did not complete
// and retries them until the socket accepts them (or teardown exhausts them).

/// Encoded-and-transformed leave-farewell gossip datagrams retained after a
/// non-completing UDP send, keyed by their destination and retried FIFO.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) type LeaveDrain = VecDeque<(SocketAddr, Vec<u8>)>;

/// Surface a leave-farewell datagram the driver could not deliver. Unlike
/// best-effort periodic gossip the drop is logged (the send error is
/// deterministic, config/IO-class, and the fan-out has no next round to mask
/// it). A no-op without the `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
fn trace_leave_send_error(_peer: SocketAddr, _err: &io::Error) {
  #[cfg(feature = "tracing")]
  tracing::debug!(peer = %_peer, error = %_err, "serf leave farewell datagram send failed");
}

/// Surface retained leave-farewell datagrams abandoned when the teardown drain
/// deadline won — the socket stayed unwritable, so shutdown proceeds and the
/// affected peers will read the departure as a failure. A no-op without the
/// `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn trace_leave_drain_residue(_count: usize) {
  #[cfg(feature = "tracing")]
  tracing::debug!(
    residual = _count,
    "serf leave farewell datagrams abandoned at the teardown drain deadline"
  );
}

/// Surface a leave-farewell datagram dropped because it could not be encoded or
/// encrypted — a deterministic config-class failure, unlike a transient
/// best-effort gossip drop. A no-op without the `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn trace_leave_transform_error(_peer: SocketAddr) {
  #[cfg(feature = "tracing")]
  tracing::debug!(peer = %_peer, "serf leave farewell datagram could not be encoded or encrypted");
}

/// Record the outcome of one readiness-based leave-farewell datagram send into
/// `retained`:
/// - `Ready(Ok)` — it left the socket; nothing to retain.
/// - `Ready(Err)` — attempted; logged and NOT retained (a deterministic error
///   the next round cannot mask).
/// - `Pending` — the socket is backpressured; `(peer, datagram)` is retained for
///   the next writable wake (the datagram is copied only on this path).
///
/// Returns `true` when the datagram was retained, so a caller draining fresh
/// transmits learns the socket is backpressured.
/// Upper bound on how long a pump's teardown parks waiting for retained
/// leave-farewell datagrams to drain before releasing the socket anyway. Keeps
/// shutdown from hanging on a persistently unwritable socket while still giving
/// the farewell a real window to reach the wire.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) const LEAVE_DRAIN_TEARDOWN_BOUND: core::time::Duration =
  core::time::Duration::from_secs(1);

#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn retain_leave_datagram(
  retained: &mut LeaveDrain,
  peer: SocketAddr,
  datagram: &[u8],
  outcome: Poll<io::Result<usize>>,
) -> bool {
  match outcome {
    Poll::Ready(Ok(_)) => false,
    Poll::Ready(Err(err)) => {
      trace_leave_send_error(peer, &err);
      false
    }
    Poll::Pending => {
      retained.push_back((peer, datagram.to_vec()));
      true
    }
  }
}

/// Send one already-transformed gossip datagram over the plain-UDP socket.
///
/// Periodic gossip (`leave_initiated == false`) is best-effort: a non-completing
/// send drops the datagram. Once `leave()` has been initiated the leave fan-out
/// is retained on backpressure via [`retain_leave_datagram`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn poll_send_gossip<S>(
  retained: &mut LeaveDrain,
  leave_initiated: bool,
  socket: Option<&S>,
  cx: &mut Context<'_>,
  peer: SocketAddr,
  on_wire: &[u8],
) where
  S: UdpSocket,
{
  let Some(socket) = socket else {
    return;
  };
  let outcome = socket.poll_send_to(cx, on_wire, peer);
  if leave_initiated {
    retain_leave_datagram(retained, peer, on_wire, outcome);
  }
}

/// Retry the retained leave-farewell datagrams FIRST (oldest to newest), sending
/// each over `socket` until one is backpressured. A `Ready(Err)` is logged and
/// counts as attempted; a `Pending` keeps that datagram (and every later one)
/// retained and stops the pass. Bounded: a single front-to-back sweep with no
/// re-enqueue of a just-sent datagram, so it cannot loop.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn retry_retained_leave<S>(
  retained: &mut LeaveDrain,
  socket: Option<&S>,
  cx: &mut Context<'_>,
) where
  S: UdpSocket,
{
  let Some(socket) = socket else {
    return;
  };
  while let Some((peer, datagram)) = retained.pop_front() {
    match socket.poll_send_to(cx, &datagram, peer) {
      Poll::Ready(Ok(_)) => {}
      Poll::Ready(Err(err)) => trace_leave_send_error(peer, &err),
      Poll::Pending => {
        retained.push_front((peer, datagram));
        break;
      }
    }
  }
}

#[cfg(all(test, any(feature = "tcp", feature = "quic")))]
mod tests;
