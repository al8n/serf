<div align="center">
<h1>serf-reactor</h1>
</div>
<div align="center">

Runtime-agnostic async serf driver over TCP, TLS, and QUIC — drives the Sans-I/O
[`serf-proto`] machine on `tokio` or `smol` via [`agnostic`].

[<img alt="github" src="https://img.shields.io/badge/github-al8n/serf-8da0cb?style=for-the-badge&logo=Github" height="22">][Github-url]
<img alt="LoC" src="https://img.shields.io/endpoint?url=https%3A%2F%2Fgist.githubusercontent.com%2Fal8n%2Fd29ceff54c025fe4e8b144a51efb9324%2Fraw%2Fserf-reactor" height="22">
[<img alt="Build" src="https://img.shields.io/github/actions/workflow/status/al8n/serf/ci-tokio.yml?logo=Github-Actions&style=for-the-badge" height="22">][CI-url]
[<img alt="codecov" src="https://img.shields.io/codecov/c/gh/al8n/serf?style=for-the-badge&token=6R3QFWRWHL&logo=codecov" height="22">][codecov-url]

[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-serf--reactor-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs" height="20">][doc-url]
[<img alt="crates.io" src="https://img.shields.io/crates/v/serf-reactor?style=for-the-badge&logo=rust" height="22">][crates-url]
[<img alt="crates.io" src="https://img.shields.io/crates/d/serf-reactor?color=critical&logo=rust&style=for-the-badge" height="22">][crates-url]
<img alt="license" src="https://img.shields.io/badge/License-MPL%202.0-blue.svg?style=for-the-badge&fontColor=white&logoColor=ffffff&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiPz4KPHN2ZyBpZD0iX+WbvuWxgl8xIiBkYXRhLW5hbWU9IuWbvuWxgiAxIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA0NzMuNDcgMjU1LjEyMiI+CiAgPGRlZnM+CiAgICA8c3R5bGU+CiAgICAgIC5jbHMtMSB7CiAgICAgICAgZmlsbDogI2ZmZjsKICAgICAgICBzdHJva2Utd2lkdGg6IDBweDsKICAgICAgfQogICAgPC9zdHlsZT4KICA8L2RlZnM+CiAgPHBvbHlnb24gY2xhc3M9ImNscy0xIiBwb2ludHM9IjM0MC4wNjUgLjQ4NCAzMzQuMzczIDEuMjA5IDMyOC45MjkgMi4xNzUgMzIzLjczMSAzLjYyOCAzMTguNzggNS4zMTggMzEzLjgzMiA3LjAxMiAzMDkuMTI3IDkuMTkgMzA0LjY3MiAxMS42MDYgMzAwLjQ2NiAxNC4yNjggMjk2LjI1OSAxNy4xNjkgMjkyLjI5NyAyMC4zMTIgMjg4LjU4NiAyMy42OTcgMjg1LjEyIDI3LjMyNiAyODEuOTAyIDMxLjE5NCAyNzguNjg0IDM1LjMwNyAyNzUuOTYyIDM5LjQxNiAyNzMuMjQgNDQuMDExIDI3MC43NjUgNDguNjA3IDI2OC41MzkgNTMuNjg1IDI2Ni41NTkgNDguMzYzIDI2NC4wODMgNDMuMjg1IDI2MS42MDggMzguNDUxIDI1OC42MzkgMzMuODU0IDI1NS40MjIgMjkuNzQ0IDI1MS45NTUgMjUuODc1IDI0OC4yNDQgMjIuMjQ3IDI0NC41MzEgMTguODYzIDI0MC4zMjIgMTUuNzE5IDIzNi4xMTYgMTMuMDU5IDIzMS40MTQgMTAuMzk3IDIyNi45NTkgOC4yMjIgMjIyLjAwOCA2LjI4OCAyMTcuMDU3IDQuNTk0IDIxMi4xMDkgMy4xNDMgMjA2LjkxMSAxLjkzNCAyMDEuNDY0IC45NjggMTkwLjU3NiAwIDE3OS45MzIgMCAxNzQuNzM1IC40ODQgMTY5Ljc4NiAuOTY4IDE2NC44MzUgMS42OTMgMTYwLjEzNCAyLjkwMyAxNTUuNjc4IDQuMTEyIDE1MS4yMjMgNS41NjIgMTQ2Ljc2NyA3LjI1MyAxNDIuODA3IDkuMTkgMTM4LjYwMiAxMS4xMjUgMTM0Ljg4OCAxMy41NDEgMTMxLjE3NSAxNS45NiAxMjcuNzExIDE4LjYxOSAxMjQuMjQ0IDIxLjUyMiAxMjEuMDI5IDI0LjY2NiAxMTguMDU4IDI3LjgwOSAxMTUuMDg3IDMxLjE5NCAxMTIuMzY1IDM0LjgyMiAxMDkuODg5IDM4LjY5MSAxMDcuNjYzIDQyLjU2IDEwNy42NjMgNS4wNzggMCA1LjA3OCAwIDU4Ljc2NCAzMy45MDcgNTguNzY0IDMzLjkwNyAyMDAuNDcgMCAyMDAuNDcgMCAyNTUuMTIyIDE1Ni42NjcgMjU1LjEyMiAxNTYuNjY3IDIwMC40NyAxMDcuNjYzIDIwMC40NyAxMDcuNjYzIDEwOC4zMzcgMTA3LjkwOSAxMDMuMjU5IDEwOC42NTIgOTguNDIxIDEwOS4zOTYgOTMuODI3IDExMC4zODUgODkuNDc0IDExMS42MjMgODUuMTIxIDExMy4xMDcgODEuMjUyIDExNC44NCA3Ny4zODMgMTE2LjgyIDczLjk5OCAxMTkuMDQ3IDcwLjYxMSAxMjEuNzcyIDY3LjcxMSAxMjQuNDk0IDY1LjA1MSAxMjcuNDYxIDYyLjYzMyAxMzAuOTI5IDYwLjQ1NSAxMzQuNjM5IDU4LjUyIDEzOC4zNTIgNTcuMDcgMTQyLjU2MSA1NS44NiAxNDcuMjYzIDU0Ljg5NSAxNTEuOTY1IDU0LjQxIDE1Ny4xNjIgNTQuMTY3IDE2MS4zNzEgNTQuMTY3IDE2NS4zMzEgNTQuNjUxIDE2OS4yOTEgNTUuMzc2IDE3Mi43NTUgNTYuMzQ1IDE3Ni4yMjEgNTcuNTU0IDE3OS40MzkgNTkuMDA1IDE4Mi40MDggNjAuNjk4IDE4NS4xMyA2Mi44NzMgMTg3LjYwNSA2NS4yOTIgMTkwLjA4MSA2Ny45NTIgMTkyLjA2MSA3MC44NTQgMTk0LjA0MSA3NC4yMzkgMTk1LjUyNCA3OC4xMDggMTk3LjAxMSA4MS45NzcgMTk4LjI0OSA4Ni41NzQgMTk5LjIzOCA5MS4xNjcgMTk5Ljk4IDk2LjI0NiAyMDAuNzIyIDEwMS44MDkgMjAwLjk3MSAxMDcuODUyIDIwMC45NzEgMjU1LjEyMiAzMDcuMzk3IDI1NS4xMjIgMzA3LjM5NyAyMDAuNDcgMjczLjQ4NyAyMDAuNDcgMjczLjQ4NyAxMTMuNDE1IDI3My43MzYgMTA4LjMzNyAyNzMuOTgzIDEwMy4yNTkgMjc0LjQ3OCA5OC40MjEgMjc1LjQ2NiA5My44MjcgMjc2LjQ1OCA4OS40NzQgMjc3LjY5NiA4NS4xMjEgMjc5LjE4IDgxLjI1MiAyODAuOTEzIDc3LjM4MyAyODIuODk0IDczLjk5OCAyODUuMTIgNzAuNjExIDI4Ny41OTUgNjcuNzExIDI5MC41NjcgNjUuMDUxIDI5My41MzQgNjIuNjMzIDI5Ny4wMDIgNjAuNDU1IDMwMC40NjYgNTguNTIgMzA0LjQyNSA1Ny4wNyAzMDguNjM1IDU1Ljg2IDMxMy4zMzYgNTQuODk1IDMxOC4wMzggNTQuNDEgMzIzLjIzNSA1NC4xNjcgMzI3LjQ0NCA1NC4xNjcgMzMxLjQwNCA1NC42NTEgMzM1LjM2NCA1NS4zNzYgMzM4LjgyOCA1Ni4zNDUgMzQyLjI5MiA1Ny41NTQgMzQ1LjUwOSA1OS4wMDUgMzQ4LjQ4MSA2MC42OTggMzUxLjIwMyA2Mi44NzMgMzUzLjY3OCA2NS4yOTIgMzU1LjkwNCA2Ny45NTIgMzU4LjEzMyA3MC44NTQgMzYwLjExNCA3NC4yMzkgMzYzLjA4MiA4MS45NzcgMzY0LjMyIDg2LjU3NCAzNjUuMzExIDkxLjE2NyAzNjYuMDUzIDk2LjI0NiAzNjYuNzk1IDEwMS44MDkgMzY3LjA0NCAxMDcuODUyIDM2Ny4wNDQgMjU1LjEyMiA0NzMuNDcgMjU1LjEyMiA0NzMuNDcgMjAwLjQ3IDQzOS41NiAyMDAuNDcgNDM5LjU2IDg2LjMzIDQzOS4zMTMgNzcuNjI0IDQzOC4zMjIgNjkuNjQ1IDQzNi44MzggNjEuOTA3IDQzNC44NTggNTQuNjUxIDQzMi4zODMgNDcuODgyIDQyOS40MTQgNDEuNTk1IDQyNS45NDggMzUuNzg4IDQyMS45ODggMzAuMjI5IDQxNy41MzIgMjUuMzkxIDQxMy4wNzcgMjAuNzk3IDQwNy44OCAxNi45MjggNDAyLjY4MiAxMy4zIDM5Ni45OTEgMTAuMTU3IDM5MS4wNTIgNy4yNTMgMzg0Ljg2MyA1LjA3OCAzNzguNjc0IDMuMTQzIDM3MS45OTMgMS42OTMgMzY1LjU1OCAuNzI1IDM1OC42MjkgMCAzNDUuNzU5IDAgMzQwLjA2NSAuNDg0Ii8+Cjwvc3ZnPg==" height="22">

[<img alt="Discord" src="https://img.shields.io/discord/835936528140206122?style=for-the-badge&logo=discord&logoColor=white&label=Discord&color=7289da" height="22">][discord]

</div>

## Introduction

`serf-reactor` binds serf's pure Sans-I/O machine ([`serf-proto`]) to a
readiness-based ("reactor") async runtime, generic over an [`agnostic`]
`Runtime` so the same driver runs on `tokio` and `smol` with no change to
protocol behavior. It owns the driver task, the command queue, the
observation-delegate dispatch, and the transport plumbing (TCP, TLS, QUIC)
connecting the stateless machine to real I/O.

It is the `Send`/`Arc` sibling of the `!Send` [`serf-compio`] driver — every
shared value is an `Arc` with atomic / `arc_swap` interior mutability, and
the observation-delegate hooks return `Send` futures — and shares
[`serf-driver`]'s engines (the observable snapshot, the append-only
snapshotter, the key-management apply logic) with it, so only the I/O
substrate differs. `serf-reactor` is the primary async serf driver for most
applications; a future [`serf`] umbrella facade will wrap the runtime choice
the way [`memberlist`] already does for [`memberlist-reactor`].

