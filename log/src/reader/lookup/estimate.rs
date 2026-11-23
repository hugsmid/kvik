//! What lookups count of the segments they map, and the estimate of a lookup's facts from those counts and the sealed
//! segments' indexes (Q-1111).

use super::*;

/// Counts a fact of `series` at `t` in `tally`.
pub(super) fn note(tally: &mut Vec<IndexRecord>, series_id: u32, t: i64) {
    let k = match tally.binary_search_by_key(&series_id, |r| r.series_id) {
        Ok(k) => k,
        Err(k) => {
            tally.insert(
                k,
                IndexRecord {
                    series_id,
                    fact_count: 0,
                    min_t: t,
                    max_t: t,
                },
            );
            k
        }
    };
    let r = &mut tally[k];
    r.fact_count = r.fact_count.saturating_add(1);
    r.min_t = r.min_t.min(t);
    r.max_t = r.max_t.max(t);
}

/// A segment's facts of one series between `from` and `to`, both inclusive, taking them as spread evenly over its
/// span.
fn share(r: &IndexRecord, from: i64, to: i64) -> f64 {
    let (a, z) = (from.max(r.min_t), to.min(r.max_t));
    if a > z {
        return 0.0;
    }
    let width = i128::from(r.max_t) - i128::from(r.min_t) + 1;
    let part = i128::from(z) - i128::from(a) + 1;
    f64::from(r.fact_count) * (part as f64 / width as f64)
}

impl LogReader {
    /// About how many facts a lookup of `wanted` would find in the segments the last lookup listed, from the sealed
    /// segments' indexes and the tallies of the segments lookups mapped. Segments listed since, and facts appended
    /// since a map's last scan, are not counted; facts voided since, or after a batch that failed to decode, still
    /// are. `None` before a lookup has listed them, or when a listed segment has neither. A guide for sizing lookups,
    /// never a bound.
    pub fn estimate(&self, wanted: &[Wanted]) -> Option<u64> {
        let listed = self.caches.listed.as_ref()?;
        let mut n = 0.0;
        for id in listed {
            let records = match (self.caches.index.get(id), self.caches.maps.get(id)) {
                (Some(Cached { value: Some(r), .. }), _) => r,
                (_, Some(m)) if m.value.resume.is_some() => &m.value.tally,
                _ => return None,
            };
            for w in wanted {
                if let Ok(k) = records.binary_search_by_key(&w.series_id, |r| r.series_id) {
                    n += share(&records[k], w.from, w.to);
                }
            }
        }
        Some(n.ceil() as u64)
    }

    /// The greatest `t` of `series_id` in the sealed segments whose indexes this reader holds.
    #[cfg(test)]
    pub(in crate::reader) fn sealed_max_t(&self, series_id: u32) -> Option<i64> {
        self.caches
            .index
            .values()
            .filter_map(|c| c.value.as_ref())
            .filter_map(|records| records.iter().find(|r| r.series_id == series_id))
            .map(|r| r.max_t)
            .max()
    }
}
