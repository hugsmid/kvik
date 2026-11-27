//! The visible state of a tree root (design 7.1b section 3.3), the cover of a span (section 2), the part merge
//! and the merge planner (section 5). Pure functions over decoded roots and parts; files are the writer's concern.

pub(crate) mod merge;
mod plan;

use std::collections::BTreeMap;

use kvik_log::writer::Position;

pub use merge::{MergeError, merge, next_root, part_ref};
pub use plan::{DAY, Plan, Policy, Tier, plan, settled_period};

use crate::level::{Level, MINUTE};
use crate::node::Node;
use crate::part::{Entry, Part};
use crate::root::{PartRef, Root};

/// The unit of dirtiness and of cover edges (design section 2): levels at least this wide are cover levels, the
/// wider ones roll-ups of the next finer; narrower levels are folded from facts.
pub const QUARTER: i64 = 15 * MINUTE;

/// `(series, level, start)`, the key of a visible entry.
pub type Key = (u64, u8, i64);

/// A visible entry: node bytes, version and stale mark (section 3.3).
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    /// The node version (Q-1422).
    pub as_of: Position,
    /// `summary_stale`: the reader takes facts.
    pub stale: bool,
    /// The node; `None` only with `stale`.
    pub node: Option<Node>,
}

/// Why a visible state could not be formed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    /// The root lists a part that was not supplied.
    #[error("root lists part {0:#x}, not supplied")]
    Missing(u64),
    /// Two live parts of one `seq` hold the same key, so neither overlays the other.
    #[error("parts of seq {seq} both hold series {series} level {level} at {start}")]
    SameSeq {
        /// The shared `seq`.
        seq: u64,
        /// Series id.
        series: u64,
        /// Level id.
        level: u8,
        /// Span start.
        start: i64,
    },
}

/// Whether entries of a part of `seq` are visible for `series` under the root's floors (section 3.2).
pub fn above_floors(root: &Root, series: u64, seq: u64) -> bool {
    seq >= root.floor
        && root
            .floors
            .binary_search_by_key(&series, |f| f.series)
            .map_or(true, |i| seq >= root.floors[i].seq)
}

/// Whether `series` is served from the tree: not under an open floor.
pub fn served(root: &Root, series: u64) -> bool {
    root.floors
        .binary_search_by_key(&series, |f| f.series)
        .map_or(true, |i| !root.floors[i].rebuilding)
}

/// Whether `[from, to)` lies inside one covered range of `level`.
pub fn covered(root: &Root, level: u8, from: i64, to: i64) -> bool {
    root.covered
        .iter()
        .any(|c| c.level == level && c.from <= from && to <= c.to)
}

/// `visible(R)` (section 3.3): per key, the entry of the live part with the highest `seq`, floors applied, inside
/// `covered`, empty markers left out. Parts the root does not list are ignored.
pub fn visible(root: &Root, parts: &[Part]) -> Result<BTreeMap<Key, Seen>, SetError> {
    let live = live_parts(root, parts)?;
    Ok(seen(root, overlay(root, &live)?))
}

/// The entries of `visible(R)` of one series by `(level, start)`, borrowed from the root's live parts (a reader's
/// state, design section 6).
pub fn visible_entries<'a>(
    root: &Root,
    live: &[&'a Part],
    series: u64,
) -> Result<BTreeMap<(u8, i64), &'a Entry>, SetError> {
    Ok(overlay_refs(root, live, Some(series))?
        .into_iter()
        .filter(|(k, (_, e))| keep(root, *k, e))
        .map(|((_, level, start), (_, e))| ((level, start), e))
        .collect())
}

fn seen(root: &Root, won: BTreeMap<Key, (u64, Entry)>) -> BTreeMap<Key, Seen> {
    won.into_iter()
        .filter(|(k, (_, e))| keep(root, *k, e))
        .map(|(k, (_, e))| {
            (
                k,
                Seen {
                    as_of: e.as_of,
                    stale: e.stale,
                    node: e.node,
                },
            )
        })
        .collect()
}

