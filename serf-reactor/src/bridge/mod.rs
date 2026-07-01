//! Per-bridge byte-mover task — moves bytes between one reliable exchange's TCP
//! stream and the pump, waking the pump after each inbound enqueue.
//!
//! The `Send`/`agnostic` sibling of serf-compio's `!Send` compio bridge, built to
//! memberlist-reactor's readiness `bridge_task` template: `into_split`, a
//! `select_biased` of cancel-vs-`out_rx`-vs-read, an out-of-band oneshot cancel,
//! and an `R::sleep(close_timeout)` no-progress drain backstop.
//!
//! ## Push/pull half-close lifecycle
//!
//! The reliable exchange is a one-shot request-response. A push/pull peer
//! half-closes (FINs) after sending its half, then reads the reply. So a read EOF
//! retires only the read side; the bridge stays alive to write the reply over the
//! half-open connection and tears down only when the pump drops the handle
//! (disconnecting `out_rx` and `cancel_rx`).
//!
//! ## Graceful close vs hard abort
//!
//! - A graceful `StreamAction::Close` drops the [`BridgeHandle`], disconnecting
//!   `out_rx` and `cancel_rx`. The bridge first drains the `BridgeOut::Data`
//!   already queued (flushing the exchange's final response), then exits on the
//!   `out_rx` disconnect. The `cancel_rx` disconnect does NOT preempt that drain —
//!   it is mapped to a never-resolving future below.
//! - A failed `StreamAction::Abort` (and the shutdown freeze) sends `()` on
//!   `cancel_rx` before dropping the handle. That resolves `cancel_fut`, the
//!   biased-FIRST arm of the both-halves-live read select, so it preempts the
//!   bridge ahead of a racing read — a peer-FIN readable just after the signal is
//!   NOT read and folded into a fabricated EOF — and ahead of a write stalled on
//!   an unresponsive peer, discarding the queued bytes. A cancel-break emits no
//!   EOF; only a real `read == 0` does. An `inbound_tx` send already in flight is
//!   awaited in an arm body, not the select, so it still completes first.
//!
//! ## Graceful-drain backstop (`close_timeout`)
//!
//! A graceful Close has NO remaining cancel path (the handle is gone), so a peer
//! that sent its request+FIN and then STOPPED reading would wedge the post-Close
//! drain forever — leaking this detached task and its socket. The drain is
//! therefore a chunked write loop, and EACH partial write is bounded by a fresh
//! `close_timeout`. Because progress resets the deadline, this is a NO-PROGRESS
//! (idle) timeout, not a cap on total drain duration: a peer that keeps reading —
//! even slowly, so a large frame outlasts `close_timeout` overall — advances on
//! every chunk and never trips it. It fires only when a single partial write
//! makes NO progress for the full `close_timeout`; the bridge is then torn down,
//! dropping the write half so the OS RSTs the stuck stream.

use std::{future::Future, net::Shutdown, sync::Arc, time::Duration};

use agnostic::{Runtime, net::TcpStream};
use flume::{Receiver, Sender};
use futures_channel::oneshot;
use futures_util::{
  AsyncReadExt, AsyncWriteExt, FutureExt,
  future::{FusedFuture, pending},
  pin_mut, select_biased,
};
use memberlist_proto::Instant;

use crate::{
  driver::{
    shared::ExchangeId,
    stream::{BridgeData, BridgeEof, BridgeInbound, BridgeOut},
  },
  shared::Shared,
};

