//! Stream-plane driver pump — single owner of the serf `StreamEndpoint`, the
//! UDP gossip socket, the TCP reliable listener, and the per-bridge handle
//! table.
//!
//! Runs a `select_biased` loop over its arms in priority order: gossip UDP
//! recv, the coordinator-supplied wake timer, listener accept, the command
//! channel, outbound-dial completion, and per-bridge inbound bytes. After each
//! fired arm the pump drains every outbound surface (`poll_action`,
//! `poll_transport_transmit`, `poll_memberlist_transmit`, `poll_event`) until
//! no method makes progress, republishes a fresh [`SerfSnapshot`] when state
//! changed, and re-enters the select. The pump owns the `StreamEndpoint`
//! outright; user-facing handles communicate exclusively via the command
//! channel and read state through the lock-free snapshot. The listener and
//! gossip socket are explicitly closed (awaited) when the loop exits so the
//! bound ports are released before shutdown returns.

use std::{
  cell::Cell,
  collections::{HashMap, HashSet},
  io,
  net::SocketAddr,
  rc::Rc,
};

use core::{
  task::{Context, Poll, Waker},
  time::Duration,
};

use bytes::Bytes;
use compio::{
  buf::BufResult,
  net::{TcpListener, TcpStream, UdpSocket},
};
use flume::{Receiver, Sender};
use futures_util::{FutureExt, future::FusedFuture, pin_mut, select_biased};
use lochan::mpsc;
use memberlist_proto::{
  Instant, SeedableRng, StreamId, Transmit,
  codec::{
    DecodeOptions, EncodeOptions, decode_incoming, encode_outgoing, encode_outgoing_compound,
    parse_messages,
  },
  streams::{StreamAction, StreamTransport},
};
use serf_proto::{
  ExchangeKind, ExchangeStatus, LamportTime, StreamEndpoint, event::Event, members::SerfState,
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
    options::{RuntimeOptions, StreamTransportOptions},
    shared::{
      ExchangeId, add_obs_payload, dispatch_event_delegate, drain_past_due_udp,
      observation_payload_bytes, yield_once,
    },
  },
  error::{JoinFailed, Result, SerfError},
  snapshot::{SerfSnapshot, SnapshotCell},
};
#[cfg(encryption)]
use serf_proto::{KeyResponseArgs, event::KeyRequest};

/// Driver-side state for one outstanding await-result join call.
///
/// A [`Command::Join`] carrying [`JoinKind::WaitForCompletion`] dispatches one
/// push/pull per resolved seed and parks the per-call state here. Contact
/// accounting is strictly per-OUTBOUND-EXCHANGE, observed via the machine's
/// [`Event::ExchangeCompleted`] filtered to [`ExchangeKind::PushPull`]: each
/// `start_push_pull` allocates a fresh [`ExchangeId`]; the driver tracks every
/// dispatched id in `pending` and pushes the peer address into `contacted` for
/// each that terminates [`ExchangeStatus::Succeeded`]. Tracking by `ExchangeId`
/// (not by `SocketAddr`) keeps duplicate-seed semantics: passing the same
/// address twice yields two exchanges counted independently.
///
/// Reply resolution and ignore-stream cleanup are SEPARATE terminal states. The
/// caller's reply resolves on all-exchanges-done OR `deadline` (whichever first),
/// consuming `reply`. The ignore-stream cleanup must wait until every dispatched
/// exchange has completed — i.e. `pending` is empty — because a `StreamId`
/// recorded for a still-live exchange must stay in the machine's ignore set so a
/// late merge still suppresses the peer's pre-join user events. The waiter is
/// removed from `joins` only once BOTH terminals are reached (reply sent and
/// `pending` empty); on a deadline that fires with exchanges still live it
/// replies and LINGERS, holding its `ignore_streams` until they complete.
struct PendingJoin {
  /// Outbound exchange ids this waiter dispatched and is still awaiting a
  /// terminal `ExchangeCompleted` for. An id is removed when its completion
  /// arrives (success or failure); when this set empties the ignore-stream
  /// cleanup runs and the waiter is reaped.
  pending: HashSet<ExchangeId>,
  /// Peer addresses of the dispatched exchanges that terminated `Succeeded`.
  /// Duplicate seeds contribute one entry per successful exchange.
  contacted: SmallVec<[SocketAddr; 1]>,
  /// The `StreamId`s this join recorded in the machine's per-exchange ignore set
  /// (non-empty only for an `ignore_old` join). Each is consumed by its own merge
  /// on the success path; any that did NOT merge (dial failure / timeout / empty
  /// body) are cleared via `clear_ignore_join_stream` only once every dispatched
  /// exchange has completed (`pending` empty), so a failed `ignore_old` exchange
  /// never leaks its `StreamId` and a still-live one is never prematurely cleared.
  ignore_streams: SmallVec<[StreamId; 1]>,
  /// Total outbound-exchange count this call dispatched — the `JoinAllFailed`
  /// denominator on a zero-contact resolution.
  requested: usize,
  /// Wall-clock instant past which the driver replies with whatever `contacted`
  /// set it has accumulated even if `pending` is non-empty.
  deadline: Instant,
  /// One-shot reply channel back to the caller, taken when the reply resolves
  /// (all-exchanges-done or `deadline`). `None` once resolved; the waiter then
  /// lingers — only to drive ignore-stream cleanup — until `pending` empties.
  /// See [`JoinReply`].
  reply: Option<futures_channel::oneshot::Sender<JoinReply>>,
}

