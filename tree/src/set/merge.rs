//! The part merge (design 7.1b section 5 Merges) as a pure function, and the root that publishes it.

use std::collections::BTreeMap;

use kvik_log::header::FileHeader;

use super::{
    Key, Plan, QUARTER, SetError, cover_levels, covered, interleaved, live_parts, overlay,
    period_overlaps, rollups, width,
};
use crate::fold::merge as merge_nodes;
use crate::level::Level;
use crate::node::{Node, Params};
use crate::part::{self, Class, Entry, Part, Run};
use crate::root::{Covered, PartRef, Retired, Root};

/// Why a plan could not be merged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    /// The live parts do not form a visible state.
    #[error(transparent)]
    Set(#[from] SetError),
    /// An input is not a live part of the root, or is named twice.
    #[error("input {0:#x} is not a live part, or is named twice")]
    NotLive(u64),
    /// The plan has no inputs, or its periods are empty, unordered or overlapping.
    #[error("the plan's inputs or periods are malformed")]
    Plan,
    /// A live part outside the plan overlaps an input and has a `seq` inside the inputs', so the output's `seq`
    /// would reorder it.
    #[error("live part {0:#x} sits between the inputs' seqs")]
    Interleaved(u64),
    /// Roll-ups need every live part of the output periods among the inputs.
    #[error("roll-ups need every live part of the period among the inputs")]
    Incomplete,
    /// An input's level table gives a level another width than the root's.
    #[error("part {0:#x} disagrees with the root's level table")]
    Levels(u64),
    /// A kept entry lies in no output period.
    #[error("series {series} level {level} at {start} lies in no output period")]
    Outside {
        /// Series id.
        series: u64,
        /// Level id.
        level: u8,
        /// Span start.
        start: i64,
    },
    /// A child node is not of the kind the series' parameters give.
    #[error("series {0} has a node of another kind than its parameters")]
    Kind(u64),
    /// A roll-up merge has no parameters for a series with entries in its periods (Q-1821).
    #[error("series {0} has entries in the periods but no parameters for its roll-ups")]
    NoParams(u64),
}

/// Merges the plan's inputs into one part per output period that keeps an entry (section 5 Merges): per key the
/// highest-`seq` entry, entries under a floor dropped, entries of levels finer than [`QUARTER`] ending at or before
/// `plan.minute_from` dropped, empty markers dropped when no other live part overlaps the periods, and with
/// `plan.rollups` every roll-up recomputed from its children (invariant T2) where the finest cover level is
/// covered. The output takes the inputs' highest `seq`, so a live part outside the plan whose `seq` lies between
/// the inputs' is refused (Q-1554). Output `file_id`s count up from the root's `next_file_id`; `template` gives the rest of
/// the A.1 header. `params` gives a series' parameters; a roll-up merge refuses a series with a cover-level entry
/// in the periods and no parameters (Q-1821).
pub fn merge<F>(
    root: &Root,
    parts: &[Part],
    plan: &Plan,
    template: &FileHeader,
    params: F,
) -> Result<Vec<Part>, MergeError>
where
    F: Fn(u64) -> Option<Params>,
{
    if plan.inputs.is_empty()
        || plan.periods.iter().any(|p| p.1 == 0)
        || plan
            .periods
            .windows(2)
            .any(|w| w[0].0.saturating_add(w[0].1 as i64) > w[1].0)
    {
        return Err(MergeError::Plan);
    }
    let live = live_parts(root, parts)?;
    let mut inputs: Vec<&Part> = Vec::with_capacity(plan.inputs.len());
    for (i, id) in plan.inputs.iter().enumerate() {
        if plan.inputs[..i].contains(id) {
            return Err(MergeError::NotLive(*id));
        }
        let p = live
            .iter()
            .find(|p| p.header.file_id == *id)
            .ok_or(MergeError::NotLive(*id))?;
        for run in &p.runs {
            if width(&p.levels, run.level) != width(&root.levels, run.level) {
                return Err(MergeError::Levels(*id));
            }
        }
        inputs.push(p);
    }
    let hi = inputs.iter().map(|p| p.seq).max().expect("inputs");
    if let Some(id) = interleaved(root, &plan.inputs, &plan.periods) {
        return Err(MergeError::Interleaved(id));
    }
    let complete = !root.parts.iter().any(|r| {
        !plan.inputs.contains(&r.file_id)
            && plan.periods.iter().any(|&(s, l)| period_overlaps(r, s, l))
    });
    if plan.rollups && !complete {
        return Err(MergeError::Incomplete);
    }

    let mut kept: BTreeMap<Key, Entry> = overlay(root, &inputs)?
        .into_iter()
        .filter(|((_, level, start), (_, e))| {
            let w = width(&root.levels, *level).expect("checked against the table");
            let minute_dropped = w < QUARTER && start.saturating_add(w) <= plan.minute_from;
            let empty_dropped = complete && !e.stale && e.node.is_none();
            !minute_dropped && !empty_dropped
        })
        .map(|(k, (_, e))| (k, e))
        .collect();
    if plan.rollups {
        roll_up(root, plan, &mut kept, &params)?;
    }

    let mut out: Vec<BTreeMap<(u64, u8), Vec<Entry>>> = vec![BTreeMap::new(); plan.periods.len()];
    for ((series, level, start), e) in kept {
        let w = width(&root.levels, level).expect("checked against the table");
        let at = plan.periods.iter().position(|&(s, l)| {
            s <= start && start.saturating_add(w) <= s.saturating_add(l as i64)
        });
        let Some(at) = at else {
            return Err(MergeError::Outside {
                series,
                level,
                start,
            });
        };
        out[at].entry((series, level)).or_default().push(e);
    }
    let through = inputs.iter().map(|p| p.through).max().expect("inputs");
    let mut file_id = root.next_file_id;
    Ok(out
        .into_iter()
        .zip(&plan.periods)
        .filter(|(runs, _)| !runs.is_empty())
        .map(|(runs, &(period_start, period_len))| {
            let header = FileHeader {
                kind: part::KIND,
                file_id,
                ..*template
            };
            file_id += 1;
            Part {
                header,
                seq: hi,
                class: Class::Merged,
                period_start,
                period_len,
                through,
                levels: root.levels.clone(),
                runs: runs
                    .into_iter()
                    .map(|((series, level), entries)| Run {
                        series,
                        level,
                        entries,
                    })
                    .collect(),
            }
        })
        .collect())
}

