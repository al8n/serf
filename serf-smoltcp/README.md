<div align="center">
<h1>serf-smoltcp</h1>
</div>
<div align="center">

Executor-free `no_std` serf driver over the [smoltcp] TCP/IP stack — no OS and no
async runtime required.

[<img alt="github" src="https://img.shields.io/badge/github-al8n/serf-8da0cb?style=for-the-badge&logo=Github" height="22">][Github-url]
<img alt="LoC" src="https://img.shields.io/endpoint?url=https%3A%2F%2Fgist.githubusercontent.com%2Fal8n%2Fd29ceff54c025fe4e8b144a51efb9324%2Fraw%2Fserf-smoltcp" height="22">
[<img alt="Build" src="https://img.shields.io/github/actions/workflow/status/al8n/serf/embedded.yml?logo=Github-Actions&style=for-the-badge" height="22">][CI-url]
[<img alt="codecov" src="https://img.shields.io/codecov/c/gh/al8n/serf?style=for-the-badge&token=6R3QFWRWHL&logo=codecov" height="22">][codecov-url]

[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-serf--smoltcp-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs" height="20">][doc-url]
[<img alt="crates.io" src="https://img.shields.io/crates/v/serf-smoltcp?style=for-the-badge&logo=rust" height="22">][crates-url]
[<img alt="crates.io" src="https://img.shields.io/crates/d/serf-smoltcp?color=critical&logo=rust&style=for-the-badge" height="22">][crates-url]
<img alt="license" src="https://img.shields.io/badge/License-MPL%202.0-blue.svg?style=for-the-badge&fontColor=white&logoColor=ffffff&logo=data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiPz4KPHN2ZyBpZD0iX+WbvuWxgl8xIiBkYXRhLW5hbWU9IuWbvuWxgiAxIiB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA0NzMuNDcgMjU1LjEyMiI+CiAgPGRlZnM+CiAgICA8c3R5bGU+CiAgICAgIC5jbHMtMSB7CiAgICAgICAgZmlsbDogI2ZmZjsKICAgICAgICBzdHJva2Utd2lkdGg6IDBweDsKICAgICAgfQogICAgPC9zdHlsZT4KICA8L2RlZnM+CiAgPHBvbHlnb24gY2xhc3M9ImNscy0xIiBwb2ludHM9IjM0MC4wNjUgLjQ4NCAzMzQuMzczIDEuMjA5IDMyOC45MjkgMi4xNzUgMzIzLjczMSAzLjYyOCAzMTguNzggNS4zMTggMzEzLjgzMiA3LjAxMiAzMDkuMTI3IDkuMTkgMzA0LjY3MiAxMS42MDYgMzAwLjQ2NiAxNC4yNjggMjk2LjI1OSAxNy4xNjkgMjkyLjI5NyAyMC4zMTIgMjg4LjU4NiAyMy42OTcgMjg1LjEyIDI3LjMyNiAyODEuOTAyIDMxLjE5NCAyNzguNjg0IDM1LjMwNyAyNzUuOTYyIDM5LjQxNiAyNzMuMjQgNDQuMDExIDI3MC43NjUgNDguNjA3IDI2OC41MzkgNTMuNjg1IDI2Ni41NTkgNDguMzYzIDI2NC4wODMgNDMuMjg1IDI2MS42MDggMzguNDUxIDI1OC42MzkgMzMuODU0IDI1NS40MjIgMjkuNzQ0IDI1MS45NTUgMjUuODc1IDI0OC4yNDQgMjIuMjQ3IDI0NC41MzEgMTguODYzIDI0MC4zMjIgMTUuNzE5IDIzNi4xMTYgMTMuMDU5IDIzMS40MTQgMTAuMzk3IDIyNi45NTkgOC4yMjIgMjIyLjAwOCA2LjI4OCAyMTcuMDU3IDQuNTk0IDIxMi4xMDkgMy4xNDMgMjA2LjkxMSAxLjkzNCAyMDEuNDY0IC45NjggMTkwLjU3NiAwIDE3OS45MzIgMCAxNzQuNzM1IC40ODQgMTY5Ljc4NiAuOTY4IDE2NC44MzUgMS42OTMgMTYwLjEzNCAyLjkwMyAxNTUuNjc4IDQuMTEyIDE1MS4yMjMgNS41NjIgMTQ2Ljc2NyA3LjI1MyAxNDIuODA3IDkuMTkgMTM4LjYwMiAxMS4xMjUgMTM0Ljg4OCAxMy41NDEgMTMxLjE3NSAxNS45NiAxMjcuNzExIDE4LjYxOSAxMjQuMjQ0IDIxLjUyMiAxMjEuMDI5IDI0LjY2NiAxMTguMDU4IDI3LjgwOSAxMTUuMDg3IDMxLjE5NCAxMTIuMzY1IDM0LjgyMiAxMDkuODg5IDM4LjY5MSAxMDcuNjYzIDQyLjU2IDEwNy42NjMgNS4wNzggMCA1LjA3OCAwIDU4Ljc2NCAzMy45MDcgNTguNzY0IDMzLjkwNyAyMDAuNDcgMCAyMDAuNDcgMCAyNTUuMTIyIDE1Ni42NjcgMjU1LjEyMiAxNTYuNjY3IDIwMC40NyAxMDcuNjYzIDIwMC40NyAxMDcuNjYzIDEwOC4zMzcgMTA3LjkwOSAxMDMuMjU5IDEwOC42NTIgOTguNDIxIDEwOS4zOTYgOTMuODI3IDExMC4zODUgODkuNDc0IDExMS42MjMgODUuMTIxIDExMy4xMDcgODEuMjUyIDExNC44NCA3Ny4zODMgMTE2LjgyIDczLjk5OCAxMTkuMDQ3IDcwLjYxMSAxMjEuNzcyIDY3LjcxMSAxMjQuNDk0IDY1LjA1MSAxMjcuNDYxIDYyLjYzMyAxMzAuOTI5IDYwLjQ1NSAxMzQuNjM5IDU4LjUyIDEzOC4zNTIgNTcuMDcgMTQyLjU2MSA1NS44NiAxNDcuMjYzIDU0Ljg5NSAxNTEuOTY1IDU0LjQxIDE1Ny4xNjIgNTQuMTY3IDE2MS4zNzEgNTQuMTY3IDE2NS4zMzEgNTQuNjUxIDE2OS4yOTEgNTUuMzc2IDE3Mi43NTUgNTYuMzQ1IDE3Ni4yMjEgNTcuNTU0IDE3OS40MzkgNTkuMDA1IDE4Mi40MDggNjAuNjk4IDE4NS4xMyA2Mi44NzMgMTg3LjYwNSA2NS4yOTIgMTkwLjA4MSA2Ny45NTIgMTkyLjA2MSA3MC44NTQgMTk0LjA0MSA3NC4yMzkgMTk1LjUyNCA3OC4xMDggMTk3LjAxMSA4MS45NzcgMTk4LjI0OSA4Ni41NzQgMTk5LjIzOCA5MS4xNjcgMTk5Ljk4IDk2LjI0NiAyMDAuNzIyIDEwMS44MDkgMjAwLjk3MSAxMDcuODUyIDIwMC45NzEgMjU1LjEyMiAzMDcuMzk3IDI1NS4xMjIgMzA3LjM5NyAyMDAuNDcgMjczLjQ4NyAyMDAuNDcgMjczLjQ4NyAxMTMuNDE1IDI3My43MzYgMTA4LjMzNyAyNzMuOTgzIDEwMy4yNTkgMjc0LjQ3OCA5OC40MjEgMjc1LjQ2NiA5My44MjcgMjc2LjQ1OCA4OS40NzQgMjc3LjY5NiA4NS4xMjEgMjc5LjE4IDgxLjI1MiAyODAuOTEzIDc3LjM4MyAyODIuODk0IDczLjk5OCAyODUuMTIgNzAuNjExIDI4Ny41OTUgNjcuNzExIDI5MC41NjcgNjUuMDUxIDI5My41MzQgNjIuNjMzIDI5Ny4wMDIgNjAuNDU1IDMwMC40NjYgNTguNTIgMzA0LjQyNSA1Ny4wNyAzMDguNjM1IDU1Ljg2IDMxMy4zMzYgNTQuODk1IDMxOC4wMzggNTQuNDEgMzIzLjIzNSA1NC4xNjcgMzI3LjQ0NCA1NC4xNjcgMzMxLjQwNCA1NC42NTEgMzM1LjM2NCA1NS4zNzYgMzM4LjgyOCA1Ni4zNDUgMzQyLjI5MiA1Ny41NTQgMzQ1LjUwOSA1OS4wMDUgMzQ4LjQ4MSA2MC42OTggMzUxLjIwMyA2Mi44NzMgMzUzLjY3OCA2NS4yOTIgMzU1LjkwNCA2Ny45NTIgMzU4LjEzMyA3MC44NTQgMzYwLjExNCA3NC4yMzkgMzYzLjA4MiA4MS45NzcgMzY0LjMyIDg2LjU3NCAzNjUuMzExIDkxLjE2NyAzNjYuMDUzIDk2LjI0NiAzNjYuNzk1IDEwMS44MDkgMzY3LjA0NCAxMDcuODUyIDM2Ny4wNDQgMjU1LjEyMiA0NzMuNDcgMjU1LjEyMiA0NzMuNDcgMjAwLjQ3IDQzOS41NiAyMDAuNDcgNDM5LjU2IDg2LjMzIDQzOS4zMTMgNzcuNjI0IDQzOC4zMjIgNjkuNjQ1IDQzNi44MzggNjEuOTA3IDQzNC44NTggNTQuNjUxIDQzMi4zODMgNDcuODgyIDQyOS40MTQgNDEuNTk1IDQyNS45NDggMzUuNzg4IDQyMS45ODggMzAuMjI5IDQxNy41MzIgMjUuMzkxIDQxMy4wNzcgMjAuNzk3IDQwNy44OCAxNi45MjggNDAyLjY4MiAxMy4zIDM5Ni45OTEgMTAuMTU3IDM5MS4wNTIgNy4yNTMgMzg0Ljg2MyA1LjA3OCAzNzguNjc0IDMuMTQzIDM3MS45OTMgMS42OTMgMzY1LjU1OCAuNzI1IDM1OC42MjkgMCAzNDUuNzU5IDAgMzQwLjA2NSAuNDg0Ii8+Cjwvc3ZnPg==" height="22">

[<img alt="Discord" src="https://img.shields.io/discord/835936528140206122?style=for-the-badge&logo=discord&logoColor=white&label=Discord&color=7289da" height="22">][discord]

</div>

## Introduction

`serf-smoltcp` drives serf's transport-agnostic core ([`serf-embedded`]'s
`SerfEngine`, which composes serf's super-machine over the memberlist reliable
coordinator) on a [smoltcp] TCP/IP stack. It owns no executor and performs no
blocking: you pump it from your own poll loop alongside the smoltcp `Interface`,
and it tells you the next instant it wants to be polled.

