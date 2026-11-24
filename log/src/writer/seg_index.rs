//! The segment index a writer keeps for the segment in use as facts are appended (`format.md` A.8): one record per
//! series, held in a vector for the dense ids a store assigns and a tree above, so a fact costs one indexed access.

use std::collections::BTreeMap;

use crate::body::IndexRecord;
use crate::series_batches::{BatchRef, MAX_REFS, SeriesBatches};

/// Ids below this are held in the vector.
const DENSE: u32 = 1 << 16;

#[derive(Debug, Default)]
pub(super) struct SegIndex {
    dense: Vec<Option<IndexRecord>>,
    sparse: BTreeMap<u32, IndexRecord>,
    lists: Lists,
}

/// The (series, batch) pairs of the segment for its series batches block (A.8a): each batch's series in the order
/// first met, the batch's offset known from [`SegIndex::begin_batch`] and its length from [`SegIndex::end_batch`].
/// Given up (`given_up`) past [`MAX_REFS`] pairs, at a recovery entry, or at an offset past `u32`.
#[derive(Debug, Default)]
struct Lists {
    pairs: Vec<(u32, BatchRef)>,
    batches: u64,
    /// The batch a series last had a pair in, plus one: dense ids by index, the rest by id.
    seen_dense: Vec<u64>,
    seen_sparse: BTreeMap<u32, u64>,
    batch_start: usize,
    offset: u32,
    given_up: bool,
}

impl SegIndex {
    /// An index whose segment carries no lists (A.8a).
    pub(super) fn without_lists() -> Self {
        let mut s = SegIndex::default();
        s.lists.give_up();
        s
    }

    /// Counts a fact of `series_id` at `t` whose reading reaches `reach`.
    pub(super) fn note(&mut self, series_id: u32, t: i64, reach: i64) {
        let fresh = || IndexRecord {
            series_id,
            fact_count: 0,
            min_t: t,
            max_t: reach,
        };
        let rec = if series_id < DENSE {
            let i = series_id as usize;
            if i >= self.dense.len() {
                self.dense.resize(i + 1, None);
            }
            self.dense[i].get_or_insert_with(fresh)
        } else {
            self.sparse.entry(series_id).or_insert_with(fresh)
        };
        rec.fact_count += 1;
        rec.min_t = rec.min_t.min(t);
        rec.max_t = rec.max_t.max(reach);
        self.lists.note(series_id);
    }

    /// Starts a batch written at `offset`; its facts follow through [`SegIndex::note`].
    pub(super) fn begin_batch(&mut self, offset: u64) {
        let l = &mut self.lists;
        l.batches += 1;
        l.batch_start = l.pairs.len();
        match u32::try_from(offset) {
            Ok(o) => l.offset = o,
            Err(_) => l.give_up(),
        }
    }

    /// Ends the batch begun last, `length` bytes long; `recovery` when it holds a recovery entry, which every read
    /// of the segment must meet, so the segment carries no lists.
    pub(super) fn end_batch(&mut self, length: u64, recovery: bool) {
        let l = &mut self.lists;
        if recovery {
            l.give_up();
        }
        let Ok(length) = u32::try_from(length) else {
            l.give_up();
            return;
        };
        if l.given_up {
            return;
        }
        for (_, r) in &mut l.pairs[l.batch_start..] {
            r.length = length;
        }
    }

    /// The records in series order, and the series batches block's lists when the segment carries one: only when a
    /// series is on average in at most half the batches, as otherwise a read of a few series reads most batches
    /// anyway.
    pub(super) fn into_parts(self) -> (Vec<IndexRecord>, Option<SeriesBatches>) {
        let lists = self.lists;
        let records = SegIndex {
            lists: Lists::default(),
            ..self
        }
        .into_records();
        let every = lists.batches.saturating_mul(records.len() as u64);
        let keep = !lists.given_up
            && !records.is_empty()
            && (lists.pairs.len() as u64).saturating_mul(2) <= every;
        (
            records,
            keep.then(|| SeriesBatches::from_pairs(lists.pairs)),
        )
    }

    /// The records in series order.
    pub(super) fn into_records(self) -> Vec<IndexRecord> {
        self.dense
            .into_iter()
            .flatten()
            .chain(self.sparse.into_values())
            .collect()
    }
}

