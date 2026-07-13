//! Pure snapshot record format — encode/decode a single `SnapshotRecord` to/from bytes.
//!
//! The snapshot file is a flat append-only stream of variable-length binary records,
//! each beginning with a one-byte discriminant tag followed by a record-type-specific
//! body.  The tag values are fixed and must match the legacy `serf-core` layout for
//! forward-compatible snapshots:
//!
//! | Tag | Variant        | Body                                                |
//! |-----|----------------|-----------------------------------------------------|
//! |  0  | Alive          | u32 LE node-len + node bytes                        |
//! |  1  | NotAlive       | u32 LE node-len + node bytes                        |
//! |  2  | Clock          | u64 LE lamport time                                 |
//! |  3  | EventClock     | u64 LE lamport time                                 |
//! |  4  | QueryClock     | u64 LE lamport time                                 |
//! |  5  | Coordinate     | (coordinates feature) see below                     |
//! |  6  | Leave          | (empty body)                                        |
//! |  7  | Comment        | (empty body)                                        |
//!
//! The `Coordinate` record body (tag 5, feature="coordinates"):
//! `[node_len: u32 LE][node bytes][dim: u32 LE][dim × f64 LE][error: f64 LE][adj: f64 LE][height: f64 LE]`
//!
//! The serf-core oracle did not persist any coordinate payload (tag only, silently skipped on
//! replay).  This implementation stores the node + Vivaldi coordinate so the driver can warm
//! the `CoordinateClient` on restart without re-probing.
//!
//! The encode/decode API is pure: `&[u8]` slice in, `Bytes` out.
//! No file handles, no `std::fs`, no `Instant::now()`.  The driver owns I/O;
//! this module is consumed by the snapshot replay function in the driver.

use std::vec::Vec;

use bytes::{BufMut, Bytes, BytesMut};
use memberlist_proto::{Data, DataRef};

use crate::LamportTime;

#[cfg(feature = "coordinates")]
use crate::Coordinate;

// ── Tag constants ─────────────────────────────────────────────────────────────

const TAG_ALIVE: u8 = 0;
const TAG_NOT_ALIVE: u8 = 1;
const TAG_CLOCK: u8 = 2;
const TAG_EVENT_CLOCK: u8 = 3;
const TAG_QUERY_CLOCK: u8 = 4;
const TAG_COORDINATE: u8 = 5;
const TAG_LEAVE: u8 = 6;
const TAG_COMMENT: u8 = 7;

// ── SnapshotError ─────────────────────────────────────────────────────────────

