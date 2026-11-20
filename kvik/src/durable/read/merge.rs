//! The k-way merge of sources' rows and the views over them (the query design's §2.2 Merge and views): rows in
//! `(t, series, position)` order, a position met twice dropped; the current view keeps the fact of highest rank per
//! key, as of a position when the pass is bounded there; history keeps every fact (spec §2.3).

use std::cmp::Ordering;

use crate::durable::index::Stored;

fn order(a: &Stored, b: &Stored) -> Ordering {
    (a.row.t, a.row.series_id, a.position).cmp(&(b.row.t, b.row.series_id, b.position))
}

/// Sorts one source's rows into the merge's order.
pub(crate) fn sort(rows: &mut [Stored]) {
    rows.sort_unstable_by(order);
}

/// Merges sources' rows, each already in the merge's order, into one run in that order; a position more than one
/// source holds is kept once (the recent window and the log tail overlap in the writer's process).
pub(crate) fn merge(mut parts: Vec<Vec<Stored>>) -> Vec<Stored> {
    parts.retain(|p| !p.is_empty());
    if parts.len() <= 1 {
        return parts.pop().unwrap_or_default();
    }
    let mut out: Vec<Stored> = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    let mut iters: Vec<std::vec::IntoIter<Stored>> =
        parts.into_iter().map(Vec::into_iter).collect();
    let mut heads: Vec<Option<Stored>> = iters.iter_mut().map(Iterator::next).collect();
    loop {
        let mut best: Option<usize> = None;
        for i in 0..heads.len() {
            let Some(r) = &heads[i] else { continue };
            if best.is_none_or(|b| order(r, heads[b].as_ref().expect("a head")) == Ordering::Less) {
                best = Some(i);
            }
        }
        let Some(b) = best else { break };
        let r = std::mem::replace(&mut heads[b], iters[b].next()).expect("a head");
        if out
            .last()
            .is_none_or(|l| (l.row.series_id, l.position) != (r.row.series_id, r.position))
        {
            out.push(r);
        }
    }
    out
}

/// The rank of a fact among those of its key (spec §2.2): its position, or `(changed_at, position)` in a versioned
/// series.
fn rank(s: &Stored, versioned: bool) -> (Option<u64>, kvik_log::writer::Position) {
    (if versioned { s.row.changed_at } else { None }, s.position)
}

/// The rows of a view from merged rows: every row in history, else the one of highest rank per `(t, series)`.
pub(crate) fn select<'a>(
    rows: &'a [Stored],
    history: bool,
    versioned: impl Fn(u32) -> bool + 'a,
) -> impl Iterator<Item = &'a Stored> + 'a {
    let mut i = 0;
    std::iter::from_fn(move || {
        let first = rows.get(i)?;
        if history {
            i += 1;
            return Some(first);
        }
        let v = versioned(first.row.series_id);
        let mut best = first;
        i += 1;
        while let Some(r) = rows.get(i)
            && (r.row.t, r.row.series_id) == (first.row.t, first.row.series_id)
        {
            if rank(r, v) > rank(best, v) {
                best = r;
            }
            i += 1;
        }
        Some(best)
    })
}

#[cfg(test)]
mod tests {
    use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
    use kvik_log::writer::Position as LogPos;
    use proptest::prelude::*;

    use super::*;

    fn stored(series: u32, t: i64, seq: u64, changed_at: Option<u64>) -> Stored {
        Stored {
            position: LogPos {
                generation: 1,
                sequence: seq,
            },
            received_at: 0,
            row: FactRow {
                series_id: series,
                t,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: Value::F64(seq),
                changed_at,
            },
        }
    }

    proptest! {
        #[test]
        fn a_merge_of_overlapping_sources_equals_one_sorted_run_of_their_union(
            rows in prop::collection::btree_map(1u64..200, (0u32..3, -5i64..5, prop::option::of(0u64..3)), 0..60),
            split in prop::collection::vec(0u8..7, 60),
        ) {
            let all: Vec<Stored> = rows.iter().map(|(p, (s, t, c))| stored(*s, *t, *p, *c)).collect();
            let mut parts = vec![Vec::new(), Vec::new(), Vec::new()];
            for (k, r) in all.iter().enumerate() {
                let m = split[k];
                for (j, part) in parts.iter_mut().enumerate() {
                    if m & (1 << j) != 0 || (m == 0 && j == 0) {
                        part.push(r.clone());
                    }
                }
            }
            for p in &mut parts {
                sort(p);
            }
            let mut want = all.clone();
            sort(&mut want);
            prop_assert_eq!(merge(parts), want.clone());
            let current: Vec<&Stored> = select(&want, false, |s| s == 1).collect();
            for (s, t) in want.iter().map(|r| (r.row.series_id, r.row.t)) {
                let best = want
                    .iter()
                    .filter(|r| (r.row.series_id, r.row.t) == (s, t))
                    .max_by_key(|r| rank(r, s == 1))
                    .unwrap();
                prop_assert!(current.contains(&best));
            }
            prop_assert_eq!(select(&want, true, |_| false).count(), want.len());
        }
    }
}
