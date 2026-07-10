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
  DEFAULT_EVENT_BUFFER_CAP, EndpointOptions, HardwareAddress, InterfaceOptions, IpAddress, IpCidr,
  Options, Serf, SerfOptions, SerfState, SocketAddrResolver, TransformOptions,
  stream_io::SmoltcpStream,
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

/// Encode a well-formed gossip datagram carrying a single `Alive` for a distinct
/// node id/address, using the default (unlabelled, unencrypted) codec so the engine
/// decodes it exactly as any inbound gossip frame.
fn alive_frame(i: usize) -> Vec<u8> {
  let ip = Ipv4Addr::new(10, 1, (i / 250) as u8, (i % 250 + 1) as u8);
  let node = Node::new(
    SmolStr::from(std::format!("flood-{i}")),
    SocketAddr::new(IpAddr::V4(ip), 7946),
  );
  let alive = Alive::new(1, node)
    .with_meta(Meta::empty())
    .with_protocol_version(ProtocolVersion::V1)
    .with_delegate_version(DelegateVersion::V1);
  let msg: Message<SmolStr, SocketAddr> = Message::Alive(alive);
  encode_outgoing(&msg, &EncodeOptions::new(None))
    .expect("encode alive gossip frame")
    .to_vec()
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
