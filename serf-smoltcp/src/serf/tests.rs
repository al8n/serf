use core::{
  cell::RefCell,
  net::{IpAddr, Ipv4Addr, SocketAddr},
};

use memberlist_proto::{
  Instant, Node,
  codec::{EncodeOptions, encode_outgoing},
  typed::{Alive, DelegateVersion, Message, Meta, ProtocolVersion},
};
use serf_embedded::GossipIo;
use smol_str::SmolStr;
use smoltcp::{
  phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken},
  time::Instant as SmolInstant,
};

use crate::{
  DEFAULT_EVENT_BUFFER_CAP, EndpointOptions, EthernetAddress, HardwareAddress, InterfaceOptions,
  IpAddress, IpCidr, Ipv4Address, Options, Route, Serf, SerfOptions, SerfState, SocketAddrResolver,
  TransformOptions, stream_io::SmoltcpStream,
};

/// A `Medium::Ip` device that never delivers a frame — enough to construct a node
/// (construction binds sockets and builds the interface but performs no I/O).
struct NullDevice;

struct NRx;
struct NTx;

impl RxToken for NRx {
  fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
    f(&[])
  }
}

impl TxToken for NTx {
  fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
    let mut buf = std::vec![0u8; len];
    f(&mut buf)
  }
}

impl Device for NullDevice {
  type RxToken<'a> = NRx;
  type TxToken<'a> = NTx;

  fn receive(&mut self, _t: SmolInstant) -> Option<(NRx, NTx)> {
    None
  }

  fn transmit(&mut self, _t: SmolInstant) -> Option<NTx> {
    Some(NTx)
  }

  fn capabilities(&self) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ip;
    caps.max_transmission_unit = 1500;
    caps.checksum = ChecksumCapabilities::ignored();
    caps
  }
}

fn ip_iface(octet: u8) -> InterfaceOptions {
  InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, octet), 24))
    .with_random_seed(octet as u64)
}

fn now() -> Instant {
  Instant::from_origin(core::time::Duration::from_secs(86_400))
}

fn try_build(
  advertise: SocketAddr,
  device: &mut NullDevice,
) -> Result<Serf<SmolStr, SocketAddr, NullDevice>, crate::InitError> {
  Serf::try_new(
    Options::new(),
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("a"), advertise),
    SerfOptions::new(),
    &SocketAddrResolver,
    device,
    now(),
  )
}

#[test]
fn construction_succeeds_alive_single_member() {
  let mut dev = NullDevice;
  let node = try_build(
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    &mut dev,
  )
  .expect("a valid configuration constructs");
  assert_eq!(node.state(), SerfState::Alive);
  // serf registers its self-member lazily once the node starts operating, so a
  // freshly constructed (not-yet-started) node has no members recorded yet.
  assert_eq!(node.num_members(), 0);
  assert!(!node.is_shutdown());
  assert_eq!(node.local_id(), SmolStr::new("a"));
  assert_eq!(
    node.advertise_address(),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946)
  );
  // The pinned interface seed is applied verbatim.
  assert_eq!(node.interface_random_seed(), 1);
}

#[test]
fn non_routable_advertise_is_rejected() {
  let mut dev = NullDevice;
  // The unspecified address is not a routable advertise destination.
  // `Serf` is not `Debug` (it holds a smoltcp `Interface`), so `expect_err` is
  // unavailable; match the error out with `let-else`.
  let Err(err) = try_build(
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946),
    &mut dev,
  ) else {
    panic!("a non-routable advertise must be rejected");
  };
  assert!(matches!(err, crate::InitError::NonRoutableAdvertiseAddr(_)));
}

#[test]
fn zero_port_is_rejected() {
  let mut dev = NullDevice;
  let Err(err) = Serf::<SmolStr, SocketAddr, NullDevice>::try_new(
    Options::new().with_port(0),
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(
      SmolStr::new("a"),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    ),
    SerfOptions::new(),
    &SocketAddrResolver,
    &mut dev,
    now(),
  ) else {
    panic!("port 0 must be rejected");
  };
  assert!(matches!(err, crate::InitError::ZeroPort));
}

#[test]
fn over_ceiling_user_event_size_is_rejected() {
  let mut dev = NullDevice;
  // A `max_user_event_size` above the absolute ceiling is rejected at
  // construction rather than dropping oversize user events at send time.
  let Err(err) = Serf::<SmolStr, SocketAddr, NullDevice>::try_new(
    Options::new(),
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(
      SmolStr::new("a"),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    ),
    SerfOptions::new().with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1),
    &SocketAddrResolver,
    &mut dev,
    now(),
  ) else {
    panic!("an over-ceiling max_user_event_size must be rejected");
  };
  assert!(matches!(err, crate::InitError::InvalidSerfOptions(_)));
}

