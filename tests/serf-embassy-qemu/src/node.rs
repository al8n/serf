//! The proof task: stand up two serf nodes over an in-memory paired embassy-net
//! link, converge them, and cross a user event on the emulated core.
//!
//! This is the bare-metal twin of `serf-embassy/tests/loopback.rs` +
//! `user_event.rs`: two embassy-net stacks (static IPs `169.254.1.1` and `.2`)
//! cross-wired by a [`paired_device`], a [`Serf`] + [`Runner`] on each, node B
//! joining node A as a seed, a wait for both to see two members, and then a user
//! event broadcast by A that B must observe as [`Event::User`]. Where the host
//! tests race every future under one `block_on`, here the four run loops (two serf
//! [`Runner`]s + two `embassy_net::Runner`s) are spawned as embassy tasks and the
//! main task drives the join, convergence, and the user-event crossing.
//!
//! All borrowed state (stack resources, socket buffers) is promoted to `'static`
//! via [`StaticCell`] so the run loops can be `'static` embassy tasks. On the
//! user-event crossing the emulator exits 0; a deadline guard exits 1.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use embassy_executor::Spawner;
use embassy_net::{
  Config as NetConfig, Ipv4Cidr, Runner as NetRunner, Stack, StackResources, StaticConfigV4,
  tcp::TcpSocket,
  udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Timer};
use semihosting::println;
use serf_embassy::{
  Bytes, EndpointOptions, Event, MaybeResolved, Options, Runner, SeedableRng, Serf, SerfOptions,
  SmallRng, SocketAddrResolver, TransformOptions, now,
};
use smol_str::SmolStr;
use static_cell::StaticCell;

use crate::paired_device::{PairedDevice, pair};

/// TCP socket pool size per node (a listener plus dial/accept sockets), matching
/// the host loopback test.
const POOL: usize = 4;
/// Per-TCP-socket rx/tx buffer bytes.
const TCP_BUF: usize = 4096;
/// Per-node UDP datagram buffer bytes (gossip datagrams are small; this holds a
/// handful in flight).
const UDP_BUF: usize = 8 * 1024;
/// Per-node UDP packet-metadata slots.
const UDP_META: usize = 16;
/// `StackResources` socket budget: the TCP pool plus headroom for UDP + DHCP-less
/// control sockets, mirroring the host test's `POOL + 2`.
const SOCKS: usize = POOL + 2;
/// Convergence + user-event deadline in virtual time. The SysTick driver advances
/// ~1 ms of virtual time per tick, so this is ~20_000 SysTicks — ample for a
/// two-node join plus a gossiped user event yet bounded so a regression fails fast.
const DEADLINE: Duration = Duration::from_secs(20);
/// Poll cadence while waiting for convergence and the user event.
const POLL: Duration = Duration::from_millis(10);

/// Gossip/TCP port both nodes bind (the single-port memberlist model serf runs on).
const PORT: u16 = 7946;

/// Build a `SocketAddr` in the link-local `169.254.1.0/24` test subnet.
fn addr(last: u8, port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 1, last)), port)
}

/// All the owned buffers one node's sockets borrow, promoted to `'static` so the
/// sockets (and the `Serf`/`Runner` holding them) are `'static`.
struct NodeBufs {
  udp_rx_meta: [PacketMetadata; UDP_META],
  udp_rx: [u8; UDP_BUF],
  udp_tx_meta: [PacketMetadata; UDP_META],
  udp_tx: [u8; UDP_BUF],
  tcp_rx: [[u8; TCP_BUF]; POOL],
  tcp_tx: [[u8; TCP_BUF]; POOL],
}

impl NodeBufs {
  const fn new() -> Self {
    Self {
      udp_rx_meta: [PacketMetadata::EMPTY; UDP_META],
      udp_rx: [0u8; UDP_BUF],
      udp_tx_meta: [PacketMetadata::EMPTY; UDP_META],
      udp_tx: [0u8; UDP_BUF],
      tcp_rx: [[0u8; TCP_BUF]; POOL],
      tcp_tx: [[0u8; TCP_BUF]; POOL],
    }
  }
}

/// Build one node's `UdpSocket` + `[TcpSocket; POOL]` over its `'static` stack and
/// `'static` bufs.
fn build_sockets(
  stack: Stack<'static>,
  bufs: &'static mut NodeBufs,
) -> (UdpSocket<'static>, [TcpSocket<'static>; POOL]) {
  let udp = UdpSocket::new(
    stack,
    &mut bufs.udp_rx_meta,
    &mut bufs.udp_rx,
    &mut bufs.udp_tx_meta,
    &mut bufs.udp_tx,
  );
  // Pair each rx buffer with its tx buffer in lockstep, yielding `POOL` sockets.
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
fn build_stack(
  device: PairedDevice,
  resources: &'static mut StackResources<SOCKS>,
  last: u8,
  seed: u64,
) -> (Stack<'static>, NetRunner<'static, PairedDevice>) {
  let config = NetConfig::ipv4_static(StaticConfigV4 {
    address: Ipv4Cidr::new(Ipv4Addr::new(169, 254, 1, last), 24),
    gateway: None,
    dns_servers: Default::default(),
  });
  embassy_net::new(device, config, resources, seed)
}

/// Drive one embassy-net stack's run loop. Two instances (one per stack) move
/// frames across the paired devices.
#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: NetRunner<'static, PairedDevice>) -> ! {
  runner.run().await
}

