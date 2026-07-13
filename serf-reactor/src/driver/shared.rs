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
use memberlist_proto::Instant;
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
//
// An errored send never accepted the CURRENT datagram, and on a shared
// unconnected UDP socket the kernel may surface an asynchronous error left by
// an EARLIER packet to a DIFFERENT peer — so an ICMP-reflection-class error
// (reset/refused) is disambiguated by bounded retry: the retry both drains the
// stale error slot and re-hands this datagram to the socket, and an error that
// persists across the retries is credibly this destination's own answer (a
// peer that is itself gone), which the reference implementation logs and
// proceeds past. Every other error kind — aborts (a local software abort on
// Windows), unreachables (usually the LOCAL routing table's answer), a closed
// or invalid socket — is a local delivery failure that fails the leave.

/// One retained leave-farewell gossip datagram: its destination, the
/// encoded-and-transformed bytes, and how many ICMP-class send errors it has
/// absorbed (bounded by [`FAREWELL_ICMP_ERROR_LIMIT`]).
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct LeaveDatagram {
  peer: SocketAddr,
  bytes: Vec<u8>,
  icmp_errors: u8,
}

#[cfg(all(test, any(feature = "tcp", feature = "quic")))]
impl LeaveDatagram {
  /// Test seam: build a retained entry directly (production entries are built
  /// only by the retention helpers in this module).
  // Test-only: consumed by the tokio-gated pump tests, so a smol-only test
  // build sees no caller.
  #[allow(dead_code)]
  pub(crate) fn for_tests(peer: SocketAddr, bytes: Vec<u8>, icmp_errors: u8) -> Self {
    Self {
      peer,
      bytes,
      icmp_errors,
    }
  }
}

/// Per-pump accounting for the leave-farewell fan-out's delivery.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) struct FarewellAccounting {
  /// A LOCAL send/transform failure occurred — the farewell (or part of it)
  /// never left this host — so the parked leave resolves
  /// [`LeaveFarewellUndelivered`](crate::error::SerfError::LeaveFarewellUndelivered)
  /// instead of a false `Ok`.
  pub(crate) send_failed: bool,
  /// Earliest instant the next retained-farewell retry pass may run. Armed
  /// whenever a datagram absorbs an ICMP-class error: consecutive absorbs must
  /// sample the socket's error slot at temporally DISTINCT instants, so
  /// back-to-back self-wake polls (a loaded pump re-waking itself) cannot burn
  /// the bounded allowance against one stale asynchronous error. Folded into
  /// the pump's timer targets so the wake arrives when the epoch elapses.
  pub(crate) retry_after: Option<Instant>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl FarewellAccounting {
  pub(crate) const fn new() -> Self {
    Self {
      send_failed: false,
      retry_after: None,
    }
  }
}

/// Minimum wall-clock spacing between retained-farewell retry passes once a
/// datagram has absorbed an ICMP-class error. Short enough that the full
/// [`FAREWELL_ICMP_ERROR_LIMIT`] allowance fits comfortably inside the
/// teardown drain bound and any realistic leave timeout; long enough that each
/// retry is a temporally distinct sample of the socket's error slot.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) const FAREWELL_ICMP_RETRY_INTERVAL: core::time::Duration =
  core::time::Duration::from_millis(100);

/// Leave-farewell datagrams retained after a non-completing UDP send, retried
/// FIFO.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) type LeaveDrain = VecDeque<LeaveDatagram>;

/// Total ICMP-class (`ConnectionReset` / `ConnectionRefused`) send errors one
/// farewell datagram absorbs before it is dropped as answered-by-the-network.
/// The first errors are ambiguous (a stale asynchronous error from an earlier
/// packet to a different peer may occupy the socket's error slot), so the
/// datagram is retried; at the limit the answer is attributed to this
/// destination itself.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) const FAREWELL_ICMP_ERROR_LIMIT: u8 = 3;

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

