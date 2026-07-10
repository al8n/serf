//! Shutdown is TERMINAL: a stopped node — whether it called `Serf::shutdown` or lost
//! an id-conflict vote — stops pumping, closes its sockets, and rejects further
//! commands, so two nodes can never stay active under one identity.
//!
//! Both triggers reach the same terminal path (`begin_shutdown`). These end-to-end
//! tests drive the public `Serf::shutdown` and assert that ENFORCEMENT: the runner
//! future completes, the sockets wind down, in-flight and subsequent commands fail
//! with the shutdown error, and a peer is unaffected. The extra `Event::Shutdown` a
//! real vote loss surfaces on top (the engine emits it and the drain buffers it for
//! the app) is covered by the driver's own `shared` unit test.

mod support;

use core::cell::Cell;

use embassy_futures::select::{Either, select};
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use futures::executor::block_on;
use serf_embassy::{
  Bytes, JoinError, MaybeResolved, OpError, ReachedSet, SocketAddrResolver, TransformOptions, now,
};

use support::cluster::{
  NodeBufs, POOL, TEST_TIMEOUT, addr, build_node, build_sockets, build_stack, devices,
  join_and_converge,
};

/// Two converged nodes; A calls the public `shutdown()`. A's stop must be terminal —
/// its `Runner::run` future completes (the pump stopped and the sockets wound down)
/// and every subsequent command fails fast with [`OpError::Shutdown`] — while the
/// peer B keeps running and keeps accepting commands.
///
/// Reverting the enforcement to flag-only (the pump keeps looping, the handle keeps
/// accepting commands) makes A's runner never complete and the post-shutdown command
/// succeed, so both assertions below fail.
#[test]
fn shutdown_stops_the_node_and_spares_the_peer() {
  let (dev_a, dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let mut res_b = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);
  let (stack_b, mut net_b) = build_stack(dev_b, &mut res_b, 2, 0x3333_4444);

  let mut bufs_a = NodeBufs::new();
  let mut bufs_b = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);
  let (udp_b, tcp_b) = build_sockets(stack_b, &mut bufs_b);

  let now = now();
  let (ml_a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, TransformOptions::default());
  let (ml_b, run_b) = build_node(udp_b, tcp_b, "b", 2, now, TransformOptions::default());

  block_on(async {
    // Latched by A's runner future the instant it completes, so the orchestration
    // can observe the terminal stop (and then park so the composed infra keeps B and
    // both stacks alive while the assertions run).
    let a_stopped = Cell::new(false);
    let run_a_then_park = async {
      run_a.run().await;
      a_stopped.set(true);
      core::future::pending::<()>().await;
    };

    let orchestrate = async {
      // Converge: B joins A, both reach a 2-member view.
      join_and_converge(&ml_a, &ml_b).await;

      // A stops abruptly via the public shutdown command.
      ml_a.shutdown();

      // A's flag flips AND its runner future completes (the pump stopped and
      // `Runner::run`'s select collapsed the workers, closing the sockets).
      loop {
        if ml_a.is_shutdown() && a_stopped.get() {
          break;
        }
        Timer::after(Duration::from_millis(2)).await;
      }

      // A rejects further commands fast — a stopped node cannot keep acting.
      let post = ml_a.user_event("post-shutdown", Bytes::from_static(b"x"), false);
      assert!(
        matches!(post, Err(OpError::Shutdown)),
        "a command after shutdown must fail with OpError::Shutdown, got {post:?}"
      );
      assert!(
        ml_a.leave().is_err(),
        "leave after shutdown must also be rejected"
      );

      // The peer keeps running: not shut down, and still accepting commands.
      assert!(!ml_b.is_shutdown(), "the peer must not be shut down");
      ml_b
        .user_event("peer-still-live", Bytes::from_static(b"y"), false)
        .expect("the peer keeps accepting commands");
      true
    };

    // Drive A's (stopping) runner, B's (forever) runner, both stacks, and the
    // orchestration against the wall-clock timeout.
    let infra = select(
      select(run_a_then_park, run_b.run()),
      select(net_a.run(), net_b.run()),
    );
    match select(orchestrate, select(infra, Timer::after(TEST_TIMEOUT))).await {
      Either::First(ok) => assert!(ok),
      Either::Second(_) => panic!("conflict-shutdown test timed out after {TEST_TIMEOUT:?}"),
    }
  });
}

/// A join in flight when a `shutdown()` lands must resolve with
/// [`JoinError::Shutdown`] rather than spin its backstop forever, and the
/// join's drop-cancel of the orphaned engine-side entry must be clean (no panic).
#[test]
fn join_in_flight_resolves_on_shutdown() {
  // A single node is enough: it joins an unreachable seed so the push/pull stays in
  // flight, then shuts down before the dial can resolve.
  let (dev_a, _dev_b) = devices();
  let mut res_a = StackResources::<{ POOL + 2 }>::new();
  let (stack_a, mut net_a) = build_stack(dev_a, &mut res_a, 1, 0x1111_2222);

  let mut bufs_a = NodeBufs::new();
  let (udp_a, tcp_a) = build_sockets(stack_a, &mut bufs_a);

  let now = now();
  let (ml_a, run_a) = build_node(udp_a, tcp_a, "a", 1, now, TransformOptions::default());

  block_on(async {
    let run_a_then_park = async {
      run_a.run().await;
      core::future::pending::<()>().await;
    };

    let orchestrate = async {
      // Start a join to an address no node answers, so it parks in flight. The seed
      // slice must outlive the held `join_fut`, so bind it.
      let seeds = [MaybeResolved::Resolved(addr(9, 7946))];
      let join_fut = ml_a.join(&SocketAddrResolver, &seeds, false);
      // Once the join is registered and in flight, shut the node down.
      let trigger = async {
        loop {
          if ml_a.pending_join_count() > 0 {
            break;
          }
          Timer::after(Duration::from_millis(2)).await;
        }
        ml_a.shutdown();
        // The join resolves; this branch just keeps the trigger alive meanwhile.
        core::future::pending::<Result<ReachedSet, JoinError>>().await
      };
      let result = match select(join_fut, trigger).await {
        Either::First(r) => r,
        Either::Second(unreached) => unreached,
      };
      assert!(
        matches!(result, Err(JoinError::Shutdown)),
        "a join in flight when shutdown lands must resolve with JoinError::Shutdown, got {result:?}"
      );
      // A join attempted AFTER shutdown also fails fast (and the guard drop above was
      // clean — a panic there would have aborted the test).
      let again = ml_a
        .join(
          &SocketAddrResolver,
          &[MaybeResolved::Resolved(addr(9, 7946))],
          false,
        )
        .await;
      assert!(
        matches!(again, Err(JoinError::Shutdown)),
        "a join after shutdown must fail fast with JoinError::Shutdown, got {again:?}"
      );
      true
    };

    let infra = select(run_a_then_park, net_a.run());
    match select(orchestrate, select(infra, Timer::after(TEST_TIMEOUT))).await {
      Either::First(ok) => assert!(ok),
      Either::Second(_) => panic!("join-vs-shutdown test timed out after {TEST_TIMEOUT:?}"),
    }
  });
}