// The explicit-RNG constructor runs the same preflight, so an over-ceiling
// configuration cannot slip in through the production entropy-seeded path.
#[test]
fn over_ceiling_user_event_size_is_rejected_by_with_rng() {
  use memberlist_proto::{SeedableRng, SmallRng};

  let mut dev = NullDevice;
  let Err(err) = Serf::<SmolStr, SocketAddr, NullDevice>::with_rng(
    Options::new(),
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(
      SmolStr::new("a"),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    ),
    SerfOptions::new().with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1),
    &SocketAddrResolver,
    &mut dev,
    now(),
    SmallRng::seed_from_u64(1),
    SmallRng::seed_from_u64(2),
  ) else {
    panic!("an over-ceiling max_user_event_size must be rejected by with_rng");
  };
  assert!(matches!(err, crate::InitError::InvalidSerfOptions(_)));
}

/// A [`GossipIo`] that replays a fixed list of pre-encoded datagrams once (one per
/// `recv`), draining `send`. Feeds a controlled flood of inbound gossip straight to
/// the engine's pump.
struct FloodGossip {
  frames: Vec<Vec<u8>>,
  idx: usize,
  src: SocketAddr,
}

impl GossipIo for FloodGossip {
  fn recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
    let frame = self.frames.get(self.idx)?;
    self.idx += 1;
    let n = frame.len().min(buf.len());
    buf[..n].copy_from_slice(&frame[..n]);
    Some((self.src, n))
  }

  fn send(&mut self, _bytes: &[u8], _dest: SocketAddr) {}
}

/// Encode a well-formed gossip datagram carrying a single `Alive` for `id` at
/// `advertise`, using the default (unlabelled, unencrypted) codec so the engine
/// decodes it exactly as any inbound gossip frame.
fn alive_frame_at(id: &str, advertise: SocketAddr) -> Vec<u8> {
  let node = Node::new(SmolStr::from(id), advertise);
  let alive = Alive::new(1, node)
    .with_meta(Meta::empty())
    .with_protocol_version(ProtocolVersion::V1)
    .with_delegate_version(DelegateVersion::V1);
  let msg: Message<SmolStr, SocketAddr> = Message::Alive(alive);
  encode_outgoing(&msg, &EncodeOptions::new(None))
    .expect("encode alive gossip frame")
    .to_vec()
}

/// A distinct `Alive` per index, each at its own address.
fn alive_frame(i: usize) -> Vec<u8> {
  let ip = Ipv4Addr::new(10, 1, (i / 250) as u8, (i % 250 + 1) as u8);
  alive_frame_at(
    &std::format!("flood-{i}"),
    SocketAddr::new(IpAddr::V4(ip), 7946),
  )
}

/// A single pump that produces more passive observations than the event buffer cap
/// makes the ENGINE shed the excess into its own `events_dropped` counter, while the
/// cap-sized remainder still fits the driver's empty queue (no driver-side drop).
/// [`Serf::events_dropped`] must sum BOTH counters, so it must report the loss —
/// returning only the driver's count would report 0 despite real drops.
#[test]
fn single_pump_over_cap_drop_is_counted() {
  let mut dev = NullDevice;
  let mut node = try_build(
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    &mut dev,
  )
  .expect("a valid configuration constructs");
  node.start(now());

  // Flood > cap distinct alives in ONE pump: each is a brand-new member → one
  // `Event::Member(Join)` observation, so the pump emits > cap passive events and
  // the engine sheds the surplus.
  let count = DEFAULT_EVENT_BUFFER_CAP + 200;
  let frames: Vec<Vec<u8>> = (0..count).map(alive_frame).collect();
  let mut flood = FloodGossip {
    frames,
    idx: 0,
    src: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946),
  };

  // Pump directly over the flood gossip and the real reliable-plane view (no
  // reliable activity occurs; the alives ride the gossip plane).
  {
    let sockets = RefCell::new(&mut node.sockets);
    let mut stream = SmoltcpStream::new(&mut node.iface, &sockets);
    node.engine.pump(now(), &mut flood, &mut stream);
  }

  // The engine shed observations this single pump, so the driver's public counter —
  // the engine's drops plus its own — must be non-zero. Before the fix it returned
  // only the (still-zero) driver count and reported 0 despite the loss.
  assert!(
    node.events_dropped() > 0,
    "events_dropped must reflect the engine's over-cap passive-observation drops, got {}",
    node.events_dropped()
  );

  // Draining the buffered survivors into the driver's own (empty, cap-sized) queue
  // adds no driver-side drop, so the reported total still reflects the engine loss.
  node.drain_engine_events(now());
  assert!(
    node.events_dropped() > 0,
    "events_dropped must still reflect the engine drops after the driver drains, got {}",
    node.events_dropped()
  );
}

