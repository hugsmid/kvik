//! Rebuilds (design 7.1b section 5 Rebuild), merges as the writer runs them, the scrub, and the roll-up ancestors a
//! checkpoint recomputes where a period is merged (section 5 step 3).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kvik_log::header::FileHeader;
use kvik_log::writer::Position;

use super::{Fact, TreeFiles, WriteError, part_name, quarter_entries, read_file};
use crate::level::Level;
use crate::node::Params;
use crate::part::{self, Class, Entry, Part, Run};
use crate::root::{Covered, PartRef, Retired};
use crate::set::{
    self, DAY, Key, Policy, QUARTER, cover_levels, merge::ceil, merge::normalise, merge::rolled,
    period_overlaps, rollups,
};

/// Entries per `(series, level)`, each list in ascending start.
pub type Runs = BTreeMap<(u64, u8), Vec<Entry>>;

/// What one rebuild publishes, in one root at the current K: its parts, the live parts it replaces, the ranges it
/// adds to `covered` and the series whose open floor it closes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rebuild {
    /// Per part period `(start, length)`, the entries per `(series, level)` in ascending start.
    pub parts: BTreeMap<(i64, u64), Runs>,
    /// Live parts it retires.
    pub retire: Vec<u64>,
    /// Ranges added to `covered`.
    pub cover: Vec<Covered>,
    /// Series whose open floor closes: the tree serves them again from the parts above the floor.
    pub close: Vec<u64>,
}

impl Rebuild {
    /// Adds a series' entries of one level to the part of `period`.
    pub fn add(&mut self, period: (i64, u64), series: u64, level: u8, e: Entry) {
        let v = self
            .parts
            .entry(period)
            .or_default()
            .entry((series, level))
            .or_default();
        debug_assert!(v.last().is_none_or(|l| l.start < e.start));
        v.push(e);
    }
}

fn width(levels: &[Level], id: u8) -> i64 {
    levels
        .iter()
        .find(|l| l.id == id)
        .map_or(QUARTER, |l| l.width)
}

/// The entries a rebuild writes for one series from its current facts (ascending `t`, one per `t`): the levels of
/// `stored` per quarter-hour holding a fact, then, when the quarter-hour level is stored, each roll-up of `levels`
/// over its children (T2); a span only where `keep(level, start, end)`, empty nodes left out. Sorted by level and
/// start.
pub fn series_entries(
    levels: &[Level],
    stored: &[Level],
    p: &Params,
    facts: &[Fact],
    keep: impl Fn(u8, i64, i64) -> bool,
) -> Vec<(u8, Entry)> {
    let mut out: BTreeMap<(u8, i64), Entry> = BTreeMap::new();
    let mut i = 0;
    while i < facts.len() {
        let q = facts[i].t - facts[i].t.rem_euclid(QUARTER);
        let j = i + facts[i..].partition_point(|f| f.t < q + QUARTER);
        for (l, e) in quarter_entries(p, stored, q, &facts[i..j]) {
            if e.node.is_some() && keep(l, e.start, e.start + width(levels, l)) {
                out.insert((l, e.start), e);
            }
        }
        i = j;
    }
    if stored.iter().any(|l| l.width == QUARTER) {
        let mut pairs = rollups(levels);
        pairs.reverse();
        for (parent, child) in pairs {
            let mut groups: BTreeMap<i64, Vec<&Entry>> = BTreeMap::new();
            for (_, e) in out.range((child.id, i64::MIN)..=(child.id, i64::MAX)) {
                groups
                    .entry(e.start - e.start.rem_euclid(parent.width))
                    .or_default()
                    .push(e);
            }
            let made: Vec<Entry> = groups
                .into_iter()
                .filter_map(|(x, ch)| {
                    let end = x.checked_add(parent.width)?;
                    keep(parent.id, x, end).then(|| {
                        rolled(p, 0, x, end, &ch)
                            .expect("children folded under the same parameters")
                    })
                })
                .collect();
            out.extend(made.into_iter().map(|e| ((parent.id, e.start), e)));
        }
    }
    out.into_iter().map(|((l, _), e)| (l, e)).collect()
}

fn lo_bound(w: i64) -> i64 {
    ceil(i64::MIN, w)
}

fn hi_bound(w: i64) -> i64 {
    i64::MAX - i64::MAX.rem_euclid(w)
}

