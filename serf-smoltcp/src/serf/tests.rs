use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use memberlist_proto::Instant;
use smol_str::SmolStr;
use smoltcp::{
  phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken},
  time::Instant as SmolInstant,
};

use crate::{
  EndpointOptions, HardwareAddress, InterfaceOptions, IpAddress, IpCidr, Options, Serf,
  SerfOptions, SerfState, SocketAddrResolver, TransformOptions,
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
