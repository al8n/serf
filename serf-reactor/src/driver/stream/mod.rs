//! The stream-plane driver pump: a quinn-style `Future::poll` that owns the serf
//! [`StreamEndpoint`], its UDP gossip socket, the TCP reliable listener's accept
//! task, and the per-bridge handle table. Per-exchange reliable TCP I/O runs in
//! spawned bridge tasks (one per exchange) wired to the pump by `flume` channels;
//! inbound connections are accepted by a dedicated task (the listener's `accept`
//! is async-only).
//!
//! This is the `Send`/`Arc`/`agnostic` sibling of serf-compio's `!Send`
//! `stream_driver_loop`, restructured onto memberlist-reactor's readiness pump:
//! there is NO top-level `select!` and NO completion-backend drain. Each poll
//! drains queued [`Command`]s, recv-loops the gossip socket to kernel-empty
//! (`poll_recv_from` → `Poll::Pending`), services accept / dial / bridge-inbound
//! channels, runs [`drain_surfaces`](StreamDriver::drain_surfaces) (decode
//! buffered ingress, route transport/gossip egress, emit events), and fires
//! `handle_timeout` INLINE at exactly one site. serf-compio's `poll_with(ZERO)`
//! reap / `drain_past_due_udp` / `fire_timeout_with_drain` are deleted — those
//! exist only because io_uring is completion-based; the reactor is readiness-based
//! and `Poll::Pending` from the socket IS the kernel-empty signal.

use std::{
  collections::{HashMap, HashSet},
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
  AsyncSpawner, Runtime,
  net::{Net, TcpListener, TcpStream, UdpSocket},
};
use bytes::Bytes;
use flume::{Receiver, Sender, TrySendError};
use futures_channel::oneshot;
use futures_util::{FutureExt, select};
use memberlist_proto::{
  Instant, SeedableRng, StreamId, Transmit,
  codec::{
    DecodeOptions, EncodeOptions, decode_incoming, encode_outgoing, encode_outgoing_compound,
    parse_messages,
  },
  streams::{StreamAction, StreamTransport},
};
use serf_driver::SerfSnapshot;
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
    shared::{ExchangeId, dispatch_event_delegate, observation_payload_bytes},
  },
  error::{JoinFailed, Result, SerfError},
  shared::Shared,
};
#[cfg(encryption)]
use serf_proto::{KeyResponseArgs, event::KeyRequest};

/// Hard ceiling on the per-recv UDP buffer — UDP's wire payload is capped at
/// 65507 bytes once the IP/UDP headers are deducted, so a larger buffer just
/// wastes an allocation.
const GOSSIP_RECV_BUF_MAX: usize = 65507;

/// The largest the encrypted wrapper can inflate a gossip datagram, or `0` when no
/// encryption backend is built in — so an encrypted datagram is not silently
/// truncated by the kernel.
#[cfg(encryption)]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = memberlist_proto::ENCRYPTED_WRAPPER_OVERHEAD;
#[cfg(not(encryption))]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = 0;

/// Capacity of the accepted-connection channel; the accept task backpressures
/// once the pump is this many connections behind.
pub(crate) const ACCEPT_CAP: usize = 256;

/// Cap on the count of application-data events retained after a full observation
/// channel (the payload byte budget bounds their bytes; this bounds their count).
const OBS_OVERFLOW_MAX: usize = 1024;

/// How long the SWIM suspicion tick may be held back under a sustained UDP flood
/// before it fires bounded-early. The refuting Ack that would clear a false
/// `Suspect` rides ONLY the UDP recv path, which a flood pins every poll, so —
/// unlike the reliable plane — the SWIM tick cannot gate on a drain watermark. It
/// need not: SWIM refutes within its multi-second suspicion window, so a few ms of
/// staleness is the correct freshness-under-load trade. This is a `Duration`, not
/// a poll count: the failure mode of a fixed count is that at a low `iter_drain_cap`
/// (or a large exchange) a count elapses while real pre-deadline work is still
/// buffered, whereas a wall-clock grace bounds the staleness directly.
const SWIM_STALENESS_GRACE: Duration = Duration::from_millis(5);

/// A message from the pump to a bridge's TCP write side. Teardown is signalled out
/// of band by dropping the [`BridgeHandle`], not by a variant here, so it can
/// preempt even a write stalled on an unresponsive peer.
pub(crate) enum BridgeOut {
  /// Plaintext transport bytes to write to the peer.
  Data(Bytes),
  /// Half-close the write side (FIN) after the send half retired.
  ShutdownWrite,
}

/// Payload of [`BridgeInbound::Data`].
pub(crate) struct BridgeData {
  pub(crate) eid: ExchangeId,
  pub(crate) bytes: Vec<u8>,
  /// Wall-clock instant the bridge read these bytes; forwarded as the machine's
  /// observation time so a response that arrived BEFORE the exchange deadline is
  /// not retroactively timed out by the pump's later `Instant::now()` sample.
  pub(crate) received_at: Instant,
}

/// Payload of [`BridgeInbound::Eof`] and [`BridgeInbound::Error`].
pub(crate) struct BridgeEof {
  pub(crate) eid: ExchangeId,
  pub(crate) received_at: Instant,
}

/// Inbound transport bytes / EOF / error from a bridge's TCP read side to the
/// pump.
pub(crate) enum BridgeInbound {
  /// Bytes read from the peer for an exchange.
  Data(BridgeData),
  /// The peer cleanly closed its write side (transport `read == 0`).
  Eof(BridgeEof),
  /// A transport READ/WRITE error — routed to `handle_transport_error` so a
  /// one-way UserMessage is NOT falsely completed as success by the benign-EOF
  /// path.
  Error(BridgeEof),
}

/// The pump's end of a live bridge: the write channel plus an explicit-abort
/// channel. A graceful `StreamAction::Close` drops the whole handle (`out_tx`
/// disconnects, the bridge drains queued `Data` then exits); a failed
/// `StreamAction::Abort` sends `()` on `cancel_tx` first, preempting even a
/// stalled write and discarding queued bytes.
struct BridgeHandle {
  out_tx: Sender<BridgeOut>,
  cancel_tx: oneshot::Sender<()>,
}

/// The result a dial task reports back to the pump.
enum DialStatus<R>
where
  R: Runtime,
{
  /// The connection succeeded; hand the stream and its channels back.
  Connected(DialConnected<R>),
  /// The connection failed or timed out; the pump fails the exchange.
  Failed(DialFailed),
}

/// Payload of [`DialStatus::Connected`].
struct DialConnected<R>
where
  R: Runtime,
{
  eid: ExchangeId,
  stream: <R::Net as Net>::TcpStream,
  out_rx: Receiver<BridgeOut>,
  cancel_rx: oneshot::Receiver<()>,
}

/// Payload of [`DialStatus::Failed`].
struct DialFailed {
  eid: ExchangeId,
  /// Instant the dial task observed the failure, so a pre-deadline dial failure
  /// terminalizes cleanly rather than being read as a timeout.
  received_at: Instant,
}

