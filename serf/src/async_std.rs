pub use memberlist::agnostic::async_std::AsyncStdRuntime;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tcp`](memberlist::net::stream_layer::tcp::Tcp) stream layer with `async-std` runtime.
#[cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm"))))
)]
pub type AsyncStdTcpSerf<I, A, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tcp::Tcp<AsyncStdRuntime>,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tls`](memberlist::net::stream_layer::tls::Tls) stream layer with `async-std` runtime.
#[cfg(all(feature = "tls", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "tls", not(target_family = "wasm")))))]
pub type AsyncStdTlsSerf<I, A, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tls::Tls<AsyncStdRuntime>,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`Quinn`](memberlist::quic::stream_layer::quinn::Quinn) stream layer with `async-std` runtime.
#[cfg(all(feature = "quinn", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "quinn", not(target_family = "wasm")))))]
pub type AsyncStdQuicSerf<I, A, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::quinn::Quinn<AsyncStdRuntime>,
    AsyncStdRuntime,
  >,
  D,
>;
