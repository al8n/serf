//! serf handle: construction, accessors, the caller-owned poll loop, and the
//! serf command surface.
//!
//! [`Serf`] wraps the transport-agnostic [`SerfEngine`](serf_embedded::SerfEngine)
//! with a smoltcp TCP/IP stack. The caller owns the device (`D`) and drives the
//! node by calling [`poll`](Serf::poll) in a super-loop: it ticks the smoltcp
//! stack, drives the engine over a [`SmoltcpGossip`] + [`SmoltcpStream`] view of
//! the just-ticked sockets, then acts on serf's mandatory driver-actioned events
//! IN the poll cycle (a lost id-conflict [`Event::Shutdown`] flips a stop flag; an
//! encryption [`Event::KeyRequest`] is applied to the engine's live wire keyring and
//! answered, both through the engine's `handle_key_request`), buffering every drained
//! event for the app's own
//! [`poll_event`](Serf::poll_event). All protocol work lives in the shared engine;
//! this driver supplies only the link layer plus the mandatory-event side effects.

use core::{
  cell::RefCell,
  hash::Hash,
  marker::PhantomData,
  net::{IpAddr, SocketAddr},
};

use std::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};

use memberlist_proto::{EndpointOptions, Instant, Rng, SeedableRng, SmallRng};
use serf_embedded::{
  DEFAULT_EVENT_BUFFER_CAP, Event, JoinFailed, JoinId, MaybeResolved, ReachedSet, SerfEngine,
  SerfError, SerfOptions, validate_runtime_config,
};
use serf_proto::{
  endpoint::{QueryId, QueryParams},
  event::QueryEvent,
  members::{Member, SerfState},
  typed::Tags,
};
use smoltcp::{
  iface::{Config as IfConfig, Interface, SocketHandle, SocketSet},
  phy::Device,
  socket::{tcp, udp},
};

#[cfg(encryption)]
use serf_embedded::{Keyring, SecretKey};

use crate::{
  InitError, InterfaceOptions, JoinError, Options, Resolver, TransformOptions,
  addr::{from_smoltcp_instant, to_endpoint, to_smoltcp_instant},
  error::{GossipMtuTooLarge, MediumMismatch},
  gossip_io::SmoltcpGossip,
  interface::{HardwareAddress, Medium},
  stream_io::SmoltcpStream,
};

/// The maximum UDP payload (`u16` length minus the 8-byte UDP header), the
/// hard ceiling for an on-wire gossip datagram. Matches the async drivers.
const UDP_PAYLOAD_MAX: usize = 65507;

/// The largest the encrypted wrapper can inflate a gossip datagram, or `0` when
/// no encryption backend is built in. serf's gossip plane carries only the
/// encryption wrapper (no checksum / compression), so this is the whole on-wire
/// inflation the arena/ceiling arithmetic must accommodate.
#[cfg(encryption)]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = memberlist_proto::ENCRYPTED_WRAPPER_OVERHEAD;
#[cfg(not(encryption))]
const ENCRYPTED_WRAPPER_OVERHEAD: usize = 0;

/// The largest per-socket TCP receive-buffer smoltcp accepts: 1 GiB.
///
/// smoltcp's `tcp::Socket::new` `panic!`s when the receive-buffer capacity
/// exceeds this (`if rx_capacity > (1 << 30)`, socket/tcp.rs), derived from the
/// RFC 1323 window-scale ceiling of 2^30. A caller-supplied
/// [`Options::tcp_socket_rx_bytes`](crate::Options::tcp_socket_rx_bytes) past it
/// would panic inside the fallible constructor, so `try_new` rejects it first.
/// The transmit buffer has no such limit and is not capped.
const TCP_RX_BUFFER_MAX: usize = 1 << 30;

/// The most pump → drain passes one [`Serf::poll`] makes to reach quiescence.
///
/// A single `poll` re-pumps while a pass produced new work the current deadline /
/// egress has not yet reflected: a `respond_key` the drain just queued, or a
/// self-addressed datagram the pump's egress just looped back (see
/// [`SmoltcpGossip`]). Each self query / key response settles in a few passes; this
/// caps a pathological self-delivery cycle so a single `poll` cannot spin forever.
/// On hitting the cap with work still pending, `poll` folds `now` into its returned
/// deadline so the caller re-polls at once rather than sleeping past it.
const MAX_SELF_DELIVERY_ITERS: usize = 8;

/// Whether `addr` is a destination the smoltcp stack can actually use.
///
/// `to_endpoint(*addr).addr.is_unicast()` calls smoltcp's OWN `IpAddress::is_unicast`
/// — the exact function its route/neighbor lookups assert on — so the driver and
/// smoltcp agree byte-for-byte on what "routable" means. This mirrors the engine's
/// transport-neutral `socket_addr_is_routable` (`!(broadcast || multicast ||
/// unspecified)` plus `port != 0`) at the smoltcp boundary.
pub(crate) fn endpoint_is_routable(addr: &SocketAddr) -> bool {
  to_endpoint(*addr).addr.is_unicast() && addr.port() != 0
}

/// Derive a per-node RNG seed for *pinned* (deterministic) mode by FNV-1a hashing
/// a canonical buffer: a per-RNG domain tag, the interface seed, then the full
/// advertise address (IP octets, then port). Folding the interface seed and the
/// address through the hash makes the derived seed distinct from the interface RNG
/// and per-node unique, so two nodes that pin the same interface seed still get
/// divergent schedules. Pinned mode is for REPRODUCIBILITY (deterministic tests),
/// not secrecy.
fn seed_from(domain: u64, interface_seed: u64, advertise: &SocketAddr) -> u64 {
  const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
  let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
  let mut fold = |bytes: &[u8]| {
    for &byte in bytes {
      acc ^= u64::from(byte);
      acc = acc.wrapping_mul(FNV_PRIME);
    }
  };
  fold(&domain.to_le_bytes());
  fold(&interface_seed.to_le_bytes());
  match advertise.ip() {
    IpAddr::V4(v4) => fold(&v4.octets()),
    IpAddr::V6(v6) => fold(&v6.octets()),
  }
  fold(&advertise.port().to_le_bytes());
  acc
}

