use std::{
  collections::HashMap,
  sync::{Arc, atomic::AtomicBool},
};

use async_lock::{Mutex, RwLock};
use atomic_refcell::AtomicRefCell;
use futures::stream::FuturesUnordered;
use memberlist_core::{
  Memberlist,
  agnostic_lite::{AsyncSpawner, RuntimeLite},
  proto::MediumVec,
  queue::TransmitLimitedQueue,
  transport::{AddressResolver, Transport},
};

use super::{
  Options,
  broadcast::SerfBroadcast,
  delegate::{CompositeDelegate, Delegate},
  event::CrateEvent,
  snapshot::SnapshotHandle,
  types::{
    LamportClock, LamportTime, Members, UserEvents,
    coordinate::{Coordinate, CoordinateClient},
  },
};

mod api;
pub(crate) mod base;

mod delegate;
pub(crate) use delegate::*;

mod query;
pub use query::*;

mod internal_query;

/// Maximum 128 KB snapshot
pub(crate) const SNAPSHOT_SIZE_LIMIT: u64 = 128 * 1024;

/// Maximum 9KB for event name and payload
const USER_EVENT_SIZE_LIMIT: usize = 9 * 1024;

/// Exports the default delegate type
pub type DefaultDelegate<T> = CompositeDelegate<
  <T as Transport>::Id,
  <<T as Transport>::Resolver as AddressResolver>::ResolvedAddress,
>;

pub(crate) struct CoordCore<I> {
  pub(crate) client: CoordinateClient<I>,
  pub(crate) cache: parking_lot::RwLock<HashMap<I, Coordinate>>,
}

/// Stores all the query ids at a specific time
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct Queries {
  ltime: LamportTime,
  query_ids: MediumVec<u32>,
}

#[derive(Default)]
pub(crate) struct QueryCore<I, A> {
  responses: HashMap<LamportTime, QueryResponse<I, A>>,
  min_time: LamportTime,
  buffer: Vec<Option<Queries>>,
}

#[viewit::viewit]
pub(crate) struct EventCore {
  min_time: LamportTime,
  buffer: Vec<Option<UserEvents>>,
}

/// The state of the Serf instance.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum SerfState {
  /// Alive state
  Alive,
  /// Leaving state
  Leaving,
  /// Left state
  Left,
  /// Shutdown state
  Shutdown,
}

impl SerfState {
  /// Returns the string representation of the state.
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::Alive => "alive",
      Self::Leaving => "leaving",
      Self::Left => "left",
      Self::Shutdown => "shutdown",
    }
  }
}

impl core::fmt::Display for SerfState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.as_str())
  }
}

struct NumMembers<I, A>(Arc<RwLock<Members<I, A>>>);

impl<I, A> Clone for NumMembers<I, A> {
  fn clone(&self) -> Self {
    Self(self.0.clone())
  }
}

impl<I, A> From<Arc<RwLock<Members<I, A>>>> for NumMembers<I, A> {
  fn from(value: Arc<RwLock<Members<I, A>>>) -> Self {
    Self(value)
  }
}

impl<I, A> memberlist_core::queue::NodeCalculator for NumMembers<I, A>
where
  I: Send + Sync + 'static,
  A: Send + Sync + 'static,
{
  async fn num_nodes(&self) -> usize {
    self.0.read().await.states.len()
  }
}

pub(crate) struct SerfCore<T, D = DefaultDelegate<T>>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  pub(crate) clock: LamportClock,
  pub(crate) event_clock: LamportClock,
  pub(crate) query_clock: LamportClock,

  broadcasts: Arc<TransmitLimitedQueue<SerfBroadcast, NumMembers<T::Id, T::ResolvedAddress>>>,
  event_broadcasts: Arc<TransmitLimitedQueue<SerfBroadcast, NumMembers<T::Id, T::ResolvedAddress>>>,
  query_broadcasts: Arc<TransmitLimitedQueue<SerfBroadcast, NumMembers<T::Id, T::ResolvedAddress>>>,

  pub(crate) memberlist: Memberlist<T, SerfDelegate<T, D>>,
  pub(crate) members: Arc<RwLock<Members<T::Id, T::ResolvedAddress>>>,
  event_tx: async_channel::Sender<CrateEvent<T, D>>,
  pub(crate) event_join_ignore: AtomicBool,

  pub(crate) event_core: RwLock<EventCore>,
  query_core: Arc<RwLock<QueryCore<T::Id, T::ResolvedAddress>>>,
  handles: AtomicRefCell<
    FuturesUnordered<<<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()>>,
  >,
  pub(crate) opts: Options,

  state: parking_lot::Mutex<SerfState>,

  join_lock: Mutex<()>,

  snapshot: Option<SnapshotHandle>,
  #[cfg(feature = "encryption")]
  key_manager: crate::key_manager::KeyManager<T, D>,
  shutdown_tx: async_channel::Sender<()>,
  shutdown_rx: async_channel::Receiver<()>,

  pub(crate) coord_core: Option<Arc<CoordCore<T::Id>>>,
}

/// Serf is a single node that is part of a single cluster that gets
/// events about joins/leaves/failures/etc. It is created with the Create
/// method.
///
/// All functions on the Serf structure are safe to call concurrently.
#[repr(transparent)]
pub struct Serf<T: Transport, D = DefaultDelegate<T>>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  pub(crate) inner: Arc<SerfCore<T, D>>,
}

impl<T: Transport, D: Delegate> Clone for Serf<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  fn clone(&self) -> Self {
    Self {
      inner: self.inner.clone(),
    }
  }
}
