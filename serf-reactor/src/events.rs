//! Event stream — the user-facing observation channel for serf events.

#[cfg(any(feature = "tcp", feature = "quic"))]
use flume::r#async::RecvStream;
#[cfg(any(feature = "tcp", feature = "quic"))]
use futures_util::Stream;
#[cfg(any(feature = "tcp", feature = "quic"))]
use serf_proto::event::Event;
#[cfg(any(feature = "tcp", feature = "quic"))]
use std::{
  pin::Pin,
  task::{Context, Poll},
};

/// Stream of serf events. Constructed via [`Serf::events`](crate::Serf::events).
///
/// Generic over the wire id / address types `<I, A>`.
///
/// **Concurrency model:** flume MPMC — multiple `events()` calls each return
/// an independent `EventStream`, but events ROUND-ROBIN between subscribers
/// (NOT broadcast). For single-consumer use this is the right shape.
///
/// **Lossy under backpressure:** the events channel is bounded. When the queue
/// is full the driver drops the newest event rather than block — a slow
/// subscriber must not stall the membership FSM.
///
/// Requires a stream or QUIC transport feature (`tcp` or `quic`).
#[cfg(any(feature = "tcp", feature = "quic"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "tcp", feature = "quic"))))]
pub struct EventStream<I, A>
where
  I: 'static,
  A: 'static,
{
  inner: RecvStream<'static, Event<I, A>>,
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> EventStream<I, A>
where
  I: 'static,
  A: 'static,
{
  /// Wrap a flume receiver into an `EventStream`.
  ///
  /// Consumes the receiver: the resulting stream lives `'static` and owns the
  /// queue handle.
  pub fn new(rx: flume::Receiver<Event<I, A>>) -> Self {
    Self {
      inner: rx.into_stream(),
    }
  }
}

#[cfg(any(feature = "tcp", feature = "quic"))]
impl<I, A> Stream for EventStream<I, A>
where
  I: 'static,
  A: 'static,
{
  type Item = Event<I, A>;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    Pin::new(&mut self.inner).poll_next(cx)
  }
}