/// With user coalescing enabled and a small buffered-volume cap, distinct-named
/// coalescing user events fed through the public command path past the cap are shed
/// by the engine's user coalescer, and the running total surfaces through the
/// public `coalesced_user_events_dropped` forward.
#[test]
fn coalesced_user_events_dropped_surfaces_overflow() {
  let cap = core::num::NonZeroUsize::new(4).unwrap();
  let mut dev = NullDevice;
  let serf_opts = SerfOptions::new()
    .with_user_coalesce_period(core::time::Duration::from_secs(10))
    .with_user_quiescent_period(core::time::Duration::from_secs(2))
    .with_max_coalesced_user_events(Some(cap));
  let mut node = Serf::<SmolStr, SocketAddr, NullDevice>::try_new(
    Options::new(),
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(
      SmolStr::new("a"),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946),
    ),
    serf_opts,
    &SocketAddrResolver,
    &mut dev,
    now(),
  )
  .expect("a valid configuration constructs");
  node.start(now());
  assert_eq!(node.coalesced_user_events_dropped(), 0);

  let n: u32 = 20;
  for i in 0..n {
    node
      .user_event(
        SmolStr::from(std::format!("evt-{i}")),
        bytes::Bytes::from_static(b"p"),
        true,
        now(),
      )
      .expect("a coalescing user event is accepted while running");
  }

  assert_eq!(
    node.coalesced_user_events_dropped(),
    u64::from(n) - cap.get() as u64,
    "every distinct-named coalescing event past the cap is shed and counted"
  );
  assert_eq!(node.coalesced_member_events_dropped(), 0);
}

#[test]
fn advertise_not_local_is_rejected() {
  let mut dev = NullDevice;
  // 10.0.0.9 is routable but not assigned to the interface (which holds 10.0.0.1).
  let Err(err) = try_build(
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 7946),
    &mut dev,
  ) else {
    panic!("an advertise IP the interface lacks must be rejected");
  };
  assert!(matches!(err, crate::InitError::AdvertiseAddrNotLocal(_)));
}

// ── link-layer and sizing validation ──────────────────────────────────────────

/// An `Medium::Ethernet` device, for the checks that only apply to an L2 medium.
struct EthernetDevice;

impl Device for EthernetDevice {
  type RxToken<'a> = NRx;
  type TxToken<'a> = NTx;

  fn receive(&mut self, _t: SmolInstant) -> Option<(NRx, NTx)> {
    None
  }

  fn transmit(&mut self, _t: SmolInstant) -> Option<NTx> {
    Some(NTx)
  }

  fn capabilities(&self) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ethernet;
    caps.max_transmission_unit = 1500;
    caps.checksum = ChecksumCapabilities::ignored();
    caps
  }
}

const LOCAL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 7946);

/// Construct over an arbitrary [`Options`] / [`InterfaceOptions`] pair and device,
/// returning only the error — `Serf` is not `Debug` (it holds a smoltcp
/// `Interface`), so the success case cannot be unwrapped for a message.
fn init_error<D>(cfg: Options, iface: InterfaceOptions, device: &mut D) -> crate::InitError
where
  D: Device,
{
  let Err(err) = Serf::<SmolStr, SocketAddr, D>::try_new(
    cfg,
    iface,
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("a"), LOCAL),
    SerfOptions::new(),
    &SocketAddrResolver,
    device,
    now(),
  ) else {
    panic!("the configuration under test must be rejected");
  };
  err
}

/// smoltcp's `Interface::new` asserts the hardware address's medium equals the
/// device's; the driver derives it first and rejects the mismatch as a typed error
/// rather than reaching that assert.
#[test]
fn a_medium_mismatch_is_rejected() {
  let mut dev = NullDevice; // Medium::Ip
  let iface = InterfaceOptions::new(HardwareAddress::Ethernet(EthernetAddress([
    0x02, 0, 0, 0, 0, 1,
  ])))
  .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
  .with_random_seed(1);

  let err = init_error(Options::new(), iface, &mut dev);
  let crate::InitError::MediumMismatch(m) = err else {
    panic!("an Ethernet hardware address on an IP device must be rejected: {err:?}");
  };
  assert_eq!(m.expected, Medium::Ethernet);
  assert_eq!(m.actual, Medium::Ip);
}

/// smoltcp stores the configured hardware address WITHOUT re-checking it, so a
/// multicast MAC would silently install an invalid L2 identity. The driver rejects
/// it instead.
#[test]
fn a_non_unicast_hardware_address_is_rejected() {
  let mut dev = EthernetDevice;
  // The low bit of the first octet marks a multicast MAC.
  let mac = EthernetAddress([0x01, 0, 0, 0, 0, 1]);
  let iface = InterfaceOptions::new(HardwareAddress::Ethernet(mac))
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
    .with_random_seed(1);

  let err = init_error(Options::new(), iface, &mut dev);
  assert!(
    matches!(err, crate::InitError::NonUnicastHardwareAddress(_)),
    "{err:?}"
  );
}

