<div align="center">
<h1>serf-proto</h1>
</div>
<div align="center">

Sans-I/O state machine and wire codec for the serf cluster-orchestration protocol —
`no_std`-capable, composed over [`memberlist-proto`]'s reliable coordinator.

[<img alt="github" src="https://img.shields.io/badge/github-al8n/serf-8da0cb?style=for-the-badge&logo=Github" height="22">][Github-url]
<img alt="LoC" src="https://img.shields.io/endpoint?url=https%3A%2F%2Fgist.githubusercontent.com%2Fal8n%2Fd29ceff54c025fe4e8b144a51efb9324%2Fraw%2Fserf-proto" height="22">
[<img alt="Build" src="https://img.shields.io/github/actions/workflow/status/al8n/serf/ci-core.yml?logo=Github-Actions&style=for-the-badge" height="22">][CI-url]
[<img alt="codecov" src="https://img.shields.io/codecov/c/gh/al8n/serf?style=for-the-badge&token=6R3QFWRWHL&logo=codecov" height="22">][codecov-url]

[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-serf--proto-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs" height="20">][doc-url]
[<img alt="crates.io" src="https://img.shields.io/crates/v/serf-proto?style=for-the-badge&logo=rust" height="22">][crates-url]
[<img alt="crates.io" src="https://img.shields.io/crates/d/serf-proto?color=critical&logo=rust&style=for-the-badge" height="22">][crates-url]
<img alt="license" src="https://img.shields.io/badge/License-MPL%202.0-blue.svg?style=for-the-badge&fontColor=white&logoColor=ffffff&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiPz4KPHN2ZyBpZD0iX+WbvuWxgl8xIiBkYXRhLW5hbWU9IuWbvuWxgiAxIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA0NzMuNDcgMjU1LjEyMiI+CiAgPGRlZnM+CiAgICA8c3R5bGU+CiAgICAgIC5jbHMtMSB7CiAgICAgICAgZmlsbDogI2ZmZjsKICAgICAgICBzdHJva2Utd2lkdGg6IDBweDsKICAgICAgfQogICAgPC9zdHlsZT4KICA8L2RlZnM+CiAgPHBvbHlnb24gY2xhc3M9ImNscy0xIiBwb2ludHM9IjM0MC4wNjUgLjQ4NCAzMzQuMzczIDEuMjA5IDMyOC45MjkgMi4xNzUgMzIzLjczMSAzLjYyOCAzMTguNzggNS4zMTggMzEzLjgzMiA3LjAxMiAzMDkuMTI3IDkuMTkgMzA0LjY3MiAxMS42MDYgMzAwLjQ2NiAxNC4yNjggMjk2LjI1OSAxNy4xNjkgMjkyLjI5NyAyMC4zMTIgMjg4LjU4NiAyMy42OTcgMjg1LjEyIDI3LjMyNiAyODEuOTAyIDMxLjE5NCAyNzguNjg0IDM1LjMwNyAyNzUuOTYyIDM5LjQxNiAyNzMuMjQgNDQuMDExIDI3MC43NjUgNDguNjA3IDI2OC41MzkgNTMuNjg1IDI2Ni41NTkgNDguMzYzIDI2NC4wODMgNDMuMjg1IDI2MS42MDggMzguNDUxIDI1OC42MzkgMzMuODU0IDI1NS40MjIgMjkuNzQ0IDI1MS45NTUgMjUuODc1IDI0OC4yNDQgMjIuMjQ3IDI0NC41MzEgMTguODYzIDI0MC4zMjIgMTUuNzE5IDIzNi4xMTYgMTMuMDU5IDIzMS40MTQgMTAuMzk3IDIyNi45NTkgOC4yMjIgMjIyLjAwOCA2LjI4OCAyMTcuMDU3IDQuNTk0IDIxMi4xMDkgMy4xNDMgMjA2LjkxMSAxLjkzNCAyMDEuNDY0IC45NjggMTkwLjU3NiAwIDE3OS45MzIgMCAxNzQuNzM1IC40ODQgMTY5Ljc4NiAuOTY4IDE2NC44MzUgMS42OTMgMTYwLjEzNCAyLjkwMyAxNTUuNjc4IDQuMTEyIDE1MS4yMjMgNS41NjIgMTQ2Ljc2NyA3LjI1MyAxNDIuODA3IDkuMTkgMTM4LjYwMiAxMS4xMjUgMTM0Ljg4OCAxMy41NDEgMTMxLjE3NSAxNS45NiAxMjcuNzExIDE4LjYxOSAxMjQuMjQ0IDIxLjUyMiAxMjEuMDI5IDI0LjY2NiAxMTguMDU4IDI3LjgwOSAxMTUuMDg3IDMxLjE5NCAxMTIuMzY1IDM0LjgyMiAxMDkuODg5IDM4LjY5MSAxMDcuNjYzIDQyLjU2IDEwNy42NjMgNS4wNzggMCA1LjA3OCAwIDU4Ljc2NCAzMy45MDcgNTguNzY0IDMzLjkwNyAyMDAuNDcgMCAyMDAuNDcgMCAyNTUuMTIyIDE1Ni42NjcgMjU1LjEyMiAxNTYuNjY3IDIwMC40NyAxMDcuNjYzIDIwMC40NyAxMDcuNjYzIDEwOC4zMzcgMTA3LjkwOSAxMDMuMjU5IDEwOC42NTIgOTguNDIxIDEwOS4zOTYgOTMuODI3IDExMC4zODUgODkuNDc0IDExMS42MjMgODUuMTIxIDExMy4xMDcgODEuMjUyIDExNC44NCA3Ny4zODMgMTE2LjgyIDczLjk5OCAxMTkuMDQ3IDcwLjYxMSAxMjEuNzcyIDY3LjcxMSAxMjQuNDk0IDY1LjA1MSAxMjcuNDYxIDYyLjYzMyAxMzAuOTI5IDYwLjQ1NSAxMzQuNjM5IDU4LjUyIDEzOC4zNTIgNTcuMDcgMTQyLjU2MSA1NS44NiAxNDcuMjYzIDU0Ljg5NSAxNTEuOTY1IDU0LjQxIDE1Ny4xNjIgNTQuMTY3IDE2MS4zNzEgNTQuMTY3IDE2NS4zMzEgNTQuNjUxIDE2OS4yOTEgNTUuMzc2IDE3Mi43NTUgNTYuMzQ1IDE3Ni4yMjEgNTcuNTU0IDE3OS40MzkgNTkuMDA1IDE4Mi40MDggNjAuNjk4IDE4NS4xMyA2Mi44NzMgMTg3LjYwNSA2NS4yOTIgMTkwLjA4MSA2Ny45NTIgMTkyLjA2MSA3MC44NTQgMTk0LjA0MSA3NC4yMzkgMTk1LjUyNCA3OC4xMDggMTk3LjAxMSA4MS45NzcgMTk4LjI0OSA4Ni41NzQgMTk5LjIzOCA5MS4xNjcgMTk5Ljk4IDk2LjI0NiAyMDAuNzIyIDEwMS44MDkgMjAwLjk3MSAxMDcuODUyIDIwMC45NzEgMjU1LjEyMiAzMDcuMzk3IDI1NS4xMjIgMzA3LjM5NyAyMDAuNDcgMjczLjQ4NyAyMDAuNDcgMjczLjQ4NyAxMTMuNDE1IDI3My43MzYgMTA4LjMzNyAyNzMuOTgzIDEwMy4yNTkgMjc0LjQ3OCA5OC40MjEgMjc1LjQ2NiA5My44MjcgMjc2LjQ1OCA4OS40NzQgMjc3LjY5NiA4NS4xMjEgMjc5LjE4IDgxLjI1MiAyODAuOTEzIDc3LjM4MyAyODIuODk0IDczLjk5OCAyODUuMTIgNzAuNjExIDI4Ny41OTUgNjcuNzExIDI5MC41NjcgNjUuMDUxIDI5My41MzQgNjIuNjMzIDI5Ny4wMDIgNjAuNDU1IDMwMC40NjYgNTguNTIgMzA0LjQyNSA1Ny4wNyAzMDguNjM1IDU1Ljg2IDMxMy4zMzYgNTQuODk1IDMxOC4wMzggNTQuNDEgMzIzLjIzNSA1NC4xNjcgMzI3LjQ0NCA1NC4xNjcgMzMxLjQwNCA1NC42NTEgMzM1LjM2NCA1NS4zNzYgMzM4LjgyOCA1Ni4zNDUgMzQyLjI5MiA1Ny41NTQgMzQ1LjUwOSA1OS4wMDUgMzQ4LjQ4MSA2MC42OTggMzUxLjIwMyA2Mi44NzMgMzUzLjY3OCA2NS4yOTIgMzU1LjkwNCA2Ny45NTIgMzU4LjEzMyA3MC44NTQgMzYwLjExNCA3NC4yMzkgMzYzLjA4MiA4MS45NzcgMzY0LjMyIDg2LjU3NCAzNjUuMzExIDkxLjE2NyAzNjYuMDUzIDk2LjI0NiAzNjYuNzk1IDEwMS44MDkgMzY3LjA0NCAxMDcuODUyIDM2Ny4wNDQgMjU1LjEyMiA0NzMuNDcgMjU1LjEyMiA0NzMuNDcgMjAwLjQ3IDQzOS41NiAyMDAuNDcgNDM5LjU2IDg2LjMzIDQzOS4zMTMgNzcuNjI0IDQzOC4zMjIgNjkuNjQ1IDQzNi44MzggNjEuOTA3IDQzNC44NTggNTQuNjUxIDQzMi4zODMgNDcuODgyIDQyOS40MTQgNDEuNTk1IDQyNS45NDggMzUuNzg4IDQyMS45ODggMzAuMjI5IDQxNy41MzIgMjUuMzkxIDQxMy4wNzcgMjAuNzk3IDQwNy44OCAxNi45MjggNDAyLjY4MiAxMy4zIDM5Ni45OTEgMTAuMTU3IDM5MS4wNTIgNy4yNTMgMzg0Ljg2MyA1LjA3OCAzNzguNjc0IDMuMTQzIDM3MS45OTMgMS42OTMgMzY1LjU1OCAuNzI1IDM1OC42MjkgMCAzNDUuNzU5IDAgMzQwLjA2NSAuNDg0Ii8+Cjwvc3ZnPg==" height="22">

[<img alt="Discord" src="https://img.shields.io/discord/835936528140206122?style=for-the-badge&logo=discord&logoColor=white&label=Discord&color=7289da" height="22">][discord]

</div>

## Introduction

`serf-proto` implements serf's cluster-orchestration layer — join/leave intents,
user events, queries and query responses, push-pull anti-entropy, and (optionally)
Vivaldi network coordinates — as a deterministic state machine that performs **no
I/O of its own**. It depends on [`memberlist-proto`] for the `Data`/`DataRef` codec
primitives and defines serf's own message set on top of them: a [`buffa`]-generated
protobuf wire codec (`[TAG_BYTE][VARINT_LEN][BUFFA_BODY]` framing) plus the typed
bridge that converts between serf's Rust-native message shapes and the generated
codec types.

Serf logic composes with a memberlist reliable coordinator into one Sans-I/O
super-machine, in the shape [`quinn-proto`] popularized: feed inbound bytes and
timer ticks in through `handle_*` / `handle_timeout`, drain outbound transmits and
events out through `poll_transmit` / `poll_event`. `StreamEndpoint` pairs the
serf-logic core with memberlist's TCP/TLS stream coordinator; `QuicEndpoint` pairs
the same core with memberlist's QUIC coordinator. Both hold the core and the
coordinator as disjoint fields and reach the coordinator only through a narrow
`Reliable` seam, so the serf-logic core itself never names a concrete transport.

This crate is the core the driver crates are thin layers over. [`serf-driver`] holds
the runtime-independent glue shared by the async drivers; [`serf-reactor`] (tokio /
smol) and [`serf-compio`] (compio) bind it to a real runtime; the bare-metal
`serf-embedded` stack drives it on `serf-smoltcp` / `serf-embassy` with no runtime at
all. Most applications want one of those, or the [`serf`] facade, rather than this
core directly.

## Wire evolution

The legacy Go serf carried two negotiation knobs — `protocol_version` and
`delegate_version` — so mixed-version clusters could gate features at runtime. This
stack deliberately carries neither: the wire forms new↔new clusters only, and there
is no per-message version field to dispatch on.

- **Additive evolution rides proto3 semantics.** Every message body is a proto3
  message; a new optional field decodes as its default on nodes that predate it and
  is skipped (not erred) by nodes that do not know it. Never reuse or renumber a
  field, change its wire type, or make an optional field required — those are
  breaking changes. The framing envelope is additive the same way: an unknown
  message tag is dropped with its body length consumed, so a new message type
  degrades to a no-op on old nodes rather than a parse failure.
- **Breaking changes are a new cluster generation, fenced by the cluster label.**
  The gossip codec stamps every packet and stream with the configured label and
  ingress drops anything mismatched, so a layout that cannot be expressed
  additively ships as a new deployment under a new label and never meets the old
  one on a socket.
- **Delegates are a compile-time surface.** The delegate traits are Rust API
  versioned by the crate's semver; there is nothing to negotiate on the wire.

## Feature tiers

Unlike a pure wire-codec crate, serf's membership/event/query state is
intrinsically heap-backed (`Vec` / `Box` / `String` / maps), so a build must select
at least one of:

| Features | Environment |
|----------|-------------|
| `std` *(default)* | `std` hosts (tokio, smol, compio) |
| `alloc` | `no_std` with a global allocator (embassy, smoltcp, …) |

## Transports & options

| Feature | Adds |
|---------|------|
| `tcp` | `StreamEndpoint` over memberlist's plain-TCP reliable coordinator (`no_std` + `alloc`) |
| `tls` | rustls record layer on top of `tcp` (implies `tcp`; `std`-only) |
| `quic` + `quic-rustls-ring` | `QuicEndpoint` over memberlist's QUIC (quinn-proto) coordinator (`std`-only) |
| `coordinates` | Vivaldi network-coordinate estimation (`f64` transcendentals; `std`-only) |
| `tag-regex` *(default)* | regex-backed `Filter::Tag` matching; falls back to exact-equality without it |
| `aes-gcm` / `chacha20-poly1305` | key-management messages (`KeyRequest` / `KeyResponse`) for the matching AEAD backend; `encryption` enables both |

## Installation

```toml
[dependencies]
serf-proto = "0.5"                                                          # std (default)

# no_std + alloc, with the plain-tcp coordinator:
serf-proto = { version = "0.5", default-features = false, features = ["alloc", "tcp"] }
```

## The serf family

The crates split protocol logic from I/O, mirroring the memberlist layering:

[`serf`] (facade) · **`serf-proto`** (this crate) · [`serf-driver`] (shared driver
glue) · [`serf-reactor`] (tokio / smol driver) · [`serf-compio`] (compio driver) ·
`serf-embedded` (shared `no_std` core) · `serf-smoltcp` (smoltcp driver) ·
`serf-embassy` (embassy driver).

## License

`serf-proto` is under the terms of the MPL-2.0 license.

See [LICENSE] for details.

Copyright (c) 2025 Al Liu.

Copyright (c) 2013 HashiCorp, Inc.

[`quinn-proto`]: https://crates.io/crates/quinn-proto
[`memberlist-proto`]: https://crates.io/crates/memberlist-proto
[`buffa`]: https://crates.io/crates/buffa
[`serf`]: https://crates.io/crates/serf
[`serf-driver`]: https://crates.io/crates/serf-driver
[`serf-reactor`]: https://crates.io/crates/serf-reactor
[`serf-compio`]: https://crates.io/crates/serf-compio
[LICENSE]: https://github.com/al8n/serf/blob/main/LICENSE
[Github-url]: https://github.com/al8n/serf/
[CI-url]: https://github.com/al8n/serf/actions/workflows/ci-core.yml
[codecov-url]: https://app.codecov.io/gh/al8n/serf/
[doc-url]: https://docs.rs/serf-proto
[crates-url]: https://crates.io/crates/serf-proto
[discord]: https://discord.gg/4JyVhKFcrt
