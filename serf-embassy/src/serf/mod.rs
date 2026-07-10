//! The cloneable async [`Serf`] handle and node construction.
//!
//! [`Serf::new`] builds the shared [`SerfEngine`](serf_embedded::SerfEngine) over
//! the caller's embassy-net sockets, seeds the reliable-plane pool with the `N`
//! TCP slots, installs the first listener, and hands back the handle paired with
//! the [`Runner`] the caller drives. The handle is a thin shared reference
//! (`Rc<Shared>`); clone it freely to issue commands and read membership from
//! multiple places on the single executor.
//!
//! The async ops (`join`) enqueue work on the engine and park on the run loop's
//! signals; the sync commands (`user_event` / `query` / `set_tags` / key
//! management) enqueue and wake the pump; the sync accessors borrow the engine
//! directly. serf's mandatory driver-actioned events (a lost-conflict
//! [`Event::Shutdown`], an inbound [`Event::KeyRequest`]) are handled by the
//! [`Runner`] in its post-pump drain, before the observation is buffered — so the
//! handle only ever OBSERVES them via [`poll_event`](Serf::poll_event).

use core::{marker::PhantomData, net::SocketAddr};

use alloc::{boxed::Box, rc::Rc, vec::Vec};
use std::sync::Arc;

use embassy_futures::select::{Either, select};
use embassy_net::{tcp::TcpSocket, udp::UdpSocket};
use embassy_time::Timer;
use memberlist_proto::{EndpointOptions, Instant, Rng, SeedableRng, SmallRng};
use serf_embedded::{
  Event, JoinId, MaybeResolved, ReachedSet, SerfEngine, SerfOptions, TransformOptions,
  validate_runtime_config,
};
use serf_proto::{
  endpoint::{QueryId, QueryParams},
  event::QueryEvent,
  members::{Member, SerfState},
  typed::Tags,
};

#[cfg(encryption)]
use serf_embedded::{Keyring, SecretKey};

use crate::{
  config::Options,
  error::{InitError, JoinError, OpError, SocketTimeoutOutOfRange},
  mailbox::{Command, Mailbox},
  resolver::AddressResolver,
  runner::Runner,
  shared::Shared,
  stream_io::{SlotId, SlotWake},
  time,
};

/// The largest [`Options::socket_timeout`](crate::Options::socket_timeout)
/// [`Serf::new`] accepts. A per-socket inactivity backstop longer than a day is
/// nonsensical for serf (reliable exchanges complete in milliseconds), and
/// rejecting larger values keeps the timeout safely within EVERY downstream duration
/// domain — the `embassy_time` tick count, its `as_micros` conversion (which
/// multiplies before dividing), and smoltcp's `i64` `Instant` arithmetic — at ANY
/// tick rate, so no configurable value can overflow that chain into a wrapped,
/// effectively-past deadline that would abort a TCP slot immediately.
const MAX_SOCKET_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(86_400);

/// Floor a portable `core::Duration` to whole `embassy_time` ticks at `tick_hz`,
/// exactly as [`embassy_time::Duration::from_ticks`] will store it.
///
/// Pure and parameterized on `tick_hz` (rather than reading the
/// [`embassy_time::TICK_HZ`] constant directly) so the coarse-rate rounding behavior is
/// unit-testable without rebuilding `embassy-time` at another tick rate. The nanosecond
/// basis keeps the conversion exact at fine tick rates (a microsecond basis would
/// silently drop sub-microsecond resolution); the `u128` saturating multiply and `u64`
/// clamp keep it total for any input, so an out-of-range duration converts to a
/// saturated tick count rather than panicking.
fn duration_to_ticks(d: core::time::Duration, tick_hz: u128) -> u64 {
  let ticks = d.as_nanos().saturating_mul(tick_hz) / 1_000_000_000;
  u64::try_from(ticks).unwrap_or(u64::MAX)
}

/// The whole-microsecond timeout embassy-net actually installs into smoltcp for an
/// already-floored embassy tick count at `tick_hz`.
///
/// embassy-net hands smoltcp `embassy_time::Duration::as_micros()` — a SECOND floor on
/// top of the tick flooring: at a tick rate finer than 1 MHz the tick count carries
/// sub-microsecond resolution this floor discards. Reproduced here (saturating `u128`) so
/// validation reasons about the value smoltcp receives, microseconds, not the
/// intermediate tick count.
fn installed_micros(ticks: u64, tick_hz: u128) -> u128 {
  u128::from(ticks).saturating_mul(1_000_000) / tick_hz
}

