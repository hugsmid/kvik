//! A point lookup in the local log: the facts of given series at given event times, read up to the durable mark
//! through the segment indexes of sealed segments (`format.md` A.8) and a scan of every other segment.
//!
//! A lookup reads what a refresh would accept (the reader rules of the module documentation): the mark read again
//! first, the mark's segment only below its offset, no batch above the mark's last position, and a recovery entry
//! voiding what precedes it in its segment above its `after`. It never moves what [`LogReader::refresh`] has taken.
//! Damage is never read as absence: a range is reported complete only if every segment that may hold it was read
//! and every invalid region met in such a segment is one the caller knows to be lost.

use std::collections::BTreeMap;

use super::files::{read_at_full, read_part};
use super::{LogReader, ReaderError};
use crate::batch::SegmentIdentity;
use crate::block::{BLOCK_HEADER_LEN, block_type, read_block};
use crate::body::{
    END_MARKER_LEN, IndexRecord, SegmentIndex, TRAILER_LEN, Trailer, decode_end_marker,
};
use crate::content::{BatchContent, Item};
use crate::facts::FactRow;
use crate::header::HEADER_LEN;
use crate::name::SegmentName;
use crate::scan::{Found, ScanLimit, Scanner};
use crate::writer::Position;

/// One series' facts wanted with event time from `from` to `to`, both inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Wanted {
    /// The series.
    pub series_id: u32,
    /// The least event time wanted.
    pub from: i64,
    /// The greatest event time wanted.
    pub to: i64,
}

/// A fact a lookup found.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundFact {
    /// Its position.
    pub position: Position,
    /// Its batch's receive time.
    pub received_at: i64,
    /// The row.
    pub row: FactRow,
}

/// What a lookup found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lookup {
    /// The facts in the wanted ranges, in log order.
    pub facts: Vec<FoundFact>,
    /// Per wanted range, in the order given: whether every place that may hold it was read.
    pub complete: Vec<bool>,
    /// The mark's position read up to.
    pub durable: Position,
    /// Segments read for the lookup.
    pub segments_read: u64,
}

/// A sealed segment's index as the lookup uses it: `None` when the segment has none that reads.
pub(super) type SealIndex = Option<Vec<IndexRecord>>;

impl LogReader {
    /// The facts in the `wanted` ranges in the local log up to the durable mark. `lost(file_id, start)` names the
    /// invalid regions whose positions the caller takes as lost (`spec.md` §3 Recovery rule 2); any other region
    /// in a segment that may hold a range, and any segment that cannot be read, leaves that range incomplete. Fails
    /// only when the mark or the segment listing cannot be read.
    pub fn lookup(
        &mut self,
        wanted: &[Wanted],
        lost: &dyn Fn(u64, u64) -> bool,
    ) -> Result<Lookup, ReaderError> {
        let mark = self.read_mark()?;
        self.keep_alive()?;
        let mut out = Lookup {
            facts: Vec::new(),
            complete: vec![true; wanted.len()],
            durable: Position {
                generation: mark.generation,
                sequence: mark.last_sequence,
            },
            segments_read: 0,
        };
        if wanted.is_empty() {
            return Ok(out);
        }
        let mut names = self.segments()?;
        names.retain(|n| n.file_id > 0 && n.file_id <= mark.file_id);
        if mark.file_id > 0 && names.last().is_none_or(|n| n.file_id != mark.file_id) {
            match self.find_segment(mark.file_id)? {
                Some(n) => names.push(n),
                None => {
                    out.complete.fill(false);
                    return Ok(out);
                }
            }
        }
        self.seal_index
            .retain(|id, _| names.iter().any(|n| n.file_id == *id));
        let mut buf = Vec::new();
        for name in names {
            let index = if name.sealed {
                self.seal_index_of(name)
            } else {
                None
            };
            let candidates: Vec<usize> = (0..wanted.len())
                .filter(|&i| {
                    let w = &wanted[i];
                    index.as_ref().is_none_or(|recs| {
                        recs.binary_search_by_key(&w.series_id, |r| r.series_id)
                            .is_ok_and(|k| recs[k].min_t <= w.to && w.from <= recs[k].max_t)
                    })
                })
                .collect();
            if candidates.is_empty() {
                continue;
            }
            let limit = if name.file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let read = self.lookup_segment(
                name,
                limit,
                (mark.generation, mark.last_sequence),
                wanted,
                &candidates,
                lost,
                &mut buf,
                &mut out.facts,
            );
            out.segments_read += 1;
            if !read {
                for i in candidates {
                    out.complete[i] = false;
                }
            }
        }
        Ok(out)
    }

