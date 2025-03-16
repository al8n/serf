use std::{collections::HashMap, time::Duration};

use futures::lock::Mutex;
use serf_core::{coordinate::Coordinate, delegate::Delegate, event::QueryEvent, transport::Transport, types::{bytes::Bytes, MaybeResolvedAddress, Member, MemberStatus, Node, SmolStr, Tags}};

#[cfg(feature = "encryption")]
use serf_core::types::SecretKey;

use serde::{Serialize, Deserialize};

mod event_stream;
mod query_response_stream;

trait StreamClient<T, D> {
  fn send(&self, header: ResponseHeader, data: &[u8]) -> impl Future<Output = std::io::Result<()>> + Send;

  fn register_query(&self, query: QueryEvent<T, D>) -> impl Future<Output = u64> + Send
  where
    T: Transport,
    D: Delegate<Id = T::Id, Address = T::ResolvedAddress>;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum IpcVersion {
  V1 = 1,
}

/// Response header is sent before each response
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResponseHeader {
  seq: u64,
  err: SmolStr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct HandshakeRequest {
  version: IpcVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthRequest {
  auth_key: SmolStr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinateRequest<I> {
  node: I,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinateResponse {
  coord: Coordinate,
  ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventRequest {
  name: SmolStr,
  payload: Bytes,
  coalesce: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ForceLeaveRequest<I> {
  node: I,
  prune: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JoinRequest<I, A, R> {
  existing: Vec<Node<I, MaybeResolvedAddress<A, R>>>,
  replay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JoinResponse {
  num: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MembersFilteredRequest<I> {
  tags: Tags,
  status: MemberStatus,
  name: I,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MembersResponse<I, A> {
  members: Vec<Member<I, A>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg(feature = "encryption")]
struct KeyRequest {
  key: SecretKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg(feature = "encryption")]
struct KeyResponse {
  messages: HashMap<SmolStr, SmolStr>,
  keys: HashMap<SecretKey, u32>,
  num_nodes: u32,
  num_err: u32,
  num_resp: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MonitorRequest {
  log_filter: SmolStr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StreamRequest {
  ty: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StopRequest {
  stop: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TagsRequest {
  tags: Tags,
  delete_tags: Vec<SmolStr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryRequest<I> {
  filter_nodes: Vec<I>,
  filter_tags: Tags,
  request_ack: bool,
  relay_factor: u8,
  timeout: Duration,
  name: SmolStr,
  payload: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RespondRequest {
  id: u64,
  payload: Bytes,
}

pub struct IPCClient<T, D, R, W>
where
  T: Transport,
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
{
  query_id: u64, // used to increment query IDs
  name: SmolStr,
  reader: R,
  writer: Mutex<W>,
  version: IpcVersion, // From the handshake

  pending_queries: Mutex<HashMap<u64, QueryEvent<T, D>>>,

  did_auth: bool, // Dis we get an auth token yet?
}