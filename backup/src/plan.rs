//! What a restore installs, from the records it read (`spec.md` §8 Restore step 1). Pure, and differential against
//! the model's `restore_from`. The restored log ends before the first listed log segment with no copy-verified record:
//! routine, not an error.

use std::collections::BTreeSet;

use kvik_files::catalogue::{Catalogue, FileEntry, Tier};
use kvik_files::desc::{FileRef, kind};
use kvik_files::record::{AckedLoss, Body, Record};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_log::manifest::RecordPosition;
use kvik_log::mark::ResetStamp;

/// What a plan is for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanMode {
    /// A restore that claims a generation.
    Full,
    /// A drill: claims nothing, uploads nothing.
    Drill,
}

/// What a plan is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanInput<'a> {
    /// The generation claimed by the executor; a drill passes the highest claimed + 1.
    pub generation: u32,
    /// The reset stamp's nonce.
    pub nonce: u64,
    /// The generation of the records read.
    pub source_generation: u32,
    /// The fold of the records read (Q-108 applied by the reader).
    pub source: &'a Catalogue,
    /// Full or drill.
    pub mode: PlanMode,
    /// Positions a recovery entry names as `after`; empty for a log with no gap.
    pub recovery_after: &'a [RecordPosition],
}

/// The restore record to write and the files to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    /// The record.
    pub record: RestoreRecord,
    /// The files to fetch, ascending by position.
    pub fetch: Vec<RestoredFile>,
}

/// Why no plan can be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// No records were read.
    #[error("no manifest records")]
    NoRecords,
    /// No generation is left to claim.
    #[error("no generation is left")]
    GenerationsExhausted,
    /// A file at or below a carried end has no copy.
    #[error("file {file:?} has no verified copy")]
    NotCopied {
        /// The file.
        file: FileRef,
    },
    /// No file holds a position at or below the end.
    #[error("no file holds {at:?}")]
    MissingBelowEnd {
        /// The position.
        at: RecordPosition,
    },
    /// The records contradict each other.
    #[error("invalid source: {0}")]
    Invalid(&'static str),
}

/// The signature of the planner, so that a restore driver takes a hand-written one until 8.3a's lands.
pub type PlanFn = fn(&PlanInput<'_>) -> Result<RestorePlan, PlanError>;

/// The plan for `input` (`spec.md` §8 Restore step 1, Generations).
///
/// The restored log is scanned in the source generation only, from sequence 1, over the copied log segments' ranges:
/// a gap that a recorded loss covers, or that a recovery entry named in `recovery_after` follows directly, does not
/// end it (Q-290). Positions of earlier generations at or below a carried end must be held by copied segments or
/// recorded losses. Every live file with a copy is listed, set-aside ones included; no other file is.
pub fn plan(input: &PlanInput<'_>) -> Result<RestorePlan, PlanError> {
    let src = input.source;
    let s = input.source_generation;
    if src.last_epoch() == 0 {
        return Err(PlanError::NoRecords);
    }
    if s == u32::MAX {
        return Err(PlanError::GenerationsExhausted);
    }
    if input.generation <= s {
        return Err(PlanError::Invalid(
            "the generation is not above the source generation",
        ));
    }
    if src.generation() != s {
        return Err(PlanError::Invalid(
            "the records read are of another generation",
        ));
    }
    if input.nonce == 0 {
        return Err(PlanError::Invalid("a reset stamp's nonce is never zero"));
    }
    let read = src.restores().last();
    match read {
        None if s > 0 => {
            return Err(PlanError::Invalid(
                "a generation above 0 without its restore record",
            ));
        }
        Some(r) if r.generation != s => {
            return Err(PlanError::Invalid(
                "the restore record read is of another generation",
            ));
        }
        _ => {}
    }
    let carried_ends = read
        .map(|r| r.set_aside().ends().to_vec())
        .unwrap_or_default();
    let segments: Vec<&FileEntry> = src
        .live()
        .filter(|e| e.description.kind == kind::LOG_SEGMENT)
        .collect();
    let scope = Scope {
        segments: &segments,
        losses: src
            .losses()
            .map(|l| (l.record.first, l.record.last))
            .collect(),
        after: input.recovery_after,
    };
    for k in settled_generations(&carried_ends) {
        let bound = carried_ends
            .iter()
            .filter(|e| e.generation > k)
            .map(|e| e.end)
            .min()
            .filter(|b| b.0 == k);
        let Some((_, to)) = bound else { continue };
        let missing = scope.scan(k, Some(to))?.1;
        if let Some(at) = missing.filter(|q| *q <= to) {
            return Err(match scope.uncopied_holding((k, at)) {
                Some(file) => PlanError::NotCopied { file },
                None => PlanError::MissingBelowEnd { at: (k, at) },
            });
        }
    }
    let restored_end = match scope.scan(s, None)?.0 {
        Some(q) => (s, q),
        None => read.map_or((0, 0), |r| r.restored_end),
    };
    let mut files: Vec<RestoredFile> = src
        .live()
        .filter_map(|e| {
            Some(RestoredFile {
                description: e.description.clone(),
                tier: e.tier,
                copy: e.deep.clone()?,
            })
        })
        .collect();
    files.sort_by_key(|f| {
        let d = &f.description;
        (d.kind, d.generation, d.file_id)
    });
    let mut acknowledgements: Vec<AckedLoss> = src
        .acknowledged_ranges()
        .map(|(identity, first, last)| AckedLoss {
            identity,
            first,
            last,
        })
        .collect();
    acknowledgements.sort();
    acknowledgements.dedup();
    let record = RestoreRecord {
        generation: input.generation,
        stamp: ResetStamp {
            sequence: 1,
            nonce: input.nonce,
        },
        source_generation: s,
        restored_end,
        carried_ends,
        losses: src.losses().map(|l| l.record.clone()).collect(),
        acknowledgements,
        entry_losses: read.map(|r| r.entry_losses.clone()).unwrap_or_default(),
        files,
        drill: input.mode == PlanMode::Drill,
    };
    record.check().map_err(PlanError::Invalid)?;
    let mut fetch: Vec<RestoredFile> = record
        .files
        .iter()
        .filter(|f| f.description.kind == kind::LOG_SEGMENT || f.tier != Tier::Deep)
        .cloned()
        .collect();
    fetch.sort_by_key(|f| {
        let d = &f.description;
        (d.range.map_or((d.generation, 0), |r| r.0), d.file_ref())
    });
    Ok(RestorePlan { record, fetch })
}

/// The generations whose positions a carried end settles: those its end lies in (`spec.md` §8 Restore step 1).
fn settled_generations(ends: &[kvik_log::setaside::RestoredEnd]) -> BTreeSet<u32> {
    ends.iter().map(|e| e.end.0).collect()
}

struct Scope<'a> {
    segments: &'a [&'a FileEntry],
    losses: Vec<(RecordPosition, RecordPosition)>,
    after: &'a [RecordPosition],
}

