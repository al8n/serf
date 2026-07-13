<div align="center">

<img src="https://raw.githubusercontent.com/al8n/serf/main/art/logo.png" height = "200px">

<h1>Serf</h1>


</div>
<div align="center">

A highly customable, adaptable, runtime agnostic and WASM/WASI friendly decentralized solution for service discovery and orchestration that is lightweight, highly available, and fault tolerant.

Port and improve [HashiCorp's serf](https://github.com/hashicorp/serf) to Rust.

[<img alt="github" src="https://img.shields.io/badge/github-al8n/serf-8da0cb?style=for-the-badge&logo=Github" height="22">][Github-url]
<img alt="LoC" src="https://img.shields.io/endpoint?url=https%3A%2F%2Fgist.githubusercontent.com%2Fal8n%2Fd29ceff54c025fe4e8b144a51efb9324%2Fraw%2Fserf" height="22">
[<img alt="Build" src="https://img.shields.io/github/actions/workflow/status/al8n/serf/ci-tokio.yml?logo=Github-Actions&style=for-the-badge" height="22">][CI-url]
[<img alt="codecov" src="https://img.shields.io/codecov/c/gh/al8n/serf?style=for-the-badge&token=6R3QFWRWHL&logo=codecov" height="22">][codecov-url]

[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-serf-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs" height="20">][doc-url]
[<img alt="crates.io" src="https://img.shields.io/crates/v/serf?style=for-the-badge&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iaXNvLTg4NTktMSI/Pg0KPCEtLSBHZW5lcmF0b3I6IEFkb2JlIElsbHVzdHJhdG9yIDE5LjAuMCwgU1ZHIEV4cG9ydCBQbHVnLUluIC4gU1ZHIFZlcnNpb246IDYuMDAgQnVpbGQgMCkgIC0tPg0KPHN2ZyB2ZXJzaW9uPSIxLjEiIGlkPSJMYXllcl8xIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHhtbG5zOnhsaW5rPSJodHRwOi8vd3d3LnczLm9yZy8xOTk5L3hsaW5rIiB4PSIwcHgiIHk9IjBweCINCgkgdmlld0JveD0iMCAwIDUxMiA1MTIiIHhtbDpzcGFjZT0icHJlc2VydmUiPg0KPGc+DQoJPGc+DQoJCTxwYXRoIGQ9Ik0yNTYsMEwzMS41MjgsMTEyLjIzNnYyODcuNTI4TDI1Niw1MTJsMjI0LjQ3Mi0xMTIuMjM2VjExMi4yMzZMMjU2LDB6IE0yMzQuMjc3LDQ1Mi41NjRMNzQuOTc0LDM3Mi45MTNWMTYwLjgxDQoJCQlsMTU5LjMwMyw3OS42NTFWNDUyLjU2NHogTTEwMS44MjYsMTI1LjY2MkwyNTYsNDguNTc2bDE1NC4xNzQsNzcuMDg3TDI1NiwyMDIuNzQ5TDEwMS44MjYsMTI1LjY2MnogTTQzNy4wMjYsMzcyLjkxMw0KCQkJbC0xNTkuMzAzLDc5LjY1MVYyNDAuNDYxbDE1OS4zMDMtNzkuNjUxVjM3Mi45MTN6IiBmaWxsPSIjRkZGIi8+DQoJPC9nPg0KPC9nPg0KPGc+DQo8L2c+DQo8Zz4NCjwvZz4NCjxnPg0KPC9nPg0KPGc+DQo8L2c+DQo8Zz4NCjwvZz4NCjxnPg0KPC9nPg0KPGc+DQo8L2c+DQo8Zz4NCjwvZz4NCjxnPg0KPC9nPg0KPGc+DQo8L2c+DQo8Zz4NCjwvZz4NCjxnPg0KPC9nPg0KPGc+DQo8L2c+DQo8Zz4NCjwvZz4NCjxnPg0KPC9nPg0KPC9zdmc+DQo=" height="22">][crates-url]
[<img alt="crates.io" src="https://img.shields.io/crates/d/serf?color=critical&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBzdGFuZGFsb25lPSJubyI/PjwhRE9DVFlQRSBzdmcgUFVCTElDICItLy9XM0MvL0RURCBTVkcgMS4xLy9FTiIgImh0dHA6Ly93d3cudzMub3JnL0dyYXBoaWNzL1NWRy8xLjEvRFREL3N2ZzExLmR0ZCI+PHN2ZyB0PSIxNjQ1MTE3MzMyOTU5IiBjbGFzcz0iaWNvbiIgdmlld0JveD0iMCAwIDEwMjQgMTAyNCIgdmVyc2lvbj0iMS4xIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHAtaWQ9IjM0MjEiIGRhdGEtc3BtLWFuY2hvci1pZD0iYTMxM3guNzc4MTA2OS4wLmkzIiB3aWR0aD0iNDgiIGhlaWdodD0iNDgiIHhtbG5zOnhsaW5rPSJodHRwOi8vd3d3LnczLm9yZy8xOTk5L3hsaW5rIj48ZGVmcz48c3R5bGUgdHlwZT0idGV4dC9jc3MiPjwvc3R5bGU+PC9kZWZzPjxwYXRoIGQ9Ik00NjkuMzEyIDU3MC4yNHYtMjU2aDg1LjM3NnYyNTZoMTI4TDUxMiA3NTYuMjg4IDM0MS4zMTIgNTcwLjI0aDEyOHpNMTAyNCA2NDAuMTI4QzEwMjQgNzgyLjkxMiA5MTkuODcyIDg5NiA3ODcuNjQ4IDg5NmgtNTEyQzEyMy45MDQgODk2IDAgNzYxLjYgMCA1OTcuNTA0IDAgNDUxLjk2OCA5NC42NTYgMzMxLjUyIDIyNi40MzIgMzAyLjk3NiAyODQuMTYgMTk1LjQ1NiAzOTEuODA4IDEyOCA1MTIgMTI4YzE1Mi4zMiAwIDI4Mi4xMTIgMTA4LjQxNiAzMjMuMzkyIDI2MS4xMkM5NDEuODg4IDQxMy40NCAxMDI0IDUxOS4wNCAxMDI0IDY0MC4xOTJ6IG0tMjU5LjItMjA1LjMxMmMtMjQuNDQ4LTEyOS4wMjQtMTI4Ljg5Ni0yMjIuNzItMjUyLjgtMjIyLjcyLTk3LjI4IDAtMTgzLjA0IDU3LjM0NC0yMjQuNjQgMTQ3LjQ1NmwtOS4yOCAyMC4yMjQtMjAuOTI4IDIuOTQ0Yy0xMDMuMzYgMTQuNC0xNzguMzY4IDEwNC4zMi0xNzguMzY4IDIxNC43MiAwIDExNy45NTIgODguODMyIDIxNC40IDE5Ni45MjggMjE0LjRoNTEyYzg4LjMyIDAgMTU3LjUwNC03NS4xMzYgMTU3LjUwNC0xNzEuNzEyIDAtODguMDY0LTY1LjkyLTE2NC45MjgtMTQ0Ljk2LTE3MS43NzZsLTI5LjUwNC0yLjU2LTUuODg4LTMwLjk3NnoiIGZpbGw9IiNmZmZmZmYiIHAtaWQ9IjM0MjIiIGRhdGEtc3BtLWFuY2hvci1pZD0iYTMxM3guNzc4MTA2OS4wLmkwIiBjbGFzcz0iIj48L3BhdGg+PC9zdmc+&style=for-the-badge" height="22">][crates-url]
<img alt="license" src="https://img.shields.io/badge/License-MPL%202.0-blue.svg?style=for-the-badge&fontColor=white&logoColor=ffffff&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiPz4KPHN2ZyBpZD0iX+WbvuWxgl8xIiBkYXRhLW5hbWU9IuWbvuWxgiAxIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA0NzMuNDcgMjU1LjEyMiI+CiAgPGRlZnM+CiAgICA8c3R5bGU+CiAgICAgIC5jbHMtMSB7CiAgICAgICAgZmlsbDogI2ZmZjsKICAgICAgICBzdHJva2Utd2lkdGg6IDBweDsKICAgICAgfQogICAgPC9zdHlsZT4KICA8L2RlZnM+CiAgPHBvbHlnb24gY2xhc3M9ImNscy0xIiBwb2ludHM9IjM0MC4wNjUgLjQ4NCAzMzQuMzczIDEuMjA5IDMyOC45MjkgMi4xNzUgMzIzLjczMSAzLjYyOCAzMTguNzggNS4zMTggMzEzLjgzMiA3LjAxMiAzMDkuMTI3IDkuMTkgMzA0LjY3MiAxMS42MDYgMzAwLjQ2NiAxNC4yNjggMjk2LjI1OSAxNy4xNjkgMjkyLjI5NyAyMC4zMTIgMjg4LjU4NiAyMy42OTcgMjg1LjEyIDI3LjMyNiAyODEuOTAyIDMxLjE5NCAyNzguNjg0IDM1LjMwNyAyNzUuOTYyIDM5LjQxNiAyNzMuMjQgNDQuMDExIDI3MC43NjUgNDguNjA3IDI2OC41MzkgNTMuNjg1IDI2Ni41NTkgNDguMzYzIDI2NC4wODMgNDMuMjg1IDI2MS42MDggMzguNDUxIDI1OC42MzkgMzMuODU0IDI1NS40MjIgMjkuNzQ0IDI1MS45NTUgMjUuODc1IDI0OC4yNDQgMjIuMjQ3IDI0NC41MzEgMTguODYzIDI0MC4zMjIgMTUuNzE5IDIzNi4xMTYgMTMuMDU5IDIzMS40MTQgMTAuMzk3IDIyNi45NTkgOC4yMjIgMjIyLjAwOCA2LjI4OCAyMTcuMDU3IDQuNTk0IDIxMi4xMDkgMy4xNDMgMjA2LjkxMSAxLjkzNCAyMDEuNDY0IC45NjggMTkwLjU3NiAwIDE3OS45MzIgMCAxNzQuNzM1IC40ODQgMTY5Ljc4NiAuOTY4IDE2NC44MzUgMS42OTMgMTYwLjEzNCAyLjkwMyAxNTUuNjc4IDQuMTEyIDE1MS4yMjMgNS41NjIgMTQ2Ljc2NyA3LjI1MyAxNDIuODA3IDkuMTkgMTM4LjYwMiAxMS4xMjUgMTM0Ljg4OCAxMy41NDEgMTMxLjE3NSAxNS45NiAxMjcuNzExIDE4LjYxOSAxMjQuMjQ0IDIxLjUyMiAxMjEuMDI5IDI0LjY2NiAxMTguMDU4IDI3LjgwOSAxMTUuMDg3IDMxLjE5NCAxMTIuMzY1IDM0LjgyMiAxMDkuODg5IDM4LjY5MSAxMDcuNjYzIDQyLjU2IDEwNy42NjMgNS4wNzggMCA1LjA3OCAwIDU4Ljc2NCAzMy45MDcgNTguNzY0IDMzLjkwNyAyMDAuNDcgMCAyMDAuNDcgMCAyNTUuMTIyIDE1Ni42NjcgMjU1LjEyMiAxNTYuNjY3IDIwMC40NyAxMDcuNjYzIDIwMC40NyAxMDcuNjYzIDEwOC4zMzcgMTA3LjkwOSAxMDMuMjU5IDEwOC42NTIgOTguNDIxIDEwOS4zOTYgOTMuODI3IDExMC4zODUgODkuNDc0IDExMS42MjMgODUuMTIxIDExMy4xMDcgODEuMjUyIDExNC44NCA3Ny4zODMgMTE2LjgyIDczLjk5OCAxMTkuMDQ3IDcwLjYxMSAxMjEuNzcyIDY3LjcxMSAxMjQuNDk0IDY1LjA1MSAxMjcuNDYxIDYyLjYzMyAxMzAuOTI5IDYwLjQ1NSAxMzQuNjM5IDU4LjUyIDEzOC4zNTIgNTcuMDcgMTQyLjU2MSA1NS44NiAxNDcuMjYzIDU0Ljg5NSAxNTEuOTY1IDU0LjQxIDE1Ny4xNjIgNTQuMTY3IDE2MS4zNzEgNTQuMTY3IDE2NS4zMzEgNTQuNjUxIDE2OS4yOTEgNTUuMzc2IDE3Mi43NTUgNTYuMzQ1IDE3Ni4yMjEgNTcuNTU0IDE3OS40MzkgNTkuMDA1IDE4Mi40MDggNjAuNjk4IDE4NS4xMyA2Mi44NzMgMTg3LjYwNSA2NS4yOTIgMTkwLjA4MSA2Ny45NTIgMTkyLjA2MSA3MC44NTQgMTk0LjA0MSA3NC4yMzkgMTk1LjUyNCA3OC4xMDggMTk3LjAxMSA4MS45NzcgMTk4LjI0OSA4Ni41NzQgMTk5LjIzOCA5MS4xNjcgMTk5Ljk4IDk2LjI0NiAyMDAuNzIyIDEwMS44MDkgMjAwLjk3MSAxMDcuODUyIDIwMC45NzEgMjU1LjEyMiAzMDcuMzk3IDI1NS4xMjIgMzA3LjM5NyAyMDAuNDcgMjczLjQ4NyAyMDAuNDcgMjczLjQ4NyAxMTMuNDE1IDI3My43MzYgMTA4LjMzNyAyNzMuOTgzIDEwMy4yNTkgMjc0LjQ3OCA5OC40MjEgMjc1LjQ2NiA5My44MjcgMjc2LjQ1OCA4OS40NzQgMjc3LjY5NiA4NS4xMjEgMjc5LjE4IDgxLjI1MiAyODAuOTEzIDc3LjM4MyAyODIuODk0IDczLjk5OCAyODUuMTIgNzAuNjExIDI4Ny41OTUgNjcuNzExIDI5MC41NjcgNjUuMDUxIDI5My41MzQgNjIuNjMzIDI5Ny4wMDIgNjAuNDU1IDMwMC40NjYgNTguNTIgMzA0LjQyNSA1Ny4wNyAzMDguNjM1IDU1Ljg2IDMxMy4zMzYgNTQuODk1IDMxOC4wMzggNTQuNDEgMzIzLjIzNSA1NC4xNjcgMzI3LjQ0NCA1NC4xNjcgMzMxLjQwNCA1NC42NTEgMzM1LjM2NCA1NS4zNzYgMzM4LjgyOCA1Ni4zNDUgMzQyLjI5MiA1Ny41NTQgMzQ1LjUwOSA1OS4wMDUgMzQ4LjQ4MSA2MC42OTggMzUxLjIwMyA2Mi44NzMgMzUzLjY3OCA2NS4yOTIgMzU1LjkwNCA2Ny45NTIgMzU4LjEzMyA3MC44NTQgMzYwLjExNCA3NC4yMzkgMzYzLjA4MiA4MS45NzcgMzY0LjMyIDg2LjU3NCAzNjUuMzExIDkxLjE2NyAzNjYuMDUzIDk2LjI0NiAzNjYuNzk1IDEwMS44MDkgMzY3LjA0NCAxMDcuODUyIDM2Ny4wNDQgMjU1LjEyMiA0NzMuNDcgMjU1LjEyMiA0NzMuNDcgMjAwLjQ3IDQzOS41NiAyMDAuNDcgNDM5LjU2IDg2LjMzIDQzOS4zMTMgNzcuNjI0IDQzOC4zMjIgNjkuNjQ1IDQzNi44MzggNjEuOTA3IDQzNC44NTggNTQuNjUxIDQzMi4zODMgNDcuODgyIDQyOS40MTQgNDEuNTk1IDQyNS45NDggMzUuNzg4IDQyMS45ODggMzAuMjI5IDQxNy41MzIgMjUuMzkxIDQxMy4wNzcgMjAuNzk3IDQwNy44OCAxNi45MjggNDAyLjY4MiAxMy4zIDM5Ni45OTEgMTAuMTU3IDM5MS4wNTIgNy4yNTMgMzg0Ljg2MyA1LjA3OCAzNzguNjc0IDMuMTQzIDM3MS45OTMgMS42OTMgMzY1LjU1OCAuNzI1IDM1OC42MjkgMCAzNDUuNzU5IDAgMzQwLjA2NSAuNDg0Ii8+Cjwvc3ZnPg==" height="22">

[<img alt="github" src="https://img.shields.io/discord/835936528140206122?style=for-the-badge&logo=discord&logoColor=white&label=Discord&color=7289da" height="22">][discord]

</div>

## Introduction

`serf` is a facade over the serf crate family: depend on this one crate, pick a driver
through Cargo features, and get a ready-to-use node — instead of wiring the protocol
core, a transport, and a runtime together yourself. It mirrors the [`memberlist`]
umbrella crate.

Its protocol logic composes serf's own event / query / tag super-machine
([`serf-proto`]) over [`memberlist-proto`]'s SWIM coordinator: memberlist supplies
gossip membership and failure detection, and serf layers per-node tags, custom user
events, and a gossip-relayed query/response protocol — including live key rotation
through the same query mechanism — on top of it. Thin async drivers adapt that pure
core to `tokio` / `smol`, `compio`, and bare-metal `no_std` targets, so the same
protocol logic runs on a server or a microcontroller.

This is a Rust port of [HashiCorp's Serf], extended with a Sans-I/O architecture and
`no_std` / bare-metal support.

For the full project overview — protocol background, cross-crate design rationale,
Q&A, and related projects — see the [project README].

## Highlights

- **Sans-I/O core.** All protocol logic lives in [`serf-proto`] as a pure super-machine
  composed over [`memberlist-proto`]'s SWIM coordinator — no sockets, threads, or
  clocks — making it deterministic and exhaustively unit-tested. The drivers only
  shuttle bytes and time in and out.
- **Runtime-agnostic.** Drive it from `tokio`, `smol`, or `compio` (thread-per-core)
  with no change to protocol behavior.
- **`no_std` and bare-metal.** The core runs on `alloc`, and [`serf-smoltcp`] /
  [`serf-embassy`] bring full serf membership, events, and queries to embedded targets.
- **Membership, events, and queries.** Inherits SWIM gossip membership and failure
  detection from [`memberlist`], and adds per-node tags, custom user events, and a
  gossip-relayed query/response protocol — including live key rotation through the
  same query mechanism.
- **Pluggable transports.** Plain TCP, TLS-over-TCP (`rustls`), or QUIC (`quinn-proto`)
  reliable planes, each with a UDP / datagram gossip plane carrying opt-in AEAD
  encryption.
- **Customizable.** Bring your own `Id`, `Address`, `AddressResolver`, and delegates
  (member / merge / query / user-event / reconnect).
- **Observable, à la carte.** Opt into `tracing` — compiled out when unused.
- **Config-file & CLI friendly.** Every `*Options` type optionally derives `serde` and
  `clap`.

## The family

The crates split protocol logic from I/O, mirroring the `memberlist` layering:

| Crate | Role |
|-------|------|
| [`serf`] | this crate — batteries-included facade (core + default `tokio` driver) |
| [`serf-proto`] | Sans-I/O protocol super-machine + wire codec (`no_std`-capable) |
| [`serf-driver`] | runtime-agnostic glue shared by the reactor and compio drivers |
| [`serf-reactor`] | runtime-agnostic async driver (`tokio` & `smol`), TCP/TLS/QUIC |
| [`serf-compio`] | `compio` (thread-per-core, io_uring / IOCP) async driver |
| [`serf-embedded`] | shared `no_std` driving core for the embedded drivers |
| [`serf-smoltcp`] | executor-free `no_std` driver over smoltcp (caller-poll) |
| [`serf-embassy`] | embassy-net async `no_std` driver, built on [`serf-embedded`] |

## Installation

> **Build requirement:** `serf-proto` and its [`memberlist-proto`] dependency invoke
> [`protoc`][protoc] at build time to generate the wire codec, so the Protocol Buffers
> compiler must be on `PATH` (e.g. `apt install protobuf-compiler`, `brew install
> protobuf`).

```toml
[dependencies]
serf = "0.5" # tokio runtime + tcp transport by default
```

For `smol` instead of `tokio`:

```toml
[dependencies]
serf = { version = "0.5", default-features = false, features = ["smol", "tcp"] }
```

For the `compio` (completion-based, thread-per-core) runtime:

```toml
[dependencies]
serf = { version = "0.5", default-features = false, features = ["compio", "tcp"] }
```

For bare-metal (`no_std`) targets, enable `smoltcp` (the executor-free engine) or
`embassy` (the embassy-net async driver) — neither pulls in `std`:

```toml
[dependencies]
serf = { version = "0.5", default-features = false, features = ["embassy", "tcp"] }
```

The minimum supported Rust version (MSRV) is **1.85.0** (edition 2024); the `smoltcp`,
`embassy`, and `embedded` drivers require **1.96.0**.

## Example

Common types (`Options` variants, delegates, resolvers, …) are re-exported from the
per-driver module, which also pins the runtime, so a `tokio` build reaches its handle
through `serf::tokio` and never names the runtime generic:

```rust,ignore
use serf::tokio::{Serf, SerfOptions, SocketAddrResolver, TcpTransportOptions, VoidDelegate};

// `serf::tokio::Serf` is the reactor handle with its runtime already pinned to tokio,
// so the runtime generic never appears in your code. Build a node with the inherent
// constructors — `Serf::tcp`, `Serf::tls`, `Serf::quic`, and their `*_with_rng`
// variants — then drive the cluster through the returned handle's `join` / `leave` /
// `user_event` / `query` / `install_key` methods.
```

`serf::reactor` re-exports the same driver unpinned (generic over an [`agnostic`]
runtime); `serf::compio`, `serf::smoltcp`, `serf::embassy`, and `serf::embedded` expose
their respective driver crates the same way. The pure protocol core is always available
as `serf::proto` (a re-export of [`serf-proto`]), regardless of which driver feature is
enabled.

## Feature flags

Pick **one** driver, **one or more** transports, and any optional extensions you need.

- **Drivers** — `tokio` *(default)*, `smol`, `compio` (thread-per-core), `reactor`
  (generic over an [`agnostic`] runtime), `smoltcp` / `embassy` / `embedded` (`no_std`).
- **Transports** — `tcp` *(default)*; `tls` + a backend (`tls-rustls-ring`,
  `tls-rustls-aws-lc-rs`); `quic` + a backend (`quic-rustls-ring`).
- **Capability tiers** — `std` and `alloc` are independent; the `no_std` drivers select
  `alloc` instead of `std`.
- **Protocol extensions** — `coordinates` (Vivaldi network coordinate estimation),
  `aes-gcm` / `chacha20-poly1305` (gossip + plain-TCP reliable AEAD encryption, plus
  serf's gossip-driven key-management queries), `encryption` (umbrella for both
  backends).
- **Config** — `serde` (config-file round-trips) and `clap` (CLI flags + env) on the
  `*Options` types; std-only.
- **Other** — `tag-regex` (regex-backed tag-filter matching; falls back to exact-match
  without it), `dns` (DNS address resolver), `getifs` (auto-detect the advertise
  address from local interfaces), `cidr` (CIDR peer-admission allow-list, `no_std`
  drivers), `tracing` (spans around the driver operations) — each forwarded to
  whichever driver is selected.

## Design

Every driver wraps the same pure core: [`serf-proto`] runs entirely without sockets,
threads, or clocks of its own, so protocol behavior is identical no matter which driver
you pick. Drivers only shuttle bytes and time in and out and translate the machine's
outputs into delegate callbacks and an event stream. Swap `tokio` for `smoltcp` and the
membership, failure-detection, tag, event, and query semantics do not change — only the
I/O underneath does.

## License

`serf` is under the terms of the MPL-2.0 license.

See [LICENSE] for details.

Copyright (c) 2025 Al Liu.

Copyright (c) 2013 HashiCorp, Inc.

[HashiCorp's Serf]: https://github.com/hashicorp/serf
[project README]: https://github.com/al8n/serf/blob/main/README.md

[`memberlist`]: https://crates.io/crates/memberlist
[`memberlist-proto`]: https://crates.io/crates/memberlist-proto
[`agnostic`]: https://github.com/al8n/agnostic
[`serf`]: https://crates.io/crates/serf
[`serf-proto`]: https://crates.io/crates/serf-proto
[`serf-driver`]: https://crates.io/crates/serf-driver
[`serf-reactor`]: https://crates.io/crates/serf-reactor
[`serf-compio`]: https://crates.io/crates/serf-compio
[`serf-embedded`]: https://crates.io/crates/serf-embedded
[`serf-smoltcp`]: https://crates.io/crates/serf-smoltcp
[`serf-embassy`]: https://crates.io/crates/serf-embassy
[protoc]: https://grpc.io/docs/protoc-installation/
[LICENSE]: https://github.com/al8n/serf/blob/main/LICENSE
[Github-url]: https://github.com/al8n/serf/
[CI-url]: https://github.com/al8n/serf/actions/workflows/ci-tokio.yml
[doc-url]: https://docs.rs/serf
[crates-url]: https://crates.io/crates/serf
[codecov-url]: https://app.codecov.io/gh/al8n/serf/
[discord]: https://discord.gg/4JyVhKFcrt