/// Driver-side state for one outstanding await-result join call.
///
/// A [`Command::Join`] carrying [`JoinKind::WaitForCompletion`] dispatches one
/// push/pull per resolved seed and parks the per-call state here. Contact
/// accounting is strictly per-OUTBOUND-EXCHANGE, observed via the machine's
/// [`Event::ExchangeCompleted`] filtered to [`ExchangeKind::PushPull`]. Reply
/// resolution and ignore-stream cleanup are SEPARATE terminal states: the reply
/// resolves on all-exchanges-done OR `deadline` (whichever first); the ignore
/// streams are cleared only once every dispatched exchange has completed
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
  /// This is a driver-local FALLBACK, subordinate to the machine's arrival-time
  /// `ExchangeCompleted`: [`Self::resolve_reply`] runs only from the `fire` path,
  /// which requires the reliable backlog to have drained (`reap_watermark`), so
  /// this deadline can never reap a join whose completing frame — stamped
  /// `received_at < deadline` — is still in flight. serf-proto's `StreamEndpoint`
  /// exposes no per-exchange CALLER deadline (`start_join_push_pull` takes only
  /// `peer, ignore_old, now`; the exchange deadline is the frozen inner FSM's own),
  /// so this second clock cannot yet be removed outright. It is instead reconciled
  /// at construction via [`clamp_join_deadline`], which caps it at the exchange
  /// deadline so the watermark always covers a completion that beat that exchange;
  /// plumbing the caller deadline INTO the exchange is a serf-proto follow-up.
  deadline: Instant,
  /// One-shot reply channel back to the caller, taken when the reply resolves.
  /// `None` once resolved; the waiter then lingers — only to drive ignore-stream
  /// cleanup — until `pending` empties.
  reply: Option<oneshot::Sender<JoinReply>>,
}