/// The ranges a backfill of `[a, b)` adds to `covered` (`None` for no bound): the quarter-hour level over it, finer
/// levels from `now` less `minute_keep` (Q-1420), and the roll-ups over the whole UTC days in it whose late window
/// has passed at `now`, which the planner then finds merged.
pub fn backfill_cover(
    levels: &[Level],
    a: Option<i64>,
    b: Option<i64>,
    now: i64,
    policy: &Policy,
) -> Vec<Covered> {
    let settled = {
        let t = now.saturating_sub(policy.late_window);
        t - t.rem_euclid(DAY)
    };
    let mut out = Vec::new();
    for l in levels {
        let lo = a.map_or(lo_bound(l.width), |a| ceil(a, l.width));
        let hi = b.map_or(hi_bound(l.width), |b| b - b.rem_euclid(l.width));
        let (from, to) = if l.width < QUARTER {
            (
                lo.max(ceil(now.saturating_sub(policy.minute_keep), l.width)),
                hi,
            )
        } else if l.width == QUARTER {
            (lo, hi)
        } else if DAY % l.width == 0 {
            (ceil(lo, DAY), hi.min(settled))
        } else {
            continue;
        };
        if from < to {
            out.push(Covered {
                level: l.id,
                from,
                to,
            });
        }
    }
    out
}

fn end(r: &PartRef) -> i64 {
    r.period_start.saturating_add(r.period_len as i64)
}

impl TreeFiles {
    /// Publishes `r` one part period a root at `k`, newest first (Q-2112), so that a stop loses at most the period in
    /// hand. `cover(from)` is what `covered` gains once the periods from `from` up are published, `None` for the last
    /// root, which also takes the retired parts and the closed floors. The first root raises the floors it closes
    /// to its own `seq`.
    pub fn rebuild_newest_first(
        &mut self,
        k: Position,
        r: Rebuild,
        cover: impl Fn(Option<i64>) -> Vec<Covered>,
        wall: i64,
        now: Duration,
    ) -> Result<(), WriteError> {
        let Rebuild {
            parts,
            retire,
            close,
            ..
        } = r;
        let mut periods: Vec<_> = parts.into_iter().collect();
        let oldest = periods.first().cloned();
        // A stopped attempt's parts sit above the open floor at an older K; an entry this attempt leaves out (a node
        // now empty) would stay visible once the floor closes (T1).
        let mut raise = close.clone();
        for (period, runs) in periods.drain(..).skip(1).rev() {
            let one = Rebuild {
                parts: BTreeMap::from([(period, runs)]),
                cover: cover(Some(period.0)),
                ..Rebuild::default()
            };
            self.publish_rebuild(k, one, &std::mem::take(&mut raise), wall, now)?;
        }
        let last = Rebuild {
            parts: oldest.into_iter().collect(),
            retire,
            cover: cover(None),
            close,
        };
        self.publish_rebuild(k, last, &raise, wall, now)
    }

    /// Publishes a rebuild at `k`, which must be the root's K (section 5 Rebuild): its parts, of class rebuilt and of
    /// one new `seq`, the parts it retires, its `covered` ranges and its closed floors, in one root.
    pub fn rebuild(
        &mut self,
        k: Position,
        r: Rebuild,
        wall: i64,
        now: Duration,
    ) -> Result<(), WriteError> {
        self.publish_rebuild(k, r, &[], wall, now)
    }

    /// [`TreeFiles::rebuild`], with the open floors of `raise` moved up to the new root's `seq`.
    fn publish_rebuild(
        &mut self,
        k: Position,
        r: Rebuild,
        raise: &[u64],
        wall: i64,
        now: Duration,
    ) -> Result<(), WriteError> {
        if k != self.root.checkpoint {
            return Err(WriteError::NotAtCheckpoint {
                k,
                root: self.root.checkpoint,
            });
        }
        let seq = self.next_seq;
        let mut parts = Vec::new();
        for ((period_start, period_len), runs) in r.parts {
            let runs: Vec<Run> = runs
                .into_iter()
                .filter(|(_, es)| !es.is_empty())
                .map(|((series, level), entries)| Run {
                    series,
                    level,
                    entries,
                })
                .collect();
            if runs.is_empty() {
                continue;
            }
            let file_id = self.take_file_id();
            parts.push(Part {
                header: FileHeader::new(
                    part::KIND,
                    self.id.generation,
                    file_id,
                    wall,
                    self.id.store_id,
                ),
                seq,
                class: Class::Rebuilt,
                period_start,
                period_len,
                through: k,
                levels: self.root.levels.clone(),
                runs,
            });
        }
        let mut next = self.root.clone();
        next.parts.retain(|p| !r.retire.contains(&p.file_id));
        let live = |id: &&u64| self.root.parts.iter().any(|p| p.file_id == **id);
        next.retired
            .extend(r.retire.iter().filter(live).map(|&file_id| Retired {
                file_id,
                retired_at: wall,
            }));
        next.retired.sort_by_key(|x| x.file_id);
        next.retired.dedup_by_key(|x| x.file_id);
        next.covered.extend(r.cover);
        next.covered = normalise(std::mem::take(&mut next.covered));
        for f in &mut next.floors {
            if f.rebuilding && raise.contains(&f.series) {
                f.seq = seq;
            }
            if r.close.contains(&f.series) {
                f.rebuilding = false;
            }
        }
        self.publish(next, parts, wall, now)
    }