/// Moves bytes between one exchange's TCP stream and the pump, waking it after
/// each inbound enqueue. Reads forward to `inbound_tx`; `out_rx` drives writes
/// and the write half-close.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn bridge_task<I, R, S>(
  stream: S,
  eid: ExchangeId,
  out_rx: Receiver<BridgeOut>,
  cancel_rx: oneshot::Receiver<()>,
  inbound_tx: Sender<BridgeInbound>,
  shared: Arc<Shared<I>>,
  recv_buf_len: usize,
  close_timeout: Duration,
) where
  I: Send + Sync + 'static,
  R: Runtime,
  S: TcpStream,
{
  let (mut read_half, mut write_half) = stream.into_split();
  let mut buf = vec![0u8; recv_buf_len.max(1)];
  let mut read_eof = false;
  let mut write_closed = false;
  // Hoist a single cancel future that resolves ONLY on an explicit abort. A
  // graceful Close drops `cancel_tx` (Err(Canceled)); that is NOT an abort, so
  // map it to a future that never resolves — queued writes then complete and the
  // bridge tears down via the `out_rx` disconnect after draining. Because the
  // cancellation maps to `pending()`, `cancel_fut` never wins the write race on a
  // graceful close, so a write is never dropped mid-flight (no partial-write /
  // duplication hazard); an explicit abort still resolves and preempts.
  let cancel_fut = async {
    match cancel_rx.await {
      Ok(()) => (),
      Err(_) => pending::<()>().await,
    }
  }
  .fuse();
  pin_mut!(cancel_fut);
  loop {
    // A push/pull peer half-closes (FINs) after sending its half, then reads the
    // reply. So a read EOF retires only the read side; the bridge stays alive to
    // write the reply over the half-open connection and tears down only when the
    // pump drops the handle (disconnecting out_rx and cancel_rx).
    if read_eof {
      match out_rx.recv_async().await {
        Ok(BridgeOut::Data(bytes)) => {
          // Tear down on either teardown signal: an explicit abort, OR a drain
          // that makes NO progress for `close_timeout` on a non-reading peer (a
          // graceful Close has no cancel path, so the idle timeout is its only
          // backstop). A slow-but-reading peer resets the deadline each chunk.
          if !write_closed
            && write_cancellable::<_, R, _>(&mut write_half, &bytes, &mut cancel_fut, close_timeout)
              .await
          {
            break;
          }
        }
        Ok(BridgeOut::ShutdownWrite) => {
          // Ignoring Err: half-closing a gone peer is moot.
          let _ = write_half.close().await;
          write_closed = true;
        }
        Err(_) => break,
      }
      continue;
    }
    // Both halves live. Bias the shutdown/abort cancel AHEAD of the read so a
    // freeze (`cancel_tx.send(())` then handle drop) stops this bridge's reads
    // before a racing peer-FIN can be read and folded as a completed EOF: a FIN
    // unread at the freeze instant is genuinely in-flight and must be ABSENT from
    // the shutdown reached set. The `out` arm is also ahead of the read, so a
    // graceful Close (handle drop, no cancel) tears down on the out-channel
    // disconnect rather than a racing late read. An already-read EOF is still
    // preserved: its `inbound_tx` send is awaited in the read arm BODY, not in
    // this select, so once the bridge is parked on that send the cancel cannot
    // preempt it — cancel only wins when the loop comes back to a READ.
    select_biased! {
      // Cancel first: ONLY an explicit abort/freeze (`cancel_tx.send(())`)
      // resolves this future — a graceful-Close handle-drop is mapped to
      // `pending()`. It breaks at once WITHOUT emitting any EOF; only a real
      // `read == 0` below emits one, so a cancel-break never fabricates a seed.
      () = &mut cancel_fut => break,
      out = out_rx.recv_async().fuse() => match out {
        Ok(BridgeOut::Data(bytes)) => {
          // Tear down on an explicit abort OR a drain that makes NO progress for
          // `close_timeout` on a non-reading peer (the graceful-Close backstop).
          // A slow-but-reading peer resets the deadline each chunk and is not
          // timed out.
          if !write_closed
            && write_cancellable::<_, R, _>(
              &mut write_half,
              &bytes,
              &mut cancel_fut,
              close_timeout,
            )
            .await
          {
            break;
          }
        }
        Ok(BridgeOut::ShutdownWrite) => {
          // Ignoring Err: half-closing a gone peer is moot.
          let _ = write_half.close().await;
          write_closed = true;
        }
        // The pump dropped the handle (Close / shutdown): tear down.
        Err(_) => break,
      },
      read = read_half.read(&mut buf).fuse() => match read {
        // A clean `read == 0` (peer half-closed) is a benign EOF anchor; a read
        // ERROR is a transport failure and must NOT take the benign-EOF path (it
        // would falsely complete a one-way UserMessage as success). Both stop
        // this task's reads.
        Ok(0) | Err(_) => {
          // Timestamp at read completion, before any send backpressure.
          let payload = BridgeEof { eid, received_at: Instant::now() };
          let msg = if read.is_err() {
            BridgeInbound::Error(payload)
          } else {
            BridgeInbound::Eof(payload)
          };
          // Bounded channel: await space (backpressure), then wake the pump.
          if inbound_tx.send_async(msg).await.is_err() {
            break;
          }
          shared.wake_driver();
          read_eof = true;
        }
        Ok(n) => {
          let msg = BridgeInbound::Data(BridgeData {
            eid,
            bytes: buf[..n].to_vec(),
            received_at: Instant::now(),
          });
          if inbound_tx.send_async(msg).await.is_err() {
            break;
          }
          shared.wake_driver();
        }
      },
    }
  }
  // Drop the inbound sender BEFORE waking the driver. The shutdown drain reads
  // `inbound_rx` to all-senders-gone via `try_recv` and deliberately does NOT
  // hold a persistent flume `recv_async` waker (a per-poll temporary registers
  // then deregisters on drop), so flume's own last-sender disconnect wakes
  // nothing — the driver future would never be re-polled and `shutdown().await`
  // would hang. Dropping first, then waking, guarantees the re-polled driver
  // observes the disconnect (the drop is release-ordered ahead of the wake). This
  // fires on EVERY exit path — a frozen bridge's out-channel disconnect, an
  // explicit cancel/abort, an inbound-send error, or a read/write error — which
  // is what lets the drain reach Disconnected without awaiting any bridge join.
  drop(inbound_tx);
  shared.wake_driver();
  // Best-effort: ensure the OS socket is fully closed once the bridge exits.
  let _ = <S as TcpStream>::reunite(read_half, write_half).map(|s| s.shutdown(Shutdown::Both));
}

