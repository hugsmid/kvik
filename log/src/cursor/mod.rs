//! Cursor files (`format.md` A.15, `spec.md` §4 Consumer state): a consumer's durable cursor, the latch of each of its
//! condition funnels and the last reset point it confirmed, in `cursors/<name>.cur`.
//!
//! A cursor file is an A.1 header (kind 4) followed by blocks of type `0x30`, appended, one per cursor sync, each
//! preceded in the same write by a gate checkpoint (`0x31`) when one is written. The state is the last valid block
//! of each type: a torn last block leaves the previous, older state, never one ahead of a confirmation. A file with no valid header or no valid block is unreadable. Only the writer opens cursor files;
//! once a file passes 16 MiB it is written again under `<name>.cur.new` with the current state, synced and renamed
//! over the old one.
//!
//! ```
//! use kvik_log::cursor::{CursorState, decode, encode_block, encode_header};
//! use kvik_log::writer::Position;
//!
//! let state = CursorState {
//!     consumer: "alarms".into(),
//!     position: Position { generation: 0, sequence: 42 },
//!     latches: Vec::new(),
//!     reset: None,
//!     gate: None,
//! };
//! let mut bytes = encode_header([1; 16], 0, 5).to_vec();
//! bytes.extend(encode_block(&state).unwrap());
//! assert_eq!(decode(&bytes, &[1; 16]).unwrap().state, state);
//! ```

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::block::{BLOCK_HEADER_LEN, BlockHeader, encode_block as frame};
use crate::bytes::{u16_at, u32_at, u64_at};
use crate::crc::crc32c;
use crate::entries::consumer_name_ok;
use crate::error::{Element, EncodeError, FormatError};
use crate::fs::{Fs, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::store::read_all;
use crate::writer::Position;
use crate::writer::sources::{PositionSource, SourceContext, SourceReport};

mod checkpoint;
#[cfg(test)]
mod tests;

pub use checkpoint::{
    CHECKPOINT_BLOCK, Checkpoint, CheckpointFact, CheckpointInstance, CheckpointSeries,
    CheckpointState, CheckpointValue, GATE_POINT_LEN, GateCursor, GateId,
    decode_body as decode_checkpoint, encode_body as encode_checkpoint,
};

/// The directory cursor files live in (`format.md` A.12).
pub const CURSORS_DIR: &str = "cursors";
/// The block type of a cursor state.
pub const CURSOR_BLOCK: u8 = 0x30;
/// The size past which a cursor file is written again (A.15).
pub const REWRITE_AT: u64 = 16 << 20;

/// The latched state of a condition funnel (field 2, `spec.md` §6 Latch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LatchState {
    /// 0 or 1: a threshold, held-for or stale-for condition, false or true.
    Active(bool),
    /// 2 or 3: an on-change condition's last emitted value, none before any.
    Value(Option<f64>),
}

/// A condition funnel's latch (field 2): the last state its consumer confirmed and where it was emitted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Latch {
    /// The funnel id.
    pub funnel: u32,
    /// The funnel's incarnation: the position of the routing change that added it.
    pub incarnation: Position,
    /// The position the delivery that carried it was emitted at.
    pub emitted: Position,
    /// The state.
    pub state: LatchState,
}

/// A consumer's durable state, as one block holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorState {
    /// Field 1: the consumer's name (A.12).
    pub consumer: String,
    /// The last confirmed position.
    pub position: Position,
    /// Field 2 (repeated, in funnel order): latches.
    pub latches: Vec<Latch>,
    /// Field 3: the id of the last reset point the consumer confirmed, if any (`spec.md` §4 Resync).
    pub reset: Option<Position>,
    /// Field 4: the gate cursor, present once the consumer has gates (`spec.md` §6 Gates, Resume).
    pub gate: Option<GateCursor>,
}

impl CursorState {
    /// The highest position the state names: its cursor, its latches' emission and incarnation positions and its
    /// reset point (a source of F, `spec.md` §3 Recovery rule 3).
    pub fn highest(&self) -> Position {
        self.latches
            .iter()
            .flat_map(|l| [l.incarnation, l.emitted])
            .chain(self.reset)
            .chain(self.gate.and_then(|g| g.position()))
            .fold(self.position, Position::max)
    }
}

/// A decoded cursor file.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorFile {
    /// The header's generation.
    pub generation: u32,
    /// The state of the last valid `0x30` block.
    pub state: CursorState,
    /// The last valid `0x31` block.
    pub checkpoint: Option<Checkpoint>,
    /// The offset after the last valid block of either type: where the next block goes.
    pub end: u64,
}

