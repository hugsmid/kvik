//! The L0 planner (§3.1 Inputs): whole sealed segments the manifest lists, in position order, from the first above
//! C(g), up to `run_bytes`, never skipping one. A listed segment absent from `log/` is a candidate without a seal, so
//! it stalls the planner rather than being passed (Q-1522).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use kvik_files::record::Body;
use kvik_log::body::Trailer;
use kvik_log::directory::catalogue::read_trailer;
use kvik_log::fs::{Fs, OpenMode};
use kvik_log::manifest::{listed_segments, read_manifest};
use kvik_log::name::SegmentName;
use kvik_log::store::{LOG_DIR, MANIFEST};

use super::{Cause, MAX_RUN_INPUTS, Stall};

/// A sealed log segment as the planner sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Its generation.
    pub(crate) generation: u32,
    /// Its `file_id`.
    pub(crate) file_id: u64,
    /// Its path.
    pub(crate) path: PathBuf,
    /// Its length and trailer; `None` when they cannot be read.
    pub(crate) seal: Option<(u64, Trailer)>,
    /// Whether the manifest lists it (`0x22`).
    pub(crate) listed: bool,
    /// Its first and last sequence as its manifest description gives them.
    pub(crate) range: Option<(u64, u64)>,
}

/// What the planner decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Nothing waits.
    Idle,
    /// A run of these segments, in position order.
    Run(Vec<Candidate>),
    /// Compaction of log segments stalls at a segment.
    Stalled(Stall),
}

/// Every sealed segment under `log/` and every segment the manifest lists, in position order, with its trailer
/// where one can be read.
pub(crate) fn candidates(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
) -> io::Result<Vec<Candidate>> {
    let listed = listed(fs, dir, store_id)?;
    let log = dir.join(LOG_DIR);
    let mut names: BTreeSet<(u32, u64)> = fs
        .read_dir(&log)?
        .into_iter()
        .filter_map(|e| SegmentName::parse(&e.name).filter(|n| n.sealed))
        .map(|n| (n.generation, n.file_id))
        .collect();
    names.extend(listed.keys());
    Ok(names
        .into_iter()
        .map(|(generation, file_id)| {
            let name = SegmentName {
                generation,
                file_id,
                sealed: true,
            };
            let path = log.join(name.to_string());
            Candidate {
                generation,
                file_id,
                seal: read_trailer(fs, &path),
                path,
                listed: listed.contains_key(&(generation, file_id)),
                range: listed.get(&(generation, file_id)).copied().flatten(),
            }
        })
        .collect())
}

/// The segments the manifest lists, each with the sequence range its `0x22` description gives.
fn listed(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
) -> io::Result<BTreeMap<(u32, u64), Option<(u64, u64)>>> {
    let f = match fs.open(&dir.join(MANIFEST), OpenMode::Read) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    let bytes = super::run::read_whole(&*f)?;
    let mut out: BTreeMap<(u32, u64), Option<(u64, u64)>> = read_manifest(&bytes, store_id)
        .map(|read| listed_segments(&bytes, &read))
        .unwrap_or_default()
        .into_iter()
        .map(|k| (k, None))
        .collect();
    let Ok(image) = kvik_files::image::read(&bytes, store_id) else {
        return Ok(out);
    };
    for r in image.records() {
        let Body::Sealed(descs) = &r.body else {
            continue;
        };
        for d in descs {
            if let Some(range) = out.get_mut(&(d.generation, d.file_id)) {
                *range = d
                    .range
                    .filter(|(a, b)| a.0 == d.generation && b.0 == d.generation)
                    .map(|(a, b)| (a.1, b.1));
            }
        }
    }
    Ok(out)
}

/// The next run: `done` holds C(g) per generation; a run takes at least one segment and stops once its inputs reach
/// `run_bytes`, at a segment the manifest does not list yet, or at [`MAX_RUN_INPUTS`].
pub(crate) fn plan(candidates: &[Candidate], done: &BTreeMap<u32, u64>, run_bytes: u64) -> Plan {
    let mut run = Vec::new();
    let mut bytes = 0u64;
    for c in candidates {
        let c_g = done.get(&c.generation).copied();
        let last = c
            .seal
            .as_ref()
            .map(|s| s.1.last_sequence)
            .or(c.range.map(|r| r.1));
        if let (Some(c_g), Some(last)) = (c_g, last)
            && last <= c_g
        {
            continue;
        }
        let Some((len, trailer)) = &c.seal else {
            return stop(run, c, Cause::Unreadable("no valid trailer"));
        };
        if c_g.is_some_and(|c_g| trailer.first_sequence <= c_g) {
            return stop(run, c, Cause::Straddles);
        }
        if !c.listed {
            break;
        }
        run.push(c.clone());
        bytes = bytes.saturating_add(*len);
        if bytes >= run_bytes || run.len() == MAX_RUN_INPUTS {
            break;
        }
    }
    if run.is_empty() {
        Plan::Idle
    } else {
        Plan::Run(run)
    }
}

/// The run so far, or a stall at `c` when nothing was taken before it.
fn stop(run: Vec<Candidate>, c: &Candidate, cause: Cause) -> Plan {
    if run.is_empty() {
        Plan::Stalled(Stall {
            generation: c.generation,
            file_id: c.file_id,
            cause,
        })
    } else {
        Plan::Run(run)
    }
}
