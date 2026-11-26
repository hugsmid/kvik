//! Verification of a run's outputs (§3.2): each read back from the device and checked for counts against the
//! sealer's, per-series checksums, placement and derived content, and hashed for its description.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

use kvik_chunk::file::{
    CanonicalError, Class, CompactedFile, FileError, Identity, Layout, Position, Reach, SeriesInfo,
    check_canonical,
};
use kvik_log::fs::OpenMode;
use sha2::{Digest, Sha256};

use super::run::{Inputs, RunInput, Taken, rank, read_whole};
use super::{Output, entry_digest, fact_digest};

/// The input side of the counts, from what the sealer wrote: trailers' `entry_count` and the segment indexes'
/// per-series `fact_count`, less positions met twice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Anchors {
    pub(crate) positions: u64,
    pub(crate) facts: BTreeMap<u32, u64>,
}

/// An output as written, before it is read back.
pub(super) struct Written {
    pub(super) id: Identity,
    pub(super) layout: Layout,
    pub(super) len: u64,
    pub(super) path: PathBuf,
    /// The name and reach each series was written with.
    pub(super) infos: BTreeMap<u32, (Option<String>, Reach)>,
}

/// An output that fails a check of §3.2.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifyError {
    /// It could not be read back.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The positional reader refused it.
    #[error(transparent)]
    Read(#[from] FileError),
    /// Its bytes are not the canonical ones for its content (step 4).
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
    /// Rows or entries differ in number from the sealer's counts (step 1).
    #[error("counts differ from the sealer's")]
    Counts,
    /// A per-series checksum, or the entries' checksum when `None`, differs (step 2).
    #[error("checksum of series {0:?} differs")]
    Checksum(Option<u32>),
    /// A row is out of place (step 3).
    #[error("misplaced: {0}")]
    Placement(&'static str),
    /// Its description names other inputs, run or run size than the run's (§3.5 rebuild rule).
    #[error("description disagrees with the run: {0}")]
    Description(&'static str),
}

/// Reads every output back and checks it; returns the outputs with their hashes.
pub(super) fn verify(
    input: &RunInput<'_>,
    taken: &Inputs,
    written: &[Written],
) -> Result<Vec<Output>, VerifyError> {
    let mut sums_in: BTreeMap<u32, u128> = BTreeMap::new();
    let mut entries_in = 0u128;
    for (&p, row) in &taken.rows {
        match &row.item {
            Taken::Fact { fact, .. } => {
                let s = sums_in.entry(fact.series_id).or_default();
                *s = s.wrapping_add(fact_digest(fact, row.received_at, row.commit_time));
            }
            Taken::Entry(e) => {
                let d = entry_digest(p, e, row.received_at, row.commit_time)
                    .map_err(|_| VerifyError::Checksum(None))?;
                entries_in = entries_in.wrapping_add(d);
            }
        }
    }
    let mut sums_out: BTreeMap<u32, u128> = BTreeMap::new();
    let mut entries_out = 0u128;
    let mut facts_out: BTreeMap<u32, u64> = BTreeMap::new();
    let mut seen: BTreeSet<Position> = BTreeSet::new();
    let mut current: BTreeMap<(u32, i64), (u64, Position)> = BTreeMap::new();
    let mut revisions: Vec<((u32, i64), (u64, Position))> = Vec::new();
    let mut out = Vec::new();
    for w in written {
        let bytes = read_whole(&*input.fs.open(&w.path, OpenMode::Read)?)?;
        let file = CompactedFile::open(bytes.as_slice(), &w.id, Some(&w.layout))?;
        if !file.ignored().is_empty() {
            return Err(VerifyError::Placement(
                "an element the writer does not write",
            ));
        }
        check_canonical(&bytes, &w.id, &|s| {
            let (name, reach) = w.infos.get(&s).cloned().unwrap_or((None, Reach::Point));
            SeriesInfo { name, reach }
        })?;
        let d = file.description();
        if d.inputs != taken.names {
            return Err(VerifyError::Description("inputs"));
        }
        if d.run_id != input.first_file_id {
            return Err(VerifyError::Description("run id"));
        }
        if d.run_count as usize != written.len() {
            return Err(VerifyError::Description("run count"));
        }
        let within = |t: i64| {
            (i128::from(d.period_start)..i128::from(d.period_start) + i128::from(d.period_len))
                .contains(&i128::from(t))
        };
        for e in file.index()? {
            for r in file.read_chunk(&e)? {
                let f = &r.fact;
                let p = f.position;
                if !within(f.t) {
                    return Err(VerifyError::Placement("a row outside its file's period"));
                }
                if e.set_aside() != input.set_aside.holds(p) {
                    return Err(VerifyError::Placement("a set-aside row in the wrong chunk"));
                }
                if !e.set_aside() {
                    let key = (f.series_id, f.t);
                    match d.class {
                        Class::Current => {
                            if current.insert(key, rank(f)).is_some() {
                                return Err(VerifyError::Placement("two current rows of one key"));
                            }
                        }
                        Class::Revision => revisions.push((key, rank(f))),
                    }
                }
                if !seen.insert(p) {
                    return Err(VerifyError::Placement("a position held twice"));
                }
                *facts_out.entry(f.series_id).or_default() += 1;
                let s = sums_out.entry(f.series_id).or_default();
                *s = s.wrapping_add(fact_digest(f, r.received_at, r.commit_time));
            }
        }
        for row in file.entries()? {
            if !within(row.commit_time) {
                return Err(VerifyError::Placement("an entry outside its file's period"));
            }
            if !seen.insert(row.position) {
                return Err(VerifyError::Placement("a position held twice"));
            }
            let d = entry_digest(row.position, &row.entry, row.received_at, row.commit_time)
                .map_err(|_| VerifyError::Checksum(None))?;
            entries_out = entries_out.wrapping_add(d);
        }
        out.push(Output {
            id: w.id,
            layout: w.layout.clone(),
            len: w.len,
            sha256: Sha256::digest(&bytes).into(),
            path: w.path.clone(),
        });
    }
    if revisions
        .iter()
        .any(|(key, r)| current.get(key).is_none_or(|top| top <= r))
    {
        return Err(VerifyError::Placement(
            "a revision row not outranked in the run",
        ));
    }
    if seen.len() as u64 != taken.anchors.positions || facts_out != taken.anchors.facts {
        return Err(VerifyError::Counts);
    }
    for (s, sum) in &sums_in {
        if sums_out.get(s) != Some(sum) {
            return Err(VerifyError::Checksum(Some(*s)));
        }
    }
    if entries_in != entries_out {
        return Err(VerifyError::Checksum(None));
    }
    Ok(out)
}
