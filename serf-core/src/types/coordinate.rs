use core::time::Duration;

use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, RepeatedDecoder, WireType,
  utils::{merge, skip},
};
use rand::Rng;
use smallvec::SmallVec;

use std::{
  collections::HashMap,
  sync::atomic::{AtomicUsize, Ordering},
};

use memberlist_core::CheapClone;
use parking_lot::RwLock;

/// Used to convert float seconds to nanoseconds.
const SECONDS_TO_NANOSECONDS: f64 = 1.0e9;
/// Used to decide if two coordinates are on top of each
/// other.
const ZERO_THRESHOLD: f64 = 1.0e-6;

/// The default dimensionality of the coordinate system.
const DEFAULT_DIMENSIONALITY: usize = 8;

/// The default adjustment window size.
const DEFAULT_ADJUSTMENT_WINDOW_SIZE: usize = 20;

const DEFAULT_LATENCY_FILTER_SAMPLES_SIZE: usize = 8;

/// CoordinateOptions is used to set the parameters of the Vivaldi-based coordinate mapping
/// algorithm.
///
/// The following references are called out at various points in the documentation
/// here:
///
/// [1] Dabek, Frank, et al. "Vivaldi: A decentralized network coordinate system."
///     ACM SIGCOMM Computer Communication Review. Vol. 34. No. 4. ACM, 2004.
/// [2] Ledlie, Jonathan, Paul Gardner, and Margo I. Seltzer. "Network Coordinates
///     in the Wild." NSDI. Vol. 7. 2007.
/// [3] Lee, Sanghwan, et al. "On suitability of Euclidean embedding for
///     host-based network coordinate systems." Networking, IEEE/ACM Transactions
///     on 18.1 (2010): 27-40.
#[viewit::viewit(
  vis_all = "pub(crate)",
  getters(style = "ref", vis_all = "pub"),
  setters(prefix = "with", vis_all = "pub")
)]
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CoordinateOptions {
  /// The dimensionality of the coordinate system. As discussed in [2], more
  /// dimensions improves the accuracy of the estimates up to a point. Per [2]
  /// we chose 8 dimensions plus a non-Euclidean height.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the dimensionality of the coordinate system.")
    ),
    setter(attrs(doc = "Sets the dimensionality of the coordinate system."))
  )]
  dimensionality: usize,

  /// The default error value when a node hasn't yet made
  /// any observations. It also serves as an upper limit on the error value in
  /// case observations cause the error value to increase without bound.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the default error value when a node hasn't yet made any observations.")
    ),
    setter(attrs(
      doc = "Sets the default error value when a node hasn't yet made any observations."
    ))
  )]
  vivaldi_error_max: f64,

  /// A tuning factor that controls the maximum impact an
  /// observation can have on a node's confidence. See [1] for more details.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the maximum impact an observation can have on a node's confidence.")
    ),
    setter(attrs(
      doc = "Sets the maximum impact an observation can have on a node's confidence."
    ))
  )]
  vivaldi_ce: f64,

  /// A tuning factor that controls the maximum impact an
  /// observation can have on a node's coordinate. See [1] for more details.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the maximum impact an observation can have on a node's coordinate.")
    ),
    setter(attrs(
      doc = "Sets the maximum impact an observation can have on a node's coordinate."
    ))
  )]
  vivaldi_cc: f64,

  /// A tuning factor that determines how many samples
  /// we retain to calculate the adjustment factor as discussed in [3]. Setting
  /// this to zero disables this feature.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns how many samples we retain to calculate the adjustment factor.")
    ),
    setter(attrs(doc = "Sets how many samples we retain to calculate the adjustment factor."))
  )]
  adjustment_window_size: usize,

  /// The minimum value of the height parameter. Since this
  /// always must be positive, it will introduce a small amount error, so
  /// the chosen value should be relatively small compared to "normal"
  /// coordinates.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the minimum value of the height parameter.")
    ),
    setter(attrs(doc = "Sets the minimum value of the height parameter."))
  )]
  height_min: f64,

  /// The maximum number of samples that are retained
  /// per node, in order to compute a median. The intent is to ride out blips
  /// but still keep the delay low, since our time to probe any given node is
  /// pretty infrequent. See [2] for more details.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns the maximum number of samples that are retained per node.")
    ),
    setter(attrs(doc = "Sets the maximum number of samples that are retained per node."))
  )]
  latency_filter_size: usize,

  /// A tuning factor that sets how much gravity has an effect
  /// to try to re-center coordinates. See [2] for more details.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(doc = "Returns how much gravity has an effect to try to re-center coordinates.")
    ),
    setter(attrs(doc = "Sets how much gravity has an effect to try to re-center coordinates."))
  )]
  gravity_rho: f64,

  #[cfg(feature = "metrics")]
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        cfg(feature = "metrics"),
        doc = "The metric labels used to identify the metrics for this coordinate client."
      )
    ),
    setter(attrs(
      cfg(feature = "metrics"),
      doc = "Sets the metric labels used to identify the metrics for this coordinate client."
    ))
  )]
  metric_labels: std::sync::Arc<memberlist_core::proto::MetricLabels>,
}