impl PendingJoin {
  /// Resolve the caller's reply once, from the current `contacted` set. Idempotent:
  /// after the first call `reply` is `None` and this is a no-op, so the deadline
  /// path and the all-exchanges-done path never double-send. A zero-contact
  /// resolution is the `JoinAllFailed` the reaper would otherwise have produced.
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

/// Apply one terminal `ExchangeCompleted` to its await-result join waiter (if
/// any), driving both decoupled terminals: remove `eid` from the waiter's
/// `pending` and, on success, push the peer into `contacted`; resolve the
/// caller's reply the instant `pending` empties (ahead of the observation
/// hand-off, so a slow delegate cannot delay it); and once the waiter is fully
/// done (reply sent AND `pending` empty) clear its still-recorded ignore-join
/// streams and reap it. A `StreamId` the success-path merge already consumed is
/// absent, so the clear removes only the streams whose exchange did not merge.
///
/// Shared by the live drain and unit tests; takes the decoded
/// `(eid, peer, succeeded)` rather than the `Event` so it is callable without
/// constructing a coordinator-internal `ExchangeCompleted`.
fn complete_join_exchange<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  pending_joins: &mut Vec<PendingJoin>,
  eid: ExchangeId,
  peer: SocketAddr,
  succeeded: bool,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
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

/// Driver-side state for the single in-flight graceful-leave operation.
///
/// A [`Command::Leave`] that finds the endpoint `Alive` initiates the machine's
/// `leave()`, which queues the leave intent + direct notices to peers and
/// withholds [`Event::LeftCluster`] until they have drained. The pump parks this
/// here and replies only once that `LeftCluster` arrives (success) or `deadline`
/// elapses ([`SerfError::LeaveTimeout`]) — so a returned `Ok(())` means the
/// leave actually reached the wire, never merely that it was queued.
///
/// Leave is a SHARED operation: a second `Command::Leave` racing an in-flight
/// one (cloned handles can both call `leave()`) does NOT re-invoke
/// `endpoint.leave()` (a repeated leave once already `Leaving`/`Left` is a
/// terminal no-op that emits no completion event, so a fresh waiter would hang).
/// Instead it joins this in-flight operation by pushing its reply onto
/// `repliers`. Every terminal path — `LeftCluster` success, timeout reap,
/// shutdown — drains EVERY replier.
struct PendingLeave {
  /// Reply channels of every `leave()` caller that joined this in-flight leave
  /// — the initiator plus any racing clones. Drained together on the single
  /// terminal outcome (`Ok` on `LeftCluster`, `LeaveTimeout` on deadline,
  /// `Shutdown` on teardown).
  repliers: Vec<futures_channel::oneshot::Sender<Result<()>>>,
  /// Wall-clock instant past which the pump replies [`SerfError::LeaveTimeout`]
  /// to every replier even if `LeftCluster` has not yet fired.
  deadline: Instant,
}

impl PendingLeave {
  /// Reply to every joined `leave()` caller with a fresh `Result<()>` from
  /// `make_result`. A constructor closure (rather than a single cloned value)
  /// sidesteps `SerfError` not being `Clone` — every terminal outcome here
  /// (`Ok(())`, `LeaveTimeout`, `Shutdown`) is trivially reconstructible.
  async fn resolve_all(self, mut make_result: impl FnMut() -> Result<()>) {
    for replier in self.repliers {
      // Ignoring Err: a `leave()` caller dropped its reply receiver (its
      // user-facing future was cancelled); nothing to surface.
      let _ = replier.send(make_result());
    }
  }
}

/// Pump-loop-local state tracking outstanding commands awaiting completion.
struct PendingCommands {
  /// Outstanding await-result join waiters. See [`PendingJoin`].
  joins: Vec<PendingJoin>,
  /// Outstanding graceful-leave waiter (at most one at a time). See [`PendingLeave`].
  leave: Option<PendingLeave>,
}

/// Payload for [`BridgeInbound::Bytes`]: a slice of plaintext bytes the
/// per-bridge task read from its stream half, addressed to one exchange.
pub(crate) struct BridgeBytes {
  /// Exchange the bytes belong to.
  pub(crate) eid: ExchangeId,
  /// Owned heap copy of the bytes read off the socket.
  pub(crate) bytes: Vec<u8>,
  /// Wall-clock instant at which the bridge observed these bytes on the
  /// socket. Passed to `handle_transport_data` as the observation time so a
  /// response that arrived BEFORE the exchange deadline is not retroactively
  /// timed out by the pump's later `Instant::now()` sample.
  pub(crate) received_at: Instant,
}

/// Payload for [`BridgeInbound::Eof`]: the per-bridge task observed an orderly
/// close (read returned `Ok(0)` or the close-signal arm fired).
pub(crate) struct BridgeEof {
  /// Exchange that hit EOF.
  pub(crate) eid: ExchangeId,
  /// Wall-clock instant at which the bridge observed the peer's FIN. See
  /// [`BridgeBytes::received_at`] for the deadline-gate rationale.
  pub(crate) received_at: Instant,
}

/// Payload for [`BridgeInbound::Error`]: the per-bridge task hit an I/O error
/// on either the read or write half.
///
/// The underlying `io::Error` is not carried: the coordinator's
/// `handle_transport_error` keys only on the exchange id, so the error value is
/// not needed downstream.
pub(crate) struct BridgeError {
  /// Exchange that failed.
  pub(crate) eid: ExchangeId,
  /// Wall-clock instant at which the bridge observed the error. See
  /// [`BridgeBytes::received_at`] for the deadline-gate rationale.
  pub(crate) received_at: Instant,
}

/// Payload for [`BridgeReady::OutboundOk`]: an outbound dial task successfully
/// connected to the peer.
///
/// Carries the `out_rx` allocated at Connect time alongside the stream so the
/// bridge spawned at receipt sees every byte the pump queued via
/// `drain_transport_transmits` between Connect and dial completion (the machine
/// surfaces the first push/pull request on the same tick the Connect lands).
pub(crate) struct OutboundOkReady {
  /// The exchange the connection belongs to.
  pub(crate) eid: ExchangeId,
  /// The connected stream.
  pub(crate) stream: TcpStream,
  /// The receive half of the bridge's pre-allocated out-channel.
  pub(crate) out_rx: mpsc::Receiver<BridgeOut>,
  /// The receive half of the bridge's pre-allocated cancel channel.
  pub(crate) cancel_rx: futures_channel::oneshot::Receiver<()>,
}

/// Payload for [`BridgeReady::OutboundFail`]: an outbound dial task hit a
/// connect error or dial timeout.
///
/// The underlying `io::Error` is not carried: `handle_dial_failed` keys only on
/// the exchange id.
pub(crate) struct OutboundFailReady {
  /// The exchange whose dial failed.
  pub(crate) eid: ExchangeId,
  /// Wall-clock instant at which the dial task observed the failure. See
  /// [`BridgeBytes::received_at`] for the deadline-gate rationale — a
  /// pre-deadline dial failure is observed as a clean terminalization rather
  /// than rejected as a timeout.
  pub(crate) received_at: Instant,
}

/// Messages an outbound dial task sends back to the pump.
pub(crate) enum BridgeReady {
  /// An outbound dial completed successfully.
  OutboundOk(OutboundOkReady),
  /// An outbound dial failed.
  OutboundFail(OutboundFailReady),
}

/// Messages a per-bridge task sends back to the pump.
pub(crate) enum BridgeInbound {
  /// Plaintext bytes read from the stream.
  Bytes(BridgeBytes),
  /// Orderly close.
  Eof(BridgeEof),
  /// Unrecoverable I/O error.
  Error(BridgeError),
}

/// Messages the pump sends to a per-bridge byte-mover task.
///
/// All variants share one channel so the bridge processes them in FIFO order:
/// every byte queued before a `ShutdownWrite` / `Close` is written to the peer
/// before the close signal fires.
pub(crate) enum BridgeOut {
  /// Outbound bytes the coordinator surfaced via `poll_transport_transmit`.
  Bytes(Vec<u8>),
  /// Half-close the write side of the bridge's stream — the FIN-on-send-half
  /// anchor of the push/pull half-close. Driven by [`StreamAction::Shutdown`].
  ShutdownWrite,
  /// Full close — the bridge sends `BridgeInbound::Eof` and exits. Driven by
  /// [`StreamAction::Close`].
  Close,
}

/// Per-bridge handle the pump owns to communicate with the byte-mover.
///
/// Two channels: the FIFO `out_tx` for bytes + graceful control, and an
/// out-of-band `cancel_tx` priority signal for a hard abort. A graceful
/// [`StreamAction::Close`] drives a flush-then-exit teardown via `out_tx`; a
/// failed [`StreamAction::Abort`] drives a hard cancel via `cancel_tx` that the
/// bridge selects on with priority, discarding any still-queued stale bytes.
struct BridgeHandle {
  /// Bytes-or-graceful-control FIFO into the bridge task.
  out_tx: mpsc::Sender<BridgeOut>,
  /// Out-of-band hard-abort signal.
  cancel_tx: futures_channel::oneshot::Sender<()>,
}

/// Hard ceiling on the per-recv UDP buffer — UDP's wire payload is capped at
/// 65507 bytes once the IP/UDP headers are deducted, so a larger buffer just
/// wastes an allocation per iteration.
const GOSSIP_RECV_BUF_MAX: usize = 65507;

/// The largest the encrypted wrapper can inflate a gossip datagram, or `0` when
/// no encryption backend is built in. The wrapper carries the algorithm tag,
/// nonce, and AEAD auth tag; sizing the recv buffer to include it keeps an
/// encrypted datagram from being silently truncated by the kernel.
#[cfg(encryption)]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = memberlist_proto::ENCRYPTED_WRAPPER_OVERHEAD;
#[cfg(not(encryption))]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = 0;

/// Compute the per-recv UDP buffer size from the coordinator's `gossip_mtu`,
/// plus the encrypted-wrapper overhead when an encryption backend is built in,
/// clamped at [`GOSSIP_RECV_BUF_MAX`]. Sizing to the inflated value keeps a
/// configured `gossip_mtu` close to the historical default from being truncated
/// once the encryption tag/nonce are added on the wire.
fn gossip_recv_buf_len<I, RT, G, R>(endpoint: &StreamEndpoint<I, SocketAddr, RT, G, R>) -> usize
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  endpoint
    .gossip_mtu()
    .saturating_add(ENCRYPTED_WRAPPER_OVERHEAD)
    .min(GOSSIP_RECV_BUF_MAX)
}