    /// A live part read and checked against the root's record of it: length, decode and every field of the record.
    pub fn load(&self, r: &PartRef) -> Option<Part> {
        let b = read_file(
            &*self.fs,
            &self.dir.join(part_name(self.id.generation, r.file_id)),
        )
        .ok()?;
        if b.len() as u64 != r.byte_len {
            return None;
        }
        let p = Part::decode(&b).ok()?;
        (set::part_ref(&p, &b) == *r).then_some(p)
    }

    /// The scrub (section 7): the live parts that are missing, short, corrupt or unlike the root's record of them.
    pub fn scrub(&self) -> Vec<u64> {
        self.root
            .parts
            .iter()
            .filter(|r| self.load(r).is_none())
            .map(|r| r.file_id)
            .collect()
    }

    /// The period a damaged part's rebuild covers and the live parts it replaces: the part's period, widened until
    /// no live part overlaps it partly, so that the rebuild is the whole truth of its period.
    pub fn damaged_period(&self, file_id: u64) -> Option<((i64, u64), Vec<u64>)> {
        let r = self.root.parts.iter().find(|r| r.file_id == file_id)?;
        let (mut a, mut b) = (r.period_start, end(r));
        loop {
            let over: Vec<&PartRef> = self
                .root
                .parts
                .iter()
                .filter(|p| p.period_start < b && a < end(p))
                .collect();
            let na = over.iter().map(|p| p.period_start).min().unwrap_or(a);
            let nb = over.iter().map(|p| end(p)).max().unwrap_or(b);
            if (na, nb) == (a, b) {
                let len = u64::try_from(i128::from(b) - i128::from(a)).ok()?;
                return Some(((a, len), over.iter().map(|p| p.file_id).collect()));
            }
            (a, b) = (na, nb);
        }
    }

    /// Runs the merges due at `at` (section 5 Merges), one root each, at most `max`; returns how many ran. A damaged
    /// input stops it with [`WriteError::Damaged`], for the scrub's rebuild.
    pub fn merge_due<F>(
        &mut self,
        at: i64,
        policy: &Policy,
        params: F,
        max: usize,
        wall: i64,
        now: Duration,
    ) -> Result<usize, WriteError>
    where
        F: Fn(u64) -> Option<Params>,
    {
        let mut n = 0;
        while n < max {
            let Some(plan) = set::plan(&self.root, at, policy).into_iter().next() else {
                break;
            };
            let spans: Vec<(i64, u64)> = self
                .root
                .parts
                .iter()
                .filter(|r| plan.inputs.contains(&r.file_id))
                .map(|r| (r.period_start, r.period_len))
                .chain(plan.periods.iter().copied())
                .collect();
            let mut view = self.root.clone();
            view.parts.retain(|r| {
                plan.inputs.contains(&r.file_id)
                    || spans.iter().any(|&(s, l)| period_overlaps(r, s, l))
            });
            view.next_file_id = self.next_file;
            let parts = self.load_all(&view.parts)?;
            let template =
                FileHeader::new(part::KIND, self.id.generation, 0, wall, self.id.store_id);
            let out = set::merge(&view, &parts, &plan, &template, &params)?;
            for _ in &out {
                self.take_file_id();
            }
            let next = set::next_root(&self.root, &plan, &[], wall);
            self.publish(next, out, wall, now)?;
            n += 1;
        }
        Ok(n)
    }

