//! Unit tests for the pure Vivaldi `CoordinateClient` engine.

use core::time::Duration;

use memberlist_proto::SmallRng;
use rand::SeedableRng;

use crate::Coordinate;

use super::{
  CoordinateClient, CoordinateClientStats, CoordinateError, CoordinateOptions, ZERO_THRESHOLD,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns a deterministic seeded RNG for use in tests.
fn test_rng() -> SmallRng {
  SmallRng::seed_from_u64(0xdead_beef_cafe_babe)
}

/// Returns a `CoordinateOptions` with the given dimensionality and zero
/// height_min (makes distance assertions easier).
fn opts_dim(d: usize) -> CoordinateOptions {
  CoordinateOptions::new()
    .with_dimensionality(d)
    .with_height_min(0.0)
}

/// Build a zero coordinate with `dim` dimensions.
fn zero_coord(dim: usize) -> Coordinate {
  Coordinate {
    vec: vec![0.0; dim],
    error: CoordinateOptions::new().vivaldi_error_max(),
    adjustment: 0.0,
    height: 0.0,
  }
}

/// Assert two floats are within `ZERO_THRESHOLD` of each other.
fn assert_float_eq(a: f64, b: f64) {
  assert!(
    (a - b).abs() <= ZERO_THRESHOLD,
    "float mismatch: {a:.9} != {b:.9}"
  );
}

// ── Construction ──────────────────────────────────────────────────────────────

/// A freshly constructed client begins at the origin with default error and
/// zero height (when `height_min` is 0).
#[test]
fn new_client_starts_at_origin() {
  let opts = opts_dim(3);
  let c = CoordinateClient::<u32>::new(opts.clone());
  let coord = c.get_coordinate();
  assert_eq!(coord.vec, vec![0.0; 3]);
  assert_float_eq(coord.error, opts.vivaldi_error_max());
  assert_float_eq(coord.adjustment, 0.0);
}

/// `default()` and `new()` produce identical defaults.
#[test]
fn options_default_matches_new() {
  let a = CoordinateOptions::default();
  let b = CoordinateOptions::new();
  assert_eq!(a, b);
}

// ── Update convergence ────────────────────────────────────────────────────────

/// After one update, the coordinate moves to reduce the error between the
/// predicted and observed RTT.  Here the peer is above the origin and the
/// observed RTT is twice the peer's displacement — the client should scoot
/// *away* from the peer (downward in dim 2) to increase its predicted distance.
#[test]
fn coordinate_update_moves_toward_peer() {
  let opts = opts_dim(3);
  let mut c = CoordinateClient::<u32>::new(opts.clone());
  let before = c.get_coordinate();

  let mut peer = zero_coord(3);
  peer.vec[2] = 0.001; // peer is 0.001 s away in dim 2

  let rtt = Duration::from_nanos((2.0 * peer.vec[2] * 1.0e9) as u64);
  let mut rng = test_rng();
  let updated = c.update(&2u32, &peer, rtt, &mut rng).unwrap();

  // The client should have moved (coordinate changed from origin).
  assert_ne!(updated.vec, before.vec);
  // Specifically, it should move away from the peer (dim 2 goes negative).
  assert!(
    updated.vec[2] < 0.0,
    "client should move away from peer in dim 2"
  );
}

/// Repeated updates between two symmetrically placed nodes converge: the
/// coordinate difference should approach the true RTT (within some tolerance).
#[test]
fn update_converges_over_multiple_observations() {
  let opts = CoordinateOptions::new()
    .with_dimensionality(3)
    .with_height_min(0.0);
  let mut a = CoordinateClient::<u32>::new(opts.clone());
  let mut b = CoordinateClient::<u32>::new(opts);

  let true_rtt = Duration::from_millis(20);
  let mut rng = test_rng();

  for _ in 0..100 {
    let coord_b = b.get_coordinate();
    a.update(&1u32, &coord_b, true_rtt, &mut rng).unwrap();
    let coord_a = a.get_coordinate();
    b.update(&0u32, &coord_a, true_rtt, &mut rng).unwrap();
  }

  let predicted = a.distance_to(&b.get_coordinate());
  let diff_ms = (predicted.as_secs_f64() - true_rtt.as_secs_f64()).abs() * 1000.0;
  // After 100 rounds the prediction should be within 5 ms of the true RTT.
  assert!(
    diff_ms < 5.0,
    "convergence failed: predicted {predicted:?}, true {true_rtt:?}, diff {diff_ms:.3} ms"
  );
}

// ── distance_to ───────────────────────────────────────────────────────────────

/// `distance_to` is symmetric: distance from C1 to C2 equals distance from C2 to C1.
///
/// Both `Coordinate::distance_to` and `CoordinateClient::distance_to` are tested since
/// the `Coordinate` wire type carries adjustments that feed into the symmetric formula.
#[test]
fn distance_is_symmetric() {
  let opts = opts_dim(3);

  // Two clients both starting at the origin; give them different coordinates manually.
  let mut c1 = Coordinate {
    vec: vec![1.0, 2.0, 3.0],
    error: 1.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let mut c2 = Coordinate {
    vec: vec![4.0, -1.0, 0.0],
    error: 1.5,
    adjustment: 0.0,
    height: 0.0,
  };

  // Test at the Coordinate level (both directions).
  let mut a = CoordinateClient::<u32>::new(opts.clone());
  let mut b = CoordinateClient::<u32>::new(opts);

  a.set_coordinate(c1.clone()).unwrap();
  b.set_coordinate(c2.clone()).unwrap();

  let d_ab = a.distance_to(&c2);
  let d_ba = b.distance_to(&c1);
  assert_eq!(
    d_ab, d_ba,
    "distance must be symmetric: {d_ab:?} != {d_ba:?}"
  );

  // Also confirm the formula is symmetric when adjustments are non-zero.
  c1.adjustment = 0.001;
  c2.adjustment = 0.002;
  a.set_coordinate(c1.clone()).unwrap();
  b.set_coordinate(c2.clone()).unwrap();
  let d_adj_ab = a.distance_to(&c2);
  let d_adj_ba = b.distance_to(&c1);
  assert_eq!(
    d_adj_ab, d_adj_ba,
    "distance must be symmetric with adjustments"
  );
}

/// With zero height, a coordinate exactly `x` seconds away in one dimension
/// should report a distance of `x` seconds.
#[test]
fn distance_to_exact_value_when_height_zero() {
  let opts = opts_dim(3).with_height_min(0.0);
  let a = CoordinateClient::<u32>::new(opts.clone());

  let mut other = zero_coord(3);
  other.height = 0.0;
  other.adjustment = 0.0;
  other.vec[2] = 12.345;

  let expected = Duration::from_nanos((12.345 * 1.0e9) as u64);
  let got = a.distance_to(&other);
  assert_eq!(got, expected);
}

// ── Height term ───────────────────────────────────────────────────────────────

/// When `height_min > 0` the height term introduces a positive floor on the
/// distance estimate even for co-located coordinates.
#[test]
fn height_term_adds_floor_to_distance() {
  // Use CoordinateOptions::new() so that height_min = 10e-6 (non-zero).
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let a = CoordinateClient::<u32>::new(opts.clone());
  let other = Coordinate {
    vec: vec![0.0; 3],
    error: opts.vivaldi_error_max(),
    adjustment: 0.0,
    height: opts.height_min(),
  };
  // Both nodes have the same coordinate vector: distance = 2 * height_min.
  let dist = a.distance_to(&other).as_secs_f64();
  assert!(
    dist > 0.0,
    "height term should produce a positive floor, got {dist}"
  );
  assert_float_eq(dist, 2.0 * opts.height_min());
}

// ── Adjustment term ───────────────────────────────────────────────────────────

/// After several updates where the observed RTT differs from the predicted
/// distance, the adjustment field becomes non-zero.
#[test]
fn adjustment_becomes_nonzero_after_updates() {
  let opts = CoordinateOptions::new()
    .with_dimensionality(3)
    .with_height_min(0.0);
  let mut c = CoordinateClient::<u32>::new(opts);

  // Peer at the origin — the client starts at the origin too, so the
  // Euclidean distance is 0 while we observe a non-zero RTT.  The
  // adjustment window will accumulate `rtt - raw_distance = 0.1 - ~0`,
  // driving the adjustment to a positive value.
  let peer = zero_coord(3);

  let mut rng = test_rng();
  for _ in 0..50 {
    c.update(&42u32, &peer, Duration::from_millis(100), &mut rng)
      .unwrap();
  }

  let coord = c.get_coordinate();
  // The adjustment must have drifted away from its initial zero.
  assert_ne!(
    coord.adjustment, 0.0,
    "adjustment should be non-zero after updates with mismatched RTT"
  );
}

/// When `adjustment_window_size == 0` the adjustment field stays at zero.
#[test]
fn adjustment_stays_zero_when_window_disabled() {
  let opts = CoordinateOptions::new()
    .with_dimensionality(3)
    .with_height_min(0.0)
    .with_adjustment_window_size(0);
  let mut c = CoordinateClient::<u32>::new(opts);

  let mut peer = zero_coord(3);
  peer.vec[2] = 0.05;

  let mut rng = test_rng();
  for _ in 0..10 {
    c.update(&1u32, &peer, Duration::from_millis(50), &mut rng)
      .unwrap();
  }

  assert_float_eq(c.get_coordinate().adjustment, 0.0);
}

// ── Gravity term ──────────────────────────────────────────────────────────────

/// The gravity term keeps the coordinate from drifting to infinity.  After
/// many updates to a peer far from the origin the coordinate should remain
/// finite and not grow unboundedly.
#[test]
fn gravity_prevents_unbounded_drift() {
  let opts = CoordinateOptions::new()
    .with_dimensionality(3)
    .with_height_min(0.0);
  let mut c = CoordinateClient::<u32>::new(opts);

  // Peer sits far from the origin.
  let peer = Coordinate {
    vec: vec![1000.0, 0.0, 0.0],
    error: 1.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let mut rng = test_rng();
  for _ in 0..200 {
    // Provide an RTT consistent with the distance so Vivaldi pushes outward.
    let _ = c.update(&1u32, &peer, Duration::from_secs(1), &mut rng);
  }

  let coord = c.get_coordinate();
  assert!(
    coord.vec.iter().all(|f| f.is_finite()),
    "coordinate must stay finite"
  );
  assert!(
    coord.vec[0].abs() < 2000.0,
    "gravity should prevent unbounded drift"
  );
}

// ── Latency filter ────────────────────────────────────────────────────────────

/// The latency filter returns the median of the sliding window of RTT samples
/// and ages out old samples correctly.
#[test]
fn latency_filter_returns_median() {
  let opts = CoordinateOptions::new().with_latency_filter_size(3);
  let mut c = CoordinateClient::<u32>::new(opts);

  // First sample: median of [0.201] = 0.201.
  assert_float_eq(c.latency_filter(&1u32, 0.201), 0.201);
  // Second: median of [0.201, 0.200] = 0.201 (middle of sorted pair).
  assert_float_eq(c.latency_filter(&1u32, 0.200), 0.201);
  // Third: median of [0.201, 0.200, 0.207] sorted = [0.200, 0.201, 0.207] -> 0.201.
  assert_float_eq(c.latency_filter(&1u32, 0.207), 0.201);

  // A glitch pushed in: window slides to [0.200, 0.207, 1.9] -> median 0.207.
  assert_float_eq(c.latency_filter(&1u32, 1.9), 0.207);
  // Next: [0.207, 1.9, 0.203] -> sorted [0.203, 0.207, 1.9] -> 0.207.
  assert_float_eq(c.latency_filter(&1u32, 0.203), 0.207);
}

/// Different peers have independent sample windows.
#[test]
fn latency_filter_independent_per_peer() {
  let opts = CoordinateOptions::new().with_latency_filter_size(3);
  let mut c = CoordinateClient::<u32>::new(opts);

  c.latency_filter(&1u32, 0.201);
  c.latency_filter(&1u32, 0.200);
  // Peer 2 has never been seen; first sample is the median.
  assert_float_eq(c.latency_filter(&2u32, 0.310), 0.310);
}

/// `forget_node` clears per-peer history so the next sample starts fresh.
#[test]
fn forget_node_clears_peer_history() {
  let opts = CoordinateOptions::new().with_latency_filter_size(3);
  let mut c = CoordinateClient::<u32>::new(opts);

  c.latency_filter(&1u32, 0.888);
  c.latency_filter(&1u32, 0.888);
  c.forget_node(&1u32);
  // After forgetting, a new sample should be the only one in the window.
  assert_float_eq(c.latency_filter(&1u32, 0.123), 0.123);
}

// ── Error and stats bookkeeping ───────────────────────────────────────────────

/// `stats()` starts at zero and records resets when an update produces an
/// invalid result (NaN poisoned internally, then a valid update triggers the
/// reset path).
#[test]
fn stats_records_resets() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts.clone());

  assert_eq!(c.stats().resets(), 0);

  // Poison the coordinate directly (simulates internal NaN production).
  c.coord.vec[0] = f64::NAN;

  let peer = zero_coord(3);
  let mut rng = test_rng();
  // The update should detect the invalid result, reset, and increment the counter.
  let result = c
    .update(&1u32, &peer, Duration::from_millis(250), &mut rng)
    .unwrap();
  assert!(
    result.vec.iter().all(|f| f.is_finite()),
    "reset should produce valid coordinate"
  );
  assert_eq!(c.stats().resets(), 1);
}

/// `CoordinateClientStats::resets()` accessor works correctly.
#[test]
fn stats_struct_default_is_zero() {
  let s = CoordinateClientStats::default();
  assert_eq!(s.resets(), 0);
}

// ── NaN / invalid input defense ───────────────────────────────────────────────

/// Feeding a NaN coordinate from a peer is rejected before any mutation.
#[test]
fn nan_peer_coordinate_is_rejected() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts.clone());

  let mut bad = zero_coord(3);
  bad.vec[0] = f64::NAN;

  let mut rng = test_rng();
  let err = c
    .update(&1u32, &bad, Duration::from_millis(250), &mut rng)
    .unwrap_err();
  assert_eq!(err, CoordinateError::InvalidCoordinate);
  // Client coordinate must be unchanged.
  assert!(c.get_coordinate().vec.iter().all(|f| f.is_finite()));
}

