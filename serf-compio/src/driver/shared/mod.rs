//! Driver helpers shared by every transport backend's driver loop.
//!
//! These are the observation / event hand-off helpers independent of the
//! reliable plane: the [`Delegate`] hook dispatcher, the cooperative yield
//! that drains a bounded observation channel, and the observation byte-backstop
//! accounting. They live here so every backend reuses them without duplicating
//! the logic.
#[cfg(any(feature = "tcp", feature = "quic"))]
use core::task::Poll;

/// Yield to the runtime exactly once.
///
/// The event drain is synchronous — no `.await` fires for membership events —
/// so on a single-threaded runtime the observation task is not scheduled
/// mid-drain. A bounded `obs_tx` would overflow on a single large-but-valid
/// burst (e.g. a join push-pull carrying many members) before the task drains
/// a single event. Yielding hands the scheduler to the already-woken
/// observation task so it can drain `obs_rx` before the drain continues.
///
/// Runtime-agnostic: re-arms the waker and returns `Pending` once, so the
/// executor runs other ready tasks before re-polling this one.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) async fn yield_once() {
  let mut yielded = false;
  core::future::poll_fn(move |cx| {
    if yielded {
      Poll::Ready(())
    } else {
      yielded = true;
      cx.waker().wake_by_ref();
      Poll::Pending
    }
  })
  .await
}

/// Coordinator-allocated handle for one in-flight reliable exchange.
///
/// Shared by the TCP driver and the per-bridge task so they agree on the
/// same opaque id without the rest of the crate naming the machine's
/// streams module.
#[cfg(feature = "tcp")]
pub(crate) type ExchangeId = memberlist_proto::event::ExchangeId;

/// Dispatch the matching [`Delegate`] hook for one drained serf [`Event`].
///
/// Member hooks run once per affected member in the batch; user-event and
/// query hooks run once per event. The observation delegate observes
/// transitions the FSM has already applied — it is NOT an admission gate.
///
/// Requires a stream or QUIC transport feature.
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
    // RelayDropped, DialRequested, KeyResponse, KeyRequest) carry no
    // observation hook — the driver surfaces them through the EventStream.
    _ => {}
  }
}

/// Byte-backstop weight accounting: add a just-enqueued event's payload
/// weight (if any) to the counter. Paired with the subtract in each
/// driver's observation task on dequeue.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn add_obs_payload(counter: &std::cell::Cell<u64>, bytes: Option<u64>) {
  if let Some(b) = bytes {
    counter.set(counter.get().saturating_add(b));
  }
}

/// Byte-backstop weight of a serf event. Delegates to
/// [`serf_driver::observation_payload_bytes`].
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) use serf_driver::observation_payload_bytes;

// ── leave-farewell delivery ───────────────────────────────────────────────────
//
// Periodic gossip is best-effort: a failed UDP send drops the datagram, and SWIM
// re-sends on the next round. The graceful-leave fan-out has NO next round — a
// dropped farewell leaves peers to classify the intentional departure as a
// failure while `leave()` reported success — so once `leave()` has been initiated
// the pump CLASSIFIES each fan-out send's outcome instead of discarding it, and a
// local delivery failure resolves the parked leave with
// `LeaveFarewellUndelivered` rather than a false `Ok`.
//
// Completion I/O has no backpressure state to park on: `send_to().await` resolves
// only once the operation itself completed, and compio's backends fold a
// would-block into an internal re-arm and an interrupt into a syscall retry. So
// each awaited send either handed the datagram to the socket or answered with a
// real error — there is nothing to retain for a later writable wake.
//
// An errored send never accepted the CURRENT datagram, and on a shared
// unconnected UDP socket the kernel may surface an asynchronous error left by an
// EARLIER packet to a DIFFERENT peer — so an ICMP-reflection-class error
// (reset/refused) is disambiguated by bounded re-send: each re-send is a distinct
// syscall that both drains the stale error slot and re-hands this datagram to the
// socket, and an error that persists across the re-sends is credibly this
// destination's own answer (a peer that is itself gone), which the reference
// implementation logs and proceeds past. Every other error kind — aborts (a local
// software abort on Windows), unreachables (usually the LOCAL routing table's
// answer), a closed or invalid socket — is a local delivery failure that fails the
// leave.

