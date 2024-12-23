use std::{
  borrow::Cow,
  collections::HashSet,
  fs::{File, OpenOptions},
  io::{BufReader, BufWriter, Read, Seek, Write},
  mem,
  path::PathBuf,
  time::Duration,
};

#[cfg(unix)]
use std::os::unix::prelude::OpenOptionsExt;

use async_channel::{Receiver, Sender};
use byteorder::{LittleEndian, ReadBytesExt};
use futures::FutureExt;
use memberlist_core::{
  agnostic_lite::{AsyncSpawner, RuntimeLite},
  bytes::{BufMut, BytesMut},
  tracing,
  transport::{AddressResolver, Id, MaybeResolvedAddress, Node, Transport},
  types::TinyVec,
  CheapClone,
};
use rand::seq::SliceRandom;
use serf_types::UserEventMessage;

use crate::{
  delegate::{Delegate, TransformDelegate},
  event::{CrateEvent, MemberEvent, MemberEventType},
  invalid_data_io_error,
  types::{Epoch, LamportClock, LamportTime},
};

/// How often we force a flush of the snapshot file
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// How often we fetch the current lamport time of the cluster and write to the snapshot file
const CLOCK_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// The extention we use for the temporary file during compaction
const TMP_EXT: &str = "compact";

/// How often we attempt to recover from
/// errors writing to the snapshot file.
const SNAPSHOT_ERROR_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);

/// The size of the event buffers between Serf and the
/// consuming application. If this is exhausted we will block Serf and Memberlist.
const EVENT_CH_SIZE: usize = 2048;

/// The time limit to write pending events to the snapshot during a shutdown
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// An estimated bytes per node to snapshot
const SNAPSHOT_BYTES_PER_NODE: usize = 128;

/// The threshold we apply to
/// the snapshot size estimate (nodes * bytes per node) before compacting.
const SNAPSHOT_COMPACTION_THRESHOLD: usize = 2;

/// Errors that can occur while interacting with snapshots
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
  /// Returned when opening a snapshot fails
  #[error("failed to open snapshot: {0}")]
  Open(std::io::Error),
  /// Returned when opening a new snapshot fails
  #[error("failed to open new snapshot: {0}")]
  OpenNew(std::io::Error),
  /// Returned when flush new snapshot fails
  #[error("failed to flush new snapshot: {0}")]
  FlushNew(std::io::Error),
  /// Returned when flush snapshot fails
  #[error("failed to flush snapshot: {0}")]
  Flush(std::io::Error),
  /// Returned when fsync snapshot fails
  #[error("failed to fsync snapshot: {0}")]
  Sync(std::io::Error),
  /// Returned when stat snapshot fails
  #[error("failed to stat snapshot: {0}")]
  Stat(std::io::Error),
  /// Returned when remove old snapshot fails
  #[error("failed to remove old snapshot: {0}")]
  Remove(std::io::Error),
  /// Returned when installing a new snapshot fails
  #[error("failed to install new snapshot: {0}")]
  Install(std::io::Error),
  /// Returned when writing to a new snapshot fails
  #[error("failed to write to new snapshot: {0}")]
  WriteNew(std::io::Error),
  /// Returned when writing to a snapshot fails
  #[error("failed to write to snapshot: {0}")]
  Write(std::io::Error),
  /// Returned when seek to start of a snapshot fails
  #[error("failed to seek to beginning of snapshot: {0}")]
  SeekStart(std::io::Error),
  /// Returned when seek to end of a snapshot fails
  #[error("failed to seek to end of snapshot: {0}")]
  SeekEnd(std::io::Error),
  /// Returned when replaying a snapshot fails
  #[error("failed to replay snapshot: {0}")]
  Replay(std::io::Error),
  /// Returned when fail to decode snapshot record type.
  #[error(transparent)]
  UnknownRecordType(#[from] UnknownRecordType),
}

/// UnknownRecordType is used to indicate that we encountered an unknown
/// record type while reading a snapshot file.
#[derive(Debug, thiserror::Error)]
#[error("unrecognized snapshot record type: {0}")]
pub struct UnknownRecordType(u8);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum SnapshotRecordType {
  Alive = 0,
  NotAlive = 1,
  Clock = 2,
  EventClock = 3,
  QueryClock = 4,
  Coordinate = 5,
  Leave = 6,
  Comment = 7,
}

