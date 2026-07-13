//! Unit tests for the serf wire vocabulary types.

use super::*;

/// A default `Coordinate` is the EMPTY coordinate — no dimensions at all — not a
/// zeroed point in some assumed dimensionality.  The distinction matters: an
/// empty vector carries no dimensionality claim, so a peer's first observed
/// coordinate establishes it rather than silently mismatching an assumed one.
#[cfg(feature = "coordinates")]
#[test]
fn a_default_coordinate_is_dimensionless_and_at_rest() {
  let c = Coordinate::default();
  assert!(
    c.vec.is_empty(),
    "the default coordinate claims no dimensionality"
  );
  assert_eq!(c.error, 0.0);
  assert_eq!(c.adjustment, 0.0);
  assert_eq!(c.height, 0.0);
}

/// `Tags::with_capacity` reserves without inserting: the map is still empty, and
/// `is_empty` agrees with `len` as entries arrive.
#[test]
fn tags_with_capacity_starts_empty_and_tracks_its_length() {
  let mut tags = Tags::with_capacity(8);
  assert!(tags.is_empty(), "a reserved-but-unfilled map is empty");
  assert_eq!(tags.len(), 0);

  tags.0.insert("role".into(), "web".into());
  assert!(!tags.is_empty(), "an inserted tag makes the map non-empty");
  assert_eq!(tags.len(), 1);
}