impl Default for CoordinateOptions {
  fn default() -> Self {
    Self::new()
  }
}

impl CoordinateOptions {
  /// Returns a `CoordinateOptions` that has some default values suitable for
  /// basic testing of the algorithm, but not tuned to any particular type of cluster.
  #[inline]
  pub fn new() -> Self {
    Self {
      dimensionality: DEFAULT_DIMENSIONALITY,
      vivaldi_error_max: 1.5,
      vivaldi_ce: 0.25,
      vivaldi_cc: 0.25,
      adjustment_window_size: 20,
      height_min: 10.0e-6,
      latency_filter_size: 3,
      gravity_rho: 150.0,
      #[cfg(feature = "metrics")]
      metric_labels: std::sync::Arc::new(memberlist_core::proto::MetricLabels::default()),
    }
  }
}

/// A specialized structure for holding network coordinates for the
/// Vivaldi-based coordinate mapping algorithm. All of the fields should be public
/// to enable this to be serialized. All values in here are in units of seconds.
#[viewit::viewit(getters(style = "move"), setters(prefix = "with"))]
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Coordinate {
  /// The Euclidean portion of the coordinate. This is used along
  /// with the other fields to provide an overall distance estimate. The
  /// units here are seconds.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the Euclidean portion of the coordinate.")
    ),
    setter(attrs(doc = "Sets the Euclidean portion of the coordinate."))
  )]
  portion: SmallVec<[f64; DEFAULT_DIMENSIONALITY]>,
  /// Reflects the confidence in the given coordinate and is updated
  /// dynamically by the Vivaldi Client. This is dimensionless.
  #[viewit(
    getter(const, attrs(doc = "Returns the confidence in the given coordinate.")),
    setter(attrs(doc = "Sets the confidence in the given coordinate."))
  )]
  error: f64,
  /// A distance offset computed based on a calculation over
  /// observations from all other nodes over a fixed window and is updated
  /// dynamically by the Vivaldi Client. The units here are seconds.
  #[viewit(
    getter(const, attrs(doc = "Returns the distance offset.")),
    setter(attrs(doc = "Sets the distance offset."))
  )]
  adjustment: f64,
  /// A distance offset that accounts for non-Euclidean effects
  /// which model the access links from nodes to the core Internet. The access
  /// links are usually set by bandwidth and congestion, and the core links
  /// usually follow distance based on geography.
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns the distance offset that accounts for non-Euclidean effects.")
    ),
    setter(attrs(doc = "Sets the distance offset that accounts for non-Euclidean effects."))
  )]
  height: f64,
}

impl Default for Coordinate {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl Coordinate {
  /// Creates a new coordinate at the origin, using the default options
  /// to supply key initial values.
  #[inline]
  pub fn new() -> Self {
    Self::with_options(CoordinateOptions::new())
  }

  /// Creates a new coordinate at the origin, using the given options
  /// to supply key initial values.
  #[inline]
  pub fn with_options(opts: CoordinateOptions) -> Self {
    let mut vec = SmallVec::with_capacity(opts.dimensionality);
    vec.resize(opts.dimensionality, 0.0);
    Self {
      portion: vec,
      error: opts.vivaldi_error_max,
      adjustment: 0.0,
      height: opts.height_min,
    }
  }

  /// Returns true if the coordinate is valid.
  #[inline]
  pub fn is_valid(&self) -> bool {
    self.portion.iter().all(|&f| f.is_finite())
      && self.error.is_finite()
      && self.adjustment.is_finite()
      && self.height.is_finite()
  }

  /// Returns true if the dimensions of the coordinates are compatible.
  #[inline]
  pub fn is_compatible_with(&self, other: &Self) -> bool {
    self.portion.len() == other.portion.len()
  }

  /// Returns the result of applying the force from the direction of the
  /// other coordinate.
  pub fn apply_force(&self, height_min: f64, force: f64, other: &Self) -> Self {
    assert!(
      self.is_compatible_with(other),
      "coordinate dimensionality does not match"
    );

    let mut ret = self.clone();
    let (mut unit, mag) = unit_vector_at(&self.portion, &other.portion);
    add_in_place(&mut ret.portion, mul_in_place(&mut unit, force));
    if mag > ZERO_THRESHOLD {
      ret.height = (ret.height + other.height) * force / mag + ret.height;
      ret.height = ret.height.max(height_min);
    }
    ret
  }

  /// Apply the result of applying the force from the direction of the
  /// other coordinate to self.
  pub fn apply_force_in_place(&mut self, height_min: f64, force: f64, other: &Self) {
    assert!(
      self.is_compatible_with(other),
      "coordinate dimensionality does not match"
    );
    let (mut unit, mag) = unit_vector_at(&self.portion, &other.portion);
    add_in_place(&mut self.portion, mul_in_place(&mut unit, force));

    if mag > ZERO_THRESHOLD {
      self.height = (self.height + other.height) * force / mag + self.height;
      self.height = self.height.max(height_min);
    }
  }