impl TryFrom<u8> for SnapshotRecordType {
  type Error = UnknownRecordType;

  fn try_from(value: u8) -> Result<Self, Self::Error> {
    match value {
      0 => Ok(Self::Alive),
      1 => Ok(Self::NotAlive),
      2 => Ok(Self::Clock),
      3 => Ok(Self::EventClock),
      4 => Ok(Self::QueryClock),
      5 => Ok(Self::Coordinate),
      6 => Ok(Self::Leave),
      7 => Ok(Self::Comment),
      v => Err(UnknownRecordType(v)),
    }
  }
}

#[allow(dead_code)]
enum SnapshotRecord<'a, I: Clone, A: Clone> {
  Alive(Cow<'a, Node<I, A>>),
  NotAlive(Cow<'a, Node<I, A>>),
  Clock(LamportTime),
  EventClock(LamportTime),
  QueryClock(LamportTime),
  Coordinate,
  Leave,
  Comment,
}

const MAX_INLINED_BYTES: usize = 64;

macro_rules! encode {
  ($w:ident.$node: ident::$status: ident) => {{
    let node = $node.as_ref();
    let encoded_node_len = T::node_encoded_len(node);
    let encoded_len = 4 + 1 + encoded_node_len;
    if encoded_len <= MAX_INLINED_BYTES {
      let mut buf = [0u8; MAX_INLINED_BYTES];
      buf[0] = Self::$status;
      buf[1..5].copy_from_slice(&(encoded_node_len as u32).to_le_bytes());
      T::encode_node(node, &mut buf[5..]).map_err(invalid_data_io_error)?;
      $w.write_all(&buf[..encoded_len]).map(|_| encoded_len)
    } else {
      let mut buf = BytesMut::with_capacity(encoded_len);
      buf.put_u8(Self::$status);
      buf.put_u32_le(encoded_node_len as u32);
      T::encode_node(node, &mut buf).map_err(invalid_data_io_error)?;
      $w.write_all(&buf).map(|_| encoded_len)
    }
  }};
  ($w:ident.$t: ident($status: ident)) => {{
    const N: usize = mem::size_of::<u8>() + mem::size_of::<u64>();
    let mut data = [0u8; N];
    data[0] = Self::$status;
    data[1..N].copy_from_slice(&$t.to_le_bytes());
    $w.write_all(&data).map(|_| N)
  }};
  ($w:ident.$ident: ident) => {{
    $w.write_all(&[Self::$ident]).map(|_| 1)
  }};
}

impl<I, A> SnapshotRecord<'_, I, A>
where
  I: Id,
  A: CheapClone + Send + Sync + 'static,
{
  const ALIVE: u8 = 0;
  const NOT_ALIVE: u8 = 1;
  const CLOCK: u8 = 2;
  const EVENT_CLOCK: u8 = 3;
  const QUERY_CLOCK: u8 = 4;
  const COORDINATE: u8 = 5;
  const LEAVE: u8 = 6;
  const COMMENT: u8 = 7;

  fn encode<T: TransformDelegate<Id = I, Address = A>, W: Write>(
    &self,
    w: &mut W,
  ) -> std::io::Result<usize> {
    match self {
      Self::Alive(id) => encode!(w.id::ALIVE),
      Self::NotAlive(id) => encode!(w.id::NOT_ALIVE),
      Self::Clock(t) => encode!(w.t(CLOCK)),
      Self::EventClock(t) => encode!(w.t(EVENT_CLOCK)),
      Self::QueryClock(t) => encode!(w.t(QUERY_CLOCK)),
      Self::Coordinate => encode!(w.COORDINATE),
      Self::Leave => encode!(w.LEAVE),
      Self::Comment => encode!(w.COMMENT),
    }
  }
}

#[viewit::viewit]
pub(crate) struct ReplayResult<I, A> {
  alive_nodes: HashSet<Node<I, A>>,
  last_clock: LamportTime,
  last_event_clock: LamportTime,
  last_query_clock: LamportTime,
  offset: u64,
  fh: File,
  path: PathBuf,
}

pub(crate) fn open_and_replay_snapshot<
  I: Id,
  A: CheapClone + core::hash::Hash + Eq + Send + Sync + 'static,
  T: TransformDelegate<Id = I, Address = A>,
  P: AsRef<std::path::Path>,
>(
  p: &P,
  rejoin_after_leave: bool,
) -> Result<ReplayResult<I, A>, SnapshotError> {
  // Try to open the file
  #[cfg(unix)]
  let fh = OpenOptions::new()
    .create(true)
    .append(true)
    .read(true)
    .mode(0o644)
    .open(p)
    .map_err(SnapshotError::Open)?;
  #[cfg(not(unix))]
  let fh = OpenOptions::new()
    .create(true)
    .append(true)
    .read(true)
    .write(true)
    .open(p)
    .map_err(SnapshotError::Open)?;

  // Determine the offset
  let offset = fh.metadata().map_err(SnapshotError::Stat)?.len();

  // Read each line
  let mut reader = BufReader::new(fh);
  let mut buf = Vec::new();
  let mut alive_nodes = HashSet::new();
  let mut last_clock = LamportTime::ZERO;
  let mut last_event_clock = LamportTime::ZERO;
  let mut last_query_clock = LamportTime::ZERO;

  loop {
    let kind = match reader.read_u8() {
      Ok(b) => SnapshotRecordType::try_from(b)?,
      Err(e) => {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
          break;
        }
        return Err(SnapshotError::Replay(e));
      }
    };

    match kind {
      SnapshotRecordType::Alive => {
        let len = reader
          .read_u32::<LittleEndian>()
          .map_err(SnapshotError::Replay)? as usize;
        buf.resize(len, 0);
        reader.read_exact(&mut buf).map_err(SnapshotError::Replay)?;

        let (_, node) =
          T::decode_node(&buf).map_err(|e| SnapshotError::Replay(invalid_data_io_error(e)))?;
        alive_nodes.insert(node);
      }
      SnapshotRecordType::NotAlive => {
        let len = reader
          .read_u32::<LittleEndian>()
          .map_err(SnapshotError::Replay)? as usize;
        buf.resize(len, 0);
        reader.read_exact(&mut buf).map_err(SnapshotError::Replay)?;

        let (_, node) =
          T::decode_node(&buf).map_err(|e| SnapshotError::Replay(invalid_data_io_error(e)))?;
        alive_nodes.remove(&node);
      }
      SnapshotRecordType::Clock => {
        let t = reader
          .read_u64::<LittleEndian>()
          .map_err(SnapshotError::Replay)?;
        last_clock = LamportTime::new(t);
      }
      SnapshotRecordType::EventClock => {
        let t = reader
          .read_u64::<LittleEndian>()
          .map_err(SnapshotError::Replay)?;
        last_event_clock = LamportTime::new(t);
      }
      SnapshotRecordType::QueryClock => {
        let t = reader
          .read_u64::<LittleEndian>()
          .map_err(SnapshotError::Replay)?;
        last_query_clock = LamportTime::new(t);
      }
      SnapshotRecordType::Coordinate => continue,
      SnapshotRecordType::Leave => {
        // Ignore a leave if we plan on re-joining
        if rejoin_after_leave {
          tracing::info!("serf: ignoring previous leave in snapshot");
          continue;
        }
        alive_nodes.clear();
        last_clock = LamportTime::ZERO;
        last_event_clock = LamportTime::ZERO;
        last_query_clock = LamportTime::ZERO;
      }
      SnapshotRecordType::Comment => continue,
    }
  }

  // Seek to the end
  let mut f = reader.into_inner();

  f.seek(std::io::SeekFrom::End(0))
    .map(|_| ReplayResult {
      alive_nodes,
      last_clock,
      last_event_clock,
      last_query_clock,
      offset,
      fh: f,
      path: p.as_ref().to_path_buf(),
    })
    .map_err(SnapshotError::SeekEnd)
}

