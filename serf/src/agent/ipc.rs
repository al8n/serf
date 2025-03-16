use std::{collections::HashMap, time::Duration};

use memberlist::{bytes::Bytes, net::Node};
#[cfg(feature = "encryption")]
use serf_core::types::SecretKey;
use serf_core::{
  coordinate::Coordinate,
  types::{LamportTime, MaybeResolvedAddress, Member, MemberStatus, Tags},
};

use smol_str::SmolStr;

enum IpcCommand {
  Handshake,
  Event,
  ForceLeave,
  Join,
  Leave,
}

#[derive(Debug, Clone, Copy)]
enum IpcVersion {
  V1 = 1,
}

#[derive(Debug, Clone, Copy)]
struct HandshakeRequest {
  version: IpcVersion,
}

struct AuthRequest {
  auth_key: SmolStr,
}

struct CoordinateRequest<I> {
  node: I,
}

struct CoordinateResponse {
  coord: Coordinate,
  ok: bool,
}

struct EventRequest {
  name: SmolStr,
  payload: Bytes,
  coalesce: bool,
}

struct ForceLeaveRequest<I> {
  node: I,
  prune: bool,
}

struct JoinRequest<I, A, R> {
  existing: Vec<Node<I, MaybeResolvedAddress<A, R>>>,
  replay: bool,
}

struct JoinResponse {
  num: u32,
}

struct MembersFilteredRequest<I> {
  tags: Tags,
  status: MemberStatus,
  name: I,
}

struct MembersResponse<I, A> {
  members: Vec<Member<I, A>>,
}

#[cfg(feature = "encryption")]
struct KeyRequest {
  key: SecretKey,
}

#[cfg(feature = "encryption")]
struct KeyResponse {
  messages: HashMap<SmolStr, SmolStr>,
  keys: HashMap<SecretKey, u32>,
  num_nodes: u32,
  num_err: u32,
  num_resp: u32,
}

struct MonitorRequest {
  log_filter: u8,
}

struct StreamRequest {
  ty: u8,
}

struct StopRequest {
  stop: u64,
}

struct TagsRequest {
  tags: Tags,
  delete_tags: Vec<SmolStr>,
}

struct QueryRequest<I> {
  filter_nodes: Vec<I>,
  filter_tags: Tags,
  request_ack: bool,
  relay_factor: u8,
  timeout: Duration,
  name: SmolStr,
  payload: Bytes,
}

struct RespondRequest {
  id: u64,
  payload: Bytes,
}

struct QueryRecord<I> {
  ty: u8,
  from: I,
  payload: Bytes,
}

struct LogRecord {
  log: SmolStr,
}

struct UserEventRecord {
  event: SmolStr,
  ltime: LamportTime,
  name: SmolStr,
  payload: Bytes,
  coalesce: bool,
}

struct QueryEventRecord {
  id: u64,
  event: SmolStr,
  ltime: LamportTime,
  name: SmolStr,
  payload: Bytes,
}