/// FNV domain tag for the memberlist gossip RNG (peer selection, timing jitter).
const GOSSIP_DOMAIN: u64 = 0x9E37_79B9_7F4A_7C15;
/// FNV domain tag for serf's own core RNG (query ids, relay/reconnect selection);
/// distinct from [`GOSSIP_DOMAIN`] so the two schedules never coincide.
const SERF_DOMAIN: u64 = 0x2545_F491_4F6C_DD1D;

/// Assemble the [`serf_embedded::Options`] the engine reads from the driver's
/// [`crate::Options`].
///
/// The `Options` name collision: the driver's `crate::Options` carries link-layer
/// sizing (socket buffers, UDP arenas, `tcp_pool_size`) that stays on the driver,
/// while `serf_embedded::Options` carries only the port and close timeout (plus the
/// CIDR policy) the engine reads directly.
fn embedded_options(cfg: &Options) -> serf_embedded::Options {
  let opts = serf_embedded::Options::new()
    .with_port(cfg.port)
    .with_close_timeout(cfg.close_timeout);
  #[cfg(feature = "cidr")]
  let opts = match cfg.cidr_policy.clone() {
    Some(policy) => opts.with_cidr_policy(policy),
    None => opts,
  };
  opts
}

/// Derive the medium a [`HardwareAddress`] selects, or `None` for a medium this
/// driver does not support (smoltcp's `Ieee802154`), so the caller can surface a
/// typed [`InitError::UnsupportedMedium`] instead of reaching an `unreachable!()`.
fn hardware_address_medium(addr: &HardwareAddress) -> Option<Medium> {
  match addr {
    HardwareAddress::Ip => Some(Medium::Ip),
    HardwareAddress::Ethernet(_) => Some(Medium::Ethernet),
    #[allow(unreachable_patterns)]
    _ => None,
  }
}

/// Returns the earlier of two optional deadlines. If only one is `Some`, that
/// deadline wins; if both are `None` the result is `None`.
fn min_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
  match (a, b) {
    (Some(x), Some(y)) => Some(core::cmp::min(x, y)),
    (x, y) => x.or(y),
  }
}

/// An executor-free serf node that composes serf's super-machine (via
/// [`SerfEngine`](serf_embedded::SerfEngine)) with a smoltcp TCP/IP stack.
///
/// The caller owns the device (`D`) and drives the node by calling [`poll`](Self::poll)
/// in a super-loop. Construction binds the gossip UDP socket, allocates the
/// reliable-plane TCP socket pool, and wires up the engine; no I/O occurs there.
///
/// `I` is the node identifier type (e.g. `SmolStr`). `A` is the resolver's
/// unresolved address type. `D` is the smoltcp [`Device`]. `G` is the memberlist
/// gossip RNG and `SR` is serf's own core RNG (both default to [`SmallRng`]); the
/// two are seeded independently so fresh nodes never share a query-id schedule.
pub struct Serf<I, A, D, G = SmallRng, SR = SmallRng>
where
  I: Eq + Hash,
{
  iface: Interface,
  /// The seed handed to smoltcp's interface RNG at construction (TCP ISN /
  /// ephemeral port selection). Retained because smoltcp does not expose it;
  /// surfaced via [`Serf::interface_random_seed`] for diagnostics.
  iface_random_seed: u64,
  sockets: SocketSet<'static>,
  /// Handle into `sockets` for the gossip UDP socket.
  udp: SocketHandle,
  /// The transport-agnostic serf driving core: serf's super-machine, the
  /// reliable-plane connection state machine and its `SocketHandle` pool, the
  /// gossip codec pipeline, and the join/await-result queues.
  engine: SerfEngine<I, SocketHandle, G, SR>,
  /// The local node's resolved advertise address, retained for
  /// [`advertise_address`](Self::advertise_address) (the engine does not surface it).
  advertise: SocketAddr,
  /// Driver-level application-event queue. Each [`poll`](Self::poll) drains the
  /// engine's events (mandatory-first), acts on the mandatory ones, and buffers
  /// every event here so the app's own [`poll_event`](Self::poll_event) observes
  /// the full serf surface (including the mandatory events) after the driver acted.
  /// Bounded at [`DEFAULT_EVENT_BUFFER_CAP`] with drop-oldest so an app that never
  /// drains it cannot grow memory without bound on a long-running embedded node.
  app_events: VecDeque<Event<I, SocketAddr>>,
  /// Count of app events shed from `app_events` because the app never drained
  /// [`poll_event`](Self::poll_event) fast enough and the backlog hit the cap.
  app_events_dropped: u64,
  /// Driver-owned self-delivery buffer for gossip datagrams this node addressed to
  /// its OWN advertise address (a response to its own query / key-request). smoltcp
  /// does not loop a self-addressed datagram back into recv like an OS UDP socket,
  /// so [`SmoltcpGossip`] diverts such datagrams here on send and replays them on
  /// recv, and [`poll`](Self::poll) drives the pump to quiescence so the looped-back
  /// datagram is ingested and its response collected within the same tick.
  loopback: VecDeque<Vec<u8>>,
  /// Set once the driver observed a lost id-conflict [`Event::Shutdown`]; the
  /// caller reads it via [`is_shutdown`](Self::is_shutdown) and stops polling.
  shutdown: bool,
  // `D` is passed to construction and each `poll`; `PhantomData` makes the struct
  // generic over it without holding it.
  _device: PhantomData<D>,
  // Ties the handle to the resolver's unresolved address type. `fn(A)` keeps the
  // marker contravariant in `A` and free of drop/auto-trait obligations.
  _a: PhantomData<fn(A)>,
}