/// RTTs above 10 seconds are rejected.
#[test]
fn rtt_above_10s_is_rejected() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts);
  let peer = zero_coord(3);
  let mut rng = test_rng();

  let err = c
    .update(&1u32, &peer, Duration::from_secs(11), &mut rng)
    .unwrap_err();
  assert!(matches!(err, CoordinateError::InvalidRtt(_)));
}

/// A coordinate with mismatched dimensionality is rejected.
#[test]
fn dimensionality_mismatch_is_rejected() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts);
  // Peer has 4 dimensions.
  let alien = Coordinate {
    vec: vec![0.0; 4],
    error: 1.5,
    adjustment: 0.0,
    height: 0.0,
  };
  let mut rng = test_rng();

  let err = c
    .update(&1u32, &alien, Duration::from_millis(10), &mut rng)
    .unwrap_err();
  assert_eq!(err, CoordinateError::DimensionalityMismatch);
}

/// `set_coordinate` rejects a coordinate with incompatible dimensions.
#[test]
fn set_coordinate_rejects_wrong_dimensions() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts);
  let alien = Coordinate {
    vec: vec![0.0; 6],
    error: 1.5,
    adjustment: 0.0,
    height: 0.0,
  };

  let err = c.set_coordinate(alien).unwrap_err();
  assert_eq!(err, CoordinateError::DimensionalityMismatch);
}

