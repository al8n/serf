use super::*;

#[test]
fn invalid_gossip_mtu_accessors_and_display() {
  let payload = InvalidGossipMtu::new(70_000, 65_467);
  assert_eq!(payload.configured(), 70_000);
  assert_eq!(payload.ceiling(), 65_467);
  let shown = format!("{payload}");
  assert!(!shown.is_empty());
  assert!(shown.contains("70000"));
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn gossip_mtu_too_small_accessors_and_display() {
  let payload = GossipMtuTooSmall::new(64, 512);
  assert_eq!(payload.configured(), 64);
  assert_eq!(payload.minimum(), 512);
  let shown = format!("{payload}");
  assert!(!shown.is_empty());
  assert!(shown.contains("512"));
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn invalid_advertise_addr_accessors_and_display() {
  let addr: std::net::SocketAddr = "0.0.0.0:5000".parse().unwrap();
  let payload = InvalidAdvertiseAddr::new(addr, "wildcard bind".to_string());
  assert_eq!(payload.addr(), addr);
  assert_eq!(payload.reason(), "wildcard bind");
  let shown = format!("{payload}");
  assert!(!shown.is_empty());
  assert!(shown.contains("wildcard bind"));
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn invalid_option_accessors_and_display() {
  let payload = InvalidOption::new("idle_wake_interval", "must be nonzero".to_string());
  assert_eq!(payload.option(), "idle_wake_interval");
  assert_eq!(payload.reason(), "must be nonzero");
  let shown = format!("{payload}");
  assert!(shown.contains("idle_wake_interval"));
  assert!(shown.contains("must be nonzero"));
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn every_variant_displays_and_debugs() {
  let variants: &[SerfError] = &[
    SerfError::Io(io::Error::other("disk")),
    SerfError::Entropy(io::Error::other("entropy")),
    SerfError::Resolve(io::Error::other("dns")),
    SerfError::LeaveTimeout,
    SerfError::Shutdown,
    SerfError::NotRunning,
    SerfError::JoinAllFailed(JoinFailed::new(3, 0)),
    SerfError::InvalidGossipMtu(InvalidGossipMtu::new(70_000, 65_467)),
    SerfError::GossipMtuTooSmall(GossipMtuTooSmall::new(64, 512)),
    SerfError::InvalidAdvertiseAddr(InvalidAdvertiseAddr::new(
      "0.0.0.0:5000".parse().unwrap(),
      "wildcard".to_string(),
    )),
    SerfError::InvalidOption(InvalidOption::new(
      "idle_wake_interval",
      "nonzero".to_string(),
    )),
    SerfError::CommandSend,
    SerfError::ReplyClosed,
  ];

  for err in variants {
    assert!(
      !format!("{err}").is_empty(),
      "Display non-empty for {err:?}"
    );
    assert!(!format!("{err:?}").is_empty(), "Debug non-empty");
  }
}

#[test]
fn from_io_conversion() {
  let err: SerfError = io::Error::other("boom").into();
  assert!(matches!(err, SerfError::Io(_)));
  assert_eq!(err.to_string(), io::Error::other("boom").to_string());
}
