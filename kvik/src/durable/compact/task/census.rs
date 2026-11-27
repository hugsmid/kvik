//! The census over every input and the passes planned from it (compaction design §4 Memory; Q-1536, Q-1537, Q-1840).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kvik_chunk::file::{BLOCK_ROWS, Batch, Fact, IndexEntry, Input, Position};
use kvik_log::content::Item;
use kvik_log::entries::{Definition, Entry};

use super::super::run::info;
use super::super::segment::{Decoded, Event, PIECE, Segment};
use super::super::verify::Expect;
use super::super::{
    Cause, KIND_LOG, Output, RunError, chunk_fact, entry_digest, fact_digest, period,
};

use super::passes::{Meta, Rows, row_cost};
use super::{FIXED, RunTask, Stage};

/// Partitions one pass writes at most (§3.1 Bounded outputs).
const MAX_OPEN: usize = 16;
/// What a writer holds for a commit-time row (Q-1840): the row and its count of rows held (`FileWriter::held_bytes`).
const BATCH_ROW: u64 = std::mem::size_of::<Batch>() as u64 + 8;
/// What a writer's block being built holds beyond its sink: a block of rows as `FileWriter::held_bytes` counts them.
const BLOCK_RESERVE: u64 = (BLOCK_ROWS * (std::mem::size_of::<Fact>() + 255)) as u64;
/// The least a group may hold, so a census near the cap does not split every series; a cap of 0 makes every series
/// its own pass, split as finely as it goes.
const MIN_GROUP: u64 = 1 << 20;
const DAY: i64 = 86_400_000_000_000;

/// Everything the census took from the inputs.
#[derive(Default)]
pub(super) struct Census {
    pub(super) expect: Expect,
    pub(super) batches: BTreeMap<Position, Batch>,
    pub(super) defs: BTreeMap<u32, Vec<(Position, Definition)>>,
    /// Each series' row bytes, rows and least and greatest `t`.
    pub(super) bytes: BTreeMap<u32, (u64, u64, i64, i64)>,
    /// Rows a day.
    pub(super) days: BTreeMap<i64, u64>,
    /// Digests of positions inside two inputs' ranges, so a position met twice is compared and kept once.
    pub(super) dups: BTreeMap<Position, u128>,
    /// Times whose period may leave `i64`, decided once the last commit time is known.
    pub(super) extreme: Vec<(Position, i64)>,
    pub(super) ranges: Vec<(Position, Position, u32, u64)>,
    pub(super) segment: (u64, BTreeMap<u32, u64>),
}

/// What a `BTreeMap` of `n` entries allocates: nodes are about half full after ordered inserts (Q-1841).
pub(super) fn tree<K, V>(n: usize) -> u64 {
    (n * (2 * (std::mem::size_of::<K>() + std::mem::size_of::<V>()) + 16)) as u64
}

impl Census {
    pub(super) fn held(&self) -> u64 {
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
pub(super) struct Group {
    pub(super) series: Range<usize>,
    pub(super) t: Option<TRange>,
    /// Its rows; for a range of `t`, a share of the series' rows with a quarter to spare.
    pub(super) rows: u64,
}

/// One range of `t` of a series split across passes.
#[derive(Clone, Copy, Debug)]
pub(super) struct TRange {
    pub(super) lo: i64,
    pub(super) hi: i128,
    pub(super) first: bool,
    pub(super) last: bool,
}

impl Group {
    pub(super) fn takes(&self, order: usize, t: i64, aside: bool) -> bool {
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
    pub(super) fn continues(&self) -> bool {
        self.t.is_some_and(|r| !r.last)
    }
}

/// The most passes one series is split into.
pub(super) const MAX_SPLIT: i128 = 64;

#[derive(Clone, Copy)]
pub(super) struct Pass {
    pub(super) window: usize,
    pub(super) group: usize,
    pub(super) count: bool,
    pub(super) write: bool,
}

/// The passes, from the census.
pub(super) struct Plan {
    pub(super) batches: Vec<Batch>,
    pub(super) last_commit_time: i64,
    pub(super) periods: Vec<(i64, i64)>,
    pub(super) order: BTreeMap<u32, usize>,
    pub(super) groups: Vec<Group>,
    pub(super) windows: Vec<Range<usize>>,
    /// Rows a period.
    pub(super) period_rows: Vec<u64>,
    /// Rows each window and group took in its count pass, so its write pass holds them exactly.
    pub(super) taken: BTreeMap<(usize, usize), usize>,
    pub(super) passes: Vec<Pass>,
    pub(super) metas: BTreeMap<(usize, u8), Meta>,
    pub(super) files: Vec<(usize, u8)>,
    pub(super) names: Vec<Input>,
    /// A series renamed in the inputs: one group, and each file's own name order (Q-1412).
    pub(super) renamed: bool,
}

impl Plan {
    pub(super) fn held(&self) -> u64 {
        (self.batches.capacity() * std::mem::size_of::<Batch>()) as u64
            + tree::<u32, usize>(self.order.len())
            + tree::<(usize, u8), Meta>(self.metas.len())
            + self
                .metas
                .values()
                .map(|m| tree::<u32, u64>(m.positions.len()) + m.used.capacity() as u64 * 8)
                .sum::<u64>()
    }

    pub(super) fn period_of(&self, t: i64, weeks: &BTreeSet<i64>) -> Option<usize> {
        let p = period(t, self.last_commit_time, weeks)?;
        self.periods.binary_search(&p).ok()
    }

    pub(super) fn batch_of(&self, p: Position) -> usize {
        self.batches
            .partition_point(|b| (b.generation, b.last_sequence) < p)
    }
}

impl RunTask<'_> {
    /// One census step: opens the next input, or takes one batch of it, or plans the passes after the last.
    pub(super) fn census_step(&mut self) -> Result<Option<Vec<Output>>, RunError> {
        let Stage::Census { seg, segment } = &mut self.stage else {
            unreachable!("in the census")
        };
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

    /// Turns the census into passes (compaction design §4 Memory).
    pub(super) fn plan(&mut self) -> Result<(), RunError> {
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
}

/// Partitions a window opens and what each of its writers may hold, for a census of `series` and `batches`
/// (Q-1840, Q-1940): their index entries and commit-time rows fit a quarter of `cap`, their whole writers half.
pub(in super::super) fn window(cap: u64, series: u64, batches: u64) -> (usize, u64) {
    let per_file = series * 2 * std::mem::size_of::<IndexEntry>() as u64 + batches * BATCH_ROW;
    let per_writer = per_file + 2 * PIECE as u64 + BLOCK_RESERVE;
    let open = (cap / 4 / per_file.max(1))
        .min(cap / 2 / per_writer)
        .clamp(1, MAX_OPEN as u64) as usize;
    (open, per_writer)
}

/// The census's share of one batch: digests, counts, commit-time row, definitions, sizes and days.
pub(super) fn take(c: &mut Census, s: &Segment, d: Decoded) -> Result<(), RunError> {
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
pub(super) fn seal_counts(c: &mut Census, s: &Segment) -> Result<(), RunError> {
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