/// `set_coordinate` accepts a valid coordinate and updates the estimate.
#[test]
fn set_coordinate_updates_estimate() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts);
  let new_coord = Coordinate {
    vec: vec![1.0, 2.0, 3.0],
    error: 0.5,
    adjustment: 0.0,
    height: 0.0,
  };
  c.set_coordinate(new_coord.clone()).unwrap();
  assert_eq!(c.get_coordinate().vec, new_coord.vec);
}

// ── Zero-RTT handling ─────────────────────────────────────────────────────────

// ── FIX: zero latency_filter_size does not panic ──────────────────────────────

/// `CoordinateOptions::with_latency_filter_size(0)` must be clamped to 1 so
/// that the first `update` call does not panic.
///
/// Without the clamp: `latency_filter` pushes a sample (len = 1 > 0), then
/// removes index 0 (len = 0), then indexes `tmp[tmp.len() / 2]` = `tmp[0]`
/// on an empty vec — a bounds-check panic.
///
/// Regression for `CoordinateOptions::with_latency_filter_size(0)`.
#[test]
fn latency_filter_size_zero_is_clamped_and_does_not_panic() {
  let opts = CoordinateOptions::new()
    .with_dimensionality(3)
    .with_latency_filter_size(0);
  // The clamp must have taken effect.
  assert_eq!(
    opts.latency_filter_size(),
    1,
    "latency_filter_size(0) must be clamped to 1"
  );

  let mut c = CoordinateClient::<u32>::new(opts);
  let peer = Coordinate {
    vec: vec![0.0; 3],
    error: CoordinateOptions::new().vivaldi_error_max(),
    adjustment: 0.0,
    height: 0.0,
  };
  let mut rng = test_rng();
  // Must not panic.
  let result = c.update(&1u32, &peer, Duration::from_millis(10), &mut rng);
  assert!(
    result.is_ok(),
    "update with latency_filter_size clamped to 1 must not panic or error"
  );
}