pub(crate) struct SnapshotHandle {
  wait_rx: Receiver<()>,
  shutdown_rx: Receiver<()>,
  leave_tx: Sender<()>,
}

impl SnapshotHandle {
  /// Used to wait until the snapshotter finishes shut down
  pub(crate) async fn wait(&self) {
    let _ = self.wait_rx.recv().await;
  }

  /// Used to remove known nodes to prevent a restart from
  /// causing a join. Otherwise nodes will re-join after leaving!
  pub(crate) async fn leave(&self) {
    futures::select! {
      _ = self.leave_tx.send(()).fuse() => {},
      _ = self.shutdown_rx.recv().fuse() => {},
    }
  }
}

/// Responsible for ingesting events and persisting
/// them to disk, and providing a recovery mechanism at start time.
pub(crate) struct Snapshot<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  alive_nodes: HashSet<Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>>,
  clock: LamportClock,
  fh: Option<BufWriter<File>>,
  last_flush: Epoch,
  last_clock: LamportTime,
  last_event_clock: LamportTime,
  last_query_clock: LamportTime,
  leave_rx: Receiver<()>,
  leaving: bool,
  min_compact_size: u64,
  path: PathBuf,
  offset: u64,
  rejoin_after_leave: bool,
  stream_rx: Receiver<CrateEvent<T, D>>,
  shutdown_rx: Receiver<()>,
  wait_tx: Sender<()>,
  last_attempted_compaction: Epoch,
  #[cfg(feature = "metrics")]
  metric_labels: std::sync::Arc<memberlist_core::types::MetricLabels>,
}

