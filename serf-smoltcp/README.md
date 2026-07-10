<div align="center">
<h1>serf-smoltcp</h1>
</div>
<div align="center">

Executor-free `no_std` serf driver over the [smoltcp] TCP/IP stack — no OS and no
async runtime required.

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