impl CursorFile {
    /// The highest position the file names: its state's and its checkpoint's.
    pub fn highest(&self) -> Position {
        self.checkpoint
            .as_ref()
            .map(Checkpoint::highest)
            .map_or(self.state.highest(), |c| c.max(self.state.highest()))
    }
}

/// Why a cursor file cannot be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CursorError {
    /// No valid header or no valid block: the file is unreadable (`spec.md` §3 Recovery rule 3).
    #[error("unreadable cursor file")]
    Unreadable,
    /// A newer major version or an unknown critical element in a valid block: a version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

/// The file name of a consumer's cursor file.
pub fn file_name(consumer: &str) -> String {
    format!("{consumer}.cur")
}

/// The path of a consumer's cursor file in the store in `store_dir`.
pub fn path(store_dir: &Path, consumer: &str) -> PathBuf {
    store_dir.join(CURSORS_DIR).join(file_name(consumer))
}

/// The consumer a cursor file name belongs to, if it is one.
pub fn parse_name(name: &str) -> Option<&str> {
    name.strip_suffix(".cur").filter(|c| consumer_name_ok(c))
}

/// A cursor file's header (`file_id` 0, the generation at creation).
pub fn encode_header(store_id: [u8; 16], generation: u32, created_at: i64) -> [u8; HEADER_LEN] {
    FileHeader::new(kind::CURSOR, generation, 0, created_at, store_id).encode()
}

pub(crate) fn put_pos(out: &mut Vec<u8>, p: Position) {
    out.extend_from_slice(&p.generation.to_le_bytes());
    out.extend_from_slice(&p.sequence.to_le_bytes());
}

pub(crate) fn pos_at(b: &[u8], at: usize) -> Position {
    Position {
        generation: u32_at(b, at),
        sequence: u64_at(b, at + 4),
    }
}

