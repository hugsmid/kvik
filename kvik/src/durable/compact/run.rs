//! The run (§3.1): read whole sealed segments, take every position once, place each row by period and class, write
//! one current and at most one revision file per period, then verify (§3.2). [`RunTask`] does it a step at a time
//! within a memory cap; [`run`] drives it to its end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kvik_chunk::file::{Fact, Identity, Position, Reach};
use kvik_chunk::flag;
use kvik_log::entries::Definition;
use kvik_log::fs::Fs;
use kvik_log::setaside::SetAside;

use super::plan::Candidate;
use super::task::RunTask;
use super::{Output, RunError, series_info};

/// The directory compacted segments are written to (`format-compacted.md` B.1 Name).
pub(crate) const DATA_DIR: &str = "data";
/// `maintenance.memory` on the minimal class (compaction design §4 Memory).
pub(crate) const MINIMAL_MEMORY: u64 = 32 << 20;

/// What a run needs besides its inputs.
#[derive(Clone, Copy)]
pub(crate) struct RunInput<'a> {
    /// The store's file system.
    pub(crate) fs: &'a dyn Fs,
    /// The store directory.
    pub(crate) dir: &'a Path,
    /// The store's id.
    pub(crate) store_id: [u8; 16],
    /// The generation the outputs are written in.
    pub(crate) generation: u32,
    /// The `file_id` of the first output; the others follow it.
    pub(crate) first_file_id: u64,
    /// The outputs' `created_at`.
    pub(crate) created_at: i64,
    /// Positions set aside beyond a restored end (§8 Restore step 1).
    pub(crate) set_aside: &'a SetAside,
    /// Weeks whose merge is published: their rows go to week files (§3.1 Partitioning).
    pub(crate) weeks_merged: &'a BTreeSet<i64>,
    /// The definition of a series in force below the run's first position.
    pub(crate) registry: &'a (dyn Fn(u32) -> Option<Definition> + Sync),
}

pub(crate) fn read_whole(f: &dyn kvik_log::fs::FsFile) -> std::io::Result<Vec<u8>> {
    let len = usize::try_from(f.len()?).map_err(|_| std::io::Error::other("file too large"))?;
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = f.read_at(&mut buf[done..], done as u64)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(buf)
}

/// Runs compaction over `segments` (from [`super::plan`]) to its end within the minimal class's memory and returns
/// the verified, unpublished outputs.
pub(crate) fn run(input: &RunInput<'_>, segments: &[Candidate]) -> Result<Vec<Output>, RunError> {
    let mut task = RunTask::new(*input, segments.to_vec(), MINIMAL_MEMORY);
    while !task.step_run() {}
    task.outcome().expect("ended")
}

/// A fact's rank within its key (§3.1 Class): `(changed_at, position)` on a row flagged `has_changed_at`, else the
/// position.
pub(super) fn rank(f: &Fact) -> (u64, Position) {
    let changed = if f.flags & flag::HAS_CHANGED_AT != 0 {
        f.changed_at
    } else {
        0
    };
    (changed, f.position)
}

/// The data directory's path for an output under its `.tmp` name.
pub(super) fn tmp_path(dir: &Path, id: &Identity) -> PathBuf {
    dir.join(DATA_DIR)
        .join(format!("{:08x}-{:016x}.kvc.tmp", id.generation, id.file_id))
}

/// The name and reach of a series in a file whose greatest position is `greatest`: from the last definition met in
/// the inputs at or below it, else from the registry below the run (Q-1412).
pub(super) fn info(
    input: &RunInput<'_>,
    defs: &BTreeMap<u32, Vec<(Position, Definition)>>,
    series: u32,
    greatest: Position,
) -> (Option<String>, Reach) {
    let met = defs.get(&series).and_then(|d| {
        d.iter()
            .rev()
            .find(|(p, _)| *p <= greatest)
            .map(|(_, d)| d.clone())
    });
    met.or_else(|| (input.registry)(series))
        .map_or((None, Reach::Point), |d| series_info(&d))
}