impl Scope<'_> {
    /// Scans generation `g` from sequence 1 (to `to`, if given): the last sequence held before the first gap that no
    /// loss covers and no recovery entry bridges, and that gap's first sequence.
    fn scan(&self, g: u32, to: Option<u64>) -> Result<(Option<u64>, Option<u64>), PlanError> {
        let mut held = Vec::new();
        for e in self.segments.iter().filter(|e| e.deep.is_some()) {
            if let Some(r) = clip(e, g)? {
                held.push(r);
            }
        }
        let held = merged(held);
        let losses = merged(
            self.losses
                .iter()
                .filter_map(|(a, b)| clip_range(*a, *b, g))
                .collect(),
        );
        let after: BTreeSet<u64> = self
            .after
            .iter()
            .filter(|p| p.0 == g)
            .map(|p| p.1)
            .collect();
        let mut covered = 0u64;
        let mut last_held = None;
        for &(a, b) in &held {
            if a > covered + 1
                && let Some(u) = first_unbridged(covered + 1, a - 1, &losses, &after, true)
            {
                return Ok((last_held, Some(u)));
            }
            if b > covered {
                covered = b;
                last_held = Some(b);
            }
            if covered == u64::MAX {
                return Ok((last_held, None));
            }
        }
        let tail = to
            .filter(|t| *t > covered)
            .and_then(|t| first_unbridged(covered + 1, t, &losses, &after, false));
        Ok((last_held, tail))
    }

    fn uncopied_holding(&self, p: RecordPosition) -> Option<FileRef> {
        self.segments
            .iter()
            .find(|e| e.deep.is_none() && e.description.holds(p))
            .map(|e| e.description.file_ref())
    }
}

/// The sequences of generation `g` a segment holds.
fn clip(e: &FileEntry, g: u32) -> Result<Option<(u64, u64)>, PlanError> {
    let Some((a, b)) = e.description.range else {
        return Ok(None);
    };
    if a.0 != b.0 {
        return Err(PlanError::Invalid(
            "a log segment holding positions of two generations",
        ));
    }
    Ok(clip_range(a, b, g))
}

fn clip_range(a: RecordPosition, b: RecordPosition, g: u32) -> Option<(u64, u64)> {
    if a.0 > g || b.0 < g {
        return None;
    }
    let lo = if a.0 == g { a.1 } else { 0 };
    let hi = if b.0 == g { b.1 } else { u64::MAX };
    Some((lo, hi))
}

/// Sorted, with overlapping and adjacent ranges joined.
fn merged(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(l) if a <= l.1.saturating_add(1) => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// The first sequence of `lo..=hi` that no loss covers, unless a recovery entry right after `hi` names the sequence
/// before the uncovered rest as its `after` (`spec.md` §8 Restore step 1). `None`: the gap does not end the log.
fn first_unbridged(
    lo: u64,
    hi: u64,
    losses: &[(u64, u64)],
    after: &BTreeSet<u64>,
    entry_follows: bool,
) -> Option<u64> {
    let i = losses.partition_point(|&(_, b)| b < lo);
    let u = match losses.get(i) {
        Some(&(a, b)) if a <= lo => b.checked_add(1)?,
        _ => lo,
    };
    if u > hi {
        return None;
    }
    let loss_inside = losses
        .get(i..)
        .unwrap_or_default()
        .iter()
        .any(|&(a, _)| a > u && a <= hi);
    if entry_follows && !loss_inside && after.contains(&(u - 1)) {
        return None;
    }
    Some(u)
}

/// The records a restore reads, from one generation's records in epoch order (Q-108): from the last rebuild record
/// whose rebuilt manifest is whole in the list, its closing empty record (one without field 100) included, up to the
/// next rebuild record; with no whole rebuilt manifest, the records before the first rebuild record that opens one.
pub fn records_to_read(records: &[Record]) -> &[Record] {
    let opens = |r: &Record| matches!(r.body, Body::Rebuild(_)) && r.old_epoch.is_none();
    let closes = |r: &Record| matches!(r.body, Body::Empty) && r.old_epoch.is_none();
    let mut end = records.len();
    while let Some(i) = records[..end].iter().rposition(opens) {
        if records[i + 1..end].iter().any(closes) {
            return &records[i..end];
        }
        end = i;
    }
    &records[..end]
}
