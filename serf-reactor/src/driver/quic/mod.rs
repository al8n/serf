//! The QUIC-plane driver pump: a quinn-style `Future::poll` that solely owns the
//! serf [`QuicEndpoint`], its single UDP socket, and the periodic schedulers.
//!
//! Unlike the stream plane, QUIC carries no per-exchange bridge table: the
//! coordinator (quinn-proto inside `QuicEndpoint`) multiplexes the reliable
//! push/pull streams over the ONE UDP socket, and serf's datagram gossip rides the
//! same socket, both fed through [`QuicEndpoint::handle_udp`]. This is the
//! `Send`/`Arc`/`agnostic` sibling of serf-compio's `!Send` `quic_driver_loop`,
//! restructured onto memberlist-reactor's readiness pump: there is NO top-level
//! `select!` and NO completion-backend drain. Each poll drains queued
//! [`Command`]s, recv-loops the socket to kernel-empty (`poll_recv_from` →
//! `Poll::Pending`), runs [`drain_surfaces`](QuicDriver::drain_surfaces) (decode
//! the buffered gossip ingress — a datagram-carried Ack — route the QUIC/gossip
//! egress, fold events), and then fires `handle_timeout` INLINE at exactly one
//! site. serf-compio's `fire_quic_timeout` / `drain_past_due_udp` chokepoints are
//! deleted — those exist only because io_uring is completion-based; the reactor is
//! readiness-based and `Poll::Pending` from the socket IS the kernel-empty signal.
//!
//! ## The shared-UDP-path timer/reap gate
//!
//! On QUIC the reliable plane and the gossip plane share the one UDP socket, so —
//! unlike the stream driver, whose reliable completions ride a disjoint TCP FIFO a
//! watermark can gate exactly — a join's resolving push/pull completion and a
//! gossip flood arrive on the SAME path. There is thus no per-completion watermark
//! to observe; the non-premature gate is instead UDP-recv QUIESCENCE. When the
//! recv loop stops on `Poll::Pending` (`recv_quiescent`), every kernel-ready packet
//! — including any completing QUIC stream packet — was fed through `handle_udp`
//! this poll and its `ExchangeCompleted` folded by `drain_surfaces` BEFORE the
//! reaps run, so a due reap / reliable-exchange `handle_timeout` fires only over a
//! genuinely-absent completion. A stop that is NOT `Poll::Pending` — a saturated
//! batch (`recv_capped`) OR a recv error (which is not a kernel-empty signal: a
//! completing packet may sit behind it) — is backlog-uncertain and holds the fire
//! back until a bounded wall-clock staleness grace elapses, then fires for liveness
//! (quinn's connection timers must advance and a parked join/leave must resolve).
//! The grace is bounded-early exactly as the stream driver's SWIM tick is; on QUIC
//! it covers the reliable plane too, because that plane shares the same path.

#![cfg(feature = "quic")]

use std::{
  collections::{HashSet, VecDeque},
  future::Future,
  net::SocketAddr,
  pin::Pin,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  task::{Context, Poll},
  time::Duration,
};

use agnostic::{
  Runtime,
  net::{Net, UdpSocket},
};
use bytes::Bytes;
use flume::{Receiver, Sender, TrySendError};
use futures_channel::oneshot;
use memberlist_proto::{
  DatagramSendStatus, Instant, SeedableRng, StreamId, Transmit, UnreliableTransport,
  codec::{
    DecodeOptions, EncodeOptions, decode_incoming, encode_outgoing, encode_outgoing_compound,
    parse_messages,
  },
};
use serf_driver::SerfSnapshot;
use serf_proto::{
  ExchangeKind, ExchangeStatus, LamportTime, QuicEndpoint, event::Event, members::SerfState,
};
use smallvec::SmallVec;

#[cfg(encryption)]
use crate::command::{KeyCmd, ListKeysCmd};
#[cfg(encryption)]
use crate::delegate::KeyringDelegate;
use crate::{
  Channel,
  command::{
    Command, ForceLeaveCmd, JoinCmd, JoinKind, JoinReply, LeaveCmd, QueryCmd, RespondCmd,
    SetTagsCmd, ShutdownCmd, WaitForCompletionArgs,
  },
  delegate::Delegate,
  driver::{
    options::RuntimeOptions,
    shared::{
      ExchangeId, LEAVE_DRAIN_TEARDOWN_BOUND, LeaveDrain, dispatch_event_delegate, leave_outcome,
      observation_payload_bytes, poll_send_gossip, retry_retained_leave, trace_leave_drain_residue,
      trace_leave_transform_error,
    },
  },
  drop_counter::ReactorDropCounter,
  error::{JoinFailed, Result, SerfError},
  shared::Shared,
};
#[cfg(encryption)]
use serf_proto::{KeyResponseArgs, event::KeyRequest};

/// Hard ceiling on the gossip-plane contribution to the per-recv UDP buffer —
/// UDP's IPv4 wire payload is capped at 65507 bytes once the IP/UDP headers are
/// deducted, so inflating the gossip path past it just wastes an allocation. The
/// raw-QUIC plane is bounded independently by quinn's `max_udp_payload_size`
/// (≤ 65527) and is intentionally not subject to this gossip cap.
const GOSSIP_RECV_BUF_MAX: usize = 65507;

/// The largest the encrypted wrapper can inflate a gossip datagram, or `0` when no
/// encryption backend is built in — so an encrypted datagram is not silently
/// truncated by the kernel.
#[cfg(encryption)]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = memberlist_proto::ENCRYPTED_WRAPPER_OVERHEAD;
#[cfg(not(encryption))]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = 0;

/// Cap on the count of application-data events retained after a full observation
/// channel (the payload byte budget bounds their bytes; this bounds their count).
const OBS_OVERFLOW_MAX: usize = 1024;

/// How long a due `handle_timeout` / deadline reap may be held back under a
/// sustained UDP flood before it fires bounded-early.
///
/// On QUIC the reliable push/pull completions and the gossip/probe datagrams share
/// the one UDP recv path (there is no disjoint reliable FIFO to watermark, unlike
/// the stream driver), so a saturated recv batch could hide EITHER a probe Ack OR
/// a reliable-exchange completion behind the flood. Rather than gate each plane on
/// its own drain, the pump gates the single `handle_timeout` site (and the
/// join/leave reaps) on recv quiescence, and — when a flood pins the batch every
/// poll — fires after this bounded staleness. A `Duration`, not a poll count: at a
/// low `iter_drain_cap` (or a large exchange) a fixed count could elapse while real
/// pre-deadline work is still buffered, whereas a wall-clock grace bounds the
/// staleness directly. It is short because a false SWIM `Suspect` self-refutes
/// within the multi-second suspicion window and a prematurely-failed push/pull is
/// retriable, whereas freezing quinn's connection timers under load is not — so
/// liveness wins once the grace elapses.
const SHARED_PATH_STALENESS_GRACE: Duration = Duration::from_millis(5);

/// How long the pump parks after a UDP recv ERROR stop before retrying the recv,
/// when there is nothing else bounded to make progress on (no saturated batch, no
/// due deadline).
///
/// A recv error is not the kernel-empty `Poll::Pending` signal, so it must not be
/// read as quiescence — it gates the timer/reap like a saturated batch (see the
/// module docs). But it is ALSO not a reason to self-wake: an immediate
/// `wake_by_ref` on every errored poll would busy-spin a core between deadlines, and
/// — because a `Poll::Ready(Err(_))` from the socket registers NO readiness waker —
/// no socket wake will re-poll the pump either. So a recv error with nothing else
/// pending arms a real sleep for this bounded backoff and parks; the timer alone
/// re-polls and retries the errored recv. Small, because a UDP recv error is
/// typically transient (a stale ICMP port-unreachable surfacing as `ECONNREFUSED`,
/// or a momentary `ENOBUFS`) and clears on the next poll, and the shared socket's
/// gossip/QUIC recv is stalled only for this window.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(5);

/// Size the per-recv UDP buffer to the larger of the two planes that share this
/// one socket.
///
/// The gossip plane needs `gossip_mtu` inflated by the encrypted-wrapper overhead
/// (algorithm tag + nonce + AEAD auth tag), clamped at [`GOSSIP_RECV_BUF_MAX`].
/// The raw-QUIC plane needs whatever max UDP payload the quinn `EndpointConfig`
/// accepts — which a valid caller can set above the gossip MTU (quinn's default
/// 1472 already exceeds the 1400 default `gossip_mtu`, and callers can raise it
/// further). Sizing below either lets the kernel truncate that plane's largest
/// datagram before the coordinator's first-byte demux sees it, corrupting QUIC
/// handshakes/streams while leaving construction silently successful.
///
/// `quic_max_udp_payload` is quinn-bounded to `[1200, 65527]`, so it always fits
/// `usize`; the conversion fallback is purely defensive. The QUIC plane is NOT
/// clamped at [`GOSSIP_RECV_BUF_MAX`] — quinn already bounds it, and clamping would
/// shrink the buffer below a configured 65508..=65527 ceiling.
fn recv_buf_len_for(gossip_mtu: usize, quic_max_udp_payload: u64) -> usize {
  let gossip_path = gossip_mtu
    .saturating_add(ENCRYPTED_WRAPPER_OVERHEAD)
    .min(GOSSIP_RECV_BUF_MAX);
  let quic_path = usize::try_from(quic_max_udp_payload).unwrap_or(GOSSIP_RECV_BUF_MAX);
  gossip_path.max(quic_path)
}

