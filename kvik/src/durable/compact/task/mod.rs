//! The run as a background task stepped on `kvik-maint` within `maintenance.memory` (compaction design §3.1, §4;
//! roadmap 6.2a2b). A census reads every input once for the digests, the sealer's counts, the commit-time rows and
//! each series' size; then passes over the inputs take the rows of one group of series (in name order) for at most
//! 16 partitions. Count passes come first, since a file's description precedes its data and names its positions
//! (Q-1535); a run of one pass counts and writes from the same rows. Each file is written through its writer's
//! sink in 128 KiB pieces and synced every 1 MiB, then verified a block at a time.

mod census;
mod passes;
mod verification;

use std::collections::BTreeMap;
use std::path::PathBuf;

use kvik_chunk::file::Position;
use kvik_log::entries::Entry;
use kvik_log::writer::share::{Background, Next};

use super::plan::Candidate;
use super::run::RunInput;
use super::segment::{PIECE, Segment};
use super::verify::{Check, Written};
use super::{Cause, Output, RunError, Stall};

#[cfg(test)]
pub(super) use census::window;
use census::{Census, Plan};
use passes::{Open, Rows, pending};

/// Bytes written to an output between its syncs (compaction design §4).
pub(super) const SYNC_EVERY: u64 = 1 << 20;
/// The compression context and other fixed costs, counted whole (compaction design §4 Memory).
pub(super) const FIXED: u64 = 2 << 20;

enum Stage {
    Census {
        seg: usize,
        segment: Option<Segment>,
    },
    Pass {
        k: usize,
        rows: Rows,
    },
    Verify(Box<Check>),
    Done,
}

/// The stepped run.
pub(crate) struct RunTask<'a> {
    input: RunInput<'a>,
    segments: Vec<Candidate>,
    cap: u64,
    census: Census,
    plan: Option<Plan>,
    entries: BTreeMap<usize, Vec<(Position, Entry)>>,
    open: Vec<Open>,
    written: Vec<Written>,
    created: Vec<PathBuf>,
    data_dir: bool,
    stage: Stage,
    peak: u64,
    outcome: Option<Result<Vec<Output>, RunError>>,
}

impl<'a> RunTask<'a> {
    /// A run over `segments` (from [`super::plan`]) that holds at most about `memory` bytes.
    pub(crate) fn new(input: RunInput<'a>, segments: Vec<Candidate>, memory: u64) -> Self {
        let ranges = segments
            .iter()
            .map(|c| {
                let (first, last) = c
                    .seal
                    .as_ref()
                    .map(|(_, t)| (t.first_sequence, t.last_sequence))
                    .or(c.range)
                    .unwrap_or((0, u64::MAX));
                (
                    (c.generation, first),
                    (c.generation, last),
                    c.generation,
                    c.file_id,
                )
            })
            .collect();
        Self {
            input,
            segments,
            cap: memory,
            census: Census {
                ranges,
                ..Census::default()
            },
            plan: None,
            entries: BTreeMap::new(),
            open: Vec::new(),
            written: Vec::new(),
            created: Vec::new(),
            data_dir: false,
            stage: Stage::Census {
                seg: 0,
                segment: None,
            },
            peak: 0,
            outcome: None,
        }
    }

    /// The most bytes it held after any step.
    pub(crate) fn peak(&self) -> u64 {
        self.peak
    }

    /// The stage it is in, for a measurement's report.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        match &self.stage {
            Stage::Census { .. } => "census",
            Stage::Pass { rows, .. } if !rows.collected => "collect",
            Stage::Pass { .. } => "write",
            Stage::Verify(_) => "verify",
            Stage::Done => "done",
        }
    }

    /// The outcome once the run has ended.
    pub(crate) fn outcome(&mut self) -> Option<Result<Vec<Output>, RunError>> {
        self.outcome.take()
    }

    fn held(&self) -> u64 {
        let segment = match &self.stage {
            Stage::Census { segment, .. } => segment.as_ref().map_or(0, Segment::held),
            Stage::Pass { rows, .. } => rows.segment.as_ref().map_or(0, Segment::held) + rows.bytes,
            Stage::Verify(c) => c.held(),
            Stage::Done => 0,
        };
        let writers: u64 = self
            .open
            .iter()
            .filter_map(|o| o.w.as_ref())
            .map(|w| w.held_bytes() + 2 * PIECE as u64)
            .sum();
        let entries: u64 = self.entries.values().map(|e| e.len() as u64 * 192).sum();
        FIXED
            + segment
            + writers
            + entries
            + self.census.held()
            + self.plan.as_ref().map_or(0, Plan::held)
            + self.written.len() as u64 * 192
    }

    /// Runs one step; `true` once the run has ended and [`Self::outcome`] holds its result.
    pub(crate) fn step_run(&mut self) -> bool {
        if self.outcome.is_some() || matches!(self.stage, Stage::Done) {
            return true;
        }
        let r = self.advance();
        self.peak = self.peak.max(self.held());
        match r {
            Ok(None) => false,
            Ok(Some(out)) => {
                self.stage = Stage::Done;
                self.outcome = Some(Ok(out));
                true
            }
            Err(e) => {
                for p in &self.created {
                    let _ = self.input.fs.remove_file(p);
                }
                self.open.clear();
                self.stage = Stage::Done;
                self.outcome = Some(Err(e));
                true
            }
        }
    }

    fn advance(&mut self) -> Result<Option<Vec<Output>>, RunError> {
        match &self.stage {
            Stage::Census { .. } => self.census_step(),
            Stage::Pass { .. } => self.pass_step(),
            Stage::Verify(_) => self.verify_step(),
            Stage::Done => Ok(None),
        }
    }

    fn stall_at(&self, p: Position, cause: Cause) -> RunError {
        let (generation, file_id) = self
            .census
            .ranges
            .iter()
            .find(|r| r.0 <= p && p <= r.1)
            .map_or((p.0, 0), |r| (r.2, r.3));
        RunError::Stalled(Stall {
            generation,
            file_id,
            cause,
        })
    }
}

impl Background for RunTask<'static> {
    fn next(&mut self) -> Next {
        match &self.stage {
            Stage::Census {
                segment: Some(s), ..
            } => s.reading().map_or(Next::Cpu, Next::Device),
            Stage::Census { seg, .. } if *seg < self.segments.len() => Next::Device(0),
            Stage::Pass { k, .. } if self.plan.as_ref().is_some_and(|p| *k >= p.passes.len()) => {
                Next::Device(0)
            }
            Stage::Pass { rows, .. } if !rows.collected => match &rows.segment {
                Some(s) => s.reading().map_or(Next::Cpu, Next::Device),
                None if rows.seg < self.segments.len() => Next::Device(0),
                None => Next::Cpu,
            },
            Stage::Pass { .. } => {
                if self.open.iter().any(|o| o.out.is_none())
                    || (!self.data_dir && !self.open.is_empty())
                {
                    Next::Device(0)
                } else if self
                    .open
                    .iter()
                    .any(|o| pending(o) >= PIECE || !o.tail.is_empty())
                {
                    Next::Device(PIECE as u64)
                } else if self
                    .open
                    .iter()
                    .any(|o| o.unsynced >= SYNC_EVERY || o.written.is_some())
                {
                    Next::Device(0)
                } else {
                    Next::Cpu
                }
            }
            Stage::Verify(c) => c.next_bytes().map_or(Next::Cpu, Next::Device),
            Stage::Done => Next::Idle,
            Stage::Census { .. } => Next::Cpu,
        }
    }

    fn step(&mut self) {
        self.step_run();
    }
}