/// Per-pump accounting for the graceful-leave farewell fan-out.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct Farewell {
  /// `leave()` has been initiated, so the gossip egress carries the departure
  /// fan-out: a send that fails is classified rather than dropped best-effort.
  pub(crate) initiated: bool,
  /// A LOCAL send or transform failure occurred — the farewell (or part of it)
  /// never left this host — so the parked leave resolves
  /// [`LeaveFarewellUndelivered`](crate::error::SerfError::LeaveFarewellUndelivered)
  /// instead of a false `Ok`.
  pub(crate) send_failed: bool,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl Farewell {
  /// A pump that has not yet initiated a leave.
  pub(crate) const fn new() -> Self {
    Self {
      initiated: false,
      send_failed: false,
    }
  }
}

/// Total ICMP-class (`ConnectionReset` / `ConnectionRefused`) send errors one
/// farewell datagram absorbs before it is dropped as answered-by-the-network.
/// The first errors are ambiguous (a stale asynchronous error from an earlier
/// packet to a different peer may occupy the socket's error slot), so the
/// datagram is re-sent; at the limit the answer is attributed to this destination
/// itself.
#[cfg(any(feature = "tcp", feature = "quic"))]
const FAREWELL_ICMP_ERROR_LIMIT: u8 = 3;

/// The final outcome of one graceful leave: `Ok` when every farewell datagram
/// reached the socket, [`LeaveFarewellUndelivered`] when a local send or
/// transform failure lost part of the fan-out.
///
/// [`LeaveFarewellUndelivered`]: crate::error::SerfError::LeaveFarewellUndelivered
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn leave_outcome(send_failed: bool) -> crate::error::Result<()> {
  if send_failed {
    Err(crate::error::SerfError::LeaveFarewellUndelivered)
  } else {
    Ok(())
  }
}

/// The disposition of one errored leave-farewell send.
#[cfg(any(feature = "tcp", feature = "quic"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ErroredFarewell {
  /// Ambiguous ICMP-class error under the limit: re-send the datagram (with the
  /// bumped absorb count) — the re-send drains a possibly-stale asynchronous
  /// error slot and re-hands the datagram to the socket.
  Resend(u8),
  /// ICMP-class errors persisted to [`FAREWELL_ICMP_ERROR_LIMIT`]: the answer is
  /// credibly this destination's own (the peer is itself gone). Logged and
  /// dropped without failing the leave, as the reference implementation does.
  PeerAnswered,
  /// Any other error kind: a LOCAL delivery failure — the farewell never left
  /// this host — so the leave resolves
  /// [`LeaveFarewellUndelivered`](crate::error::SerfError::LeaveFarewellUndelivered).
  LocalFailure,
}

/// Classify one errored farewell send, given how many ICMP-class errors this
/// datagram has already absorbed.
///
/// Only `ConnectionReset` / `ConnectionRefused` are the ambiguous
/// ICMP-reflection class: they are what a gone peer's ICMP answer surfaces on
/// every platform (and what Windows reflects routinely after a send to a closed
/// port), and on a shared unconnected socket they may equally be a stale answer
/// to an EARLIER packet for a different peer — hence bounded re-send rather than
/// trusting either reading. `ConnectionAborted` is a local software abort on
/// Windows, the unreachables usually report the LOCAL routing table's answer, and
/// everything else (a closed or invalid socket, a vanished source address, a
/// broken pipe) is unambiguously local.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn classify_errored_farewell(icmp_errors: u8, err: &std::io::Error) -> ErroredFarewell {
  if matches!(
    err.kind(),
    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
  ) {
    let absorbed = icmp_errors.saturating_add(1);
    if absorbed < FAREWELL_ICMP_ERROR_LIMIT {
      ErroredFarewell::Resend(absorbed)
    } else {
      ErroredFarewell::PeerAnswered
    }
  } else {
    ErroredFarewell::LocalFailure
  }
}