/// Drive one serf node's run loop (engine pump + the `POOL` reliable-plane
/// workers). Two instances, one per node. `Runner::run` returns only on a terminal
/// shutdown, which the proof never triggers, so each stays live for the run.
#[embassy_executor::task(pool_size = 2)]
async fn serf_task(runner: Runner<'static, SmolStr, POOL>) {
  runner.run().await;
}

/// The single spawned entry task: build both nodes, spawn the four run loops,
/// drive the join, wait for convergence, then cross a user event and exit the
/// emulator with the pass/fail code.
#[embassy_executor::task]
pub async fn main_task(spawner: Spawner) -> ! {
  // Promote every borrowed resource to `'static`. Distinct cells per node so the
  // two stacks and two socket sets do not alias.
  static RES_A: StaticCell<StackResources<SOCKS>> = StaticCell::new();
  static RES_B: StaticCell<StackResources<SOCKS>> = StaticCell::new();
  static BUFS_A: StaticCell<NodeBufs> = StaticCell::new();
  static BUFS_B: StaticCell<NodeBufs> = StaticCell::new();

  let (dev_a, dev_b) = pair();
  let (stack_a, net_a) = build_stack(dev_a, RES_A.init(StackResources::new()), 1, 0x1111_2222);
  let (stack_b, net_b) = build_stack(dev_b, RES_B.init(StackResources::new()), 2, 0x3333_4444);

  let (udp_a, tcp_a) = build_sockets(stack_a, BUFS_A.init(NodeBufs::new()));
  let (udp_b, tcp_b) = build_sockets(stack_b, BUFS_B.init(NodeBufs::new()));

  let clock = now();
  // Each node seeds its gossip RNG and serf's own core RNG independently so no two
  // nodes share a `(ltime, id)` query-id schedule; the fixed seeds keep the run
  // reproducible.
  let (ml_a, run_a) = Serf::new_with_rng::<_, POOL>(
    Options::new(),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("a"), addr(1, PORT)),
    SerfOptions::new(),
    &SocketAddrResolver,
    udp_a,
    tcp_a,
    clock,
    SmallRng::seed_from_u64(1),
    SmallRng::seed_from_u64(101),
  )
  .await
  .expect("build node a");
  let (ml_b, run_b) = Serf::new_with_rng::<_, POOL>(
    Options::new(),
    TransformOptions::default(),
    EndpointOptions::new(SmolStr::new("b"), addr(2, PORT)),
    SerfOptions::new(),
    &SocketAddrResolver,
    udp_b,
    tcp_b,
    clock,
    SmallRng::seed_from_u64(2),
    SmallRng::seed_from_u64(102),
  )
  .await
  .expect("build node b");

  // Spawn the four run loops. `must_spawn`: each pool slot is used exactly once,
  // so the spawn cannot fail.
  spawner.must_spawn(net_task(net_a));
  spawner.must_spawn(net_task(net_b));
  spawner.must_spawn(serf_task(run_a));
  spawner.must_spawn(serf_task(run_b));

  // B joins A as a seed; the convergence wait below bounds success by the deadline.
  // Ignoring Err: a failed seed join surfaces as the convergence loop timing out,
  // which the proof already treats as the failure signal. `ignore_old = false`
  // keeps the default replay of A's pre-join user events (there are none here).
  let _ = ml_b
    .join(
      &SocketAddrResolver,
      &[MaybeResolved::Resolved(addr(1, PORT))],
      false,
    )
    .await;
  println!("join issued; waiting for convergence");

  let mut waited = Duration::from_secs(0);

  // Phase 1: both views reach two members (A learns B from the push/pull a tick
  // after the exchange).
  loop {
    let a = ml_a.num_members();
    let b = ml_b.num_members();
    if a == 2 && b == 2 {
      println!("converged: A and B both see 2 members");
      break;
    }
    if waited >= DEADLINE {
      println!("timed out before convergence: a={} b={}", a, b);
      semihosting::process::exit(1);
    }
    Timer::after(POLL).await;
    waited += POLL;
  }

  // Phase 2: the serf-above-memberlist proof — A broadcasts a user event and B must
  // observe it as `Event::User`, which can only happen if serf's gossip plane truly
  // disseminated it across the emulated link, not just SWIM membership.
  ml_a
    .user_event("greet", Bytes::from_static(b"hello"), false)
    .expect("broadcast a user event from a running node");
  println!("user event broadcast; waiting for B to observe it");

  loop {
    match ml_b.poll_event() {
      Some(Event::User(_)) => {
        println!("user event crossed: B observed Event::User; proof complete");
        semihosting::process::exit(0);
      }
      // Drain and ignore any other buffered events (e.g. the membership batch from
      // the join) and keep polling for the user event.
      Some(_) => continue,
      None => {
        if waited >= DEADLINE {
          println!("timed out before B observed the user event");
          semihosting::process::exit(1);
        }
        Timer::after(POLL).await;
        waited += POLL;
      }
    }
  }
}
