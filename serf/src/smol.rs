pub use memberlist::agnostic::smol::SmolRuntime;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tcp`](memberlist::net::stream_layer::tcp::Tcp) stream layer with `smol` runtime.
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
pub type SmolTcpSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tcp::Tcp<SmolRuntime>,
    W,
    SmolRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tls`](memberlist::net::stream_layer::tls::Tls) stream layer with `smol` runtime.
#[cfg(all(feature = "tls", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "tls", not(target_family = "wasm")))))]
pub type SmolTlsSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::tls::Tls<SmolRuntime>,
    W,
    SmolRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`NativeTls`](memberlist::net::stream_layer::native_tls::NativeTls) stream layer with `smol` runtime.
#[cfg(all(feature = "native-tls", not(target_family = "wasm")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(feature = "native-tls", not(target_family = "wasm"))))
)]
pub type SmolNativeTlsSerf<I, A, W, D> = serf_core::Serf<
  memberlist::net::NetTransport<
    I,
    A,
    memberlist::net::stream_layer::native_tls::NativeTls<SmolRuntime>,
    W,
    SmolRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`Quinn`](memberlist::quic::stream_layer::quinn::Quinn) stream layer with `smol` runtime.
#[cfg(all(feature = "quinn", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "quinn", not(target_family = "wasm")))))]
pub type SmolQuicSerf<I, A, W, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::quinn::Quinn<SmolRuntime>,
    W,
    SmolRuntime,
  >,
  D,
>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`S2n`](memberlist::quic::stream_layer::s2n::S2n) stream layer with `smol` runtime.
#[cfg(all(feature = "s2n", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "s2n", not(target_family = "wasm")))))]
pub type SmolS2nSerf<I, A, W, D> = serf_core::Serf<
  memberlist::quic::QuicTransport<
    I,
    A,
    memberlist::quic::stream_layer::s2n::S2n<SmolRuntime>,
    W,
    SmolRuntime,
  >,
  D,
>;
