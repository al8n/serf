//! Shared two-node cluster scaffolding over the paired embassy-net driver: the
//! per-node socket buffers, stack + socket construction, node construction, and
//! the `drive` harness that runs both serf loops, both stack loops, and the
//! operation under test against a wall-clock timeout.

#![allow(dead_code)]

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use embassy_futures::select::{Either, select};
use embassy_net::{
  Config as NetConfig, Ipv4Cidr, Runner as NetRunner, Stack, StackResources, StaticConfigV4,
  tcp::TcpSocket,
  udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use memberlist_proto::{Instant, SeedableRng, SmallRng};
use serf_embassy::{
  EndpointOptions, MaybeResolved, Options, Runner, Serf, SerfOptions, SocketAddrResolver,
  TransformOptions,
};
use smol_str::SmolStr;

use super::paired_device::{PairedDevice, pair};

/// TCP socket pool size per node (a listener plus dial/accept sockets).
pub const POOL: usize = 4;
/// Per-TCP-socket rx/tx buffer bytes.
pub const TCP_BUF: usize = 4096;
/// Wall-clock cap on each test so a wedged plane fails fast.
/// Wall-clock cap on each test so a wedged plane fails fast. Generous because a
/// serf key-management query runs to its own multi-second deadline, and the key
/// rotation suite issues three of them back to back.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A node's `169.254.1.<last>:<port>` wire address.
pub fn addr(last: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 1, last)), port)
}

/// All the owned buffers one node's sockets borrow. Declared in the test frame so
/// the sockets (and the `Serf`/`Runner` that hold them) can borrow them for the
/// whole `block_on`.
pub struct NodeBufs {
  udp_rx_meta: [PacketMetadata; 16],
  udp_rx: [u8; 16 * 1024],
  udp_tx_meta: [PacketMetadata; 16],
  udp_tx: [u8; 16 * 1024],
  tcp_rx: [[u8; TCP_BUF]; POOL],
  tcp_tx: [[u8; TCP_BUF]; POOL],
}

impl Default for NodeBufs {
  fn default() -> Self {
    Self::new()
  }
}

impl NodeBufs {
  pub fn new() -> Self {
    Self {
      udp_rx_meta: [PacketMetadata::EMPTY; 16],
      udp_rx: [0u8; 16 * 1024],
      udp_tx_meta: [PacketMetadata::EMPTY; 16],
      udp_tx: [0u8; 16 * 1024],
      tcp_rx: [[0u8; TCP_BUF]; POOL],
      tcp_tx: [[0u8; TCP_BUF]; POOL],
    }
  }
}

/// Build one node's `UdpSocket` + `[TcpSocket; POOL]` over its `Stack` and bufs.
pub fn build_sockets<'a>(
  stack: Stack<'a>,
  bufs: &'a mut NodeBufs,
) -> (UdpSocket<'a>, [TcpSocket<'a>; POOL]) {
  let udp = UdpSocket::new(
    stack,
    &mut bufs.udp_rx_meta,
    &mut bufs.udp_rx,
    &mut bufs.udp_tx_meta,
    &mut bufs.udp_tx,
  );
  let mut rx_iter = bufs.tcp_rx.iter_mut();
  let mut tx_iter = bufs.tcp_tx.iter_mut();
  let tcp = core::array::from_fn::<_, POOL, _>(|_| {
    let rx = rx_iter.next().expect("POOL rx buffers");
    let tx = tx_iter.next().expect("POOL tx buffers");
    TcpSocket::new(stack, rx, tx)
  });
  (udp, tcp)
}

/// Build a static-IPv4 embassy-net stack over a paired device.
pub fn build_stack<'a>(
  device: PairedDevice,
  resources: &'a mut StackResources<{ POOL + 2 }>,
  last: u8,
  seed: u64,
) -> (Stack<'a>, NetRunner<'a, PairedDevice>) {
  let config = NetConfig::ipv4_static(StaticConfigV4 {
    address: Ipv4Cidr::new(Ipv4Addr::new(169, 254, 1, last), 16),
    gateway: None,
    dns_servers: Default::default(),
  });
  embassy_net::new(device, config, resources, seed)
}

/// Build one serf node over its sockets, seeding the gossip and serf RNGs
/// distinctly (from `last` so two nodes never share a schedule).
pub fn build_node<'a>(
  udp: UdpSocket<'a>,
  tcp: [TcpSocket<'a>; POOL],
  id: &str,
  last: u8,
  now_: Instant,
  transform: TransformOptions,
) -> (Serf<SmolStr, SocketAddr>, Runner<'a, SmolStr, POOL>) {
  block_on(Serf::new_with_rng::<_, POOL>(
    Options::new(),
    transform,
    EndpointOptions::new(SmolStr::new(id), addr(last, 7946)),
    SerfOptions::new(),
    &SocketAddrResolver,
    udp,
    tcp,
    now_,
    SmallRng::seed_from_u64(u64::from(last)),
    SmallRng::seed_from_u64(u64::from(last) + 100),
  ))
  .expect("build node")
}

/// Convenience: create the two paired stacks and return their runners plus a
/// closure-free tuple of the pieces a test drives. Kept as separate calls in each
/// test so the borrows (buffers / resources) live in the test frame.
pub type PairedStacks<'a> = (Stack<'a>, NetRunner<'a, PairedDevice>);

/// B joins A as a seed and both converge on a 2-member view. Panics via the outer
/// timeout if convergence stalls.
pub async fn join_and_converge(a: &Serf<SmolStr, SocketAddr>, b: &Serf<SmolStr, SocketAddr>) {
  b.join(
    &SocketAddrResolver,
    &[MaybeResolved::Resolved(addr(1, 7946))],
    false,
  )
  .await
  .expect("join from a running node");
  loop {
    if a.num_members() == 2 && b.num_members() == 2 {
      break;
    }
    Timer::after(Duration::from_millis(10)).await;
  }
}

/// Drive `op` to completion against both serf run loops, both embassy-net stack
/// run loops, and the test timeout. Returns the op's value, or panics on timeout.
pub async fn drive<T>(
  op: impl core::future::Future<Output = T>,
  ml_a: Runner<'_, SmolStr, POOL>,
  ml_b: Runner<'_, SmolStr, POOL>,
  net_a: &mut NetRunner<'_, PairedDevice>,
  net_b: &mut NetRunner<'_, PairedDevice>,
) -> T {
  let nets = select(net_a.run(), net_b.run());
  let mls = select(ml_a.run(), ml_b.run());
  let infra = select(nets, mls);
  match select(op, select(infra, Timer::after(TEST_TIMEOUT))).await {
    Either::First(v) => v,
    Either::Second(_) => panic!("test timed out after {TEST_TIMEOUT:?}"),
  }
}

/// Build the two cross-wired paired devices for a test.
pub fn devices() -> (PairedDevice, PairedDevice) {
  pair()
}
