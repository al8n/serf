//! Unit tests for the QUIC driver's recv-buffer sizing — the pure decision that
//! keeps neither the gossip plane nor the raw-QUIC plane truncated on the shared
//! UDP socket. The end-to-end pump behaviour (join / converge / user-event / query
//! / leave, encrypted convergence, mismatched-key enforcement) is covered by the
//! real-node suite in `tests/quic.rs`.

use super::{ENCRYPTED_WRAPPER_OVERHEAD, GOSSIP_RECV_BUF_MAX, recv_buf_len_for};

/// The recv buffer follows the LARGER of the two planes sharing the one socket. A
/// caller can set a quinn `max_udp_payload_size` above the gossip MTU — quinn's own
/// default 1472 already exceeds the 1400 default `gossip_mtu` — so the buffer must
/// be sized from the QUIC plane, not silently left at the gossip size that would
/// truncate a full-size QUIC packet.
#[test]
fn recv_buf_sizes_to_the_larger_plane() {
  // QUIC plane far above the gossip plane (jumbo `max_udp_payload_size`): the buffer
  // follows the QUIC plane regardless of the small AEAD overhead.
  assert_eq!(recv_buf_len_for(1400, 9000), 9000);

  // quinn's default max UDP payload (1472) exceeds the default gossip MTU (1400), so
  // the buffer is sized to 1472, not 1400 — a full-size QUIC packet is not
  // truncated. Holds with or without an AEAD backend since
  // 1472 > 1400 + ENCRYPTED_WRAPPER_OVERHEAD.
  assert_eq!(recv_buf_len_for(1400, 1472), 1472);
}

/// When the gossip plane is the larger of the two, the QUIC ceiling never shrinks it
/// below the AEAD-inflated gossip requirement.
#[test]
fn recv_buf_keeps_the_gossip_requirement() {
  // Large configured gossip MTU, default-ish QUIC payload: the gossip plane wins and
  // keeps its encrypted-wrapper headroom.
  assert_eq!(
    recv_buf_len_for(16_000, 1472),
    16_000 + ENCRYPTED_WRAPPER_OVERHEAD
  );

  // The gossip plane is capped at the IPv4 UDP maximum; a small QUIC payload cannot
  // shrink it below that cap.
  assert_eq!(recv_buf_len_for(70_000, 1472), GOSSIP_RECV_BUF_MAX);
}

/// quinn permits a `max_udp_payload_size` up to 65527, just above the gossip plane's
/// 65507 cap; the QUIC plane is NOT clamped to that cap, so such a packet is
/// buffered in full.
#[test]
fn recv_buf_does_not_clamp_quic_below_quinn_max() {
  assert_eq!(recv_buf_len_for(70_000, 65_527), 65_527);
}