/// Whether an overlay's winning entry is visible: its series served, a node or a stale mark, inside `covered`.
fn keep(root: &Root, (series, level, start): Key, e: &Entry) -> bool {
    width(&root.levels, level).is_some_and(|w| {
        served(root, series)
            && (e.stale || e.node.is_some())
            && covered(root, level, start, start + w)
    })
}

pub(crate) fn live_parts<'a>(root: &Root, parts: &'a [Part]) -> Result<Vec<&'a Part>, SetError> {
    root.parts
        .iter()
        .map(|r| {
            parts
                .iter()
                .find(|p| p.header.file_id == r.file_id)
                .ok_or(SetError::Missing(r.file_id))
        })
        .collect()
}

/// Per key the highest-`seq` entry of `parts` above the floors, empty markers included; two entries of the winning
/// `seq` are refused, whatever the order of `parts`.
pub(crate) fn overlay(
    root: &Root,
    parts: &[&Part],
) -> Result<BTreeMap<Key, (u64, Entry)>, SetError> {
    Ok(overlay_refs(root, parts, None)?
        .into_iter()
        .map(|(k, (s, e))| (k, (s, e.clone())))
        .collect())
}

/// [`overlay`] borrowing its entries, over the runs of `series` alone or of every series.
fn overlay_refs<'a>(
    root: &Root,
    parts: &[&'a Part],
    series: Option<u64>,
) -> Result<BTreeMap<Key, (u64, &'a Entry)>, SetError> {
    let mut won: BTreeMap<Key, (u64, &'a Entry, bool)> = BTreeMap::new();
    for p in parts {
        let runs = match series {
            // Runs are in ascending `(series, level)`.
            Some(s) => {
                let lo = p.runs.partition_point(|r| r.series < s);
                let hi = p.runs.partition_point(|r| r.series <= s);
                &p.runs[lo..hi]
            }
            None => &p.runs[..],
        };
        for run in runs {
            if !above_floors(root, run.series, p.seq) {
                continue;
            }
            for e in &run.entries {
                let k = (run.series, run.level, e.start);
                match won.get_mut(&k) {
                    Some((s, _, _)) if *s > p.seq => {}
                    Some((s, _, tied)) if *s == p.seq => *tied = true,
                    _ => {
                        won.insert(k, (p.seq, e, false));
                    }
                }
            }
        }
    }
    if let Some((&(series, level, start), (seq, _, _))) = won.iter().find(|(_, v)| v.2) {
        return Err(SetError::SameSeq {
            seq: *seq,
            series,
            level,
            start,
        });
    }
    Ok(won.into_iter().map(|(k, (s, e, _))| (k, (s, e))).collect())
}

/// A live part outside `inputs` that a merge's output of the inputs' highest `seq` would reorder or share a `seq`
/// with (Q-1554, Q-1540): its `seq` lies in the inputs' range, its period overlaps an input's or an output period,
/// and its series range overlaps the inputs'.
pub(crate) fn interleaved(root: &Root, inputs: &[u64], periods: &[(i64, u64)]) -> Option<u64> {
    let ins: Vec<&PartRef> = root
        .parts
        .iter()
        .filter(|r| inputs.contains(&r.file_id))
        .collect();
    let lo = ins.iter().map(|r| r.seq).min()?;
    let hi = ins.iter().map(|r| r.seq).max()?;
    let s_lo = ins.iter().map(|r| r.least_series).min()?;
    let s_hi = ins.iter().map(|r| r.greatest_series).max()?;
    let spans: Vec<(i64, u64)> = ins
        .iter()
        .map(|r| (r.period_start, r.period_len))
        .chain(periods.iter().copied())
        .collect();
    root.parts
        .iter()
        .find(|r| {
            !inputs.contains(&r.file_id)
                && (lo..=hi).contains(&r.seq)
                && r.least_series <= s_hi
                && s_lo <= r.greatest_series
                && spans.iter().any(|&(s, l)| period_overlaps(r, s, l))
        })
        .map(|r| r.file_id)
}

pub(crate) fn period_overlaps(r: &PartRef, start: i64, len: u64) -> bool {
    r.period_start < start.saturating_add(len as i64)
        && start < r.period_start.saturating_add(r.period_len as i64)
}

pub(crate) fn width(levels: &[Level], id: u8) -> Option<i64> {
    levels.iter().find(|l| l.id == id).map(|l| l.width)
}

/// The cover levels of a table (at least [`QUARTER`] wide), widest first.
pub fn cover_levels(levels: &[Level]) -> Vec<Level> {
    let mut v: Vec<Level> = levels
        .iter()
        .copied()
        .filter(|l| l.width >= QUARTER)
        .collect();
    v.sort_by_key(|l| std::cmp::Reverse(l.width));
    v
}

/// Each roll-up level with the level it merges (section 2): a cover level above the finest, over the next finer
/// cover level, when that width divides its own. A level whose width it does not divide has no children and is
/// not written by a merge.
pub fn rollups(levels: &[Level]) -> Vec<(Level, Level)> {
    let c = cover_levels(levels);
    c.windows(2)
        .filter(|w| w[0].width % w[1].width == 0)
        .map(|w| (w[0], w[1]))
        .collect()
}

/// One tile of a cover, left to right.
#[derive(Clone, Debug, PartialEq)]
pub enum Tile<'a, S = Seen> {
    /// A covered node span: the visible entry, or `None` for an empty node.
    Node {
        /// Level id.
        level: u8,
        /// Span start.
        start: i64,
        /// Span end.
        end: i64,
        /// The visible entry; a `stale` one sends the reader to facts.
        seen: Option<&'a S>,
    },
    /// Quarter-hours no level covers, or a series not served: the reader takes facts.
    Facts {
        /// Start.
        start: i64,
        /// End.
        end: i64,
    },
}

