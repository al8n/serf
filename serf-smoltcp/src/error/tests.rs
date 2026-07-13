use super::*;
use crate::interface::{
  EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address, Medium, Route,
};
use core::net::{Ipv4Addr, SocketAddr};
use std::{format, vec, vec::Vec};

fn sample_cidr() -> IpCidr {
  IpCidr::new(IpAddress::v4(224, 0, 0, 1), 24)
}

fn sample_route() -> Route {
  Route::new_ipv4_gateway(Ipv4Address::new(10, 0, 0, 1))
}

fn sample_socket_addr() -> SocketAddr {
  SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 7946)
}

// A non-unicast (multicast) MAC: the low bit of the first octet set.
fn multicast_mac() -> HardwareAddress {
  HardwareAddress::Ethernet(EthernetAddress([0x01, 0, 0, 0, 0, 1]))
}

// Build one representative value of every `InitError` variant so the Display
// and Debug arms are all exercised.
fn all_variants() -> Vec<InitError> {
  vec![
    InitError::MediumMismatch(MediumMismatch {
      expected: Medium::Ethernet,
      actual: Medium::Ip,
    }),
    InitError::UnsupportedMedium,
    InitError::NonUnicastHardwareAddress(multicast_mac()),
    InitError::NonUnicastIpAddress(sample_cidr()),
    InitError::NonRoutableAdvertiseAddr(sample_socket_addr()),
    InitError::MissingIpAddress,
    InitError::TooManyIpAddresses,
    InitError::TooManyRoutes,
    InitError::ZeroPort,
    InitError::AdvertisePortMismatch,
    InitError::AdvertiseAddrNotLocal(sample_socket_addr()),
    InitError::NonUnicastRouteGateway(sample_route()),
    InitError::RouteFamilyMismatch(sample_route()),
    InitError::Entropy,
    InitError::Endpoint(EndpointInitError::AwarenessMultiplierZero),
    #[cfg(encryption)]
    InitError::Encryption(memberlist_proto::EncryptionError::AuthFailed),
    InitError::GossipMtuTooLarge(GossipMtuTooLarge {
      gossip_mtu: 70_000,
      ceiling: 65_467,
    }),
    InitError::UdpArenaTooLarge,
    InitError::TcpPoolTooSmall,
    InitError::ZeroTcpSocketBuffer,
    InitError::TcpRxBufferTooLarge,
    InitError::ZeroUdpPackets,
    InitError::ZeroCloseTimeout,
    InitError::InvalidSerfOptions(
      crate::SerfOptions::new()
        .with_max_user_event_size(crate::SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1)
        .validate()
        .expect_err("an over-ceiling max_user_event_size is invalid"),
    ),
  ]
}

#[test]
fn every_variant_displays_and_debugs_non_empty() {
  for err in all_variants() {
    assert!(
      !format!("{err}").is_empty(),
      "Display non-empty for {err:?}"
    );
    assert!(!format!("{err:?}").is_empty(), "Debug non-empty");
  }
}

#[test]
fn gossip_mtu_too_large_payload_display() {
  let payload = GossipMtuTooLarge {
    gossip_mtu: 70_000,
    ceiling: 65_467,
  };
  let shown = format!("{payload}");
  assert!(shown.contains("70000"), "{shown}");
  assert!(shown.contains("65467"), "{shown}");
  // Copy + PartialEq are derived.
  assert_eq!(payload, payload);
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn medium_mismatch_payload_display() {
  let payload = MediumMismatch {
    expected: Medium::Ethernet,
    actual: Medium::Ip,
  };
  assert!(!format!("{payload}").is_empty());
  assert_eq!(payload, payload);
  assert!(!format!("{payload:?}").is_empty());
}

#[test]
fn from_endpoint_init_error() {
  let err: InitError = EndpointInitError::AwarenessMultiplierZero.into();
  assert!(matches!(
    err,
    InitError::Endpoint(EndpointInitError::AwarenessMultiplierZero)
  ));
}

#[cfg(encryption)]
#[test]
fn from_encryption_error() {
  let err: InitError = memberlist_proto::EncryptionError::NoMatchingKey.into();
  assert!(matches!(err, InitError::Encryption(_)));
}

// `from_embedded` maps each embedded failure mode to its driver equivalent,
// and routes any future (non_exhaustive) variant to a generic endpoint error.
#[test]
fn from_embedded_maps_each_mode() {
  use serf_embedded::MemberlistInitError as E;

  assert!(matches!(
    InitError::from_memberlist(E::ZeroPort),
    InitError::ZeroPort
  ));
  assert!(matches!(
    InitError::from_memberlist(E::AdvertisePortMismatch),
    InitError::AdvertisePortMismatch
  ));
  assert!(matches!(
    InitError::from_memberlist(E::ZeroCloseTimeout),
    InitError::ZeroCloseTimeout
  ));
  assert!(matches!(
    InitError::from_memberlist(E::NonRoutableAdvertiseAddr(sample_socket_addr())),
    InitError::NonRoutableAdvertiseAddr(_)
  ));
  assert!(matches!(
    InitError::from_memberlist(E::Endpoint(EndpointInitError::AwarenessMultiplierZero)),
    InitError::Endpoint(_)
  ));
  #[cfg(encryption)]
  assert!(matches!(
    InitError::from_memberlist(E::Encryption(memberlist_proto::EncryptionError::AuthFailed)),
    InitError::Encryption(_)
  ));

  // The carried ceiling/value survive the GossipMtuTooLarge remap.
  let mapped = InitError::from_memberlist(E::GossipMtuTooLarge(serf_embedded::GossipMtuTooLarge {
    gossip_mtu: 99_999,
    ceiling: 65_467,
  }));
  match mapped {
    InitError::GossipMtuTooLarge(g) => {
      assert_eq!(g.gossip_mtu, 99_999);
      assert_eq!(g.ceiling, 65_467);
    }
    other => panic!("expected GossipMtuTooLarge, got {other:?}"),
  }
}

#[test]
fn join_error_predicates_and_display() {
  let e = JoinError::NoAddresses;
  assert!(e.is_no_addresses());
  assert!(!e.is_resolve());
  assert!(!e.is_control());
  assert!(!format!("{e}").is_empty());
}

// Under `std` the `Error::source` chains only for the wrapping variants.
#[cfg(feature = "std")]
#[test]
fn source_chains_only_for_wrapping_variants() {
  use std::error::Error as _;

  assert!(
    InitError::Endpoint(EndpointInitError::AwarenessMultiplierZero)
      .source()
      .is_some()
  );
  #[cfg(encryption)]
  assert!(
    InitError::Encryption(memberlist_proto::EncryptionError::AuthFailed)
      .source()
      .is_some()
  );
  // A leaf variant carries no source.
  assert!(InitError::ZeroPort.source().is_none());
  assert!(InitError::Entropy.source().is_none());
  assert!(
    InitError::GossipMtuTooLarge(GossipMtuTooLarge {
      gossip_mtu: 70_000,
      ceiling: 65_467,
    })
    .source()
    .is_none()
  );
}

/// A resolver error with a recognisable rendering, so the boxed `Resolve` arms can
/// be checked for actually carrying their cause into the message and source chain.
#[derive(Debug)]
struct ResolverFault;

impl core::fmt::Display for ResolverFault {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.write_str("the resolver gave up")
  }
}