  /// Returns the distance between this coordinate and the other
  /// coordinate, including adjustments.
  pub fn distance_to(&self, other: &Self) -> Duration {
    assert!(
      self.is_compatible_with(other),
      "coordinate dimensionality does not match"
    );

    let dist = self.raw_distance_to(other);
    let adjusted_dist = dist + self.adjustment + other.adjustment;
    let dist = if adjusted_dist > 0.0 {
      adjusted_dist
    } else {
      dist
    };
    Duration::from_nanos((dist * SECONDS_TO_NANOSECONDS) as u64)
  }

  #[inline]
  pub(crate) fn raw_distance_to(&self, other: &Self) -> f64 {
    magnitude_in_place(diff_in_place(&self.portion, &other.portion)) + self.height + other.height
  }
}

#[inline]
fn add_in_place(vec1: &mut [f64], vec2: &[f64]) {
  for (x, y) in vec1.iter_mut().zip(vec2.iter()) {
    *x += y;
  }
}

/// Returns difference between the vec1 and vec2. This assumes the
/// dimensions have already been checked to be compatible.
#[inline]
fn diff(vec1: &[f64], vec2: &[f64]) -> SmallVec<[f64; DEFAULT_DIMENSIONALITY]> {
  vec1.iter().zip(vec2).map(|(x, y)| x - y).collect()
}

/// computes difference between the vec1 and vec2 in place. This assumes the
/// dimensions have already been checked to be compatible.
#[inline]
fn diff_in_place<'a>(vec1: &'a [f64], vec2: &'a [f64]) -> impl Iterator<Item = f64> + 'a {
  vec1.iter().zip(vec2).map(|(x, y)| x - y)
}

/// multiplied by a scalar factor in place.
#[inline]
fn mul_in_place(vec: &mut [f64], factor: f64) -> &mut [f64] {
  for x in vec.iter_mut() {
    *x *= factor;
  }
  vec
}

/// Computes the magnitude of the vec.
#[inline]
fn magnitude_in_place(vec: impl Iterator<Item = f64>) -> f64 {
  vec.fold(0.0, |acc, x| acc + x * x).sqrt()
}

/// Returns a unit vector pointing at vec1 from vec2. If the two
/// positions are the same then a random unit vector is returned. We also return
/// the distance between the points for use in the later height calculation.
fn unit_vector_at(vec1: &[f64], vec2: &[f64]) -> (SmallVec<[f64; DEFAULT_DIMENSIONALITY]>, f64) {
  let mut ret = diff(vec1, vec2);

  let mag = magnitude_in_place(ret.iter().copied());
  if mag > ZERO_THRESHOLD {
    mul_in_place(&mut ret, mag.recip());
    return (ret, mag);
  }

  for x in ret.iter_mut() {
    *x = rand_f64() - 0.5;
  }

  let mag = magnitude_in_place(ret.iter().copied());
  if mag > ZERO_THRESHOLD {
    mul_in_place(&mut ret, mag.recip());
    return (ret, 0.0);
  }

  // And finally just give up and make a unit vector along the first
  // dimension. This should be exceedingly rare.
  ret.fill(0.0);
  ret[0] = 1.0;
  (ret, 0.0)
}

fn rand_f64() -> f64 {
  let mut rng = rand::rng();
  loop {
    let f = (rng.random_range::<u64, _>(0..(1u64 << 63u64)) as f64) / ((1u64 << 63u64) as f64);
    if f == 1.0 {
      continue;
    }
    return f;
  }
}

const PORTION_TAG: u8 = 1;
const ERROR_TAG: u8 = 2;
const ADJUSTMENT_TAG: u8 = 3;
const HEIGHT_TAG: u8 = 4;
const PORTION_BYTE: u8 = merge(WireType::Fixed64, PORTION_TAG);
const ERROR_BYTE: u8 = merge(WireType::Fixed64, ERROR_TAG);
const ADJUSTMENT_BYTE: u8 = merge(WireType::Fixed64, ADJUSTMENT_TAG);
const HEIGHT_BYTE: u8 = merge(WireType::Fixed64, HEIGHT_TAG);

/// The reference type to [`Coordinate`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CoordinateRef<'a> {
  portion: RepeatedDecoder<'a>,
  error: f64,
  adjustment: f64,
  height: f64,
}