/// Surface a leave-farewell datagram dropped after absorbing
/// [`FAREWELL_ICMP_ERROR_LIMIT`] ICMP-class send errors: the network's answer
/// is attributed to this destination (a peer that is itself gone), and the
/// reference implementation logs such peers and proceeds. A no-op without the
/// `tracing` feature.
#[cfg(any(feature = "tcp", feature = "quic"))]
fn trace_leave_peer_answered(_peer: SocketAddr) {
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
pub(crate) fn trace_leave_transform_error(_peer: SocketAddr) {
  #[cfg(feature = "tracing")]
  tracing::debug!(peer = %_peer, "serf leave farewell datagram could not be encoded or encrypted");
}

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
enum ErroredFarewell {
  /// Ambiguous ICMP-class error under the retry limit: retain the datagram
  /// (with the bumped absorb count) and re-attempt it — the retry drains a
  /// possibly-stale asynchronous error slot and re-hands the datagram to the
  /// socket.
  Retry(u8),
  /// ICMP-class errors persisted to [`FAREWELL_ICMP_ERROR_LIMIT`]: the answer
  /// is credibly this destination's own (the peer is itself gone). Logged and
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
/// every platform (and what Windows reflects routinely after a send to a
/// closed port), and on a shared unconnected socket they may equally be a
/// stale answer to an EARLIER packet for a different peer — hence bounded
/// retry rather than trusting either reading. `ConnectionAborted` is a local
/// software abort on Windows, the unreachables usually report the LOCAL
/// routing table's answer, and everything else (a closed or invalid socket, a
/// vanished source address, a broken pipe) is unambiguously local.
#[cfg(any(feature = "tcp", feature = "quic"))]
fn classify_errored_farewell(icmp_errors: u8, err: &io::Error) -> ErroredFarewell {
  if matches!(
    err.kind(),
    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
  ) {
    let absorbed = icmp_errors.saturating_add(1);
    if absorbed < FAREWELL_ICMP_ERROR_LIMIT {
      ErroredFarewell::Retry(absorbed)
    } else {
      ErroredFarewell::PeerAnswered
    }
  } else {
    ErroredFarewell::LocalFailure
  }
}

/// Upper bound on how long a pump's teardown parks waiting for retained
/// leave-farewell datagrams to drain before releasing the socket anyway. Keeps
/// shutdown from hanging on a persistently unwritable socket while still giving
/// the farewell a real window to reach the wire.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) const LEAVE_DRAIN_TEARDOWN_BOUND: core::time::Duration =
  core::time::Duration::from_secs(1);

/// The parked key-response machinery, shared with the other runtime drivers
/// through `serf-driver`.
#[cfg(all(any(feature = "tcp", feature = "quic"), encryption))]
pub(crate) use serf_driver::{
  AppliedKeyRequest, KEYRING_PERSIST_POLL_INTERVAL, PendingKeyResponse, settle_parked_key_response,
};

/// Record the outcome of one readiness-based leave-farewell datagram send into
/// `retained`:
/// - `Ready(Ok)` — it left the socket; nothing to retain.
/// - `Ready(Err)` — the socket did NOT accept this datagram. An ICMP-class
///   error retains it for an epoch-gated retry (the error slot may hold a
///   stale answer to an earlier packet for a different peer) and arms
///   `acct.retry_after`; a LOCAL failure sets `acct.send_failed`, so the
///   parked leave resolves with an error instead of a false success.
/// - `Pending` — the socket is backpressured; the datagram is retained for the
///   next writable wake.
///
/// The datagram bytes are copied only on the retain paths. Returns `true` when
/// the datagram was retained.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn retain_leave_datagram(
  retained: &mut LeaveDrain,
  peer: SocketAddr,
  datagram: &[u8],
  outcome: Poll<io::Result<usize>>,
  now: Instant,
  acct: &mut FarewellAccounting,
) -> bool {
  match outcome {
    Poll::Ready(Ok(_)) => false,
    Poll::Ready(Err(err)) => {
      trace_leave_send_error(peer, &err);
      match classify_errored_farewell(0, &err) {
        ErroredFarewell::Retry(absorbed) => {
          acct.retry_after = Some(now + FAREWELL_ICMP_RETRY_INTERVAL);
          retained.push_back(LeaveDatagram {
            peer,
            bytes: datagram.to_vec(),
            icmp_errors: absorbed,
          });
          true
        }
        ErroredFarewell::PeerAnswered => {
          trace_leave_peer_answered(peer);
          false
        }
        ErroredFarewell::LocalFailure => {
          acct.send_failed = true;
          false
        }
      }
    }
    Poll::Pending => {
      retained.push_back(LeaveDatagram {
        peer,
        bytes: datagram.to_vec(),
        icmp_errors: 0,
      });
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn poll_send_gossip<S>(
  retained: &mut LeaveDrain,
  leave_initiated: bool,
  socket: Option<&S>,
  cx: &mut Context<'_>,
  peer: SocketAddr,
  on_wire: &[u8],
  now: Instant,
  acct: &mut FarewellAccounting,
) where
  S: UdpSocket,
{
  let Some(socket) = socket else {
    return;
  };
  let outcome = socket.poll_send_to(cx, on_wire, peer);
  if leave_initiated {
    retain_leave_datagram(retained, peer, on_wire, outcome, now, acct);
  }
}

/// Retry the retained leave-farewell datagrams FIRST (oldest to newest),
/// sending each over `socket` until one is backpressured. An ICMP-class
/// `Ready(Err)` re-retains the datagram at the BACK of the queue (bounded by
/// its absorb count) so the rest of the queue drains ahead of the re-attempt; a
/// LOCAL failure sets `send_failed` so the parked leave resolves with an error;
/// a `Pending` keeps that datagram (and every later one) retained and stops the
/// pass. Bounded: the pass pops at most the queue's initial length, so a
/// re-retained datagram is re-attempted on the NEXT pass, never this one.
#[cfg(any(feature = "tcp", feature = "quic"))]
pub(crate) fn retry_retained_leave<S>(
  retained: &mut LeaveDrain,
  socket: Option<&S>,
  cx: &mut Context<'_>,
  now: Instant,
  acct: &mut FarewellAccounting,
) where
  S: UdpSocket,
{
  let Some(socket) = socket else {
    return;
  };
  let mut budget = retained.len();
  while budget > 0 {
    budget -= 1;
    let Some(d) = retained.pop_front() else {
      break;
    };
    let outcome = socket.poll_send_to(cx, &d.bytes, d.peer);
    if settle_retried_farewell(retained, d, outcome, now, acct) {
      break;
    }
  }
}

/// Fold one retried farewell's send outcome back into the retention state.
/// An ICMP-class re-retention arms `acct.retry_after`, epoch-gating the next
/// retry pass. Returns `true` when the sweep must stop (the socket is
/// backpressured; the datagram went back to the FRONT so FIFO order is
/// preserved).
#[cfg(any(feature = "tcp", feature = "quic"))]
fn settle_retried_farewell(
  retained: &mut LeaveDrain,
  mut d: LeaveDatagram,
  outcome: Poll<io::Result<usize>>,
  now: Instant,
  acct: &mut FarewellAccounting,
) -> bool {
  match outcome {
    Poll::Ready(Ok(_)) => false,
    Poll::Ready(Err(err)) => {
      trace_leave_send_error(d.peer, &err);
      match classify_errored_farewell(d.icmp_errors, &err) {
        ErroredFarewell::Retry(absorbed) => {
          acct.retry_after = Some(now + FAREWELL_ICMP_RETRY_INTERVAL);
          d.icmp_errors = absorbed;
          retained.push_back(d);
        }
        ErroredFarewell::PeerAnswered => trace_leave_peer_answered(d.peer),
        ErroredFarewell::LocalFailure => acct.send_failed = true,
      }
      false
    }
    Poll::Pending => {
      retained.push_front(d);
      true
    }
  }
}

#[cfg(all(test, any(feature = "tcp", feature = "quic")))]
mod tests;