/// Errors produced by snapshot record encode/decode.
///
/// Pure codec errors only — no file-I/O variants here; those belong to the driver.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
  /// The tag byte identifies an unknown record type.
  #[error("unknown snapshot record tag: {0}")]
  UnknownTag(u8),
  /// The record body is shorter than required for its type.
  #[error("snapshot record truncated: need {need} bytes, have {have}")]
  Truncated {
    /// Bytes needed to complete the record.
    need: usize,
    /// Bytes actually available.
    have: usize,
  },
  /// A `memberlist_proto::Data` encode error.
  #[error("encode error: {0}")]
  Encode(#[from] memberlist_proto::EncodeError),
  /// A `memberlist_proto::Data` decode error.
  #[error("decode error: {0}")]
  Decode(#[from] memberlist_proto::data::DecodeError),
}

// ── CoordinateRecord ─────────────────────────────────────────────────────────

/// A node-coordinate pair stored inside a [`SnapshotRecord::Coordinate`] record.
///
/// Unlike the serf-core oracle (which wrote only the tag byte and skipped the
/// payload on replay), this implementation stores the full `Node`+`Coordinate`
/// so the driver can warm the `CoordinateClient` from disk without re-probing.
#[cfg(feature = "coordinates")]
#[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
#[derive(Debug, Clone, PartialEq)]
pub struct CoordinateRecord<I, A> {
  /// The node whose coordinate is stored.
  node: memberlist_proto::Node<I, A>,
  /// The cached Vivaldi coordinate for that node.
  coordinate: Coordinate,
}

#[cfg(feature = "coordinates")]
impl<I, A> CoordinateRecord<I, A> {
  /// Constructs a new `CoordinateRecord`.
  pub fn new(node: memberlist_proto::Node<I, A>, coordinate: Coordinate) -> Self {
    Self { node, coordinate }
  }

  /// The node this coordinate describes.
  pub const fn node(&self) -> &memberlist_proto::Node<I, A> {
    &self.node
  }

  /// The Vivaldi coordinate cached for this node.
  pub const fn coordinate(&self) -> &Coordinate {
    &self.coordinate
  }
}

// ── SnapshotRecord ────────────────────────────────────────────────────────────

/// A single record in the serf snapshot file.
///
/// The binary format mirrors the `serf-core` `SnapshotRecord` discriminants so
/// snapshots are forward-compatible.  See the module-level table for the per-tag
/// layout.
///
/// Note: `Eq` is not derived when the `coordinates` feature is enabled because
/// the `Coordinate` variant contains `f64` fields (`f64` does not implement `Eq`).
/// Under the feature, only `PartialEq` is available.
#[derive(Debug, Clone, PartialEq, derive_more::IsVariant)]
#[cfg_attr(not(feature = "coordinates"), derive(Eq))]
#[non_exhaustive]
pub enum SnapshotRecord<I, A> {
  /// A node that was alive at snapshot time.
  Alive(memberlist_proto::Node<I, A>),
  /// A node that was no longer alive at snapshot time.
  NotAlive(memberlist_proto::Node<I, A>),
  /// The member Lamport clock at snapshot time.
  Clock(LamportTime),
  /// The event Lamport clock at snapshot time.
  EventClock(LamportTime),
  /// The query Lamport clock at snapshot time.
  QueryClock(LamportTime),
  /// A cached Vivaldi coordinate for a node (requires `feature = "coordinates"`).
  #[cfg(feature = "coordinates")]
  #[cfg_attr(docsrs, doc(cfg(feature = "coordinates")))]
  Coordinate(CoordinateRecord<I, A>),
  /// A leave marker: the local node cleanly left before this snapshot was written.
  Leave,
  /// An informational comment record; ignored on replay.
  Comment,
}

impl<I, A> SnapshotRecord<I, A>
where
  I: Data,
  A: Data,
{
  /// Encode this record to a self-framed [`Bytes`] buffer.
  ///
  /// The returned buffer begins with the discriminant tag byte and is fully
  /// self-contained (the decoder does not need external framing).
  pub fn encode(&self) -> Result<Bytes, SnapshotError> {
    match self {
      Self::Alive(node) => encode_node(TAG_ALIVE, node),
      Self::NotAlive(node) => encode_node(TAG_NOT_ALIVE, node),
      Self::Clock(t) => Ok(encode_clock(TAG_CLOCK, *t)),
      Self::EventClock(t) => Ok(encode_clock(TAG_EVENT_CLOCK, *t)),
      Self::QueryClock(t) => Ok(encode_clock(TAG_QUERY_CLOCK, *t)),
      #[cfg(feature = "coordinates")]
      Self::Coordinate(rec) => encode_coordinate_record(rec),
      Self::Leave => Ok(Bytes::from_static(&[TAG_LEAVE])),
      Self::Comment => Ok(Bytes::from_static(&[TAG_COMMENT])),
    }
  }

  /// Decode one record from the front of `buf`.
  ///
  /// Returns `(record, bytes_consumed)`.  The caller advances its cursor by
  /// `bytes_consumed` to read the next record.
  ///
  /// # Errors
  /// - [`SnapshotError::Truncated`] — buffer ends inside the record body.
  /// - [`SnapshotError::UnknownTag`] — unrecognised discriminant byte.
  /// - [`SnapshotError::Decode`] — node-id or node-addr decode failure.
  pub fn decode(buf: &[u8]) -> Result<(Self, usize), SnapshotError> {
    let tag = *buf
      .first()
      .ok_or(SnapshotError::Truncated { need: 1, have: 0 })?;

    match tag {
      TAG_ALIVE => {
        let (node, n) = decode_node(&buf[1..])?;
        Ok((Self::Alive(node), 1 + n))
      }
      TAG_NOT_ALIVE => {
        let (node, n) = decode_node(&buf[1..])?;
        Ok((Self::NotAlive(node), 1 + n))
      }
      TAG_CLOCK => {
        let t = decode_clock(&buf[1..])?;
        Ok((Self::Clock(t), 9))
      }
      TAG_EVENT_CLOCK => {
        let t = decode_clock(&buf[1..])?;
        Ok((Self::EventClock(t), 9))
      }
      TAG_QUERY_CLOCK => {
        let t = decode_clock(&buf[1..])?;
        Ok((Self::QueryClock(t), 9))
      }
      TAG_COORDINATE => {
        #[cfg(feature = "coordinates")]
        {
          let (rec, n) = decode_coordinate_record(&buf[1..])?;
          Ok((Self::Coordinate(rec), 1 + n))
        }
        // When the coordinates feature is off, a coordinate record from a
        // coordinates-enabled snapshot cannot be decoded without knowing its
        // length.  The driver should not feed such snapshots to a
        // coordinates-disabled build.
        #[cfg(not(feature = "coordinates"))]
        Err(SnapshotError::UnknownTag(TAG_COORDINATE))
      }
      TAG_LEAVE => Ok((Self::Leave, 1)),
      TAG_COMMENT => Ok((Self::Comment, 1)),
      other => Err(SnapshotError::UnknownTag(other)),
    }
  }
}

// ── Encoding helpers ──────────────────────────────────────────────────────────

/// `[tag][node_len: u32 LE][node bytes]`
fn encode_node<I, A>(tag: u8, node: &memberlist_proto::Node<I, A>) -> Result<Bytes, SnapshotError>
where
  I: Data,
  A: Data,
{
  let node_len = node.encoded_len();
  let total = 1 + 4 + node_len;
  let mut buf = BytesMut::with_capacity(total);
  buf.put_u8(tag);
  buf.put_u32_le(node_len as u32);
  let prev = buf.len();
  buf.resize(prev + node_len, 0);
  let written = node.encode(&mut buf[prev..])?;
  buf.truncate(prev + written);
  Ok(buf.freeze())
}

/// `[tag][time: u64 LE]`
fn encode_clock(tag: u8, t: LamportTime) -> Bytes {
  let mut buf = [0u8; 9];
  buf[0] = tag;
  buf[1..9].copy_from_slice(&u64::from(t).to_le_bytes());
  Bytes::copy_from_slice(&buf)
}

/// `[TAG_COORDINATE][node_len: u32 LE][node bytes][dim: u32 LE][dim × f64 LE][error f64 LE][adj f64 LE][height f64 LE]`
#[cfg(feature = "coordinates")]
fn encode_coordinate_record<I, A>(rec: &CoordinateRecord<I, A>) -> Result<Bytes, SnapshotError>
where
  I: Data,
  A: Data,
{
  let node = &rec.node;
  let coord = &rec.coordinate;

  let node_len = node.encoded_len();
  let dim = coord.vec.len();
  // coord body: 4 (dim count) + dim*8 (f64s) + 8 (error) + 8 (adj) + 8 (height)
  let coord_body_len = 4 + dim * 8 + 8 + 8 + 8;
  let total = 1 + 4 + node_len + coord_body_len;

  let mut buf = BytesMut::with_capacity(total);
  buf.put_u8(TAG_COORDINATE);
  // node
  buf.put_u32_le(node_len as u32);
  let prev = buf.len();
  buf.resize(prev + node_len, 0);
  let written = node.encode(&mut buf[prev..])?;
  buf.truncate(prev + written);
  // coordinate: dim count + vec elements + error + adjustment + height
  buf.put_u32_le(dim as u32);
  for &v in &coord.vec {
    buf.put_f64_le(v);
  }
  buf.put_f64_le(coord.error);
  buf.put_f64_le(coord.adjustment);
  buf.put_f64_le(coord.height);
  Ok(buf.freeze())
}

// ── Decoding helpers ──────────────────────────────────────────────────────────

/// Decode `[node_len: u32 LE][node bytes]` from `buf`.
///
/// Returns `(node, bytes_consumed)` where consumed includes the 4-byte length prefix.
fn decode_node<I, A>(buf: &[u8]) -> Result<(memberlist_proto::Node<I, A>, usize), SnapshotError>
where
  I: Data,
  A: Data,
{
  if buf.len() < 4 {
    return Err(SnapshotError::Truncated {
      need: 4,
      have: buf.len(),
    });
  }
  let node_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
  let body_end = 4 + node_len;
  if buf.len() < body_end {
    return Err(SnapshotError::Truncated {
      need: body_end,
      have: buf.len(),
    });
  }
  let node_bytes = &buf[4..body_end];
  type NodeRef<'a, I, A> = memberlist_proto::Node<<I as Data>::Ref<'a>, <A as Data>::Ref<'a>>;
  let (consumed, node_ref) =
    <NodeRef<'_, I, A> as DataRef<'_, memberlist_proto::Node<I, A>>>::decode(node_bytes)
      .map_err(SnapshotError::Decode)?;
  if consumed != node_len {
    return Err(SnapshotError::Decode(
      memberlist_proto::data::DecodeError::custom(format!(
        "node decode consumed {consumed} of {node_len} bytes"
      )),
    ));
  }
  let node = memberlist_proto::Node::<I, A>::from_ref(node_ref).map_err(SnapshotError::Decode)?;
  Ok((node, 4 + node_len))
}

/// Decode a u64 LE lamport time from `buf[0..8]`.
fn decode_clock(buf: &[u8]) -> Result<LamportTime, SnapshotError> {
  if buf.len() < 8 {
    return Err(SnapshotError::Truncated {
      need: 8,
      have: buf.len(),
    });
  }
  let t = u64::from_le_bytes([
    buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
  ]);
  Ok(LamportTime::new(t))
}

/// Decode a coordinate record body (after the tag byte).
///
/// Layout: `[node_len: u32 LE][node bytes][dim: u32 LE][dim × f64 LE][error f64][adj f64][height f64]`
#[cfg(feature = "coordinates")]
fn decode_coordinate_record<I, A>(
  buf: &[u8],
) -> Result<(CoordinateRecord<I, A>, usize), SnapshotError>
where
  I: Data,
  A: Data,
{
  // Decode the node first.
  let (node, node_n) = decode_node(buf)?;
  let rest = &buf[node_n..];

  // Read dim count.
  if rest.len() < 4 {
    return Err(SnapshotError::Truncated {
      need: 4,
      have: rest.len(),
    });
  }
  let dim = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;

  // 4 (dim) + dim*8 + 8 (error) + 8 (adj) + 8 (height)
  let coord_tail = 4 + dim * 8 + 8 + 8 + 8;
  if rest.len() < coord_tail {
    return Err(SnapshotError::Truncated {
      need: coord_tail,
      have: rest.len(),
    });
  }

  let mut off = 4; // past the dim u32
  let mut vec = Vec::with_capacity(dim);
  for _ in 0..dim {
    let v = f64::from_le_bytes([
      rest[off],
      rest[off + 1],
      rest[off + 2],
      rest[off + 3],
      rest[off + 4],
      rest[off + 5],
      rest[off + 6],
      rest[off + 7],
    ]);
    vec.push(v);
    off += 8;
  }
  let error = f64::from_le_bytes([
    rest[off],
    rest[off + 1],
    rest[off + 2],
    rest[off + 3],
    rest[off + 4],
    rest[off + 5],
    rest[off + 6],
    rest[off + 7],
  ]);
  off += 8;
  let adjustment = f64::from_le_bytes([
    rest[off],
    rest[off + 1],
    rest[off + 2],
    rest[off + 3],
    rest[off + 4],
    rest[off + 5],
    rest[off + 6],
    rest[off + 7],
  ]);
  off += 8;
  let height = f64::from_le_bytes([
    rest[off],
    rest[off + 1],
    rest[off + 2],
    rest[off + 3],
    rest[off + 4],
    rest[off + 5],
    rest[off + 6],
    rest[off + 7],
  ]);
  off += 8;

  let coordinate = Coordinate {
    vec,
    error,
    adjustment,
    height,
  };
  Ok((CoordinateRecord::new(node, coordinate), node_n + off))
}

// ── ReplayResult ─────────────────────────────────────────────────────────────

/// The output of replaying a snapshot record stream.
///
/// Contains the set of alive nodes recovered from the snapshot, the three
/// Lamport clock high-water marks, and any coordinate records (when the
/// `coordinates` feature is enabled).
///
/// The driver reads the snapshot file into [`SnapshotRecord`] values (via
/// [`SnapshotRecord::decode`]) and calls [`ReplayResult::replay`]; serf's
/// `Endpoint::load_snapshot` then
/// applies the result to the machine state.  This type owns no file handles or
/// I/O state — it is a pure data carrier.
///
/// Mirrors the pure-record-folding logic in Go serf
/// `snapshot.go` `open_and_replay_snapshot` (lines 228-331).
#[derive(Debug, Clone)]
pub struct ReplayResult<I, A> {
  /// Nodes that were alive at the end of the snapshot, after applying all
  /// `Alive` / `NotAlive` / `Leave` records.
  ///
  /// When `rejoin_after_leave` is `false` and the snapshot ends with a
  /// `Leave` record, this list is empty (the leave cleared it).
  pub alive_nodes: Vec<memberlist_proto::Node<I, A>>,
  /// The member Lamport clock high-water mark seen in the snapshot.
  ///
  /// The `Endpoint` will set its member clock to at least this value + 1
  /// on `load_snapshot` (G5).
  pub last_clock: LamportTime,
  /// The event Lamport clock high-water mark seen in the snapshot.
  ///
  /// The `Endpoint` will set `event_buffer.min_time` to `last_event_clock + 1`
  /// so stale pre-snapshot events are not replayed after restart (G5).
  pub last_event_clock: LamportTime,
  /// The query Lamport clock high-water mark seen in the snapshot.
  ///
  /// The `Endpoint` will set `query_buffer.min_time` to `last_query_clock + 1`
  /// so stale pre-snapshot queries are not replayed after restart (G5).
  pub last_query_clock: LamportTime,
}

impl<I, A> ReplayResult<I, A>
where
  I: Eq + core::hash::Hash,
{
  /// Replay a flat sequence of [`SnapshotRecord`] values into a [`ReplayResult`].
  ///
  /// This is the pure fold that mirrors Go serf `snapshot.go`
  /// `open_and_replay_snapshot` (lines 265–331), but without any file I/O.
  /// The driver feeds decoded records; this function folds them into the
  /// recovered alive-node set and the three clock high-water marks.
  ///
  /// **G10 — rejoin-after-leave gating:**
  /// When a [`SnapshotRecord::Leave`] record is encountered:
  /// - If `rejoin_after_leave` is `true`, the record is **ignored** — the
  ///   alive-node set and clock floors are preserved so the node re-joins with
  ///   its previous membership intact.
  /// - If `rejoin_after_leave` is `false`, the alive-node set is **cleared**
  ///   and all three clocks are **reset to zero** — the node starts fresh and
  ///   does NOT automatically re-dial its previous peers.
  ///
  /// **Record semantics (mirrors oracle):**
  /// - `Alive(node)` — insert `node` into the alive set.
  /// - `NotAlive(node)` — remove `node` from the alive set.
  /// - `Clock(t)` — update `last_clock` to `t` (last wins).
  /// - `EventClock(t)` — update `last_event_clock` to `t` (last wins).
  /// - `QueryClock(t)` — update `last_query_clock` to `t` (last wins).
  /// - `Coordinate(_)` — ignored here; the driver caches coordinates
  ///   separately so the `CoordinateClient` can be warmed on restart.
  /// - `Leave` — apply the G10 gate described above.
  /// - `Comment` — ignored.
  pub fn replay(
    records: impl IntoIterator<Item = SnapshotRecord<I, A>>,
    rejoin_after_leave: bool,
  ) -> Self
  where
    A: Eq + core::hash::Hash + Clone,
    I: Clone,
  {
    // `alive_vec` preserves snapshot record order (first-seen wins for
    // insertion position); `alive_set` gives O(1) membership tests and
    // drives dedup so each node appears at most once.  Using a Vec here
    // rather than a HashSet ensures `alive_nodes` is emitted in a stable,
    // input-record order — two drivers that replay the same snapshot bytes
    // produce an identical `DialRequested` sequence.
    let mut alive_vec: Vec<memberlist_proto::Node<I, A>> = Vec::new();
    let mut alive_set: crate::FxHashSet<memberlist_proto::Node<I, A>> = crate::FxHashSet::default();
    let mut last_clock = LamportTime::ZERO;
    let mut last_event_clock = LamportTime::ZERO;
    let mut last_query_clock = LamportTime::ZERO;

    for record in records {
      match record {
        SnapshotRecord::Alive(node) => {
          if alive_set.insert(node.clone()) {
            alive_vec.push(node);
          }
        }
        SnapshotRecord::NotAlive(node) => {
          if alive_set.remove(&node) {
            alive_vec.retain(|n| n != &node);
          }
        }
        SnapshotRecord::Clock(t) => {
          last_clock = t;
        }
        SnapshotRecord::EventClock(t) => {
          last_event_clock = t;
        }
        SnapshotRecord::QueryClock(t) => {
          last_query_clock = t;
        }
        SnapshotRecord::Leave => {
          if rejoin_after_leave {
            // G10: ignore the leave record — preserve state for rejoin.
            // Go serf logs "ignoring previous leave in snapshot" here.
          } else {
            // G10: clear everything — the node left and should not auto-rejoin.
            alive_vec.clear();
            alive_set.clear();
            last_clock = LamportTime::ZERO;
            last_event_clock = LamportTime::ZERO;
            last_query_clock = LamportTime::ZERO;
          }
        }
        SnapshotRecord::Comment => {
          // Informational only; skip.
        }
        // Coordinate records are driver-side concerns; the machine does not
        // consume them here (the `CoordinateClient` is warmed by the driver
        // calling `get_coordinate` / `set_coordinate` after replay).
        #[cfg(feature = "coordinates")]
        SnapshotRecord::Coordinate(_) => {}
      }
    }

    Self {
      alive_nodes: alive_vec,
      last_clock,
      last_event_clock,
      last_query_clock,
    }
  }
}

#[cfg(test)]
mod tests;