fn field(out: &mut Vec<u8>, id: u16, v: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
    out.extend_from_slice(v);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

/// The length of a latch field's value.
const LATCH_LEN: usize = 40;

/// One block holding `state`, framed (A.2).
pub fn encode_block(state: &CursorState) -> Result<Vec<u8>, EncodeError> {
    if !consumer_name_ok(&state.consumer) {
        return Err(EncodeError::TooLarge);
    }
    let mut body = Vec::new();
    put_pos(&mut body, state.position);
    body.extend_from_slice(&[0; 4]);
    field(&mut body, 1, state.consumer.as_bytes());
    for l in &state.latches {
        let mut v = Vec::with_capacity(LATCH_LEN);
        v.extend_from_slice(&l.funnel.to_le_bytes());
        put_pos(&mut v, l.incarnation);
        put_pos(&mut v, l.emitted);
        let (code, value) = match l.state {
            LatchState::Active(false) => (0u8, 0f64),
            LatchState::Active(true) => (1, 0.0),
            LatchState::Value(None) => (2, 0.0),
            LatchState::Value(Some(x)) => (3, x),
        };
        v.extend_from_slice(&[code, 0, 0, 0]);
        v.extend_from_slice(&value.to_bits().to_le_bytes());
        field(&mut body, 2, &v);
    }
    if let Some(r) = state.reset {
        let mut v = Vec::new();
        put_pos(&mut v, r);
        field(&mut body, 3, &v);
    }
    if let Some(g) = &state.gate {
        let mut v = Vec::with_capacity(GATE_POINT_LEN);
        checkpoint::put_point(&mut v, g);
        field(&mut body, 4, &v);
    }
    let mut out = Vec::new();
    frame(CURSOR_BLOCK, 0, &body, &mut out)?;
    Ok(out)
}

/// A block body's state; `None` if it fails a check, a version refusal as an error.
fn decode_body(body: &[u8], offset: u64) -> Result<Option<CursorState>, CursorError> {
    if body.len() < 16 {
        return Ok(None);
    }
    let position = pos_at(body, 0);
    let mut at = 16;
    let mut consumer = None;
    let mut latches = Vec::new();
    let mut reset = None;
    let mut gate = None;
    let mut prev = 0u16;
    while at < body.len() {
        let Some(h) = body.get(at..at + 4) else {
            return Ok(None);
        };
        let (id, len) = (u16_at(h, 0), usize::from(u16_at(h, 2)));
        let Some(v) = body.get(at + 4..at + 4 + len) else {
            return Ok(None);
        };
        at = (at + 4 + len).div_ceil(4) * 4;
        if id >= 0x8000 {
            continue;
        }
        if id < prev || (id == prev && id != 2) {
            return Ok(None);
        }
        prev = id;
        match id {
            1 => match std::str::from_utf8(v) {
                Ok(s) if consumer_name_ok(s) => consumer = Some(s.to_owned()),
                _ => return Ok(None),
            },
            2 => {
                if v.len() != LATCH_LEN {
                    return Ok(None);
                }
                let value = f64::from_bits(u64_at(v, 32));
                let state = match v[28] {
                    0 => LatchState::Active(false),
                    1 => LatchState::Active(true),
                    2 => LatchState::Value(None),
                    3 if value.is_finite() => LatchState::Value(Some(value)),
                    _ => return Ok(None),
                };
                latches.push(Latch {
                    funnel: u32_at(v, 0),
                    incarnation: pos_at(v, 4),
                    emitted: pos_at(v, 16),
                    state,
                });
            }
            3 => {
                if v.len() != 12 {
                    return Ok(None);
                }
                reset = Some(pos_at(v, 0));
            }
            4 => {
                if v.len() != GATE_POINT_LEN {
                    return Ok(None);
                }
                match checkpoint::point_at(v) {
                    Some(g) => gate = Some(g),
                    None => return Ok(None),
                }
            }
            _ => {
                return Err(CursorError::Unsupported(FormatError::Unsupported {
                    offset,
                    element: Element::Field {
                        entry_type: 0,
                        field: id,
                    },
                }));
            }
        }
    }
    Ok(consumer.map(|consumer| CursorState {
        consumer,
        position,
        latches,
        reset,
        gate,
    }))
}

/// Reads a cursor file: its last valid block of each type. Blocks after the first invalid one are not read (a torn
/// append).
pub fn decode(bytes: &[u8], store_id: &[u8; 16]) -> Result<CursorFile, CursorError> {
    let h = FileHeader::decode(bytes).ok_or(CursorError::Unreadable)?;
    if h.format_major > crate::FORMAT_MAJOR {
        return Err(CursorError::Unsupported(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(h.format_major),
        }));
    }
    if h.kind != kind::CURSOR || &h.store_id != store_id {
        return Err(CursorError::Unreadable);
    }
    let mut at = HEADER_LEN;
    let mut last = None;
    let mut checkpoint = None;
    let mut end = None;
    while let Some(head) = bytes
        .get(at..at + BLOCK_HEADER_LEN)
        .and_then(|b| <&[u8; BLOCK_HEADER_LEN]>::try_from(b).ok())
    {
        let Some(bh) = BlockHeader::decode(head) else {
            break;
        };
        let start = at + BLOCK_HEADER_LEN;
        let Some(body) = bytes.get(start..start + bh.body_len as usize) else {
            break;
        };
        if crc32c(body) != bh.body_crc32c {
            break;
        }
        let next = at + bh.block_len() as usize;
        match bh.block_type {
            CURSOR_BLOCK => match decode_body(body, at as u64)? {
                Some(s) => {
                    last = Some(s);
                    end = Some(next);
                }
                None => break,
            },
            CHECKPOINT_BLOCK => match checkpoint::decode_body(body) {
                Some(c) => {
                    checkpoint = Some(c);
                    end = Some(next);
                }
                None => break,
            },
            _ if bh.skippable() => {}
            t => {
                return Err(CursorError::Unsupported(FormatError::Unsupported {
                    offset: at as u64,
                    element: Element::BlockType(t),
                }));
            }
        }
        at = next;
    }
    let (state, end) = last.zip(end).ok_or(CursorError::Unreadable)?;
    Ok(CursorFile {
        generation: h.generation,
        state,
        checkpoint,
        end: end as u64,
    })
}

/// Reads a consumer's cursor file: `Ok(None)` when there is none.
pub fn read(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
    consumer: &str,
) -> Result<Option<CursorFile>, CursorError> {
    match read_all(fs, &path(store_dir, consumer)) {
        Ok(bytes) => decode(&bytes, store_id).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(CursorError::Unreadable),
    }
}

/// Every cursor file of the store: consumer names with what reading each gave. A directory that cannot be listed
/// gives none.
pub fn read_all_cursors(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
) -> Vec<(String, Result<CursorFile, CursorError>)> {
    let dir = store_dir.join(CURSORS_DIR);
    let Ok(entries) = fs.read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| parse_name(&e.name).map(str::to_owned))
        .map(|c| {
            let r = match read(fs, store_dir, store_id, &c) {
                Ok(Some(f)) => Ok(f),
                Ok(None) => Err(CursorError::Unreadable),
                Err(e) => Err(e),
            };
            (c, r)
        })
        .collect()
}