// Construction seeding BOTH RNGs from the interface seed / system entropy.
impl<I, A, D> Serf<I, A, D, SmallRng, SmallRng>
where
  I: memberlist_proto::Id + Clone,
{
  /// Construct a node, panicking on a misconfiguration or entropy failure.
  ///
  /// The convenience wrapper over [`try_new`](Self::try_new); use it only when the
  /// configuration is a static constant known to be valid and the host's entropy
  /// source cannot fail.
  ///
  /// # Panics
  ///
  /// Panics if [`try_new`](Self::try_new) returns an [`InitError`].
  #[allow(clippy::too_many_arguments)]
  pub fn new<Res>(
    cfg: Options,
    iface: InterfaceOptions,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, A>,
    serf_opts: SerfOptions,
    resolver: &Res,
    device: &mut D,
    now: Instant,
  ) -> Self
  where
    Res: Resolver<Address = A>,
    D: Device,
  {
    Self::try_new(
      cfg, iface, transform, ep_cfg, serf_opts, resolver, device, now,
    )
    .expect("Serf::new: invalid configuration or entropy failure; use try_new to handle")
  }

  /// Fallibly construct a node.
  ///
  /// Builds the smoltcp `Interface`, allocates the gossip UDP socket and the
  /// reliable-plane TCP socket pool, and wires up the engine. Both the gossip RNG
  /// and serf's core RNG are seeded here — derived from the interface seed and the
  /// advertise address when [`InterfaceOptions::random_seed`](crate::InterfaceOptions)
  /// is pinned, or from an independent `getrandom` draw when it is not — so a
  /// production node never shares a query-id schedule with a peer.
  ///
  /// # Errors
  ///
  /// Returns [`InitError`] instead of panicking when the configuration is invalid
  /// for the bound device (an unsupported or mismatched medium, a non-unicast
  /// hardware or IP address, a missing/over-capacity address or route, a
  /// non-routable advertise address, an entropy failure, a resolver failure, an
  /// unusable encryption keyring, or a machine-endpoint init failure).
  #[allow(clippy::too_many_arguments)]
  pub fn try_new<Res>(
    cfg: Options,
    iface: InterfaceOptions,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, A>,
    serf_opts: SerfOptions,
    resolver: &Res,
    device: &mut D,
    now: Instant,
  ) -> Result<Self, InitError>
  where
    Res: Resolver<Address = A>,
    D: Device,
  {
    // Advertise-independent config preflight before touching the link layer.
    let embedded_cfg = embedded_options(&cfg);
    validate_runtime_config(&embedded_cfg, &transform, ep_cfg.gossip_mtu())
      .map_err(InitError::from_embedded)?;
    // Reject a serf-level configuration the engine cannot honor (an over-ceiling
    // `max_user_event_size`, or a self-contradictory coalescing pair) at the same
    // deterministic preflight, before drawing entropy or touching the link layer.
    serf_opts
      .validate()
      .map_err(InitError::InvalidSerfOptions)?;

    // Resolve the advertise address, then re-type `ep_cfg` so the rest of
    // construction only ever sees the resolved wire `SocketAddr`.
    let resolved_advertise = resolver
      .resolve(ep_cfg.advertise_addr_ref())
      .map_err(|e| InitError::Resolve(Box::new(e)))?
      .into_iter()
      .next()
      .ok_or(InitError::NoAddresses)?;
    let ep_cfg = ep_cfg.map_advertise(|_| resolved_advertise);

    // Interface seed (pinned or system entropy) plus per-RNG derived seeds. In
    // entropy mode the gossip and serf seeds are independent system-entropy draws;
    // in pinned mode they are deterministically derived from the interface seed and
    // advertise address (reproducible, per-node distinct — see `seed_from`).
    let advertise = *ep_cfg.advertise_addr_ref();
    let (random_seed, gossip_seed, serf_seed) = match iface.random_seed {
      Some(s) => (
        s,
        seed_from(GOSSIP_DOMAIN, s, &advertise),
        seed_from(SERF_DOMAIN, s, &advertise),
      ),
      None => {
        let mut b = [0u8; 24];
        getrandom::fill(&mut b).map_err(|_| InitError::Entropy)?;
        let word = |i: usize| {
          u64::from_le_bytes([
            b[i],
            b[i + 1],
            b[i + 2],
            b[i + 3],
            b[i + 4],
            b[i + 5],
            b[i + 6],
            b[i + 7],
          ])
        };
        (word(0), word(8), word(16))
      }
    };

    Self::assemble(
      cfg,
      iface,
      transform,
      ep_cfg,
      serf_opts,
      device,
      now,
      random_seed,
      SmallRng::seed_from_u64(gossip_seed),
      SmallRng::seed_from_u64(serf_seed),
    )
  }
}

