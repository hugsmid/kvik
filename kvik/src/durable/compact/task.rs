//! The run as a background task stepped on `kvik-maint` within `maintenance.memory` (compaction design §3.1, §4;
//! roadmap 6.2a2b). A census reads every input once for the digests, the sealer's counts, the commit-time rows and
//! each series' size; then passes over the inputs take the rows of one group of series (in name order) for at most
//! 16 partitions. Count passes come first, since a file's description precedes its data and names its positions
//! (Q-1535); a run of one pass counts and writes from the same rows. Each file is written through its writer's
//! sink in 128 KiB pieces and synced every 1 MiB, then verified a block at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Range;
use std::path::PathBuf;

use kvik_chunk::file::{
    BLOCK_ROWS, Batch, CHUNK_ROWS, Class, Description, Fact, FileWriter, Identity, IndexEntry,
    Input, Level, Position, SeriesInfo,
};
use kvik_log::content::Item;
use kvik_log::entries::{Definition, Entry};
use kvik_log::fs::{FsFile, OpenMode};
use kvik_log::writer::share::{Background, Next};

use super::plan::Candidate;
use super::run::{DATA_DIR, RunInput, info, rank, tmp_path};
use super::segment::{Decoded, Event, PIECE, Segment};
use super::verify::{Check, Expect, Written};
use super::{
    Cause, KIND_LOG, Output, RunError, Stall, chunk_fact, entry_digest, fact_digest, period,
};

/// Partitions one pass writes at most (§3.1 Bounded outputs).
const MAX_OPEN: usize = 16;
/// What a writer holds for a commit-time row (Q-1840): the row and its count of rows held (`FileWriter::held_bytes`).
const BATCH_ROW: u64 = std::mem::size_of::<Batch>() as u64 + 8;
/// Bytes written to an output between its syncs (compaction design §4).
const SYNC_EVERY: u64 = 1 << 20;
/// What a writer's block being built holds beyond its sink: a block of rows as `FileWriter::held_bytes` counts them.
const BLOCK_RESERVE: u64 = (BLOCK_ROWS * (std::mem::size_of::<Fact>() + 255)) as u64;
/// The least a group may hold, so a census near the cap does not split every series; a cap of 0 makes every series
/// its own pass, split as finely as it goes.
const MIN_GROUP: u64 = 1 << 20;
/// The compression context and other fixed costs, counted whole (compaction design §4 Memory).
const FIXED: u64 = 2 << 20;
const DAY: i64 = 86_400_000_000_000;
const CURRENT: u8 = 0;
const REVISION: u8 = 1;

/// Bytes a row held in a pass costs: the fact, its text at full length and its sort key (§4: real size).
fn row_cost(text: usize) -> u64 {
    (std::mem::size_of::<PRow>() + text) as u64
}

/// One row of a pass.
struct PRow {
    period: usize,
    class: u8,
    order: usize,
    aside: bool,
    fact: Fact,
}

/// What a file will hold, from the count passes.
#[derive(Default)]
struct Meta {
    positions: BTreeMap<u32, u64>,
    greatest: Position,
    set_aside: bool,
    entries: bool,
    used: Vec<u64>,
    /// Its series (a series' set-aside rows counted again) and rows, so its writer's index is allocated once.
    series: u64,
    rows: u64,
}

impl Meta {
    fn hold(&mut self, p: Position, batch: usize) {
        *self.positions.entry(p.0).or_default() += 1;
        self.greatest = self.greatest.max(p);
        if self.used.len() <= batch / 64 {
            self.used.resize(batch / 64 + 1, 0);
        }
        self.used[batch / 64] |= 1 << (batch % 64);
    }
}

/// Everything the census took from the inputs.
#[derive(Default)]
struct Census {
    expect: Expect,
    batches: BTreeMap<Position, Batch>,
    defs: BTreeMap<u32, Vec<(Position, Definition)>>,
    /// Each series' row bytes, rows and least and greatest `t`.
    bytes: BTreeMap<u32, (u64, u64, i64, i64)>,
    /// Rows a day.
    days: BTreeMap<i64, u64>,
    /// Digests of positions inside two inputs' ranges, so a position met twice is compared and kept once.
    dups: BTreeMap<Position, u128>,
    /// Times whose period may leave `i64`, decided once the last commit time is known.
    extreme: Vec<(Position, i64)>,
    ranges: Vec<(Position, Position, u32, u64)>,
    segment: (u64, BTreeMap<u32, u64>),
}