/// Zero-RTT observations are valid (can occur with coarse-grained monotonic
/// clocks) and do not panic or produce NaN.
#[test]
fn zero_rtt_is_handled_gracefully() {
  let opts = CoordinateOptions::new().with_dimensionality(3);
  let mut c = CoordinateClient::<u32>::new(opts);
  let peer = zero_coord(3);
  let mut rng = test_rng();

  let result = c.update(&1u32, &peer, Duration::ZERO, &mut rng).unwrap();
  assert!(result.vec.iter().all(|f| f.is_finite()));
}

// ── Coincident-point jitter ───────────────────────────────────────────────────

/// When the client and peer share the same position, `update` must not panic
/// (the `unit_vector_at` fallback applies random jitter from the injected rng).
#[test]
fn coincident_coordinates_do_not_panic() {
  let opts = opts_dim(3);
  let mut c = CoordinateClient::<u32>::new(opts.clone());

  // Peer at same position as origin (all zeros).
  let peer = Coordinate {
    vec: vec![0.0; 3],
    error: opts.vivaldi_error_max(),
    adjustment: 0.0,
    height: 0.0,
  };
  let mut rng = test_rng();
  let result = c.update(&1u32, &peer, Duration::from_millis(10), &mut rng);
  assert!(
    result.is_ok(),
    "coincident-point update should not panic or error"
  );
}

