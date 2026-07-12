//! Driver-side snapshot persistence: an append-only record file the pump feeds
//! on every surfaced membership change, replayed at construction to recover
//! membership and the Lamport clocks across a restart.
//!
//! The machine owns the record format ([`SnapshotRecord`]) and the replay fold
//! ([`serf_proto::snapshot::ReplayResult`]); this module owns the file. The
//! `Serf` constructor reads and decodes the file (so a corrupt snapshot fails
//! construction loudly), the transport body replays it into the endpoint
//! (`load_snapshot` re-dials the recovered peers through the normal push/pull
//! machinery), and the pump appends records at its event chokepoint.

use std::{
  fs,
  io::{self, Write as _},
  net::SocketAddr,
  path::PathBuf,
};

use memberlist_proto::Data;
use serf_proto::{LamportTime, snapshot::SnapshotRecord};

use super::options::SnapshotOptions;

/// The snapshot writer paired with the records already on disk, as handed
/// from the constructor (which opens and decodes) to the transport body
/// (which replays and pumps).
pub(crate) type OpenedSnapshot<I> = (Snapshotter<I>, Vec<SnapshotRecord<I, std::net::SocketAddr>>);

/// The append-side of the snapshot file, held by the driver pump.
///
/// Writes are buffered and flushed after every append batch — a surfaced
/// membership change is durable once the pump's poll returns. When the file
/// grows past the compaction threshold, the next append rewrites it to just
/// the current alive set and clock floors (via a sibling temp file and an
/// atomic rename), exactly the state a replay needs.
pub(crate) struct Snapshotter<I> {
  path: PathBuf,
  file: io::BufWriter<fs::File>,
  bytes_written: u64,
  compact_threshold: u64,
  last_member_clock: LamportTime,
  last_event_clock: LamportTime,
  last_query_clock: LamportTime,
  /// The local node has cleanly left (a `Leave` marker was appended) with no
  /// membership activity since. Compaction must re-emit the marker LAST so
  /// the rewritten file still replays to a gated fresh start under
  /// `rejoin_after_leave = false` while preserving the pre-leave membership
  /// for the opt-in posture.
  clean_left: bool,
  _id: core::marker::PhantomData<fn(I)>,
}

