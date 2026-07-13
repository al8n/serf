use super::*;

#[test]
fn member_event_kind_as_str_round_trips() {
  assert_eq!(MemberEventKind::Join.as_str(), "join");
  assert_eq!(MemberEventKind::Leave.as_str(), "leave");
  assert_eq!(MemberEventKind::Failed.as_str(), "failed");
  assert_eq!(MemberEventKind::Update.as_str(), "update");
  assert_eq!(MemberEventKind::Reap.as_str(), "reap");
}

#[test]
fn member_event_kind_display_matches_as_str() {
  for kind in [
    MemberEventKind::Join,
    MemberEventKind::Leave,
    MemberEventKind::Failed,
    MemberEventKind::Update,
    MemberEventKind::Reap,
  ] {
    assert_eq!(kind.to_string(), kind.as_str());
  }
}

#[test]
fn event_is_variant_helpers() {
  use core::net::SocketAddr;

  let ev: Event<u32, SocketAddr> = Event::Shutdown;
  assert!(ev.is_shutdown());
  let ev: Event<u32, SocketAddr> = Event::LeftCluster;
  assert!(ev.is_left_cluster());
}