impl<'a> DataRef<'a, Coordinate> for CoordinateRef<'a> {
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let buf_len = buf.len();

    let mut portion_offsets = None;
    let mut num_portions = 0;
    let mut error = None;
    let mut adjustment = None;
    let mut height = None;

    while offset < buf_len {
      match buf[offset] {
        PORTION_BYTE => {
          let readed = skip("Coordinate", &buf[offset..])?;
          if let Some((ref mut fnso, ref mut lnso)) = portion_offsets {
            if *fnso > offset {
              *fnso = offset;
            }

            if *lnso < offset + readed {
              *lnso = offset + readed;
            }
          } else {
            portion_offsets = Some((offset, offset + readed));
          }
          num_portions += 1;
          offset += readed;
        }
        ERROR_BYTE => {
          if error.is_some() {
            return Err(DecodeError::duplicate_field(
              "Coordinate",
              "error",
              ERROR_TAG,
            ));
          }
          offset += 1;

          let (len, val) = <f64 as Data>::decode(&buf[offset..])?;
          offset += len;
          error = Some(val);
        }
        ADJUSTMENT_BYTE => {
          if adjustment.is_some() {
            return Err(DecodeError::duplicate_field(
              "Coordinate",
              "adjustment",
              ADJUSTMENT_TAG,
            ));
          }
          offset += 1;

          let (len, val) = <f64 as Data>::decode(&buf[offset..])?;
          offset += len;
          adjustment = Some(val);
        }
        HEIGHT_BYTE => {
          if height.is_some() {
            return Err(DecodeError::duplicate_field(
              "Coordinate",
              "height",
              HEIGHT_TAG,
            ));
          }
          offset += 1;

          let (len, val) = <f64 as Data>::decode(&buf[offset..])?;
          offset += len;
          height = Some(val);
        }
        _ => offset += skip("Coordinate", &buf[offset..])?,
      }
    }

    Ok((
      offset,
      Self {
        portion: if let Some((start, end)) = portion_offsets {
          RepeatedDecoder::new(PORTION_TAG, WireType::Fixed64, buf)
            .with_nums(num_portions)
            .with_offsets(start, end)
        } else {
          RepeatedDecoder::new(PORTION_TAG, WireType::Fixed64, buf)
        },
        error: error.ok_or_else(|| DecodeError::missing_field("Coordinate", "error"))?,
        adjustment: adjustment
          .ok_or_else(|| DecodeError::missing_field("Coordinate", "adjustment"))?,
        height: height.ok_or_else(|| DecodeError::missing_field("Coordinate", "height"))?,
      },
    ))
  }
}

impl Data for Coordinate {
  type Ref<'a> = CoordinateRef<'a>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    val
      .portion
      .iter::<f64>()
      .collect::<Result<SmallVec<[f64; DEFAULT_DIMENSIONALITY]>, _>>()
      .map(|portion| Self {
        portion,
        error: val.error,
        adjustment: val.adjustment,
        height: val.height,
      })
  }

  fn encoded_len(&self) -> usize {
    self
      .portion
      .iter()
      .fold(0, |acc, x| acc + 1 + x.encoded_len_with_length_delimited())
      + 1
      + self.error.encoded_len()
      + 1
      + self.adjustment.encoded_len()
      + 1
      + self.height.encoded_len()
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    macro_rules! bail {
      ($this:ident($offset:expr, $len:ident)) => {
        if $offset >= $len {
          return Err(EncodeError::insufficient_buffer(self.encoded_len(), $len));
        }
      };
    }

    let mut offset = 0;
    let buf_len = buf.len();
    for x in self.portion.iter() {
      bail!(self(offset, buf_len));
      buf[offset] = PORTION_BYTE;
      offset += 1;
      offset += x
        .encode(&mut buf[offset..])
        .map_err(|e| e.update(self.encoded_len(), buf_len))?;
    }

    bail!(self(offset, buf_len));
    buf[offset] = ERROR_BYTE;
    offset += 1;
    offset += self.error.encode(&mut buf[offset..])?;

    bail!(self(offset, buf_len));
    buf[offset] = ADJUSTMENT_BYTE;
    offset += 1;
    offset += self.adjustment.encode(&mut buf[offset..])?;

    bail!(self(offset, buf_len));
    buf[offset] = HEIGHT_BYTE;
    offset += 1;
    offset += self.height.encode(&mut buf[offset..])?;

    Ok(offset)
  }
}

/// Error type for the [`Coordinate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoordinateError {
  /// Returned when the dimensions of the coordinates are not compatible.
  #[error("dimensions aren't compatible")]
  DimensionalityMismatch,
  /// Returned when the coordinate is invalid.
  #[error("invalid coordinate")]
  InvalidCoordinate,
  /// Returned when the round trip time is not in a valid range.
  #[error("round trip time not in valid range, duration {0:?} is not a value less than 10s")]
  InvalidRTT(Duration),
}

/// Used to record events that occur when updating coordinates.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CoordinateClientStats {
  /// Incremented any time we reset our local coordinate because
  /// our calculations have resulted in an invalid state.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(
        doc = "Returns the number of times we reset our local coordinate because our calculations have resulted in an invalid state."
      )
    ),
    setter(attrs(
      doc = "Sets the number of times we reset our local coordinate because our calculations have resulted in an invalid state."
    ))
  )]
  resets: usize,
}

impl Default for CoordinateClientStats {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl CoordinateClientStats {
  #[inline]
  const fn new() -> Self {
    Self { resets: 0 }
  }
}

struct CoordinateClientInner<I> {
  /// The current estimate of the client's network coordinate.
  coord: Coordinate,

  /// Origin is a coordinate sitting at the origin.
  origin: Coordinate,

  /// Contains the tuning parameters that govern the performance of
  /// the algorithm.
  opts: CoordinateOptions,

  /// The current index into the adjustmentSamples slice.
  adjustment_index: usize,

  /// Used to store samples for the adjustment calculation.
  adjustment_samples: SmallVec<[f64; DEFAULT_ADJUSTMENT_WINDOW_SIZE]>,