/// An interface with no address accepts no packets at all — a silently deaf node.
#[test]
fn an_interface_without_an_address_is_rejected() {
  let mut dev = NullDevice;
  let iface = InterfaceOptions::new(HardwareAddress::Ip).with_random_seed(1);
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::MissingIpAddress
  ));
}

/// smoltcp's `check_ip_addrs` `panic!`s on an address that is neither unicast nor
/// unspecified; the driver mirrors the condition and returns a typed error.
#[test]
fn a_non_unicast_interface_address_is_rejected() {
  let mut dev = NullDevice;
  let iface = InterfaceOptions::new(HardwareAddress::Ip)
    // A multicast address is neither unicast nor unspecified.
    .with_ip_addr(IpCidr::new(IpAddress::v4(224, 0, 0, 1), 24))
    .with_random_seed(1);
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::NonUnicastIpAddress(_)
  ));
}

/// A route whose gateway is not unicast would crash the running node at its first
/// off-link egress (smoltcp's neighbor lookup asserts the address is unicast), so
/// it is refused at construction.
#[test]
fn a_non_unicast_route_gateway_is_rejected() {
  let mut dev = NullDevice;
  let iface = InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
    .with_route(Route::new_ipv4_gateway(Ipv4Address::new(224, 0, 0, 1)))
    .with_random_seed(1);
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::NonUnicastRouteGateway(_)
  ));
}

/// A route to an IPv4 prefix via an IPv6 gateway can never resolve a next hop, so
/// it is refused rather than silently failing at egress.
#[test]
fn a_route_family_mismatch_is_rejected() {
  let mut dev = NullDevice;
  let crossed = Route {
    cidr: IpCidr::new(IpAddress::v4(10, 0, 0, 0), 24),
    via_router: IpAddress::v6(0xfe80, 0, 0, 0, 0, 0, 0, 1),
    preferred_until: None,
    expires_at: None,
  };
  let iface = InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
    .with_route(crossed)
    .with_random_seed(1);
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::RouteFamilyMismatch(_)
  ));
}

/// More addresses (or routes) than smoltcp's fixed interface tables can hold is a
/// typed error, not a silently-truncated interface that would drop the traffic
/// bound for the addresses that did not fit.
#[test]
fn over_capacity_interface_tables_are_rejected() {
  let mut dev = NullDevice;

  // smoltcp's address table is a fixed `heapless::Vec`; overflow it comfortably.
  let mut iface = InterfaceOptions::new(HardwareAddress::Ip).with_random_seed(1);
  for i in 1..=32u8 {
    iface = iface.with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, i), 24));
  }
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::TooManyIpAddresses
  ));

  // Likewise the route table.
  let mut iface = InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
    .with_random_seed(1);
  for i in 1..=32u8 {
    iface = iface.with_route(Route::new_ipv4_gateway(Ipv4Address::new(10, 0, 0, i)));
  }
  assert!(matches!(
    init_error(Options::new(), iface, &mut dev),
    crate::InitError::TooManyRoutes
  ));
}

/// The socket-sizing screens: each one guards a configuration smoltcp would either
/// panic on or silently accept as a permanently-dead plane.
#[test]
fn unusable_socket_sizing_is_rejected() {
  let iface = || {
    InterfaceOptions::new(HardwareAddress::Ip)
      .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
      .with_random_seed(1)
  };

  // A listener plus one dial/accept socket is the functional minimum: with one
  // socket the listener holds it and the node could never dial a seed.
  let mut dev = NullDevice;
  let mut cfg = Options::new();
  cfg.tcp_pool_size = 1;
  assert!(matches!(
    init_error(cfg, iface(), &mut dev),
    crate::InitError::TcpPoolTooSmall
  ));

  // smoltcp's `RingBuffer::new` accepts a zero-length ring and builds a socket that
  // can never receive — a silently-dead reliable plane.
  let mut cfg = Options::new();
  cfg.tcp_socket_rx_bytes = 0;
  assert!(matches!(
    init_error(cfg, iface(), &mut dev),
    crate::InitError::ZeroTcpSocketBuffer
  ));

  // smoltcp `panic!`s on a receive buffer past 1 GiB.
  let mut cfg = Options::new();
  cfg.tcp_socket_rx_bytes = (1 << 30) + 1;
  assert!(matches!(
    init_error(cfg, iface(), &mut dev),
    crate::InitError::TcpRxBufferTooLarge
  ));

  // Zero packet-metadata slots is a gossip ring that can never enqueue a datagram.
  let mut cfg = Options::new();
  cfg.udp_rx_packets = 0;
  assert!(matches!(
    init_error(cfg, iface(), &mut dev),
    crate::InitError::ZeroUdpPackets
  ));
}

