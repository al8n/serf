pub use memberlist::agnostic::async_std::AsyncStdRuntime;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tcp`](memberlist::net::stream_layer::tcp::Tcp) stream layer with `async-std` runtime.
#[cfg(all(
  any(feature = "tcp", feature = "tls", feature = "native-tls"),
  not(target_family = "wasm")
))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(
    any(feature = "tcp", feature = "tls", feature = "native-tls"),
    not(target_family = "wasm")
  )))
)]
pub type AsyncStdTcpSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tcp::Tcp<AsyncStdRuntime>,
    W,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tls`](memberlist::net::stream_layer::tls::Tls) stream layer with `async-std` runtime.
#[cfg(all(feature = "tls", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "tls", not(target_family = "wasm")))))]
pub type AsyncStdTlsSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tls::Tls<AsyncStdRuntime>,
    W,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`NativeTls`](memberlist::net::stream_layer::native_tls::NativeTls) stream layer with `async-std` runtime.
#[cfg(all(feature = "native-tls", not(target_family = "wasm")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(feature = "native-tls", not(target_family = "wasm"))))
)]
pub type AsyncStdNativeTlsSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::native_tls::NativeTls<AsyncStdRuntime>,
    W,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`Quinn`](memberlist::quic::stream_layer::quinn::Quinn) stream layer with `async-std` runtime.
#[cfg(all(feature = "quinn", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "quinn", not(target_family = "wasm")))))]
pub type AsyncStdQuicSerf<I, A, W, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::quinn::Quinn<AsyncStdRuntime>,
    W,
    AsyncStdRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`S2n`](memberlist::quic::stream_layer::s2n::S2n) stream layer with `async-std` runtime.
#[cfg(all(feature = "s2n", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "s2n", not(target_family = "wasm")))))]
pub type AsyncStdS2nSerf<I, A, W, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::s2n::S2n<AsyncStdRuntime>,
    W,
    AsyncStdRuntime,
  >,
  D,
>;
