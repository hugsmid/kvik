//! The segment index a writer keeps for the segment in use as facts are appended (`format.md` A.8): one record per
//! series, held in a vector for the dense ids a store assigns and a tree above, so a fact costs one indexed access.

use std::collections::BTreeMap;

use crate::body::IndexRecord;

/// Ids below this are held in the vector.
const DENSE: u32 = 1 << 16;

#[derive(Debug, Default)]
pub(super) struct SegIndex {
    dense: Vec<Option<IndexRecord>>,
    sparse: BTreeMap<u32, IndexRecord>,
}

impl SegIndex {
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
    }
}