/// The close timeout bounds the graceful reliable-close drain; a zero one would
/// force-abort every close immediately, truncating an in-flight push/pull response.
#[test]
fn a_zero_close_timeout_is_rejected() {
  let mut dev = NullDevice;
  let iface = InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
    .with_random_seed(1);
  assert!(matches!(
    init_error(
      Options::new().with_close_timeout(core::time::Duration::ZERO),
      iface,
      &mut dev
    ),
    crate::InitError::ZeroCloseTimeout
  ));
}

// ── the handle's read surface and command forwards ────────────────────────────

/// Pump `node` once over `gossip` at `at`, giving the engine the real
/// reliable-plane view. Also turns the smoltcp stack so a dialing socket advances.
fn pump_at(
  node: &mut Serf<SmolStr, SocketAddr, NullDevice>,
  gossip: &mut FloodGossip,
  at: Instant,
) {
  {
    let sockets = RefCell::new(&mut node.sockets);
    let mut stream = SmoltcpStream::new(&mut node.iface, &sockets);
    node.engine.pump(at, gossip, &mut stream);
  }
  let mut dev = NullDevice;
  node.iface.poll(
    crate::addr::to_smoltcp_instant(at),
    &mut dev,
    &mut node.sockets,
  );
}

/// Pump `node` once at the fixed base instant.
fn pump_over(node: &mut Serf<SmolStr, SocketAddr, NullDevice>, gossip: &mut FloodGossip) {
  pump_at(node, gossip, now());
}

/// The base instant advanced by `secs`.
fn advanced(secs: u64) -> Instant {
  Instant::from_origin(core::time::Duration::from_secs(86_400 + secs))
}

/// A gossip seam that delivers nothing — for pumps that only need to turn the
/// engine's own schedulers.
fn silent_gossip() -> FloodGossip {
  FloodGossip {
    frames: Vec::new(),
    idx: 0,
    src: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946),
  }
}

/// The reliable-plane and clock accessors report the engine's live state: a fresh
/// node has armed its listener out of the pool with nothing in flight, and each
/// command advances exactly the clock it owns.
#[test]
fn the_read_surface_reports_the_live_engine_view() {
  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  // The listener is armed from the pool; nothing else is in flight yet.
  assert!(node.listener_present(), "construction arms the listener");
  assert_eq!(
    node.pool_free_count(),
    Options::new().tcp_pool_size - 1,
    "the listener holds one pooled socket; the rest are free to dial"
  );
  assert_eq!(node.accepted_inbound_count(), 0);
  assert_eq!(node.closing_count(), 0);
  assert_eq!(node.half_closed_count(), 0);
  assert_eq!(node.pending_dial_count(), 0);
  assert_eq!(node.pending_join_count(), 0);
  assert_eq!(node.events_dropped(), 0);

  // Each command advances only its own Lamport clock.
  let (m0, e0, q0) = (node.member_time(), node.event_time(), node.query_time());
  node
    .user_event("evt", bytes::Bytes::from_static(b"p"), false, now())
    .expect("a user event is accepted while running");
  assert!(
    node.event_time() > e0,
    "a user event advances the event clock"
  );
  assert_eq!(node.query_time(), q0, "and leaves the query clock alone");

  node
    .query(
      "q",
      bytes::Bytes::from_static(b"p"),
      Default::default(),
      now(),
    )
    .expect("a query is accepted while running");
  assert!(node.query_time() > q0, "a query advances the query clock");

  // Replacing the local tags re-advertises the node's metadata and refreshes the
  // local member in the store. It is a metadata refresh, NOT a membership event,
  // so serf's member clock — which orders join/leave intents — must not move.
  let tags: crate::Tags = [("role", "leader")].into_iter().collect();
  node.set_tags(tags, now()).expect("set_tags while running");
  let local = node
    .members()
    .into_iter()
    .find(|m| m.node().id_ref() == "a")
    .expect("the local node is a member of its own view");
  assert_eq!(
    local.tags().0.get("role").map(smol_str::SmolStr::as_str),
    Some("leader"),
    "the refreshed local member must carry the new tags"
  );
  assert_eq!(
    node.member_time(),
    m0,
    "a tag refresh is not a membership intent and must not advance the member clock"
  );
  assert_eq!(node.members().len(), node.num_members());
}