// flushEvent is used to handle writing out an event
macro_rules! stream_flush_event {
  ($this:ident <- $event:ident) => {{
    // Stop recording events after a leave is issued
    if $this.leaving {
      break;
    }

    match &$event {
      CrateEvent::Member(e) => $this.process_member_event(e),
      CrateEvent::User(e) => $this.process_user_event(e),
      CrateEvent::Query(e) => $this.process_query_event(e.ltime),
      CrateEvent::InternalQuery { query, .. } => $this.process_query_event(query.ltime),
    }
  }};
}

macro_rules! tee_stream_flush_event {
  ($stream_tx:ident <- $event:ident -> $out_tx:ident) => {{
    // Forward to the internal stream, do not block
    futures::select! {
      _ = $stream_tx.send($event.clone()).fuse() => {}
      default => {}
    }

    // Forward the event immediately, do not block
    futures::select! {
      _ = $out_tx.send($event).fuse() => {}
      default => {}
    }
  }};
}

impl<D, T> Snapshot<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  #[allow(clippy::type_complexity)]
  pub(crate) fn from_replay_result(
    replay_result: ReplayResult<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
    min_compact_size: u64,
    rejoin_after_leave: bool,
    clock: LamportClock,
    out_tx: Sender<CrateEvent<T, D>>,
    shutdown_rx: Receiver<()>,
    #[cfg(feature = "metrics")] metric_labels: std::sync::Arc<memberlist_core::types::MetricLabels>,
  ) -> Result<
    (
      Sender<CrateEvent<T, D>>,
      TinyVec<Node<T::Id, MaybeResolvedAddress<T>>>,
      SnapshotHandle,
    ),
    SnapshotError,
  > {
    let (in_tx, in_rx) = async_channel::bounded(EVENT_CH_SIZE);
    let (stream_tx, stream_rx) = async_channel::bounded(EVENT_CH_SIZE);
    let (leave_tx, leave_rx) = async_channel::bounded(1);
    let (wait_tx, wait_rx) = async_channel::bounded(1);

    let ReplayResult {
      alive_nodes,
      last_clock,
      last_event_clock,
      last_query_clock,
      offset,
      fh,
      path,
    } = replay_result;

    // Create the snapshotter
    let this = Self {
      alive_nodes,
      clock,
      fh: Some(BufWriter::new(fh)),
      last_flush: Epoch::now(),
      last_clock,
      last_event_clock,
      last_query_clock,
      leave_rx,
      leaving: false,
      min_compact_size,
      path,
      offset,
      rejoin_after_leave,
      stream_rx,
      shutdown_rx: shutdown_rx.clone(),
      wait_tx,
      last_attempted_compaction: Epoch::now(),
      #[cfg(feature = "metrics")]
      metric_labels,
    };

    let mut alive_nodes = this
      .alive_nodes
      .iter()
      .map(|n| {
        let id = n.id().cheap_clone();
        let addr = n.address().cheap_clone();
        Node::new(id, MaybeResolvedAddress::resolved(addr))
      })
      .collect::<TinyVec<_>>();
    alive_nodes.shuffle(&mut rand::thread_rng());

    // Start handling new commands
    let handle = <T::Runtime as RuntimeLite>::spawn(Self::tee_stream(
      in_rx,
      stream_tx,
      out_tx,
      shutdown_rx.clone(),
    ));
    <T::Runtime as RuntimeLite>::spawn_detach(this.stream(handle));

    Ok((
      in_tx,
      alive_nodes,
      SnapshotHandle {
        wait_rx,
        shutdown_rx,
        leave_tx,
      },
    ))
  }

  /// A long running routine that is used to copy events
  /// to the output channel and the internal event handler.
  async fn tee_stream(
    in_rx: Receiver<CrateEvent<T, D>>,
    stream_tx: Sender<CrateEvent<T, D>>,
    out_tx: Sender<CrateEvent<T, D>>,
    shutdown_rx: Receiver<()>,
  ) {
    loop {
      futures::select! {
        ev = in_rx.recv().fuse() => {
          if let Ok(ev) = ev {
            tee_stream_flush_event!(stream_tx <- ev -> out_tx)
          } else {
            break;
          }
        }
        _ = shutdown_rx.recv().fuse() => {
          break;
        }
      }
    }

    // Drain any remaining events before exiting
    loop {
      futures::select! {
        ev = in_rx.recv().fuse() => {
          if let Ok(ev) = ev {
            tee_stream_flush_event!(stream_tx <- ev -> out_tx)
          } else {
            break;
          }
        }
        default => break,
      }
    }
    tracing::debug!("serf: snapshotter tee stream exits");
  }

  fn handle_leave(&mut self) {
    self.leaving = true;

    // If we plan to re-join, keep our state
    if !self.rejoin_after_leave {
      self.alive_nodes.clear();
    }
    self.try_append(SnapshotRecord::Leave);
    if let Some(fh) = self.fh.as_mut() {
      if let Err(e) = fh.flush() {
        tracing::error!(target="serf", err=%SnapshotError::Flush(e), "failed to flush leave to snapshot");
      }

      if let Err(e) = fh.get_mut().sync_all() {
        tracing::error!(target="serf", err=%SnapshotError::Sync(e), "failed to sync leave to snapshot");
      }
    }
  }

  /// Long running routine that is used to handle events
  async fn stream(
    mut self,
    tee_handle: <<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()>,
  ) {
    let mut clock_ticker = <T::Runtime as RuntimeLite>::interval(CLOCK_UPDATE_INTERVAL);

    loop {
      futures::select! {
        signal = self.leave_rx.recv().fuse() => {
          if signal.is_ok() {
            self.handle_leave();
          }
        }
        ev = self.stream_rx.recv().fuse() => {
          if let Ok(ev) = ev {
            stream_flush_event!(self <- ev)
          } else {
            break;
          }
        }
        _ = futures::StreamExt::next(&mut clock_ticker).fuse() => {
          self.update_clock();
        }
        _ = self.shutdown_rx.recv().fuse() => {
          break;
        }
      }
    }

    if self.leave_rx.try_recv().is_ok() {
      self.handle_leave();
    }

    // Setup a timeout
    let flush_timeout = <T::Runtime as RuntimeLite>::sleep(SHUTDOWN_FLUSH_TIMEOUT);
    futures::pin_mut!(flush_timeout);

    // snapshot the clock
    self.update_clock();

    // Clear out the buffers
    loop {
      futures::select! {
        ev = self.stream_rx.recv().fuse() => {
          if let Ok(ev) = ev {
            stream_flush_event!(self <- ev)
          } else {
            break;
          }
        }
        _ = (&mut flush_timeout).fuse() => {
          break;
        }
        default => {
          break;
        }
      }
    }

    if let Some(fh) = self.fh.as_mut() {
      if let Err(e) = fh.flush() {
        tracing::error!(target="serf", err=%SnapshotError::Flush(e), "failed to flush leave to snapshot");
      }

      if let Err(e) = fh.get_mut().sync_all() {
        tracing::error!(target="serf", err=%SnapshotError::Sync(e), "failed to sync leave to snapshot");
      }
    }

    self.wait_tx.close();
    tee_handle.await;
    tracing::debug!("serf: snapshotter stream exits");
  }

  /// Used to handle a single user event
  fn process_user_event(&mut self, e: &UserEventMessage) {
    // Ignore old clocks
    let ltime = e.ltime();
    if ltime <= self.last_event_clock {
      return;
    }

    self.last_event_clock = ltime;
    self.try_append(SnapshotRecord::EventClock(ltime));
  }

  /// Used to handle a single query event
  fn process_query_event(&mut self, ltime: LamportTime) {
    // Ignore old clocks
    if ltime <= self.last_query_clock {
      return;
    }

    self.last_query_clock = ltime;
    self.try_append(SnapshotRecord::QueryClock(ltime));
  }

  /// Used to handle a single member event
  fn process_member_event(
    &mut self,
    e: &MemberEvent<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
  ) {
    match e.ty {
      MemberEventType::Join => {
        for m in e.members() {
          let node = m.node();
          self.alive_nodes.insert(node.cheap_clone());
          self.try_append(SnapshotRecord::Alive(Cow::Borrowed(node)))
        }
      }
      MemberEventType::Leave | MemberEventType::Failed => {
        for m in e.members() {
          let node = m.node();
          self.alive_nodes.remove(node);
          self.try_append(SnapshotRecord::NotAlive(Cow::Borrowed(node)));
        }
      }
      _ => {}
    }
    self.update_clock();
  }

  /// Called periodically to check if we should udpate our
  /// clock value. This is done after member events but should also be done
  /// periodically due to race conditions with join and leave intents
  fn update_clock(&mut self) {
    let t: u64 = self.clock.time().into();
    let last_seen = LamportTime::from(t.saturating_sub(1));
    if last_seen > self.last_clock {
      self.last_clock = last_seen;
      self.try_append(SnapshotRecord::Clock(self.last_clock));
    }
  }

  fn try_append(
    &mut self,
    l: SnapshotRecord<'_, T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
  ) {
    if let Err(e) = self.append_line(l) {
      tracing::error!(err = %e, "serf: failed to update snapshot");
      if self.last_attempted_compaction.elapsed() > SNAPSHOT_ERROR_RECOVERY_INTERVAL {
        self.last_attempted_compaction = Epoch::now();
        tracing::info!("serf: attempting compaction to recover from error...");
        if let Err(e) = self.compact() {
          tracing::error!(err = %e, "serf: compaction failed, will reattempt after {}s", SNAPSHOT_ERROR_RECOVERY_INTERVAL.as_secs());
        } else {
          tracing::info!("serf: finished compaction, successfully recovered from error state");
        }
      }
    }
  }

  fn append_line(
    &mut self,
    l: SnapshotRecord<'_, T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
  ) -> Result<(), SnapshotError> {
    #[cfg(feature = "metrics")]
    let start = crate::types::Epoch::now();

    #[cfg(feature = "metrics")]
    let metric_labels = self.metric_labels.clone();
    #[cfg(feature = "metrics")]
    scopeguard::defer!(
      metrics::histogram!("serf.snapshot.append_line", metric_labels.iter())
        .record(start.elapsed().as_millis() as f64)
    );

    let f = self.fh.as_mut().unwrap();
    let n = l.encode::<D, _>(f).map_err(SnapshotError::Write)?;

    // check if we should flush
    if self.last_flush.elapsed() > FLUSH_INTERVAL {
      self.last_flush = Epoch::now();
      self
        .fh
        .as_mut()
        .unwrap()
        .flush()
        .map_err(SnapshotError::Flush)?;
    }

    // Check if a compaction is necessary
    self.offset += n as u64;
    if self.offset > self.snapshot_max_size() {
      self.compact()?;
    }
    Ok(())
  }

  /// Computes the maximum size and is used to force periodic compaction.
  fn snapshot_max_size(&self) -> u64 {
    let nodes = self.alive_nodes.len() as u64;
    let est_size = nodes * SNAPSHOT_BYTES_PER_NODE as u64;
    let threshold = est_size * SNAPSHOT_COMPACTION_THRESHOLD as u64;
    threshold.max(self.min_compact_size)
  }

  /// Used to compact the snapshot once it is too large
  fn compact(&mut self) -> Result<(), SnapshotError> {
    #[cfg(feature = "metrics")]
    let start = crate::types::Epoch::now();

    #[cfg(feature = "metrics")]
    let metric_labels = self.metric_labels.clone();
    #[cfg(feature = "metrics")]
    scopeguard::defer!(
      metrics::histogram!("serf.snapshot.compact", metric_labels.iter())
        .record(start.elapsed().as_millis() as f64)
    );

    // Try to open the file to new file
    let new_path = self.path.with_extension(TMP_EXT);
    #[cfg(unix)]
    let fh = OpenOptions::new()
      .create(true)
      .write(true)
      .truncate(true)
      .mode(0o755)
      .open(&new_path)
      .map_err(SnapshotError::OpenNew)?;

    #[cfg(not(unix))]
    let fh = OpenOptions::new()
      .create(true)
      .write(true)
      .truncate(true)
      .open(&new_path)
      .map_err(SnapshotError::OpenNew)?;

    // Create a buffered writer
    let mut buf = BufWriter::new(fh);

    // Write out the live nodes
    let mut offset = 0u64;
    for node in self.alive_nodes.iter() {
      offset += SnapshotRecord::Alive(Cow::Borrowed(node))
        .encode::<D, _>(&mut buf)
        .map_err(SnapshotError::WriteNew)? as u64;
    }

    // Write out the clocks
    offset += SnapshotRecord::Clock(self.last_clock)
      .encode::<D, _>(&mut buf)
      .map_err(SnapshotError::WriteNew)? as u64;

    offset += SnapshotRecord::EventClock(self.last_event_clock)
      .encode::<D, _>(&mut buf)
      .map_err(SnapshotError::WriteNew)? as u64;

    offset += SnapshotRecord::QueryClock(self.last_query_clock)
      .encode::<D, _>(&mut buf)
      .map_err(SnapshotError::WriteNew)? as u64;

    // Flush the new snapshot
    buf.flush().map_err(SnapshotError::Flush)?;

    // Sync the new snapshot
    buf.get_ref().sync_all().map_err(SnapshotError::Sync)?;
    drop(buf);

    // We now need to swap the old snapshot file with the new snapshot.
    // Turns out, Windows won't let us rename the files if we have
    // open handles to them or if the destination already exists. This
    // means we are forced to close the existing handles, delete the
    // old file, move the new one in place, and then re-open the file
    // handles.

    // Flush the existing snapshot, ignoring errors since we will
    // delete it momentarily.
    let mut old = self.fh.take().unwrap();
    let _ = old.flush();
    drop(old);

    // Delete the old file
    if let Err(e) = std::fs::remove_file(&self.path) {
      if !matches!(e.kind(), std::io::ErrorKind::NotFound) {
        return Err(SnapshotError::Remove(e));
      }
    }

    // Move the new file into place
    std::fs::rename(&new_path, &self.path).map_err(SnapshotError::Install)?;

    // Open the new snapshot
    #[cfg(unix)]
    let fh = OpenOptions::new()
      .create(true)
      .append(true)
      .read(true)
      .mode(0o755)
      .open(&self.path)
      .map_err(SnapshotError::Open)?;

    #[cfg(not(unix))]
    let fh = OpenOptions::new()
      .create(true)
      .append(true)
      .read(true)
      .write(true)
      .open(&self.path)
      .map_err(SnapshotError::Open)?;

    self.fh = Some(BufWriter::new(fh));
    self.offset = offset;
    self.last_flush = Epoch::now();
    Ok(())
  }
}
