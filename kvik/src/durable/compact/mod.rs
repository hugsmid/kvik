//! Compaction of sealed log segments into compacted segments, level L0 (`spec.md` §7; roadmap 6.2a2): the planner,
//! the run and its verification. Outputs are left unpublished, as `.tmp` files under `data/`; publishing them is
//! roadmap 6.3e, and the governor, the stepping and the memory cap are roadmap 6.2a2b (Q-1410).

mod plan;
mod run;
mod verify;

#[cfg(test)]
mod tests;

use std::io;
use std::path::PathBuf;

use kvik_chunk::file::{Fact, FileError, Identity, Layout, Position, Reach, WriteError};
use kvik_log::entries::{Definition, IntervalLength, Kind};
use kvik_log::facts::{FactRow, Value};
use sha2::{Digest, Sha256};

pub(crate) use verify::VerifyError;

/// The file kind of a log segment, as a description names its inputs (`format.md` A.1).
const KIND_LOG: u8 = 1;
/// The most inputs one run takes: a description names at most this many (`format-compacted.md` B.1).
const MAX_RUN_INPUTS: usize = kvik_chunk::file::MAX_INPUTS;

/// A segment compaction cannot take whole, which stops compaction of log segments at it (Q-799, Q-809).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stall {
    /// The segment's generation.
    pub(crate) generation: u32,
    /// The segment's `file_id`.
    pub(crate) file_id: u64,
    /// Why.
    pub(crate) cause: Cause,
}

/// Why compaction stalls at a segment (`compaction_behind`'s cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// Not sealed whole, damaged, or disagreeing with its own seal.
    Unreadable(&'static str),
    /// An element this binary does not know, or bytes it would not write: rearranging would drop something (Q-809).
    Unknown,
    /// A batch whose commit time is below an earlier batch's of its generation (Q-811 (e)).
    CommitTimeDecreases,
    /// Its positions are partly at or below C(g): it is not a whole input (§3.1).
    Straddles,
    /// A non-fact entry set aside beyond a restored end, which no compacted file holds yet (Q-1413).
    SetAsideEntry,
    /// A time whose UTC period leaves `i64`.
    PeriodOutOfRange,
}

impl Cause {
    /// Whether no repair, returning cold path or newer binary ends the stall: a period leaving `i64` (Q-1520) and
    /// commit times decreasing inside one segment (Q-1521).
    pub(crate) fn permanent(self) -> bool {
        matches!(self, Cause::PeriodOutOfRange | Cause::CommitTimeDecreases)
    }

    fn health(self) -> &'static str {
        if self.permanent() { " (permanent)" } else { "" }
    }
}

/// Why a run produced no verified outputs; the inputs are untouched in every case.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// Compaction stalls at a segment.
    #[error("compaction stalled at segment {}/{}: {:?}{}", .0.generation, .0.file_id, .0.cause, .0.cause.health())]
    Stalled(Stall),
    /// A position met twice in the inputs with differing content (§3.1, as §8 Restore step 3).
    #[error("position {0:?} met twice with differing content")]
    Conflict(Position),
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The writer refused the rows.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// An output failed a check of §3.2; it and every other output were removed.
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

impl From<FileError> for RunError {
    fn from(e: FileError) -> Self {
        RunError::Verify(VerifyError::Read(e))
    }
}

/// One verified, unpublished output of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Output {
    /// Its identity.
    pub(crate) id: Identity,
    /// What a publish record will say about it.
    pub(crate) layout: Layout,
    /// Its length in bytes.
    pub(crate) len: u64,
    /// Its plaintext SHA-256, for its description (§3.2 step 5).
    pub(crate) sha256: [u8; 32],
    /// Where it lies, under its `.tmp` name.
    pub(crate) path: PathBuf,
}