/// A queued seed is visible as pending work until it is dispatched, and cancelling
/// the join reaps it: the handle must not leak a seed the caller gave up on.
#[test]
fn a_cancelled_join_reaps_its_queued_seed() {
  use crate::MaybeResolved;

  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  let seed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946);
  let handle = node
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(seed)],
      false,
      now(),
    )
    .expect("a join from a running node is accepted");
  assert_eq!(
    node.pending_join_count(),
    1,
    "the seed is queued for the next tick's dispatch"
  );
  assert!(
    node.poll_join(handle).is_none(),
    "the join is still in flight"
  );

  node.cancel_join(handle);
  assert_eq!(
    node.pending_join_count(),
    0,
    "cancelling must drop the still-queued seed"
  );
  assert!(
    node.poll_join(handle).is_none(),
    "a cancelled join never resolves a caller reply"
  );

  // The cancel reaped the entry outright (no exchange had started), so a repeat
  // cancel of the now-unknown handle is a no-op rather than a panic.
  node.cancel_join(handle);
  assert_eq!(node.pending_join_count(), 0);
  assert!(node.poll_join(handle).is_none());
}

/// A non-empty seed set that resolves to no wire address is a discovery FAILURE, not
/// a successful no-op join — otherwise a caller would believe it had joined a
/// cluster it never contacted.
#[test]
fn a_seed_set_resolving_to_nothing_is_a_join_failure() {
  use crate::MaybeResolved;

  /// A resolver that finds no address for any name.
  struct EmptyResolver;

  impl crate::Resolver for EmptyResolver {
    type Address = SocketAddr;
    type Error = core::convert::Infallible;

    fn resolve(&self, _addr: &Self::Address) -> Result<serf_embedded::ResolvedAddrs, Self::Error> {
      Ok(serf_embedded::ResolvedAddrs::new())
    }
  }

  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  let seed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946);
  let Err(err) = node.join(
    &EmptyResolver,
    &[MaybeResolved::Unresolved(seed)],
    false,
    now(),
  ) else {
    panic!("a seed set that resolves to nothing must not report a successful join");
  };
  assert!(err.is_no_addresses(), "{err}");
  assert_eq!(node.pending_join_count(), 0, "no seed was queued");
}

/// An operator-driven `force_leave` takes a known peer out of the local view rather
/// than being a silent no-op, and a per-member reconnect policy can be installed and
/// cleared on a running node.
#[test]
fn force_leave_removes_a_known_peer_from_the_local_view() {
  use core::time::Duration;
  use serf_proto::members::Member;

  /// A reconnect policy pinning every member to a fixed timeout.
  struct FixedReconnect(Duration);

  impl serf_embedded::ReconnectDelegate<SmolStr, SocketAddr> for FixedReconnect {
    fn reconnect_timeout(&self, _m: &Member<SmolStr, SocketAddr>, _t: Duration) -> Duration {
      self.0
    }
  }

  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  node.set_reconnect_delegate(Some(std::boxed::Box::new(FixedReconnect(
    Duration::from_secs(30),
  ))));

  // Learn one peer over the gossip plane.
  let mut gossip = FloodGossip {
    frames: std::vec![alive_frame(0)],
    idx: 0,
    src: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946),
  };
  pump_over(&mut node, &mut gossip);
  let peer = SmolStr::from("flood-0");
  assert!(
    node.members().iter().any(|m| m.node().id_ref() == &peer),
    "the gossiped alive must be admitted as a member"
  );

  node
    .force_leave(peer.clone(), false, now())
    .expect("force_leave of a known member is accepted");
  pump_over(&mut node, &mut silent_gossip());

  let seen = node
    .members()
    .into_iter()
    .find(|m| m.node().id_ref() == &peer);
  match seen {
    None => {}
    Some(m) => assert_ne!(
      m.status(),
      crate::MemberStatus::Alive,
      "a force-left peer must not remain Alive in the local view"
    ),
  }

  // Clearing the override is equally accepted on a running node.
  node.set_reconnect_delegate(None);
}

// ── the engine's dial-failure, pool, and join-abandonment paths ───────────────

/// Build a node over an arbitrary [`Options`], advertising [`LOCAL`].
fn try_build_with(
  cfg: Options,
  device: &mut NullDevice,
) -> Result<Serf<SmolStr, SocketAddr, NullDevice>, crate::InitError> {
  Serf::try_new(
    cfg,
    ip_iface(1),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("a"), LOCAL),
    SerfOptions::new(),
    &SocketAddrResolver,
    device,
    now(),
  )
}

/// A policy admitting only `allow`, blocking everything else.
#[cfg(feature = "cidr")]
fn only(allow: &str) -> serf_embedded::CidrPolicy {
  let mut policy = serf_embedded::CidrPolicy::block_all();
  policy.add(allow.parse().expect("a well-formed CIDR parses"));
  policy
}