/// Surface a leave-farewell datagram the driver could not deliver. Unlike
/// best-effort periodic gossip the drop is logged (the send error is
/// deterministic, config/IO-class, and the fan-out has no next round to mask it).
/// A no-op without the `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
fn trace_leave_send_error(_peer: std::net::SocketAddr, _err: &std::io::Error) {
  #[cfg(feature = "tracing")]
  tracing::debug!(peer = %_peer, error = %_err, "serf leave farewell datagram send failed");
}

/// Surface a leave-farewell datagram dropped after absorbing
/// [`FAREWELL_ICMP_ERROR_LIMIT`] ICMP-class send errors: the network's answer is
/// attributed to this destination (a peer that is itself gone), and the reference
/// implementation logs such peers and proceeds. A no-op without the `tracing`
/// feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
fn trace_leave_peer_answered(_peer: std::net::SocketAddr) {
  #[cfg(feature = "tracing")]
  tracing::debug!(
    peer = %_peer,
    "serf leave farewell dropped after repeated ICMP-class send errors; peer presumed gone"
  );
}

/// Surface a leave-farewell datagram dropped because it could not be encoded or
/// encrypted — a deterministic config-class failure, unlike a transient
/// best-effort gossip drop. A no-op without the `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn trace_leave_transform_error(_peer: std::net::SocketAddr) {
  #[cfg(feature = "tracing")]
  tracing::debug!(peer = %_peer, "serf leave farewell datagram could not be encoded or encrypted");
}

/// Send one already-transformed gossip datagram over the plain-UDP socket.
///
/// Periodic gossip (`acct.initiated == false`) is best-effort: a failed send
/// drops the datagram and the next round recovers it. Once `leave()` has been
/// initiated the datagram is the departure fan-out, so a failed send is
/// classified by [`classify_errored_farewell`]: an ambiguous ICMP-class answer is
/// re-sent (bounded), and a LOCAL failure records `acct.send_failed` so the parked
/// leave resolves [`LeaveFarewellUndelivered`] instead of a false `Ok`.
///
/// [`LeaveFarewellUndelivered`]: crate::error::SerfError::LeaveFarewellUndelivered
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) async fn send_gossip_datagram(
  socket: &compio::net::UdpSocket,
  peer: std::net::SocketAddr,
  on_wire: Vec<u8>,
  acct: &mut Farewell,
) {
  use compio::buf::BufResult;

  if !acct.initiated {
    let BufResult(res, _buf) = socket.send_to(on_wire, peer).await;
    // Ignoring Err: a transient send error is non-fatal for periodic gossip —
    // gossip is lossy and the next probe/gossip round recovers.
    let _ = res;
    return;
  }

  // The leave fan-out. Bounded by the ICMP absorb allowance: each pass either
  // hands the datagram to the socket, re-sends it against a possibly-stale error
  // slot, or reaches a terminal classification, so the loop always ends.
  let mut buf = on_wire;
  let mut icmp_errors = 0u8;
  loop {
    let BufResult(res, returned) = socket.send_to(buf, peer).await;
    buf = returned;
    let Err(err) = res else {
      return;
    };
    trace_leave_send_error(peer, &err);
    match classify_errored_farewell(icmp_errors, &err) {
      ErroredFarewell::Resend(absorbed) => icmp_errors = absorbed,
      ErroredFarewell::PeerAnswered => {
        trace_leave_peer_answered(peer);
        return;
      }
      ErroredFarewell::LocalFailure => {
        acct.send_failed = true;
        return;
      }
    }
  }
}

