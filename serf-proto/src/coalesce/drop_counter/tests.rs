use super::DropCounter;

#[test]
fn u64_incr_counts_from_zero() {
  let mut c: u64 = 0;
  assert_eq!(c.get(), 0);
  c.incr_saturating();
  c.incr_saturating();
  c.incr_saturating();
  assert_eq!(c.get(), 3);
}

#[test]
fn u64_incr_saturates_at_max() {
  let mut c: u64 = u64::MAX - 1;
  c.incr_saturating();
  assert_eq!(c.get(), u64::MAX);
  // A further increment must not wrap back to zero.
  c.incr_saturating();
  assert_eq!(c.get(), u64::MAX);
}