// Construction with caller-supplied RNGs, plus the shared assembly path (needs
// serf's core RNG seedable, matching the engine's construction bound).
impl<I, A, D, G, SR> Serf<I, A, D, G, SR>
where
  I: memberlist_proto::Id + Clone,
  SR: SeedableRng,
{
  /// Like [`new`](Self::new) but with caller-supplied gossip + serf RNGs; the caller
  /// owns seeding them. The interface seed is still drawn here (pinned or from
  /// `getrandom`) to drive smoltcp's TCP-stack RNG.
  ///
  /// # Errors
  ///
  /// Returns [`InitError`] on the same conditions as [`try_new`](Self::try_new).
  #[allow(clippy::too_many_arguments)]
  pub fn with_rng<Res>(
    cfg: Options,
    iface: InterfaceOptions,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, A>,
    serf_opts: SerfOptions,
    resolver: &Res,
    device: &mut D,
    now: Instant,
    gossip_rng: G,
    serf_rng: SR,
  ) -> Result<Self, InitError>
  where
    Res: Resolver<Address = A>,
    D: Device,
  {
    let embedded_cfg = embedded_options(&cfg);
    validate_runtime_config(&embedded_cfg, &transform, ep_cfg.gossip_mtu())
      .map_err(InitError::from_embedded)?;
    // Reject a serf-level configuration the engine cannot honor (an over-ceiling
    // `max_user_event_size`, or a self-contradictory coalescing pair) at the same
    // deterministic preflight, before drawing entropy or touching the link layer.
    serf_opts
      .validate()
      .map_err(InitError::InvalidSerfOptions)?;

    let resolved_advertise = resolver
      .resolve(ep_cfg.advertise_addr_ref())
      .map_err(|e| InitError::Resolve(Box::new(e)))?
      .into_iter()
      .next()
      .ok_or(InitError::NoAddresses)?;
    let ep_cfg = ep_cfg.map_advertise(|_| resolved_advertise);

    // The caller owns both protocol RNGs; only the interface seed is drawn here.
    let random_seed = match iface.random_seed {
      Some(s) => s,
      None => {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).map_err(|_| InitError::Entropy)?;
        u64::from_le_bytes(b)
      }
    };

    Self::assemble(
      cfg,
      iface,
      transform,
      ep_cfg,
      serf_opts,
      device,
      now,
      random_seed,
      gossip_rng,
      serf_rng,
    )
  }

  /// The link-layer + engine assembly shared by every constructor: validate the
  /// medium/addresses/sizing against smoltcp, build the interface and sockets, run
  /// the advertise-address screens, then build the engine over the resolved
  /// `ep_cfg` with the supplied (already-seeded) RNGs.
  #[allow(clippy::too_many_arguments)]
  fn assemble(
    cfg: Options,
    iface: InterfaceOptions,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, SocketAddr>,
    serf_opts: SerfOptions,
    device: &mut D,
    now: Instant,
    random_seed: u64,
    gossip_rng: G,
    serf_rng: SR,
  ) -> Result<Self, InitError>
  where
    D: Device,
  {
    let embedded_cfg = embedded_options(&cfg);

    // 1. Validate the medium up front: smoltcp's `Interface::new` asserts the
    //    hardware address's medium equals the device's; derive it ourselves and
    //    reject an unsupported (Ieee802154) or mismatched medium as a typed error.
    let expected =
      hardware_address_medium(&iface.hardware_addr).ok_or(InitError::UnsupportedMedium)?;
    let actual = device.capabilities().medium;
    if expected != actual {
      return Err(InitError::MediumMismatch(MediumMismatch {
        expected,
        actual,
      }));
    }

    // 2. An Ethernet hardware address must be unicast (smoltcp stores it without
    //    re-checking). The `Ip` variant carries no L2 address and is always fine.
    if let HardwareAddress::Ethernet(mac) = &iface.hardware_addr {
      if !mac.is_unicast() {
        return Err(InitError::NonUnicastHardwareAddress(iface.hardware_addr));
      }
    }

    // 3. An interface with no address silently drops every packet.
    if iface.ip_addrs.is_empty() {
      return Err(InitError::MissingIpAddress);
    }

    // 4. Every configured IP must be unicast or unspecified (smoltcp's
    //    `check_ip_addrs` `panic!`s otherwise).
    for cidr in &iface.ip_addrs {
      if !cidr.address().is_unicast() && !cidr.address().is_unspecified() {
        return Err(InitError::NonUnicastIpAddress(*cidr));
      }
    }

    // Every route's gateway must be unicast and share the prefix's IP family, or it
    // can never resolve a next hop (a release-mode assert / dead route at egress).
    for route in &iface.routes {
      if !route.via_router.is_unicast() {
        return Err(InitError::NonUnicastRouteGateway(*route));
      }
      if route.cidr.address().version() != route.via_router.version() {
        return Err(InitError::RouteFamilyMismatch(*route));
      }
    }

    // smoltcp rejects port 0 on bind/listen; screen it before allocating sockets.
    if cfg.port == 0 {
      return Err(InitError::ZeroPort);
    }

    // Reject a gossip MTU whose on-wire datagram cannot fit a UDP packet (the UDP
    // arenas are sized from it, and the addition must not overflow).
    let gossip_mtu_ceiling = UDP_PAYLOAD_MAX - ENCRYPTED_WRAPPER_OVERHEAD;
    if ep_cfg.gossip_mtu() > gossip_mtu_ceiling {
      return Err(InitError::GossipMtuTooLarge(GossipMtuTooLarge {
        gossip_mtu: ep_cfg.gossip_mtu(),
        ceiling: gossip_mtu_ceiling,
      }));
    }

    // A functional reliable plane needs a listener plus one dial/accept socket.
    if cfg.tcp_pool_size < 2 {
      return Err(InitError::TcpPoolTooSmall);
    }
    // A zero-length ring is a permanently-dead socket; both halves must be non-zero.
    if cfg.tcp_socket_rx_bytes == 0 || cfg.tcp_socket_tx_bytes == 0 {
      return Err(InitError::ZeroTcpSocketBuffer);
    }
    // smoltcp `panic!`s on a receive buffer past 1 GiB.
    if cfg.tcp_socket_rx_bytes > TCP_RX_BUFFER_MAX {
      return Err(InitError::TcpRxBufferTooLarge);
    }
    // Zero packet-metadata slots is a gossip ring that can never enqueue/dequeue.
    if cfg.udp_rx_packets == 0 || cfg.udp_tx_packets == 0 {
      return Err(InitError::ZeroUdpPackets);
    }
    // A zero close timeout force-aborts every graceful close immediately.
    if cfg.close_timeout.is_zero() {
      return Err(InitError::ZeroCloseTimeout);
    }

    // Build the interface with the resolved interface seed, then apply addresses
    // and routes (both bounded by smoltcp's `heapless::Vec` capacities).
    let mut ic = IfConfig::new(iface.hardware_addr);
    ic.random_seed = random_seed;
    let mut iface_obj = Interface::new(ic, device, to_smoltcp_instant(now));

    let mut overflow = false;
    iface_obj.update_ip_addrs(|addrs| {
      for cidr in &iface.ip_addrs {
        if addrs.push(*cidr).is_err() {
          overflow = true;
          break;
        }
      }
    });
    if overflow {
      return Err(InitError::TooManyIpAddresses);
    }

    let mut route_overflow = false;
    iface_obj.routes_mut().update(|table| {
      for route in &iface.routes {
        if table.push(*route).is_err() {
          route_overflow = true;
          break;
        }
      }
    });
    if route_overflow {
      return Err(InitError::TooManyRoutes);
    }

    let iface = iface_obj;

    // Allocate the gossip UDP socket over an alloc-backed, growable socket store.
    let mut sockets = SocketSet::new(Vec::new());

    // Floor each UDP payload arena at "configured datagram slots × max on-wire
    // datagram" so an in-budget datagram is never rejected by an under-sized arena;
    // `checked_mul` guards a 32-bit overflow.
    let max_datagram = ep_cfg.gossip_mtu() + ENCRYPTED_WRAPPER_OVERHEAD;
    let udp_rx_arena = cfg.udp_rx_payload_bytes.max(
      cfg
        .udp_rx_packets
        .checked_mul(max_datagram)
        .ok_or(InitError::UdpArenaTooLarge)?,
    );
    let udp_tx_arena = cfg.udp_tx_payload_bytes.max(
      cfg
        .udp_tx_packets
        .checked_mul(max_datagram)
        .ok_or(InitError::UdpArenaTooLarge)?,
    );

    let udp_rx = udp::PacketBuffer::new(
      std::vec![udp::PacketMetadata::EMPTY; cfg.udp_rx_packets],
      std::vec![0u8; udp_rx_arena],
    );
    let udp_tx = udp::PacketBuffer::new(
      std::vec![udp::PacketMetadata::EMPTY; cfg.udp_tx_packets],
      std::vec![0u8; udp_tx_arena],
    );
    let mut udp_sock = udp::Socket::new(udp_rx, udp_tx);
    // `bind` fails only on port 0 (rejected above) or an already-open socket (fresh
    // here); propagate rather than `expect` so no panic escapes the constructor.
    udp_sock.bind(cfg.port).map_err(|_| InitError::ZeroPort)?;
    let udp = sockets.add(udp_sock);

    // The advertise address must be routable and one the interface actually holds,
    // or the node is unreachable on both planes.
    if !endpoint_is_routable(ep_cfg.advertise_addr_ref()) {
      return Err(InitError::NonRoutableAdvertiseAddr(
        *ep_cfg.advertise_addr_ref(),
      ));
    }
    let advertised_ip = to_endpoint(*ep_cfg.advertise_addr_ref()).addr;
    if !iface.has_ip_addr(advertised_ip) {
      return Err(InitError::AdvertiseAddrNotLocal(
        *ep_cfg.advertise_addr_ref(),
      ));
    }
    let advertise = *ep_cfg.advertise_addr_ref();

    // Build the engine. `try_new_at_with_rng` maps a machine/keyring/advertise
    // failure to a typed `InitError`; it installs the routable-address admission
    // filter and the label/encryption transforms internally and forwards the CIDR
    // policy carried on `embedded_cfg`.
    let mut engine = SerfEngine::try_new_at_with_rng(
      embedded_cfg,
      transform,
      ep_cfg,
      serf_opts,
      now,
      gossip_rng,
      serf_rng,
    )
    .map_err(InitError::from_embedded)?;

    // Allocate pooled TCP sockets and register their handles with the engine's
    // reliable plane; dedicate one to the passive-open listener.
    for _ in 0..cfg.tcp_pool_size {
      let rx = tcp::SocketBuffer::new(std::vec![0u8; cfg.tcp_socket_rx_bytes]);
      let tx = tcp::SocketBuffer::new(std::vec![0u8; cfg.tcp_socket_tx_bytes]);
      engine
        .plane_mut()
        .pool
        .push(sockets.add(tcp::Socket::new(rx, tx)));
    }
    if let Some(h) = engine.plane_mut().pool.take() {
      // `listen` fails only on port 0 (rejected) or an already-open socket (fresh).
      sockets
        .get_mut::<tcp::Socket>(h)
        .listen(cfg.port)
        .map_err(|_| InitError::ZeroPort)?;
      engine.set_listener(h);
    }

    Ok(Self {
      iface,
      iface_random_seed: random_seed,
      sockets,
      udp,
      engine,
      advertise,
      app_events: VecDeque::new(),
      app_events_dropped: 0,
      loopback: VecDeque::new(),
      shutdown: false,
      _device: PhantomData,
      _a: PhantomData,
    })
  }
}

