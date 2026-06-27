use super::{GossipMtuTooSmall, InvalidOption};

#[test]
fn gossip_mtu_too_small_fields_and_display() {
  let mtu = GossipMtuTooSmall::new(64, 128);
  assert_eq!(mtu.configured(), 64);
  assert_eq!(mtu.minimum(), 128);
  let s = format!("{mtu}");
  assert!(s.contains("64"), "missing configured value: {s}");
  assert!(s.contains("128"), "missing minimum value: {s}");
  let _: &dyn std::error::Error = &mtu;
}

#[test]
fn invalid_option_fields_and_display() {
  let opt = InvalidOption::new("gossip_interval", "must be nonzero".to_string());
  assert_eq!(opt.option(), "gossip_interval");
  assert_eq!(opt.reason(), "must be nonzero");
  let s = format!("{opt}");
  assert!(s.contains("gossip_interval"), "missing option name: {s}");
  assert!(s.contains("must be nonzero"), "missing reason: {s}");
  let _: &dyn std::error::Error = &opt;
}