/// Replaces every roll-up entry inside the periods with the left-to-right merge of its children (T2): present when
/// a child is, stale without a node when a child is stale (Q-1550), at the children's highest version.
fn roll_up<F>(
    root: &Root,
    plan: &Plan,
    kept: &mut BTreeMap<Key, Entry>,
    params: &F,
) -> Result<(), MergeError>
where
    F: Fn(u64) -> Option<Params>,
{
    let mut pairs = rollups(&root.levels);
    pairs.reverse();
    let Some(base) = cover_levels(&root.levels).last().copied() else {
        return Ok(());
    };
    let inside = |start: i64, w: i64| {
        plan.periods
            .iter()
            .any(|&(s, l)| s <= start && start.saturating_add(w) <= s.saturating_add(l as i64))
    };
    let covers = cover_levels(&root.levels);
    if let Some(&(s, _, _)) = kept
        .keys()
        .find(|&&(s, level, _)| covers.iter().any(|l| l.id == level) && params(s).is_none())
    {
        return Err(MergeError::NoParams(s));
    }
    let parents: Vec<Level> = pairs.iter().map(|p| p.0).collect();
    kept.retain(|&(_, level, start), _| {
        parents
            .iter()
            .find(|l| l.id == level)
            .is_none_or(|l| !inside(start, l.width))
    });
    let series: Vec<u64> = {
        let mut s: Vec<u64> = kept.keys().map(|k| k.0).collect();
        s.dedup();
        s
    };
    for s in series {
        let Some(p) = params(s) else { continue };
        for &(parent, child) in &pairs {
            let mut groups: BTreeMap<i64, Vec<&Entry>> = BTreeMap::new();
            for (_, e) in kept.range((s, child.id, i64::MIN)..=(s, child.id, i64::MAX)) {
                let Some(x) = e.start.checked_sub(e.start.rem_euclid(parent.width)) else {
                    continue;
                };
                groups.entry(x).or_default().push(e);
            }
            let mut made: Vec<(Key, Entry)> = Vec::new();
            for (x, children) in groups {
                let Some(end) = x.checked_add(parent.width) else {
                    continue;
                };
                if !inside(x, parent.width) || !covered(root, base.id, x, end) {
                    continue;
                }
                made.push(((s, parent.id, x), rolled(&p, s, x, end, &children)?));
            }
            kept.extend(made);
        }
    }
    Ok(())
}

fn rolled(
    p: &Params,
    series: u64,
    x: i64,
    end: i64,
    children: &[&Entry],
) -> Result<Entry, MergeError> {
    let as_of = children.iter().map(|e| e.as_of).max().expect("a child");
    if children.iter().any(|e| e.stale) {
        return Ok(Entry {
            start: x,
            stale: true,
            as_of,
            node: None,
        });
    }
    let mut acc = Node::empty(p.kind, x, x);
    for e in children {
        let n = e.node.as_ref().expect("empty markers were dropped");
        if n.body.kind() != p.kind {
            return Err(MergeError::Kind(series));
        }
        if acc.end < n.start {
            acc = merge_nodes(p, &acc, &Node::empty(p.kind, acc.end, n.start));
        }
        acc = merge_nodes(p, &acc, n);
    }
    if acc.end < end {
        acc = merge_nodes(p, &acc, &Node::empty(p.kind, acc.end, end));
    }
    Ok(Entry {
        start: x,
        stale: false,
        as_of,
        node: Some(acc),
    })
}