/// Single-owner pump task.
///
/// Drives the serf `StreamEndpoint` until the command channel closes (all
/// handles dropped) or a [`Command::Shutdown`] is received. All mutations on the
/// endpoint happen here; reads happen via the published [`SerfSnapshot`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_driver_loop<I, RT, D, G, R>(
  mut endpoint: StreamEndpoint<I, SocketAddr, RT, G, R>,
  gossip_socket: UdpSocket,
  listener: TcpListener,
  commands: Receiver<Command<I, SocketAddr>>,
  events_tx: Sender<Event<I, SocketAddr>>,
  events_dropped: Rc<Cell<u64>>,
  observation_dropped: Rc<Cell<u64>>,
  snapshot: SnapshotCell<I>,
  shutdown_flag: Rc<Cell<bool>>,
  driver_opts: RuntimeOptions,
  stream_opts: StreamTransportOptions,
  delegate: D,
  // Cluster label applied to both gossip encode and decode. `None` accepts
  // datagrams from any cluster.
  label: Option<Bytes>,
  // The driver's keyring delegate: applies inbound key-management ops and
  // produces the `respond_key` answer. Present only under an encryption backend.
  #[cfg(encryption)] keyring: Rc<dyn KeyringDelegate>,
) where
  D: Delegate<Id = I, Address = SocketAddr>,
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng + Unpin,
  R: rand::Rng + SeedableRng,
{
  let mut bridges: HashMap<ExchangeId, BridgeHandle> = HashMap::new();
  let (bridge_inbound_tx, mut bridge_inbound_rx) =
    mpsc::bounded::<BridgeInbound>(stream_opts.bridge_inbound_cap());
  let (bridge_ready_tx, bridge_ready_rx) = flume::unbounded::<BridgeReady>();

  // Spawn the per-driver observation task — it owns the user `Delegate` and the
  // `EventStream` sender and runs OFF this pump task: the pump `try_send`s every
  // surfaced event onto `obs_tx`, the task dispatches the matching observation
  // hook then forwards to subscribers. Decoupling keeps a slow `notify_*` /
  // `merge_remote_state` from stalling protocol advancement — and therefore a
  // parked leave reply that depends on a follow-up input the pump must service.
  let (obs_tx, obs_rx) = match driver_opts.observation_channel() {
    Channel::Unbounded => mpsc::unbounded::<Event<I, SocketAddr>>(),
    Channel::Bounded(n) => mpsc::bounded::<Event<I, SocketAddr>>(n),
  };
  // `obs_payload_bytes` tracks the bytes of payload-bearing events (`User`)
  // currently queued in `obs_tx`: the pump adds on enqueue, the obs task
  // subtracts on dequeue. The byte backstop bounds the memory a large payload
  // occupies while a delegate falls behind.
  let obs_payload_bytes = Rc::new(Cell::new(0u64));
  compio::runtime::spawn(observation_task::<I, D>(
    obs_rx,
    delegate,
    events_tx,
    events_dropped.clone(),
    obs_payload_bytes.clone(),
  ))
  .detach();

  // Stash for the [`Command::Shutdown`] reply — acked AFTER the post-loop
  // cleanup closes the listener and gossip socket so the bound ports are free
  // when the caller resumes from `shutdown.await`.
  let mut shutdown_reply: Option<futures_channel::oneshot::Sender<Result<()>>> = None;
  let mut pending = PendingCommands {
    joins: Vec::new(),
    leave: None,
  };

  // Per-pump UDP recv buffer size, derived once at entry from the coordinator's
  // `gossip_mtu` (fixed for the endpoint lifetime).
  let recv_buf_len = gossip_recv_buf_len::<I, RT, G, R>(&endpoint);

  // Observation-channel payload byte backstop. `Bounded(n)`'s count cap bounds
  // the NUMBER of queued events, but one `User` event can own up to
  // `max_stream_frame_size` bytes; cap the queued payload bytes at four frames'
  // worth. `Unbounded` opts out of dropping, so it opts out of the byte backstop.
  let obs_payload_budget: Option<u64> = match driver_opts.observation_channel() {
    Channel::Bounded(_) => Some((endpoint.max_stream_frame_size() as u64).saturating_mul(4)),
    Channel::Unbounded => None,
  };

  // Arm the periodic probe / gossip / push-pull schedulers. Without this the
  // coordinator's schedulers stay unset, so failure detection, dissemination,
  // and anti-entropy never run.
  endpoint.start_scheduling(Instant::now());
  refresh_snapshot::<I, RT, G, R>(&endpoint, &snapshot);

  // Hoist the listener-accept future ACROSS loop iterations. On a
  // completion-based backend (io_uring) `accept()` is an in-flight SQE;
  // recreating it each iteration would drop (cancel) an already-accepted
  // connection. Persisting the future means it is only recreated when it
  // RESOLVES, never dropped mid-flight.
  let mut accept_fut = Box::pin(listener.accept().fuse());

  loop {
    let mut dirty = false;
    let mut exit = false;

    // Service any already-ready accept off the select's borrow, with bounded
    // fairness, so a busy recv/timer socket cannot hold a kernel-accepted
    // connection unbridged until the peer's handshake deadline.
    if accept_fut.is_terminated() {
      accept_fut.set(listener.accept().fuse());
    }
    {
      let mut accept_cx = Context::from_waker(Waker::noop());
      let mut accepted_n = 0;
      while accepted_n < driver_opts.iter_drain_cap().max(1) {
        match accept_fut.as_mut().poll(&mut accept_cx) {
          Poll::Ready(accepted) => {
            if handle_accepted::<I, RT, G, R>(
              accepted,
              &mut endpoint,
              &mut bridges,
              &bridge_inbound_tx,
              stream_opts,
            ) {
              dirty = true;
            }
            accept_fut.set(listener.accept().fuse());
            accepted_n += 1;
          }
          Poll::Pending => break,
        }
      }
    }

    // Iter-top command fairness drain so a network flood does not starve user
    // commands (the `cmd` select arm sits below the network arms).
    let mut cmd_drained = 0;
    while cmd_drained < driver_opts.cmd_fairness_budget() {
      match commands.try_recv() {
        Ok(c) => {
          let now = Instant::now();
          let is_shutdown = matches!(c, Command::Shutdown(_));
          if is_shutdown {
            exit = true;
          }
          dispatch_command::<I, RT, G, R>(
            &mut endpoint,
            &mut bridges,
            &bridge_ready_tx,
            stream_opts,
            &mut shutdown_reply,
            &mut pending,
            driver_opts.leave_timeout(),
            c,
            now,
          )
          .await;
          cmd_drained += 1;
          dirty = true;
          if is_shutdown {
            break;
          }
        }
        // All `Serf` handles dropped: tear down exactly as a `Command::Shutdown`
        // would. Under a continuous UDP flood the main select's command arm is
        // starved by the higher-priority recv arm, so this iter-top drain is the
        // only path that observes the disconnect; collapsing it into `Empty`
        // would spin the pump forever and leak the bound sockets.
        Err(flume::TryRecvError::Disconnected) => {
          exit = true;
          break;
        }
        // No command queued right now — end the fairness drain.
        Err(flume::TryRecvError::Empty) => break,
      }
    }

    // Drain every already-arrived bridge input BEFORE the timeout decision: a
    // peer's push/pull response (or a bridge's EOF / error), AND an
    // outbound-dial completion, MUST be applied to the coordinator before any
    // `handle_timeout` call — otherwise the timeout sweep would wrongly mark
    // exchanges overdue whose terminating bytes are already in the channel.
    let mut drained = 0;
    while drained < driver_opts.iter_drain_cap() {
      match bridge_inbound_rx.try_recv() {
        Ok(inbound) => {
          dispatch_bridge_inbound::<I, RT, G, R>(&mut endpoint, inbound);
          drained += 1;
          dirty = true;
        }
        Err(_) => break,
      }
    }
    drained = 0;
    while drained < driver_opts.iter_drain_cap() {
      match bridge_ready_rx.try_recv() {
        Ok(ready) => {
          handle_bridge_ready::<I, RT, G, R>(
            &mut endpoint,
            &mut bridges,
            &bridge_inbound_tx,
            ready,
            stream_opts.bridge_recv_buf_len(),
            stream_opts.close_timeout(),
          );
          drained += 1;
          dirty = true;
        }
        Err(_) => break,
      }
    }

    // Honor `exit` from the iter-top cmd drain before the select so a quiet
    // shutdown lands promptly. Flush, reap, publish, break.
    if exit {
      drain_outputs::<I, RT, G, R>(
        &mut endpoint,
        &mut bridges,
        &bridge_ready_tx,
        stream_opts,
        &gossip_socket,
        &label,
        &obs_tx,
        &observation_dropped,
        &obs_payload_bytes,
        obs_payload_budget,
        &mut pending,
        #[cfg(encryption)]
        &*keyring,
      )
      .await;
      reap_pending_joins(&mut endpoint, &mut pending.joins, Instant::now()).await;
      reap_pending_leave(&mut pending.leave, Instant::now()).await;
      refresh_snapshot::<I, RT, G, R>(&endpoint, &snapshot);
      break;
    }

    // Re-poll the deadline AFTER applying drained inputs (which may have
    // advanced or cleared it). Fold in the earliest pending-leave deadline so
    // the timer arm fires by a graceful leave's timeout even when the
    // coordinator has no nearer deadline.
    let setup_now = Instant::now();
    let endpoint_deadline = endpoint
      .poll_timeout()
      .unwrap_or(setup_now + driver_opts.idle_wake_interval());
    let timeout_deadline = [
      Some(endpoint_deadline),
      min_pending_join_deadline(&pending.joins),
      min_pending_leave_deadline(&pending.leave),
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or(endpoint_deadline);

    // BOUNDED past-due preemption. Under a continuous UDP-recv flood the main
    // select's recv arm always wins over the timer, so `handle_timeout` would
    // never fire and stale bridges / overdue probes would never be reaped. Route
    // through the `fire_timeout_with_drain` chokepoint: it drains every
    // immediately-ready gossip datagram (decoding each inline) AND every queued
    // bridge completion before firing `handle_timeout` only if the deadline is
    // still past, so a buffered Ack that sits BEHIND an unrelated datagram in the
    // socket's queue still resolves its probe ahead of the suspicion sweep. This
    // is a safe site to build the drain's `recv_from` SQEs — the main loop's
    // `recv_fut` is not yet in flight, so there is no second-SQE race.
    if setup_now >= timeout_deadline {
      if fire_timeout_with_drain::<I, RT, G, R>(
        &mut endpoint,
        &mut bridges,
        &bridge_inbound_tx,
        &mut bridge_inbound_rx,
        &bridge_ready_rx,
        &gossip_socket,
        recv_buf_len,
        &label,
        driver_opts,
        stream_opts,
      )
      .await
      {
        dirty = true;
      }

      let terminal = drain_outputs::<I, RT, G, R>(
        &mut endpoint,
        &mut bridges,
        &bridge_ready_tx,
        stream_opts,
        &gossip_socket,
        &label,
        &obs_tx,
        &observation_dropped,
        &obs_payload_bytes,
        obs_payload_budget,
        &mut pending,
        #[cfg(encryption)]
        &*keyring,
      )
      .await;
      reap_pending_joins(&mut endpoint, &mut pending.joins, Instant::now()).await;
      reap_pending_leave(&mut pending.leave, Instant::now()).await;
      if dirty {
        refresh_snapshot::<I, RT, G, R>(&endpoint, &snapshot);
      }
      // A conflict `Event::Shutdown` observed during the drain is terminal: break
      // into teardown rather than re-entering the loop.
      if terminal {
        break;
      }
      continue;
    }

    // Drained inputs may have advanced state without past-due timer pressure —
    // flush their outputs before entering the select so a snapshot observer
    // sees the post-input state promptly.
    if dirty {
      let terminal = drain_outputs::<I, RT, G, R>(
        &mut endpoint,
        &mut bridges,
        &bridge_ready_tx,
        stream_opts,
        &gossip_socket,
        &label,
        &obs_tx,
        &observation_dropped,
        &obs_payload_bytes,
        obs_payload_budget,
        &mut pending,
        #[cfg(encryption)]
        &*keyring,
      )
      .await;
      reap_pending_joins(&mut endpoint, &mut pending.joins, Instant::now()).await;
      reap_pending_leave(&mut pending.leave, Instant::now()).await;
      refresh_snapshot::<I, RT, G, R>(&endpoint, &snapshot);
      dirty = false;
      // A conflict `Event::Shutdown` observed during the flush is terminal.
      if terminal {
        break;
      }
    }

    let mut timer_fired = false;
    {
      let recv_buf = vec![0u8; recv_buf_len];
      let recv_fut = gossip_socket.recv_from(recv_buf).fuse();
      let cmd_fut = commands.recv_async().fuse();
      let ready_fut = bridge_ready_rx.recv_async().fuse();
      let timer_fut = compio::time::sleep_until(timeout_deadline.into_std()).fuse();
      pin_mut!(recv_fut, cmd_fut, ready_fut, timer_fut);

      // Arm priority (top → bottom):
      //   1. recv     — kernel-buffered UDP gossip (an Ack resolves a probe
      //                 deadline before handle_timeout marks the peer suspect).
      //   2. timer    — past-due deadline (ahead of accept so a saturated
      //                 listener cannot starve the suspicion / probe reapers).
      //   3. accept   — inbound TCP connections (front door for new exchanges).
      //   4. cmd      — user commands (demoted below the network arms so a
      //                 cloned-handle command flood cannot starve them).
      //   5. ready    — outbound-dial completions.
      //   6. bridge_in — per-bridge byte messages (lowest priority).
      select_biased! {
        gossip = recv_fut => {
          let BufResult(res, buf) = gossip;
          if let Ok((n, src)) = res {
            let now = Instant::now();
            dispatch_gossip::<I, RT, G, R>(&mut endpoint, src, &buf[..n], now, label.clone());
            dirty = true;
          }
          // Ignoring Err: a transient recv error is non-fatal — the next
          // iteration re-arms recv with a fresh buffer.
        }
        _ = timer_fut => {
          // Defer the drain + `handle_timeout` to the `fire_timeout_with_drain`
          // chokepoint AFTER this scope drops the in-flight `recv_fut` and the
          // bridge-receiver borrows: a freshly-submitted recv can be pending on
          // its first poll on a completion backend, so the timer winning does NOT
          // prove a would-block. The chokepoint drains the gossip socket and the
          // bridge channels before deciding on `handle_timeout`, and dropping
          // `recv_fut` first avoids a second concurrent `recv_from` SQE.
          timer_fired = true;
        }
        accepted = accept_fut.as_mut() => {
          if handle_accepted::<I, RT, G, R>(
            accepted,
            &mut endpoint,
            &mut bridges,
            &bridge_inbound_tx,
            stream_opts,
          ) {
            dirty = true;
          }
        }
        cmd = cmd_fut => {
          match cmd {
            Ok(c) => {
              exit = matches!(c, Command::Shutdown(_));
              let now = Instant::now();
              dispatch_command::<I, RT, G, R>(
                &mut endpoint,
                &mut bridges,
                &bridge_ready_tx,
                stream_opts,
                &mut shutdown_reply,
                &mut pending,
                driver_opts.leave_timeout(),
                c,
                now,
              ).await;
              dirty = true;
            }
            // All handles dropped → channel closed. Treat as shutdown.
            Err(_) => exit = true,
          }
        }
        ready = ready_fut => {
          if let Ok(ready) = ready {
            handle_bridge_ready::<I, RT, G, R>(
              &mut endpoint,
              &mut bridges,
              &bridge_inbound_tx,
              ready,
              stream_opts.bridge_recv_buf_len(),
              stream_opts.close_timeout(),
            );
            dirty = true;
          }
          // Ignoring Err: the pump holds its own `bridge_ready_tx`, so the
          // channel cannot disconnect while the loop is alive.
        }
        bi = bridge_inbound_rx.recv() => {
          if let Some(inbound) = bi {
            dispatch_bridge_inbound::<I, RT, G, R>(&mut endpoint, inbound);
            dirty = true;
          }
          // Ignoring None: every bridge dropped its sender; later iterations
          // wake on other arms or on a freshly-spawned bridge.
        }
      }
    }

    // Past-due deadline, deferred from the timer arm so the in-flight `recv_fut`
    // SQE and the bridge-receiver borrows are dropped before the chokepoint
    // drains the gossip socket and `&mut`-drains the bridge receivers. Routes
    // through the SAME `fire_timeout_with_drain` as the past-due branch so a
    // near-deadline queued Ack is consumed before any suspicion.
    if timer_fired
      && fire_timeout_with_drain::<I, RT, G, R>(
        &mut endpoint,
        &mut bridges,
        &bridge_inbound_tx,
        &mut bridge_inbound_rx,
        &bridge_ready_rx,
        &gossip_socket,
        recv_buf_len,
        &label,
        driver_opts,
        stream_opts,
      )
      .await
    {
      dirty = true;
    }

    // A conflict `Event::Shutdown` drained here is terminal — fold it into
    // `exit` so the loop breaks into teardown after delivering the event.
    if drain_outputs::<I, RT, G, R>(
      &mut endpoint,
      &mut bridges,
      &bridge_ready_tx,
      stream_opts,
      &gossip_socket,
      &label,
      &obs_tx,
      &observation_dropped,
      &obs_payload_bytes,
      obs_payload_budget,
      &mut pending,
      #[cfg(encryption)]
      &*keyring,
    )
    .await
    {
      exit = true;
    }
    reap_pending_joins(&mut endpoint, &mut pending.joins, Instant::now()).await;
    reap_pending_leave(&mut pending.leave, Instant::now()).await;

    if dirty {
      refresh_snapshot::<I, RT, G, R>(&endpoint, &snapshot);
    }

    if exit {
      break;
    }
  }

  // Cleanup. Order: flip the shutdown flag so a racing clone observes it on
  // entry, drain queued commands with Err(Shutdown), drop the command receiver
  // so a late send fails fast, signal every live bridge to close, close the
  // bound sockets (awaited so their ports are released), then ack the observed
  // shutdown caller.
  shutdown_flag.set(true);
  while let Ok(c) = commands.try_recv() {
    reply_shutdown(c);
  }
  drop(commands);
  // Reply Err(Shutdown) to every parked await-result join waiter whose reply has
  // not yet resolved — their reply receivers would otherwise hang forever (the
  // loop's reap path is gone and the task is exiting). Clear EVERY remaining
  // waiter's ignore-join streams (including a lingering waiter that already
  // replied on its deadline but was awaiting exchange completion): after shutdown
  // no late merge can arrive, so nothing is left to suppress.
  for pj in pending.joins.drain(..) {
    for s in &pj.ignore_streams {
      endpoint.clear_ignore_join_stream(*s);
    }
    if let Some(reply) = pj.reply {
      // Ignoring Err: caller dropped the reply receiver.
      let _ = reply.send(Err((SmallVec::new(), SerfError::Shutdown)));
    }
  }
  if let Some(pl) = pending.leave.take() {
    pl.resolve_all(|| Err(SerfError::Shutdown)).await;
  }
  for (_eid, handle) in bridges.drain() {
    // Ignoring Err: the bridge may have exited already; close is best-effort.
    let _ = handle.out_tx.try_send(BridgeOut::Close);
  }
  // Drop the persistent accept future first: it holds an in-flight accept
  // borrowing `listener`, so the listener cannot be moved into `close()` while it
  // is alive. Cancelling a pending accept during shutdown is correct — a
  // connection arriving as the driver tears down has nothing to be served.
  drop(accept_fut);
  // Await the listener close rather than a plain drop so the bound TCP port is
  // released before the reply fires: a dropped compio listener is not guaranteed
  // to close its fd synchronously (Windows IOCP closes asynchronously), so a
  // plain drop could race a same-address rebind into AddrInUse. Mirrors the
  // awaited close the TCP/TLS construction path uses to discard a retry listener.
  // Ignoring Err: a close error during teardown is unactionable.
  let _ = listener.close().await;
  // Ignoring Err: socket close on shutdown — the runtime tears down fds anyway.
  // Awaiting the close drains the UDP gossip socket so its kernel slot is
  // released before the stashed reply fires.
  let _ = gossip_socket.close().await;

  if let Some(reply) = shutdown_reply {
    // Ignoring Err: caller dropped the reply receiver.
    let _ = reply.send(Ok(()));
  }
}

/// Reply `Err(Shutdown)` to a command drained during teardown.
fn reply_shutdown<I>(c: Command<I, SocketAddr>) {
  // Ignoring Err on each send: caller dropped the reply receiver.
  match c {
    Command::Join(JoinCmd { reply, .. }) => {
      let _ = reply.send(Err((SmallVec::new(), SerfError::Shutdown)));
    }
    Command::Leave(LeaveCmd { reply }) | Command::Shutdown(ShutdownCmd { reply }) => {
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
  }
}

/// Dispatch one [`Command`] onto the serf endpoint. Replies are best-effort via
/// the per-command reply channel — a dropped reply receiver means the caller
/// gave up.
///
/// The [`Command::Shutdown`] reply is NOT acked inline; it is stashed into
/// `shutdown_reply` so the pump acks the caller only AFTER the sockets drop in
/// the post-loop cleanup.
#[allow(clippy::too_many_arguments)]
async fn dispatch_command<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_ready_tx: &Sender<BridgeReady>,
  stream_opts: StreamTransportOptions,
  shutdown_reply: &mut Option<futures_channel::oneshot::Sender<Result<()>>>,
  pending: &mut PendingCommands,
  leave_timeout: Duration,
  cmd: Command<I, SocketAddr>,
  now: Instant,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let running = endpoint.state() == SerfState::Alive;
  match cmd {
    Command::Join(JoinCmd {
      seeds,
      kind,
      ignore_old,
      reply,
    }) => {
      // Gate on a running node: `leave()` is terminal (it stops the periodic
      // schedulers), so a join after leave would leave the node non-participating.
      if !running {
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(Err((SmallVec::new(), SerfError::NotRunning)));
        return;
      }
      // Announce the serf-level join intent so peers learn the local join ltime
      // without waiting for the next anti-entropy round.
      if let Err(e) = endpoint.join() {
        // Ignoring Err: caller dropped the reply receiver.
        let _ = reply.send(Err((SmallVec::new(), SerfError::from(e))));
        return;
      }
      // Both kinds share the same `start_push_pull` fan-out; the kind only
      // affects WHEN the reply fires. Each `start_push_pull` queues a `Connect`
      // the inline drain routes to its bridge before the next seed dials.
      match kind {
        JoinKind::Dispatch => {
          let mut dispatched: SmallVec<[SocketAddr; 1]> = SmallVec::new();
          for seed in seeds {
            // Ignoring StreamId return: the Dispatch arm tracks no per-exchange
            // waiter state — completion / failure surfaces through `poll_event`.
            let _sid = endpoint.start_join_push_pull(seed, ignore_old, now);
            while let Some(action) = endpoint.poll_action() {
              process_one_action(action, bridges, bridge_ready_tx, stream_opts, None);
            }
            dispatched.push(seed);
          }
          // Ignoring Err: caller dropped the reply receiver (the fire-and-forget
          // `dispatch_join` future was cancelled).
          let _ = reply.send(Ok(dispatched));
        }
        JoinKind::WaitForCompletion(WaitForCompletionArgs { deadline }) => {
          // Capture the resolved seed count BEFORE the loop consumes `seeds`:
          // this is the `JoinAllFailed` denominator. A seed that retires before
          // producing a `Connect` (TLS sni miss, dialer error, elapsed dial
          // deadline) never enters `exchange_ids`, so deriving `requested` from
          // the captured-exchange count would undercount the requested seeds.
          let requested = seeds.len();
          let mut exchange_ids: HashSet<ExchangeId> = HashSet::with_capacity(requested);
          // The `StreamId`s this join's `start_push_pull` calls returned; the
          // Connect capture keys on this set (not the peer) so a same-peer dial
          // flushed for another subsystem is never misattributed to this join.
          let mut started: HashSet<StreamId> = HashSet::with_capacity(requested);
          for seed in seeds {
            let sid = endpoint.start_join_push_pull(seed, ignore_old, now);
            started.insert(sid);
            while let Some(action) = endpoint.poll_action() {
              process_one_action(
                action,
                bridges,
                bridge_ready_tx,
                stream_opts,
                Some((&started, &mut exchange_ids)),
              );
            }
          }
          // An `ignore_old` join recorded every seed's `StreamId` in the machine
          // (via `start_join_push_pull`); the driver owns clearing any that fail
          // to merge. A plain join recorded nothing, so this stays empty.
          let ignore_streams: SmallVec<[StreamId; 1]> = if ignore_old {
            started.iter().copied().collect()
          } else {
            SmallVec::new()
          };
          if exchange_ids.is_empty() {
            // Every seed retired before a `Connect`, so no exchange will ever
            // surface a terminal `ExchangeCompleted` (nor a merge). Parking would
            // idle the waiter until `deadline`; resolve now with the all-failed
            // outcome, and clear the recorded ignore streams that will never merge.
            for s in &ignore_streams {
              endpoint.clear_ignore_join_stream(*s);
            }
            // Ignoring Err: caller dropped the reply receiver.
            let _ = reply.send(Err((
              SmallVec::new(),
              SerfError::JoinAllFailed(JoinFailed::new(requested, 0)),
            )));
          } else {
            pending.joins.push(PendingJoin {
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
      // not re-invoke `leave()`, which once `Leaving`/`Left` is a terminal no-op
      // emitting no second `LeftCluster`). Otherwise INITIATE: snapshot `Alive`
      // before the call (it decides whether a `LeftCluster` will fire), then park
      // (was Alive) or reply immediately (idempotent no-op / error).
      if let Some(pl) = pending.leave.as_mut() {
        pl.repliers.push(reply);
      } else {
        let was_alive = running;
        let res: Result<()> = endpoint.leave(now).map_err(SerfError::from);
        match res {
          Ok(()) if was_alive => {
            pending.leave = Some(PendingLeave {
              repliers: vec![reply],
              deadline: now + leave_timeout,
            });
          }
          // Idempotent no-op (not Alive) ⇒ no `LeftCluster` will fire, or the
          // call errored. Reply immediately; parking would hang.
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
        endpoint.force_leave(id, prune, at).map_err(SerfError::from)
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
        endpoint
          .user_event(name, payload, cmd.coalesce)
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
        endpoint
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
        endpoint
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
        endpoint.set_tags(tags).map_err(SerfError::from)
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
        endpoint.install_key(key, at).map_err(SerfError::from)
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
        endpoint.use_key(key, at).map_err(SerfError::from)
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
        endpoint.remove_key(key, at).map_err(SerfError::from)
      } else {
        Err(SerfError::NotRunning)
      };
      // Ignoring Err: caller dropped the reply receiver.
      let _ = reply.send(res);
    }
    #[cfg(encryption)]
    Command::ListKeys(ListKeysCmd { now: at, reply }) => {
      let res = if running {
        endpoint.list_keys(at).map_err(SerfError::from)
      } else {
        Err(SerfError::NotRunning)
      };
      // Ignoring Err: caller dropped the reply receiver.
      let _ = reply.send(res);
    }
    Command::Shutdown(ShutdownCmd { reply }) => {
      // Drain every live bridge so the byte-movers observe the close and exit.
      // Do NOT ack the caller here — the sockets are still bound; stash the
      // reply and let the post-loop cleanup ack AFTER they drop.
      for (_eid, handle) in bridges.drain() {
        // Ignoring Err: bridge may have already exited; close is best-effort.
        let _ = handle.out_tx.try_send(BridgeOut::Close);
      }
      *shutdown_reply = Some(reply);
    }
  }
}

/// Route one bridge inbound message into the coordinator.
fn dispatch_bridge_inbound<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  inbound: BridgeInbound,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  // Each inbound carries its own `received_at` — forwarding THAT instant (not a
  // fresh `Instant::now()`) ensures the stream FSM's deadline gate compares
  // against the true arrival time, so a response queued before the exchange
  // deadline is not retroactively marked Timeout.
  match inbound {
    BridgeInbound::Bytes(BridgeBytes {
      eid,
      bytes,
      received_at,
    }) => {
      endpoint.handle_transport_data(eid, &bytes, false, received_at);
    }
    BridgeInbound::Eof(BridgeEof { eid, received_at }) => {
      // Feed the read-half EOF anchor. Do NOT remove the `BridgeHandle` — for an
      // inbound (server-side) push/pull bridge the read EOF arrives BEFORE the
      // response is generated; the machine queues the response in this same
      // call. The bridge entry stays until the matching `StreamAction::Close`.
      endpoint.handle_transport_data(eid, &[], true, received_at);
    }
    BridgeInbound::Error(BridgeError { eid, received_at }) => {
      // A transport ERROR is NOT a clean EOF: route it to `handle_transport_error`
      // so the bridge fails rather than taking the benign-EOF path.
      endpoint.handle_transport_error(eid, received_at);
    }
  }
}

/// Decode and feed one inbound UDP gossip datagram into the coordinator, then
/// drain its memberlist ingress queue and feed every decoded message back
/// through `handle_message`.
///
/// The codec hop is `decrypt → label-strip → decode`: with an encryption
/// backend built in, the encryption wrapper is stripped (and authenticated)
/// before the cluster label is verified; with none built in the serf gossip
/// plane carries no wire transforms, so the raw bytes ARE the label frame. A
/// compound datagram is split into its ordered messages by `parse_messages`,
/// each fed as a typed message.
fn dispatch_gossip<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  src: SocketAddr,
  datagram: &[u8],
  now: Instant,
  label: Option<Bytes>,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  endpoint.handle_gossip(src, datagram, now);

  let decode_opts = DecodeOptions::new(label);
  while let Some((from_addr, raw)) = endpoint.poll_memberlist_ingress() {
    // Reverse the wire transform stack the peer applied before decoding. With an
    // encryption backend built in, `decrypt_gossip` strips (and authenticates)
    // the encryption wrapper — returning the frame unchanged when no keyring is
    // configured, and dropping a frame the keyring cannot decrypt. With none
    // built in the serf gossip plane carries no transforms, so the raw bytes are
    // the plain label frame. A dropped datagram is recovered on the next gossip
    // round (gossip is lossy and self-healing).
    #[cfg(encryption)]
    let plain = match endpoint.decrypt_gossip(&raw) {
      Ok(p) => Bytes::from(p),
      Err(_) => continue,
    };
    #[cfg(not(encryption))]
    let plain = raw;
    // Strip the optional cluster label and verify it matches; a mismatched or
    // absent label on a labeled cluster (or vice versa) is dropped here.
    let inner = match decode_incoming(plain, &decode_opts) {
      Ok(b) => b,
      Err(_) => continue,
    };
    // Demux plain vs compound and feed each decoded message to the coordinator.
    // A malformed frame drops the whole datagram (lossy gossip; the peer
    // retransmits on the next round).
    let msgs = match parse_messages::<I, SocketAddr>(inner) {
      Ok(m) => m,
      Err(_) => continue,
    };
    for msg in msgs {
      endpoint.handle_message(from_addr, msg, now);
    }
  }
}

/// Process one [`StreamAction`].
///
/// Connect: pre-allocate the bridge's out-channel and insert the [`BridgeHandle`]
/// BEFORE spawning the dial task, so bytes the machine surfaces on the same tick
/// as the Connect reach the bridge via the `out_rx` handed to it on dial
/// completion. Shutdown / Close / Abort signal the per-bridge channel.
///
/// `capture` is `Some` only on the await-result join path: when the Connect's
/// originating [`StreamId`] is in the `started` set, its [`ExchangeId`] is
/// recorded into the waiter's pending set, binding it to that join alone.
fn process_one_action(
  action: StreamAction,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_ready_tx: &Sender<BridgeReady>,
  stream_opts: StreamTransportOptions,
  capture: Option<(&HashSet<StreamId>, &mut HashSet<ExchangeId>)>,
) {
  match action {
    StreamAction::Connect(info) => {
      let eid = info.id();
      let peer = info.peer();
      if let Some((started, pending_exchanges)) = capture
        && started.contains(&info.stream_id())
      {
        pending_exchanges.insert(eid);
      }
      let (out_tx, out_rx) = mpsc::unbounded::<BridgeOut>();
      let (cancel_tx, cancel_rx) = futures_channel::oneshot::channel::<()>();
      bridges.insert(eid, BridgeHandle { out_tx, cancel_tx });
      let ready_tx = bridge_ready_tx.clone();
      let dial_timeout = stream_opts.dial_timeout();
      compio::runtime::spawn(async move {
        // Bound the dial so a connect to an unreachable peer reports failure
        // promptly instead of hanging on the kernel's default timeout.
        let dial = TcpStream::connect(peer).fuse();
        let timeout = compio::time::sleep(dial_timeout).fuse();
        pin_mut!(dial, timeout);
        let msg = select_biased! {
          res = dial => match res {
            Ok(stream) => BridgeReady::OutboundOk(OutboundOkReady {
              eid,
              stream,
              out_rx,
              cancel_rx,
            }),
            // Dropping `out_rx` / `cancel_rx` disconnects the channels; any
            // bytes the pump queued during the dial are dropped — correct, the
            // exchange never produced a wire to write them on.
            Err(_) => BridgeReady::OutboundFail(OutboundFailReady {
              eid,
              received_at: Instant::now(),
            }),
          },
          _ = timeout => BridgeReady::OutboundFail(OutboundFailReady {
            eid,
            received_at: Instant::now(),
          }),
        };
        // Ignoring Err: pump has exited; the dial result is unobservable.
        let _ = ready_tx.send_async(msg).await;
      })
      .detach();
    }
    StreamAction::Shutdown(eref) => {
      // Half-close the send side (the push/pull half-close anchor). The bridge
      // writes every queued `Bytes` ahead of `ShutdownWrite`, then shuts down
      // its write half and continues reading.
      if let Some(handle) = bridges.get(&eref.id()) {
        // Ignoring Err: the bridge may have already exited; best-effort.
        let _ = handle.out_tx.try_send(BridgeOut::ShutdownWrite);
      }
    }
    StreamAction::Close(eref) => {
      // Graceful full teardown. Removing the handle is symmetric with the
      // bridge's exit — later bytes for this exchange miss the lookup in
      // `drain_transport_transmits` and are dropped.
      if let Some(handle) = bridges.remove(&eref.id()) {
        // Ignoring Err: see Shutdown arm.
        let _ = handle.out_tx.try_send(BridgeOut::Close);
      }
    }
    StreamAction::Abort(eref) => {
      // Hard teardown of a FAILED exchange. Signal `cancel_tx`: the bridge
      // breaks immediately, discarding any still-queued stale bytes.
      if let Some(handle) = bridges.remove(&eref.id()) {
        // Ignoring Err: the bridge may have already exited; best-effort.
        let _ = handle.cancel_tx.send(());
      }
    }
  }
}

/// Drain every [`StreamAction`] the coordinator has queued. Returns `true` iff
/// any action was processed.
fn drain_actions<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_ready_tx: &Sender<BridgeReady>,
  stream_opts: StreamTransportOptions,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut progress = false;
  while let Some(action) = endpoint.poll_action() {
    progress = true;
    process_one_action(action, bridges, bridge_ready_tx, stream_opts, None);
  }
  progress
}

/// Drain every queued per-exchange transport-transmit and forward the bytes to
/// the matching bridge's write half. MUST run before the action queue advances
/// past a pending `Shutdown` / `Close` — the coordinator withholds the teardown
/// for an exchange until its `poll_transport_transmit` queue is empty.
fn drain_transport_transmits<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &HashMap<ExchangeId, BridgeHandle>,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut progress = false;
  while let Some((eid, _peer, bytes)) = endpoint.poll_transport_transmit() {
    progress = true;
    let Some(handle) = bridges.get(&eid) else {
      // No live bridge — the dial failed and was removed, or a Close retired
      // the exchange. Draining unblocks the matching Shutdown/Close.
      continue;
    };
    // Ignoring Err: the only failure on an unbounded channel is Disconnected
    // (the bridge exited and reads no more bytes); dropping is safe.
    let _ = handle.out_tx.try_send(BridgeOut::Bytes(bytes.to_vec()));
  }
  progress
}

/// Drain every queued unreliable (UDP gossip) [`Transmit`] and send it on the
/// gossip socket. Outbound gossip is label-stamped (`encode_outgoing` /
/// `encode_outgoing_compound`), then — with an encryption backend built in —
/// wrapped in the encryption layer (`encrypt_gossip`) before it hits the wire.
async fn drain_transmits<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  gossip_socket: &UdpSocket,
  label: Option<Bytes>,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let encode_opts = EncodeOptions::new(label);
  let mut progress = false;
  while let Some(transmit) = endpoint.poll_memberlist_transmit() {
    progress = true;
    let (peer, plain): (SocketAddr, Bytes) = match transmit {
      Transmit::Packet(pkt) => {
        let (to, msg) = pkt.into_parts();
        match encode_outgoing(&msg, &encode_opts) {
          Ok(b) => (to, b),
          // A locally-built message that fails to encode is dropped so one bad
          // codec invocation cannot wedge the pump.
          Err(_) => continue,
        }
      }
      Transmit::Compound(cmp) => {
        let (to, msgs) = cmp.into_parts();
        match encode_outgoing_compound(&msgs, &encode_opts) {
          Ok(b) => (to, b),
          Err(_) => continue,
        }
      }
    };
    // Wrap the label frame in the encryption layer when an encryption backend is
    // built in: `encrypt_gossip` is identity when no keyring is configured, and
    // drops the datagram if a configured backend rejects it rather than emitting
    // plaintext on an encrypted-cluster path. With none built in the frame goes
    // out as-is.
    #[allow(unused_mut)]
    let mut on_wire: Vec<u8> = plain.to_vec();
    #[cfg(encryption)]
    {
      on_wire = match endpoint.encrypt_gossip(&on_wire) {
        Ok(bytes) => bytes,
        Err(_) => continue,
      };
    }
    let BufResult(res, _buf) = gossip_socket.send_to(on_wire, peer).await;
    // Ignoring Err: a transient send error is non-fatal — gossip is lossy and
    // the next probe/gossip round recovers.
    let _ = res;
  }
  progress
}

/// Read-modify-write `endpoint`'s LIVE wire keyring for one inbound [`KeyRequest`],
/// returning the [`KeyResponseArgs`] built from the post-op live state.
///
/// The endpoint-facing wrapper over [`serf_driver::apply_key_request`]: it reads
/// the coordinator's live `encryption_options`, applies the op variant-exactly
/// against the live ring, and on a real mutation publishes the rotated ring back
/// via `set_encryption_options` — so the gossip and reliable planes re-key in
/// lockstep — then notifies the keyring observer for persistence. A node with no
/// keyring configured answers `result = false` and makes no wire change; a
/// read-only `list` or a refused op leaves the wire untouched.
#[cfg(encryption)]
fn apply_key_request_live<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  delegate: &dyn KeyringDelegate,
  req: &KeyRequest<I, SocketAddr>,
) -> KeyResponseArgs
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut encryption = endpoint.encryption_options().clone();
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
    endpoint.set_encryption_options(encryption);
    delegate.keyring_updated(&new_ring);
  }
  resp
}