/// A series' name and reach for a compacted file, from the definition in force at the file's greatest position.
pub(crate) fn series_info(def: &Definition) -> (Option<String>, Reach) {
    let reach = match (&def.kind, &def.interval) {
        (Kind::Interval, Some(IntervalLength::Fixed { length, .. })) => Reach::Fixed(*length),
        (Kind::Interval, Some(IntervalLength::Day { .. })) => Reach::Day,
        (Kind::Interval, Some(IntervalLength::Month { .. })) => Reach::Month,
        _ => Reach::Point,
    };
    (Some(def.name.clone()), reach)
}

/// A log fact row as a compacted segment holds it (`format-compacted.md` B.1, `format.md` A.5).
pub(crate) fn chunk_fact(row: &FactRow, position: Position) -> Fact {
    let (value, text, scale) = match &row.value {
        Value::F64(bits) => (*bits, Vec::new(), 0),
        Value::Decimal { units, scale } => (*units as u64, Vec::new(), *scale),
        Value::State(s) => (u64::from(*s), Vec::new(), 0),
        Value::Bool(b) => (u64::from(*b), Vec::new(), 0),
        Value::Text(s) => (s.len() as u64, s.as_bytes().to_vec(), 0),
        Value::Absent { scale, .. } => (0, Vec::new(), *scale),
    };
    Fact {
        series_id: row.series_id,
        position,
        t: row.t,
        quality: row.quality as u8,
        flags: row.flag_bits(),
        value_type: row.value.value_type().code(),
        value,
        text,
        changed_at: row.changed_at.unwrap_or(0),
        scale,
    }
}

/// The digest of one fact for the per-series checksum (§3.2 step 2, Q-800): the first 16 bytes of the SHA-256 of
/// its tuple. The function is provisional until 6.2a2b measures its cost (Q-1410).
pub(crate) fn fact_digest(f: &Fact, received_at: i64, commit_time: i64) -> u128 {
    let mut h = Sha256::new();
    h.update(f.position.0.to_le_bytes());
    h.update(f.position.1.to_le_bytes());
    h.update(f.t.to_le_bytes());
    h.update(received_at.to_le_bytes());
    h.update(commit_time.to_le_bytes());
    h.update([f.quality]);
    h.update(f.flags.to_le_bytes());
    h.update([f.value_type]);
    h.update(f.value.to_le_bytes());
    h.update((f.text.len() as u64).to_le_bytes());
    h.update(&f.text);
    h.update(f.changed_at.to_le_bytes());
    h.update([f.scale]);
    first_16(h)
}

/// The digest of one non-fact entry: its A.6 bytes, position, `received_at` and commit time.
pub(crate) fn entry_digest(
    p: Position,
    entry: &kvik_log::entries::Entry,
    received_at: i64,
    commit_time: i64,
) -> Result<u128, kvik_log::EncodeError> {
    let body = kvik_log::entries::EntriesBlock {
        first_sequence: p.1,
        received_at,
        entries: vec![entry.clone()],
    }
    .encode()?;
    let mut h = Sha256::new();
    h.update(p.0.to_le_bytes());
    h.update(commit_time.to_le_bytes());
    h.update((body.len() as u64).to_le_bytes());
    h.update(&body);
    Ok(first_16(h))
}

fn first_16(h: Sha256) -> u128 {
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    u128::from_le_bytes(b)
}

/// The UTC period of a time: its day, or its week when that week is merged or the day ended more than 14 days
/// before the run's last commit time (§3.1 Partitioning). `None` when the period leaves `i64`.
pub(crate) fn period(
    t: i64,
    last_commit_time: i64,
    weeks_merged: &std::collections::BTreeSet<i64>,
) -> Option<(i64, i64)> {
    const DAY: i64 = 86_400_000_000_000;
    let day = t.div_euclid(DAY);
    let week = day.div_euclid(7);
    let ended = day.checked_add(15)?.checked_mul(DAY);
    let old = ended.is_some_and(|e| e < last_commit_time);
    let len = if old || weeks_merged.contains(&week) {
        7 * DAY
    } else {
        DAY
    };
    let start = t.div_euclid(len).checked_mul(len)?;
    start.checked_add(len).map(|_| (start, len))
}