/// The root's record of an encoded part (T.12): identity and extent from the part, length and `file_crc32c` from
/// its bytes, which must be the part's encoding.
pub fn part_ref(p: &Part, bytes: &[u8]) -> PartRef {
    let tail = bytes.len() - 16;
    let trailer_at =
        u64::from_le_bytes(bytes[tail..tail + 8].try_into().expect("8 bytes")) as usize;
    let crc_at = trailer_at + 16 + 96;
    let file_crc32c = u32::from_le_bytes(bytes[crc_at..crc_at + 4].try_into().expect("4 bytes"));
    let spans = p.runs.iter().flat_map(|r| {
        let w = width(&p.levels, r.level).unwrap_or(0);
        r.entries
            .iter()
            .map(move |e| (e.start, e.start.saturating_add(w)))
    });
    let (least_start, greatest_end) = spans
        .fold(None, |acc: Option<(i64, i64)>, (s, e)| {
            Some(acc.map_or((s, e), |(a, b)| (a.min(s), b.max(e))))
        })
        .unwrap_or((0, 0));
    PartRef {
        file_id: p.header.file_id,
        seq: p.seq,
        class: p.class,
        file_crc32c,
        period_start: p.period_start,
        period_len: p.period_len,
        least_start,
        greatest_end,
        least_series: p.runs.first().map_or(0, |r| r.series),
        greatest_series: p.runs.last().map_or(0, |r| r.series),
        byte_len: bytes.len() as u64,
    }
}

/// The root that publishes a merge: the outputs live, the inputs retired at `now`, `seq` one higher; with roll-ups,
/// each roll-up level covered over the periods where the finest cover level is; finer levels than [`QUARTER`] no
/// longer covered before `plan.minute_from`, for every period: minute entries of parts outside the plan before the
/// cut are no longer visible.
pub fn next_root(root: &Root, plan: &Plan, outputs: &[PartRef], now: i64) -> Root {
    let mut r = root.clone();
    r.seq = root.seq + 1;
    r.header.file_id = r.seq;
    r.next_file_id = root.next_file_id + outputs.len() as u64;
    r.parts.retain(|p| !plan.inputs.contains(&p.file_id));
    r.parts.extend_from_slice(outputs);
    r.parts.sort_by_key(|p| p.file_id);
    r.retired.extend(plan.inputs.iter().map(|&file_id| Retired {
        file_id,
        retired_at: now,
    }));
    r.retired.sort_by_key(|x| x.file_id);
    let mut cov = root.covered.clone();
    if plan.rollups
        && let Some(base) = cover_levels(&root.levels).last().copied()
    {
        let base_ranges: Vec<(i64, i64)> = root
            .covered
            .iter()
            .filter(|c| c.level == base.id)
            .map(|c| (c.from, c.to))
            .collect();
        for (parent, _) in rollups(&root.levels) {
            for &(s, l) in &plan.periods {
                let e = s.saturating_add(l as i64);
                for &(a, b) in &base_ranges {
                    let from = ceil(a.max(s), parent.width);
                    let to = b.min(e) - b.min(e).rem_euclid(parent.width);
                    if from < to {
                        cov.push(Covered {
                            level: parent.id,
                            from,
                            to,
                        });
                    }
                }
            }
        }
    }
    for l in root.levels.iter().filter(|l| l.width < QUARTER) {
        let cut = ceil(plan.minute_from, l.width);
        cov = cov
            .into_iter()
            .filter_map(|c| match c.level == l.id {
                false => Some(c),
                true if c.to <= cut => None,
                true => Some(Covered {
                    from: c.from.max(cut),
                    ..c
                }),
            })
            .collect();
    }
    r.covered = normalise(cov);
    r
}

pub(crate) fn ceil(x: i64, w: i64) -> i64 {
    match x.rem_euclid(w) {
        0 => x,
        r => x.saturating_add(w - r),
    }
}

/// Sorted by level and start, overlapping and touching ranges joined.
fn normalise(mut v: Vec<Covered>) -> Vec<Covered> {
    v.sort_by_key(|c| (c.level, c.from));
    let mut out: Vec<Covered> = Vec::with_capacity(v.len());
    for c in v {
        match out.last_mut() {
            Some(l) if l.level == c.level && l.to >= c.from => l.to = l.to.max(c.to),
            _ => out.push(c),
        }
    }
    out
}
