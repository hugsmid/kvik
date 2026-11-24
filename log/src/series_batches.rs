//! The series batch lists a sealed segment may carry after its segment index (`format.md` A.8a, Q-1150): for each
//! series, the complete batches of the segment that hold one of its facts, so that a read of a few series reads only
//! their batches.

use crate::bytes::{u16_at, u32_at};

/// Header of a series batches body.
pub const SERIES_BATCHES_HEADER_LEN: usize = 16;
/// Size of a series record in v1.
pub const SERIES_RECORD_LEN: u16 = 8;
/// Size of a batch reference in v1.
pub const BATCH_REF_LEN: u16 = 8;
/// The most references a writer keeps for one segment; past it the segment carries no lists (Q-1150).
pub const MAX_REFS: usize = 1 << 18;

/// One batch of a sealed segment: its offset and its bytes, commit block included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchRef {
    /// File offset of the batch's first block header.
    pub offset: u32,
    /// The batch's bytes (`batch_bytes` plus its commit block).
    pub length: u32,
}

impl BatchRef {
    /// The offset just past the batch.
    pub fn end(&self) -> u64 {
        u64::from(self.offset) + u64::from(self.length)
    }
}

/// A series batches body (`format.md` A.8a).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SeriesBatches {
    /// Series ids, strictly ascending, each with where its references start in `refs` and how many there are.
    pub series: Vec<(u32, u32, u32)>,
    /// References, by series in the order of `series`, each series' in ascending offset.
    pub refs: Vec<BatchRef>,
}

impl SeriesBatches {
    /// Lists from (series, batch) pairs in any order; pairs repeated are kept once.
    pub fn from_pairs(mut pairs: Vec<(u32, BatchRef)>) -> Self {
        pairs.sort_unstable();
        pairs.dedup();
        let mut series: Vec<(u32, u32, u32)> = Vec::new();
        let mut refs = Vec::with_capacity(pairs.len());
        for (id, r) in pairs {
            match series.last_mut() {
                Some(last) if last.0 == id => last.2 += 1,
                _ => series.push((id, refs.len() as u32, 1)),
            }
            refs.push(r);
        }
        SeriesBatches { series, refs }
    }

    /// The batches holding `series_id`'s facts; `None` when the series is not listed.
    pub fn of(&self, series_id: u32) -> Option<&[BatchRef]> {
        let k = self.series.binary_search_by_key(&series_id, |s| s.0).ok()?;
        let (_, start, count) = self.series[k];
        self.refs
            .get(start as usize..start as usize + count as usize)
    }

    /// Bytes the lists take in memory.
    pub fn bytes(&self) -> usize {
        self.series.len() * std::mem::size_of::<(u32, u32, u32)>()
            + self.refs.len() * std::mem::size_of::<BatchRef>()
    }

    /// The body, with v1 records and references.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(
            SERIES_BATCHES_HEADER_LEN
                + self.series.len() * usize::from(SERIES_RECORD_LEN)
                + self.refs.len() * usize::from(BATCH_REF_LEN),
        );
        b.extend_from_slice(&(self.series.len() as u32).to_le_bytes());
        b.extend_from_slice(&SERIES_RECORD_LEN.to_le_bytes());
        b.extend_from_slice(&BATCH_REF_LEN.to_le_bytes());
        b.extend_from_slice(&(self.refs.len() as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        for (id, _, count) in &self.series {
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
        }
        for r in &self.refs {
            b.extend_from_slice(&r.offset.to_le_bytes());
            b.extend_from_slice(&r.length.to_le_bytes());
        }
        b
    }

    /// The lists in `body`, every reference inside `[64, index_offset)`; `None` when a check of A.8a fails. Records
    /// and references longer than v1's have their tail ignored.
    pub fn decode(body: &[u8], index_offset: u64) -> Option<Self> {
        let l = Layout::read(body)?;
        let mut refs = Vec::with_capacity(l.ref_count);
        for &(_, start, n) in &l.series {
            refs.extend(l.refs_of(body, start, n, index_offset)?);
        }
        Some(SeriesBatches {
            series: l.series,
            refs,
        })
    }

    /// The ids `body` lists, ascending, and the batches holding any of `wanted` (ascending ids), in file order and
    /// once each; only the records and those series' references are checked (A.8a).
    pub fn wanted_in(
        body: &[u8],
        index_offset: u64,
        wanted: &[u32],
    ) -> Option<(Vec<u32>, Vec<BatchRef>)> {
        let l = Layout::read(body)?;
        let mut refs = Vec::new();
        for id in wanted {
            if let Ok(k) = l.series.binary_search_by_key(id, |s| s.0) {
                let (_, start, n) = l.series[k];
                refs.extend(l.refs_of(body, start, n, index_offset)?);
            }
        }
        if wanted.len() > 1 {
            refs.sort_unstable();
            refs.dedup();
        }
        Some((l.series.iter().map(|s| s.0).collect(), refs))
    }
}