It mirrors [`memberlist-smoltcp`] over `serf-embedded` instead of
`memberlist-embedded`: all the payload-agnostic link-layer glue is reused through
`serf-embedded`, and only the thin smoltcp socket views (`GossipIo` / `StreamIo`)
are reimplemented. The `Serf` handle owns the smoltcp sockets + interface, and its
`poll(now, device)` ticks the stack, pumps the engine, then acts on serf's
mandatory events in the poll cycle (a lost id-conflict `Shutdown` flips a stop
flag; an encryption `KeyRequest` is applied to the local keyring and answered),
buffering every event for the app's own `poll_event`.

`no_std` + `alloc`: the protocol state lives in slab-backed pools, so there is no
per-packet heap traffic on the hot path.

## Installation

```toml
[dependencies]
serf-smoltcp = { version = "0.5", default-features = false, features = ["alloc"] }
```

## Example

```rust,ignore
use serf_smoltcp::{
    EndpointOptions, InterfaceOptions, Options, Serf, SerfOptions, SocketAddrResolver,
    TransformOptions,
};
use smol_str::SmolStr;

// `device` is your smoltcp `Device`; `now` is a portable `Instant` your firmware
// advances. `advertise` is the local node's `SocketAddr`.
let mut node = Serf::<SmolStr, _, _>::new(
    Options::new(),
    InterfaceOptions::new(hardware_addr), // + IP addresses, routes, RNG seed
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("node-a"), advertise),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut device,
    now,
);
node.start(now);

// Pump from your own loop; `poll` advances serf + the smoltcp interface and returns
// the next instant the driver wants to be polled (or `None`).
loop {
    let next = node.poll(now, &mut device);
    if node.is_shutdown() {
        break;
    }
    while let Some(event) = node.poll_event() {
        // react to serf events (member join/leave/update, user events, queries, …)
    }
    // ...sleep until `next` or until the device is RX-ready, then advance `now`...
}
```

