# serf-embassy

An async `no_std` [serf](https://github.com/al8n/serf) driver over the
[embassy-net](https://github.com/embassy-rs/embassy) network stack.

`serf-embassy` drives [`serf-embedded`](../serf-embedded)'s transport-agnostic
`SerfEngine` on an embassy-net TCP/IP stack. It mirrors
[`memberlist-embassy`](https://github.com/al8n/memberlist)'s architecture —
per-pool-slot async socket workers mediated by a per-slot mailbox, a synchronous
`pump` runner, and a cloneable async handle sharing state through
`embassy-sync` signals — but folds serf's richer machine (queries, user events,
key management) on top of membership.

The caller owns the embassy-net `Stack` and supplies a gossip `UdpSocket` plus a
pool of reliable-plane `TcpSocket`s; `Serf::new` wires up the engine and hands
back the handle paired with a `Runner` to spawn as a task.

## Serf-specific behaviors

- **Self-addressed gossip loopback.** embassy-net (smoltcp underneath) does not
  loop a self-addressed UDP datagram back into `recv` like an OS socket. serf
  directs a node's response to its OWN locally-originated query / key request at
  its own advertise address, so the gossip view diverts such datagrams into a
  driver loopback buffer the next pump ingests.
- **Mandatory events actioned before lossy buffering.** The runner's post-pump
  drain applies an `Event::Shutdown` (stop) and an `Event::KeyRequest` (through
  the engine's live-keyring `handle_key_request` chokepoint) BEFORE the event
  copy is buffered for the app's bounded event queue.
- **Core-owned await-result join.** `Serf::join` dispatches the engine's
  await-result join and awaits its `poll_join` outcome, mapping
  `Ok(ReachedSet)` / `JoinFailed` into the handle's result.

## Features

- `std` (default) / `alloc` — independent capability tiers.
- `cidr` — CIDR peer-admission allow-list.
- `aes-gcm` / `chacha20-poly1305` / `encryption` — gossip + reliable-plane AEAD.

## License

Licensed under the MPL-2.0 license.