## Installation

```toml
[dependencies]
serf-reactor = { version = "0.5", features = ["tcp", "tokio"] }
serf-proto = "0.5"
```

## Example

```rust,ignore
use core::net::SocketAddr;
use agnostic::tokio::TokioRuntime;
use serf_proto::options::Options as SerfOptions;
use serf_reactor::{
    FirstAddrResolver, MaybeResolved, RuntimeOptions, Serf, SocketAddrResolver,
    TcpTransportOptions, VoidDelegate,
};
use smol_str::SmolStr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let advertise: SocketAddr = "127.0.0.1:7946".parse()?;
    let opts = TcpTransportOptions::<SmolStr, SocketAddr>::new()
        .with_local_id(SmolStr::new("node-a"))
        .with_advertise_addr(MaybeResolved::Resolved(advertise));

    // The runtime is a type parameter; pick `TokioRuntime` or `SmolRuntime`.
    let node = Serf::<SmolStr, SocketAddr, TokioRuntime>::tcp(
        opts, &SocketAddrResolver, &FirstAddrResolver,
        VoidDelegate::<SmolStr, SocketAddr>::new(), RuntimeOptions::new(),
        SerfOptions::new(), None, None, None, // reconnect / merge / snapshot
    )
    .await?;

    let seed: SocketAddr = "127.0.0.1:7947".parse()?;
    // join returns the seed address reached, or errs once the join deadline
    // elapses with no contact.
    let reached = node
        .join(&SocketAddrResolver, MaybeResolved::Resolved(seed), false)
        .await?;
    println!("{} members online (joined via {reached})", node.num_members());

    node.leave().await?;
    Ok(())
}
```