// Pure reads over the driver and the reliable plane — needing neither RNG.
impl<I, A, D, G, SR> Serf<I, A, D, G, SR>
where
  I: memberlist_proto::Id + Clone,
{
  /// The seed handed to smoltcp's interface RNG at construction.
  #[doc(hidden)]
  #[inline]
  pub fn interface_random_seed(&self) -> u64 {
    self.iface_random_seed
  }

  /// The local node's advertised `SocketAddr`.
  #[inline]
  pub fn advertise_address(&self) -> SocketAddr {
    self.advertise
  }

  /// Whether the driver has observed a lost id-conflict [`Event::Shutdown`] and the
  /// caller should stop polling.
  #[inline]
  pub fn is_shutdown(&self) -> bool {
    self.shutdown
  }

  /// Drain one application-visible serf event the last [`poll`](Self::poll) buffered,
  /// mandatory driver-actioned events first (the driver has ALREADY acted on them),
  /// then passive observations. `None` when the queue is empty.
  #[inline]
  pub fn poll_event(&mut self) -> Option<Event<I, SocketAddr>> {
    self.app_events.pop_front()
  }

  /// Number of inbound reliable connections accepted since construction.
  #[doc(hidden)]
  #[inline]
  pub fn accepted_inbound_count(&self) -> u64 {
    self.engine.accepted_inbound_count()
  }

  /// Number of pooled TCP sockets currently free.
  #[doc(hidden)]
  #[inline]
  pub fn pool_free_count(&self) -> usize {
    self.engine.pool_free_count()
  }

  /// Number of TCP sockets currently parked mid-close.
  #[doc(hidden)]
  #[inline]
  pub fn closing_count(&self) -> usize {
    self.engine.closing_count()
  }

  /// Number of reliable exchanges currently half-closed.
  #[doc(hidden)]
  #[inline]
  pub fn half_closed_count(&self) -> usize {
    self.engine.half_closed_count()
  }

  /// Whether a passive-open listener socket is currently installed.
  #[doc(hidden)]
  #[inline]
  pub fn listener_present(&self) -> bool {
    self.engine.listener_present()
  }

  /// Number of reliable exchanges still in `PendingDial`.
  #[doc(hidden)]
  #[inline]
  pub fn pending_dial_count(&self) -> usize {
    self.engine.pending_dial_count()
  }

  /// Number of await-result joins currently tracked.
  #[doc(hidden)]
  #[inline]
  pub fn pending_join_count(&self) -> usize {
    self.engine.pending_join_count()
  }
}