/// Validate `socket_timeout` against the engine deadlines IN THE INSTALLED MICROSECOND
/// DOMAIN at `tick_hz`, returning the embassy tick count the worker installs when it is
/// in range.
///
/// The value that actually gates the TCP socket is `socket_timeout` floored twice — to
/// whole embassy ticks (`from_ticks`) and then to whole microseconds (embassy-net's
/// `as_micros`, see [`installed_micros`]). The ordering invariant must hold on that
/// microsecond value, not on the portable input or the intermediate tick count: a coarse
/// tick rate can floor a portable-valid value below a deadline, and a tick rate finer than
/// 1 MHz can clear a tick comparison yet still install the same (or zero) microsecond
/// value. The deadlines are floored to microseconds too — the engine enforces them at full
/// resolution, but an installed whole-microsecond timeout exceeds the real deadline exactly
/// when it exceeds the deadline's microsecond floor. `socket_us > close_us` with
/// non-negative `close_us` also forces at least one installed microsecond, so an accepted
/// timeout is never the zero value smoltcp treats as an immediate abort. The upper bound is
/// checked first, in the portable domain, so the conversions cannot overflow (see
/// [`MAX_SOCKET_TIMEOUT`]).
fn checked_socket_timeout(
  socket: core::time::Duration,
  close: core::time::Duration,
  stream: core::time::Duration,
  tick_hz: u128,
) -> Option<u64> {
  if socket > MAX_SOCKET_TIMEOUT {
    return None;
  }
  let socket_ticks = duration_to_ticks(socket, tick_hz);
  let socket_us = installed_micros(socket_ticks, tick_hz);
  (socket_us > close.as_micros() && socket_us > stream.as_micros()).then_some(socket_ticks)
}

/// Assemble the [`serf_embedded::Options`] the engine reads from the driver's
/// [`crate::Options`].
///
/// The driver's `crate::Options` carries link-layer sizing (the bridge ring
/// capacities, the per-socket timeout) that stays on the driver, while
/// `serf_embedded::Options` carries only the port and close timeout (plus the CIDR
/// policy) the engine reads directly. Built once, up front, so the same value
/// drives both the construction preflight
/// ([`serf_embedded::validate_runtime_config`]) and the engine itself.
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

/// Cancel an in-flight await-result join if its future is dropped before the join
/// resolves (a `select` timeout, or the caller abandoning the await).
///
/// [`SerfEngine::join`](serf_embedded::SerfEngine::join) mints a [`JoinId`] whose
/// entry the engine reaps only once the caller polls or cancels it. A join future
/// dropped mid-await therefore leaves that entry lingering; this guard cancels the
/// join on drop so no abandoned join accumulates. On a resolved join the awaiting
/// method [`disarm`](Self::disarm)s the guard first, so a completed join is never
/// double-cancelled. Drop runs at a suspension point where no engine borrow is
/// live (borrows never span an `.await`), so the `borrow_mut` here cannot alias.
struct JoinGuard<'a, I, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  shared: &'a Shared<I, G, SR>,
  id: Option<JoinId>,
}

impl<I, G, SR> JoinGuard<'_, I, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// Disarm the guard so a resolved join is not cancelled on drop.
  fn disarm(&mut self) {
    self.id = None;
  }
}

impl<I, G, SR> Drop for JoinGuard<'_, I, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  fn drop(&mut self) {
    if let Some(id) = self.id.take() {
      self.shared.engine.borrow_mut().cancel_join(id);
    }
  }
}

/// Resolve once the shared shutdown latch flips, polled on the same 20ms cadence the
/// join wait loop uses. Raced against each unresolved-seed lookup in
/// [`Serf::join`](Serf::join) so a resolver parked in a never-completing future cannot
/// leave the join pending past an abrupt stop. `join_wake` is a single-consumer
/// [`Signal`](embassy_sync::signal::Signal), so this polls the latch rather than
/// hanging a second consumer off it.
async fn shutdown_backstop<I, G, SR>(shared: &Shared<I, G, SR>)
where
  I: memberlist_proto::Id,
{
  while !shared.is_shutdown() {
    Timer::after(embassy_time::Duration::from_millis(20)).await;
  }
}