/// What a `BTreeMap` of `n` entries allocates: nodes are about half full after ordered inserts (Q-1841).
fn tree<K, V>(n: usize) -> u64 {
    (n * (2 * (std::mem::size_of::<K>() + std::mem::size_of::<V>()) + 16)) as u64
}

impl Census {
    fn held(&self) -> u64 {
        tree::<Position, Batch>(self.batches.len())
            + tree::<u32, u128>(self.expect.sums.len())
            + tree::<u32, u64>(self.expect.anchors.facts.len() + self.segment.1.len())
            + tree::<u32, (u64, u64, i64, i64)>(self.bytes.len())
            + tree::<i64, u64>(self.days.len())
            + tree::<Position, u128>(self.dups.len())
            + tree::<u32, Vec<()>>(self.defs.len())
            + self
                .defs
                .values()
                .map(|d| d.len() as u64 * 192)
                .sum::<u64>()
    }
}

/// Series taken by one pass, in name order; a series too large for one pass alone is taken a range of `t` at a time
/// (Q-1537).
#[derive(Clone, Debug)]
struct Group {
    series: Range<usize>,
    t: Option<TRange>,
    /// Its rows; for a range of `t`, a share of the series' rows with a quarter to spare.
    rows: u64,
}

/// One range of `t` of a series split across passes.
#[derive(Clone, Copy, Debug)]
struct TRange {
    lo: i64,
    hi: i128,
    first: bool,
    last: bool,
}

impl Group {
    fn takes(&self, order: usize, t: i64, aside: bool) -> bool {
        self.series.contains(&order)
            && self.t.is_none_or(|r| {
                if aside {
                    r.first
                } else {
                    r.lo <= t && i128::from(t) < r.hi
                }
            })
    }

    /// Whether its series stay open for the next pass.
    fn continues(&self) -> bool {
        self.t.is_some_and(|r| !r.last)
    }
}

/// The most passes one series is split into.
const MAX_SPLIT: i128 = 64;

#[derive(Clone, Copy)]
struct Pass {
    window: usize,
    group: usize,
    count: bool,
    write: bool,
}

/// The passes, from the census.
struct Plan {
    batches: Vec<Batch>,
    last_commit_time: i64,
    periods: Vec<(i64, i64)>,
    order: BTreeMap<u32, usize>,
    groups: Vec<Group>,
    windows: Vec<Range<usize>>,
    /// Rows a period.
    period_rows: Vec<u64>,
    /// Rows each window and group took in its count pass, so its write pass holds them exactly.
    taken: BTreeMap<(usize, usize), usize>,
    passes: Vec<Pass>,
    metas: BTreeMap<(usize, u8), Meta>,
    files: Vec<(usize, u8)>,
    names: Vec<Input>,
    /// A series renamed in the inputs: one group, and each file's own name order (Q-1412).
    renamed: bool,
}

impl Plan {
    fn held(&self) -> u64 {
        (self.batches.capacity() * std::mem::size_of::<Batch>()) as u64
            + tree::<u32, usize>(self.order.len())
            + tree::<(usize, u8), Meta>(self.metas.len())
            + self
                .metas
                .values()
                .map(|m| tree::<u32, u64>(m.positions.len()) + m.used.capacity() as u64 * 8)
                .sum::<u64>()
    }

    fn period_of(&self, t: i64, weeks: &BTreeSet<i64>) -> Option<usize> {
        let p = period(t, self.last_commit_time, weeks)?;
        self.periods.binary_search(&p).ok()
    }

    fn batch_of(&self, p: Position) -> usize {
        self.batches
            .partition_point(|b| (b.generation, b.last_sequence) < p)
    }
}

/// An output being written.
struct Open {
    file: usize,
    id: Identity,
    w: Option<FileWriter<Vec<u8>>>,
    tail: Vec<u8>,
    /// The file, once its stale copy is removed (`removed`) and it is created, each a step of its own.
    out: Option<Box<dyn FsFile>>,
    removed: bool,
    path: PathBuf,
    at: u64,
    unsynced: u64,
    series: Option<u32>,
    /// Its writer is past its last series; the index goes out a block a step.
    finishing: bool,
    written: Option<Written>,
}

impl Open {
    fn file(&self) -> &dyn FsFile {
        self.out.as_deref().expect("opened")
    }
}