    /// Reads one segment for the `candidates` among `wanted`; returns whether it was read whole and met only lost
    /// regions.
    #[allow(clippy::too_many_arguments)]
    fn lookup_segment(
        &self,
        name: SegmentName,
        limit: ScanLimit,
        (generation, last_sequence): (u32, u64),
        wanted: &[Wanted],
        candidates: &[usize],
        lost: &dyn Fn(u64, u64) -> bool,
        buf: &mut Vec<u8>,
        facts: &mut Vec<FoundFact>,
    ) -> bool {
        let file_id = name.file_id;
        let Ok(Some((n, f))) = self.open_segment(name) else {
            return false;
        };
        let len = match limit {
            ScanLimit::Mark(m) => m.max(HEADER_LEN as u64),
            ScanLimit::End => match f.len() {
                Ok(l) => l,
                Err(_) => return false,
            },
        };
        if len > super::MAX_SEGMENT_FILE
            || read_part(buf, &*f, HEADER_LEN as u64, len, true).is_err()
        {
            return false;
        }
        let id = SegmentIdentity {
            store_id: self.store_id,
            file_id,
            generation: n.generation,
        };
        let Ok(scanner) = Scanner::new(buf, &id, limit) else {
            return false;
        };
        let mut by_series: BTreeMap<u32, Vec<(i64, i64)>> = BTreeMap::new();
        for &i in candidates {
            let w = &wanted[i];
            by_series
                .entry(w.series_id)
                .or_default()
                .push((w.from, w.to));
        }
        let first = facts.len();
        let mut whole = true;
        for found in scanner {
            match found {
                Err(_) => return false,
                Ok(Found::Region(r)) => {
                    if !lost(file_id, r.start) {
                        whole = false;
                    }
                }
                Ok(Found::Batch(b)) => {
                    let g = b.commit.generation;
                    if g == generation && b.commit.last_sequence > last_sequence {
                        break;
                    }
                    let Ok(content) = BatchContent::decode(&b) else {
                        whole = false;
                        continue;
                    };
                    if let [Item::Entry(crate::entries::Entry::Recovery(r))] =
                        content.items.as_slice()
                    {
                        let after = Position {
                            generation: g,
                            sequence: r.after,
                        };
                        let mut j = 0;
                        facts.retain(|x| {
                            j += 1;
                            j <= first || x.position <= after
                        });
                    }
                    let mut sequence = b.commit.first_sequence;
                    for item in content.items {
                        let p = Position {
                            generation: g,
                            sequence,
                        };
                        sequence += item.sequences();
                        if let Item::Fact(row) = item
                            && by_series.get(&row.series_id).is_some_and(|rs| {
                                rs.iter().any(|(a, z)| (*a..=*z).contains(&row.t))
                            })
                        {
                            facts.push(FoundFact {
                                position: p,
                                received_at: content.received_at,
                                row,
                            });
                        }
                    }
                }
            }
        }
        whole
    }

    /// A sealed segment's index, read from its seal blocks once and kept: sealed segments never change.
    fn seal_index_of(&mut self, name: SegmentName) -> SealIndex {
        if let Some(i) = self.seal_index.get(&name.file_id) {
            return i.clone();
        }
        let i = self.read_seal_index(name);
        self.seal_index.insert(name.file_id, i.clone());
        i
    }

    fn read_seal_index(&self, name: SegmentName) -> SealIndex {
        let (n, f) = self.open_segment(name).ok().flatten()?;
        if !n.sealed {
            return None;
        }
        let len = f.len().ok()?;
        let tail = (BLOCK_HEADER_LEN + TRAILER_LEN + END_MARKER_LEN) as u64;
        let start = len.checked_sub(tail)?;
        let mut b = vec![0u8; tail as usize];
        if read_at_full(&*f, &mut b, start).ok()? != b.len() {
            return None;
        }
        let trailer_offset = decode_end_marker(&b[b.len() - END_MARKER_LEN..])?;
        if trailer_offset != start {
            return None;
        }
        let block = read_block(&b, 0, (BLOCK_HEADER_LEN + TRAILER_LEN) as u64).ok()?;
        if block.header.block_type != block_type::TRAILER {
            return None;
        }
        let trailer = Trailer::decode(block.body).ok()?;
        let at = trailer.index_offset;
        if at < HEADER_LEN as u64 || at >= trailer_offset {
            return None;
        }
        let mut b = vec![0u8; usize::try_from(trailer_offset - at).ok()?];
        if read_at_full(&*f, &mut b, at).ok()? != b.len() {
            return None;
        }
        let block = read_block(&b, 0, b.len() as u64).ok()?;
        if block.header.block_type != block_type::SEGMENT_INDEX {
            return None;
        }
        let mut records = SegmentIndex::decode(block.body).ok()?.records;
        records.sort_by_key(|r| r.series_id);
        Some(records)
    }
}