/// Bounded past-due UDP drain shared by both driver pumps.
///
/// When the coordinator's wake deadline is already past, the driver must not
/// fire `handle_timeout` while a probe Ack still sits unread in the kernel
/// socket queue: the shared UDP socket interleaves gossip Acks with other
/// datagrams (raw QUIC packets on the QUIC plane, unrelated gossip on both), so
/// reading only the FIRST buffered datagram can leave a live peer's Ack unread
/// and the peer falsely suspected. Drain every immediately-ready datagram —
/// bounded by `budget` — so `on_datagram` decodes each before the caller
/// re-checks the deadline.
///
/// # Emptiness via a completion reap, not a time window
///
/// Each iteration polls one one-shot `recv_from`. If it is `Pending`, force a
/// synchronous, non-blocking proactor reap — [`Runtime::poll_with`] with a
/// [`Duration::ZERO`](core::time::Duration::ZERO) timeout — then re-poll:
/// `Ready` means a queued datagram was reaped, still-`Pending` means the socket
/// is genuinely empty. That is a CQE/readiness-grounded emptiness signal, not a
/// `sleep`-length guess at how long a kernel-buffered datagram needs to surface.
///
/// The race this guards against is **io_uring-specific**: there a
/// freshly-submitted `recv` is always `Pending` on its first poll (the SQE has
/// not been reaped yet), so any timer raced against it could fire the timeout
/// while a buffered Ack went unread. `poll_with(ZERO)` submits and reaps that SQE
/// inline, which is the io_uring fix. On kqueue and IOCP the recv runs an eager
/// syscall at submit time and is already `Ready` on its first poll for buffered
/// data, so there the eager syscall is itself the emptiness primitive and the
/// reap is a harmless no-op.
///
/// `on_datagram` processes each datagram and returns whether the deadline is
/// STILL past — returning `false` (a drained Ack resolved the probe) ends the
/// drain early so the main select regains fairness over any remaining datagrams.
/// `budget` caps the loop so a continuous flood cannot starve the timer (callers
/// pass a nonzero cap so recv always gets at least one shot).
///
/// Returns `true` iff at least one datagram was consumed (endpoint state may have
/// changed). There is no emptiness gate on firing: the caller fires
/// `handle_timeout` purely on its post-drain deadline re-check. When the socket
/// drained empty the Ack was already reaped and decoded, so the deadline is no
/// longer past; when a budget cap or a flood cut the drain short, firing for
/// liveness yields at worst a transient, SWIM-refutable false Suspect rather than
/// the frozen-timer stall.
///
/// The residual exact-instant arrival races — an io_uring recv cancelled on drop
/// losing its datagram (pre-existing), and the IOCP window where a completion is
/// posted just after the reap — are the irreducible "the Ack arrives the same
/// instant the timer fires" case, delegated to SWIM suspect-refute with no
/// machine-side compensation.
///
/// [`Runtime::poll_with`]: compio::runtime::Runtime::poll_with
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) async fn drain_past_due_udp<F>(
  socket: &compio::net::UdpSocket,
  recv_buf_len: usize,
  budget: usize,
  mut on_datagram: F,
) -> bool
where
  F: FnMut(std::net::SocketAddr, &[u8]) -> bool,
{
  use compio::buf::BufResult;
  use core::{future::Future, task::Poll};
  use futures_util::pin_mut;

  let mut drained = false;
  for _ in 0..budget {
    let buf = vec![0u8; recv_buf_len];
    let recv = socket.recv_from(buf);
    pin_mut!(recv);
    let outcome = core::future::poll_fn(|cx| match recv.as_mut().poll(cx) {
      Poll::Ready(br) => Poll::Ready(Some(br)),
      Poll::Pending => {
        // Ignoring Err: try_with_current returns Err only with no current runtime,
        // which is unreachable inside the driver loop's `block_on`.
        let _ = compio::runtime::Runtime::try_with_current(|rt| {
          rt.poll_with(Some(core::time::Duration::ZERO))
        });
        match recv.as_mut().poll(cx) {
          Poll::Ready(br) => Poll::Ready(Some(br)),
          // Still Pending after a real reap — the socket is genuinely empty.
          Poll::Pending => Poll::Ready(None),
        }
      }
    })
    .await;
    match outcome {
      Some(BufResult(Ok((n, src)), buf)) => {
        drained = true;
        // A drained Ack cleared the deadline — stop so the main select regains
        // fairness over any remaining datagrams.
        if !on_datagram(src, &buf[..n]) {
          break;
        }
      }
      // A transient recv error ends the drain like an empty socket.
      Some(BufResult(Err(_), _)) => break,
      // Still Pending after a real reap → the socket is empty.
      None => break,
    }
  }
  drained
}

#[cfg(all(test, any(feature = "tcp", feature = "quic")))]
mod tests;