  /// Used to store the last several RTT samples,
  /// keyed by node name. We will use the config's LatencyFilterSamples
  /// value to determine how many samples we keep, per node.
  latency_filter_samples: HashMap<I, SmallVec<[f64; DEFAULT_LATENCY_FILTER_SAMPLES_SIZE]>>,
}

impl<I> CoordinateClientInner<I>
where
  I: CheapClone + Eq + core::hash::Hash,
{
  /// Applies a small amount of gravity to pull coordinates towards
  /// the center of the coordinate system to combat drift. This assumes that the
  /// mutex is locked already.
  #[inline]
  fn update_gravity(&mut self) {
    let dist = self.origin.distance_to(&self.coord).as_secs();
    let force = -1.0 * f64::powf((dist as f64) / self.opts.gravity_rho, 2.0);
    self
      .coord
      .apply_force_in_place(self.opts.height_min, force, &self.origin);
  }

  #[inline]
  fn latency_filter(&mut self, node: &I, rtt_seconds: f64) -> f64 {
    let samples = self
      .latency_filter_samples
      .entry(node.cheap_clone())
      .or_insert_with(|| SmallVec::with_capacity(self.opts.latency_filter_size));

    // Add the new sample and trim the list, if needed.
    samples.push(rtt_seconds);
    if samples.len() > self.opts.latency_filter_size {
      samples.remove(0);
    }
    // Sort a copy of the samples and return the median.
    let mut tmp = SmallVec::<[f64; DEFAULT_LATENCY_FILTER_SAMPLES_SIZE]>::from_slice(samples);
    tmp.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    tmp[tmp.len() / 2]
  }

  /// Updates the Vivaldi portion of the client's coordinate. This
  /// assumes that the mutex has been locked already.
  fn update_vivaldi(&mut self, other: &Coordinate, mut rtt_seconds: f64) {
    const ZERO_THRESHOLD: f64 = 1.0e-6;

    let dist = self.coord.distance_to(other).as_secs_f64();
    rtt_seconds = rtt_seconds.max(ZERO_THRESHOLD);

    let wrongness = ((dist - rtt_seconds) / rtt_seconds).abs();

    let total_error = (self.coord.error + other.error).max(ZERO_THRESHOLD);

    let weight = self.coord.error / total_error;
    self.coord.error = ((self.opts.vivaldi_ce * weight * wrongness)
      + (self.coord.error * (1.0 - self.opts.vivaldi_ce * weight)))
      .min(self.opts.vivaldi_error_max);

    let force = self.opts.vivaldi_cc * weight * (rtt_seconds - dist);
    self
      .coord
      .apply_force_in_place(self.opts.height_min, force, other);
  }

  /// Updates the adjustment portion of the client's coordinate, if
  /// the feature is enabled. This assumes that the mutex has been locked already.
  fn update_adjustment(&mut self, other: &Coordinate, rtt_seconds: f64) {
    if self.opts.adjustment_window_size == 0 {
      return;
    }
    // Note that the existing adjustment factors don't figure in to this
    // calculation so we use the raw distance here.
    let dist = self.coord.raw_distance_to(other);
    self.adjustment_samples[self.adjustment_index] = rtt_seconds - dist;
    self.adjustment_index = (self.adjustment_index + 1) % self.opts.adjustment_window_size;

    self.coord.adjustment =
      self.adjustment_samples.iter().sum::<f64>() / (2.0 * self.opts.adjustment_window_size as f64);
  }
}

/// Manages the estimated network coordinate for a given node, and adjusts
/// it as the node observes round trip times and estimated coordinates from other
/// nodes. The core algorithm is based on Vivaldi, see the documentation for Config
/// for more details.
///
/// `CoordinateClient` is thread-safe.
// TODO: are there any better ways to avoid using a RwLock?
pub struct CoordinateClient<I> {
  inner: RwLock<CoordinateClientInner<I>>,
  /// Used to record events that occur when updating coordinates.
  stats: AtomicUsize,
}

impl<I> Default for CoordinateClient<I> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl<I> CoordinateClient<I> {
  /// Creates a new client.
  #[inline]
  pub fn new() -> Self {
    Self {
      inner: RwLock::new(CoordinateClientInner {
        coord: Coordinate::new(),
        origin: Coordinate::new(),
        opts: CoordinateOptions::new(),
        adjustment_index: 0,
        adjustment_samples: SmallVec::from_slice(&[0.0; DEFAULT_ADJUSTMENT_WINDOW_SIZE]),
        latency_filter_samples: HashMap::new(),
      }),
      stats: AtomicUsize::new(0),
    }
  }

  /// Creates a new client with given options.
  #[inline]
  pub fn with_options(opts: CoordinateOptions) -> Self {
    let mut samples = SmallVec::with_capacity(opts.adjustment_window_size);
    samples.resize(opts.adjustment_window_size, 0.0);
    Self {
      inner: RwLock::new(CoordinateClientInner {
        coord: Coordinate::with_options(opts.clone()),
        origin: Coordinate::with_options(opts.clone()),
        opts,
        adjustment_index: 0,
        adjustment_samples: samples,
        latency_filter_samples: HashMap::new(),
      }),
      stats: AtomicUsize::new(0),
    }
  }