/// Driver-side state for one outstanding await-result join call.
///
/// Mirrors the stream driver's `PendingJoin`. A [`Command::Join`] carrying
/// [`JoinKind::WaitForCompletion`] dispatches one push/pull per resolved seed and
/// parks the per-call state here. The QUIC coordinator services the dial in-band,
/// so each `start_join_push_pull`'s returned machine `StreamId` coerces directly
/// into the [`ExchangeId`] domain (via `From<StreamId>`) — the same value the
/// bridge-reap path stamps onto its [`Event::ExchangeCompleted`]. Contact
/// accounting is per-OUTBOUND-EXCHANGE, filtered to [`ExchangeKind::PushPull`].
///
/// Reply resolution and ignore-stream cleanup are SEPARATE terminal states: the
/// reply resolves on all-exchanges-done OR `deadline` (whichever first); the
/// ignore streams are cleared only once every dispatched exchange has completed
/// (`pending` empty), so a `StreamId` recorded for a still-live exchange stays in
/// the machine's ignore set and a late merge still suppresses the peer's pre-join
/// user events.
struct PendingJoin {
  /// Outbound exchange ids this waiter dispatched and is still awaiting a terminal
  /// `ExchangeCompleted` for.
  pending: HashSet<ExchangeId>,
  /// Peer addresses of the dispatched exchanges that terminated `Succeeded`.
  /// Duplicate seeds contribute one entry per successful exchange.
  contacted: SmallVec<[SocketAddr; 1]>,
  /// The `StreamId`s this join recorded in the machine's per-exchange ignore set
  /// (non-empty only for an `ignore_old` join). Cleared via
  /// `clear_ignore_join_stream` once every dispatched exchange has completed.
  ignore_streams: SmallVec<[StreamId; 1]>,
  /// Total outbound-exchange count this call dispatched — the `JoinAllFailed`
  /// denominator on a zero-contact resolution.
  requested: usize,
  /// Wall-clock instant past which the driver replies with whatever `contacted`
  /// set it has accumulated even if `pending` is non-empty.
  ///
  /// This is a driver-local FALLBACK: the join normally resolves the moment every
  /// dispatched exchange has surfaced its terminal `ExchangeCompleted` (success or
  /// the coordinator's own `stream_timeout` failure), so this deadline only bounds
  /// a hung exchange. Its reap runs behind the same shared-UDP-path quiescence gate
  /// as the reliable-exchange `handle_timeout` (see the module docs), so it never
  /// fires while a pre-deadline completion is still kernel-resident behind a
  /// non-saturating recv batch.
  deadline: Instant,
  /// One-shot reply channel back to the caller, taken when the reply resolves.
  /// `None` once resolved; the waiter then lingers — only to drive ignore-stream
  /// cleanup — until `pending` empties.
  reply: Option<oneshot::Sender<JoinReply>>,
}

impl PendingJoin {
  /// Resolve the caller's reply once, from the current `contacted` set. Idempotent:
  /// after the first call `reply` is `None` and this is a no-op, so the deadline
  /// path and the all-exchanges-done path never double-send.
  fn resolve_reply(&mut self) {
    if let Some(reply) = self.reply.take() {
      let result = if self.contacted.is_empty() {
        Err((
          SmallVec::new(),
          SerfError::JoinAllFailed(JoinFailed::new(self.requested, 0)),
        ))
      } else {
        Ok(self.contacted.clone())
      };
      // Ignoring Err: caller dropped the reply receiver (the join future was
      // cancelled).
      let _ = reply.send(result);
    }
  }

  /// This waiter has reached both terminal states — its reply resolved AND every
  /// dispatched exchange completed — so it can be removed and its ignore-stream
  /// cleanup run.
  fn is_done(&self) -> bool {
    self.reply.is_none() && self.pending.is_empty()
  }
}

/// Driver-side state for the single in-flight graceful-leave operation.
///
/// A [`Command::Leave`] that finds the endpoint `Alive` initiates the machine's
/// `leave()`, which withholds [`Event::LeftCluster`] until the leave notices have
/// drained. The pump parks this and replies only once that `LeftCluster` arrives
/// (success) or `deadline` elapses ([`SerfError::LeaveTimeout`]). Leave is SHARED:
/// a second `Command::Leave` racing an in-flight one joins it by pushing its reply
/// onto `repliers`.
struct PendingLeave {
  /// Reply channels of every `leave()` caller that joined this in-flight leave.
  repliers: Vec<oneshot::Sender<Result<()>>>,
  /// Wall-clock instant past which the pump replies [`SerfError::LeaveTimeout`] to
  /// every replier even if `LeftCluster` has not yet fired.
  deadline: Instant,
}

impl PendingLeave {
  /// Reply to every joined `leave()` caller with a fresh `Result<()>` from
  /// `make_result`. A constructor closure (rather than a cloned value) sidesteps
  /// `SerfError` not being `Clone` — every terminal outcome here (`Ok(())`,
  /// `LeaveTimeout`, `Shutdown`) is trivially reconstructible.
  fn resolve_all(self, mut make_result: impl FnMut() -> Result<()>) {
    for replier in self.repliers {
      // Ignoring Err: a `leave()` caller dropped its reply receiver.
      let _ = replier.send(make_result());
    }
  }
}

/// The single-owner QUIC driver future. Runs until shutdown (a `Shutdown` command,
/// a lost id-conflict `Event::Shutdown`, or the last handle dropped).
pub(crate) struct QuicDriver<I, R, G, SR>
where
  // Structurally required: `endpoint` names `QuicEndpoint<I, G, SR, ReactorDropCounter>`, whose struct
  // declares `I: Eq + Hash`.
  I: core::hash::Hash + Eq,
  R: Runtime,
{
  endpoint: QuicEndpoint<I, G, SR, ReactorDropCounter>,
  /// The shared UDP socket carrying QUIC packets AND plain-UDP gossip. `Option` so
  /// the shutdown branch can drop it (releasing the bound port) BEFORE acking;
  /// `Some` for the running lifetime, taken only during teardown.
  socket: Option<<R::Net as Net>::UdpSocket>,
  shared: Arc<Shared<I>>,
  /// Hand-off to the observation task (delegate dispatch + event-stream fan-out).
  obs_tx: Sender<Event<I, SocketAddr>>,
  /// Bytes of payload-bearing events queued in `obs_tx` — the byte backstop's
  /// counter (added on enqueue, subtracted by the obs task on dequeue).
  obs_payload_bytes: Arc<AtomicU64>,
  /// Queued-payload byte budget on a bounded obs channel, `None` if unbounded.
  obs_payload_budget: Option<u64>,
  /// Application-data events retained after a full obs channel, retried later.
  obs_overflow: VecDeque<Event<I, SocketAddr>>,
  /// Cluster label threaded into the gossip codec (outbound stamp + inbound
  /// verify).
  label: Option<Bytes>,
  /// Outstanding await-result join waiters.
  pending_joins: Vec<PendingJoin>,
  /// The in-flight graceful leave, resolved on `LeftCluster`.
  pending_leave: Option<PendingLeave>,
  /// Set once `leave()` has been initiated. Switches the gossip egress from
  /// best-effort (drop on non-completion) to retention for the leave fan-out,
  /// which has no next gossip round to re-send a dropped farewell — and, in
  /// `Datagram` mode, reroutes it onto the plain-UDP path, since a frame
  /// queued into quinn is emitted only when congestion control allows and so
  /// carries no socket-handoff signal.
  leave_initiated: bool,
  /// Leave-farewell datagrams retained after a non-completing plain-UDP send,
  /// retried FIFO ahead of fresh transmits and flushed before the socket drops.
  leave_drain: LeaveDrain,
  /// `LeftCluster` observed while retained farewells were still queued: the
  /// parked leave resolves the moment `leave_drain` empties, so `Ok` from
  /// `leave().await` always means the farewell reached the socket.
  left_cluster_seen: bool,
  /// Deadline bounding the teardown park that drains retained farewells.
  leave_drain_deadline: Option<Instant>,
  /// A leave-farewell send failed on the LOCAL socket (not a per-peer network
  /// signal): the parked leave resolves
  /// [`LeaveFarewellUndelivered`](SerfError::LeaveFarewellUndelivered) instead
  /// of a false `Ok`.
  leave_send_failed: bool,
  /// Parked `Shutdown` replies — acked only after the UDP socket drops, so a caller
  /// resuming from `shutdown().await` can rebind the same address. A `Vec` because
  /// several callers can race `shutdown()`.
  shutdown_reply: Vec<oneshot::Sender<Result<()>>>,
  recv_buf: Vec<u8>,
  /// Per-poll cap on each drained surface / recv batch.
  iter_drain_cap: usize,
  timer: Option<Pin<Box<R::Sleep>>>,
  timer_deadline: Option<Instant>,
  /// Wall-clock anchor for the shared-UDP-path staleness grace: the instant a due
  /// `handle_timeout` / reap first deferred purely because the recv batch was
  /// saturated (the kernel may still hold a pre-deadline completion). `None` when
  /// not deferring; the fire proceeds once [`SHARED_PATH_STALENESS_GRACE`] has
  /// elapsed since this anchor.
  timeout_stall_since: Option<Instant>,
  idle_wake: Duration,
  leave_timeout: Duration,
  /// Test-only: the number of upcoming recv-loop socket polls that must report a
  /// recv ERROR stop instead of reading the real socket. A bound UDP socket cannot
  /// be made to error on demand, so a pump test decrements this to drive the
  /// recv-error gate deterministically (an `Err` stop, then a real `Poll::Pending`
  /// quiescent stop).
  #[cfg(test)]
  recv_errors_remaining: usize,
  /// Test seam: when set, `poll_recv_once` reports a `Poll::Pending` (kernel-empty)
  /// quiescent stop WITHOUT reading the real socket. On Windows a UDP `recv_from`
  /// after sending to a closed port returns `ConnectionReset` (the ICMP
  /// port-unreachable), so a pump test needing a deterministic quiescent stop scripts
  /// it here rather than relying on the real socket returning `Pending`.
  #[cfg(test)]
  recv_force_pending: bool,
  /// The driver's keyring delegate: applies inbound key-management ops and produces
  /// the `respond_key` answer. Present only under an encryption backend.
  #[cfg(encryption)]
  keyring: Arc<dyn KeyringDelegate>,
}