impl<I> Snapshotter<I>
where
  I: Data + Clone + Eq + core::hash::Hash,
{
  /// Open (or create) the snapshot file for appending, returning the writer
  /// and the decoded records already on disk.
  ///
  /// A truncated TAIL (a crash mid-append) is tolerated: decoding stops at a
  /// partial trailing record and appends continue after the last whole one. A
  /// malformed record BEFORE the tail (an unknown tag or an undecodable node)
  /// is a hard error — the file is not trustworthy.
  pub(crate) fn open(opts: &SnapshotOptions) -> Result<OpenedSnapshot<I>, SnapshotOpenError> {
    let path = opts.path().to_path_buf();
    let raw = match fs::read(&path) {
      Ok(b) => b,
      Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
      Err(e) => return Err(SnapshotOpenError::Io(e)),
    };

    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < raw.len() {
      match SnapshotRecord::<I, SocketAddr>::decode(&raw[cursor..]) {
        Ok((rec, consumed)) => {
          records.push(rec);
          cursor += consumed;
        }
        Err(serf_proto::snapshot::SnapshotError::Truncated { .. }) => {
          // A partial trailing record from a crash mid-append: replay what is
          // whole and let the writer continue after it.
          break;
        }
        Err(e) => return Err(SnapshotOpenError::Corrupt(e)),
      }
    }

    // Truncate any partial tail so new appends start on a whole-record
    // boundary; then reopen for appending.
    let file = fs::OpenOptions::new()
      .create(true)
      .truncate(false)
      .write(true)
      .open(&path)
      .map_err(SnapshotOpenError::Io)?;
    file.set_len(cursor as u64).map_err(SnapshotOpenError::Io)?;
    drop(file);
    let file = fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .map_err(SnapshotOpenError::Io)?;

    Ok((
      Self {
        path,
        file: io::BufWriter::new(file),
        bytes_written: cursor as u64,
        compact_threshold: opts.compact_threshold(),
        last_member_clock: LamportTime::ZERO,
        last_event_clock: LamportTime::ZERO,
        last_query_clock: LamportTime::ZERO,
        clean_left: records
          .iter()
          .rev()
          .find_map(|r| match r {
            SnapshotRecord::Leave => Some(true),
            SnapshotRecord::Alive(_) | SnapshotRecord::NotAlive(_) => Some(false),
            _ => None,
          })
          .unwrap_or(false),
        _id: core::marker::PhantomData,
      },
      records,
    ))
  }

  /// Append one record, best-effort. An encode or write failure is surfaced
  /// through `tracing` (the wire state is authoritative; the file is the
  /// durable copy) and the record is dropped.
  fn append(&mut self, record: &SnapshotRecord<I, SocketAddr>) {
    match record.encode() {
      Ok(bytes) => {
        if let Err(_err) = self.file.write_all(&bytes) {
          #[cfg(feature = "tracing")]
          tracing::warn!(
            path = %self.path.display(),
            error = %_err,
            "serf snapshot append failed; the record is dropped"
          );
          return;
        }
        self.bytes_written += bytes.len() as u64;
      }
      Err(_err) => {
        #[cfg(feature = "tracing")]
        tracing::warn!(
          path = %self.path.display(),
          error = %_err,
          "serf snapshot record could not be encoded; the record is dropped"
        );
      }
    }
  }

  /// Append the membership records for one surfaced member event: `Alive` for
  /// a joined or updated member, `NotAlive` for a left, failed, or reaped one.
  pub(crate) fn append_member(
    &mut self,
    alive: bool,
    node: &memberlist_proto::Node<I, SocketAddr>,
  ) {
    let record = if alive {
      SnapshotRecord::Alive(node.clone())
    } else {
      SnapshotRecord::NotAlive(node.clone())
    };
    self.clean_left = false;
    self.append(&record);
  }

  /// Append any clock high-water marks that advanced since the last append.
  pub(crate) fn append_clocks(
    &mut self,
    member: LamportTime,
    event: LamportTime,
    query: LamportTime,
  ) {
    if member > self.last_member_clock {
      self.append(&SnapshotRecord::Clock(member));
      self.last_member_clock = member;
    }
    if event > self.last_event_clock {
      self.append(&SnapshotRecord::EventClock(event));
      self.last_event_clock = event;
    }
    if query > self.last_query_clock {
      self.append(&SnapshotRecord::QueryClock(query));
      self.last_query_clock = query;
    }
  }

  /// Append the leave marker: the local node cleanly left the cluster. On the
  /// next start, replay clears the recovered state unless
  /// `rejoin_after_leave` ignores it.
  pub(crate) fn append_leave(&mut self) {
    self.clean_left = true;
    self.append(&SnapshotRecord::Leave);
  }

  /// Flush the buffered appends to the OS, and — when the file has grown past
  /// the compaction threshold — rewrite it to just `alive` and the clock
  /// floors via a sibling temp file and an atomic rename.
  pub(crate) fn flush_and_maybe_compact(
    &mut self,
    alive: impl FnOnce() -> Vec<memberlist_proto::Node<I, SocketAddr>>,
  ) {
    if let Err(_err) = self.file.flush() {
      #[cfg(feature = "tracing")]
      tracing::warn!(
        path = %self.path.display(),
        error = %_err,
        "serf snapshot flush failed"
      );
    }
    if self.compact_threshold == 0 || self.bytes_written < self.compact_threshold {
      return;
    }
    let mut fresh: Vec<u8> = Vec::new();
    for rec in [
      SnapshotRecord::<I, SocketAddr>::Clock(self.last_member_clock),
      SnapshotRecord::EventClock(self.last_event_clock),
      SnapshotRecord::QueryClock(self.last_query_clock),
    ] {
      if let Ok(b) = rec.encode() {
        fresh.extend_from_slice(&b);
      }
    }
    for node in alive() {
      if let Ok(b) = SnapshotRecord::<I, SocketAddr>::Alive(node).encode() {
        fresh.extend_from_slice(&b);
      }
    }
    // A clean leave survives compaction: re-emitted LAST, so the rewritten
    // file replays to the gated fresh start under `rejoin_after_leave =
    // false` and to the preserved membership under the opt-in posture —
    // exactly like the original record sequence it replaces.
    if self.clean_left
      && let Ok(b) = SnapshotRecord::<I, SocketAddr>::Leave.encode()
    {
      fresh.extend_from_slice(&b);
    }
    // Write and OPEN the replacement before the rename, so no fallible
    // operation remains after the swap: a failure here leaves the grown file
    // authoritative and appends continue on it, while a completed rename is
    // always paired with a live append handle on the SAME inode.
    let tmp = self.path.with_extension("compact");
    // Plain write mode (append + truncate is a rejected combination): the
    // handle's cursor sits at end-of-file after the write below, and this
    // writer is the file's only one, so subsequent appends continue from the
    // cursor exactly as an append-mode handle would.
    let replaced = fs::OpenOptions::new()
      .create(true)
      .truncate(true)
      .write(true)
      .open(&tmp)
      .and_then(|mut file| {
        file.write_all(&fresh)?;
        file.sync_all()?;
        Ok(file)
      })
      .and_then(|file| fs::rename(&tmp, &self.path).map(|()| file));
    match replaced {
      Ok(file) => {
        self.file = io::BufWriter::new(file);
        self.bytes_written = fresh.len() as u64;
      }
      Err(_err) => {
        #[cfg(feature = "tracing")]
        tracing::warn!(
          path = %self.path.display(),
          error = %_err,
          "serf snapshot compaction failed; appends continue on the grown file"
        );
      }
    }
  }
}

/// Errors surfaced by [`Snapshotter::open`] — construction-time failures, so a
/// node never starts against a snapshot it cannot trust.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotOpenError {
  /// Reading or opening the snapshot file failed.
  #[error(transparent)]
  Io(#[from] io::Error),
  /// A record before the tail is malformed — the file is not trustworthy.
  #[error("corrupt snapshot record: {0}")]
  Corrupt(serf_proto::snapshot::SnapshotError),
}

#[cfg(test)]
mod tests;
