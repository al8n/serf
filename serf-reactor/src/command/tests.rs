use super::*;
use smol_str::SmolStr;

fn addr() -> std::net::SocketAddr {
  "127.0.0.1:5000".parse().unwrap()
}

fn unit_reply() -> Sender<Result<()>> {
  futures_channel::oneshot::channel::<Result<()>>().0
}

#[cfg(any(feature = "tcp", feature = "quic"))]
#[test]
fn join_cmd_carries_seeds_kind_and_reply() {
  use memberlist_proto::Instant;

  let (tx, _rx) = futures_channel::oneshot::channel::<JoinReply>();
  let cmd = JoinCmd {
    seeds: vec![addr()],
    kind: JoinKind::Dispatch,
    ignore_old: false,
    reply: tx,
  };
  assert_eq!(cmd.seeds.len(), 1);
  assert_eq!(cmd.seeds[0], addr());
  assert!(matches!(cmd.kind, JoinKind::Dispatch));
  assert!(!cmd.ignore_old);

  // The enum variant wraps the payload transparently; the await kind carries
  // its deadline, and the ignore_old flag rides along.
  let (tx2, _rx2) = futures_channel::oneshot::channel::<JoinReply>();
  let Command::<SmolStr, std::net::SocketAddr>::Join(c) = Command::Join(JoinCmd {
    seeds: vec![addr(), addr()],
    kind: JoinKind::WaitForCompletion(WaitForCompletionArgs {
      deadline: Instant::now(),
    }),
    ignore_old: true,
    reply: tx2,
  }) else {
    panic!("wrong Command variant");
  };
  assert_eq!(c.seeds.len(), 2);
  assert!(matches!(c.kind, JoinKind::WaitForCompletion(_)));
  assert!(
    c.ignore_old,
    "ignore_old must round-trip through the command"
  );
}

#[test]
fn leave_cmd_constructs() {
  let cmd = Command::<SmolStr, std::net::SocketAddr>::Leave(LeaveCmd {
    reply: unit_reply(),
  });
  assert!(matches!(cmd, Command::Leave(_)));
}

#[test]
fn shutdown_cmd_constructs() {
  let cmd = Command::<SmolStr, std::net::SocketAddr>::Shutdown(ShutdownCmd {
    reply: unit_reply(),
  });
  assert!(matches!(cmd, Command::Shutdown(_)));
}

#[cfg(any(feature = "tcp", feature = "quic"))]
mod transport_gated {
  use super::*;
  use bytes::Bytes;
  use memberlist_proto::Instant;
  use serf_proto::{endpoint::QueryParams, typed::Tags};

  #[test]
  fn force_leave_cmd_round_trips_fields() {
    let cmd = Command::<SmolStr, std::net::SocketAddr>::ForceLeave(ForceLeaveCmd {
      id: SmolStr::new("node-1"),
      prune: true,
      now: Instant::now(),
      reply: unit_reply(),
    });
    assert!(matches!(cmd, Command::ForceLeave(_)));
    if let Command::ForceLeave(c) = cmd {
      assert_eq!(c.id.as_str(), "node-1");
      assert!(c.prune);
    }
  }

  #[test]
  fn user_event_cmd_round_trips_fields() {
    let cmd = UserEventCmd::new(
      SmolStr::new("deploy"),
      Bytes::from_static(b"v1.2.3"),
      true,
      unit_reply(),
    );
    assert_eq!(cmd.name().as_str(), "deploy");
    assert_eq!(cmd.payload(), &Bytes::from_static(b"v1.2.3"));
    assert!(cmd.coalesce);

    let wrapped = Command::<SmolStr, std::net::SocketAddr>::UserEvent(UserEventCmd::new(
      SmolStr::new("x"),
      Bytes::from_static(b"y"),
      false,
      unit_reply(),
    ));
    assert!(matches!(wrapped, Command::UserEvent(_)));
  }

  #[test]
  fn query_cmd_round_trips_name_and_payload() {
    let (tx, _rx) = futures_channel::oneshot::channel::<Result<serf_proto::endpoint::QueryId>>();
    let cmd: QueryCmd<SmolStr> = QueryCmd::new(
      SmolStr::new("health"),
      Bytes::from_static(b"ping"),
      QueryParams::default(),
      Instant::now(),
      tx,
    );
    assert_eq!(cmd.name().as_str(), "health");
    assert_eq!(cmd.payload(), &Bytes::from_static(b"ping"));
  }

  #[test]
  fn set_tags_cmd_constructs() {
    let cmd = Command::<SmolStr, std::net::SocketAddr>::SetTags(SetTagsCmd {
      tags: Tags::default(),
      reply: unit_reply(),
    });
    assert!(matches!(cmd, Command::SetTags(_)));
  }
}
