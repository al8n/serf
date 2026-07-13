use super::*;

/// The close timeout is overridable, and a CIDR policy installed through the
/// builder is the one the engine will admit peers against.
#[test]
fn close_timeout_and_cidr_policy_are_installable() {
  let c = Options::new().with_close_timeout(Duration::from_secs(42));
  assert_eq!(c.close_timeout, Duration::from_secs(42));

  #[cfg(feature = "cidr")]
  {
    use core::net::{IpAddr, Ipv4Addr};

    let mut policy = serf_embedded::CidrPolicy::block_all();
    policy.add(
      "10.0.0.0/8"
        .parse::<serf_embedded::IpNet>()
        .expect("a well-formed CIDR parses"),
    );

    let c = Options::new().with_cidr_policy(policy);
    let installed = c.cidr_policy.expect("the policy is installed");
    assert!(
      installed.is_allowed(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))),
      "an address inside the allow-list is admitted"
    );
    assert!(
      installed.is_blocked(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
      "an address outside the allow-list is refused"
    );

    // The default posture installs no policy at all (every address admitted).
    assert!(Options::new().cidr_policy.is_none());
  }
}

#[test]
fn defaults_are_sane_and_overridable() {
  let c = Options::new();
  assert!(c.tcp_pool_size >= 1);
  assert!(c.udp_rx_payload_bytes > 0);
  let c = Options::new().with_tcp_pool_size(8).with_port(1234);
  assert_eq!(c.tcp_pool_size, 8);
  assert_eq!(c.port, 1234);
}