/// Drain every queued serf [`Event`]: synchronous protocol accounting (leave
/// completion, conflict-shutdown, key requests), then hand off to the
/// observation task. NO `.await` on user delegate code. Returns `true` iff any
/// event was drained.
///
/// Sets `*terminal` to `true` if a terminal [`Event::Shutdown`] was observed —
/// the local node lost an id-conflict vote and the pump MUST tear down. The
/// event is still delivered to subscribers before the main loop breaks.
#[allow(clippy::too_many_arguments)]
async fn drain_events<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  obs_tx: &mpsc::Sender<Event<I, SocketAddr>>,
  observation_dropped: &Cell<u64>,
  obs_payload_bytes: &Cell<u64>,
  obs_payload_budget: Option<u64>,
  pending: &mut PendingCommands,
  terminal: &mut bool,
  #[cfg(encryption)] keyring: &dyn KeyringDelegate,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut drained = false;
  while let Some(ev) = endpoint.poll_event() {
    drained = true;
    // Await-result join resolution. `ExchangeCompleted` fires for every outbound
    // bridge kind; an await-join waiter consumes only `PushPull` completions.
    // `complete_join_exchange` drives both decoupled terminals: it resolves the
    // caller's reply the moment `pending` empties (here on the pump task, ahead of
    // the observation hand-off, so a slow delegate cannot delay it) and clears the
    // ignore-join streams only once the waiter is fully done.
    if let Event::ExchangeCompleted(ref c) = ev
      && c.kind() == ExchangeKind::PushPull
    {
      complete_join_exchange(
        endpoint,
        &mut pending.joins,
        c.eid(),
        *c.peer(),
        matches!(c.outcome(), ExchangeStatus::Succeeded),
      );
    }
    // Leave-completion resolution. `LeftCluster` fires once the leave notices
    // have drained to the wire; resolving the parked waiter here — on this pump
    // task, ahead of the observation task's `notify_leave` — is what makes
    // `leave()` return promptly once the flush is done.
    if matches!(ev, Event::LeftCluster)
      && let Some(pl) = pending.leave.take()
    {
      pl.resolve_all(|| Ok(())).await;
    }
    // Conflict-shutdown enforcement. `Event::Shutdown` means the local node lost
    // an id-conflict vote and MUST stop, exactly as for a `Command::Shutdown`.
    // Flag it for the main loop (which breaks into teardown after this drain)
    // while still delivering the event to subscribers below.
    if matches!(ev, Event::Shutdown) {
      *terminal = true;
    }
    // Key-management request enforcement. `Event::KeyRequest` requires the driver
    // to apply the install/use/remove/list op to its keyring and answer the
    // originator; without this the inbound key op times out and local key state
    // never changes. Applied here on the pump (it mutates the endpoint through
    // `respond_key`) ahead of the observation hand-off below.
    #[cfg(encryption)]
    if let Event::KeyRequest(req) = &ev {
      let resp = apply_key_request_live(endpoint, keyring, req);
      // Ignoring Err: `respond_key` fails only when the response cannot be routed
      // (originator gone / relay dropped); the key op has already applied to the
      // live wire keyring.
      let _ = endpoint.respond_key(req, resp, Instant::now());
    }

    let payload_bytes = observation_payload_bytes(&ev);

    // Byte backstop (bounded channels only): the count cap does not bound memory
    // when an event carries a large user payload. If enqueueing would push the
    // queued payload bytes over budget, yield once so the obs task can drain,
    // re-check, and drop + count if still over.
    if let (Some(budget), Some(bytes)) = (obs_payload_budget, payload_bytes) {
      if obs_payload_bytes.get().saturating_add(bytes) > budget {
        yield_once().await;
      }
      if obs_payload_bytes.get().saturating_add(bytes) > budget {
        observation_dropped.set(observation_dropped.get() + 1);
        continue;
      }
    }

    // Hand off to the observation task (delegate dispatch + EventStream forward,
    // off this pump task), non-blocking. `Full` yields once then retries; drop +
    // count only if still full.
    match obs_tx.try_send(ev) {
      Ok(()) => add_obs_payload(obs_payload_bytes, payload_bytes),
      Err(mpsc::TrySendError::Closed(_)) => {}
      Err(mpsc::TrySendError::Full(ev)) => {
        yield_once().await;
        match obs_tx.try_send(ev) {
          Ok(()) => add_obs_payload(obs_payload_bytes, payload_bytes),
          Err(_) => observation_dropped.set(observation_dropped.get() + 1),
        }
      }
    }
  }
  drained
}

