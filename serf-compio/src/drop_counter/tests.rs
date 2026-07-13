use super::*;

use serf_proto::DropCounter;

#[test]
fn incr_saturates_at_max() {
  let cell = Rc::new(Cell::new(u64::MAX - 1));
  let mut w = CompioDropCounter(cell);
  w.incr_saturating();
  assert_eq!(w.get(), u64::MAX);
  // A further increment must saturate, not wrap back to zero.
  w.incr_saturating();
  assert_eq!(w.get(), u64::MAX);
}

#[test]
fn reader_observes_writer_over_the_shared_cell() {
  let (mut w, r) = drop_channel();
  assert_eq!(r.get(), 0);
  w.incr_saturating();
  w.incr_saturating();
  assert_eq!(
    r.get(),
    2,
    "the read half observes the write half's increments over one cell"
  );
}