  /// Returns a copy of the coordinate for this client.
  #[inline]
  pub fn get_coordinate(&self) -> Coordinate {
    self.inner.read().coord.clone()
  }

  /// Forces the client's coordinate to a known state.
  #[inline]
  pub fn set_coordinate(&self, coord: Coordinate) -> Result<(), CoordinateError> {
    let mut l = self.inner.write();
    Self::check_coordinate(&l.coord, &coord).map(|_| l.coord = coord)
  }

  /// Returns a copy of stats for the client.
  #[inline]
  pub fn stats(&self) -> CoordinateClientStats {
    CoordinateClientStats {
      resets: self.stats.load(Ordering::Relaxed),
    }
  }

  /// Returns the estimated RTT from the client's coordinate to other, the
  /// coordinate for another node.
  #[inline]
  pub fn distance_to(&self, coord: &Coordinate) -> Duration {
    self.inner.read().coord.distance_to(coord)
  }

  /// Returns an error if the coordinate isn't compatible with
  /// this client, or if the coordinate itself isn't valid. This assumes the mutex
  /// has been locked already.
  #[inline]
  fn check_coordinate(this: &Coordinate, coord: &Coordinate) -> Result<(), CoordinateError> {
    if !this.is_compatible_with(coord) {
      return Err(CoordinateError::DimensionalityMismatch);
    }

    if !coord.is_valid() {
      return Err(CoordinateError::InvalidCoordinate);
    }

    Ok(())
  }
}

impl<I> CoordinateClient<I>
where
  I: CheapClone + Eq + core::hash::Hash,
{
  /// Removes any client state for the given node.
  #[inline]
  pub fn forget_node(&self, node: &I) {
    self.inner.write().latency_filter_samples.remove(node);
  }

  /// Update takes other, a coordinate for another node, and rtt, a round trip
  /// time observation for a ping to that node, and updates the estimated position of
  /// the client's coordinate. Returns the updated coordinate.
  pub fn update(
    &self,
    node: &I,
    other: &Coordinate,
    rtt: Duration,
  ) -> Result<Coordinate, CoordinateError> {
    let mut l = self.inner.write();
    Self::check_coordinate(&l.coord, other)?;

    // The code down below can handle zero RTTs, which we have seen in
    // https://github.com/hashicorp/consul/issues/3789, presumably in
    // environments with coarse-grained monotonic clocks (we are still
    // trying to pin this down). In any event, this is ok from a code PoV
    // so we don't need to alert operators with spammy messages. We did
    // add a counter so this is still observable, though.
    const MAX_RTT: Duration = Duration::from_secs(10);

    if rtt > MAX_RTT {
      return Err(CoordinateError::InvalidRTT(rtt));
    }

    #[cfg(feature = "metrics")]
    if rtt.is_zero() {
      metrics::counter!("serf.coordinate.zero-rtt", l.opts.metric_labels.iter()).increment(1);
    }

    let rtt_seconds = l.latency_filter(node, rtt.as_secs_f64());
    l.update_vivaldi(other, rtt_seconds);
    l.update_adjustment(other, rtt_seconds);
    l.update_gravity();

    if !l.coord.is_valid() {
      self.stats.fetch_add(1, Ordering::Acquire);
      l.coord = Coordinate::with_options(l.opts.clone());
    }

    Ok(l.coord.clone())
  }
}

#[cfg(test)]
mod tests {
  use smol_str::SmolStr;

  use super::*;

  fn verify_equal_floats(f1: f64, f2: f64) {
    if (f1 - f2).abs() > ZERO_THRESHOLD {
      panic!("Equal assertion fail, {:9.6} != {:9.6}", f1, f2);
    }
  }

  fn verify_equal_vectors(vec1: &[f64], vec2: &[f64]) {
    if vec1.len() != vec2.len() {
      panic!("Vector length mismatch, {} != {}", vec1.len(), vec2.len());
    }

    for (v1, v2) in vec1.iter().zip(vec2.iter()) {
      verify_equal_floats(*v1, *v2);
    }
  }

  #[test]
  fn test_client_update() {
    let cfg = CoordinateOptions::default().with_dimensionality(3);

    let client = CoordinateClient::with_options(cfg.clone());

    let c = client.get_coordinate();
    verify_equal_vectors(&c.portion, [0.0, 0.0, 0.0].as_slice());

    // Place a node right above the client and observe an RTT longer than the
    // client expects, given its distance.
    let mut other = Coordinate::with_options(cfg.clone());
    other.portion[2] = 0.001;

    let rtt = Duration::from_nanos((2.0 * other.portion[2] * 1.0e9) as u64);
    let mut c = client.update(&SmolStr::from("other"), &other, rtt).unwrap();

    // The client should have scooted down to get away from it.
    assert!(c.portion[2] < 0.0);

    // Set the coordinate to a known state.
    c.portion[2] = 99.0;
    client.set_coordinate(c.clone()).unwrap();
    let c = client.get_coordinate();
    verify_equal_floats(c.portion[2], 99.0);
  }