/// A seed the CIDR policy blocks is a dial FAILURE, not a benign no-op: the socket
/// is reclaimed to the pool, the exchange terminalizes, and the await-result join
/// resolves as reaching NO seed — never as a silent success.
#[cfg(feature = "cidr")]
#[test]
fn a_cidr_blocked_seed_fails_the_join_and_reclaims_its_socket() {
  use crate::MaybeResolved;

  let mut dev = NullDevice;
  // Only the local node's own address is admitted; the seed below is blocked.
  let mut node = try_build_with(
    Options::new().with_cidr_policy(only("10.0.0.1/32")),
    &mut dev,
  )
  .expect("a valid configuration constructs");
  node.start(now());
  let free_before = node.pool_free_count();

  let seed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946);
  // `ignore_old` records an ignore token per started exchange; the terminal must
  // clear it, or the machine's ignore set leaks.
  let handle = node
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(seed)],
      true,
      now(),
    )
    .expect("a join from a running node is accepted");

  // The pump dispatches the seed, dials it, and the CIDR screen fails the dial.
  // `poll_join` delivers exactly once, so the outcome is captured as it resolves.
  let mut outcome = None;
  for _ in 0..8 {
    pump_over(&mut node, &mut silent_gossip());
    if let Some(o) = node.poll_join(handle) {
      outcome = Some(o);
      break;
    }
  }

  let Some(Err(failed)) = outcome else {
    panic!("a blocked seed must resolve the join as reaching none, got {outcome:?}");
  };
  assert_eq!(failed.requested(), 1);
  assert_eq!(failed.contacted(), 0);

  assert_eq!(
    node.pool_free_count(),
    free_before,
    "the failed dial must return its socket to the pool"
  );
  assert_eq!(
    node.pending_dial_count(),
    0,
    "no dial may stay parked after the failure"
  );
  assert_eq!(
    node.pending_join_count(),
    0,
    "the join was fully dispatched"
  );
}

/// With the pool exhausted, a dial intent is PARKED rather than lost, and picked up
/// once a slot frees — otherwise a join with more seeds than pooled sockets would
/// silently never dispatch the surplus.
///
/// The seeds are routable but unanswerable (the null device delivers nothing), so
/// the first dial HOLDS its socket in the TCP handshake until the machine's
/// exchange deadline force-aborts it and returns the slot.
#[test]
fn an_exhausted_pool_parks_a_dial_until_a_slot_frees() {
  use crate::MaybeResolved;

  let mut dev = NullDevice;
  // Two sockets: one becomes the listener, leaving exactly ONE to dial with — so a
  // two-seed join must park its second dial.
  let mut cfg = Options::new();
  cfg.tcp_pool_size = 2;
  let mut node = try_build_with(cfg, &mut dev).expect("a valid configuration constructs");
  node.start(now());
  assert_eq!(
    node.pool_free_count(),
    1,
    "the listener holds the other slot"
  );

  let seeds = [
    MaybeResolved::Resolved(SocketAddr::new(
      IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
      7946,
    )),
    MaybeResolved::Resolved(SocketAddr::new(
      IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
      7946,
    )),
  ];
  let handle = node
    .join(&SocketAddrResolver, &seeds, false, now())
    .expect("a join from a running node is accepted");

  // The first dial consumes the only free slot and stays in the handshake; the
  // second has nowhere to go and MUST be parked rather than dropped.
  pump_over(&mut node, &mut silent_gossip());
  assert_eq!(
    node.pool_free_count(),
    0,
    "the sole free slot backs the first dial"
  );
  assert_eq!(
    node.pending_dial_count(),
    1,
    "the surplus dial must be parked, not lost"
  );

  // A pump while the pool is still empty leaves the parked dial parked.
  pump_over(&mut node, &mut silent_gossip());
  assert_eq!(
    node.pending_dial_count(),
    1,
    "a parked dial stays parked while no slot is free"
  );

  // Past the machine's exchange deadline both dials terminalize; the freed slot is
  // handed to the parked dial, and the join resolves having reached no seed.
  let mut outcome = None;
  for step in 1..=40u64 {
    let at = advanced(step * 5);
    pump_at(&mut node, &mut silent_gossip(), at);
    if let Some(o) = node.poll_join(handle) {
      outcome = Some(o);
      break;
    }
  }

  let Some(Err(failed)) = outcome else {
    panic!("both unanswerable seeds must resolve the join as reaching none, got {outcome:?}");
  };
  assert_eq!(
    failed.requested(),
    2,
    "BOTH seeds must have been dispatched, including the parked one"
  );
  assert_eq!(failed.contacted(), 0);
  assert_eq!(
    node.pending_dial_count(),
    0,
    "no dial intent may be left parked once the exchanges terminalize"
  );
}