/// A cloneable handle to an embassy-net serf node.
///
/// Holds a shared reference to the node's
/// [`SerfEngine`](serf_embedded::SerfEngine) and the run loop's coordination
/// signals. `join` enqueues work on the engine and awaits the run loop; the sync
/// commands enqueue and wake the pump; the sync accessors borrow the engine
/// directly. Every method takes `&self`, so the handle is shared across the
/// executor's tasks.
///
/// `I` is the node identifier type (e.g. `smol_str::SmolStr`). `A` is the
/// resolver's unresolved address type — the advertise address is resolved to a
/// wire [`SocketAddr`] at construction and the seeds at [`join`](Self::join), so
/// the engine only ever sees `SocketAddr`. `G` is the memberlist gossip RNG and
/// `SR` is serf's own core RNG (both defaulting to [`SmallRng`]); the two are
/// seeded independently so fresh nodes never share a query-id schedule.
pub struct Serf<I, A, G = SmallRng, SR = SmallRng>
where
  I: memberlist_proto::Id,
{
  shared: Rc<Shared<I, G, SR>>,
  // Ties the handle to the resolver's unresolved address type. `fn(A)` keeps the
  // marker contravariant in `A` and free of drop/auto-trait obligations.
  _a: PhantomData<fn(A)>,
}

impl<I, A, G, SR> Clone for Serf<I, A, G, SR>
where
  I: memberlist_proto::Id,
{
  fn clone(&self) -> Self {
    Self {
      shared: self.shared.clone(),
      _a: PhantomData,
    }
  }
}

