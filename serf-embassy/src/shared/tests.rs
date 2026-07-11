use super::*;

use core::{
  net::{IpAddr, Ipv4Addr},
  time::Duration,
};

use memberlist_proto::EndpointOptions;
use serf_embedded::{Options, SerfOptions, TransformOptions};
use smol_str::SmolStr;

/// A fixed instant well past the origin, for the deterministic engine construction
/// and the drain call.
fn at() -> Instant {
  Instant::from_origin(Duration::from_secs(86_400))
}

/// A minimal single-node [`Shared`] over a real engine bound to a routable advertise
/// address, wrapped with the fresh (unsignaled) signals and empty buffers.
fn shared() -> Shared<SmolStr> {
  let advertise = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946);
  let engine = SerfEngine::<SmolStr, SlotId>::try_new_at(
    Options::new()
      .with_port(7946)
      .with_close_timeout(Duration::from_secs(10)),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("test"), advertise),
    SerfOptions::new(),
    at(),
    SmallRng::seed_from_u64(42),
  )
  .expect("a routable single-node configuration constructs");
  Shared::new(engine, advertise)
}

/// The production conflict pathway: a drained `Event::Shutdown` — the terminal event
/// serf emits when the local node loses an id-conflict vote — routes through
/// `route_drained_event` to `begin_shutdown` (latching the one-way shutdown state and
/// pulsing both wakes so the pump halts and a parked join resolves with the shutdown
/// error), AND is buffered for the app's `poll_event`.
///
/// This is the drain arm the conflict e2e no longer exercises: the public
/// `shutdown()` reaches `begin_shutdown` directly, without an engine event, so it
/// buffers no `Event::Shutdown`. Only a real vote loss surfaces that event, and this
/// covers that route.
#[test]
fn drained_shutdown_event_poisons_and_buffers() {
  let shared = shared();

  // A fresh node is live, its wakes unsignaled, its app buffer empty — so the
  // transition below is caused by the drained event alone.
  assert!(!shared.is_shutdown());
  assert!(!shared.join_wake.signaled());
  assert!(!shared.pump_wake.signaled());
  assert!(shared.pop_app_event().is_none());

  // Route the terminal event exactly as the pump's drain does for each drained event.
  let queued = shared.route_drained_event(Event::Shutdown, at());

  assert!(!queued, "a Shutdown event queues no outbound gossip work");
  // `begin_shutdown` ran: the one-way latch is set and both waiters are poisoned —
  // the pump wake so the loop stops after this drain, the join wake so a parked join
  // re-checks and resolves with the shutdown error.
  assert!(shared.is_shutdown(), "the shutdown latch must be set");
  assert!(shared.join_wake.signaled(), "parked joins must be woken");
  assert!(
    shared.pump_wake.signaled(),
    "the pump loop must be woken to observe the latch and stop"
  );
  // The terminal event still reaches the app.
  assert!(
    matches!(shared.pop_app_event(), Some(Event::Shutdown)),
    "the terminal Event::Shutdown must be buffered for poll_event"
  );
}

/// With user coalescing enabled and a small buffered-volume cap, distinct-named
/// coalescing user events fed past the cap are shed by the engine's user coalescer,
/// and the running total surfaces through the same engine borrow the public
/// `coalesced_user_events_dropped` handle accessor reads.
#[test]
fn coalesced_user_events_dropped_surfaces_overflow() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let advertise = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946);
  let engine = SerfEngine::<SmolStr, SlotId>::try_new_at(
    Options::new()
      .with_port(7946)
      .with_close_timeout(Duration::from_secs(10)),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("test"), advertise),
    SerfOptions::new()
      .with_user_coalesce_period(Duration::from_secs(10))
      .with_user_quiescent_period(Duration::from_secs(2))
      .with_max_coalesced_user_events(Some(cap)),
    at(),
    SmallRng::seed_from_u64(42),
  )
  .expect("a routable single-node configuration constructs");
  let shared = Shared::new(engine, advertise);
  assert_eq!(shared.engine.borrow().coalesced_user_events_dropped(), 0);

  let n: u32 = 20;
  for i in 0..n {
    shared
      .engine
      .borrow_mut()
      .user_event(
        SmolStr::from(alloc::format!("evt-{i}")),
        bytes::Bytes::from_static(b"p"),
        true,
        at(),
      )
      .expect("a coalescing user event is accepted while running");
  }

  assert_eq!(
    shared.engine.borrow().coalesced_user_events_dropped(),
    u64::from(n) - cap.get() as u64,
    "every distinct-named coalescing event past the cap is shed and counted"
  );
  assert_eq!(shared.engine.borrow().coalesced_member_events_dropped(), 0);
}