/// The CIDR policy composes with the built-in routable filter at membership
/// admission (logical AND), so a peer whose datagram arrives from an ADMITTED source
/// but whose self-advertised address is outside the policy is not admitted — its bad
/// address is never stored and never re-gossiped.
#[cfg(feature = "cidr")]
#[test]
fn the_cidr_policy_composes_with_the_routable_filter_at_admission() {
  let mut dev = NullDevice;
  // The gossip source (10.0.0.2) is admitted at the transport boundary; only
  // addresses inside 10.0.0.0/24 pass membership admission.
  let mut node = try_build_with(
    Options::new().with_cidr_policy(only("10.0.0.0/24")),
    &mut dev,
  )
  .expect("a valid configuration constructs");
  node.start(now());

  let admitted = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 7946);
  let outside = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)), 7946);
  let mut gossip = FloodGossip {
    frames: std::vec![
      alive_frame_at("inside", admitted),
      alive_frame_at("outside", outside),
    ],
    idx: 0,
    src: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946),
  };
  pump_over(&mut node, &mut gossip);

  let ids: std::vec::Vec<SmolStr> = node
    .members()
    .iter()
    .map(|m| m.node().id_ref().clone())
    .collect();
  assert!(
    ids.iter().any(|id| id == "inside"),
    "a peer advertising an admitted address must join: {ids:?}"
  );
  assert!(
    !ids.iter().any(|id| id == "outside"),
    "a peer advertising an address outside the policy must NOT be admitted: {ids:?}"
  );
}

/// Cancelling a join whose push/pull already dispatched forgets the caller reply
/// (so `poll_join` never yields) while the exchange runs to its own terminal, and a
/// repeat cancel of a reaped handle is a no-op rather than a panic.
#[test]
fn cancelling_a_dispatched_join_forgets_its_reply() {
  use crate::MaybeResolved;

  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  let seed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946);
  let handle = node
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(seed)],
      false,
      now(),
    )
    .expect("a join from a running node is accepted");

  // One pump dispatches the seed, so the exchange has STARTED when the cancel lands.
  pump_over(&mut node, &mut silent_gossip());
  assert_eq!(node.pending_join_count(), 1);

  node.cancel_join(handle);
  assert!(
    node.poll_join(handle).is_none(),
    "a cancelled join must never hand back a caller reply"
  );

  // Cancelling again — a handle the fold may already have reaped — is a no-op.
  node.cancel_join(handle);
  assert!(node.poll_join(handle).is_none());
}

/// Leaving hands every in-flight await-result join its reply once, from the set it
/// had actually reached: the pump initiates no further push/pull after a leave, so a
/// join that reached nothing must resolve as FAILED rather than hang forever.
#[test]
fn leaving_resolves_every_in_flight_join() {
  use crate::MaybeResolved;

  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  let seed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946);
  let handle = node
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(seed)],
      false,
      now(),
    )
    .expect("a join from a running node is accepted");
  assert!(
    node.poll_join(handle).is_none(),
    "the join is still in flight before the leave"
  );

  node.leave(now()).expect("leave from a running node");

  let Some(Err(failed)) = node.poll_join(handle) else {
    panic!("a leave must resolve the in-flight join rather than strand it");
  };
  assert_eq!(failed.contacted(), 0, "the join had reached no seed");
  assert_eq!(
    node.pending_join_count(),
    0,
    "the leave dropped the queued seed"
  );
}

/// An app that never drains `poll_event` cannot grow the driver's buffer without
/// bound: past the cap the OLDEST buffered events are shed and counted, and the
/// public counter reports the loss rather than hiding it.
#[test]
fn the_app_event_buffer_sheds_the_oldest_when_the_app_never_polls() {
  let mut dev = NullDevice;
  let mut node = try_build(LOCAL, &mut dev).expect("a valid configuration constructs");
  node.start(now());

  // Two floods, each drained into the driver's queue without the app polling: the
  // first fills it to the cap, the second must shed to stay bounded.
  let mut next_id = 0usize;
  for _ in 0..2 {
    let frames: std::vec::Vec<std::vec::Vec<u8>> = (0..DEFAULT_EVENT_BUFFER_CAP)
      .map(|_| {
        let f = alive_frame(next_id);
        next_id += 1;
        f
      })
      .collect();
    let mut gossip = FloodGossip {
      frames,
      idx: 0,
      src: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 7946),
    };
    pump_over(&mut node, &mut gossip);
    node.drain_engine_events(now());
  }

  assert!(
    node.app_events.len() <= DEFAULT_EVENT_BUFFER_CAP,
    "the driver's buffer must stay bounded at the cap, got {}",
    node.app_events.len()
  );
  assert!(
    node.app_events_dropped > 0,
    "the second drain must shed the oldest buffered events"
  );
  assert!(
    node.events_dropped() >= node.app_events_dropped,
    "the public counter must include the driver-side shedding"
  );
}
