//! Pure Vivaldi-based network coordinate engine.
//!
//! Ported faithfully from `legacy/serf-core/src/types/coordinate.rs`.
//!
//! This module is a *pure* numeric engine — no I/O, no async, no wall-clock
//! reads, and no global RNG draws.  Any randomness (the jitter applied when two
//! coordinates occupy the same point) is drawn from an injected [`Rng`].
//!
//! References used throughout:
//! - \[1\] Dabek, Frank, et al. "Vivaldi: A decentralized network coordinate
//!   system." ACM SIGCOMM 2004.
//! - \[2\] Ledlie, Jonathan, Paul Gardner, and Margo I. Seltzer. "Network
//!   Coordinates in the Wild." NSDI 2007.
//! - \[3\] Lee, Sanghwan, et al. "On suitability of Euclidean embedding for
//!   host-based network coordinate systems." IEEE/ACM Transactions on
//!   Networking, 2010.

use std::{collections::HashMap, time::Duration};

use memberlist_proto::Rng;
use rand::RngExt;

use crate::Coordinate;

/// Used to decide if two coordinates are on top of each other.
const ZERO_THRESHOLD: f64 = 1.0e-6;

/// Convert float seconds to nanoseconds.
const SECONDS_TO_NANOSECONDS: f64 = 1.0e9;

// ── CoordinateOptions ────────────────────────────────────────────────────────

/// Tuning parameters for the Vivaldi-based coordinate mapping algorithm.
///
/// The default values are suitable for basic algorithm testing but are not
/// tuned for any particular cluster topology.
///
/// See `CoordinateClient::new` for the concrete defaults.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
pub struct CoordinateOptions {
  /// Dimensionality of the coordinate space.
  ///
  /// Per [2], 8 dimensions plus a non-Euclidean height gives a good
  /// accuracy/complexity balance.
  dimensionality: usize,
  /// Upper bound on the error value.
  ///
  /// Serves as the initial error estimate when a node has not yet made any
  /// observations, and caps runaway error growth.
  vivaldi_error_max: f64,
  /// Controls the maximum impact an observation has on error confidence.
  ///
  /// See [1] for derivation.
  vivaldi_ce: f64,
  /// Controls the maximum impact an observation has on the coordinate itself.
  ///
  /// See [1] for derivation.
  vivaldi_cc: f64,
  /// Number of RTT samples retained per observation window for the adjustment
  /// factor described in [3].  Setting this to zero disables the feature.
  adjustment_window_size: usize,
  /// Minimum value for the height parameter.
  ///
  /// Always positive; introduces a small error so the value should be
  /// relatively small compared to typical coordinates.
  height_min: f64,
  /// Maximum RTT samples retained per peer for computing the median
  /// latency-filter value.
  ///
  /// The intent is to smooth over measurement blips. See [2].
  latency_filter_size: usize,
  /// Gravity coefficient.
  ///
  /// Determines how strongly coordinates drift back toward the origin to
  /// combat unbounded drift. See [2].
  gravity_rho: f64,
}

impl Default for CoordinateOptions {
  fn default() -> Self {
    Self::new()
  }
}

impl CoordinateOptions {
  /// Returns default `CoordinateOptions` tuned for basic algorithm testing.
  pub fn new() -> Self {
    Self {
      dimensionality: 8,
      vivaldi_error_max: 1.5,
      vivaldi_ce: 0.25,
      vivaldi_cc: 0.25,
      adjustment_window_size: 20,
      height_min: 10.0e-6,
      latency_filter_size: 3,
      gravity_rho: 150.0,
    }
  }

  /// Returns the dimensionality of the coordinate space.
  pub const fn dimensionality(&self) -> usize {
    self.dimensionality
  }

  /// Sets the dimensionality of the coordinate space.
  ///
  /// A value of 0 is clamped to 1: the `unit_vector_at` coincident-point
  /// retry loop iterates over the component vector; a zero-length vector
  /// yields no components to randomize, so `jmag` stays zero and the loop
  /// spins forever — a CPU hang.  1 dimension is the minimum coordinate system.
  /// Mirrors `with_latency_filter_size`'s `.max(1)` guard.
  pub fn with_dimensionality(mut self, v: usize) -> Self {
    self.dimensionality = v.max(1);
    self
  }

  /// Returns the maximum error value.
  pub const fn vivaldi_error_max(&self) -> f64 {
    self.vivaldi_error_max
  }

