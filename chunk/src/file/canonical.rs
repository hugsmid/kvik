//! The canonical checks (`docs/format-compacted.md` B.1): the writer's read-back, golden files and the scrub. The
//! file is read whole through the reader's checks, written again from what it holds with the same registry state,
//! and must come out byte for byte; a data block in the XOR class keeps the file's own compressed payload where
//! that is a canonical body of the same rows (`docs/format-chunk.md` C.10).

use std::collections::BTreeMap;

use super::error::FileError;
use super::layout::Identity;
use super::read::CompactedFile;
use super::stats::Reach;
use super::write::{Fact, FileWriter, Series, Tweaks, WriteError};
use super::{Position, parts::SET_ASIDE};

/// What the registry says about a series at a file's greatest position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesInfo {
    /// Its primary name; `None` for a series with no definition.
    pub name: Option<String>,
    /// The reach of its readings.
    pub reach: Reach,
}

/// Why a file fails the canonical checks.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    /// It fails a reader's check.
    #[error(transparent)]
    File(#[from] FileError),
    /// What it holds breaks a writer's rule.
    #[error("the content cannot be written: {0}")]
    Content(#[from] WriteError),
    /// A position held twice, or an entry a v1 writer never writes.
    #[error("a position held twice or an ignored entry")]
    Positions,
    /// Its bytes differ from the canonical ones from this offset.
    #[error("not canonical from offset {0}")]
    Bytes(u64),
}

/// Applies the canonical checks to a whole compacted segment, with `registry` giving each series' name and reach.
pub fn check_canonical(
    bytes: &[u8],
    id: &Identity,
    registry: &dyn Fn(u32) -> SeriesInfo,
) -> Result<(), CanonicalError> {
    let file = CompactedFile::open(bytes, id, None)?;
    let mut index = file.index()?;
    index.sort_by_key(|e| e.chunk_offset);
    let mut bodies = Vec::new();
    let mut series: BTreeMap<u32, (Vec<Fact>, Vec<Fact>)> = BTreeMap::new();
    let mut positions: Vec<Position> = Vec::new();
    let batches = file.batches()?;
    for e in &index {
        let d = file.directory(e)?;
        let s = series.entry(e.series_id).or_default();
        let facts = if e.flags & SET_ASIDE != 0 {
            &mut s.1
        } else {
            &mut s.0
        };
        for f in file.read_chunk(e)? {
            positions.push(f.fact.position);
            facts.push(f.fact);
        }
        for i in 0..d.blocks.len() {
            bodies.push(file.data_block_raw(e, &d, i)?.1);
        }
    }
    let mut entries = Vec::new();
    for r in file.entries()? {
        if matches!(r.entry, kvik_log::entries::Entry::Ignored { .. }) {
            return Err(CanonicalError::Positions);
        }
        positions.push(r.position);
        entries.push((r.position, r.entry));
    }
    positions.sort_unstable();
    if !positions.windows(2).all(|w| w[0] < w[1]) {
        return Err(CanonicalError::Positions);
    }
    let mut ordered: Vec<Series> = series
        .into_iter()
        .map(|(series_id, (facts, set_aside))| {
            let info = registry(series_id);
            Series {
                series_id,
                name: info.name,
                reach: info.reach,
                facts,
                set_aside,
            }
        })
        .collect();
    ordered.sort_by(|a, b| {
        let key = |s: &Series| (s.name.is_none(), s.name.clone(), s.series_id);
        key(a).cmp(&key(b))
    });
    let tweaks = Tweaks {
        bodies,
        ..Tweaks::default()
    };
    let mut w = FileWriter::new(
        Vec::with_capacity(bytes.len()),
        *id,
        file.header().created_at,
        file.description(),
        batches,
    )?
    .with_tweaks(tweaks);
    for s in &ordered {
        w.series(s)?;
    }
    w.entries(&entries)?;
    let (again, _) = w.finish()?;
    match again.iter().zip(bytes).position(|(a, b)| a != b) {
        None if again.len() == bytes.len() => Ok(()),
        None => Err(CanonicalError::Bytes(again.len().min(bytes.len()) as u64)),
        Some(at) => Err(CanonicalError::Bytes(at as u64)),
    }
}