/// Drain every outbound surface to quiescence in the documented order: actions,
/// transport-transmits, gossip transmits, events; repeat until no method makes
/// progress (a flushed byte queue releases a withheld Shutdown/Close that the
/// next `drain_actions` surfaces).
///
/// Returns `true` iff a terminal [`Event::Shutdown`] was observed while
/// draining — the local node lost an id-conflict vote and the main loop MUST
/// break into teardown.
#[allow(clippy::too_many_arguments)]
async fn drain_outputs<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_ready_tx: &Sender<BridgeReady>,
  stream_opts: StreamTransportOptions,
  gossip_socket: &UdpSocket,
  label: &Option<Bytes>,
  obs_tx: &mpsc::Sender<Event<I, SocketAddr>>,
  observation_dropped: &Cell<u64>,
  obs_payload_bytes: &Cell<u64>,
  obs_payload_budget: Option<u64>,
  pending: &mut PendingCommands,
  #[cfg(encryption)] keyring: &dyn KeyringDelegate,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut terminal = false;
  loop {
    let did_actions = drain_actions::<I, RT, G, R>(endpoint, bridges, bridge_ready_tx, stream_opts);
    let did_transports = drain_transport_transmits::<I, RT, G, R>(endpoint, bridges);
    let did_transmits =
      drain_transmits::<I, RT, G, R>(endpoint, gossip_socket, label.clone()).await;
    let did_events = drain_events::<I, RT, G, R>(
      endpoint,
      obs_tx,
      observation_dropped,
      obs_payload_bytes,
      obs_payload_budget,
      pending,
      &mut terminal,
      #[cfg(encryption)]
      keyring,
    )
    .await;
    if !(did_actions || did_transports || did_transmits || did_events) {
      break;
    }
  }
  terminal
}