  /// Sets the maximum error value.
  pub fn with_vivaldi_error_max(mut self, v: f64) -> Self {
    self.vivaldi_error_max = v;
    self
  }

  /// Returns the `vivaldi_ce` confidence tuning factor.
  pub const fn vivaldi_ce(&self) -> f64 {
    self.vivaldi_ce
  }

  /// Sets the `vivaldi_ce` confidence tuning factor.
  pub fn with_vivaldi_ce(mut self, v: f64) -> Self {
    self.vivaldi_ce = v;
    self
  }

  /// Returns the `vivaldi_cc` position tuning factor.
  pub const fn vivaldi_cc(&self) -> f64 {
    self.vivaldi_cc
  }

  /// Sets the `vivaldi_cc` position tuning factor.
  pub fn with_vivaldi_cc(mut self, v: f64) -> Self {
    self.vivaldi_cc = v;
    self
  }

  /// Returns the adjustment window size.
  pub const fn adjustment_window_size(&self) -> usize {
    self.adjustment_window_size
  }

  /// Sets the adjustment window size.
  pub fn with_adjustment_window_size(mut self, v: usize) -> Self {
    self.adjustment_window_size = v;
    self
  }

  /// Returns the minimum height value.
  pub const fn height_min(&self) -> f64 {
    self.height_min
  }

  /// Sets the minimum height value.
  pub fn with_height_min(mut self, v: f64) -> Self {
    self.height_min = v;
    self
  }

  /// Returns the latency filter window size (samples per peer).
  pub const fn latency_filter_size(&self) -> usize {
    self.latency_filter_size
  }

  /// Sets the latency filter window size.
  ///
  /// A value of 0 is clamped to 1: `latency_filter` computes a median on the
  /// sliding window and indexes `tmp[tmp.len() / 2]`; a zero-length window
  /// would push a sample, evict it immediately (len > 0 → remove), then index
  /// an empty vec — a panic.  Mirrors `EventBuffer::new`'s `.max(1)` guard.
  pub fn with_latency_filter_size(mut self, v: usize) -> Self {
    self.latency_filter_size = v.max(1);
    self
  }

  /// Returns the gravity coefficient.
  pub const fn gravity_rho(&self) -> f64 {
    self.gravity_rho
  }

  /// Sets the gravity coefficient.
  pub fn with_gravity_rho(mut self, v: f64) -> Self {
    self.gravity_rho = v;
    self
  }
}

// ── CoordinateError ──────────────────────────────────────────────────────────

/// Errors returned by [`CoordinateClient`] operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
pub enum CoordinateError {
  /// The two coordinates have incompatible dimensionalities.
  #[error("coordinate dimensions are not compatible")]
  DimensionalityMismatch,
  /// A coordinate contains non-finite (NaN or infinite) values.
  #[error("coordinate contains invalid (non-finite) values")]
  InvalidCoordinate,
  /// The supplied RTT is outside the valid range [0, 10s].
  #[error("round-trip time {0:?} is not in the valid range (must be ≤ 10 seconds)")]
  InvalidRtt(Duration),
}

// ── CoordinateClientStats ────────────────────────────────────────────────────

/// Counters recorded by [`CoordinateClient`] during updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
pub struct CoordinateClientStats {
  /// Number of times the coordinate was reset to the origin because an
  /// update produced an invalid (non-finite) result.
  resets: usize,
}

impl CoordinateClientStats {
  /// Returns the number of coordinate resets.
  pub const fn resets(&self) -> usize {
    self.resets
  }
}

// ── Coordinate helpers ───────────────────────────────────────────────────────
//
// All maths operates on `Vec<f64>` to match the serf-proto `Coordinate::vec`
// field (the oracle uses `SmallVec<[f64; 8]>` which is equivalent at our dim).

/// Add `rhs` into `lhs` in place (lhs += rhs, element-wise).
#[inline]
fn vec_add_in_place(lhs: &mut [f64], rhs: &[f64]) {
  for (x, y) in lhs.iter_mut().zip(rhs) {
    *x += y;
  }
}

/// Element-wise difference: returns lhs - rhs as an owned Vec.
#[inline]
fn vec_diff(lhs: &[f64], rhs: &[f64]) -> Vec<f64> {
  lhs.iter().zip(rhs).map(|(x, y)| x - y).collect()
}