// serf reads + command surface + the poll loop — reach serf's super-machine, so
// they carry the gossip `G: Rng` and serf's `SR: Rng + SeedableRng` bounds. The
// engine's connection handle is the concrete `SocketHandle` (Copy + Eq + Hash), so
// the pump's `C` bound is satisfied without an extra parameter.
impl<I, A, D, G, SR> Serf<I, A, D, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Arm serf's periodic probe / gossip / push-pull schedulers. Call once before
  /// the first [`poll`](Self::poll).
  pub fn start(&mut self, now: Instant) {
    self.engine.start(now);
  }

  /// Install (or clear) the per-member reconnect-timeout override
  /// [`ReconnectDelegate`](crate::ReconnectDelegate) on the serf engine.
  ///
  /// `None` (the default) keeps the flat configured reap timeouts.
  #[inline]
  pub fn set_reconnect_delegate(
    &mut self,
    delegate: Option<std::boxed::Box<dyn serf_embedded::ReconnectDelegate<I, SocketAddr>>>,
  ) {
    self.engine.set_reconnect_delegate(delegate);
  }

  /// serf's current lifecycle state.
  #[inline]
  pub fn state(&self) -> SerfState {
    self.engine.state()
  }

  /// Number of serf members currently tracked (including the local node).
  #[inline]
  pub fn num_members(&self) -> usize {
    self.engine.num_members()
  }

  /// The number of app events shed because a consumer did not keep up: the engine's
  /// own passive-observation drops plus this driver's
  /// [`poll_event`](Self::poll_event) backlog drops.
  ///
  /// Both stages are bounded at [`DEFAULT_EVENT_BUFFER_CAP`] with drop-oldest, and a
  /// single pump can shed observations INSIDE the engine (its bounded queue) before
  /// the driver's queue — freshly drained each poll — ever fills, so this sums BOTH
  /// counters; reporting only the driver's would under-count real loss.
  #[inline]
  pub fn events_dropped(&self) -> u64 {
    self
      .engine
      .events_dropped()
      .saturating_add(self.app_events_dropped)
  }

  /// Cumulative count of coalescing user events the engine's user coalescer shed
  /// because its buffered volume was at the configured cap.
  ///
  /// Lifetime total, saturating, and never cleared by a flush or reset. Reads `0`
  /// when user coalescing is disabled.
  #[inline]
  pub fn coalesced_user_events_dropped(&self) -> u64 {
    self.engine.coalesced_user_events_dropped()
  }

  /// Cumulative count of member changes the engine's member coalescer shed
  /// because its per-window map was at its cardinality cap.
  ///
  /// Lifetime total, saturating, and never cleared. Reads `0` when member
  /// coalescing is disabled.
  #[inline]
  pub fn coalesced_member_events_dropped(&self) -> u64 {
    self.engine.coalesced_member_events_dropped()
  }

  /// The local node's id.
  #[inline]
  pub fn local_id(&self) -> I {
    self.engine.local_id().clone()
  }

  /// A snapshot of every serf member currently tracked (alive, leaving, left, or
  /// failed within the reap window).
  #[inline]
  pub fn members(&self) -> Vec<Arc<Member<I, SocketAddr>>> {
    self.engine.members_snapshot()
  }

  /// The local node's serf member Lamport clock.
  #[inline]
  pub fn member_time(&self) -> u64 {
    self.engine.member_time()
  }

  /// The local node's serf event Lamport clock.
  #[inline]
  pub fn event_time(&self) -> u64 {
    self.engine.event_time()
  }

  /// The local node's serf query Lamport clock.
  #[inline]
  pub fn query_time(&self) -> u64 {
    self.engine.query_time()
  }

  /// Announce the local node's join intent and begin an await-result join to these
  /// seeds, returning a [`JoinId`] the caller polls via [`poll_join`](Self::poll_join).
  ///
  /// Each seed is resolved through `resolver` (a [`MaybeResolved::Resolved`] address
  /// is used verbatim, a [`MaybeResolved::Unresolved`] one is expanded into the wire
  /// addresses the resolver yields). Returns immediately; the poll loop initiates a
  /// push/pull to each routable seed on the next tick. When `ignore_old` is set,
  /// each seed's push/pull suppresses replay of the peer's pre-join user events.
  ///
  /// # Errors
  ///
  /// Returns [`JoinError::Control`] when serf rejects the join (e.g. the node is not
  /// running), [`JoinError::Resolve`] on a resolver failure, or
  /// [`JoinError::NoAddresses`] when a non-empty seed set resolves to no address.
  pub fn join<Res>(
    &mut self,
    resolver: &Res,
    seeds: &[MaybeResolved<A>],
    ignore_old: bool,
    now: Instant,
  ) -> Result<JoinId, JoinError>
  where
    Res: Resolver<Address = A>,
  {
    let mut resolved = Vec::with_capacity(seeds.len());
    for seed in seeds {
      match seed {
        MaybeResolved::Resolved(s) => resolved.push(*s),
        MaybeResolved::Unresolved(a) => resolved.extend(
          resolver
            .resolve(a)
            .map_err(|e| JoinError::Resolve(Box::new(e)))?,
        ),
      }
    }
    if !seeds.is_empty() && resolved.is_empty() {
      return Err(JoinError::NoAddresses);
    }
    self
      .engine
      .join(&resolved, ignore_old, now)
      .map_err(JoinError::Control)
  }

  /// Drain the terminal outcome of an await-result [`join`](Self::join), or `None`
  /// while it is still in flight. Delivered exactly once per handle.
  #[inline]
  pub fn poll_join(&mut self, handle: JoinId) -> Option<Result<ReachedSet, JoinFailed>> {
    self.engine.poll_join(handle)
  }

  /// Give up an await-result [`join`](Self::join), dropping any still-queued seeds
  /// and forgetting its caller reply, leak-free.
  #[inline]
  pub fn cancel_join(&mut self, handle: JoinId) {
    self.engine.cancel_join(handle);
  }

  /// Begin leaving the cluster. Gossips the departure and ultimately emits
  /// [`Event::LeftCluster`] via [`poll_event`](Self::poll_event).
  pub fn leave(&mut self, now: Instant) -> Result<(), SerfError> {
    self.engine.leave(now)
  }

  /// Force a named node out of the cluster (an operator-driven removal).
  pub fn force_leave(&mut self, id: I, prune: bool, now: Instant) -> Result<(), SerfError> {
    self.engine.force_leave(id, prune, now)
  }

  /// Broadcast an application user event to the cluster. `coalesce` requests that
  /// identical events be coalesced by name. Peers observe it as [`Event::User`].
  pub fn user_event(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    coalesce: bool,
    now: Instant,
  ) -> Result<(), SerfError> {
    self.engine.user_event(name, payload, coalesce, now)
  }

  /// Issue a cluster-wide query, returning its [`QueryId`]. Responders observe it as
  /// [`Event::Query`] and answer via [`respond`](Self::respond).
  pub fn query(
    &mut self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    params: QueryParams<I>,
    now: Instant,
  ) -> Result<QueryId, SerfError> {
    self.engine.query(name, payload, params, now)
  }

  /// Answer a received query. `token` is the [`QueryEvent`] delivered via
  /// [`Event::Query`].
  pub fn respond(
    &mut self,
    token: &QueryEvent<I, SocketAddr>,
    payload: bytes::Bytes,
    now: Instant,
  ) -> Result<(), SerfError> {
    self.engine.respond(token, payload, now)
  }

  /// Replace the local node's tags, re-advertising them and refreshing the local
  /// member in the membership store.
  pub fn set_tags(&mut self, tags: Tags, now: Instant) -> Result<(), SerfError> {
    self.engine.set_tags(tags, now)
  }

  /// Issue a cluster-wide `install_key` query to add `key` to every node's keyring.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn install_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.engine.install_key(key, now)
  }

  /// Issue a cluster-wide `use_key` query to promote `key` to primary.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn use_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.engine.use_key(key, now)
  }

  /// Issue a cluster-wide `remove_key` query to remove `key` from all nodes.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn remove_key(&mut self, key: SecretKey, now: Instant) -> Result<QueryId, SerfError> {
    self.engine.remove_key(key, now)
  }

  /// Issue a cluster-wide `list_keys` query to enumerate installed keys.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn list_keys(&mut self, now: Instant) -> Result<QueryId, SerfError> {
    self.engine.list_keys(now)
  }

  /// The node's LIVE wire keyring — the keyring the gossip and reliable planes
  /// actually encrypt under, and the state an inbound [`Event::KeyRequest`] rotates
  /// via the engine. `None` when the node is unencrypted. Unlike
  /// [`list_keys`](Self::list_keys) (a cluster-wide query), this is a local read of
  /// this node's own keyring for UI / diagnostics / tests.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn keyring(&self) -> Option<&Keyring> {
    self.engine.keyring()
  }

  /// Advance both the smoltcp stack and serf's state machine, act on serf's
  /// mandatory driver-actioned events, and drive to quiescence within the tick.
  /// Returns the next wakeup deadline: the minimum of the smoltcp stack's next
  /// scheduled event, the machine's next timer, and any engine-owned deadline (the
  /// soonest closing socket's abort).
  ///
  /// # Order
  ///
  /// 1. **Stack tick** — `iface.poll` drains the device and services TCP/UDP.
  /// 2. **Pump → drain, to quiescence** — the engine runs every protocol phase over
  ///    a [`SmoltcpGossip`] + [`SmoltcpStream`] view of the just-ticked sockets
  ///    (computing the next deadline and egressing outbound gossip), then the drain
  ///    acts on the driver-actioned events: [`Event::Shutdown`] flips the stop flag;
  ///    an [`Event::KeyRequest`] is applied to the engine's live wire keyring and
  ///    answered via the engine's `handle_key_request`. Every drained event is
  ///    buffered for the app's own
  ///    [`poll_event`](Self::poll_event), so membership / user / query observations
  ///    and the mandatory events alike remain visible AFTER the driver acted.
  ///
  ///    The pump computes its deadline and egresses BEFORE the drain runs, so a
  ///    `respond_key` the drain queues — or a self-addressed datagram the pump's
  ///    egress diverts into the [`loopback`](SmoltcpGossip) buffer — would not be
  ///    reflected by that pass. This step therefore re-pumps at the SAME `now` while
  ///    a pass queued a key response OR left the loopback non-empty (re-pumping at an
  ///    unchanged `now` is safe: due timers already fired, so the extra pass only
  ///    egresses the queued send and ingests the looped-back datagram), bounded by
  ///    [`MAX_SELF_DELIVERY_ITERS`]. A remote `respond_key` is thus egressed within
  ///    this `poll`, and a self-addressed response is looped back, ingested, and its
  ///    query response collected — all before `poll` returns.
  /// 3. **Deadline** — fold the stack's next scheduled event into the settled
  ///    engine deadline. If the quiescence loop exhausted its budget with work still
  ///    pending, fold `now` in too so the caller re-polls immediately.
  pub fn poll(&mut self, now: Instant, device: &mut D) -> Option<Instant>
  where
    D: Device,
  {
    let s_now = to_smoltcp_instant(now);

    // 1. Stack tick.
    self.iface.poll(s_now, device, &mut self.sockets);

    // 2. Pump → drain, re-running at the same `now` until neither a queued key
    // response nor a looped-back self-datagram remains, so both are handled within
    // this tick. The gossip and stream views share the one `SocketSet` through a
    // `RefCell` held for each pump; each takes a brief borrow and never holds one
    // across a call into the other, and the loopback buffer is a separate field
    // outside that borrow.
    let mut next = None;
    let mut settled = false;
    for _ in 0..MAX_SELF_DELIVERY_ITERS {
      next = {
        let sockets = RefCell::new(&mut self.sockets);
        let loopback = RefCell::new(&mut self.loopback);
        let mut gossip = SmoltcpGossip::new(&sockets, self.udp, &loopback, self.advertise);
        let mut stream = SmoltcpStream::new(&mut self.iface, &sockets);
        self.engine.pump(now, &mut gossip, &mut stream)
      };
      let queued = self.drain_engine_events(now);
      if !queued && self.loopback.is_empty() {
        settled = true;
        break;
      }
    }

    // 3. Fold the stack's next scheduled event into the engine's deadline. On an
    // unsettled loop (work still pending at the iteration cap), fold `now` so the
    // caller re-polls at once rather than sleeping past the stranded work.
    if !settled {
      next = min_opt(next, Some(now));
    }
    let stack = self
      .iface
      .poll_at(s_now, &self.sockets)
      .map(from_smoltcp_instant);
    min_opt(stack, next)
  }

  /// Drain the engine's event queue (mandatory-first), take the driver-owned side
  /// effect on each mandatory event, and buffer every event for the app.
  ///
  /// Returns whether the drain queued outbound gossip work the current pump's egress
  /// did not see — a key response the engine's `handle_key_request` just queued — so
  /// [`poll`](Self::poll) knows to re-pump and egress it within the same tick.
  fn drain_engine_events(&mut self, now: Instant) -> bool {
    // `now` and the queued-outbound signal are consumed only by the encryption
    // `KeyRequest` arm; a build without an AEAD backend reads neither and queues no
    // key response, so its drain never re-pumps on this account.
    #[cfg(not(encryption))]
    let _ = now;
    #[cfg(encryption)]
    let mut queued = false;
    #[cfg(not(encryption))]
    let queued = false;
    while let Some(ev) = self.engine.poll_event() {
      match &ev {
        // A lost id-conflict vote means the local node MUST stop; flag it. The event
        // still reaches the app via `poll_event`.
        Event::Shutdown => self.shutdown = true,
        // An inbound key-management request: apply the op to the engine's LIVE wire
        // keyring and answer the originator in one call. The response is a directed
        // gossip transmit egressed on the re-pump [`poll`](Self::poll) runs while
        // `queued` is set.
        #[cfg(encryption)]
        Event::KeyRequest(req) => {
          // `Ok` means a key response was queued (re-pump to egress it). Ignoring the
          // Err case: `handle_key_request` has already applied the op to the live
          // keyring; an Err means only the best-effort response was past-deadline or
          // could not be routed, which queues no outbound work.
          queued |= self.engine.handle_key_request(req, now).is_ok();
        }
        _ => {}
      }
      self.push_app_event(ev);
    }
    queued
  }

  /// Buffer one event for [`poll_event`](Self::poll_event), bounding the backlog at
  /// [`DEFAULT_EVENT_BUFFER_CAP`] with drop-oldest so a never-draining app cannot
  /// grow memory without bound.
  fn push_app_event(&mut self, ev: Event<I, SocketAddr>) {
    if self.app_events.len() >= DEFAULT_EVENT_BUFFER_CAP {
      self.app_events.pop_front();
      self.app_events_dropped += 1;
    }
    self.app_events.push_back(ev);
  }
}

#[cfg(test)]
mod tests;