/// Per-driver observation task: dispatch each event's [`Delegate`] hook, then
/// fan the event out to the `EventStream`, OFF the pump loop.
///
/// Unlike a membership-only protocol, every serf [`Event`] (member transitions,
/// user events, queries, responses) is the application's observation surface, so
/// all events are forwarded to subscribers. The forward is best-effort: a full
/// queue (slow subscriber) drops the event and counts it into `events_dropped`,
/// never blocking. The task exits when `obs_rx` closes (pump dropped `obs_tx`).
async fn observation_task<I, D>(
  mut obs_rx: mpsc::Receiver<Event<I, SocketAddr>>,
  delegate: D,
  events_tx: Sender<Event<I, SocketAddr>>,
  events_dropped: Rc<Cell<u64>>,
  obs_payload_bytes: Rc<Cell<u64>>,
) where
  D: Delegate<Id = I, Address = SocketAddr>,
  I: Clone,
{
  while let Some(ev) = obs_rx.recv().await {
    // Free the byte-backstop budget this event occupied as soon as it leaves the
    // channel — before the (possibly slow) delegate hook — so the pump's enqueue
    // side sees the reclaimed budget promptly.
    let payload = observation_payload_bytes(&ev);
    if let Some(b) = payload {
      obs_payload_bytes.set(obs_payload_bytes.get().saturating_sub(b));
    }
    // Contain a panicking delegate hook so the task SURVIVES and keeps releasing
    // the byte-backstop reservations of still-queued events. Ignoring the unwind
    // result: the panic is contained and this event is simply dropped.
    let _ = std::panic::AssertUnwindSafe(dispatch_event_delegate(&delegate, &ev))
      .catch_unwind()
      .await;
    if events_tx
      .try_send(ev)
      .is_err_and(|e| matches!(e, flume::TrySendError::Full(_)))
    {
      events_dropped.set(events_dropped.get() + 1);
    }
  }
}

