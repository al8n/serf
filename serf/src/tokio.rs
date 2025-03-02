pub use memberlist::agnostic::tokio::TokioRuntime;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tcp`](memberlist::net::stream_layer::tcp::Tcp) stream layer with `tokio` runtime.
#[cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm"))))
)]
pub type TokioTcpSerf<I, A, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tcp::Tcp<TokioRuntime>,
    TokioRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tls`](memberlist::net::stream_layer::tls::Tls) stream layer with `tokio` runtime.
#[cfg(all(feature = "tls", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "tls", not(target_family = "wasm")))))]
pub type TokioTlsSerf<I, A, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tls::Tls<TokioRuntime>,
    TokioRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`Quinn`](memberlist::quic::stream_layer::quinn::Quinn) stream layer with `tokio` runtime.
#[cfg(all(feature = "quinn", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "quinn", not(target_family = "wasm")))))]
pub type TokioQuicSerf<I, A, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::quinn::Quinn<TokioRuntime>,
    TokioRuntime,
  >,
  D,
>;