/// Appends `state` to a consumer's cursor file and syncs it (A.15). `end` is where the last valid block ends, as
/// [`decode`] or the previous append gave it, or `None` when the file does not exist yet or is unreadable: it is then
/// created whole (an unreadable one is replaced), synced, and the directory synced. A file past 16 MiB is written
/// again under `<name>.cur.new`, synced, renamed over the old one, and the directory synced. Returns the new end.
///
/// The caller makes sure the state names no position above the last synced mark (`spec.md` §3 Durable mark).
pub fn append(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: [u8; 16],
    generation: u32,
    created_at: i64,
    state: &CursorState,
    end: Option<u64>,
) -> io::Result<u64> {
    let rec = Record {
        state,
        checkpoint: None,
        fresh: false,
    };
    append_record(fs, store_dir, store_id, generation, created_at, rec, end)
}

/// What one cursor write holds.
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    /// The cursor state.
    pub state: &'a CursorState,
    /// The consumer's checkpoint: a new one, or the one the file holds, carried into a file written again.
    pub checkpoint: Option<&'a Checkpoint>,
    /// `checkpoint` is new: it is appended, before the state, in the same write and sync.
    pub fresh: bool,
}

/// [`append`] with a checkpoint (A.15): a fresh one goes before the state block in one write and one sync; a file
/// created or written again holds the checkpoint whether fresh or carried.
pub fn append_record(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: [u8; 16],
    generation: u32,
    created_at: i64,
    rec: Record<'_>,
    end: Option<u64>,
) -> io::Result<u64> {
    let state = rec.state;
    let mut ck = Vec::new();
    if let Some(c) = rec.checkpoint {
        let body = checkpoint::encode_body(c)
            .ok_or_else(|| io::Error::other("a checkpoint value cannot be written"))?;
        frame(CHECKPOINT_BLOCK, 0, &body, &mut ck).map_err(io::Error::other)?;
    }
    let cursor = encode_block(state).map_err(io::Error::other)?;
    let mut block = if rec.fresh { ck.clone() } else { Vec::new() };
    block.extend_from_slice(&cursor);
    let dir = store_dir.join(CURSORS_DIR);
    let target = path(store_dir, &state.consumer);
    match end {
        Some(end) if end + (block.len() as u64) <= REWRITE_AT => {
            let f = fs.open(&target, OpenMode::ReadWrite)?;
            f.write_at(&block, end)?;
            f.sync_data()?;
            Ok(end + block.len() as u64)
        }
        _ => {
            let created = fs.read_dir(&dir).is_err();
            fs.create_dir_all(&dir)?;
            if created {
                fs.sync_dir(store_dir)?;
            }
            let mut bytes = encode_header(store_id, generation, created_at).to_vec();
            bytes.extend_from_slice(&ck);
            bytes.extend_from_slice(&cursor);
            let new = dir.join(format!("{}.new", file_name(&state.consumer)));
            let _ = fs.remove_file(&new);
            let f = fs.open(&new, OpenMode::CreateNew)?;
            f.write_at(&bytes, 0)?;
            f.sync_all()?;
            drop(f);
            fs.rename(&new, &target)?;
            fs.sync_dir(&dir)?;
            Ok(bytes.len() as u64)
        }
    }
}

/// Cursor files as a source of F (`spec.md` §3 Recovery rule 3): the highest position any of them names, and the
/// consumers whose cursor file is missing (a consumer named by a routing change the start has seen) or unreadable,
/// for the recovery entry's field 47. A cursor file with an unknown critical element stops the start (A.15).
#[derive(Clone, Copy, Debug, Default)]
pub struct CursorSource;

impl PositionSource for CursorSource {
    fn kind(&self) -> &'static str {
        "cursor files"
    }

    fn read(&self, ctx: &SourceContext<'_>) -> Result<SourceReport, FormatError> {
        let mut report = SourceReport::default();
        let mut found = BTreeSet::new();
        let mut named = BTreeSet::new();
        for (consumer, r) in read_all_cursors(ctx.fs, ctx.store_dir, &ctx.store_id) {
            match r {
                Ok(f) => {
                    let h = f.highest();
                    if h.generation == ctx.generation {
                        report.highest = report.highest.max(h.sequence);
                    }
                }
                Err(CursorError::Unsupported(e)) => return Err(e),
                Err(CursorError::Unreadable) => {
                    named.insert(consumer.clone());
                }
            }
            found.insert(consumer);
        }
        named.extend(ctx.consumers.difference(&found).cloned());
        report.named = named.into_iter().collect();
        Ok(report)
    }
}
