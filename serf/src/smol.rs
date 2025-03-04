pub use memberlist::smol::*;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tcp`](memberlist::net::stream_layer::tcp::Tcp) stream layer with `smol` runtime.
#[cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm")))]
#[cfg_attr(
  docsrs,
  doc(cfg(all(any(feature = "tcp", feature = "tls",), not(target_family = "wasm"))))
)]
pub type SmolTcpSerf<I, A, D> = serf_core::Serf<SmolNetTransport<I, A, SmolTcp>, D>;

/// [`Serf`](super::Serf) type alias for using [`NetTransport`](memberlist::net::NetTransport) and [`Tls`](memberlist::net::stream_layer::tls::Tls) stream layer with `smol` runtime.
#[cfg(all(feature = "tls", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "tls", not(target_family = "wasm")))))]
pub type SmolTlsSerf<I, A, D> = serf_core::Serf<SmolNetTransport<I, A, SmolTls>, D>;

/// [`Serf`](super::Serf) type alias for using [`QuicTransport`](memberlist::quic::QuicTransport) and [`Quinn`](memberlist::quic::stream_layer::quinn::Quinn) stream layer with `smol` runtime.
#[cfg(all(feature = "quinn", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "quinn", not(target_family = "wasm")))))]
pub type SmolQuicSerf<I, A, D> = serf_core::Serf<SmolQuicTransport<I, A, SmolQuinn>, D>;