impl<I, R, G, SR> QuicDriver<I, R, G, SR>
where
  I: memberlist_proto::Id + Clone,
  R: Runtime,
  G: rand::Rng,
  SR: rand::Rng + SeedableRng,
{
  /// Build the driver from the endpoint, its bound UDP socket, the shared state,
  /// the observation hand-off, and the recv-buffer inputs.
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn new(
    endpoint: QuicEndpoint<I, G, SR, ReactorDropCounter>,
    socket: <R::Net as Net>::UdpSocket,
    quic_max_udp_payload: u64,
    shared: Arc<Shared<I>>,
    obs_tx: Sender<Event<I, SocketAddr>>,
    obs_payload_bytes: Arc<AtomicU64>,
    obs_payload_budget: Option<u64>,
    driver_opts: RuntimeOptions,
    label: Option<Bytes>,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Self {
    let buf_len = recv_buf_len_for(endpoint.gossip_mtu(), quic_max_udp_payload);
    Self {
      endpoint,
      socket: Some(socket),
      shared,
      obs_tx,
      obs_payload_bytes,
      obs_payload_budget,
      obs_overflow: VecDeque::new(),
      label,
      pending_joins: Vec::new(),
      pending_leave: None,
      leave_initiated: false,
      leave_drain: LeaveDrain::new(),
      left_cluster_seen: false,
      leave_drain_deadline: None,
      leave_send_failed: false,
      shutdown_reply: Vec::new(),
      recv_buf: vec![0u8; buf_len.max(1)],
      iter_drain_cap: driver_opts.iter_drain_cap().max(1),
      timer: None,
      timer_deadline: None,
      timeout_stall_since: None,
      idle_wake: driver_opts.idle_wake_interval(),
      leave_timeout: driver_opts.leave_timeout(),
      #[cfg(test)]
      recv_errors_remaining: 0,
      #[cfg(test)]
      recv_force_pending: false,
      #[cfg(encryption)]
      keyring,
    }
  }

  /// Poll the shared UDP socket once for the recv loop, returning one datagram, a
  /// recv error, or `Poll::Pending` (the kernel-empty signal). A `#[cfg(test)]`
  /// hook can script an `Err` stop here so a pump test can drive the recv-error
  /// gate deterministically — a bound socket cannot be made to error on demand.
  fn poll_recv_once(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<(usize, SocketAddr)>> {
    #[cfg(test)]
    if self.recv_errors_remaining > 0 {
      self.recv_errors_remaining -= 1;
      return Poll::Ready(Err(std::io::Error::from(
        std::io::ErrorKind::ConnectionRefused,
      )));
    }
    #[cfg(test)]
    if self.recv_force_pending {
      return Poll::Pending;
    }
    let Some(socket) = self.socket.as_ref() else {
      return Poll::Pending;
    };
    socket.poll_recv_from(cx, &mut self.recv_buf)
  }

  /// Applies one handle command to the machine.
  fn dispatch(&mut self, cmd: Command<I, SocketAddr>, now: Instant) {
    let running = self.endpoint.state() == SerfState::Alive;
    match cmd {
      Command::Join(JoinCmd {
        seeds,
        kind,
        ignore_old,
        reply,
      }) => {
        // Gate on a running node: `leave()` stops the periodic schedulers, so a
        // join after leave would leave the node non-participating.
        if !running {
          // Ignoring Err: caller dropped the reply receiver.
          let _ = reply.send(Err((SmallVec::new(), SerfError::NotRunning)));
          return;
        }
        // Announce the serf-level join intent so peers learn the local join ltime
        // without waiting for the next anti-entropy round.
        if let Err(e) = self.endpoint.join() {
          // Ignoring Err: caller dropped the reply receiver.
          let _ = reply.send(Err((SmallVec::new(), SerfError::from(e))));
          return;
        }
        // The QUIC coordinator services the dial + flushes the outbound queue
        // in-band, so each `start_join_push_pull`'s returned machine `StreamId`
        // coerces directly into the `ExchangeId` the bridge-reap path stamps onto
        // its `ExchangeCompleted` — no inline action drain / capture is needed
        // (unlike the stream driver).
        match kind {
          JoinKind::Dispatch => {
            let mut dispatched: SmallVec<[SocketAddr; 1]> = SmallVec::new();
            for seed in seeds {
              // Ignoring StreamId: the Dispatch arm tracks no per-exchange waiter
              // state — completion / failure surfaces through `poll_event`.
              let _sid = self.endpoint.start_join_push_pull(seed, ignore_old, now);
              dispatched.push(seed);
            }
            // Ignoring Err: caller dropped the reply receiver.
            let _ = reply.send(Ok(dispatched));
          }
          JoinKind::WaitForCompletion(WaitForCompletionArgs { deadline }) => {
            let requested = seeds.len();
            let mut exchange_ids: HashSet<ExchangeId> = HashSet::with_capacity(requested);
            // An `ignore_old` join records every seed's `StreamId` in the machine;
            // the driver owns clearing any that fail to merge. A plain join records
            // nothing, so this stays empty.
            let mut ignore_streams: SmallVec<[StreamId; 1]> = SmallVec::new();
            for seed in seeds {
              let sid = self.endpoint.start_join_push_pull(seed, ignore_old, now);
              if ignore_old {
                ignore_streams.push(sid);
              }
              exchange_ids.insert(ExchangeId::from(sid));
            }
            if exchange_ids.is_empty() {
              // Every seed retired before an exchange (reachable only with a
              // zero-length `seeds`, which the handle never sends for an await
              // join): resolve now — parking would hang with no terminal incoming.
              for s in &ignore_streams {
                self.endpoint.clear_ignore_join_stream(*s);
              }
              // Ignoring Err: caller dropped the reply receiver.
              let _ = reply.send(Err((
                SmallVec::new(),
                SerfError::JoinAllFailed(JoinFailed::new(requested, 0)),
              )));
            } else {
              self.pending_joins.push(PendingJoin {
                pending: exchange_ids,
                contacted: SmallVec::new(),
                ignore_streams,
                requested,
                deadline,
                reply: Some(reply),
              });
            }
          }
        }
      }
      Command::Leave(LeaveCmd { reply }) => {
        // Leave is a SHARED in-flight operation. If one is in flight, JOIN it (do
        // not re-invoke `leave()`, a terminal no-op once `Leaving`/`Left` that
        // emits no second `LeftCluster`). Otherwise INITIATE: snapshot `Alive`
        // before the call, then park (was Alive) or reply immediately (no-op /
        // error).
        if let Some(pl) = self.pending_leave.as_mut() {
          pl.repliers.push(reply);
        } else {
          let was_alive = running;
          let leave_timeout = self.leave_timeout;
          let res: Result<()> = self.endpoint.leave(now).map_err(SerfError::from);
          match res {
            Ok(()) if was_alive => {
              // The leave mutated and fan-out is queued: switch the gossip egress
              // to retention so a backpressured farewell is retried, not dropped.
              self.leave_initiated = true;
              self.pending_leave = Some(PendingLeave {
                repliers: vec![reply],
                deadline: now + leave_timeout,
              });
            }
            other => {
              // Ignoring Err: caller dropped the reply receiver.
              let _ = reply.send(other);
            }
          }
        }
      }
      Command::ForceLeave(ForceLeaveCmd {
        id,
        prune,
        now: at,
        reply,
      }) => {
        let res = if running {
          self
            .endpoint
            .force_leave(id, prune, at)
            .map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      Command::UserEvent(cmd) => {
        let res = if running {
          let name = cmd.name().clone();
          let payload = cmd.payload().clone();
          self
            .endpoint
            .user_event(name, payload, cmd.coalesce, now)
            .map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = cmd.reply.send(res);
      }
      Command::Query(cmd) => {
        let res = if running {
          let name = cmd.name().clone();
          let payload = cmd.payload().clone();
          let QueryCmd {
            params, now: at, ..
          } = &cmd;
          self
            .endpoint
            .query(name, payload, params.clone(), *at)
            .map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = cmd.reply.send(res);
      }
      Command::Respond(cmd) => {
        let res = if running {
          let payload = cmd.payload().clone();
          self
            .endpoint
            .respond(&cmd.token, payload, cmd.now)
            .map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = cmd.reply.send(res);
      }
      Command::SetTags(SetTagsCmd { tags, reply }) => {
        let res = if running {
          self.endpoint.set_tags(tags, now).map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      #[cfg(encryption)]
      Command::InstallKey(KeyCmd {
        key,
        now: at,
        reply,
      }) => {
        let res = if running {
          self.endpoint.install_key(key, at).map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      #[cfg(encryption)]
      Command::UseKey(KeyCmd {
        key,
        now: at,
        reply,
      }) => {
        let res = if running {
          self.endpoint.use_key(key, at).map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      #[cfg(encryption)]
      Command::RemoveKey(KeyCmd {
        key,
        now: at,
        reply,
      }) => {
        let res = if running {
          self.endpoint.remove_key(key, at).map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      #[cfg(encryption)]
      Command::ListKeys(ListKeysCmd { now: at, reply }) => {
        let res = if running {
          self.endpoint.list_keys(at).map_err(SerfError::from)
        } else {
          Err(SerfError::NotRunning)
        };
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(res);
      }
      Command::Shutdown(ShutdownCmd { reply }) => {
        // Do NOT ack inline: the UDP socket is still bound. Flag shutdown and park
        // the reply; the teardown branch acks every parked caller only AFTER it
        // drops the socket, so an immediate rebind on the same address after
        // `shutdown().await` succeeds.
        self.shared.begin_shutdown();
        self.shutdown_reply.push(reply);
      }
    }
  }

  /// Repeatedly runs the ordered surface pass to a FIXED POINT. A later surface can
  /// create work for an earlier one within the same poll: answering an inbound
  /// `Event::KeyRequest` (`respond_key`) queues a directed gossip transmit after
  /// the transmit surfaces were already drained, and a fed inbound message can
  /// enqueue a fresh transmit + event. Repeating the ordered pass while any surface
  /// made progress converges (the machine emits finite output per already-buffered
  /// input and each egress surface is cap-bounded), so on return `more == false`
  /// genuinely means no ready machine work remains from this poll's input. A pass
  /// that hits a per-surface cap ends the fixed point with `more == true` (after
  /// completing the pass, so no terminal event is stranded): the caller self-wakes
  /// rather than uncapped-draining an egress a fast peer can refill.
  fn drain_surfaces(&mut self, cx: &mut Context<'_>) -> (bool, bool) {
    let mut worked = false;
    loop {
      let (pass_worked, pass_more) = self.drain_surfaces_pass(cx);
      worked |= pass_worked;
      if pass_more {
        return (worked, true);
      }
      if !pass_worked {
        return (worked, false);
      }
    }
  }

  /// One ordered surface pass — inbound-gossip-ingress → outbound-gossip → raw-QUIC
  /// egress → events. Returns `(worked, more)`: whether any surface produced work,
  /// and whether a capped egress surface hit its cap (with work left). The ingress
  /// decode and the event drain are UNCAPPED (drained to empty): the ingress
  /// decode-to-empty guarantees a datagram-carried Ack is applied before the inline
  /// timer, and the uncapped event drain guarantees a surfaced terminal
  /// (`ExchangeCompleted` / `LeftCluster`) is folded before the reaps — neither may
  /// sit behind a cap under a flood. Both are bounded per poll: the ingress by the
  /// coordinator's `mem_ingress` cap, the events by the (already cap-bounded) feeds
  /// plus an O(members) `handle_timeout` burst.
  fn drain_surfaces_pass(&mut self, cx: &mut Context<'_>) -> (bool, bool) {
    let budget = self.iter_drain_cap;
    let now = Instant::now();
    let mut worked = false;
    let mut more = false;

    // Inbound gossip: decrypt + strip-label + parse, inline on the pump. Drained to
    // EMPTY (bounded by the coordinator's `mem_ingress` cap) so a datagram-carried
    // probe Ack is decoded and applied through `handle_message` BEFORE the inline
    // `handle_timeout` can mark the peer suspect. A QUIC stream packet is NOT here
    // — `handle_udp` processes reliable stream data in-band; this surface carries
    // only the buffered gossip frames.
    let decode_opts = DecodeOptions::new(self.label.clone());
    let mut ingress = false;
    while let Some((from, raw)) = self.endpoint.poll_memberlist_ingress() {
      ingress = true;
      // Reverse the wire transform stack: with an encryption backend built in,
      // `decrypt_gossip` strips (and authenticates) the encryption wrapper; with
      // none the serf gossip plane carries no transforms so the raw bytes are the
      // plain label frame. A dropped datagram self-heals on the next gossip round.
      #[cfg(encryption)]
      let plain = match self.endpoint.decrypt_gossip(&raw) {
        Ok(p) => Bytes::from(p),
        Err(_) => continue,
      };
      #[cfg(not(encryption))]
      let plain = raw;
      let inner = match decode_incoming(plain, &decode_opts) {
        Ok(b) => b,
        Err(_) => continue,
      };
      let msgs = match parse_messages::<I, SocketAddr>(inner) {
        Ok(m) => m,
        Err(_) => continue,
      };
      for msg in msgs {
        self.endpoint.handle_message(from, msg, now);
      }
    }
    worked |= ingress;

    // Outbound gossip: encode (plain or compound) + encrypt, then route onto the
    // unreliable wire the endpoint is configured for — a QUIC datagram over the
    // peer's pooled (quinn-TLS-protected) connection in `Datagram` mode, or the
    // shared UDP socket in `Udp` mode. Popping the last transmit is the endpoint's
    // leave-completion fence (it emits `LeftCluster`), so the leave/shutdown
    // datagrams reach the wire before that fence fires.
    // Once `leave()` has been initiated the fan-out rides plain UDP in BOTH
    // unreliable modes (the Datagram arm reroutes it — see the match below) and
    // is RETAINED on non-completion instead of dropped. The retained-datagram
    // RETRY does NOT run here: this pass repeats inside `drain_surfaces`'
    // fixed point, and a retained farewell must absorb at most one ICMP-class
    // error per pump wake for the bounded retry to sample the socket's error
    // slot across DISTINCT wakes — the single retry site is the top of `poll`.
    // Periodic gossip stays best-effort.
    let encode_opts = EncodeOptions::new(self.label.clone());
    let unreliable = self.endpoint.unreliable_transport();
    let mut needs_flush = false;
    let mut sent = 0;
    while sent < budget {
      let Some(transmit) = self.endpoint.poll_memberlist_transmit() else {
        break;
      };
      sent += 1;
      let (peer, plain): (SocketAddr, Bytes) = match transmit {
        Transmit::Packet(pkt) => {
          let (to, msg) = pkt.into_parts();
          match encode_outgoing(&msg, &encode_opts) {
            Ok(b) => (to, b),
            Err(_) => {
              if self.leave_initiated {
                trace_leave_transform_error(to);
                self.leave_send_failed = true;
              }
              continue;
            }
          }
        }
        Transmit::Compound(cmp) => {
          let (to, msgs) = cmp.into_parts();
          match encode_outgoing_compound(&msgs, &encode_opts) {
            Ok(b) => (to, b),
            Err(_) => {
              if self.leave_initiated {
                trace_leave_transform_error(to);
                self.leave_send_failed = true;
              }
              continue;
            }
          }
        }
      };
      #[allow(unused_mut)]
      let mut on_wire: Vec<u8> = plain.to_vec();
      #[cfg(encryption)]
      {
        on_wire = match self.endpoint.encrypt_gossip(&on_wire) {
          Ok(bytes) => bytes,
          Err(_) => {
            if self.leave_initiated {
              trace_leave_transform_error(peer);
              self.leave_send_failed = true;
            }
            continue;
          }
        };
      }
      // `Bytes` so the datagram-queue path and the UDP fallback can share the
      // encoded (and, under an encryption backend, already-sealed) payload without
      // a second copy — `clone` is an O(1) refcount bump.
      let on_wire = Bytes::from(on_wire);
      match unreliable {
        UnreliableTransport::Udp => {
          // Best-effort for periodic gossip; retained for the leave fan-out.
          poll_send_gossip(
            &mut self.leave_drain,
            self.leave_initiated,
            self.socket.as_ref(),
            cx,
            peer,
            &on_wire,
            &mut self.leave_send_failed,
          );
        }
        // Once `leave()` has been initiated the fan-out takes the plain-UDP
        // path even in `Datagram` mode: a frame queued into quinn is emitted
        // only when congestion control and pacing allow, so an empty retention
        // queue would not prove the farewell reached the socket — and the
        // fan-out has no retry round to absorb that loss. The plain-UDP send
        // gives exact socket-handoff semantics (completed, retained, or
        // error-classified), and peers demux plain gossip datagrams in every
        // mode — the `NotReady`/`TooLarge` fallbacks below rely on exactly
        // that.
        UnreliableTransport::Datagram if self.leave_initiated => {
          poll_send_gossip(
            &mut self.leave_drain,
            true,
            self.socket.as_ref(),
            cx,
            peer,
            &on_wire,
            &mut self.leave_send_failed,
          );
        }
        UnreliableTransport::Datagram => {
          match self
            .endpoint
            .queue_unreliable_datagram(peer, on_wire.clone(), now)
          {
            // Accepted onto an established QUIC connection: flush it into
            // `poll_transmit` this pass (below) so a datagram-borne probe leaves on
            // the tick its timeout is armed.
            DatagramSendStatus::Queued => {
              needs_flush = true;
              self.shared.add_datagrams_sent(1);
            }
            // NotReady may mean the queue just initiated a cold dial: flush this
            // pass so the connection's Initial is emitted now (else it does not warm
            // until the next driver wake). The gossip itself still goes out
            // immediately over the plain-UDP fallback.
            DatagramSendStatus::NotReady => {
              needs_flush = true;
              poll_send_gossip(
                &mut self.leave_drain,
                self.leave_initiated,
                self.socket.as_ref(),
                cx,
                peer,
                &on_wire,
                &mut self.leave_send_failed,
              );
            }
            // TooLarge: the connection is already Established (max_size was Some), so
            // there is no pending Initial to flush; fall back to plain UDP.
            DatagramSendStatus::TooLarge => {
              poll_send_gossip(
                &mut self.leave_drain,
                self.leave_initiated,
                self.socket.as_ref(),
                cx,
                peer,
                &on_wire,
                &mut self.leave_send_failed,
              );
            }
          }
        }
      }
    }
    worked |= sent > 0;
    more |= sent == budget;

    // Flush any datagrams queued above into `poll_transmit` THIS pass so the
    // raw-QUIC loop below sends them now — a datagram-borne probe whose timeout is
    // armed this same tick must not wait for the next driver wake (that wake can be
    // the timeout).
    if needs_flush {
      self.endpoint.flush_outbound_transmits(now);
    }

    // Raw QUIC datagrams: already wire-framed by quinn-proto (handshake, acks,
    // reliable stream data, datagram-mode gossip), no codec wrap.
    let mut raw_sent = 0;
    while raw_sent < budget {
      let Some((dest, bytes)) = self.endpoint.poll_transmit() else {
        break;
      };
      raw_sent += 1;
      if let Some(socket) = self.socket.as_ref() {
        // Ignoring Poll: no raw QUIC packet ever carries the leave fan-out —
        // once `leave()` is initiated the gossip egress above routes it over
        // plain UDP with socket-handoff retention — so raw egress stays
        // best-effort: a dropped handshake/ACK/stream packet is recovered by
        // quinn's own loss detection, and a dropped datagram-mode gossip frame
        // is re-sent by the next periodic round.
        let _ = socket.poll_send_to(cx, &bytes, dest);
      }
    }
    worked |= raw_sent > 0;
    more |= raw_sent == budget;

    // Observation events: retry the overflow first, then drain to EMPTY (uncapped)
    // so a surfaced terminal folded by `send_observation` is never stranded behind
    // a per-poll cap under a flood.
    self.flush_obs_overflow();
    let mut events = false;
    while let Some(ev) = self.endpoint.poll_event() {
      events = true;
      self.send_observation(ev);
    }
    worked |= events;

    (worked, more)
  }

  /// Retries retained overflow events into the obs channel, stopping at the first
  /// `Full`.
  fn flush_obs_overflow(&mut self) {
    while let Some(ev) = self.obs_overflow.pop_front() {
      match self.obs_tx.try_send(ev) {
        Ok(()) => {}
        Err(TrySendError::Full(ev)) => {
          self.obs_overflow.push_front(ev);
          break;
        }
        Err(TrySendError::Disconnected(ev)) => {
          // The obs task is gone: reclaim this event's reserved payload bytes.
          if let Some(bytes) = observation_payload_bytes(&ev) {
            self.obs_payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
          }
        }
      }
    }
  }

  /// Hands one event to the obs task. Applies the synchronous protocol accounting
  /// first (join/leave/conflict/key). A full channel retains application data for
  /// retry (bounded by the payload byte budget and `OBS_OVERFLOW_MAX`) and drops
  /// recoverable membership/control events, counting them.
  fn send_observation(&mut self, ev: Event<I, SocketAddr>) {
    self.account_event(&ev);
    let payload = observation_payload_bytes(&ev);
    // Byte backstop: refuse a payload event if enqueuing it would push the queued
    // payload bytes over budget.
    if let (Some(budget), Some(bytes)) = (self.obs_payload_budget, payload)
      && self
        .obs_payload_bytes
        .load(Ordering::Relaxed)
        .saturating_add(bytes)
        > budget
    {
      self.shared.add_observation_dropped(1);
      return;
    }
    // Reserve the payload bytes before the event becomes visible to the obs task,
    // so its release (subtract on receive) can never run ahead of the reservation.
    if let Some(bytes) = payload {
      self.obs_payload_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    match self.obs_tx.try_send(ev) {
      Ok(()) => {}
      Err(TrySendError::Full(ev)) => match payload {
        // Application data the event stream cannot reconstruct: retain (still
        // reserved) for a retry.
        Some(_) if self.obs_overflow.len() < OBS_OVERFLOW_MAX => {
          self.obs_overflow.push_back(ev);
        }
        // Recoverable membership/control, or the overflow is full: drop, count, and
        // roll back any reservation.
        _ => {
          if let Some(bytes) = payload {
            self.obs_payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
          }
          self.shared.add_observation_dropped(1);
        }
      },
      // The obs task is gone: roll back the reservation.
      Err(TrySendError::Disconnected(_)) => {
        if let Some(bytes) = payload {
          self.obs_payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
      }
    }
  }

  /// Synchronous protocol accounting for a surfaced event: reduce the matching
  /// await-result join on a push/pull `ExchangeCompleted`, resolve a parked leave
  /// on `LeftCluster`, begin teardown on a lost id-conflict `Shutdown`, and answer
  /// an inbound key-management request.
  fn account_event(&mut self, ev: &Event<I, SocketAddr>) {
    if let Event::ExchangeCompleted(c) = ev
      && c.kind() == ExchangeKind::PushPull
    {
      let Self {
        endpoint,
        pending_joins,
        ..
      } = self;
      complete_join_exchange(
        endpoint,
        pending_joins,
        c.eid(),
        *c.peer(),
        matches!(c.outcome(), ExchangeStatus::Succeeded),
      );
    }
    if matches!(ev, Event::LeftCluster) {
      // Resolve the parked leave only once every retained farewell datagram has
      // been accepted by the socket: `Ok` from `leave().await` means the leave
      // notices reached the transport, not merely that the machine drained its
      // fan-out. With retained datagrams still queued, remember the fence and
      // resolve when the drain empties (bounded by the caller's leave timeout).
      if self.leave_drain.is_empty() {
        if let Some(pl) = self.pending_leave.take() {
          let failed = self.leave_send_failed;
          pl.resolve_all(|| leave_outcome(failed));
        }
      } else {
        self.left_cluster_seen = true;
      }
    }
    // A lost id-conflict vote means the local node MUST stop, exactly as for a
    // `Command::Shutdown`. Flag shutdown; the pump self-wakes into the teardown
    // branch. The event still reaches subscribers through the obs hand-off.
    if matches!(ev, Event::Shutdown) {
      self.shared.begin_shutdown();
    }
    #[cfg(encryption)]
    if let Event::KeyRequest(req) = ev {
      let resp = self.apply_key_request_live(req);
      // Ignoring Err: `respond_key` fails only when the response cannot be routed;
      // the key op has already applied to the live wire keyring.
      let _ = self.endpoint.respond_key(req, resp, Instant::now());
    }
  }

  /// Read-modify-write the endpoint's LIVE wire keyring for one inbound
  /// [`KeyRequest`], returning the [`KeyResponseArgs`] built from the post-op live
  /// state.
  ///
  /// The endpoint-facing wrapper over [`serf_driver::apply_key_request`]: it reads
  /// the coordinator's live `encryption_options`, applies the op variant-exactly
  /// against the live ring, and on a real mutation publishes the rotated ring back
  /// via `set_encryption_options` — re-keying the gossip datagram plane (the QUIC
  /// reliable path always skips, quinn encrypts the stream) — then notifies the
  /// keyring observer for persistence. A node with no keyring configured answers
  /// `result = false` and makes no wire change; a read-only `list` or a refused op
  /// leaves the wire untouched.
  #[cfg(encryption)]
  fn apply_key_request_live(&mut self, req: &KeyRequest<I, SocketAddr>) -> KeyResponseArgs {
    let mut encryption = self.endpoint.encryption_options().clone();
    let Some(current) = encryption.keyring() else {
      return KeyResponseArgs {
        result: false,
        message: "no keyring configured on this node".into(),
        ..Default::default()
      };
    };
    let (resp, rotated) = serf_driver::apply_key_request(current, req.op(), req.key()).into_parts();
    if let Some(new_ring) = rotated {
      encryption.set_keyring(new_ring.clone());
      self.endpoint.set_encryption_options(encryption);
      self.keyring.keyring_updated(&new_ring);
    }
    resp
  }

  /// Reap await-result join waiters on the deadline timer (the reply terminal),
  /// then remove any waiter that has reached BOTH terminals (reply resolved AND
  /// `pending` empty), clearing its still-recorded ignore-join streams.
  fn reap_pending_joins(&mut self, now: Instant) {
    let Self {
      endpoint,
      pending_joins,
      ..
    } = self;
    let mut i = 0;
    while i < pending_joins.len() {
      if now >= pending_joins[i].deadline {
        pending_joins[i].resolve_reply();
      }
      if pending_joins[i].is_done() {
        let pj = pending_joins.swap_remove(i);
        for s in &pj.ignore_streams {
          endpoint.clear_ignore_join_stream(*s);
        }
      } else {
        i += 1;
      }
    }
  }

  /// Reap a deadline-expired graceful-leave waiter.
  fn reap_pending_leave(&mut self, now: Instant) {
    if let Some(pl) = self.pending_leave.as_ref()
      && now >= pl.deadline
    {
      let pl = self.pending_leave.take().expect("checked Some above");
      pl.resolve_all(|| Err(SerfError::LeaveTimeout));
    }
  }

  /// Earliest pending-join deadline of a still-unreplied waiter, folded into the
  /// per-poll timer target so it fires by the first expiring join.
  fn min_pending_join_deadline(&self) -> Option<Instant> {
    self
      .pending_joins
      .iter()
      .filter(|pj| pj.reply.is_some())
      .map(|pj| pj.deadline)
      .min()
  }

  /// Earliest pending-leave deadline, folded into the per-poll timer target.
  fn min_pending_leave_deadline(&self) -> Option<Instant> {
    self.pending_leave.as_ref().map(|pl| pl.deadline)
  }

  /// Publish a fresh [`SerfSnapshot`] of the endpoint's observable membership.
  /// Skips the publish when the local node is not yet present in the membership
  /// store (the local `NodeJoined` sieve has not fired), so `SerfSnapshot::new`
  /// (which requires the local node) is never called with it absent.
  fn refresh_snapshot(&self) {
    let members = self.endpoint.members_snapshot();
    let local_id = self.endpoint.local_id();
    if !members.iter().any(|m| m.node().id_ref() == local_id) {
      return;
    }
    let snap = SerfSnapshot::new(
      members,
      local_id,
      self.endpoint.state(),
      LamportTime::from(self.endpoint.member_time()),
      LamportTime::from(self.endpoint.event_time()),
      LamportTime::from(self.endpoint.query_time()),
    );
    self.shared.publish(snap);
  }

  /// (Re)arms the wakeup timer for `target` if it is not already armed for it.
  fn arm_timer(&mut self, target: Instant, now: Instant) {
    if self.timer_deadline != Some(target) {
      self.timer = Some(Box::pin(R::sleep(target.saturating_duration_since(now))));
      self.timer_deadline = Some(target);
    }
  }

  /// Arms the wakeup timer for `target` and polls it once so its waker is registered
  /// and the pump re-polls when it fires. Returns whether it fired already (an
  /// already-elapsed target), in which case the caller self-wakes via `more`. Parking
  /// on the returned `false` re-polls ONLY on the armed timer — no self-wake — which
  /// is how a recv-error backoff avoids a busy-spin.
  fn arm_and_poll_timer(&mut self, target: Instant, now: Instant, cx: &mut Context<'_>) -> bool {
    self.arm_timer(target, now);
    if let Some(timer) = self.timer.as_mut()
      && timer.as_mut().poll(cx).is_ready()
    {
      self.timer = None;
      self.timer_deadline = None;
      return true;
    }
    false
  }

  /// Reply `Err(Shutdown)` to a command drained during teardown.
  fn reply_shutdown(cmd: Command<I, SocketAddr>) {
    match cmd {
      Command::Join(JoinCmd { reply, .. }) => {
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(Err((SmallVec::new(), SerfError::Shutdown)));
      }
      Command::Leave(LeaveCmd { reply }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      Command::ForceLeave(ForceLeaveCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      Command::UserEvent(cmd) => {
        let _ = cmd.reply.send(Err(SerfError::Shutdown));
      }
      Command::Query(QueryCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      Command::Respond(RespondCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      Command::SetTags(SetTagsCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      #[cfg(encryption)]
      Command::InstallKey(KeyCmd { reply, .. })
      | Command::UseKey(KeyCmd { reply, .. })
      | Command::RemoveKey(KeyCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      #[cfg(encryption)]
      Command::ListKeys(ListKeysCmd { reply, .. }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
      Command::Shutdown(ShutdownCmd { reply }) => {
        let _ = reply.send(Err(SerfError::Shutdown));
      }
    }
  }
}

impl<I, R, G, SR> Future for QuicDriver<I, R, G, SR>
where
  I: memberlist_proto::Id + Clone + Send + Sync + Unpin + 'static,
  R: Runtime,
  G: rand::Rng + Unpin,
  SR: rand::Rng + SeedableRng + Unpin,
  QuicEndpoint<I, G, SR, ReactorDropCounter>: Unpin,
{
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let this = self.get_mut();
    let now = Instant::now();
    let mut progress = false;
    let mut more = false;

    // Drain queued commands (parks the waker for the next push).
    for cmd in this.shared.drain_commands(cx.waker()) {
      this.dispatch(cmd, now);
      progress = true;
    }

    // Retained leave-farewell retry: exactly ONCE per poll, before any fresh
    // sends (normal egress or teardown drain). A retained datagram must absorb
    // at most one ICMP-class error per pump wake, so the bounded retry samples
    // the socket's error slot across DISTINCT wakes — running it inside
    // `drain_surfaces`' fixed point (or again at teardown) could exhaust the
    // whole allowance within a single wake against one stale asynchronous
    // error. A datagram retained by THIS poll's fresh sends waits for the next
    // wake (the leave/teardown deadline timers are always armed while any
    // farewell is outstanding).
    if this.leave_initiated {
      retry_retained_leave(
        &mut this.leave_drain,
        this.socket.as_ref(),
        cx,
        &mut this.leave_send_failed,
      );
      // A `LeftCluster` observed while these datagrams were still queued
      // deferred the parked leave; resolve it now that the socket has accepted
      // every retained farewell.
      if this.left_cluster_seen && this.leave_drain.is_empty() {
        this.left_cluster_seen = false;
        if let Some(pl) = this.pending_leave.take() {
          let failed = this.leave_send_failed;
          pl.resolve_all(|| leave_outcome(failed));
        }
      }
    }

    // Shutdown: flush to quiescence (an explicit leave's `Dead`-self notices
    // must reach the wire before the socket drops), fail every parked waiter
    // and queued command, release the bound port, then ack. No implicit leave:
    // a shutdown without an explicit `leave()` is abrupt by design (mirroring
    // the reference implementation's Shutdown), so peers detect the departure
    // as a failure; an explicit leave already queued its fan-out when its
    // command was dispatched. The completion latch promises the bind address is
    // free, not that every QUIC connection has closed.
    if this.shared.is_shutdown() {
      // Drain endpoint surfaces to quiescence before reaping: a single
      // `drain_surfaces` pass is egress-capped, so a large batch of already-queued
      // `ExchangeCompleted` events would be partially skipped, leaving contacted
      // addresses unaccounted in the `Err` tuple. `account_event` folds every
      // terminal completion into the matching pending join as it drains.
      loop {
        let (_, drain_more) = this.drain_surfaces(cx);
        if !drain_more {
          break;
        }
      }
      // Close the command queue and fail any still-queued commands.
      for cmd in this.shared.close_and_drain() {
        if let Command::Shutdown(ShutdownCmd { reply }) = cmd {
          // A straggler `Shutdown`: park it too, so it is acked after the socket
          // drops like every other caller.
          this.shutdown_reply.push(reply);
        } else {
          Self::reply_shutdown(cmd);
        }
      }
      for mut pj in this.pending_joins.drain(..) {
        for s in &pj.ignore_streams {
          this.endpoint.clear_ignore_join_stream(*s);
        }
        if let Some(reply) = pj.reply.take() {
          // Ignoring Err: the join caller dropped its reply receiver. Carry the
          // addresses reached before shutdown raced this waiter.
          let _ = reply.send(Err((
            std::mem::take(&mut pj.contacted),
            SerfError::Shutdown,
          )));
        }
      }
      // Retained leave-farewell datagrams must reach the wire before the
      // socket drops. They are retried by the per-poll retry at the top of
      // `poll` (never here — a second same-poll retry could exhaust a
      // datagram's whole ICMP allowance in one wake); a residue parks the
      // teardown — bounded by a short deadline — instead of being dropped: the
      // `Pending` send has the writable waker registered while the deadline
      // timer keeps a dead socket from hanging shutdown.
      //
      // The caller's per-leave deadline governs resolution during teardown
      // exactly as in the normal poll's reap: a leave that missed its
      // configured window resolves `LeaveTimeout` even mid-shutdown (a zero
      // timeout is a loud immediate `LeaveTimeout` by contract). A leave
      // still parked past this point has a strictly-future deadline.
      this.reap_pending_leave(Instant::now());
      if this.leave_drain.is_empty() {
        // A leave racing this shutdown resolves HERE, on delivery: the
        // machine's `LeftCluster` is propagate-delay-fenced behind a
        // `handle_timeout` a tearing-down pump never runs, and the fan-out has
        // verifiably been handed to the transport (surfaces quiescent, nothing
        // retained). A residue instead resolves `Err(Shutdown)` below — the
        // farewell did not fully leave this host.
        if let Some(pl) = this.pending_leave.take() {
          let failed = this.leave_send_failed;
          pl.resolve_all(|| leave_outcome(failed));
        }
      } else {
        let now = Instant::now();
        let drain_deadline = *this
          .leave_drain_deadline
          .get_or_insert(now + LEAVE_DRAIN_TEARDOWN_BOUND);
        if now < drain_deadline {
          // Park until whichever fires first: the drain bound, or a
          // still-parked leave's deadline — whose firing must resolve
          // `LeaveTimeout` promptly, while the drain keeps the rest of its
          // window. A `Ready` timer re-enters the phase so the reap and the
          // park recompute against the new now.
          let park_until = this
            .pending_leave
            .as_ref()
            .map_or(drain_deadline, |pl| pl.deadline.min(drain_deadline));
          this.arm_timer(park_until, now);
          if let Some(timer) = this.timer.as_mut()
            && timer.as_mut().poll(cx).is_pending()
          {
            return Poll::Pending;
          }
          cx.waker().wake_by_ref();
          return Poll::Pending;
        }
        trace_leave_drain_residue(this.leave_drain.len());
        if let Some(pl) = this.pending_leave.take() {
          pl.resolve_all(|| Err(SerfError::Shutdown));
        }
      }
      // Release the bound port BEFORE acking: dropping the agnostic UDP socket
      // closes its FD synchronously, so a caller resuming from `shutdown().await`
      // can immediately rebind the same address.
      drop(this.socket.take());
      for reply in this.shutdown_reply.drain(..) {
        // Ignoring Err: the caller dropped its reply receiver.
        let _ = reply.send(Ok(()));
      }
      this.shared.mark_shutdown_complete();
      return Poll::Ready(());
    }

    // Receive QUIC/gossip (bounded; a full batch means the kernel may hold more).
    // The socket is always `Some` here — the shutdown branch above (which takes it)
    // returned before reaching this point. Only `Poll::Pending` from the socket IS
    // the kernel-empty signal, and — because `handle_udp` processes a QUIC stream
    // packet in-band — a `Poll::Pending` stop proves every completing reliable
    // packet ready this poll was fed to the machine. A recv-ERROR stop is NOT
    // kernel-empty (the kernel may still hold a completing packet behind the
    // error), so it is tracked separately and gates the reaps like a saturated
    // batch.
    let mut recv_n = 0;
    let mut recv_errored = false;
    while recv_n < this.iter_drain_cap {
      match this.poll_recv_once(cx) {
        Poll::Ready(Ok((n, src))) => {
          this.endpoint.handle_udp(src, &this.recv_buf[..n], now);
          recv_n += 1;
        }
        // Ignoring Err: a transient recv error is non-fatal (the datagram is
        // dropped, re-armed next poll) — but it is NOT quiescence, so record it for
        // the timer/reap gate below.
        Poll::Ready(Err(_)) => {
          recv_errored = true;
          break;
        }
        Poll::Pending => break,
      }
    }
    if recv_n > 0 {
      progress = true;
    }
    // Either non-quiescent stop means the kernel may still hold datagrams — possibly
    // a reliable-exchange completion or a probe Ack: a saturated batch, or a recv
    // error behind which a completing packet may sit. This is the ONLY backlog
    // signal (the ingress decode drains to empty), and it gates the shared
    // `handle_timeout` site below.
    let recv_capped = recv_n == this.iter_drain_cap;
    let recv_quiescent = !recv_capped && !recv_errored;
    // WAKE and GATE are separate concerns. Both a saturated batch and a recv error
    // are non-quiescent for the reap gate above (`recv_quiescent`), but their WAKE
    // policy differs: a saturated batch is bounded backlog to drain next poll, so it
    // self-wakes; a recv ERROR is not bounded progress — a persistent one would
    // self-wake every poll and burn a core, and the socket registered no readiness
    // waker on its `Ready(Err)` — so it does NOT self-wake here. The deadline gate
    // below instead arms a bounded `RECV_ERROR_BACKOFF` timer for it and parks.
    more |= recv_capped;

    // Drain machine surfaces (gossip/QUIC egress capped, ingress + events uncapped).
    let (drained, drain_more) = this.drain_surfaces(cx);
    progress |= drained;
    more |= drain_more;
    // A conflict `Event::Shutdown` observed during the drain flips the shutdown
    // latch; self-wake so the next poll enters the teardown branch.
    if this.shared.is_shutdown() {
      more = true;
    }

    // Timer + deadline reaps under the shared-UDP-path quiescence gate. On QUIC
    // every resolving input (a reliable push/pull completion, a probe Ack) rides
    // the one UDP recv, so — with no disjoint FIFO to watermark — the non-premature
    // gate is recv QUIESCENCE: a due `handle_timeout` / join / leave reap fires only
    // when the recv loop stopped on `Poll::Pending` (`recv_quiescent`, so every
    // kernel-ready completing packet was fed through `handle_udp` and its terminal
    // folded by `drain_surfaces` above), OR — under a flood that saturates the batch
    // (or a persistent recv error) every poll — after a bounded staleness grace, for
    // liveness. A saturated batch AND a recv-error stop are both backlog-uncertain:
    // neither proves the kernel is empty, so both defer the reaps.
    let endpoint_deadline = this
      .endpoint
      .poll_timeout()
      .map(|d| d.min(now + this.idle_wake))
      .unwrap_or(now + this.idle_wake);
    let reap_deadline = [
      this.min_pending_join_deadline(),
      this.min_pending_leave_deadline(),
    ]
    .into_iter()
    .flatten()
    .min();
    // `endpoint_deadline` folds `idle_wake`, so `ep_due` is exactly "the coordinator
    // has an elapsed deadline"; a bare idle wake is not due and takes the idle arm.
    let ep_due = endpoint_deadline <= now;
    let reap_due = reap_deadline.is_some_and(|d| d <= now);

    if ep_due || reap_due {
      // Anchor the staleness grace the first poll the fire is held back purely by a
      // non-quiescent recv stop — a saturated batch OR a recv error, either of which
      // may hide a pre-deadline completion still in the kernel.
      if !recv_quiescent {
        this.timeout_stall_since.get_or_insert(now);
      }
      let grace_ok = recv_quiescent
        || this
          .timeout_stall_since
          .is_some_and(|t| now.saturating_duration_since(t) >= SHARED_PATH_STALENESS_GRACE);
      if grace_ok {
        // The recv stopped on `Poll::Pending` (every kernel-ready completing packet
        // was folded by `drain_surfaces` above) or the grace elapsed. Fire the
        // coordinator's elapsed deadlines, fold the UNCAPPED terminal events they
        // emit (`LeftCluster` / `ExchangeCompleted`) BEFORE the reaps — a same-poll
        // leave whose `LeftCluster` went unfolded would otherwise reap a false
        // `LeaveTimeout` — then reap the deadline residue.
        if ep_due {
          this.endpoint.handle_timeout(now);
        }
        while let Some(ev) = this.endpoint.poll_event() {
          this.send_observation(ev);
        }
        this.reap_pending_joins(now);
        this.reap_pending_leave(now);
        this.timeout_stall_since = None;
        progress = true;
        more = true;
      } else if recv_capped {
        // A deadline is due but the recv stop was a SATURATED BATCH (real backlog) and
        // the grace has not elapsed: DEFER by self-waking so the next poll drains the
        // recv toward a `Poll::Pending` quiescent stop. No timer is armed — the
        // deadline already elapsed, so the `more` self-wake alone re-polls (no lost
        // wakeup), and the wall-clock grace bounds the deferral.
        more = true;
      } else {
        // A deadline is due but the recv stop was a recv ERROR (the only remaining
        // non-quiescent case) and the grace has not elapsed: DEFER. There is nothing
        // bounded to drain, so an immediate self-wake would busy-spin the whole grace;
        // instead park on a timer at the grace expiry, when the force-fire (grace
        // elapsed) re-polls the recv. The grace still bounds the hold-back, so a
        // persistent error cannot starve the timer. `timeout_stall_since` is `Some`
        // here (anchored just above for this non-quiescent stop).
        let grace_deadline = this
          .timeout_stall_since
          .map_or(now, |t| t + SHARED_PATH_STALENESS_GRACE);
        if this.arm_and_poll_timer(grace_deadline, now, cx) {
          more = true;
        }
      }
    } else {
      // Idle: nothing due. Clear the stall anchor, then arm + poll the sleep for the
      // next deadline; NO self-wake (an armed sleep or socket readiness re-polls). A
      // recv ERROR folds a bounded `RECV_ERROR_BACKOFF` into the target so the errored
      // recv is retried within the backoff even when the next deadline is far off (the
      // idle wake is 60s) — a `Ready(Err)` socket registered no readiness waker, so
      // only this timer re-polls.
      this.timeout_stall_since = None;
      let mut target = reap_deadline.map_or(endpoint_deadline, |d| d.min(endpoint_deadline));
      if recv_errored {
        target = target.min(now + RECV_ERROR_BACKOFF);
      }
      if this.arm_and_poll_timer(target, now, cx) {
        more = true;
      }
    }

    // Republish the snapshot whenever the pump made progress (the serf endpoint
    // exposes no cheap version stamp, so — as in serf-compio — a productive poll
    // rebuilds and republishes the observable membership).
    if progress {
      this.refresh_snapshot();
    }

    // Yield to other tasks, but re-poll promptly while work remains.
    if more {
      cx.waker().wake_by_ref();
    }
    Poll::Pending
  }
}

/// Apply one terminal `ExchangeCompleted` to its await-result join waiter (if any):
/// remove `eid` from `pending`, push the peer into `contacted` on success, resolve
/// the caller's reply the instant `pending` empties (ahead of the obs hand-off, so
/// a slow delegate cannot delay it), and once fully done clear the still-recorded
/// ignore-join streams and reap the waiter.
fn complete_join_exchange<I, G, SR>(
  endpoint: &mut QuicEndpoint<I, G, SR, ReactorDropCounter>,
  pending_joins: &mut Vec<PendingJoin>,
  eid: ExchangeId,
  peer: SocketAddr,
  succeeded: bool,
) where
  I: memberlist_proto::Id + Clone,
  G: rand::Rng,
  SR: rand::Rng + SeedableRng,
{
  let Some(idx) = pending_joins
    .iter()
    .position(|pj| pj.pending.contains(&eid))
  else {
    return;
  };
  let pj = &mut pending_joins[idx];
  pj.pending.remove(&eid);
  if succeeded {
    pj.contacted.push(peer);
  }
  // Resolve the reply the moment every dispatched exchange has terminated. If the
  // deadline already replied, `reply` is `None` and this is a no-op.
  if pj.pending.is_empty() {
    pj.resolve_reply();
  }
  if pending_joins[idx].is_done() {
    let pj = pending_joins.swap_remove(idx);
    for s in &pj.ignore_streams {
      endpoint.clear_ignore_join_stream(*s);
    }
  }
}

/// The observation task: drains machine events off the pump, invokes the
/// [`Delegate`] hooks, and forwards every serf event to the
/// [`EventStream`](crate::EventStream). The forward is best-effort (a full queue
/// drops + counts, never blocks).
async fn observation_task<I, D>(
  obs_rx: Receiver<Event<I, SocketAddr>>,
  delegate: D,
  events_tx: Sender<Event<I, SocketAddr>>,
  shared: Arc<Shared<I>>,
  obs_payload_bytes: Arc<AtomicU64>,
) where
  I: Clone + Send + Sync + 'static,
  D: Delegate<Id = I, Address = SocketAddr>,
{
  use futures_util::FutureExt;
  use std::panic::AssertUnwindSafe;
  while let Ok(ev) = obs_rx.recv_async().await {
    // Reclaim the byte-backstop budget this event occupied, before the (possibly
    // slow) delegate hook, so the pump's enqueue side sees it promptly.
    let payload = observation_payload_bytes(&ev);
    if let Some(bytes) = payload {
      obs_payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
    // Contain a panicking delegate hook so the task survives and keeps releasing the
    // reservations of still-queued events. Ignoring the unwind result: the panic is
    // contained and the event is still forwarded to subscribers below.
    let _ = AssertUnwindSafe(dispatch_event_delegate(&delegate, &ev))
      .catch_unwind()
      .await;
    if events_tx
      .try_send(ev)
      .is_err_and(|e| matches!(e, TrySendError::Full(_)))
    {
      shared.add_events_dropped(1);
    }
  }
}

/// Set up the QUIC driver: arm the periodic schedulers, spawn the observation task,
/// then build the [`QuicDriver`] future. The caller (`Transport::run`) awaits it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_quic_driver<I, R, G, SR, D>(
  mut endpoint: QuicEndpoint<I, G, SR, ReactorDropCounter>,
  socket: <R::Net as Net>::UdpSocket,
  quic_max_udp_payload: u64,
  shared: Arc<Shared<I>>,
  events_tx: Sender<Event<I, SocketAddr>>,
  delegate: D,
  driver_opts: RuntimeOptions,
  label: Option<Bytes>,
  #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
) -> QuicDriver<I, R, G, SR>
where
  I: memberlist_proto::Id + Clone + Send + Sync + Unpin + 'static,
  R: Runtime,
  G: rand::Rng,
  SR: rand::Rng + SeedableRng,
  D: Delegate<Id = I, Address = SocketAddr>,
{
  // Arm the periodic probe / gossip / push-pull schedulers. Without this the
  // coordinator's schedulers stay unset, so failure detection, dissemination, and
  // anti-entropy never run.
  endpoint.start_scheduling(Instant::now());

  // Observation byte backstop: bound the queued payload bytes a slow delegate can
  // pin (the obs-channel count cap alone does not).
  let obs_payload_bytes = Arc::new(AtomicU64::new(0));
  let obs_payload_budget: Option<u64> = match driver_opts.observation_channel() {
    Channel::Bounded(_) => Some((endpoint.max_stream_frame_size() as u64).saturating_mul(4)),
    Channel::Unbounded => None,
  };
  let (obs_tx, obs_rx) = match driver_opts.observation_channel() {
    Channel::Bounded(n) => flume::bounded(n),
    Channel::Unbounded => flume::unbounded(),
  };
  R::spawn_detach(observation_task::<I, D>(
    obs_rx,
    delegate,
    events_tx,
    shared.clone(),
    obs_payload_bytes.clone(),
  ));

  QuicDriver::<I, R, G, SR>::new(
    endpoint,
    socket,
    quic_max_udp_payload,
    shared,
    obs_tx,
    obs_payload_bytes,
    obs_payload_budget,
    driver_opts,
    label,
    #[cfg(encryption)]
    keyring,
  )
}

#[cfg(test)]
mod tests;