impl PendingJoin {
  /// Resolve the caller's reply once, from the current `contacted` set.
  /// Idempotent: after the first call `reply` is `None` and this is a no-op, so
  /// the deadline path and the all-exchanges-done path never double-send.
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

/// Reconcile a caller's await-result join deadline with the machine's push/pull
/// exchange deadline (`now + stream_timeout`), returning the effective
/// [`PendingJoin::deadline`].
///
/// A join we initiate carries two clocks: this driver-local fallback deadline and
/// the coordinator's own per-exchange deadline (`now + stream_timeout`). The pump's
/// reap watermark re-snapshots to cover a join's still-buffered completion only when
/// that join's deadline goes due ([`StreamDriver::max_due_join_leave_deadline`]);
/// the exchange deadline, hidden behind any earlier endpoint deadline in
/// `poll_timeout`'s min, never widens the watermark on its own. So a caller deadline
/// LATER than the exchange deadline lets an elapsed exchange emit a terminal
/// `ExchangeCompleted(Failed)` while the watermark still covers only an earlier
/// crossing's backlog — reaping a premature `JoinAllFailed` for a `join_deadline`
/// that has not elapsed. Clamping the driver deadline so it never exceeds the
/// exchange deadline keeps the join's deadline in `max_due` whenever the exchange
/// could fail, so the watermark has already widened to cover a completion that
/// arrived before it. Plumbing the caller deadline INTO the exchange (collapsing the
/// two clocks) is a serf-proto follow-up.
fn clamp_join_deadline(
  caller_deadline: Instant,
  now: Instant,
  stream_timeout: Duration,
) -> Instant {
  caller_deadline.min(now + stream_timeout)
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

/// The single-owner stream driver future. Runs until shutdown (a `Shutdown`
/// command, a lost id-conflict `Event::Shutdown`, or the last handle dropped).
pub(crate) struct StreamDriver<I, R, T, G, SR>
where
  // Structurally required: `endpoint` names `StreamEndpoint<I, SocketAddr, T, G,
  // SR>`, whose struct declares `I: Eq + Hash` and `where T: StreamTransport`.
  I: core::hash::Hash + Eq,
  R: Runtime,
  T: StreamTransport,
{
  endpoint: StreamEndpoint<I, SocketAddr, T, G, SR>,
  /// Unreliable gossip datagrams. `Option` so the shutdown branch can drop it
  /// (releasing the bound UDP port) BEFORE acking; `Some` for the running
  /// lifetime, taken only during teardown.
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
  obs_overflow: std::collections::VecDeque<Event<I, SocketAddr>>,
  /// Cluster label threaded into the gossip codec (outbound stamp + inbound
  /// verify).
  label: Option<Bytes>,
  /// Outstanding await-result join waiters.
  pending_joins: Vec<PendingJoin>,
  /// The in-flight graceful leave, resolved on `LeftCluster`.
  pending_leave: Option<PendingLeave>,
  /// Parked `Shutdown` replies — acked only after the bind sockets drop, so a
  /// caller resuming from `shutdown().await` can rebind the same address. A `Vec`
  /// because several callers can race `shutdown()`.
  shutdown_reply: Vec<oneshot::Sender<Result<()>>>,
  /// Each live exchange's bridge: its write channel and teardown handle.
  bridges: HashMap<ExchangeId, BridgeHandle>,
  /// Inbound connections from the accept task.
  accepted_rx: Receiver<(<R::Net as Net>::TcpStream, SocketAddr)>,
  /// Held only to be dropped on driver exit; closing it cancels the accept task's
  /// pending `accept()` so the listener is released. `Option` so the shutdown
  /// branch can drop it before acking.
  accept_shutdown_tx: Option<Sender<()>>,
  /// Join handle of the accept task; awaited on shutdown before acking so the
  /// listener FD is released (not merely signalled).
  accept_join: Option<<R::Spawner as AsyncSpawner>::JoinHandle<()>>,
  /// Inbound transport bytes/EOF from the bridge read tasks. `Some` through the
  /// running lifetime and the shutdown drain; taken once the drain disconnects,
  /// which doubles as the one-time reap guard.
  inbound_rx: Option<Receiver<BridgeInbound>>,
  /// Template inbound sender cloned into each bridge. `Option` so the shutdown
  /// freeze can drop it: with it gone, the channel reaches Disconnected exactly
  /// when the last frozen bridge exits — the drain's terminating condition.
  inbound_tx: Option<Sender<BridgeInbound>>,
  /// Dial completions from the dial tasks.
  dial_rx: Receiver<DialStatus<R>>,
  /// Cloned into each dial task to report its outcome.
  dial_tx: Sender<DialStatus<R>>,
  recv_buf: Vec<u8>,
  /// Per-poll cap on each drained surface / recv batch.
  iter_drain_cap: usize,
  timer: Option<Pin<Box<R::Sleep>>>,
  timer_deadline: Option<Instant>,
  /// Monotone count of bridge-inbound items the pump has removed from
  /// `inbound_rx` (the step-6 drain AND the shutdown drain). The reliable-plane
  /// watermark is expressed against this counter.
  inbound_drained_total: u64,
  /// The reliable plane's pre-deadline backlog target: the value
  /// `inbound_drained_total` reaches once every item that was in flight toward
  /// `inbound_rx` as of the LATEST currently-due deadline has been drained.
  /// `None` when no deadline is due. The join/leave reaps and the
  /// reliable-exchange deadlines inside `handle_timeout` fire only once
  /// `inbound_drained_total` reaches it — non-premature by FIFO, and (because a
  /// join completion never rides the UDP gossip flood) immune to that flood with
  /// no force-fire.
  ///
  /// Snapshotted to the live inbound depth and RE-snapshotted whenever the latest
  /// due deadline advances past [`Self::reap_watermark_covers`]. `inbound_rx` is
  /// FIFO and the backlog only grows with time, so the tail depth at the greatest
  /// due deadline dominates every earlier due deadline's pre-deadline completions.
  /// A single sticky snapshot taken at the FIRST due-crossing instead expires a
  /// later overlapping deadline against the first deadline's (smaller) target,
  /// reaping its still-buffered completion prematurely.
  ///
  /// Covers only frames a bridge has already READ (`received_at < deadline`,
  /// queued or parked). A frame still kernel-resident-but-unread at the deadline
  /// is stamped `received_at >= deadline` at its later read, so the FSM's
  /// arrival-time gate rejects it as a `Timeout` — a correct, Go-faithful
  /// read-deadline outcome (a read completing after the deadline is late), NOT a
  /// dropped success, so the watermark does not (and must not) wait for it.
  reap_watermark: Option<u64>,
  /// The latest due deadline [`Self::reap_watermark`] currently covers. The
  /// watermark re-snapshots to the live inbound depth whenever the greatest due
  /// deadline advances past this, so it always covers the backlog as of the
  /// latest due deadline — not merely the first that went due. `None` exactly
  /// when `reap_watermark` is `None`.
  reap_watermark_covers: Option<Instant>,
  /// Wall-clock anchor for the SWIM suspicion plane: the instant the SWIM tick
  /// first deferred on a pinned UDP path (reliable backlog already clear). `None`
  /// when the SWIM tick is not deferring. The tick fires once
  /// [`SWIM_STALENESS_GRACE`] has elapsed since this anchor.
  swim_stall_since: Option<Instant>,
  /// Frames a bridge has read but the pump has not yet received — queued in
  /// `inbound_rx` OR parked on a bridge's saturated `send_async`. Bridges bump it
  /// before each send; the pump clears it on receive. Lets the watermark account
  /// for a completion parked OUTSIDE `inbound_rx.len()`, and is exact (drains to
  /// `0`) so the watermark is always reachable.
  bridge_inbound_inflight: Arc<AtomicU64>,
  idle_wake: Duration,
  leave_timeout: Duration,
  /// The reliable push/pull exchange timeout the coordinator stamps on each
  /// dispatched exchange (`now + stream_timeout`), snapshotted from the same
  /// `EndpointOptions` the coordinator is built from. An await-result join's
  /// caller deadline is reconciled against it in [`clamp_join_deadline`].
  stream_timeout: Duration,
  close_timeout: Duration,
  dial_timeout: Duration,
  bridge_recv_buf_len: usize,
  /// The driver's keyring delegate: applies inbound key-management ops and
  /// produces the `respond_key` answer. Present only under an encryption backend.
  #[cfg(encryption)]
  keyring: Arc<dyn KeyringDelegate>,
}

impl<I, R, T, G, SR> StreamDriver<I, R, T, G, SR>
where
  I: memberlist_proto::Id + Clone,
  R: Runtime,
  T: StreamTransport,
  G: rand::Rng,
  SR: rand::Rng + SeedableRng,
{
  /// Build the driver from the endpoint, its bound gossip socket, the shared
  /// state, the observation hand-off, and the accept task's channels/handle.
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn new(
    endpoint: StreamEndpoint<I, SocketAddr, T, G, SR>,
    socket: <R::Net as Net>::UdpSocket,
    shared: Arc<Shared<I>>,
    obs_tx: Sender<Event<I, SocketAddr>>,
    obs_payload_bytes: Arc<AtomicU64>,
    obs_payload_budget: Option<u64>,
    accepted_rx: Receiver<(<R::Net as Net>::TcpStream, SocketAddr)>,
    accept_shutdown_tx: Sender<()>,
    accept_join: <R::Spawner as AsyncSpawner>::JoinHandle<()>,
    driver_opts: RuntimeOptions,
    stream_opts: StreamTransportOptions,
    label: Option<Bytes>,
    stream_timeout: Duration,
    #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
  ) -> Self {
    let buf_len = endpoint
      .gossip_mtu()
      .saturating_add(ENCRYPTED_WRAPPER_OVERHEAD)
      .min(GOSSIP_RECV_BUF_MAX);
    let (inbound_tx, inbound_rx) = flume::bounded(stream_opts.bridge_inbound_cap().max(1));
    let (dial_tx, dial_rx) = flume::unbounded();
    Self {
      endpoint,
      socket: Some(socket),
      shared,
      obs_tx,
      obs_payload_bytes,
      obs_payload_budget,
      obs_overflow: std::collections::VecDeque::new(),
      label,
      pending_joins: Vec::new(),
      pending_leave: None,
      shutdown_reply: Vec::new(),
      bridges: HashMap::new(),
      accepted_rx,
      accept_shutdown_tx: Some(accept_shutdown_tx),
      accept_join: Some(accept_join),
      inbound_rx: Some(inbound_rx),
      inbound_tx: Some(inbound_tx),
      dial_rx,
      dial_tx,
      recv_buf: vec![0u8; buf_len.max(1)],
      iter_drain_cap: driver_opts.iter_drain_cap().max(1),
      timer: None,
      timer_deadline: None,
      inbound_drained_total: 0,
      reap_watermark: None,
      reap_watermark_covers: None,
      swim_stall_since: None,
      bridge_inbound_inflight: Arc::new(AtomicU64::new(0)),
      idle_wake: driver_opts.idle_wake_interval(),
      leave_timeout: driver_opts.leave_timeout(),
      stream_timeout,
      close_timeout: stream_opts.close_timeout(),
      dial_timeout: stream_opts.dial_timeout(),
      bridge_recv_buf_len: stream_opts.bridge_recv_buf_len(),
      #[cfg(encryption)]
      keyring,
    }
  }

  /// Spawn the dial task that connects to `peer` for outbound exchange `eid`.
  fn spawn_dial(
    &self,
    eid: ExchangeId,
    peer: SocketAddr,
    out_rx: Receiver<BridgeOut>,
    cancel_rx: oneshot::Receiver<()>,
  ) {
    R::spawn_detach(dial_task::<I, R>(
      eid,
      peer,
      self.dial_timeout,
      out_rx,
      cancel_rx,
      self.dial_tx.clone(),
      self.shared.clone(),
    ));
  }

  /// Spawn the per-exchange bridge byte-mover for `eid`. The caller has already
  /// inserted the matching [`BridgeHandle`] so bytes queued before the bridge
  /// spawned reach the wire via the `out_rx` handed in here.
  fn spawn_bridge(
    &self,
    eid: ExchangeId,
    stream: <R::Net as Net>::TcpStream,
    out_rx: Receiver<BridgeOut>,
    cancel_rx: oneshot::Receiver<()>,
  ) where
    I: Send + Sync + 'static,
  {
    R::spawn_detach(crate::bridge::bridge_task::<I, R, <R::Net as Net>::TcpStream>(
      stream,
      eid,
      out_rx,
      cancel_rx,
      self
        .inbound_tx
        .as_ref()
        .expect("a bridge is only spawned while running, before the shutdown freeze drops the template inbound sender")
        .clone(),
      self.bridge_inbound_inflight.clone(),
      self.shared.clone(),
      self.bridge_recv_buf_len,
      self.close_timeout,
    ));
  }

  /// Applies one handle command to the machine.
  fn dispatch(&mut self, cmd: Command<I, SocketAddr>, now: Instant)
  where
    I: Send + Sync + 'static,
  {
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
        match kind {
          JoinKind::Dispatch => {
            let mut dispatched: SmallVec<[SocketAddr; 1]> = SmallVec::new();
            for seed in seeds {
              // Ignoring StreamId: the Dispatch arm tracks no per-exchange waiter
              // state — completion / failure surfaces through `poll_event`.
              let _sid = self.endpoint.start_join_push_pull(seed, ignore_old, now);
              while let Some(action) = self.endpoint.poll_action() {
                self.handle_stream_action(action, None);
              }
              dispatched.push(seed);
            }
            // Ignoring Err: caller dropped the reply receiver.
            let _ = reply.send(Ok(dispatched));
          }
          JoinKind::WaitForCompletion(WaitForCompletionArgs { deadline }) => {
            // Capture the resolved seed count BEFORE the loop consumes `seeds`:
            // this is the `JoinAllFailed` denominator. A seed that retires before
            // producing a `Connect` never enters `exchange_ids`, so deriving
            // `requested` from the captured-exchange count would undercount.
            let requested = seeds.len();
            let mut exchange_ids: HashSet<ExchangeId> = HashSet::with_capacity(requested);
            // The `StreamId`s this join's `start_join_push_pull` calls returned;
            // the Connect capture keys on this set (not the peer) so a same-peer
            // dial flushed for another subsystem is never misattributed here.
            let mut started: HashSet<StreamId> = HashSet::with_capacity(requested);
            for seed in seeds {
              let sid = self.endpoint.start_join_push_pull(seed, ignore_old, now);
              started.insert(sid);
              while let Some(action) = self.endpoint.poll_action() {
                self.handle_stream_action(action, Some((&started, &mut exchange_ids)));
              }
            }
            // An `ignore_old` join recorded every seed's `StreamId` in the machine;
            // the driver owns clearing any that fail to merge. A plain join
            // recorded nothing, so this stays empty.
            let ignore_streams: SmallVec<[StreamId; 1]> = if ignore_old {
              started.iter().copied().collect()
            } else {
              SmallVec::new()
            };
            if exchange_ids.is_empty() {
              // Every seed retired before a `Connect`: no exchange will ever
              // surface a terminal completion (nor a merge). Resolve now with the
              // all-failed outcome, clearing recorded ignore streams that never
              // merge.
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
                deadline: clamp_join_deadline(deadline, now, self.stream_timeout),
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
        // Do NOT ack inline: the gossip socket and TCP listener are still bound.
        // Flag shutdown and park the reply; the teardown branch acks every parked
        // caller only AFTER it drops both, so an immediate rebind on the same
        // address after `shutdown().await` succeeds.
        self.shared.begin_shutdown();
        self.shutdown_reply.push(reply);
      }
    }
  }

  /// Applies one stream action: open a dial, half-close a bridge's write, or tear
  /// a bridge down. On the await-result join path `capture` records each
  /// exchange's `Connect` id synchronously so the completion accounting can bind
  /// it to that join alone.
  fn handle_stream_action(
    &mut self,
    action: StreamAction,
    capture: Option<(&HashSet<StreamId>, &mut HashSet<ExchangeId>)>,
  ) where
    I: Send + Sync + 'static,
  {
    match action {
      StreamAction::Connect(info) => {
        let eid = info.id();
        let peer = info.peer();
        if let Some((started, pending_exchanges)) = capture
          && started.contains(&info.stream_id())
        {
          pending_exchanges.insert(eid);
        }
        let (out_tx, out_rx) = flume::unbounded();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        // No bridge task yet — the dial owns the connecting FD until it completes,
        // at which point `DialStatus::Connected` spawns the bridge.
        self.bridges.insert(eid, BridgeHandle { out_tx, cancel_tx });
        self.spawn_dial(eid, peer, out_rx, cancel_rx);
      }
      StreamAction::Shutdown(eref) => {
        if let Some(handle) = self.bridges.get(&eref.id()) {
          // Ignoring Err: the bridge task exited; its socket is already gone.
          let _ = handle.out_tx.try_send(BridgeOut::ShutdownWrite);
        }
      }
      StreamAction::Close(eref) => {
        // Graceful close. Dropping the handle disconnects `out_tx` and
        // `cancel_tx`: the bridge drains the `BridgeOut::Data` it already queued
        // (writing the exchange's final response), then exits on the `out_rx`
        // disconnect. The `cancel_tx` disconnect is mapped to a never-resolving
        // future in the bridge, so it does NOT preempt that drain.
        self.bridges.remove(&eref.id());
      }
      StreamAction::Abort(eref) => {
        // A FAILED exchange. Send the explicit cancel BEFORE dropping the handle:
        // it preempts the bridge — even a write stalled on an unresponsive peer —
        // and discards any queued bytes.
        if let Some(handle) = self.bridges.remove(&eref.id()) {
          // Ignoring Err: the bridge already exited (cancel receiver gone).
          let _ = handle.cancel_tx.send(());
        }
      }
    }
  }

  /// Route one accepted inbound connection: allocate the exchange, register the
  /// bridge handle, and spawn the byte mover. Returns `true` iff an accept was
  /// processed (a state-affecting event the caller treats as progress).
  fn handle_accepted(
    &mut self,
    stream: <R::Net as Net>::TcpStream,
    peer: SocketAddr,
    now: Instant,
  ) -> bool
  where
    I: Send + Sync + 'static,
  {
    let Some(eid) = self.endpoint.accept_connection(peer, now) else {
      // Not admitted (leaving, the inbound-stream cap is reached, or a
      // record-layer config error): drop the accepted stream rather than spawn a
      // byte mover for a connection the machine will never feed.
      drop(stream);
      return true;
    };
    let (out_tx, out_rx) = flume::unbounded();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    self.bridges.insert(eid, BridgeHandle { out_tx, cancel_tx });
    self.spawn_bridge(eid, stream, out_rx, cancel_rx);
    true
  }

  /// Route one dial completion: bridge a connected stream, or fail the exchange.
  fn handle_dial_status(&mut self, status: DialStatus<R>)
  where
    I: Send + Sync + 'static,
  {
    match status {
      DialStatus::Connected(DialConnected {
        eid,
        stream,
        out_rx,
        cancel_rx,
      }) => {
        // If the exchange was reaped while dialing, its handle is gone; drop the
        // stream and channels rather than bridge a dead exchange.
        if self.bridges.contains_key(&eid) {
          self.spawn_bridge(eid, stream, out_rx, cancel_rx);
        }
      }
      DialStatus::Failed(DialFailed { eid, received_at }) => {
        // Drive the exchange to a DIAL FAILURE — NOT a benign EOF feed. A connect
        // that never established has no wire, and a one-way `UserMessage` maps a
        // clean EOF to a SUCCESSFUL completion, which would falsely report a
        // reliable send as succeeding on an unreachable peer.
        self.bridges.remove(&eid);
        self.endpoint.handle_dial_failed(eid, received_at);
      }
    }
  }

  /// Route one bridge inbound message into the coordinator, forwarding each
  /// message's own `received_at` so the stream FSM's deadline gate compares
  /// against the true arrival time.
  fn dispatch_bridge_inbound(&mut self, inbound: BridgeInbound) {
    match inbound {
      BridgeInbound::Data(BridgeData {
        eid,
        bytes,
        received_at,
      }) => {
        self
          .endpoint
          .handle_transport_data(eid, &bytes, false, received_at);
      }
      BridgeInbound::Eof(BridgeEof { eid, received_at }) => {
        // Feed the read-half EOF anchor. Do NOT remove the `BridgeHandle` — for an
        // inbound (server-side) push/pull bridge the read EOF arrives BEFORE the
        // response is generated; the bridge entry stays until the matching
        // `StreamAction::Close`.
        self
          .endpoint
          .handle_transport_data(eid, &[], true, received_at);
      }
      BridgeInbound::Error(BridgeEof { eid, received_at }) => {
        // A transport ERROR is NOT a clean EOF: route it to `handle_transport_error`
        // so the bridge fails rather than taking the benign-EOF path.
        self.endpoint.handle_transport_error(eid, received_at);
      }
    }
  }

  /// Repeatedly runs the ordered surface pass to a FIXED POINT. A later surface can
  /// create work for an earlier one within the same poll: draining an exchange's
  /// final transport transmit releases its withheld [`StreamAction::Close`] for the
  /// action surface, and answering an `Event::KeyRequest` (`respond_key`) queues a
  /// directed gossip transmit after the transmit surface was already drained. A
  /// single ordered pass would leave that late-generated work buffered and report
  /// false quiescence (`more == false` with ready machine work undrained), so the
  /// timer could fire and the pump return `Pending` with a TCP bridge still open or
  /// a key response delayed past its query deadline.
  ///
  /// Repeating the ordered pass while any surface made progress converges: the
  /// machine emits finite output per (finite, already-buffered) input and each
  /// surface is cap-bounded, so successive passes strictly drain the buffered work
  /// down. On return `more == false` therefore genuinely means no ready machine work
  /// remains from this poll's input. A pass that hits a per-surface cap ends the
  /// fixed point immediately with `more == true`: the caller self-wakes and re-polls
  /// rather than uncapped-draining a surface a fast peer can refill, keeping each
  /// poll bounded.
  fn drain_surfaces(&mut self, cx: &mut Context<'_>) -> (bool, bool, bool)
  where
    I: Send + Sync + 'static,
  {
    let now = Instant::now();
    let mut worked = false;
    let mut ingress_capped = false;
    loop {
      let (pass_worked, pass_more, pass_ingress_capped) = self.drain_surfaces_pass(cx, now);
      worked |= pass_worked;
      // `ingress_capped` gates the SWIM staleness plane (a pinned UDP path), so
      // surface it across passes even though the machine's inbound ingress is fed
      // only by the pre-drain recv loop and so caps at most in the first pass.
      ingress_capped |= pass_ingress_capped;
      if pass_more {
        // A per-surface cap was hit: end the fixed point and self-wake (`more`)
        // rather than repeat the pass, so the poll stays bounded.
        return (worked, true, ingress_capped);
      }
      if !pass_worked {
        // No surface made progress: the fixed point is reached and no ready machine
        // work remains from this poll's input.
        return (worked, false, ingress_capped);
      }
    }
  }

  /// One ordered surface pass — inbound-ingress → action → transport → gossip →
  /// event — each capped surface draining up to `iter_drain_cap` items. Returns
  /// `(worked, more, ingress_capped)`: whether any surface produced work in this
  /// pass, whether any capped surface hit its cap, and whether the inbound-ingress
  /// surface specifically hit its cap (the UDP recv path's second stage, which
  /// gates the SWIM staleness plane). The event surface is UNCAPPED (drained to
  /// empty) so a surfaced terminal is never stranded behind a cap.
  /// [`Self::drain_surfaces`] iterates this to a fixed point so a later surface
  /// feeding an earlier one is drained the same poll.
  fn drain_surfaces_pass(&mut self, cx: &mut Context<'_>, now: Instant) -> (bool, bool, bool)
  where
    I: Send + Sync + 'static,
  {
    let budget = self.iter_drain_cap;
    let mut worked = false;
    let mut more = false;

    // Inbound gossip: decrypt + strip-label + parse, inline on the pump. The
    // parsed messages reach the single-owner machine HERE, with their arrival-time
    // `now`.
    let decode_opts = DecodeOptions::new(self.label.clone());
    let mut ingress = 0;
    while ingress < budget {
      let Some((from, raw)) = self.endpoint.poll_memberlist_ingress() else {
        break;
      };
      ingress += 1;
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
    worked |= ingress > 0;
    let ingress_capped = ingress == budget;
    more |= ingress_capped;

    // Stream actions: open dials, half-close, or tear down reliable exchanges.
    let mut actions = 0;
    while actions < budget {
      let Some(action) = self.endpoint.poll_action() else {
        break;
      };
      actions += 1;
      self.handle_stream_action(action, None);
    }
    worked |= actions > 0;
    more |= actions == budget;

    // Outbound transport bytes: route each exchange's plaintext to its bridge.
    let mut tx = 0;
    while tx < budget {
      let Some((eid, _peer, bytes)) = self.endpoint.poll_transport_transmit() else {
        break;
      };
      tx += 1;
      if let Some(handle) = self.bridges.get(&eid) {
        // Ignoring Err: the bridge task exited; the exchange will time out.
        let _ = handle.out_tx.try_send(BridgeOut::Data(bytes));
      }
    }
    worked |= tx > 0;
    more |= tx == budget;

    // Outbound gossip: encode (plain or compound) + encrypt, then send. Popping
    // the last transmit is the endpoint's leave-completion fence (it emits
    // `LeftCluster`), so the leave/shutdown datagrams reach the socket before that
    // fence fires.
    let encode_opts = EncodeOptions::new(self.label.clone());
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
      #[allow(unused_mut)]
      let mut on_wire: Vec<u8> = plain.to_vec();
      #[cfg(encryption)]
      {
        on_wire = match self.endpoint.encrypt_gossip(&on_wire) {
          Ok(bytes) => bytes,
          Err(_) => continue,
        };
      }
      if let Some(socket) = self.socket.as_ref() {
        // Ignoring Poll: gossip is best-effort — a full or errored UDP send drops
        // the datagram and SWIM recovers on the next round.
        let _ = socket.poll_send_to(cx, &on_wire, peer);
      }
    }
    worked |= sent > 0;
    more |= sent == budget;

    // Observation events: retry the overflow first, then drain to EMPTY. UNLIKE
    // the other surfaces this one is NOT capped: a surfaced terminal
    // (`ExchangeCompleted` / `LeftCluster`) folded by `send_observation` must never
    // be stranded behind a per-poll cap under a UDP flood — that residence is
    // exactly what let a flood defeat the reap gate. Sound: `pending_events` is
    // pump-fed (bounded per poll by the already-capped feeds + an O(members)
    // `handle_timeout` burst), `send_observation` is non-blocking (overflow+drop),
    // and there is no event→event feedback, so the drain terminates.
    self.flush_obs_overflow();
    let mut events = false;
    while let Some(ev) = self.endpoint.poll_event() {
      events = true;
      self.send_observation(ev);
    }
    worked |= events;

    (worked, more, ingress_capped)
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
        // Recoverable membership/control, or the overflow is full: drop, count,
        // and roll back any reservation.
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
    if matches!(ev, Event::LeftCluster)
      && let Some(pl) = self.pending_leave.take()
    {
      pl.resolve_all(|| Ok(()));
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
  /// via `set_encryption_options` — so the gossip and reliable planes re-key in
  /// lockstep — then notifies the keyring observer for persistence. A node with no
  /// keyring configured answers `result = false` and makes no wire change; a
  /// read-only `list` or a refused op leaves the wire untouched.
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

  /// The GREATEST join/leave deadline already due (`<= now`), or `None` when none
  /// is. Mirrors the min-deadline folds ([`Self::min_pending_join_deadline`]
  /// filters to still-unreplied joins) but takes the max, so the reap watermark
  /// re-snapshots to cover the backlog as of the LATEST due deadline; the FIFO
  /// backlog then covers every earlier due deadline too.
  fn max_due_join_leave_deadline(&self, now: Instant) -> Option<Instant> {
    self
      .pending_joins
      .iter()
      .filter(|pj| pj.reply.is_some())
      .map(|pj| pj.deadline)
      .chain(self.pending_leave.as_ref().map(|pl| pl.deadline))
      .filter(|&d| d <= now)
      .max()
  }

  /// Frames that a bridge has read but the pump has not yet drained, expressed as
  /// the pair `(queued, parked)`: `queued` is the observable `inbound_rx` depth,
  /// `parked` is the residue a bridge has read (`received_at < deadline`) but is
  /// still parked delivering on a SATURATED `send_async` — a completion OUTSIDE
  /// `inbound_rx.len()` that a raw depth watermark would miss. `bridge_inbound_inflight`
  /// counts queued+parked (bridges bump before each send, the pump clears on
  /// receive), so `parked = inflight - queued`. Both terms are added to the reap
  /// watermark; every counted frame is received exactly once, so the watermark is
  /// always reachable (no over-count can strand it above the drain).
  fn inbound_backlog_watermark_terms(&self) -> (u64, u64) {
    let queued = self.inbound_rx.as_ref().map_or(0, |rx| rx.len() as u64);
    let inflight = self.bridge_inbound_inflight.load(Ordering::Acquire);
    (queued, inflight.saturating_sub(queued))
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

impl<I, R, T, G, SR> Future for StreamDriver<I, R, T, G, SR>
where
  I: memberlist_proto::Id + Clone + Send + Sync + Unpin + 'static,
  R: Runtime,
  T: StreamTransport + Unpin,
  T::Options: Unpin,
  G: rand::Rng + Unpin,
  SR: rand::Rng + SeedableRng + Unpin,
  StreamEndpoint<I, SocketAddr, T, G, SR>: Unpin,
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

    // Shutdown, in ordered phases: (1) FREEZE — best-effort leave, cancel every
    // bridge's reads, drop the template inbound sender, and release the bind
    // sockets; (2) DRAIN the bridge inbound channel to all-senders-gone, folding
    // every already-read completion into its join's contacted set; (3) REAP the
    // parked joins/leave and fail the queued commands; (4) await ONLY the accept
    // task's exit before acking. The completion latch promises the bind address
    // is free (the UDP gossip socket + the TCP listener), not that every connected
    // stream FD has closed.
    if this.shared.is_shutdown() {
      if this.accept_shutdown_tx.is_some() {
        // FREEZE (one-time). Best-effort leave, freeze every live bridge, drop the
        // template inbound sender so the channel can reach all-senders-gone, and
        // release the bind sockets.
        // Ignoring Err: best-effort leave during shutdown.
        let _ = this.endpoint.leave(Instant::now());
        for (_, handle) in this.bridges.drain() {
          // Ignoring Err: the bridge may have already exited (cancel receiver
          // gone); the freeze is best-effort.
          let _ = handle.cancel_tx.send(());
        }
        this.inbound_tx = None;
        // Dropping `accept_shutdown_tx` cancels the accept task's pending
        // `accept()`; its `listener` local is released when the task is next
        // scheduled (awaited below). Dropping the gossip socket closes its UDP FD
        // synchronously.
        drop(this.accept_shutdown_tx.take());
        drop(this.socket.take());
      }

      // DRAIN-TO-DISCONNECTED (re-entrant), then REAP (once). Fold every
      // already-read completion into its join's contacted set BEFORE reaping. The
      // drain reads `inbound_rx` to all-senders-gone via `try_recv`; each frozen
      // bridge calls `wake_driver()` on exit, so the last exit re-polls us to
      // observe Disconnected. `inbound_rx` is taken once it disconnects, doubling
      // as the one-time reap guard.
      if this.inbound_rx.is_some() {
        let drained_to_disconnect = loop {
          let mut channel_work = false;
          let mut hit_disconnect = false;
          if let Some(inbound_rx) = this.inbound_rx.as_ref() {
            loop {
              match inbound_rx.try_recv() {
                // Inline the machine feed (rather than `dispatch_bridge_inbound`,
                // which borrows all of `*this`) so the `&this.inbound_rx` held by
                // the outer `if let` and the disjoint `&mut this.endpoint` here do
                // not overlap.
                Ok(BridgeInbound::Data(BridgeData {
                  eid,
                  bytes,
                  received_at,
                })) => {
                  this.inbound_drained_total += 1;
                  this.bridge_inbound_inflight.fetch_sub(1, Ordering::Release);
                  this
                    .endpoint
                    .handle_transport_data(eid, &bytes, false, received_at);
                  channel_work = true;
                }
                Ok(BridgeInbound::Eof(BridgeEof { eid, received_at })) => {
                  this.inbound_drained_total += 1;
                  this.bridge_inbound_inflight.fetch_sub(1, Ordering::Release);
                  this
                    .endpoint
                    .handle_transport_data(eid, &[], true, received_at);
                  channel_work = true;
                }
                Ok(BridgeInbound::Error(BridgeEof { eid, received_at })) => {
                  this.inbound_drained_total += 1;
                  this.bridge_inbound_inflight.fetch_sub(1, Ordering::Release);
                  this.endpoint.handle_transport_error(eid, received_at);
                  channel_work = true;
                }
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => {
                  hit_disconnect = true;
                  break;
                }
              }
            }
          }
          // Pull the resulting machine surfaces to QUIESCENCE; account_event folds
          // every terminal completion into the matching pending join.
          let (_, surf_more, _) = this.drain_surfaces(cx);
          if channel_work || surf_more {
            continue;
          }
          break hit_disconnect;
        };
        if !drained_to_disconnect {
          // The command drain and the shutdown drain loop above both drive the
          // endpoint, so a coalescer drop may have been counted this poll. Publish
          // before parking on the frozen bridges' exit so a `Serf` clone reading the
          // handle from another worker sees the current total, not a stale pre-drain
          // value held until the last bridge re-polls us.
          this
            .shared
            .set_coalesced_user_events_dropped(this.endpoint.coalesced_user_events_dropped());
          this
            .shared
            .set_coalesced_member_events_dropped(this.endpoint.coalesced_member_events_dropped());
          return Poll::Pending;
        }
        drop(this.inbound_rx.take());
        // Close the command queue and fail any still-queued commands.
        for cmd in this.shared.close_and_drain() {
          if let Command::Shutdown(ShutdownCmd { reply }) = cmd {
            // A straggler `Shutdown`: park it too, so it is acked after the socket
            // and listener drop like every other caller.
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
        if let Some(pl) = this.pending_leave.take() {
          pl.resolve_all(|| Err(SerfError::Shutdown));
        }
      }

      // Await the accept task's exit before acking so the TCP listener FD is
      // actually released (not merely signalled).
      if let Some(join) = this.accept_join.as_mut() {
        // Ignoring Ok/Err: only readiness matters — the listener is released once
        // the task has exited, regardless of how.
        if join.poll_unpin(cx).is_pending() {
          // The endpoint was driven earlier this poll (command drain + shutdown drain
          // loop), so publish before parking on the accept task's exit: the wait can be
          // arbitrarily long, and a `Serf` clone reading the handle from another worker
          // must not see a stale pre-drain total until the task finally re-polls us.
          this
            .shared
            .set_coalesced_user_events_dropped(this.endpoint.coalesced_user_events_dropped());
          this
            .shared
            .set_coalesced_member_events_dropped(this.endpoint.coalesced_member_events_dropped());
          return Poll::Pending;
        }
        this.accept_join = None;
      }
      // The coalescer drop counters are cumulative; publish them a final time as the
      // driver future completes so the drops shed in the last command drain — an
      // overflow immediately followed by a `Shutdown` in the same pass — are not lost
      // from the public total. This MUST precede every shutdown completion signal
      // below (the stashed replies and the completion latch): a `shutdown().await`
      // caller released first could resume on another worker and read the stale
      // pre-drain total before these stores land. The normal per-poll publish below
      // is skipped once this shutdown branch returns, so this store is the
      // completeness backstop.
      this
        .shared
        .set_coalesced_user_events_dropped(this.endpoint.coalesced_user_events_dropped());
      this
        .shared
        .set_coalesced_member_events_dropped(this.endpoint.coalesced_member_events_dropped());
      // The bind address is now free. Ack the stashed replies and release any late
      // `shutdown()` caller parked on the completion latch, then stop.
      for reply in this.shutdown_reply.drain(..) {
        // Ignoring Err: the caller dropped its reply receiver.
        let _ = reply.send(Ok(()));
      }
      this.shared.mark_shutdown_complete();
      return Poll::Ready(());
    }

    // Receive gossip (bounded; a full batch means more may be waiting). The socket
    // is always `Some` here — the shutdown branch above (which takes it) returned
    // before reaching this point. `Poll::Pending` from the socket IS the
    // kernel-empty signal (the readiness analogue of serf-compio's completion
    // reap), so the recv-loop stops. A full batch sets `more`, which defers the
    // single `handle_timeout` site below to a later, quiescent poll.
    let mut recv_n = 0;
    while recv_n < this.iter_drain_cap {
      let Some(socket) = this.socket.as_ref() else {
        break;
      };
      match socket.poll_recv_from(cx, &mut this.recv_buf) {
        Poll::Ready(Ok((n, src))) => {
          this.endpoint.handle_gossip(src, &this.recv_buf[..n], now);
          recv_n += 1;
        }
        // Ignoring Err: a transient recv error is non-fatal; re-armed next poll.
        Poll::Ready(Err(_)) => break,
        Poll::Pending => break,
      }
    }
    if recv_n > 0 {
      progress = true;
    }
    // Stage 1 of the UDP path: a full recv batch means the kernel may hold more.
    // Folded (with the ingress-decode stage) into `udp_backlog`, which gates ONLY
    // the SWIM staleness plane — never the reliable reap.
    let recv_capped = recv_n == this.iter_drain_cap;
    more |= recv_capped;

    // Accept inbound connections. Aux tasks wake the driver after enqueueing, so
    // `try_recv` (no waker registration) is sufficient.
    while let Ok((stream, peer)) = this.accepted_rx.try_recv() {
      if this.handle_accepted(stream, peer, now) {
        progress = true;
      }
    }

    // Dial outcomes.
    while let Ok(status) = this.dial_rx.try_recv() {
      this.handle_dial_status(status);
      progress = true;
    }

    // Inbound transport bytes/EOF from the bridge read tasks (bounded per poll; a
    // full batch self-wakes via `more`).
    let mut inbound_n = 0;
    while inbound_n < this.iter_drain_cap {
      let Some(inbound_rx) = this.inbound_rx.as_ref() else {
        break;
      };
      let Ok(msg) = inbound_rx.try_recv() else {
        break;
      };
      inbound_n += 1;
      // Advance the reliable-plane drain counter and clear the frame's in-flight
      // reservation BEFORE folding it, so the watermark reflects post-drain state.
      this.inbound_drained_total += 1;
      this.bridge_inbound_inflight.fetch_sub(1, Ordering::Release);
      this.dispatch_bridge_inbound(msg);
    }
    if inbound_n > 0 {
      progress = true;
    }
    if inbound_n == this.iter_drain_cap {
      more = true;
    }

    // Drain machine surfaces (bounded per surface; the event surface uncapped).
    let (drained, drain_more, ingress_capped) = this.drain_surfaces(cx);
    progress |= drained;
    more |= drain_more;
    // A conflict `Event::Shutdown` observed during the drain flips the shutdown
    // latch; self-wake so the next poll enters the teardown branch.
    if this.shared.is_shutdown() {
      more = true;
    }

    // Timer + deadline reaps under two RESIDENCE-SCOPED gates (replacing the old
    // fixed-count deferral, which fired prematurely at a low `iter_drain_cap` or a
    // large exchange, and could starve under a flood). Every join/leave-resolving
    // input rides the reliable `inbound_rx` FIFO; a UDP gossip flood deposits ZERO
    // there. So the reliable reaps + the reliable-exchange deadlines in
    // `handle_timeout` gate on the exact `inbound_rx` backlog DEPTH — snapshotted
    // to cover the LATEST due deadline (`reap_watermark`) — while the refutable SWIM
    // suspicion tick, whose Ack rides only the pinned UDP path, fires bounded-early
    // after a wall-clock staleness grace.
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
    // has an elapsed SWIM / reliable-exchange deadline"; a bare idle wake is not due
    // and takes the idle arm below.
    let ep_due = endpoint_deadline <= now;
    let reap_due = reap_deadline.is_some_and(|d| d <= now);

    // Snapshot the reliable backlog target to cover the LATEST currently-due
    // deadline, not merely the first that went due. `inbound_rx` is FIFO and grows
    // only with time, so the tail depth at the greatest due deadline dominates
    // every earlier due deadline's pre-deadline completions. Re-snapshot to the
    // live inbound depth (queued in `inbound_rx` OR parked on a saturated bridge
    // hand-off) whenever that max due deadline advances past what the watermark
    // already covers — the endpoint contributes its earliest deadline (`poll_timeout`
    // exposes only the min), each still-unreplied join and the leave its own — so a
    // later overlapping join/leave deadline re-arms a wider target rather than
    // reaping its still-buffered completion against the first deadline's mark.
    let max_due = {
      let mut m = ep_due.then_some(endpoint_deadline);
      if let Some(d) = this.max_due_join_leave_deadline(now) {
        m = Some(m.map_or(d, |cur| cur.max(d)));
      }
      m
    };
    match max_due {
      Some(md) => {
        if this.reap_watermark.is_none() || this.reap_watermark_covers.is_none_or(|c| md > c) {
          let (queued, parked) = this.inbound_backlog_watermark_terms();
          this.reap_watermark = Some(this.inbound_drained_total + queued + parked);
          this.reap_watermark_covers = Some(md);
        }
      }
      None => {
        this.reap_watermark = None;
        this.reap_watermark_covers = None;
        this.swim_stall_since = None;
      }
    }
    // GATE 1 (reliable plane): the pre-deadline backlog has drained. FIFO ⇒ a
    // completion buffered at the deadline is folded (resolving its join/leave)
    // before the counter reaches the watermark; the mark advances only on a new
    // deadline crossing, never on a live append ⇒ a concurrent flood (UDP, or
    // bridge appends behind the mark) never pushes it away, so the pump drains to
    // it in bounded polls with no force-fire.
    let tcp_clear = this
      .reap_watermark
      .is_none_or(|w| this.inbound_drained_total >= w);
    // Both UDP stages: a full recv batch (kernel may hold more) or a capped ingress
    // decode. Gates ONLY the SWIM plane.
    let udp_backlog = recv_capped || ingress_capped;
    // Anchor the SWIM staleness grace the first poll the tick is held back purely by
    // a pinned UDP path (reliable backlog already clear, nothing else due).
    if ep_due && !reap_due && tcp_clear && udp_backlog {
      this.swim_stall_since.get_or_insert(now);
    }
    // GATE 2 (SWIM plane): fire once the UDP path drained this poll, OR the staleness
    // grace elapsed. A reap forcing the shared `handle_timeout` also satisfies it
    // (`reap_due` short-circuits below).
    let swim_ok = !udp_backlog
      || this
        .swim_stall_since
        .is_some_and(|t| now.saturating_duration_since(t) >= SWIM_STALENESS_GRACE);
    let fire = tcp_clear && (ep_due || reap_due) && (reap_due || swim_ok);

    if fire {
      // Reliable backlog folded: fire the coordinator's elapsed deadlines, then fold
      // the UNCAPPED terminal events they emit (`LeftCluster` / `ExchangeCompleted`)
      // BEFORE the reaps — a same-poll leave whose `LeftCluster` is unfolded would
      // otherwise reap a false `LeaveTimeout` — then reap the deadline residue (a
      // no-op for a resolve already folded here).
      if ep_due {
        this.endpoint.handle_timeout(now);
      }
      while let Some(ev) = this.endpoint.poll_event() {
        this.send_observation(ev);
      }
      this.reap_pending_joins(now);
      this.reap_pending_leave(now);
      this.reap_watermark = None;
      this.reap_watermark_covers = None;
      this.swim_stall_since = None;
      progress = true;
      more = true;
    } else if ep_due || reap_due {
      // A deadline is due but its gate is not yet satisfied (reliable backlog still
      // draining, or the SWIM plane inside its grace). DEFER: self-wake and re-poll
      // so step-6 drains `inbound_rx` toward the watermark / the grace elapses. No
      // timer is armed — the deadline already elapsed, so the `more` self-wake alone
      // re-polls (no lost wakeup), and the exact watermark guarantees termination.
      more = true;
    } else {
      // Idle: nothing due. Arm + poll the sleep for the next deadline; NO self-wake
      // (return `Pending` — the armed sleep, a bridge `wake_driver`, or a socket
      // readiness re-polls us).
      let target = reap_deadline.map_or(endpoint_deadline, |d| d.min(endpoint_deadline));
      this.arm_timer(target, now);
      if let Some(timer) = this.timer.as_mut()
        && timer.as_mut().poll(cx).is_ready()
      {
        // The sleep elapsed at/just after arming: clear it and self-wake so the next
        // poll re-evaluates with `now` advanced and fires through the gates above.
        this.timer = None;
        this.timer_deadline = None;
        more = true;
      }
    }

    // Republish the snapshot whenever the pump made progress (the serf endpoint
    // exposes no cheap version stamp, so — as in serf-compio — a productive poll
    // rebuilds and republishes the observable membership).
    if progress {
      this.refresh_snapshot();
    }

    // Republish the endpoint's coalescer drop counters for the handle: the driver
    // owns the endpoint, so these cumulative reads are unreachable from a `Serf`
    // clone otherwise. Monotonic, single writer, so storing the latest value once
    // per poll is exact.
    this
      .shared
      .set_coalesced_user_events_dropped(this.endpoint.coalesced_user_events_dropped());
    this
      .shared
      .set_coalesced_member_events_dropped(this.endpoint.coalesced_member_events_dropped());

    // Yield to other tasks, but re-poll promptly while work remains.
    if more {
      cx.waker().wake_by_ref();
    }
    Poll::Pending
  }
}

/// Apply one terminal `ExchangeCompleted` to its await-result join waiter (if
/// any): remove `eid` from `pending`, push the peer into `contacted` on success,
/// resolve the caller's reply the instant `pending` empties (ahead of the obs
/// hand-off, so a slow delegate cannot delay it), and once fully done clear the
/// still-recorded ignore-join streams and reap the waiter.
fn complete_join_exchange<I, T, G, SR>(
  endpoint: &mut StreamEndpoint<I, SocketAddr, T, G, SR>,
  pending_joins: &mut Vec<PendingJoin>,
  eid: ExchangeId,
  peer: SocketAddr,
  succeeded: bool,
) where
  I: memberlist_proto::Id + Clone,
  T: StreamTransport,
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

/// Accepts inbound TCP connections and forwards each to the pump, waking it after
/// each enqueue. `accept` is async-only, so this cannot fold into the pump's poll;
/// it stops promptly when the driver drops `shutdown_rx`'s sender, which cancels
/// the pending `accept()` so the listener is released.
pub(crate) async fn accept_task<I, L>(
  listener: L,
  accepted_tx: Sender<(L::Stream, SocketAddr)>,
  shutdown_rx: Receiver<()>,
  shared: Arc<Shared<I>>,
) where
  I: Send + Sync + 'static,
  L: TcpListener,
{
  loop {
    select! {
      conn = listener.accept().fuse() => match conn {
        Ok((stream, peer)) => {
          // Bounded channel: await space (accept backpressure). Race the send
          // against shutdown so a full queue at shutdown cannot wedge the task.
          select! {
            res = accepted_tx.send_async((stream, peer)).fuse() => {
              if res.is_err() {
                break;
              }
              shared.wake_driver();
            }
            _ = shutdown_rx.recv_async().fuse() => break,
          }
        }
        // Ignoring Err: a transient accept error (e.g. a reset mid-handshake) is
        // non-fatal; keep listening for the next connection.
        Err(_) => continue,
      },
      // The driver dropped its shutdown sender: stop and release the listener.
      _ = shutdown_rx.recv_async().fuse() => break,
    }
  }
}

/// Dials `peer` for outbound exchange `eid`, bounded by `dial_timeout`, and
/// reports the outcome to the pump, waking it afterwards.
async fn dial_task<I, R>(
  eid: ExchangeId,
  peer: SocketAddr,
  dial_timeout: Duration,
  out_rx: Receiver<BridgeOut>,
  cancel_rx: oneshot::Receiver<()>,
  dial_tx: Sender<DialStatus<R>>,
  shared: Arc<Shared<I>>,
) where
  I: Send + Sync + 'static,
  R: Runtime,
{
  let outcome = match <R::Net as Net>::TcpStream::connect_timeout(&peer, dial_timeout).await {
    Ok(stream) => DialStatus::Connected(DialConnected {
      eid,
      stream,
      out_rx,
      cancel_rx,
    }),
    Err(_) => DialStatus::Failed(DialFailed {
      eid,
      received_at: Instant::now(),
    }),
  };
  // Ignoring Err: the pump dropped its receiver (driver shut down).
  let _ = dial_tx.send(outcome);
  shared.wake_driver();
}

/// The observation task: drains machine events off the pump, invokes the
/// [`Delegate`] hooks, and forwards every serf event to the
/// [`EventStream`](crate::EventStream). Every serf event is the application's
/// observation surface, so all are forwarded to subscribers; the forward is
/// best-effort (a full queue drops + counts, never blocks).
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
  use std::panic::AssertUnwindSafe;
  while let Ok(ev) = obs_rx.recv_async().await {
    // Reclaim the byte-backstop budget this event occupied, before the (possibly
    // slow) delegate hook, so the pump's enqueue side sees it promptly.
    let payload = observation_payload_bytes(&ev);
    if let Some(bytes) = payload {
      obs_payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
    // Contain a panicking delegate hook so the task survives and keeps releasing
    // the reservations of still-queued events. Ignoring the unwind result: the
    // panic is contained and the event is still forwarded to subscribers below.
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

/// Set up the stream driver: spawn the observation task and the accept task, then
/// build the [`StreamDriver`] future (with the periodic schedulers armed). The
/// caller (`Transport::run`) awaits the returned future.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_stream_driver<I, R, T, D, G, SR>(
  mut endpoint: StreamEndpoint<I, SocketAddr, T, G, SR>,
  gossip_socket: <R::Net as Net>::UdpSocket,
  listener: <R::Net as Net>::TcpListener,
  shared: Arc<Shared<I>>,
  events_tx: Sender<Event<I, SocketAddr>>,
  delegate: D,
  driver_opts: RuntimeOptions,
  stream_opts: StreamTransportOptions,
  label: Option<Bytes>,
  stream_timeout: Duration,
  #[cfg(encryption)] keyring: Arc<dyn KeyringDelegate>,
) -> StreamDriver<I, R, T, G, SR>
where
  I: memberlist_proto::Id + Clone + Send + Sync + Unpin + 'static,
  R: Runtime,
  T: StreamTransport,
  D: Delegate<Id = I, Address = SocketAddr>,
  G: rand::Rng,
  SR: rand::Rng + SeedableRng,
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

  // Inbound connections arrive on a dedicated accept task (accept is async); it is
  // cancelled when the driver drops `accept_shutdown_tx`. Retain its join handle
  // (spawn, not spawn_detach) so the driver AWAITS its exit on shutdown before
  // acking — the listener FD lives in the task and is released only when it exits.
  let (accepted_tx, accepted_rx) = flume::bounded(ACCEPT_CAP);
  let (accept_shutdown_tx, accept_shutdown_rx) = flume::bounded(1);
  let accept_join = R::spawn(accept_task::<I, <R::Net as Net>::TcpListener>(
    listener,
    accepted_tx,
    accept_shutdown_rx,
    shared.clone(),
  ));

  StreamDriver::<I, R, T, G, SR>::new(
    endpoint,
    gossip_socket,
    shared,
    obs_tx,
    obs_payload_bytes,
    obs_payload_budget,
    accepted_rx,
    accept_shutdown_tx,
    accept_join,
    driver_opts,
    stream_opts,
    label,
    stream_timeout,
    #[cfg(encryption)]
    keyring,
  )
}

#[cfg(all(test, feature = "tokio"))]
mod tests;
