<div align="center">
<h1>serf-embedded</h1>
</div>
<div align="center">

The transport-agnostic `no_std` driving core shared by serf's embedded drivers.

</div>

`serf-embedded` is the link-layer-independent core the future serf smoltcp /
embassy-net drivers are built on. It owns serf's super-machine
(`serf_proto::StreamEndpoint` over the plain-TCP `RawRecords` path) and the
pooled-stream reliable plane, driving both through the [`GossipIo`] / [`StreamIo`]
seams a driver supplies to [`SerfEngine::pump`]. The engine performs **no** socket
I/O: a driver owns the UDP gossip socket and the reliable-stream socket pool, ticks
its own link-layer stack, then calls [`SerfEngine::pump`] to advance the machine
over them.

It reuses the payload-agnostic glue from
[`memberlist-embedded`](https://docs.rs/memberlist-embedded) directly — the
[`ReliablePlane`], the [`GossipIo`] / [`StreamIo`] seams, the cross-transport
transform pipeline, the bounded resolver result, and the engine sizing / error
types — and adds only [`SerfEngine`], the port of memberlist-embedded's `Engine`
that drives serf's richer super-machine (folding serf's query, user-event, and
key-management events into the pump on top of membership).

This crate is `no_std` with `alloc`: the two feature tiers `std` and `alloc` are
independent, and the core builds on bare-metal targets such as `thumbv7em-none-eabihf`.

[`GossipIo`]: https://docs.rs/memberlist-embedded/latest/memberlist_embedded/trait.GossipIo.html
[`StreamIo`]: https://docs.rs/memberlist-embedded/latest/memberlist_embedded/trait.StreamIo.html
[`ReliablePlane`]: https://docs.rs/memberlist-embedded/latest/memberlist_embedded/reliable/struct.ReliablePlane.html
[`SerfEngine`]: https://docs.rs/serf-embedded/latest/serf_embedded/struct.SerfEngine.html
[`SerfEngine::pump`]: https://docs.rs/serf-embedded/latest/serf_embedded/struct.SerfEngine.html#method.pump