## Feature flags

| Feature | Description |
|---------|-------------|
| `tcp` *(default)* | plain-TCP reliable coordinator |
| `tls` + `tls-rustls-ring` / `-aws-lc-rs` | TLS-over-TCP reliable coordinator via `rustls` |
| `quic` + `quic-rustls-ring` | QUIC reliable coordinator (streams + datagrams) |
| `coordinates` | Vivaldi network coordinate estimation |
| `aes-gcm` / `chacha20-poly1305` | AEAD encryption (gossip + reliable planes) plus serf's key-management surface |
| `tag-regex` *(default)* | regex-backed tag-filter matching |
| `tokio` / `smol` | pull in the concrete [`agnostic`] runtime implementation |
| `serde` | `Serialize` / `Deserialize` on the `*Options` config types |
| `clap` | `clap::Args` on the `*Options` config types (CLI flags + env vars) |
| `tracing` | emit `tracing` spans around the public driver operations |
| `dns` | DNS address resolution (via `hickory-proto`) |
| `getifs` | auto-detect the advertise address from the host's interfaces |

## Design

- A quinn-style poll-pump driver: one spawned task per endpoint pumps the
  [`serf-proto`] super-machine (`StreamEndpoint` / `QuicEndpoint`) — feeding
  inbound packets / timers, draining transmit and event output — over the
  runtime's sockets.