impl<I, A> Serf<I, A, SmallRng, SmallRng>
where
  I: memberlist_proto::Id + Clone,
{
  /// Construct a node over the caller's embassy-net sockets, returning the handle
  /// and the [`Runner`] to drive.
  ///
  /// The caller owns the embassy-net [`Stack`](embassy_net::Stack) and supplies a
  /// gossip [`UdpSocket`] and the reliable-plane pool of `N` [`TcpSocket`]s. `new`
  /// binds the UDP socket to `cfg.port`, wires up the transport-agnostic
  /// [`SerfEngine`](serf_embedded::SerfEngine), seeds the reliable-plane pool with
  /// the `N` slots, dedicates one to the listener, and arms serf's schedulers. No
  /// I/O occurs here. Drive the returned [`Runner`] with [`Runner::run`] and the
  /// embassy-net stack `Runner` separately.
  ///
  /// Seeds BOTH the gossip RNG and serf's core RNG from independent draws of the
  /// platform [`getrandom`] backend; use [`new_with_rng`](Self::new_with_rng) to
  /// inject your own.
  ///
  /// # Errors
  ///
  /// As [`new_with_rng`](Self::new_with_rng), plus [`InitError::Entropy`] if the
  /// platform entropy backend fails while seeding the default RNGs.
  #[allow(clippy::too_many_arguments)]
  pub async fn new<'a, Res, const N: usize>(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, A>,
    serf_opts: SerfOptions,
    resolver: &Res,
    udp_socket: UdpSocket<'a>,
    tcp_sockets: [TcpSocket<'a>; N],
    now: Instant,
  ) -> Result<(Self, Runner<'a, I, N, SmallRng, SmallRng>), InitError>
  where
    Res: AddressResolver<Address = A>,
  {
    // Draw two independent 64-bit seeds from the platform entropy backend — one
    // for the gossip RNG, one for serf's core RNG — so a fresh node never shares a
    // `(ltime, id)` query-id schedule with a peer.
    let mut b = [0u8; 16];
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
    Self::new_with_rng(
      cfg,
      transform,
      ep_cfg,
      serf_opts,
      resolver,
      udp_socket,
      tcp_sockets,
      now,
      SmallRng::seed_from_u64(word(0)),
      SmallRng::seed_from_u64(word(8)),
    )
    .await
  }
}

impl<I, A, G, SR> Serf<I, A, G, SR>
where
  I: memberlist_proto::Id + Clone,
  SR: SeedableRng,
{
  /// Like [`new`](Self::new) but with caller-supplied gossip + serf RNGs, returning
  /// the handle and the [`Runner`] to drive.
  ///
  /// Identical to [`new`](Self::new) except the RNGs: the caller owns seeding both
  /// `gossip_rng` (memberlist peer selection / timing jitter) and `serf_rng`
  /// (serf's query ids / relay selection), so no platform entropy is drawn here.
  ///
  /// # Errors
  ///
  /// - [`InitError::TcpPoolTooSmall`] — `N < 2` (a listener plus one dial/accept
  ///   socket is the functional minimum).
  /// - [`InitError::ZeroBridgeRing`] — a zero bridge ring capacity.
  /// - [`InitError::SocketTimeoutOutOfRange`] — the socket timeout, as installed
  ///   into smoltcp, is not strictly greater than both `close_timeout` and the
  ///   machine's `stream_timeout`, or is above the safe maximum.
  /// - [`InitError::Resolve`] / [`InitError::NoAddresses`] — advertise resolution.
  /// - [`InitError::Engine`] — the shared engine rejected the configuration.
  ///
  /// # Panics
  ///
  /// Panics if binding the supplied `udp_socket` to `cfg.port` fails — which, with
  /// a non-zero port and a fresh socket, embassy-net does not do.
  #[allow(clippy::too_many_arguments)]
  pub async fn new_with_rng<'a, Res, const N: usize>(
    cfg: Options,
    transform: TransformOptions,
    ep_cfg: EndpointOptions<I, A>,
    serf_opts: SerfOptions,
    resolver: &Res,
    mut udp_socket: UdpSocket<'a>,
    tcp_sockets: [TcpSocket<'a>; N],
    now: Instant,
    gossip_rng: G,
    serf_rng: SR,
  ) -> Result<(Self, Runner<'a, I, N, G, SR>), InitError>
  where
    Res: AddressResolver<Address = A>,
    // Arming the schedulers (`SerfEngine::start`, below) draws from both RNGs.
    G: Rng,
    SR: Rng,
  {
    // Deterministic local-config guards first, before resolving or binding.
    if N < 2 {
      return Err(InitError::TcpPoolTooSmall(N));
    }
    if cfg.tcp_socket_rx_bytes == 0 || cfg.tcp_socket_tx_bytes == 0 {
      return Err(InitError::ZeroBridgeRing);
    }
    // The per-socket inactivity timeout must fire strictly AFTER the engine's own
    // deadlines, enforced on the whole-microsecond value embassy-net installs into
    // smoltcp (see `checked_socket_timeout`).
    let stream_timeout = ep_cfg.stream_timeout();
    let socket_ticks = checked_socket_timeout(
      cfg.socket_timeout,
      cfg.close_timeout,
      stream_timeout,
      embassy_time::TICK_HZ as u128,
    )
    .ok_or(InitError::SocketTimeoutOutOfRange(
      SocketTimeoutOutOfRange {
        socket_timeout: cfg.socket_timeout,
        close_timeout: cfg.close_timeout,
        stream_timeout,
        max: MAX_SOCKET_TIMEOUT,
        tick_hz: embassy_time::TICK_HZ,
      },
    ))?;
    let socket_timeout = embassy_time::Duration::from_ticks(socket_ticks);

    // Engine advertise-independent preflight before resolving / binding, so a zero
    // port or over-ceiling MTU fails deterministically.
    let embedded_cfg = embedded_options(&cfg);
    validate_runtime_config(&embedded_cfg, &transform, ep_cfg.gossip_mtu())
      .map_err(InitError::from)?;

    // Resolve the advertise address into a single wire `SocketAddr`, then re-type
    // `ep_cfg` so the rest of construction — and the engine — only sees the resolved
    // address.
    let resolved = resolver
      .resolve(ep_cfg.advertise_addr_ref())
      .await
      .map_err(|e| InitError::Resolve(Box::new(e)))?
      .into_iter()
      .next()
      .ok_or(InitError::NoAddresses)?;
    let ep_cfg = ep_cfg.map_advertise(|_| resolved);
    let advertise = *ep_cfg.advertise_addr_ref();

    // Bind the gossip socket. The preflight rejected port 0, so with a fresh socket
    // this cannot fail; a misuse (already-bound socket) is a programming error.
    udp_socket
      .bind(cfg.port)
      .expect("binding the gossip UDP socket to the configured port failed");

    // Build the engine, seeding BOTH RNGs. `try_new_at_with_rng` maps a
    // machine/keyring/advertise failure to a typed `InitError`.
    let mut engine: SerfEngine<I, SlotId, G, SR> = SerfEngine::try_new_at_with_rng(
      embedded_cfg,
      transform,
      ep_cfg,
      serf_opts,
      now,
      gossip_rng,
      serf_rng,
    )
    .map_err(InitError::from)?;

    // Seed the reliable-plane pool with every slot id, then dedicate one to the
    // listener. The engine owns this pool (it reaches it directly, not through the
    // `StreamIo` view), exactly like the smoltcp driver.
    for i in 0..N {
      engine.plane_mut().pool.push(SlotId(i));
    }

    // Per-slot mailboxes + command wakes; the ring capacities come from the driver
    // config so a slot's bridge never holds more than a socket buffer's worth of
    // un-handed-off bytes.
    let mailboxes: [_; N] = core::array::from_fn(|_| {
      core::cell::RefCell::new(Mailbox::new(
        cfg.tcp_socket_rx_bytes,
        cfg.tcp_socket_tx_bytes,
      ))
    });
    let cmd_wakes: [SlotWake; N] = core::array::from_fn(|_| SlotWake::new());

    // Dedicate one pooled slot to the listener and post its worker a `Listen`
    // directive so it begins accepting on the bound port at startup.
    if let Some(listener) = engine.plane_mut().pool.take() {
      mailboxes[listener.0].borrow_mut().command = Command::Listen(cfg.port);
      engine.set_listener(listener);
    }

    // Arm serf's periodic probe / gossip / push-pull schedulers so they are live
    // from the first pump.
    engine.start(now);

    let shared = Rc::new(Shared::new(engine, advertise));
    let runner = Runner {
      shared: shared.clone(),
      udp: udp_socket,
      tcp: tcp_sockets,
      mailboxes,
      cmd_wakes,
      socket_timeout,
      // The engine owns and drives its own pool, so the driver-side free-list
      // starts empty (and stays unused — see `SerfStream`'s pool methods).
      free: Vec::new(),
    };

    Ok((
      Self {
        shared,
        _a: PhantomData,
      },
      runner,
    ))
  }
}

// Pure driver / reliable-plane reads — needing neither RNG.
impl<I, A, G, SR> Serf<I, A, G, SR>
where
  I: memberlist_proto::Id + Clone,
{
  /// The local node's advertised `SocketAddr`.
  #[inline]
  pub fn advertise_address(&self) -> SocketAddr {
    self.shared.advertise
  }

  /// Whether the run loop has observed a lost id-conflict [`Event::Shutdown`] and
  /// the node should stop.
  #[inline]
  pub fn is_shutdown(&self) -> bool {
    self.shared.is_shutdown()
  }

  /// Abruptly stop the local node.
  ///
  /// Latches the terminal shutdown state and wakes the run loop: the
  /// [`Runner`](crate::Runner) observes the latch BEFORE its next egress-capable pump,
  /// runs one no-pump final drain, and returns, collapsing its workers so the gossip
  /// and reliable-plane sockets wind down. Pending [`join`](Self::join)s and every
  /// subsequent command fail fast with the shutdown error
  /// ([`JoinError::Shutdown`](crate::JoinError::Shutdown) /
  /// [`OpError::Shutdown`](crate::OpError::Shutdown)); events already buffered stay
  /// drainable via [`poll_event`](Self::poll_event).
  ///
  /// An abrupt stop does NOT flush in-flight traffic: any outbound the latch
  /// pre-empts — an undisseminated gossip broadcast, a queued key response, a
  /// looped-back self-datagram — is dropped rather than transmitted, so a stopped node
  /// (a manual stop or a conflict-losing duplicate) never emits on the wire after the
  /// terminal state is observable.
  ///
  /// This does NOT gossip a leave — call [`leave`](Self::leave) for a graceful
  /// departure that notifies peers. The node initiated the stop, so no
  /// [`Event::Shutdown`](serf_embedded::Event::Shutdown) is synthesized for it; a
  /// lost id-conflict vote drives this same terminal path but, being unsolicited,
  /// DOES surface that event through [`poll_event`](Self::poll_event). Idempotent: a
  /// second call re-signals the run loop but the shutdown latch never clears.
  #[inline]
  pub fn shutdown(&self) {
    self.shared.begin_shutdown();
  }

  /// Drain one application-visible serf event the run loop buffered, mandatory
  /// driver-actioned events first (the run loop has ALREADY acted on them), then
  /// passive observations. `None` when the queue is empty.
  #[inline]
  pub fn poll_event(&self) -> Option<Event<I, SocketAddr>> {
    self.shared.pop_app_event()
  }

  /// Number of inbound reliable connections accepted since construction.
  #[doc(hidden)]
  #[inline]
  pub fn accepted_inbound_count(&self) -> u64 {
    self.shared.engine.borrow().accepted_inbound_count()
  }

  /// Number of pooled TCP slots currently free.
  #[doc(hidden)]
  #[inline]
  pub fn pool_free_count(&self) -> usize {
    self.shared.engine.borrow().pool_free_count()
  }

  /// Number of TCP slots currently parked mid-close.
  #[doc(hidden)]
  #[inline]
  pub fn closing_count(&self) -> usize {
    self.shared.engine.borrow().closing_count()
  }

  /// Number of reliable exchanges currently half-closed.
  #[doc(hidden)]
  #[inline]
  pub fn half_closed_count(&self) -> usize {
    self.shared.engine.borrow().half_closed_count()
  }

  /// Whether a passive-open listener slot is currently installed.
  #[doc(hidden)]
  #[inline]
  pub fn listener_present(&self) -> bool {
    self.shared.engine.borrow().listener_present()
  }

  /// Number of reliable exchanges still in `PendingDial`.
  #[doc(hidden)]
  #[inline]
  pub fn pending_dial_count(&self) -> usize {
    self.shared.engine.borrow().pending_dial_count()
  }

  /// Number of await-result joins currently tracked.
  #[doc(hidden)]
  #[inline]
  pub fn pending_join_count(&self) -> usize {
    self.shared.engine.borrow().pending_join_count()
  }
}

// serf reads + command surface + the async join — reach serf's super-machine, so
// they carry the gossip `G: Rng` and serf's `SR: Rng + SeedableRng` bounds. The
// engine's connection handle is the concrete `SlotId` (Copy + Eq + Hash), so the
// pump's `C` bound is satisfied without an extra parameter.
impl<I, A, G, SR> Serf<I, A, G, SR>
where
  I: memberlist_proto::Id + Clone,
  G: Rng,
  SR: Rng + SeedableRng,
{
  /// serf's current lifecycle state.
  #[inline]
  pub fn state(&self) -> SerfState {
    self.shared.engine.borrow().state()
  }

  /// Number of serf members currently tracked (including the local node).
  #[inline]
  pub fn num_members(&self) -> usize {
    self.shared.engine.borrow().num_members()
  }

  /// A snapshot of every serf member currently tracked (alive, leaving, left, or
  /// failed within the reap window).
  #[inline]
  pub fn members(&self) -> Vec<Arc<Member<I, SocketAddr>>> {
    self.shared.engine.borrow().members_snapshot()
  }

  /// The local node's id.
  #[inline]
  pub fn local_id(&self) -> I {
    self.shared.engine.borrow().local_id().clone()
  }

  /// The local node's serf member Lamport clock.
  #[inline]
  pub fn member_time(&self) -> u64 {
    self.shared.engine.borrow().member_time()
  }

  /// The local node's serf event Lamport clock.
  #[inline]
  pub fn event_time(&self) -> u64 {
    self.shared.engine.borrow().event_time()
  }

  /// The local node's serf query Lamport clock.
  #[inline]
  pub fn query_time(&self) -> u64 {
    self.shared.engine.borrow().query_time()
  }

  /// The number of app events shed because a consumer did not keep up: the engine's
  /// own passive-observation drops plus this driver's
  /// [`poll_event`](Self::poll_event) backlog drops.
  #[inline]
  pub fn events_dropped(&self) -> u64 {
    self.shared.events_dropped()
  }

  /// Announce the local node's join intent and await the result: resolve each seed,
  /// dispatch an await-result join on the engine, and await its outcome.
  ///
  /// Each seed is resolved through `resolver` (a [`MaybeResolved::Resolved`] address
  /// is used verbatim, a [`MaybeResolved::Unresolved`] one is expanded into the wire
  /// addresses the resolver yields). The run loop initiates a push/pull to each
  /// routable seed and folds every completion into the join; this call resolves once
  /// every dispatched push/pull has terminated: `Ok(ReachedSet)` with the reached
  /// addresses if any seed was contacted, else [`JoinError::Failed`]. If the future
  /// is dropped before it resolves (e.g. a `select` timeout), the in-flight join is
  /// cancelled so nothing leaks.
  ///
  /// When `ignore_old` is set, each seed's push/pull suppresses replay of the peer's
  /// pre-join user events.
  ///
  /// # Errors
  ///
  /// [`JoinError::Control`] when the engine rejects the join (e.g. the node is not
  /// running), [`JoinError::Resolve`] on a resolver failure, [`JoinError::NoAddresses`]
  /// when a non-empty seed set resolves to no address, [`JoinError::Failed`] when
  /// every dispatched push/pull terminated without contacting a seed, or
  /// [`JoinError::Shutdown`] if the node lost an id-conflict vote or was stopped —
  /// before dispatch, while a seed was still resolving, or while the join was in
  /// flight — so a stopped node never dispatches or hangs.
  pub async fn join<Res>(
    &self,
    resolver: &Res,
    seeds: &[MaybeResolved<A>],
    ignore_old: bool,
  ) -> Result<ReachedSet, JoinError>
  where
    Res: AddressResolver<Address = A>,
  {
    // Fail fast if the node already lost an id-conflict vote or was stopped: a join
    // under a duplicate identity is meaningless, and the stopped run loop would never
    // dispatch its push/pulls.
    if self.shared.is_shutdown() {
      return Err(JoinError::Shutdown);
    }

    let now = time::now();
    let mut resolved = Vec::with_capacity(seeds.len());
    for seed in seeds {
      match seed {
        MaybeResolved::Resolved(s) => resolved.push(*s),
        MaybeResolved::Unresolved(a) => {
          // Race each unresolved-seed lookup against the shutdown latch: a resolver
          // that never completes must not leave the join pending past an abrupt stop,
          // and one that resolves only after the stop must not reach the stopped
          // engine. The backstop resolves only when the latch flips.
          let result = match select(resolver.resolve(a), shutdown_backstop(&self.shared)).await {
            Either::First(r) => r,
            Either::Second(()) => return Err(JoinError::Shutdown),
          };
          // Re-check after the await so a latch that flipped just as the resolver won
          // the race still stops the join here: a post-shutdown resolver error or empty
          // result resolves as `Shutdown`, never `Resolve` / `NoAddresses`.
          if self.shared.is_shutdown() {
            return Err(JoinError::Shutdown);
          }
          resolved.extend(result.map_err(|e| JoinError::Resolve(Box::new(e)))?);
        }
      }
    }
    if !seeds.is_empty() && resolved.is_empty() {
      return Err(JoinError::NoAddresses);
    }

    // A shutdown latched after the final resolver await — or during an all-`Resolved`
    // seed set that raced no resolver — must not dispatch onto the stopped engine.
    if self.shared.is_shutdown() {
      return Err(JoinError::Shutdown);
    }

    let handle = self
      .shared
      .engine
      .borrow_mut()
      .join(&resolved, ignore_old, now)
      .map_err(JoinError::Control)?;
    self.shared.wake_pump();

    // Cancel the in-flight join if this future is dropped before it resolves.
    let mut guard = JoinGuard {
      shared: &self.shared,
      id: Some(handle),
    };

    // Park until the pump folds the terminal completion into the join. The run loop
    // pulses `join_wake` after every drain; race it against a short timer so a wake
    // the single-consumer signal delivered to another concurrent joiner only costs
    // an interval, never a hang.
    loop {
      let outcome = self.shared.engine.borrow_mut().poll_join(handle);
      if let Some(outcome) = outcome {
        // Resolved: do not cancel on drop.
        guard.disarm();
        return outcome.map_err(JoinError::Failed);
      }
      // The run loop stopped mid-join after a lost id-conflict shutdown: resolve
      // with the shutdown error rather than spin the backstop forever. The guard is
      // left armed so its drop cancels the now-orphaned engine-side join entry
      // exactly once (no borrow is live here, so the drop's `borrow_mut` is safe).
      if self.shared.is_shutdown() {
        return Err(JoinError::Shutdown);
      }
      // Ignoring the `Either`: whichever of the join wake or the timer fired, the
      // loop simply re-checks `poll_join`.
      let _ = select(
        self.shared.join_wake.wait(),
        Timer::after(embassy_time::Duration::from_millis(20)),
      )
      .await;
    }
  }

  /// Begin leaving the cluster. Gossips the departure and ultimately emits
  /// [`Event::LeftCluster`] via [`poll_event`](Self::poll_event).
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] if the node already lost an id-conflict vote (the run
  /// loop has stopped), or [`OpError::Serf`] if the engine rejects the leave (not in
  /// a running state — already left or a refused leave).
  pub fn leave(&self) -> Result<(), OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().leave(now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Force a named node out of the cluster (an operator-driven removal).
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] after a lost id-conflict vote, else the engine's
  /// rejection as [`OpError::Serf`].
  pub fn force_leave(&self, id: I, prune: bool) -> Result<(), OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().force_leave(id, prune, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Broadcast an application user event to the cluster. `coalesce` requests that
  /// identical events be coalesced by name. Peers observe it as [`Event::User`].
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] after a lost id-conflict vote, else the engine's
  /// rejection as [`OpError::Serf`] (e.g. an oversized event).
  pub fn user_event(
    &self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    coalesce: bool,
  ) -> Result<(), OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self
      .shared
      .engine
      .borrow_mut()
      .user_event(name, payload, coalesce, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Issue a cluster-wide query, returning its [`QueryId`]. Responders observe it as
  /// [`Event::Query`] and answer via [`respond`](Self::respond).
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] after a lost id-conflict vote, else the engine's
  /// rejection as [`OpError::Serf`].
  pub fn query(
    &self,
    name: impl Into<smol_str::SmolStr>,
    payload: bytes::Bytes,
    params: QueryParams<I>,
  ) -> Result<QueryId, OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self
      .shared
      .engine
      .borrow_mut()
      .query(name, payload, params, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Answer a received query. `token` is the [`QueryEvent`] delivered via
  /// [`Event::Query`].
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] after a lost id-conflict vote, else the engine's
  /// rejection as [`OpError::Serf`] (e.g. a duplicate or past-deadline respond).
  pub fn respond(
    &self,
    token: &QueryEvent<I, SocketAddr>,
    payload: bytes::Bytes,
  ) -> Result<(), OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().respond(token, payload, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Replace the local node's tags, re-advertising them and refreshing the local
  /// member in the membership store.
  ///
  /// # Errors
  ///
  /// [`OpError::Shutdown`] after a lost id-conflict vote, else the engine's
  /// rejection as [`OpError::Serf`].
  pub fn set_tags(&self, tags: Tags) -> Result<(), OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().set_tags(tags, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Issue a cluster-wide `install_key` query to add `key` to every node's keyring.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn install_key(&self, key: SecretKey) -> Result<QueryId, OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().install_key(key, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Issue a cluster-wide `use_key` query to promote `key` to primary.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn use_key(&self, key: SecretKey) -> Result<QueryId, OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().use_key(key, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Issue a cluster-wide `remove_key` query to remove `key` from all nodes.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn remove_key(&self, key: SecretKey) -> Result<QueryId, OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().remove_key(key, now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// Issue a cluster-wide `list_keys` query to enumerate installed keys.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn list_keys(&self) -> Result<QueryId, OpError> {
    if self.shared.is_shutdown() {
      return Err(OpError::Shutdown);
    }
    let now = time::now();
    let r = self.shared.engine.borrow_mut().list_keys(now);
    self.shared.wake_pump();
    r.map_err(OpError::from)
  }

  /// A clone of the node's LIVE wire keyring — the keyring the gossip and reliable
  /// planes actually encrypt under, and the state an inbound [`Event::KeyRequest`]
  /// rotates via the engine. `None` when the node is unencrypted. Unlike
  /// [`list_keys`](Self::list_keys) (a cluster-wide query), this is a local read of
  /// this node's own keyring for UI / diagnostics / tests. Returned by value because
  /// the engine lives behind interior mutability.
  #[cfg(encryption)]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "aes-gcm", feature = "chacha20-poly1305")))
  )]
  pub fn keyring(&self) -> Option<Keyring> {
    self.shared.engine.borrow().keyring().cloned()
  }
}
