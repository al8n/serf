use super::*;

/// A CIDR policy installed through the builder is the one the engine will admit
/// peers against; the default posture installs none (every address admitted).
#[cfg(feature = "cidr")]
#[test]
fn a_cidr_policy_is_installable() {
  use core::net::{IpAddr, Ipv4Addr};

  let mut policy = serf_embedded::CidrPolicy::block_all();
  policy.add(
    "10.0.0.0/8"
      .parse::<serf_embedded::IpNet>()
      .expect("a well-formed CIDR parses"),
  );

  let installed = Options::new()
    .with_cidr_policy(policy)
    .cidr_policy
    .expect("the policy is installed");
  assert!(
    installed.is_allowed(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))),
    "an address inside the allow-list is admitted"
  );
  assert!(
    installed.is_blocked(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
    "an address outside the allow-list is refused"
  );

  assert!(Options::new().cidr_policy.is_none());
}

#[test]
fn defaults_are_sane_and_overridable() {
  let c = Options::new();
  assert_eq!(c.port, 7946);
  assert!(c.tcp_socket_rx_bytes > 0);
  assert!(c.tcp_socket_tx_bytes > 0);
  assert!(!c.close_timeout.is_zero());
  assert!(
    c.socket_timeout > c.close_timeout,
    "socket timeout must exceed the close timeout"
  );

  let c = Options::new()
    .with_port(1234)
    .with_tcp_socket_rx_bytes(8192)
    .with_tcp_socket_tx_bytes(2048)
    .with_close_timeout(Duration::from_secs(3))
    .with_socket_timeout(Duration::from_secs(20));
  assert_eq!(c.port, 1234);
  assert_eq!(c.tcp_socket_rx_bytes, 8192);
  assert_eq!(c.tcp_socket_tx_bytes, 2048);
  assert_eq!(c.close_timeout, Duration::from_secs(3));
  assert_eq!(c.socket_timeout, Duration::from_secs(20));
}