## Feature flags

| Feature | Description |
|---------|-------------|
| `std` *(default)* | host builds and the test harness |
| `alloc` | `no_std` with a global allocator (bare metal) — build with `--no-default-features --features alloc` |
| `aes-gcm` / `-chacha20-poly1305` | AEAD encryption (gossip + plain-TCP reliable) plus serf's key-management surface |
| `encryption` | umbrella for both AEAD backends |
| `cidr` | IP allow-list admission |

## Design

- **`no_std` + `alloc`**, panic-free hot path: slab-backed protocol pools, no
  per-packet allocation.
- **Caller-poll**: no executor; you drive `poll(now, device)` from your loop and
  honor the returned next-wake instant — the same shape as advancing the smoltcp
  `Interface`.
- Built on the transport-agnostic [`serf-embedded`] `SerfEngine`, with smoltcp UDP
  (gossip) + a TCP socket pool (reliable plane) behind its I/O seams.
- serf's mandatory driver-actioned events are handled DRIVER-side in the poll
  cycle, so correctness does not depend on the application draining events.

## License

`serf-smoltcp` is under the terms of the MPL-2.0 license.

See [LICENSE] for details.

Copyright (c) 2025 Al Liu.

[smoltcp]: https://crates.io/crates/smoltcp
[`serf-embedded`]: https://crates.io/crates/serf-embedded
[`memberlist-smoltcp`]: https://crates.io/crates/memberlist-smoltcp
[LICENSE]: https://github.com/al8n/serf/blob/main/LICENSE
[Github-url]: https://github.com/al8n/serf/
[CI-url]: https://github.com/al8n/serf/actions/workflows/embedded.yml
[codecov-url]: https://app.codecov.io/gh/al8n/serf/
[doc-url]: https://docs.rs/serf-smoltcp
[crates-url]: https://crates.io/crates/serf-smoltcp
[discord]: https://discord.gg/4JyVhKFcrt