/// `cover(series, [a, b))` (section 2): left to right, at each point the widest cover level aligned there that fits
/// before `b` and is covered; facts where none is. `a` and `b` must lie on quarter-hours with `a <= b`.
///
/// # Panics
/// Panics when `a` or `b` is off a quarter-hour or `a > b`.
pub fn cover<'a>(
    root: &Root,
    vis: &'a BTreeMap<Key, Seen>,
    series: u64,
    a: i64,
    b: i64,
) -> Vec<Tile<'a>> {
    cover_by(root, series, a, b, |level, start| {
        vis.get(&(series, level, start))
    })
}

/// [`cover`] with the visible entry of `(level, start)` given by `get`.
///
/// # Panics
/// Panics when `a` or `b` is off a quarter-hour or `a > b`.
pub fn cover_by<'a, S>(
    root: &Root,
    series: u64,
    a: i64,
    b: i64,
    get: impl Fn(u8, i64) -> Option<&'a S>,
) -> Vec<Tile<'a, S>> {
    assert!(
        a.rem_euclid(QUARTER) == 0 && b.rem_euclid(QUARTER) == 0 && a <= b,
        "cover needs quarter-hour edges in order"
    );
    if !served(root, series) {
        return if a < b {
            vec![Tile::Facts { start: a, end: b }]
        } else {
            Vec::new()
        };
    }
    let levels = cover_levels(&root.levels);
    let mut out: Vec<Tile<'a, S>> = Vec::new();
    let mut x = a;
    while x < b {
        let fit = levels.iter().find(|l| {
            x.rem_euclid(l.width) == 0
                && x.checked_add(l.width).is_some_and(|e| e <= b)
                && covered(root, l.id, x, x + l.width)
        });
        match fit {
            Some(l) => {
                out.push(Tile::Node {
                    level: l.id,
                    start: x,
                    end: x + l.width,
                    seen: get(l.id, x),
                });
                x += l.width;
            }
            None => {
                let end = x + QUARTER;
                match out.last_mut() {
                    Some(Tile::Facts { end: e, .. }) if *e == x => *e = end,
                    _ => out.push(Tile::Facts { start: x, end }),
                }
                x = end;
            }
        }
    }
    out
}
