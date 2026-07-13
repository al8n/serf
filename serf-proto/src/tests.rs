//! Unit tests for the crate-root vocabulary types.

use super::LamportTime;

/// A Lamport time renders as its bare counter — the form every log line, error
/// message and operator-facing diagnostic in the crate interpolates.  A wrapper
/// like `LamportTime(7)` leaking into that output would make clock values
/// unreadable next to the plain integers the wire protocol reports.
#[test]
fn lamport_time_displays_as_its_bare_counter() {
  assert_eq!(LamportTime::new(7).to_string(), "7");
  assert_eq!(LamportTime::ZERO.to_string(), "0");
  assert_eq!(
    LamportTime::from(u64::MAX).to_string(),
    u64::MAX.to_string()
  );
}
