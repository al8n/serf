//! Deterministic two-node test substrate: a paired in-memory smoltcp `Device`
//! (one node's TX is the other's RX) on `medium::Ip`, plus a virtual clock.
#![allow(dead_code)]

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use core::net::IpAddr;

use serf_smoltcp::{HardwareAddress, InterfaceOptions, IpCidr};
use smoltcp::{
  phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken},
  time::Instant,
};

/// Build an [`InterfaceOptions`] for the harness's `Medium::Ip` devices:
/// `HardwareAddress::Ip` plus the node's `/24` address.
///
/// The interface RNG seed is derived deterministically from the node's IP, so a
/// harness node's whole stack — smoltcp ports/ISNs and the gossip / serf RNG
/// schedules the driver derives from this seed — is reproducible across runs.
pub fn ip_iface(ip: IpAddr) -> InterfaceOptions {
  let seed = match ip {
    IpAddr::V4(v4) => u32::from(v4) as u64,
    IpAddr::V6(v6) => {
      let o = v6.octets();
      u64::from_be_bytes([o[8], o[9], o[10], o[11], o[12], o[13], o[14], o[15]])
    }
  };
  InterfaceOptions::new(HardwareAddress::Ip)
    .with_ip_addr(IpCidr::new(ip.into(), 24))
    .with_random_seed(seed)
}

type Wire = Rc<RefCell<VecDeque<Vec<u8>>>>;

/// One end of a virtual link: reads from `rx`, writes to `tx`.
pub struct PairedDevice {
  rx: Wire,
  tx: Wire,
  mtu: usize,
}

impl PairedDevice {
  /// Whether a frame has been delivered to this node's receive queue but not yet
  /// drained by a `poll`. A deadline-driven loop uses this to model "an arriving
  /// packet wakes its receiver": while any node has an inbound frame pending, the
  /// loop re-polls promptly instead of sleeping the shared clock to a far timer.
  pub fn inbound_pending(&self) -> bool {
    !self.rx.borrow().is_empty()
  }
}

/// Build the two ends of one virtual link.
///
/// Frames sent by the `A` end arrive at the `B` end's receive queue, and vice
/// versa — the two FIFOs are cross-wired so `A.tx == B.rx` and `B.tx == A.rx`.
pub fn link(mtu: usize) -> (PairedDevice, PairedDevice) {
  let a2b: Wire = Rc::new(RefCell::new(VecDeque::new()));
  let b2a: Wire = Rc::new(RefCell::new(VecDeque::new()));
  (
    PairedDevice {
      rx: b2a.clone(),
      tx: a2b.clone(),
      mtu,
    },
    PairedDevice {
      rx: a2b,
      tx: b2a,
      mtu,
    },
  )
}

pub struct VRx(Vec<u8>);
pub struct VTx(Wire);

impl RxToken for VRx {
  fn consume<R, F>(self, f: F) -> R
  where
    F: FnOnce(&[u8]) -> R,
  {
    f(&self.0)
  }
}

impl TxToken for VTx {
  fn consume<R, F>(self, len: usize, f: F) -> R
  where
    F: FnOnce(&mut [u8]) -> R,
  {
    let mut buf = vec![0u8; len];
    let r = f(&mut buf);
    self.0.borrow_mut().push_back(buf);
    r
  }
}

impl Device for PairedDevice {
  type RxToken<'a>
    = VRx
  where
    Self: 'a;
  type TxToken<'a>
    = VTx
  where
    Self: 'a;

  fn receive(&mut self, _timestamp: Instant) -> Option<(VRx, VTx)> {
    let frame = self.rx.borrow_mut().pop_front()?;
    Some((VRx(frame), VTx(self.tx.clone())))
  }

  fn transmit(&mut self, _timestamp: Instant) -> Option<VTx> {
    Some(VTx(self.tx.clone()))
  }

  fn capabilities(&self) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ip;
    caps.max_transmission_unit = self.mtu;
    caps.checksum = ChecksumCapabilities::ignored();
    caps
  }
}

/// Deterministic virtual clock anchored away from zero.
///
/// `memberlist_proto::Instant - Duration` saturates at the origin; the
/// 86 400 s offset gives backward-aging headroom for any suspicion or failure
/// timers that need to subtract from `now`.
pub struct Clock {
  ms: u64,
}

impl Clock {
  pub fn new() -> Self {
    Self { ms: 86_400_000 }
  }

  pub fn now(&self) -> serf_smoltcp::Instant {
    serf_smoltcp::Instant::from_origin(core::time::Duration::from_millis(self.ms))
  }

  pub fn advance_ms(&mut self, by: u64) {
    self.ms += by;
  }

  /// Jump the clock forward to `target` (a deadline returned by `poll`), rounding
  /// UP to the next whole millisecond. Never moves backwards.
  pub fn advance_to(&mut self, target: serf_smoltcp::Instant) {
    let ns = target.since_origin().as_nanos();
    let target_ms = ns.div_ceil(1_000_000) as u64;
    if target_ms > self.ms {
      self.ms = target_ms;
    }
  }
}