/// The rows of the open pass.
#[derive(Default)]
struct Rows {
    seg: usize,
    segment: Option<Segment>,
    rows: Vec<PRow>,
    collected: bool,
    opened: bool,
    /// What `rows` costs: its capacity and the texts' bytes (Q-1841).
    bytes: u64,
    text: u64,
    next: usize,
}

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
        match &mut self.stage {
            Stage::Census { seg, segment } => {
                let Some(c) = self.segments.get(*seg) else {
                    self.plan()?;
                    self.stage = Stage::Pass {
                        k: 0,
                        rows: Rows::default(),
                    };
                    return Ok(None);
                };
                let Some(s) = segment else {
                    *segment = Some(Segment::open(self.input.fs, c)?);
                    return Ok(None);
                };
                match s.step(self.input.store_id, self.input.set_aside)? {
                    Event::Pending => {}
                    Event::Batch(d) => {
                        let s = segment.as_ref().expect("open");
                        take(&mut self.census, s, d)?;
                    }
                    Event::End => {
                        let s = segment.take().expect("open");
                        seal_counts(&mut self.census, &s)?;
                        *seg += 1;
                    }
                }
                Ok(None)
            }
            Stage::Pass { .. } => self.pass_step(),
            Stage::Verify(check) => {
                let Census { expect, defs, .. } = &mut self.census;
                let input = self.input;
                let infos = |s: u32, g: Position| -> SeriesInfo {
                    let (name, reach) = info(&input, defs, s, g);
                    SeriesInfo { name, reach }
                };
                match check.step(&input, expect, &infos)? {
                    None => Ok(None),
                    Some(out) => {
                        debug_assert_eq!(out.len(), check.files().len());
                        Ok(Some(out))
                    }
                }
            }
            Stage::Done => Ok(None),
        }
    }

    /// Turns the census into passes (compaction design §4 Memory).
    fn plan(&mut self) -> Result<(), RunError> {
        let c = &mut self.census;
        c.expect.anchors.facts.retain(|_, n| *n > 0);
        let batches: Vec<Batch> = std::mem::take(&mut c.batches).into_values().collect();
        let mut last: BTreeMap<u32, i64> = BTreeMap::new();
        for b in &batches {
            if last.get(&b.generation).is_some_and(|&t| b.commit_time < t) {
                return Err(
                    self.stall_at((b.generation, b.first_sequence), Cause::CommitTimeDecreases)
                );
            }
            last.insert(b.generation, b.commit_time);
        }
        let last_commit_time = batches
            .iter()
            .map(|b| b.commit_time)
            .max()
            .unwrap_or(i64::MIN);
        let weeks = self.input.weeks_merged;
        c.extreme.sort();
        if let Some(&(p, _)) = c
            .extreme
            .iter()
            .find(|(_, t)| period(*t, last_commit_time, weeks).is_none())
        {
            return Err(self.stall_at(p, Cause::PeriodOutOfRange));
        }
        let c = &self.census;
        let mut by_period: BTreeMap<(i64, i64), u64> = BTreeMap::new();
        for (d, n) in &c.days {
            if let Some(p) = d
                .checked_mul(DAY)
                .and_then(|t| period(t, last_commit_time, weeks))
            {
                *by_period.entry(p).or_default() += n;
            }
        }
        let periods: Vec<(i64, i64)> = by_period.keys().copied().collect();
        let period_rows: Vec<u64> = by_period.into_values().collect();
        let mut named: Vec<(Option<String>, u32)> = c
            .bytes
            .keys()
            .map(|&s| (info(&self.input, &c.defs, s, (u32::MAX, u64::MAX)).0, s))
            .collect();
        named.sort_by(|a, b| {
            (a.0.is_none(), a.0.as_deref().map(str::as_bytes), a.1).cmp(&(
                b.0.is_none(),
                b.0.as_deref().map(str::as_bytes),
                b.1,
            ))
        });
        let renamed = c.defs.iter().any(|(s, d)| {
            let below = (self.input.registry)(*s).map(|d| d.name);
            d.iter()
                .map(|(_, d)| d.name.clone())
                .chain(below)
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        });
        let (open, per_writer) = window(self.cap, c.bytes.len() as u64, batches.len() as u64);
        let reserved = FIXED + c.held() + open as u64 * per_writer;
        let budget = match self.cap {
            0 => 0,
            cap => (cap.saturating_sub(reserved) / 2).max(MIN_GROUP),
        };
        let mut groups: Vec<Group> = Vec::new();
        let mut size = 0u64;
        for (i, (_, s)) in named.iter().enumerate() {
            let (b, n, lo, hi) = c.bytes[s];
            if !renamed && b > budget {
                let parts = (i128::from(b) / i128::from(budget.max(1)) + 1).min(MAX_SPLIT);
                let width = ((i128::from(hi) - i128::from(lo)) / parts + 1).max(1);
                let mut at = i128::from(lo);
                while at <= i128::from(hi) {
                    let end = at + width;
                    groups.push(Group {
                        series: i..i + 1,
                        t: Some(TRange {
                            lo: at as i64,
                            hi: end,
                            first: at == i128::from(lo),
                            last: end > i128::from(hi),
                        }),
                        rows: n.div_ceil(parts as u64) + n.div_ceil(4 * parts as u64),
                    });
                    at = end;
                }
                size = budget;
                continue;
            }
            match groups.last_mut() {
                Some(g) if g.t.is_none() && (renamed || size + b <= budget) => {
                    g.series.end = i + 1;
                    g.rows += n;
                    size += b;
                }
                _ => {
                    groups.push(Group {
                        series: i..i + 1,
                        t: None,
                        rows: n,
                    });
                    size = b;
                }
            }
        }
        if groups.is_empty() {
            groups.push(Group {
                series: 0..0,
                t: None,
                rows: 0,
            });
        }
        let windows: Vec<Range<usize>> = (0..periods.len())
            .step_by(open)
            .map(|s| s..(s + open).min(periods.len()))
            .collect();
        let mut passes = Vec::new();
        if windows.len() * groups.len() == 1 {
            passes.push(Pass {
                window: 0,
                group: 0,
                count: true,
                write: true,
            });
        } else {
            for write in [false, true] {
                for w in 0..windows.len() {
                    for g in 0..groups.len() {
                        passes.push(Pass {
                            window: w,
                            group: g,
                            count: !write,
                            write,
                        });
                    }
                }
            }
        }
        let names: BTreeSet<Input> = self
            .segments
            .iter()
            .map(|c| Input {
                kind: KIND_LOG,
                file_id: c.file_id,
            })
            .collect();
        self.plan = Some(Plan {
            batches,
            last_commit_time,
            periods,
            order: named
                .iter()
                .enumerate()
                .map(|(i, (_, s))| (*s, i))
                .collect(),
            groups,
            windows,
            period_rows,
            taken: BTreeMap::new(),
            passes,
            metas: BTreeMap::new(),
            files: Vec::new(),
            names: names.into_iter().collect(),
            renamed,
        });
        self.census.bytes = BTreeMap::new();
        self.census.days = BTreeMap::new();
        Ok(())
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

    fn pass_step(&mut self) -> Result<Option<Vec<Output>>, RunError> {
        let plan = self.plan.as_mut().expect("planned");
        let Stage::Pass { k, rows } = &mut self.stage else {
            unreachable!("in a pass")
        };
        let Some(pass) = plan.passes.get(*k).copied() else {
            self.input.fs.sync_dir(&self.input.dir.join(DATA_DIR))?;
            let written = std::mem::take(&mut self.written);
            let ranges: Vec<(Position, Position)> =
                self.census.ranges.iter().map(|r| (r.0, r.1)).collect();
            self.census.expect.names = plan.names.clone();
            self.census.batches = BTreeMap::new();
            self.stage = Stage::Verify(Box::new(Check::new(written, &ranges)));
            self.plan = None;
            return Ok(None);
        };
        let (window, group) = (
            plan.windows[pass.window].clone(),
            plan.groups[pass.group].clone(),
        );
        let first_of_window = pass.group == 0;
        let last_of_window = pass.group + 1 == plan.groups.len();
        if !rows.collected {
            if rows.seg == 0 && rows.segment.is_none() && rows.rows.capacity() == 0 {
                let want = match plan.taken.get(&(pass.window, pass.group)) {
                    Some(&n) => n as u64,
                    None => {
                        let in_window: u64 = plan.period_rows[window.clone()].iter().sum();
                        let most = self.cap / std::mem::size_of::<PRow>() as u64;
                        group.rows.min(in_window).min(most)
                    }
                };
                rows.rows.reserve_exact(usize::try_from(want).unwrap_or(0));
                rows.bytes = (rows.rows.capacity() * std::mem::size_of::<PRow>()) as u64;
            }
            let Some(c) = self.segments.get(rows.seg) else {
                rows.collected = true;
                if pass.count {
                    plan.taken
                        .insert((pass.window, pass.group), rows.rows.len());
                }
                place(plan, &mut rows.rows, &mut self.entries, pass.count);
                if pass.count && pass.window + 1 == plan.windows.len() && last_of_window {
                    plan.files = plan.metas.keys().copied().collect();
                }
                if pass.write && plan.renamed {
                    by_file_names(&self.input, &self.census.defs, plan, &mut rows.rows);
                }
                return Ok(None);
            };
            let Some(s) = &mut rows.segment else {
                rows.segment = Some(Segment::open(self.input.fs, c)?);
                return Ok(None);
            };
            match s.step(self.input.store_id, self.input.set_aside)? {
                Event::Pending => {}
                Event::End => {
                    rows.segment = None;
                    rows.seg += 1;
                }
                Event::Batch(d) => {
                    rows.text += collect(
                        plan,
                        self.input.weeks_merged,
                        &window,
                        &group,
                        first_of_window,
                        d,
                        &mut rows.rows,
                        &mut self.entries,
                    );
                    rows.bytes =
                        (rows.rows.capacity() * std::mem::size_of::<PRow>()) as u64 + rows.text;
                }
            }
            return Ok(None);
        }
        if !pass.write {
            *k += 1;
            *rows = Rows::default();
            self.entries.clear();
            return Ok(None);
        }
        if !rows.opened && first_of_window {
            rows.opened = true;
            for i in (0..plan.files.len()).filter(|&i| window.contains(&plan.files[i].0)) {
                self.open.push(new_output(&self.input, plan, i)?);
            }
            return Ok(None);
        }
        if !self.data_dir && !self.open.is_empty() {
            self.input
                .fs
                .create_dir_all(&self.input.dir.join(DATA_DIR))?;
            self.data_dir = true;
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.out.is_none()) {
            if o.removed {
                o.out = Some(self.input.fs.open(&o.path, OpenMode::CreateNew)?);
                self.created.push(o.path.clone());
            } else {
                match self.input.fs.remove_file(&o.path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                    _ => o.removed = true,
                }
            }
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| pending(o) >= PIECE) {
            drain(o)?;
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.unsynced >= SYNC_EVERY) {
            o.file().sync_all()?;
            o.unsynced = 0;
            return Ok(None);
        }
        if rows.next < rows.rows.len() {
            write_rows(&self.input, &self.census.defs, plan, &mut self.open, rows)?;
            return Ok(None);
        }
        if !group.continues() {
            for o in &mut self.open {
                if o.series.take().is_some() {
                    o.w.as_mut().expect("open").end_series()?;
                }
            }
        }
        if !last_of_window {
            *k += 1;
            *rows = Rows::default();
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.w.is_some()) {
            let (pi, class) = plan.files[o.file];
            let w = o.w.as_mut().expect("checked");
            if !o.finishing {
                o.finishing = true;
                if class == CURRENT
                    && let Some(e) = self.entries.remove(&pi)
                {
                    w.entries(&e)?;
                }
            }
            if !w.finish_step()? {
                return Ok(None);
            }
            let (sink, done) = o.w.take().expect("checked").finish()?;
            o.tail = sink;
            let meta = &plan.metas[&plan.files[o.file]];
            o.written = Some(Written {
                id: o.id,
                layout: done.layout,
                len: done.len,
                path: o.path.clone(),
                period: plan.periods[pi],
                class: if class == CURRENT {
                    Class::Current
                } else {
                    Class::Revision
                },
                greatest: meta.greatest,
            });
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| !o.tail.is_empty()) {
            drain(o)?;
            return Ok(None);
        }
        if let Some(i) = self.open.iter().position(|o| o.written.is_some()) {
            let mut o = self.open.swap_remove(i);
            o.file().sync_all()?;
            self.written.push(o.written.take().expect("finished"));
            return Ok(None);
        }
        self.written.sort_by_key(|w| w.id.file_id);
        *k += 1;
        *rows = Rows::default();
        self.entries.clear();
        Ok(None)
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

/// Partitions a window opens and what each of its writers may hold, for a census of `series` and `batches`
/// (Q-1840, Q-1940): their index entries and commit-time rows fit a quarter of `cap`, their whole writers half.
pub(super) fn window(cap: u64, series: u64, batches: u64) -> (usize, u64) {
    let per_file = series * 2 * std::mem::size_of::<IndexEntry>() as u64 + batches * BATCH_ROW;
    let per_writer = per_file + 2 * PIECE as u64 + BLOCK_RESERVE;
    let open = (cap / 4 / per_file.max(1))
        .min(cap / 2 / per_writer)
        .clamp(1, MAX_OPEN as u64) as usize;
    (open, per_writer)
}

/// The census's share of one batch: digests, counts, commit-time row, definitions, sizes and days.
fn take(c: &mut Census, s: &Segment, d: Decoded) -> Result<(), RunError> {
    let Decoded {
        content,
        commit,
        set_aside,
    } = d;
    let first = (commit.generation, commit.first_sequence);
    let b = Batch {
        generation: commit.generation,
        first_sequence: commit.first_sequence,
        last_sequence: commit.last_sequence,
        received_at: content.received_at,
        commit_time: commit.commit_time,
    };
    match c.batches.get(&first) {
        Some(old) if *old != b => return Err(RunError::Conflict(first)),
        _ => {
            c.batches.insert(first, b);
        }
    }
    let twice = |p: Position| c.ranges.iter().filter(|r| r.0 <= p && p <= r.1).count() > 1;
    let mut seq = content.first_sequence;
    for item in &content.items {
        let p = (commit.generation, seq);
        seq += item.sequences();
        c.segment.0 += 1;
        let (digest, series, when) = match item {
            Item::Fact(r) => {
                *c.segment.1.entry(r.series_id).or_default() += 1;
                let f = chunk_fact(r, p);
                (
                    fact_digest(&f, content.received_at, commit.commit_time),
                    Some((f.series_id, f.text.len())),
                    f.t,
                )
            }
            Item::Entry(e) => {
                match &**e {
                    Entry::Ignored { .. } => return Err(s.stall(Cause::Unknown)),
                    _ if set_aside => return Err(s.stall(Cause::SetAsideEntry)),
                    Entry::Definition(d) => {
                        c.defs.entry(d.id).or_default().push((p, (**d).clone()))
                    }
                    _ => {}
                }
                let d = entry_digest(p, e, content.received_at, commit.commit_time)
                    .map_err(|_| s.stall(Cause::Unknown))?;
                (d, None, commit.commit_time)
            }
        };
        if twice(p) {
            match c.dups.get(&p) {
                Some(&old) if old != digest => return Err(RunError::Conflict(p)),
                Some(_) => {
                    let a = &mut c.expect.anchors;
                    a.positions = a.positions.wrapping_sub(1);
                    if let Some((sid, _)) = series {
                        let n = a.facts.entry(sid).or_default();
                        *n = n.wrapping_sub(1);
                    }
                    continue;
                }
                None => {
                    c.dups.insert(p, digest);
                }
            }
        }
        match series {
            Some((sid, text)) => {
                let sum = c.expect.sums.entry(sid).or_default();
                *sum = sum.wrapping_add(digest);
                let e = c.bytes.entry(sid).or_insert((0, 0, when, when));
                *e = (e.0 + row_cost(text), e.1 + 1, e.2.min(when), e.3.max(when));
            }
            None => c.expect.entries = c.expect.entries.wrapping_add(digest),
        }
        if period(when, i64::MIN, &BTreeSet::new()).is_none()
            || period(when, i64::MAX, &BTreeSet::new()).is_none()
        {
            c.extreme.push((p, when));
        } else {
            if series.is_some() {
                *c.days.entry(when.div_euclid(DAY)).or_default() += 1;
            } else {
                c.days.entry(when.div_euclid(DAY)).or_default();
            }
        }
    }
    Ok(())
}

/// Checks a segment's positions and per-series facts against its seal, and adds them to the anchors.
fn seal_counts(c: &mut Census, s: &Segment) -> Result<(), RunError> {
    let (positions, facts) = std::mem::take(&mut c.segment);
    if positions != s.sealed.positions || facts != s.sealed.facts {
        return Err(s.stall(Cause::Unreadable("counts disagree with its seal")));
    }
    let a = &mut c.expect.anchors;
    a.positions = a.positions.wrapping_add(positions);
    for (sid, n) in facts {
        let x = a.facts.entry(sid).or_default();
        *x = x.wrapping_add(n);
    }
    Ok(())
}

/// Takes the rows of one batch that fall in the pass: its group's facts in its window, and, in a window's first pass,
/// the window's entries.
#[allow(clippy::too_many_arguments)]
fn collect(
    plan: &Plan,
    weeks: &BTreeSet<i64>,
    window: &Range<usize>,
    group: &Group,
    first: bool,
    d: Decoded,
    rows: &mut Vec<PRow>,
    entries: &mut BTreeMap<usize, Vec<(Position, Entry)>>,
) -> u64 {
    let before = rows.len();
    let mut seq = d.content.first_sequence;
    for item in d.content.items {
        let p = (d.commit.generation, seq);
        seq += item.sequences();
        match item {
            Item::Fact(r) => {
                let Some(&order) = plan.order.get(&r.series_id) else {
                    continue;
                };
                let Some(pi) = plan.period_of(r.t, weeks) else {
                    continue;
                };
                if group.takes(order, r.t, d.set_aside) && window.contains(&pi) {
                    rows.push(PRow {
                        period: pi,
                        class: CURRENT,
                        order,
                        aside: d.set_aside,
                        fact: chunk_fact(&r, p),
                    });
                }
            }
            Item::Entry(e) if first => {
                if let Some(pi) = plan.period_of(d.commit.commit_time, weeks)
                    && window.contains(&pi)
                {
                    entries.entry(pi).or_default().push((p, *e));
                }
            }
            Item::Entry(_) => {}
        }
    }
    rows[before..]
        .iter()
        .map(|r| r.fact.text.capacity() as u64)
        .sum()
}

/// Ranks the pass's rows into classes (§3.1 Class), keeps a position met twice once, counts them into the files'
/// metas when `count`, and sorts them in write order.
fn place(
    plan: &mut Plan,
    rows: &mut Vec<PRow>,
    entries: &mut BTreeMap<usize, Vec<(Position, Entry)>>,
    count: bool,
) {
    // Unstable sorts hold no scratch; their keys end in the position, so equal keys are the same row met twice.
    rows.sort_unstable_by(|a, b| {
        (a.aside, a.fact.series_id, a.fact.t, rank(&a.fact)).cmp(&(
            b.aside,
            b.fact.series_id,
            b.fact.t,
            rank(&b.fact),
        ))
    });
    rows.dedup_by(|b, a| {
        a.fact.position == b.fact.position && a.fact.series_id == b.fact.series_id
    });
    for i in 0..rows.len() {
        let top = !rows[i].aside
            && rows.get(i + 1).is_none_or(|n| {
                n.aside || (n.fact.series_id, n.fact.t) != (rows[i].fact.series_id, rows[i].fact.t)
            });
        rows[i].class = if top { CURRENT } else { REVISION };
    }
    for e in entries.values_mut() {
        e.sort_unstable_by_key(|x| x.0);
        e.dedup_by_key(|x| x.0);
    }
    if count {
        for r in rows.iter() {
            let b = plan.batch_of(r.fact.position);
            plan.metas
                .entry((r.period, r.class))
                .or_default()
                .hold(r.fact.position, b);
            if r.aside {
                plan.metas
                    .get_mut(&(r.period, r.class))
                    .expect("held")
                    .set_aside = true;
            }
        }
        for (&pi, e) in entries.iter() {
            for (p, _) in e {
                let b = plan.batch_of(*p);
                let m = plan.metas.entry((pi, CURRENT)).or_default();
                m.hold(*p, b);
                m.entries = true;
            }
        }
    }
    rows.sort_unstable_by(|a, b| {
        (
            a.period,
            a.class,
            a.order,
            !a.aside,
            a.fact.t,
            a.fact.position,
        )
            .cmp(&(
                b.period,
                b.class,
                b.order,
                !b.aside,
                b.fact.t,
                b.fact.position,
            ))
    });
    if count {
        let mut last = None;
        for r in rows.iter() {
            let m = plan.metas.get_mut(&(r.period, r.class)).expect("held");
            m.rows += 1;
            let run = Some((r.period, r.class, r.order, r.aside));
            if last != run {
                m.series += 1;
                last = run;
            }
        }
    }
}

/// Orders a pass's rows by each file's own names, which differ between files when a series is renamed in the inputs.
fn by_file_names(
    input: &RunInput<'_>,
    defs: &BTreeMap<u32, Vec<(Position, Definition)>>,
    plan: &Plan,
    rows: &mut [PRow],
) {
    let mut named: BTreeMap<(usize, u8, u32), (bool, Option<String>)> = BTreeMap::new();
    for r in rows.iter() {
        let key = (r.period, r.class, r.fact.series_id);
        named.entry(key).or_insert_with(|| {
            let name = info(input, defs, key.2, plan.metas[&(key.0, key.1)].greatest).0;
            (name.is_none(), name)
        });
    }
    let mut sorted: Vec<_> = named.into_iter().collect();
    sorted.sort_by(|a, b| {
        let k = |x: &((usize, u8, u32), (bool, Option<String>))| {
            (
                x.0.0,
                x.0.1,
                x.1.0,
                x.1.1.clone().map(String::into_bytes),
                x.0.2,
            )
        };
        k(a).cmp(&k(b))
    });
    let order: BTreeMap<(usize, u8, u32), usize> = sorted
        .into_iter()
        .enumerate()
        .map(|(i, (k, _))| (k, i))
        .collect();
    for r in rows.iter_mut() {
        r.order = order[&(r.period, r.class, r.fact.series_id)];
    }
    rows.sort_unstable_by(|a, b| {
        (
            a.period,
            a.class,
            a.order,
            !a.aside,
            a.fact.t,
            a.fact.position,
        )
            .cmp(&(
                b.period,
                b.class,
                b.order,
                !b.aside,
                b.fact.t,
                b.fact.position,
            ))
    });
}

/// Creates output `i` and its writer.
fn new_output(input: &RunInput<'_>, plan: &Plan, i: usize) -> Result<Open, RunError> {
    let key = plan.files[i];
    let meta = &plan.metas[&key];
    let (start, len) = plan.periods[key.0];
    let id = Identity {
        store_id: input.store_id,
        generation: input.generation,
        file_id: input.first_file_id + i as u64,
    };
    let description = Description {
        class: if key.1 == CURRENT {
            Class::Current
        } else {
            Class::Revision
        },
        level: Level::L0,
        period_start: start,
        period_len: len,
        run_id: input.first_file_id,
        run_count: u32::try_from(plan.files.len())
            .map_err(|_| io::Error::other("too many outputs"))?,
        inputs: plan.names.clone(),
        name_range: None,
        positions: meta.positions.iter().map(|(&g, &n)| (g, n)).collect(),
        has_entries: meta.entries,
        has_set_aside: meta.set_aside,
        complete: true,
    };
    let batches: Vec<Batch> = plan
        .batches
        .iter()
        .enumerate()
        .filter(|(b, _)| {
            meta.used
                .get(b / 64)
                .is_some_and(|w| w & (1 << (b % 64)) != 0)
        })
        .map(|(_, b)| *b)
        .collect();
    let mut w = FileWriter::new(Vec::new(), id, input.created_at, &description, batches)?;
    w.reserve_index(usize::try_from(meta.series + meta.rows / CHUNK_ROWS as u64).unwrap_or(0));
    Ok(Open {
        file: i,
        id,
        w: Some(w),
        tail: Vec::new(),
        out: None,
        removed: false,
        path: tmp_path(input.dir, &id),
        at: 0,
        unsynced: 0,
        series: None,
        finishing: false,
        written: None,
    })
}

fn pending(o: &Open) -> usize {
    o.w.as_ref().map_or(o.tail.len(), |w| w.sink().len())
}

/// Writes at most one piece of an output's sink to its file.
fn drain(o: &mut Open) -> io::Result<()> {
    let b = match &mut o.w {
        Some(w) => w.sink_mut(),
        None => &mut o.tail,
    };
    let n = b.len().min(PIECE);
    o.out.as_ref().expect("opened").write_at(&b[..n], o.at)?;
    b.drain(..n);
    o.at += n as u64;
    o.unsynced += n as u64;
    Ok(())
}

/// Pushes rows of the pass to their writers until one data block is encoded or a series ends.
fn write_rows(
    input: &RunInput<'_>,
    defs: &BTreeMap<u32, Vec<(Position, Definition)>>,
    plan: &Plan,
    open: &mut [Open],
    rows: &mut Rows,
) -> Result<(), RunError> {
    let r = &rows.rows[rows.next];
    let file = plan
        .files
        .binary_search(&(r.period, r.class))
        .expect("counted");
    let o = open.iter_mut().find(|o| o.file == file).expect("opened");
    let w = o.w.as_mut().expect("open");
    let series = r.fact.series_id;
    if o.series != Some(series) {
        if o.series.take().is_some() {
            w.end_series()?;
        }
        let greatest = plan.metas[&plan.files[file]].greatest;
        let (name, reach) = info(input, defs, series, greatest);
        let mut aside = Vec::new();
        while let Some(r) = rows.rows.get(rows.next).filter(|r| {
            r.aside && r.fact.series_id == series && plan.files[file] == (r.period, r.class)
        }) {
            aside.push(r.fact.clone());
            rows.next += 1;
        }
        w.begin_series(series, name.as_deref(), reach, aside)?;
        o.series = Some(series);
        return Ok(());
    }
    let blocks = w.data_blocks();
    while let Some(r) = rows.rows.get(rows.next) {
        if r.fact.series_id != series || plan.files[file] != (r.period, r.class) {
            break;
        }
        w.push(r.fact.clone())?;
        rows.next += 1;
        if w.data_blocks() != blocks || w.sink_mut().len() >= PIECE {
            break;
        }
    }
    Ok(())
}