impl Lists {
    fn give_up(&mut self) {
        self.given_up = true;
        self.batch_start = 0;
        self.pairs = Vec::new();
        self.seen_dense = Vec::new();
        self.seen_sparse = BTreeMap::new();
    }

    fn note(&mut self, series_id: u32) {
        if self.given_up {
            return;
        }
        let mark = self.batches;
        let seen = if series_id < DENSE {
            let i = series_id as usize;
            if i >= self.seen_dense.len() {
                self.seen_dense.resize(i + 1, 0);
            }
            &mut self.seen_dense[i]
        } else {
            self.seen_sparse.entry(series_id).or_insert(0)
        };
        if *seen == mark {
            return;
        }
        *seen = mark;
        if self.pairs.len() == MAX_REFS {
            self.give_up();
            return;
        }
        self.pairs.push((
            series_id,
            BatchRef {
                offset: self.offset,
                length: 0,
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #[test]
        fn the_records_are_those_a_tree_by_series_gives(
            facts in proptest::collection::vec(
                (proptest::prop_oneof![0u32..50, (DENSE - 2)..(DENSE + 2), proptest::num::u32::ANY], -100i64..100, 0i64..10),
                0..100,
            ),
        ) {
            let mut s = SegIndex::default();
            let mut t: BTreeMap<u32, IndexRecord> = BTreeMap::new();
            for (id, at, len) in facts {
                s.note(id, at, at + len);
                let rec = t.entry(id).or_insert(IndexRecord { series_id: id, fact_count: 0, min_t: at, max_t: at + len });
                rec.fact_count += 1;
                rec.min_t = rec.min_t.min(at);
                rec.max_t = rec.max_t.max(at + len);
            }
            proptest::prop_assert_eq!(s.into_records(), t.into_values().collect::<Vec<_>>());
        }

        #[test]
        fn the_lists_name_each_batch_once_for_every_series_it_holds_while_a_series_is_in_at_most_half(
            batches in proptest::collection::vec(
                (proptest::collection::vec(proptest::prop_oneof![0u32..6, (DENSE - 1)..(DENSE + 1)], 0..8), 1u64..40),
                1..12,
            ),
            recovery_at in proptest::option::of(0usize..12),
        ) {
            let mut s = SegIndex::default();
            let mut pairs = Vec::new();
            let mut offset = 64u64;
            for (k, (series, len)) in batches.iter().enumerate() {
                s.begin_batch(offset);
                for id in series {
                    s.note(*id, 0, 0);
                    pairs.push((*id, BatchRef { offset: offset as u32, length: (len * 8) as u32 }));
                }
                s.end_batch(len * 8, recovery_at == Some(k));
                offset += len * 8;
            }
            let series: std::collections::BTreeSet<u32> = batches.iter().flat_map(|(s, _)| s.clone()).collect();
            let distinct: std::collections::BTreeSet<(u32, u64)> = pairs.iter().map(|(id, r)| (*id, u64::from(r.offset))).collect();
            let dense = distinct.len() as u64 * 2 > batches.len() as u64 * series.len() as u64;
            let recovered = recovery_at.is_some_and(|k| k < batches.len());
            let (_, lists) = s.into_parts();
            if dense || recovered || series.is_empty() {
                proptest::prop_assert_eq!(lists, None);
            } else {
                proptest::prop_assert_eq!(lists, Some(SeriesBatches::from_pairs(pairs)));
            }
        }
    }

    #[test]
    fn an_index_rebuilt_without_offsets_carries_no_lists() {
        let mut s = SegIndex::without_lists();
        s.begin_batch(64);
        s.note(1, 0, 0);
        s.end_batch(64, false);
        s.begin_batch(128);
        s.note(2, 0, 0);
        s.end_batch(64, false);
        assert_eq!(s.into_parts().1, None);
    }

    #[test]
    fn a_segment_past_the_most_references_carries_no_lists() {
        let mut s = SegIndex::default();
        for k in 0..=MAX_REFS as u64 {
            s.begin_batch(64 + k * 8);
            s.note((k % 2) as u32, 0, 0);
            s.end_batch(8, false);
        }
        assert_eq!(s.into_parts().1, None);
    }
}