  #[test]
  fn test_client_invalid_in_ping_values() {
    let cfg = CoordinateOptions::default().with_dimensionality(3);

    let client = CoordinateClient::with_options(cfg.clone());

    // Place another node
    let mut other = Coordinate::with_options(cfg);
    other.portion[2] = 0.001;
    let dist = client.distance_to(&other);

    // Update with a series of invalid ping periods, should return an error and estimated rtt remains unchanged
    let pings = [9223372036854775807f64, -35f64, 11f64];
    for p in pings {
      client
        .update(
          &SmolStr::from("node"),
          &other,
          Duration::from_nanos((p as i64).wrapping_mul(SECONDS_TO_NANOSECONDS as i64) as u64),
        )
        .unwrap_err();

      let dist_new = client.distance_to(&other);
      assert_eq!(dist_new, dist);
    }
  }

  #[test]
  fn test_client_distance_to() {
    let cfg = CoordinateOptions::default()
      .with_dimensionality(3)
      .with_height_min(0f64);

    let client = CoordinateClient::<SmolStr>::with_options(cfg.clone());

    // Fiddle a raw coordinate to put it a specific number of seconds away.
    let mut other = Coordinate::with_options(cfg);
    other.portion[2] = 12.345;
    let expected = Duration::from_nanos((other.portion[2] * SECONDS_TO_NANOSECONDS) as u64);
    let dist = client.distance_to(&other);
    assert_eq!(dist, expected);
  }