/// Scale `vec` by `factor` in place, returning the slice.
#[inline]
fn vec_scale_in_place(vec: &mut [f64], factor: f64) -> &mut [f64] {
  for x in vec.iter_mut() {
    *x *= factor;
  }
  vec
}

/// L2 magnitude of an iterator of `f64` values.
#[inline]
fn vec_magnitude(iter: impl Iterator<Item = f64>) -> f64 {
  iter.fold(0.0_f64, |acc, x| acc + x * x).sqrt()
}

/// Returns a unit vector pointing *from* `b` *toward* `a`, plus the distance
/// between `a` and `b`.
///
/// When the two points coincide (distance below [`ZERO_THRESHOLD`]), a random
/// unit direction is drawn from `rng` so that the update step does not stall.
/// The distance is reported as `0.0` in that case.
///
/// FIX (oracle used `rand::rng()` / global thread-local): jitter now draws
/// from the caller's injected `rng`, keeping the engine fully deterministic
/// under test.
fn unit_vector_at<R>(a: &[f64], b: &[f64], rng: &mut R) -> (Vec<f64>, f64)
where
  R: Rng,
{
  let mut delta = vec_diff(a, b);
  let mag = vec_magnitude(delta.iter().copied());

  if mag > ZERO_THRESHOLD {
    vec_scale_in_place(&mut delta, mag.recip());
    return (delta, mag);
  }

  // Coincident points: draw a random direction so the update does not stall.
  loop {
    for x in delta.iter_mut() {
      // Draw a uniform float in [0, 1) via the injected rng.
      *x = rng.random_range::<f64, _>(0.0..1.0) - 0.5;
    }
    let jmag = vec_magnitude(delta.iter().copied());
    if jmag > ZERO_THRESHOLD {
      vec_scale_in_place(&mut delta, jmag.recip());
      return (delta, 0.0);
    }
    // Extraordinarily unlikely to loop more than once; retry rather than
    // silently emitting a zero vector.
  }
}

// ── Coordinate methods (extension helpers) ───────────────────────────────────

/// Raw distance between two coordinates (no adjustment, sum of Euclidean +
/// height terms).
fn raw_distance(a: &Coordinate, b: &Coordinate) -> f64 {
  vec_magnitude(a.vec.iter().zip(&b.vec).map(|(x, y)| x - y)) + a.height + b.height
}

/// Distance including both nodes' adjustment terms, floored to the raw value
/// if the sum goes negative.
fn adjusted_distance_secs(a: &Coordinate, b: &Coordinate) -> f64 {
  let raw = raw_distance(a, b);
  let adj = raw + a.adjustment + b.adjustment;
  if adj > 0.0 { adj } else { raw }
}

/// Apply force `f` (from `other`'s direction) to `coord` in place.
fn apply_force_in_place<R>(
  coord: &mut Coordinate,
  height_min: f64,
  force: f64,
  other: &Coordinate,
  rng: &mut R,
) where
  R: Rng,
{
  let (mut unit, mag) = unit_vector_at(&coord.vec, &other.vec, rng);
  vec_scale_in_place(&mut unit, force);
  vec_add_in_place(&mut coord.vec, &unit);

  if mag > ZERO_THRESHOLD {
    coord.height = (coord.height + other.height) * force / mag + coord.height;
    coord.height = coord.height.max(height_min);
  }
}

// ── CoordinateClient ─────────────────────────────────────────────────────────

/// Manages the estimated Vivaldi network coordinate for a local node.
///
/// Adjusts the coordinate as the node observes round-trip times and remote
/// coordinates from peers.  This is a *pure, single-threaded* engine:
///
/// - No I/O, no async, no threads.
/// - No wall-clock reads — the RTT is always passed in by the caller.
/// - No global RNG — random jitter (for coincident-point handling) is drawn
///   from the `rng` parameter of [`update`].
/// - `stats.resets` is a plain `usize` (no atomics — single-threaded machine).
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
pub struct CoordinateClient<I> {
  /// Current coordinate estimate for this node.
  coord: Coordinate,
  /// Coordinate at the origin (all zeros) used for gravity updates.
  origin: Coordinate,
  /// Algorithm tuning parameters.
  opts: CoordinateOptions,
  /// Circular index into `adjustment_samples`.
  adjustment_index: usize,
  /// Rolling window of `(rtt - raw_distance)` samples used to compute the
  /// adjustment factor described in [3].
  adjustment_samples: Vec<f64>,
  /// Per-peer sliding windows of raw RTT samples for the latency filter.
  latency_filter_samples: HashMap<I, Vec<f64>>,
  /// Lifetime counters.
  stats: CoordinateClientStats,
}