- A command queue + one-shot replies bridge the public `Serf` handle to the
  driver task; membership reads go through a lock-free published snapshot
  and never block the pump.
- The async observation composite (`MemberDelegate` / `UserEventDelegate` /
  `QueryDelegate`) returns `Send` futures fired from the driver task; the
  synchronous `MergeDelegate` push/pull veto and, under encryption, the
  `KeyringDelegate` rotation observer run inline on the pump instead.
- Owns its own `Resolver` / `AdvertiseAddrResolver` boundary, so resolution
  stays outside the Sans-I/O core.

## License

`serf-reactor` is under the terms of the MPL-2.0 license.

See [LICENSE] for details.

Copyright (c) 2025 Al Liu.

Copyright (c) 2013 HashiCorp, Inc.

[`agnostic`]: https://github.com/al8n/agnostic
[`memberlist`]: https://crates.io/crates/memberlist
[`memberlist-reactor`]: https://crates.io/crates/memberlist-reactor
[`serf`]: https://crates.io/crates/serf
[`serf-proto`]: https://crates.io/crates/serf-proto
[`serf-driver`]: https://crates.io/crates/serf-driver
[`serf-compio`]: https://crates.io/crates/serf-compio
[LICENSE]: https://github.com/al8n/serf/blob/main/LICENSE
[Github-url]: https://github.com/al8n/serf/
[CI-url]: https://github.com/al8n/serf/actions/workflows/ci-tokio.yml
[codecov-url]: https://app.codecov.io/gh/al8n/serf/
[doc-url]: https://docs.rs/serf-reactor
[crates-url]: https://crates.io/crates/serf-reactor
[discord]: https://discord.gg/4JyVhKFcrt