/// The records of a series batches body and where its references lie.
struct Layout {
    series: Vec<(u32, u32, u32)>,
    refs_at: usize,
    ref_size: usize,
    ref_count: usize,
}

impl Layout {
    fn read(body: &[u8]) -> Option<Self> {
        if body.len() < SERIES_BATCHES_HEADER_LEN {
            return None;
        }
        let count = u32_at(body, 0) as usize;
        let rec = usize::from(u16_at(body, 4));
        let ref_size = usize::from(u16_at(body, 6));
        let ref_count = u32_at(body, 8) as usize;
        if rec < usize::from(SERIES_RECORD_LEN) || ref_size < usize::from(BATCH_REF_LEN) {
            return None;
        }
        let refs_at = count
            .checked_mul(rec)?
            .checked_add(SERIES_BATCHES_HEADER_LEN)?;
        if refs_at.checked_add(ref_count.checked_mul(ref_size)?)? != body.len() {
            return None;
        }
        let mut series: Vec<(u32, u32, u32)> = Vec::with_capacity(count);
        let mut start = 0u32;
        for r in body[SERIES_BATCHES_HEADER_LEN..refs_at].chunks_exact(rec) {
            let (id, n) = (u32_at(r, 0), u32_at(r, 4));
            if n == 0 || series.last().is_some_and(|&(last, _, _)| id <= last) {
                return None;
            }
            series.push((id, start, n));
            start = start.checked_add(n)?;
        }
        (start as usize == ref_count).then_some(Layout {
            series,
            refs_at,
            ref_size,
            ref_count,
        })
    }

    /// One series' references, checked: aligned, inside `[64, index_offset)`, ascending and apart.
    fn refs_of(&self, body: &[u8], start: u32, n: u32, index_offset: u64) -> Option<Vec<BatchRef>> {
        let from = self.refs_at + start as usize * self.ref_size;
        let own: Vec<BatchRef> = body[from..from + n as usize * self.ref_size]
            .chunks_exact(self.ref_size)
            .map(|r| BatchRef {
                offset: u32_at(r, 0),
                length: u32_at(r, 4),
            })
            .collect();
        let lower = crate::header::HEADER_LEN as u64;
        let bad = own.iter().any(|r| {
            u64::from(r.offset) < lower
                || r.length == 0
                || r.end() > index_offset
                || r.offset % 8 != 0
                || r.length % 8 != 0
        }) || own.windows(2).any(|w| u64::from(w[1].offset) < w[0].end());
        (!bad).then_some(own)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u32, length: u32) -> BatchRef {
        BatchRef { offset, length }
    }

    #[test]
    fn lists_round_trip_and_give_each_series_its_batches_in_order() {
        let l = SeriesBatches::from_pairs(vec![
            (7, r(200, 64)),
            (3, r(64, 136)),
            (7, r(64, 136)),
            (3, r(64, 136)),
        ]);
        assert_eq!(l.of(3), Some(&[r(64, 136)][..]));
        assert_eq!(l.of(7), Some(&[r(64, 136), r(200, 64)][..]));
        assert_eq!(l.of(5), None);
        assert_eq!(SeriesBatches::decode(&l.encode(), 264), Some(l));
    }

    #[test]
    fn a_reference_past_the_index_or_overlapping_the_one_before_is_refused() {
        let past = SeriesBatches::from_pairs(vec![(1, r(64, 136))]);
        assert_eq!(SeriesBatches::decode(&past.encode(), 192), None);
        let overlap = SeriesBatches::from_pairs(vec![(1, r(64, 136)), (1, r(192, 64))]);
        assert_eq!(SeriesBatches::decode(&overlap.encode(), 4096), None);
    }

    #[test]
    fn a_body_whose_counts_do_not_fill_it_or_whose_ids_repeat_is_refused() {
        let l = SeriesBatches::from_pairs(vec![(1, r(64, 64)), (2, r(128, 64))]);
        let mut b = l.encode();
        b.extend_from_slice(&[0; 8]);
        assert_eq!(SeriesBatches::decode(&b, 4096), None);
        let mut b = l.encode();
        b[SERIES_BATCHES_HEADER_LEN + 8..SERIES_BATCHES_HEADER_LEN + 12]
            .copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(SeriesBatches::decode(&b, 4096), None);
        assert_eq!(SeriesBatches::decode(&[0; 8], 4096), None);
    }

    #[test]
    fn longer_records_and_references_have_their_tail_ignored() {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&12u16.to_le_bytes());
        b.extend_from_slice(&12u16.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&[9, 0, 0, 0, 1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        b.extend_from_slice(&[64, 0, 0, 0, 64, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        let l = SeriesBatches::decode(&b, 4096).expect("decodes");
        assert_eq!(l.of(9), Some(&[r(64, 64)][..]));
    }
}