  #[test]
  fn test_client_latency_filter() {
    let cfg = CoordinateOptions::default().with_latency_filter_size(3);

    let client = CoordinateClient::with_options(cfg);

    // Make sure we get the median, and that things age properly.
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.201),
      0.201,
    );
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.200),
      0.201,
    );
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.207),
      0.201,
    );

    // This glitch will get median-ed out and never seen by Vivaldi.
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 1.9),
      0.207,
    );
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.203),
      0.207,
    );
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.199),
      0.203,
    );
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.211),
      0.203,
    );

    // Make sure different nodes are not coupled.
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("bob"), 0.310),
      0.310,
    );

    // Make sure we don't leak coordinates for nodes that leave.
    client.forget_node(&SmolStr::from("alice"));
    verify_equal_floats(
      client
        .inner
        .write()
        .latency_filter(&SmolStr::from("alice"), 0.888),
      0.888,
    );
  }

  #[test]
  fn test_client_nan_defense() {
    let cfg = CoordinateOptions::default().with_dimensionality(3);

    let client = CoordinateClient::with_options(cfg.clone());

    // Block a bad coordinate from coming in.
    let mut other = Coordinate::with_options(cfg.clone());
    other.portion[0] = f64::NAN;
    assert!(!other.is_valid());

    let rtt = Duration::from_millis(250);
    let c = client
      .update(&SmolStr::from("node"), &other, rtt)
      .unwrap_err();
    assert_eq!(c, CoordinateError::InvalidCoordinate);
    let c = client.get_coordinate();
    assert!(c.is_valid());

    // Block setting an incompatible coordinate directly.
    other.portion.resize(other.portion.len() * 2, 0.0);
    let e = client.set_coordinate(other).unwrap_err();
    assert_eq!(e, CoordinateError::DimensionalityMismatch);
    let c = client.get_coordinate();
    assert!(c.is_valid());

    // Poison the internal state and make sure we reset on an update.
    client.inner.write().coord.portion[0] = f64::NAN;
    let other = Coordinate::with_options(cfg);
    let c = client.update(&SmolStr::from("node"), &other, rtt).unwrap();
    assert!(c.is_valid());
    assert_eq!(client.stats().resets, 1);
  }

  #[test]
  fn test_coordinate_new() {
    let opts = CoordinateOptions::default();
    let c = Coordinate::with_options(opts.clone());
    assert_eq!(opts.dimensionality, c.portion.len());
  }

  #[test]
  fn test_coordinate_is_valid() {
    let c = Coordinate::new();
    let mut fields = vec![];
    for i in 0..c.portion.len() {
      fields.push(c.portion[i]);
    }
    fields.push(c.error);
    fields.push(c.adjustment);
    fields.push(c.height);

    for field in fields.iter_mut() {
      assert!(c.is_valid());
      *field = f64::NAN;
    }
  }

  #[test]
  fn test_coordinate_is_compatible_with() {
    let cfg = CoordinateOptions::default().with_dimensionality(3);

    let c1 = Coordinate::with_options(cfg.clone());
    let c2 = Coordinate::with_options(cfg.clone());
    let cfg = cfg.with_dimensionality(2);
    let alien = Coordinate::with_options(cfg);

    assert!(c1.is_compatible_with(&c2));
    assert!(!c1.is_compatible_with(&alien));
    assert!(c2.is_compatible_with(&c1));
    assert!(!c2.is_compatible_with(&alien));
  }

  #[test]
  #[should_panic(expected = "coordinate dimensionality does not match")]
  fn test_coordinate_apply_force() {
    let cfg = CoordinateOptions::default()
      .with_dimensionality(3)
      .with_height_min(0f64);

    let origin = Coordinate::with_options(cfg.clone());

    // This proves that we normalize, get the direction right, and apply the
    // force multiplier correctly.
    let mut above = Coordinate::with_options(cfg.clone());
    above.portion[0] = 0.0;
    above.portion[1] = 0.0;
    above.portion[2] = 2.9;
    let c = origin.apply_force(cfg.height_min, 5.3, &above);

    verify_equal_vectors(&c.portion, [0.0, 0.0, -5.3].as_slice());

    // Scoot a point not starting at the origin to make sure there's nothing
    // special there.
    let mut right = Coordinate::with_options(cfg.clone());
    right.portion[0] = 3.4;
    right.portion[1] = 0.0;
    right.portion[2] = -5.3;
    let c = c.apply_force(cfg.height_min, 2.0, &right);
    verify_equal_vectors(&c.portion, [-2.0, 0.0, -5.3].as_slice());

    // If the points are right on top of each other, then we should end up
    // in a random direction, one unit away. This makes sure the unit vector
    // build up doesn't divide by zero.
    let c = origin.apply_force(cfg.height_min, 1.0, &origin);
    verify_equal_floats(origin.distance_to(&c).as_secs_f64(), 1.0);

    // Enable a minimum height and make sure that gets factored in properly.
    let cfg = cfg.with_height_min(10.0e-6);
    let origin = Coordinate::with_options(cfg.clone());
    let c = origin.apply_force(cfg.height_min, 5.3, &above);
    verify_equal_vectors(&c.portion, [0.0, 0.0, -5.3].as_slice());
    verify_equal_floats(c.height, cfg.height_min + 5.3 * cfg.height_min / 2.9);

    // Make sure the height minimum is enforced.
    let c = origin.apply_force(cfg.height_min, -5.3, &above);
    verify_equal_vectors(&c.portion, [0.0, 0.0, 5.3].as_slice());
    verify_equal_floats(c.height, cfg.height_min);

    // Shenanigans should get called if the dimensions don't match.
    let mut bad = c.clone();
    bad.portion = SmallVec::from_slice(&vec![0.0; bad.portion.len() + 1]);
    c.apply_force(cfg.height_min, 1.0, &bad);
  }

  #[test]
  fn test_coordinate_add() {
    let mut vec1 = [1.0, -3.0, 3.0];
    let vec2 = [-4.0, 5.0, 6.0];
    add_in_place(&mut vec1, &vec2);
    verify_equal_vectors(&vec1, [-3.0, 2.0, 9.0].as_slice());

    let zero = [0.0; 3];
    let mut vec1 = [1.0, -3.0, 3.0];
    add_in_place(&mut vec1, &zero);
    verify_equal_vectors(&[1.0, -3.0, 3.0], vec1.as_slice());
  }

  #[test]
  fn test_coordinate_diff() {
    let vec1 = [1.0, -3.0, 3.0];
    let vec2 = [-4.0, 5.0, 6.0];
    verify_equal_vectors(diff(&vec1, &vec2).as_slice(), [5.0, -8.0, -3.0].as_slice());

    let zero = [0.0; 3];
    verify_equal_vectors(diff(&vec1, &zero).as_slice(), vec1.as_slice());
  }

  #[test]
  fn test_coordinate_diff_in_place() {
    let vec1 = [1.0, -3.0, 3.0];
    let vec2 = [-4.0, 5.0, 6.0];
    verify_equal_vectors(
      &diff_in_place(&vec1, &vec2).collect::<Vec<_>>(),
      [5.0, -8.0, -3.0].as_slice(),
    );

    let zero = [0.0; 3];
    verify_equal_vectors(
      &diff_in_place(&vec1, &zero).collect::<Vec<_>>(),
      vec1.as_slice(),
    );
  }

  #[test]
  fn test_coordinate_magnitude() {
    let zero = [0.0; 3];
    verify_equal_floats(magnitude_in_place(zero.into_iter()), 0.0);

    let vec = [1.0, -2.0, 3.0];
    verify_equal_floats(magnitude_in_place(vec.into_iter()), 3.7416573867739413);
  }

  #[test]
  fn test_coordinate_unit_vector_at() {
    let vec1 = [1.0, 2.0, 3.0];
    let vec2 = [0.5, 0.6, 0.7];
    let (u, mag) = unit_vector_at(&vec1, &vec2);
    verify_equal_vectors(
      &u,
      [0.18257418583505536, 0.511207720338155, 0.8398412548412546].as_slice(),
    );
    verify_equal_floats(magnitude_in_place(u.iter().copied()), 1.0);
    let vec1 = [1.0, 2.0, 3.0];
    verify_equal_floats(mag, magnitude_in_place(diff(&vec1, &vec2).into_iter()));

    // If we give positions that are equal we should get a random unit vector
    // returned to us, rather than a divide by zero.
    let vec1 = [1.0, 2.0, 3.0];
    let (u, mag) = unit_vector_at(&vec1, &vec1);
    verify_equal_floats(mag, 0.0);
    verify_equal_floats(magnitude_in_place(u.iter().copied()), 1.0);
  }
}