impl core::error::Error for ResolverFault {}

/// The advertise-resolution arms name the step that failed and carry the resolver's
/// own error into the message, so a caller need not guess which lookup broke.
#[test]
fn advertise_resolution_failures_render_their_cause() {
  assert_eq!(
    format!("{}", InitError::Resolve(Box::new(ResolverFault))),
    "advertise address resolution failed: the resolver gave up"
  );
  assert_eq!(
    format!("{}", InitError::NoAddresses),
    "advertise address resolution returned no addresses"
  );
}

/// The boxed resolver error chains, so a caller that knows its concrete resolver can
/// downcast to it.
#[cfg(feature = "std")]
#[test]
fn a_resolver_failure_chains_as_the_source() {
  use std::error::Error as _;

  let err = InitError::Resolve(Box::new(ResolverFault));
  let source = err.source().expect("the boxed resolver error chains");
  assert_eq!(format!("{source}"), "the resolver gave up");
  assert!(
    source.downcast_ref::<ResolverFault>().is_some(),
    "the concrete resolver error survives boxing"
  );
  assert!(InitError::NoAddresses.source().is_none());
}

/// `from_embedded` maps BOTH halves of the engine's construction error: its serf
/// options half passes through as the driver's typed cause, and its memberlist half
/// is remapped through `from_memberlist`.
#[test]
fn from_embedded_maps_both_engine_halves() {
  let invalid = crate::SerfOptions::new()
    .with_max_user_event_size(crate::SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1)
    .validate()
    .expect_err("an over-ceiling max_user_event_size is invalid");
  assert!(matches!(
    InitError::from_embedded(serf_embedded::InitError::InvalidSerfOptions(invalid)),
    InitError::InvalidSerfOptions(_)
  ));

  assert!(matches!(
    InitError::from_embedded(serf_embedded::InitError::Memberlist(
      serf_embedded::MemberlistInitError::ZeroPort
    )),
    InitError::ZeroPort
  ));
}

/// The join failure modes each name the step that failed, and the wrapping ones
/// chain their cause.
#[test]
fn join_error_renders_and_chains_each_cause() {
  assert_eq!(
    format!("{}", JoinError::Resolve(Box::new(ResolverFault))),
    "seed address resolution failed: the resolver gave up"
  );
  assert_eq!(
    format!("{}", JoinError::NoAddresses),
    "no wire address resolved for any seed"
  );

  // The control arm names the rejection and carries the engine's own message.
  let rejected = serf_embedded::SerfError::BadLeaveState(crate::SerfState::Leaving);
  let rendered = format!("{}", JoinError::Control(rejected));
  assert!(rendered.starts_with("join was rejected: "), "{rendered}");
  assert!(
    rendered.contains(&format!(
      "{}",
      serf_embedded::SerfError::BadLeaveState(crate::SerfState::Leaving)
    )),
    "{rendered}"
  );
}

/// An engine rejection converts into the join error's control arm rather than being
/// flattened into a message.
#[test]
fn join_error_converts_from_an_engine_rejection() {
  let err: JoinError = serf_embedded::SerfError::LeaveClockExhausted.into();
  assert!(err.is_control());
  assert!(!err.is_resolve());
  assert!(!err.is_no_addresses());
  assert!(matches!(
    err,
    JoinError::Control(serf_embedded::SerfError::LeaveClockExhausted)
  ));
}

#[cfg(feature = "std")]
#[test]
fn join_error_source_chains_only_for_wrapping_variants() {
  use std::error::Error as _;

  let resolve = JoinError::Resolve(Box::new(ResolverFault));
  assert_eq!(
    format!("{}", resolve.source().expect("the resolver error chains")),
    "the resolver gave up"
  );

  let control = JoinError::Control(serf_embedded::SerfError::LeaveClockExhausted);
  assert!(control.source().is_some());

  assert!(
    JoinError::NoAddresses.source().is_none(),
    "a discovery failure carries no inner cause"
  );
}