// ── FIX 2: zero dimensionality must not cause an infinite spin ────────────────

/// `CoordinateOptions::with_dimensionality(0)` must be clamped to 1.
///
/// Without the clamp: `CoordinateClient::new` builds a zero-length coordinate
/// vector; on `update`, `unit_vector_at` enters the coincident-point retry
/// loop.  The loop iterates over the empty component vector (yielding nothing
/// to randomize), so `jmag` stays 0.0 forever — an infinite CPU spin.
///
/// With the clamp the update completes in bounded time.
#[cfg(feature = "coordinates")]
#[test]
fn dimensionality_zero_is_clamped_to_one_and_update_does_not_spin() {
  let opts = CoordinateOptions::new().with_dimensionality(0);
  assert_eq!(
    opts.dimensionality(),
    1,
    "with_dimensionality(0) must be clamped to 1"
  );

  let mut c = CoordinateClient::<u32>::new(opts);

  // Peer also at dimensionality 1 (clamped), all-zero vector.
  let peer = Coordinate {
    vec: vec![0.0; 1],
    error: CoordinateOptions::new().vivaldi_error_max(),
    adjustment: 0.0,
    height: 0.0,
  };
  let mut rng = test_rng();
  // This call must return (not spin) within the test timeout.
  let result = c.update(&1u32, &peer, Duration::from_millis(10), &mut rng);
  assert!(
    result.is_ok(),
    "update with dimensionality clamped to 1 must not spin or error: {result:?}"
  );
}
