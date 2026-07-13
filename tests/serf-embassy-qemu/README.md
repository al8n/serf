<div align="center">
<h1>serf-embassy-qemu</h1>
</div>
<div align="center">

Bare-metal QEMU execution proof for the **serf** embassy driver.

[<img alt="github" src="https://img.shields.io/badge/github-al8n/serf-8da0cb?style=for-the-badge&logo=Github" height="22">][Github-url]

</div>

## Introduction

`serf-embassy-qemu` boots [`serf-embassy`] on an emulated Cortex-M (the `mps2-an386`
machine) under QEMU and joins two nodes over embassy-net, then propagates a user event
between them — an end-to-end proof that the `no_std` async serf driver runs on real bare
metal, with its own `cortex-m-rt` runtime, a SysTick embassy-time driver, and a no_std
entropy backend.

Two serf nodes converge on a two-member view and then one broadcasts a user event the
other observes as `Event::User` — the minimal serf-above-memberlist signal, exercising the
whole stack (SWIM membership plus serf's gossip plane) on the emulated core. Success is
reported via the semihosting process exit code (0 = pass).

It is the CI execution gate for the embassy driver and doubles as a complete, runnable
wiring example. Because it pins a foreign default target, it is **excluded** from the host
workspace and built on its own:

```sh
cd tests/serf-embassy-qemu && cargo run
```

This is an internal proof binary (`publish = false`) for the [serf] workspace.

## License

`serf-embassy-qemu` is under the terms of the MPL-2.0 license.

See [LICENSE] for details.

Copyright (c) 2025 Al Liu.

Copyright (c) 2013 HashiCorp, Inc.

[serf]: https://github.com/al8n/serf
[`serf-embassy`]: https://crates.io/crates/serf-embassy
[LICENSE]: https://github.com/al8n/serf/blob/main/LICENSE
[Github-url]: https://github.com/al8n/serf/