/// Reap await-result join waiters on the deadline timer. This drives ONLY the
/// reply terminal: a waiter whose `deadline` has elapsed replies its partial
/// `contacted` (the same timeout semantics the caller always had). Reply
/// resolution is decoupled from ignore-stream cleanup — on a deadline that fires
/// with exchanges still live (`pending` non-empty) the waiter replies and
/// LINGERS, keeping its `ignore_streams` recorded so a late merge for a still-live
/// exchange still suppresses; the lingering waiter is then reaped by
/// [`complete_join_exchange`] when its last exchange completes. The all-exchanges-
/// done case (including a zero-exchange degenerate waiter) is reaped there too.
///
/// A waiter is removed (and its still-recorded ignore-join streams cleared) here
/// only once BOTH terminals are reached — reply resolved AND `pending` empty —
/// covering any timer/event ordering. A stream the success-path merge already
/// consumed is absent, so the clear removes only the streams whose exchange did
/// not merge. `swap_remove` is sound because `joins` has no ordering.
async fn reap_pending_joins<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  pending_joins: &mut Vec<PendingJoin>,
  now: Instant,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut i = 0;
  while i < pending_joins.len() {
    if now >= pending_joins[i].deadline {
      // Deadline reached: resolve the reply with the partial `contacted`. This is
      // a no-op if the all-exchanges-done path already replied.
      pending_joins[i].resolve_reply();
    }
    if pending_joins[i].is_done() {
      // Reply resolved AND every dispatched exchange completed — clear the
      // ignore-join streams that did not merge and reap the waiter.
      let pj = pending_joins.swap_remove(i);
      for s in &pj.ignore_streams {
        endpoint.clear_ignore_join_stream(*s);
      }
    } else {
      i += 1;
    }
  }
}

/// Earliest pending-join deadline, if any — folded into the per-iteration
/// `timeout_deadline` so the timer fires by the first expiring join's deadline.
fn min_pending_join_deadline(pending_joins: &[PendingJoin]) -> Option<Instant> {
  pending_joins
    .iter()
    .filter(|pj| pj.reply.is_some())
    .map(|pj| pj.deadline)
    .min()
}

/// Reap a deadline-expired graceful-leave waiter. If `pending_leave`'s deadline
/// has elapsed without `Event::LeftCluster` having resolved it, reply
/// [`SerfError::LeaveTimeout`] to every joined replier and clear the slot.
async fn reap_pending_leave(pending_leave: &mut Option<PendingLeave>, now: Instant) {
  if let Some(pl) = pending_leave.as_ref()
    && now >= pl.deadline
  {
    let pl = pending_leave.take().expect("checked Some above");
    pl.resolve_all(|| Err(SerfError::LeaveTimeout)).await;
  }
}

