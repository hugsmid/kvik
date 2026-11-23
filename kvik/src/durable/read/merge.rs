//! The k-way merge of sources' rows and the views over them (the query design's §2.2 Merge and views): rows in
//! `(t, series, position)` order, a position met twice dropped; the current view keeps the fact of highest rank per
//! key, as of a position when the pass is bounded there; history keeps every fact (spec §2.3).

use std::cmp::Ordering;

use crate::durable::index::Stored;

fn order(a: &Stored, b: &Stored) -> Ordering {
    (a.row.t, a.row.series_id, a.position).cmp(&(b.row.t, b.row.series_id, b.position))
}

/// A row's place in the merge's order, and its index.
type Key = (i64, u32, kvik_log::writer::Position, u32);

/// What sorting by keys holds for each row: its key, then its index.
pub(crate) const KEY_BYTES: u64 = (std::mem::size_of::<Key>() + std::mem::size_of::<u32>()) as u64;

/// Sorts one source's rows into the merge's order. When `room` bytes hold their keys, the keys are sorted and each
/// row moved once to its place, as a row is several times a key's size; else the rows are sorted in place.
pub(crate) fn sort(rows: &mut [Stored], room: u64) {
    if rows.is_sorted_by(|a, b| order(a, b) != Ordering::Greater) {
        return;
    }
    let n = rows.len();
    if u32::try_from(n).is_err() || (n as u64).saturating_mul(KEY_BYTES) > room {
        rows.sort_unstable_by(order);
        return;
    }
    let mut keys: Vec<Key> = rows
        .iter()
        .enumerate()
        .map(|(i, s)| (s.row.t, s.row.series_id, s.position, i as u32))
        .collect();
    keys.sort_unstable();
    let mut from: Vec<u32> = keys.iter().map(|k| k.3).collect();
    drop(keys);
    // Each cycle of the permutation in turn: the row for place `k` is at `from[k]`; a place done points at itself.
    for start in 0..n {
        let mut k = start;
        loop {
            let src = from[k] as usize;
            from[k] = k as u32;
            if src == start {
                break;
            }
            rows.swap(k, src);
            k = src;
        }
    }
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
pub(crate) fn rank(s: &Stored, versioned: bool) -> (Option<u64>, kvik_log::writer::Position) {
    (if versioned { s.row.changed_at } else { None }, s.position)
}

/// Keeps the rows of a view, in place and in order: every row in history, else the one of highest rank per
/// `(t, series)`.
pub(crate) fn keep_view(rows: &mut Vec<Stored>, history: bool, versioned: impl Fn(u32) -> bool) {
    if history {
        return;
    }
    let (mut w, mut i) = (0, 0);
    while i < rows.len() {
        let key = (rows[i].row.t, rows[i].row.series_id);
        let v = versioned(key.1);
        let mut best = i;
        i += 1;
        while i < rows.len() && (rows[i].row.t, rows[i].row.series_id) == key {
            if rank(&rows[i], v) > rank(&rows[best], v) {
                best = i;
            }
            i += 1;
        }
        rows.swap(w, best);
        w += 1;
    }
    rows.truncate(w);
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
            for (j, p) in parts.iter_mut().enumerate() {
                sort(p, if j == 0 { 0 } else { u64::MAX });
            }
            let mut want = all.clone();
            sort(&mut want, 0);
            prop_assert_eq!(merge(parts), want.clone());
            let mut current = want.clone();
            keep_view(&mut current, false, |s| s == 1);
            for (s, t) in want.iter().map(|r| (r.row.series_id, r.row.t)) {
                let best = want
                    .iter()
                    .filter(|r| (r.row.series_id, r.row.t) == (s, t))
                    .max_by_key(|r| rank(r, s == 1))
                    .unwrap();
                prop_assert!(current.contains(best));
            }
            prop_assert_eq!(current.len(), current.iter().map(|r| (r.row.t, r.row.series_id)).collect::<std::collections::BTreeSet<_>>().len());
            let mut history = want.clone();
            keep_view(&mut history, true, |_| false);
            prop_assert_eq!(history, want.clone());
        }

        #[test]
        fn a_sort_by_keys_equals_a_sort_in_place_and_holds_to_the_room_given(
            rows in prop::collection::btree_map(1u64..400, (0u32..4, -9i64..9), 0..120),
            order in prop::collection::vec(any::<prop::sample::Index>(), 120),
            room in 0usize..130,
        ) {
            let mut all: Vec<Stored> = rows.iter().map(|(p, (s, t))| stored(*s, *t, *p, None)).collect();
            for (k, i) in order.iter().enumerate().take(all.len()) {
                let j = i.index(all.len());
                all.swap(k, j);
            }
            let mut by_keys = all.clone();
            sort(&mut by_keys, room as u64 * KEY_BYTES);
            let mut in_place = all;
            in_place.sort_unstable_by(super::order);
            prop_assert_eq!(by_keys, in_place);
        }
    }
}