impl<I> CoordinateClient<I>
where
  I: Eq + core::hash::Hash + Clone,
{
  /// Creates a new client with the given tuning options.
  pub fn new(opts: CoordinateOptions) -> Self {
    let mut coord_vec = Vec::with_capacity(opts.dimensionality);
    coord_vec.resize(opts.dimensionality, 0.0_f64);

    let coord = Coordinate {
      vec: coord_vec.clone(),
      error: opts.vivaldi_error_max,
      adjustment: 0.0,
      height: opts.height_min,
    };
    let origin = Coordinate {
      vec: coord_vec,
      error: opts.vivaldi_error_max,
      adjustment: 0.0,
      height: opts.height_min,
    };

    let adj_size = if opts.adjustment_window_size > 0 {
      opts.adjustment_window_size
    } else {
      1 // avoid a zero-length vec; update_adjustment is a no-op when size==0
    };
    let adjustment_samples = vec![0.0_f64; adj_size];

    Self {
      coord,
      origin,
      opts,
      adjustment_index: 0,
      adjustment_samples,
      latency_filter_samples: HashMap::new(),
      stats: CoordinateClientStats::default(),
    }
  }

  /// Returns a copy of the current coordinate estimate.
  pub fn get_coordinate(&self) -> Coordinate {
    self.coord.clone()
  }

  /// Forces the coordinate to a known state, for testing or snapshot restore.
  ///
  /// Returns an error if `coord` is incompatible with the current
  /// dimensionality or contains non-finite values.
  pub fn set_coordinate(&mut self, coord: Coordinate) -> Result<(), CoordinateError> {
    Self::check_coordinate(&self.coord, &coord)?;
    self.coord = coord;
    Ok(())
  }

  /// Returns the estimated RTT from this node to `other`.
  pub fn distance_to(&self, other: &Coordinate) -> Duration {
    let secs = adjusted_distance_secs(&self.coord, other);
    Duration::from_nanos((secs * SECONDS_TO_NANOSECONDS) as u64)
  }

  /// Returns a copy of the lifetime counters.
  pub fn stats(&self) -> CoordinateClientStats {
    self.stats
  }

  /// Removes any latency-filter history for `node` (call on member reap so
  /// stale samples do not affect a rejoining peer).
  pub fn forget_node(&mut self, node: &I) {
    self.latency_filter_samples.remove(node);
  }

  /// Observes a measured round-trip time (`rtt`) to a peer and updates the
  /// local coordinate estimate.
  ///
  /// `rng` supplies the random jitter needed if this node and `other` happen
  /// to occupy the same point in coordinate space (exceedingly rare in
  /// practice).  Passing the same seeded RNG produces deterministic results.
  ///
  /// Returns the updated coordinate on success.
  ///
  /// # Errors
  ///
  /// - [`CoordinateError::DimensionalityMismatch`] — `other` has a different
  ///   dimensionality than this client.
  /// - [`CoordinateError::InvalidCoordinate`] — `other` contains a non-finite
  ///   value.
  /// - [`CoordinateError::InvalidRtt`] — `rtt` exceeds 10 seconds (a hard cap
  ///   that guards against stale or wildly incorrect measurements).
  pub fn update<R>(
    &mut self,
    node: &I,
    other: &Coordinate,
    rtt: Duration,
    rng: &mut R,
  ) -> Result<Coordinate, CoordinateError>
  where
    R: Rng,
  {
    Self::check_coordinate(&self.coord, other)?;

    const MAX_RTT: Duration = Duration::from_secs(10);
    if rtt > MAX_RTT {
      return Err(CoordinateError::InvalidRtt(rtt));
    }

    // Zero RTTs are valid (coarse-grained monotonic clocks can produce them);
    // the algorithm handles them gracefully via the ZERO_THRESHOLD floor.

    let rtt_seconds = self.latency_filter(node, rtt.as_secs_f64());
    self.update_vivaldi(other, rtt_seconds, rng);
    self.update_adjustment(other, rtt_seconds);
    self.update_gravity(rng);

    if !self.is_valid() {
      // The update produced a degenerate coordinate; reset to the origin and
      // count the event so callers can observe it.
      self.stats.resets += 1;
      let mut vec = Vec::with_capacity(self.opts.dimensionality);
      vec.resize(self.opts.dimensionality, 0.0_f64);
      self.coord = Coordinate {
        vec,
        error: self.opts.vivaldi_error_max,
        adjustment: 0.0,
        height: self.opts.height_min,
      };
    }

    Ok(self.coord.clone())
  }

  // ── private helpers ────────────────────────────────────────────────────────

  /// Returns `true` if the current coordinate contains only finite values.
  fn is_valid(&self) -> bool {
    self.coord.vec.iter().all(|f| f.is_finite())
      && self.coord.error.is_finite()
      && self.coord.adjustment.is_finite()
      && self.coord.height.is_finite()
  }

  /// Returns an error if `coord` is dimensionally incompatible with `reference`
  /// or contains non-finite values.
  fn check_coordinate(reference: &Coordinate, coord: &Coordinate) -> Result<(), CoordinateError> {
    if reference.vec.len() != coord.vec.len() {
      return Err(CoordinateError::DimensionalityMismatch);
    }
    let valid = coord.vec.iter().all(|f| f.is_finite())
      && coord.error.is_finite()
      && coord.adjustment.is_finite()
      && coord.height.is_finite();
    if !valid {
      return Err(CoordinateError::InvalidCoordinate);
    }
    Ok(())
  }

  /// Returns the median of the per-peer RTT sample window, inserting `rtt`
  /// first.  Older samples are evicted when the window is full (sliding
  /// window of size `latency_filter_size`).
  fn latency_filter(&mut self, node: &I, rtt_seconds: f64) -> f64 {
    let size = self.opts.latency_filter_size;
    let samples = self
      .latency_filter_samples
      .entry(node.clone())
      .or_insert_with(|| Vec::with_capacity(size));

    samples.push(rtt_seconds);
    if samples.len() > size {
      samples.remove(0);
    }

    let mut tmp = samples.clone();
    tmp.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    tmp[tmp.len() / 2]
  }

  /// Applies the Vivaldi spring force to move the local coordinate toward
  /// `other`, weighted by confidence.  See [1].
  fn update_vivaldi<R>(&mut self, other: &Coordinate, rtt_seconds: f64, rng: &mut R)
  where
    R: Rng,
  {
    let dist = adjusted_distance_secs(&self.coord, other);
    let rtt_seconds = rtt_seconds.max(ZERO_THRESHOLD);

    let wrongness = ((dist - rtt_seconds) / rtt_seconds).abs();

    let total_error = (self.coord.error + other.error).max(ZERO_THRESHOLD);
    let weight = self.coord.error / total_error;

    self.coord.error = ((self.opts.vivaldi_ce * weight * wrongness)
      + self.coord.error * (1.0 - self.opts.vivaldi_ce * weight))
      .min(self.opts.vivaldi_error_max);

    let force = self.opts.vivaldi_cc * weight * (rtt_seconds - dist);
    apply_force_in_place(&mut self.coord, self.opts.height_min, force, other, rng);
  }

  /// Updates the rolling adjustment factor window ([3]).  No-op when
  /// `adjustment_window_size == 0`.
  fn update_adjustment(&mut self, other: &Coordinate, rtt_seconds: f64) {
    if self.opts.adjustment_window_size == 0 {
      return;
    }
    let dist = raw_distance(&self.coord, other);
    self.adjustment_samples[self.adjustment_index] = rtt_seconds - dist;
    self.adjustment_index = (self.adjustment_index + 1) % self.opts.adjustment_window_size;

    self.coord.adjustment =
      self.adjustment_samples.iter().sum::<f64>() / (2.0 * self.opts.adjustment_window_size as f64);
  }

  /// Applies a small gravity force pulling the coordinate back toward the
  /// origin to combat long-term drift ([2]).
  fn update_gravity<R>(&mut self, rng: &mut R)
  where
    R: Rng,
  {
    let dist = adjusted_distance_secs(&self.origin, &self.coord);
    let force = -f64::powi(dist / self.opts.gravity_rho, 2);
    let origin = self.origin.clone();
    apply_force_in_place(&mut self.coord, self.opts.height_min, force, &origin, rng);
  }
}

#[cfg(test)]
mod tests;