/// Earliest pending-leave deadline, if any — folded into the per-iteration
/// `timeout_deadline` so the timer fires even under a continuous network flood.
fn min_pending_leave_deadline(pending_leave: &Option<PendingLeave>) -> Option<Instant> {
  pending_leave.as_ref().map(|pl| pl.deadline)
}

/// Drain-first timeout chokepoint for the stream pump — the single site that
/// calls `handle_timeout` on this plane.
///
/// Both the past-due preemption branch and the main select's timer arm route
/// through here. A freshly-submitted `recv` can be pending on its first poll on a
/// completion backend (io_uring), so a biased select's timer arm winning does NOT
/// prove a genuine would-block: a near-deadline gossip Ack may be queued or
/// immediately readable when the timer fires. The order is UDP drain (each
/// datagram decoded inline by `dispatch_gossip`) → bridge-completion drain (a
/// peer's push/pull response / EOF / error and an outbound-dial completion must be
/// applied before the sweep, or an exchange whose terminating bytes are in the
/// channel is wrongly marked overdue) → deadline re-check → `handle_timeout` only
/// if still past. Returns `true` iff any work was applied.
///
/// The caller MUST ensure no other `recv_from` SQE on `gossip_socket` is in flight
/// (the main loop drops its `recv_fut` before invoking this), so the bounded drain
/// is the sole builder of recv SQEs here.
#[allow(clippy::too_many_arguments)]
async fn fire_timeout_with_drain<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_inbound_tx: &mpsc::Sender<BridgeInbound>,
  bridge_inbound_rx: &mut mpsc::Receiver<BridgeInbound>,
  bridge_ready_rx: &Receiver<BridgeReady>,
  gossip_socket: &UdpSocket,
  recv_buf_len: usize,
  label: &Option<Bytes>,
  driver_opts: RuntimeOptions,
  stream_opts: StreamTransportOptions,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let mut dirty = false;

  // UDP drain FIRST: read every immediately-ready gossip datagram (bounded by the
  // inbound drain cap; `.max(1)` so recv always gets at least one shot even at a
  // zero cap) and decode each inline via `dispatch_gossip`, so a near-deadline Ack
  // sitting BEHIND an unrelated datagram still resolves its probe before the
  // suspicion sweep. Emptiness is reaped via `poll_with(ZERO)` inside the drain,
  // not a time window.
  let drained = drain_past_due_udp(
    gossip_socket,
    recv_buf_len,
    driver_opts.iter_drain_cap().max(1),
    |src, datagram| {
      let now = Instant::now();
      // The stream pump decodes inline in `dispatch_gossip` (handle_gossip +
      // memberlist-ingress drain), so a drained Ack resolves its probe deadline
      // here, before the recheck below decides on `handle_timeout`.
      dispatch_gossip::<I, RT, G, R>(endpoint, src, datagram, now, label.clone());
      // Report whether the coordinator deadline is STILL past: a drained Ack that
      // resolved it ends the drain so the main select regains fairness.
      now
        >= endpoint
          .poll_timeout()
          .unwrap_or(now + driver_opts.idle_wake_interval())
    },
  )
  .await;
  if drained {
    dirty = true;
  }

  // Bridge-completion drain (no cap): every already-queued bridge input must be
  // applied before the timeout sweep.
  while let Ok(inbound) = bridge_inbound_rx.try_recv() {
    dispatch_bridge_inbound::<I, RT, G, R>(endpoint, inbound);
    dirty = true;
  }
  while let Ok(ready) = bridge_ready_rx.try_recv() {
    handle_bridge_ready::<I, RT, G, R>(
      endpoint,
      bridges,
      bridge_inbound_tx,
      ready,
      stream_opts.bridge_recv_buf_len(),
      stream_opts.close_timeout(),
    );
    dirty = true;
  }

  // Deadline re-check, then `handle_timeout` iff the deadline is still past: when
  // the UDP drain emptied the socket a buffered Ack was already decoded inline
  // above (so the deadline is no longer past), and when a flood capped the drain,
  // firing for liveness is at worst a transient SWIM-refutable false Suspect
  // rather than a frozen-timer stall.
  let now = Instant::now();
  let after_drain_deadline = endpoint
    .poll_timeout()
    .unwrap_or(now + driver_opts.idle_wake_interval());
  if now >= after_drain_deadline {
    endpoint.handle_timeout(now);
    dirty = true;
  }
  dirty
}

/// Publish a fresh [`SerfSnapshot`] of the endpoint's observable membership to
/// the snapshot cell.
///
/// Skips the publish when the local node is not yet present in the serf
/// membership store (the local `NodeJoined` sieve has not fired): the prior
/// snapshot — seeded at construction — stays current, and `SerfSnapshot::new`
/// (which requires the local node) is never called with it absent.
fn refresh_snapshot<I, RT, G, R>(
  endpoint: &StreamEndpoint<I, SocketAddr, RT, G, R>,
  snapshot: &SnapshotCell<I>,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  let members = endpoint.members_snapshot();
  let local_id = endpoint.local_id();
  if !members.iter().any(|m| m.node().id_ref() == local_id) {
    return;
  }
  let snap = SerfSnapshot::new(
    members,
    local_id,
    endpoint.state(),
    LamportTime::from(endpoint.member_time()),
    LamportTime::from(endpoint.event_time()),
    LamportTime::from(endpoint.query_time()),
  );
  *snapshot.borrow_mut() = Rc::new(snap);
}

/// Route one [`BridgeReady`] — an outbound-dial result — into the coordinator,
/// spawning a per-bridge byte-mover on success.
fn handle_bridge_ready<I, RT, G, R>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_inbound_tx: &mpsc::Sender<BridgeInbound>,
  ready: BridgeReady,
  recv_buf_len: usize,
  close_timeout: Duration,
) where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  match ready {
    BridgeReady::OutboundOk(OutboundOkReady {
      eid,
      stream,
      out_rx,
      cancel_rx,
    }) => {
      // The handle was pre-inserted at Connect time. If it is gone the
      // coordinator retired the exchange while the dial was in flight; drop the
      // stream so the peer is never asked to honor an exchange we no longer track.
      if !bridges.contains_key(&eid) {
        drop(stream);
        drop(out_rx);
        drop(cancel_rx);
        return;
      }
      spawn_bridge(
        stream,
        eid,
        out_rx,
        cancel_rx,
        bridge_inbound_tx,
        recv_buf_len,
        close_timeout,
      );
    }
    BridgeReady::OutboundFail(OutboundFailReady { eid, received_at }) => {
      // Remove the handle, then terminalize the exchange as a DIAL FAILURE (a
      // connect that never established has no wire; a benign EOF would falsely
      // complete a one-way exchange as success).
      bridges.remove(&eid);
      endpoint.handle_dial_failed(eid, received_at);
    }
  }
}

/// Route one accepted inbound connection: allocate the exchange, register the
/// bridge handle, and spawn the byte mover. Returns `true` iff an accept was
/// processed (a state-affecting event the caller treats as dirty).
fn handle_accepted<I, RT, G, R>(
  accepted: io::Result<(TcpStream, SocketAddr)>,
  endpoint: &mut StreamEndpoint<I, SocketAddr, RT, G, R>,
  bridges: &mut HashMap<ExchangeId, BridgeHandle>,
  bridge_inbound_tx: &mpsc::Sender<BridgeInbound>,
  stream_opts: StreamTransportOptions,
) -> bool
where
  I: memberlist_proto::Id + Clone,
  RT: StreamTransport,
  G: rand::Rng,
  R: rand::Rng + SeedableRng,
{
  match accepted {
    Ok((stream, peer)) => {
      let now = Instant::now();
      let Some(eid) = endpoint.accept_connection(peer, now) else {
        // Not admitted (leaving, the inbound-stream cap is reached, or a
        // record-layer config error): drop the accepted stream rather than spawn
        // a byte mover for a connection the machine will never feed.
        drop(stream);
        return true;
      };
      let (out_tx, out_rx) = mpsc::unbounded::<BridgeOut>();
      let (cancel_tx, cancel_rx) = futures_channel::oneshot::channel::<()>();
      bridges.insert(eid, BridgeHandle { out_tx, cancel_tx });
      spawn_bridge(
        stream,
        eid,
        out_rx,
        cancel_rx,
        bridge_inbound_tx,
        stream_opts.bridge_recv_buf_len(),
        stream_opts.close_timeout(),
      );
      true
    }
    // Transient accept error (fd pressure, peer reset mid-handshake). The kernel
    // keeps the listener open; the resolved accept is re-armed at the loop top.
    Err(_) => false,
  }
}

/// Spawn the [`crate::bridge::bridge_task`] byte-mover for `eid`. The caller has
/// already inserted the matching [`BridgeHandle`] so bytes queued before the
/// bridge spawned reach the wire via the `out_rx` handed in here.
fn spawn_bridge(
  stream: TcpStream,
  eid: ExchangeId,
  out_rx: mpsc::Receiver<BridgeOut>,
  cancel_rx: futures_channel::oneshot::Receiver<()>,
  bridge_inbound_tx: &mpsc::Sender<BridgeInbound>,
  recv_buf_len: usize,
  close_timeout: Duration,
) {
  let inbound_tx = bridge_inbound_tx.clone();
  compio::runtime::spawn(crate::bridge::bridge_task(
    stream,
    eid,
    out_rx,
    cancel_rx,
    inbound_tx,
    recv_buf_len,
    close_timeout,
  ))
  .detach();
}

#[cfg(test)]
mod tests;
