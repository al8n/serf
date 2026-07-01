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
use serf_proto::{KeyRequestOperation, KeyResponseArgs, event::KeyRequest};

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
  idle_wake: Duration,
  leave_timeout: Duration,
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
      idle_wake: driver_opts.idle_wake_interval(),
      leave_timeout: driver_opts.leave_timeout(),
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
          self.endpoint.set_tags(tags).map_err(SerfError::from)
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

  /// Drains each machine surface up to `iter_drain_cap` items in one pass.
  /// Returns `(worked, more)`: whether any surface produced work, and whether any
  /// surface hit its cap with work left (self-wake).
  fn drain_surfaces(&mut self, cx: &mut Context<'_>) -> (bool, bool)
  where
    I: Send + Sync + 'static,
  {
    let now = Instant::now();
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
    more |= ingress == budget;

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

    // Observation events: retry the overflow first, then drain up to the budget.
    self.flush_obs_overflow();
    let mut events = 0;
    while events < budget {
      let Some(ev) = self.endpoint.poll_event() else {
        break;
      };
      events += 1;
      self.send_observation(ev);
    }
    worked |= events > 0;
    more |= events == budget;

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
      let resp = apply_key_request(&*self.keyring, req);
      // Ignoring Err: `respond_key` fails only when the response cannot be routed;
      // the key op has already applied locally.
      let _ = self.endpoint.respond_key(req, resp, Instant::now());
    }
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
                  this
                    .endpoint
                    .handle_transport_data(eid, &bytes, false, received_at);
                  channel_work = true;
                }
                Ok(BridgeInbound::Eof(BridgeEof { eid, received_at })) => {
                  this
                    .endpoint
                    .handle_transport_data(eid, &[], true, received_at);
                  channel_work = true;
                }
                Ok(BridgeInbound::Error(BridgeEof { eid, received_at })) => {
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
          let (_, surf_more) = this.drain_surfaces(cx);
          if channel_work || surf_more {
            continue;
          }
          break hit_disconnect;
        };
        if !drained_to_disconnect {
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
          return Poll::Pending;
        }
        this.accept_join = None;
      }
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
    if recv_n == this.iter_drain_cap {
      more = true;
    }

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
      this.dispatch_bridge_inbound(msg);
    }
    if inbound_n > 0 {
      progress = true;
    }
    if inbound_n == this.iter_drain_cap {
      more = true;
    }

    // Drain machine surfaces (bounded per surface).
    let (drained, drain_more) = this.drain_surfaces(cx);
    progress |= drained;
    more |= drain_more;
    // A conflict `Event::Shutdown` observed during the drain flips the shutdown
    // latch; self-wake so the next poll enters the teardown branch.
    if this.shared.is_shutdown() {
      more = true;
    }

    // Timer + deadline reaps, gated on quiescence (`!more`). While `more` — the UDP
    // recv loop or the bridge-inbound loop hit its per-poll `iter_drain_cap`, or a
    // machine surface still had queued work — a ready pre-deadline datagram or
    // completion may sit BEHIND that cap, undrained. Firing `handle_timeout` (or a
    // deadline reap) now could time out a probe / await-result join / graceful
    // leave whose resolving Ack / ExchangeCompleted / LeftCluster is already
    // waiting one poll behind, yielding false suspicion, a spurious `JoinAllFailed`,
    // or a `LeaveTimeout`. The `more` self-wake below re-polls and drains that work
    // first; the single `handle_timeout` site and the join / leave deadline reaps
    // run only once the socket is drained to `Poll::Pending` and `drain_surfaces`
    // is quiescent. The kernel buffer is finite and each poll makes `iter_drain_cap`
    // progress before re-polling, so this defers the timer without starving it.
    if !more {
      // Fire an overdue deadline inline (the single `handle_timeout` site), else arm
      // + poll the sleep. Fold in the earliest pending-join / -leave deadline so a
      // parked waiter's timeout fires even when the coordinator has no nearer one.
      let endpoint_deadline = this
        .endpoint
        .poll_timeout()
        .map(|d| d.min(now + this.idle_wake))
        .unwrap_or(now + this.idle_wake);
      let target = [
        Some(endpoint_deadline),
        this.min_pending_join_deadline(),
        this.min_pending_leave_deadline(),
      ]
      .into_iter()
      .flatten()
      .min()
      .unwrap_or(endpoint_deadline);
      if target <= now {
        this.endpoint.handle_timeout(now);
        progress = true;
        more = true;
      } else {
        this.arm_timer(target, now);
        if let Some(timer) = this.timer.as_mut()
          && timer.as_mut().poll(cx).is_ready()
        {
          this.endpoint.handle_timeout(Instant::now());
          this.timer = None;
          this.timer_deadline = None;
          progress = true;
          more = true;
        }
      }

      // Reap deadline-expired join / leave waiters (a fired `handle_timeout` may
      // have completed exchanges; the deadline path resolves the rest).
      this.reap_pending_joins(now);
      this.reap_pending_leave(now);
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

/// Apply one inbound [`KeyRequest`] to the driver's keyring delegate, producing
/// the [`KeyResponseArgs`] the pump forwards to `respond_key`.
#[cfg(encryption)]
fn apply_key_request<I, A>(
  keyring: &dyn KeyringDelegate,
  req: &KeyRequest<I, A>,
) -> KeyResponseArgs {
  match (req.op(), req.key()) {
    (KeyRequestOperation::Install, Some(key)) => keyring.install(*key),
    (KeyRequestOperation::Use, Some(key)) => keyring.use_key(*key),
    (KeyRequestOperation::Remove, Some(key)) => keyring.remove(*key),
    (KeyRequestOperation::List, _) => keyring.list(),
    (_, None) => KeyResponseArgs {
      result: false,
      message: "key-management request missing its required key".into(),
      keys: Vec::new(),
      primary_key: None,
    },
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
    #[cfg(encryption)]
    keyring,
  )
}

#[cfg(all(test, feature = "tokio"))]
mod tests;