/// Writes `bytes` fully via a chunked loop, racing EACH partial write against TWO
/// backstops.
///
/// 1. The bridge's hoisted `cancel_fut` — resolves ONLY on an explicit
///    `StreamAction::Abort` (a graceful Close maps its handle-drop disconnect to a
///    never-resolving future). Listed FIRST in every iteration so it preempts
///    immediately, even a write stalled mid-frame on an unresponsive peer; on a
///    graceful close the write always wins this arm and the queued bytes flush.
/// 2. An `R::sleep(close_timeout)` re-armed FRESH on every iteration — the
///    backstop for a post-Close drain that has NO remaining cancel path. Because
///    the deadline resets on each partial write, it is a NO-PROGRESS (idle)
///    timeout, not a cap on total write duration: a peer that keeps reading —
///    even slowly — advances on every chunk and never trips it. It fires only
///    when a single partial write makes NO progress for the full `close_timeout`.
///
/// Returns true if the bridge should tear down (drop the write half → RST):
/// aborted, the drain made no progress for `close_timeout`, or a write error /
/// write-zero. Returns false only on a fully-written frame.
async fn write_cancellable<W, R, C>(
  write_half: &mut W,
  bytes: &[u8],
  mut cancel_fut: &mut C,
  close_timeout: Duration,
) -> bool
where
  W: futures_util::io::AsyncWrite + Unpin,
  R: Runtime,
  C: Future<Output = ()> + FusedFuture + Unpin,
{
  let mut written = 0;
  while written < bytes.len() {
    // Re-arm the deadline FRESH each iteration: progress (a non-empty partial
    // write) resets the clock, so this is an idle timeout, not a total-duration
    // cap. A slow-but-reading peer advances every chunk and never trips it.
    let timeout_fut = R::sleep(close_timeout).fuse();
    pin_mut!(timeout_fut);
    let res = select_biased! {
      // Explicit abort only (a disconnect was mapped to `pending()`). Listed
      // first so it can preempt even a write blocked mid-frame on an unresponsive
      // peer, ahead of the timeout backstop.
      _ = cancel_fut => return true,
      // Backstop: no progress on this partial write for the full `close_timeout`
      // (a non-reading peer) → tear down (RST on teardown).
      _ = timeout_fut => return true,
      // Write the unwritten tail; a partial write returns its byte count.
      res = write_half.write(&bytes[written..]).fuse() => res,
    };
    match res {
      // Progress: advance and loop with a fresh deadline.
      Ok(n) if n > 0 => written += n,
      // A zero-byte write makes no progress, and a write error ends the exchange:
      // tear down rather than spin or write further.
      // Ignoring Err: the inbound side observes the same broken socket on its next
      // read; surfacing it here would race the pump's teardown.
      _ => return true,
    }
  }
  false
}
