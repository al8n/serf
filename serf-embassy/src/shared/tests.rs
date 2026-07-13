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

/// Build an engine over an arbitrary port / advertise / transform triple.
fn try_engine(
  port: u16,
  advertise: SocketAddr,
  transform: TransformOptions,
) -> Result<SerfEngine<SmolStr, SlotId>, serf_embedded::InitError> {
  SerfEngine::<SmolStr, SlotId>::try_new_at(
    Options::new()
      .with_port(port)
      .with_close_timeout(Duration::from_secs(10)),
    transform,
    EndpointOptions::new(SmolStr::new("test"), advertise),
    SerfOptions::new(),
    at(),
    SmallRng::seed_from_u64(42),
  )
}

/// A node must advertise an address its peers can route a reply to: an unspecified
/// IP would be gossiped cluster-wide and then be useless to every peer that selected
/// it as an egress destination, so it is refused before the endpoint exists.
#[test]
fn a_non_routable_advertise_address_is_refused() {
  // `SerfEngine` is not `Debug`, so the error is matched out with a `let-else`.
  let Err(err) = try_engine(
    7946,
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946),
    TransformOptions::default(),
  ) else {
    panic!("an unspecified advertise address must be refused");
  };
  assert!(
    matches!(
      err,
      serf_embedded::InitError::Memberlist(
        serf_embedded::MemberlistInitError::NonRoutableAdvertiseAddr(_)
      )
    ),
    "{err:?}"
  );
}

/// One port serves both the gossip and reliable planes, and an embedded interface
/// has no NAT — so a node advertising a port it does not bind would have every peer
/// routing to a port nothing listens on.
#[test]
fn an_advertise_port_that_does_not_match_the_bound_port_is_refused() {
  let Err(err) = try_engine(
    7946,
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7947),
    TransformOptions::default(),
  ) else {
    panic!("an advertise port other than the bound port must be refused");
  };
  assert!(
    matches!(
      err,
      serf_embedded::InitError::Memberlist(
        serf_embedded::MemberlistInitError::AdvertisePortMismatch
      )
    ),
    "{err:?}"
  );
}

/// A labelled cluster that opts out of the inbound label check still constructs: the
/// opt-out is a migration posture (accept unlabelled peers while the label rolls
/// out), not a rejected configuration.
#[test]
fn a_label_with_the_inbound_check_skipped_constructs() {
  let advertise = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946);
  let transform = TransformOptions::default()
    .with_label(Some(alloc::vec![b'm', b'i', b'g']))
    .expect("a short label is valid")
    .with_skip_inbound_label_check(true);

  let Ok(engine) = try_engine(7946, advertise, transform) else {
    panic!("skipping the inbound label check is a valid migration posture");
  };
  assert_eq!(engine.port(), 7946);
}

/// An app that never drains `poll_event` cannot grow the driver's buffer without
/// bound: at the cap the OLDEST buffered event is shed and counted, and the public
/// `events_dropped` total reports the loss rather than hiding it.
#[test]
fn the_app_event_buffer_sheds_the_oldest_at_the_cap() {
  let shared = shared();
  assert_eq!(shared.events_dropped(), 0);

  let overflow = 5usize;
  for _ in 0..DEFAULT_EVENT_BUFFER_CAP + overflow {
    shared.push_app_event(Event::LeftCluster);
  }

  assert_eq!(
    shared.app_events.borrow().len(),
    DEFAULT_EVENT_BUFFER_CAP,
    "the buffer must stay bounded at the cap"
  );
  assert_eq!(
    shared.app_events_dropped.get(),
    overflow as u64,
    "every event past the cap is shed and counted"
  );
  assert_eq!(
    shared.events_dropped(),
    overflow as u64,
    "the public total must report the driver-side shedding"
  );
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