    fn load_all(&self, refs: &[PartRef]) -> Result<Vec<Part>, WriteError> {
        refs.iter()
            .map(|r| self.load(r).ok_or(WriteError::Damaged(r.file_id)))
            .collect()
    }

    /// The roll-up entries a checkpoint of `entries` must also write (section 5 step 3, T2): each roll-up span over a
    /// changed quarter-hour entry that is covered, recomputed from the visible children overlaid by `entries`; an
    /// empty marker where no child is left. A span over a part that will not load is marked stale, and those parts
    /// are returned for a repair at the same cut (Q-2111).
    pub fn ancestors<F>(&self, entries: &Runs, params: F) -> Result<(Runs, Vec<u64>), WriteError>
    where
        F: Fn(u64) -> Option<Params>,
    {
        let root = &self.root;
        let mut pairs = rollups(&root.levels);
        pairs.reverse();
        let Some(base) = cover_levels(&root.levels).last().copied() else {
            return Ok((BTreeMap::new(), Vec::new()));
        };
        let mut affected: BTreeSet<Key> = BTreeSet::new();
        for ((s, level), es) in entries {
            if *level != base.id {
                continue;
            }
            for e in es {
                for (parent, _) in &pairs {
                    let x = e.start - e.start.rem_euclid(parent.width);
                    if x.checked_add(parent.width)
                        .is_some_and(|end| set::covered(root, parent.id, x, end))
                    {
                        affected.insert((*s, parent.id, x));
                    }
                }
            }
        }
        if affected.is_empty() {
            return Ok((BTreeMap::new(), Vec::new()));
        }
        let spans: Vec<(i64, u64)> = affected
            .iter()
            .map(|&(_, l, x)| (x, width(&root.levels, l) as u64))
            .collect();
        let mut view = root.clone();
        view.parts
            .retain(|r| spans.iter().any(|&(s, l)| period_overlaps(r, s, l)));
        // Whole parts are read and decoded; a part directory read (roadmap 7.1b step 5) narrows this.
        let mut parts = Vec::new();
        let mut damaged = Vec::new();
        view.parts.retain(|r| match self.load(r) {
            Some(p) => {
                parts.push(p);
                true
            }
            None => {
                damaged.push(*r);
                false
            }
        });
        let series: BTreeSet<u64> = affected.iter().map(|k| k.0).collect();
        let mut work: BTreeMap<Key, Entry> = set::visible(&view, &parts)
            .map_err(set::MergeError::from)?
            .into_iter()
            .filter(|(k, _)| series.contains(&k.0))
            .map(|((s, l, start), seen)| {
                (
                    (s, l, start),
                    Entry {
                        start,
                        stale: seen.stale,
                        as_of: seen.as_of,
                        node: seen.node,
                    },
                )
            })
            .collect();
        for ((s, level), es) in entries {
            if !series.contains(s) {
                continue;
            }
            for e in es {
                if e.node.is_none() && !e.stale {
                    work.remove(&(*s, *level, e.start));
                } else {
                    work.insert((*s, *level, e.start), e.clone());
                }
            }
        }
        let mut out: Runs = BTreeMap::new();
        for s in series {
            let p = params(s).ok_or(set::MergeError::NoParams(s))?;
            for (parent, child) in &pairs {
                let xs: Vec<i64> = affected
                    .range((s, parent.id, i64::MIN)..=(s, parent.id, i64::MAX))
                    .map(|k| k.2)
                    .collect();
                for x in xs {
                    let end = x + parent.width;
                    let children: Vec<&Entry> = work
                        .range((s, child.id, x)..(s, child.id, end))
                        .map(|(_, e)| e)
                        .collect();
                    let e = if damaged
                        .iter()
                        .any(|r| period_overlaps(r, x, parent.width as u64))
                    {
                        Entry {
                            start: x,
                            stale: true,
                            as_of: children.iter().map(|c| c.as_of).max().unwrap_or_default(),
                            node: None,
                        }
                    } else if children.is_empty() {
                        Entry {
                            start: x,
                            stale: false,
                            as_of: Position::default(),
                            node: None,
                        }
                    } else {
                        rolled(&p, s, x, end, &children)?
                    };
                    if e.node.is_none() && !e.stale {
                        work.remove(&(s, parent.id, x));
                    } else {
                        work.insert((s, parent.id, x), e.clone());
                    }
                    out.entry((s, parent.id)).or_default().push(e);
                }
            }
        }
        Ok((out, damaged.iter().map(|r| r.file_id).collect()))
    }
}
