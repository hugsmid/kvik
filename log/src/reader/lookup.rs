//! A point lookup in the local log: the facts of given series at given event times, read up to the durable mark
//! through the segment indexes of sealed segments (`format.md` A.8) and a scan of every other segment.
//!
//! A lookup reads what a refresh would accept (the reader rules of the module documentation): the mark read again
//! first, the mark's segment only below its offset, no batch above the mark's last position, and a recovery entry
//! voiding what precedes it in its segment above its `after`. It never moves what [`LogReader::refresh`] has taken.
//! Damage is never read as absence: a range is reported complete only if every segment that may hold it was read
//! and every invalid region met in such a segment is one the caller knows to be lost.

use std::collections::BTreeMap;

use super::files::read_at_full;
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
use crate::scan::{Found, Resume, ScanLimit, Scanner};
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
    /// Whether the lease was renewed when due ([`LogReader::keep_alive`]).
    pub lease_kept: bool,
}

/// A sealed segment's index as the lookup uses it: `None` when the segment has none that reads.
pub(super) type SealIndex = Option<Vec<IndexRecord>>;

/// What lookups have read of one segment: each complete batch's place, positions and range of event times, the
/// invalid regions met, and where a scan of what was appended since resumes. One record per batch.
#[derive(Clone, Debug, Default)]
pub(super) struct SegmentMap {
    batches: Vec<Span>,
    /// Invalid regions met, by start and end offset.
    regions: Vec<(u64, u64)>,
    resume: Option<Resume>,
    /// A recovery entry was met: what precedes it in the segment may be void (module documentation), so the segment
    /// is read whole by every lookup.
    recovery: bool,
}

#[derive(Clone, Copy, Debug)]
struct Span {
    offset: u64,
    end: u64,
    generation: u32,
    first: u64,
    last: u64,
    /// The least and greatest event time of its facts; `(i64::MAX, i64::MIN)` without one.
    min_t: i64,
    max_t: i64,
}

/// The batch at `s`, read and checked again; `None` if it no longer reads as the same complete batch.
fn read_batch_at(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    s: &Span,
    buf: &mut [u8],
) -> Option<BatchContent> {
    let (a, z) = (
        usize::try_from(s.offset).ok()?,
        usize::try_from(s.end).ok()?,
    );
    let part = buf.get_mut(a..z)?;
    if read_at_full(f, part, s.offset).ok()? != part.len() {
        return None;
    }
    let from = Resume {
        offset: s.offset,
        last_sequence: Some(s.first.saturating_sub(1)),
    };
    let mut scanner = Scanner::resume(&buf[..z], id, ScanLimit::Mark(s.end), from).ok()?;
    match scanner.next()? {
        Ok(Found::Batch(b))
            if b.offset == s.offset && b.end() == s.end && b.commit.first_sequence == s.first =>
        {
            BatchContent::decode(&b).ok()
        }
        _ => None,
    }
}

impl LogReader {
    /// The facts in the `wanted` ranges in the local log up to the durable mark. `lost(file_id, start, end)` names
    /// the invalid regions whose positions the caller takes as lost (`spec.md` §3 Recovery rule 2); any other region
    /// in a segment that may hold a range, and any segment that cannot be read, leaves that range incomplete. Fails
    /// only when the mark or the segment listing cannot be read.
    pub fn lookup(
        &mut self,
        wanted: &[Wanted],
        lost: &dyn Fn(u64, u64, u64) -> bool,
    ) -> Result<Lookup, ReaderError> {
        let mark = self.read_mark()?;
        // The lease's renewal is apart from the read: a failure to write it makes no range incomplete.
        let lease_kept = self.keep_alive().is_ok();
        let mut out = Lookup {
            facts: Vec::new(),
            complete: vec![true; wanted.len()],
            lease_kept,
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
        self.maps
            .retain(|id, _| names.iter().any(|n| n.file_id == *id));
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

    /// Reads one segment for the `candidates` among `wanted`; returns whether every batch that may hold one was read
    /// and only lost regions were met. Batches a lookup has read before are read again only when their event times
    /// may hold a wanted key ([`SegmentMap`]); batches appended since are scanned and added to the map.
    #[allow(clippy::too_many_arguments)]
    fn lookup_segment(
        &mut self,
        name: SegmentName,
        limit: ScanLimit,
        (generation, last_sequence): (u32, u64),
        wanted: &[Wanted],
        candidates: &[usize],
        lost: &dyn Fn(u64, u64, u64) -> bool,
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
        if len > super::MAX_SEGMENT_FILE {
            return false;
        }
        // Zeroed lazily by the allocator: only the bytes read are touched. The header and the parts read sit at their
        // file offsets, as the scanner reads them.
        let Ok(size) = usize::try_from(len) else {
            return false;
        };
        let mut buf = vec![0u8; size];
        let head = size.min(HEADER_LEN);
        if read_at_full(&*f, &mut buf[..head], 0).is_err() {
            return false;
        }
        let id = SegmentIdentity {
            store_id: self.store_id,
            file_id,
            generation: n.generation,
        };
        let mut by_series: BTreeMap<u32, Vec<(i64, i64)>> = BTreeMap::new();
        for &i in candidates {
            let w = &wanted[i];
            by_series
                .entry(w.series_id)
                .or_default()
                .push((w.from, w.to));
        }
        let take =
            |content: BatchContent, g: u32, first_sequence: u64, facts: &mut Vec<FoundFact>| {
                let mut sequence = first_sequence;
                for item in content.items {
                    let p = Position {
                        generation: g,
                        sequence,
                    };
                    sequence += item.sequences();
                    if let Item::Fact(row) = item
                        && by_series
                            .get(&row.series_id)
                            .is_some_and(|rs| rs.iter().any(|(a, z)| (*a..=*z).contains(&row.t)))
                    {
                        facts.push(FoundFact {
                            position: p,
                            received_at: content.received_at,
                            row,
                        });
                    }
                }
            };
        let first = facts.len();
        let mut map = self.maps.remove(&file_id).unwrap_or_default();
        if map.recovery || map.resume.is_some_and(|r| r.offset > len) {
            map = SegmentMap::default();
        }
        // The batches read before whose event times may hold a wanted key, each checked again as it is read.
        let may_hold = |s: &Span| {
            candidates
                .iter()
                .any(|&i| wanted[i].from <= s.max_t && s.min_t <= wanted[i].to)
        };
        let mut stale = false;
        for s in map.batches.iter().filter(|s| may_hold(s)) {
            if s.generation == generation && s.last > last_sequence {
                break;
            }
            match read_batch_at(&*f, &id, s, &mut buf) {
                Some(content) => take(content, s.generation, s.first, facts),
                None => {
                    stale = true;
                    break;
                }
            }
        }
        if stale {
            facts.truncate(first);
            map = SegmentMap::default();
        }
        let mut whole = map.regions.iter().all(|(a, z)| lost(file_id, *a, *z));
        let from = map.resume.unwrap_or(Resume {
            offset: HEADER_LEN as u64,
            last_sequence: None,
        });
        if from.offset < len {
            let at = from.offset as usize;
            match read_at_full(&*f, &mut buf[at..], from.offset) {
                Ok(n) => buf[at + n..].fill(0),
                Err(_) => return false,
            }
        }
        let Ok(mut scanner) = Scanner::resume(&buf, &id, limit, from) else {
            return false;
        };
        loop {
            let before = scanner.resume_point();
            let Some(found) = scanner.next() else { break };
            match found {
                Err(_) => return false,
                Ok(Found::Region(r)) => {
                    map.regions.push((r.start, r.end));
                    if !lost(file_id, r.start, r.end) {
                        whole = false;
                    }
                }
                Ok(Found::Batch(b)) => {
                    let g = b.commit.generation;
                    if g == generation && b.commit.last_sequence > last_sequence {
                        map.resume = Some(before);
                        break;
                    }
                    let Ok(content) = BatchContent::decode(&b) else {
                        // Kept as a region, so that every later lookup of the segment is incomplete too.
                        whole = false;
                        map.resume = Some(scanner.resume_point());
                        map.regions.push((b.offset, b.end()));
                        continue;
                    };
                    let (min_t, max_t) = content
                        .items
                        .iter()
                        .filter_map(|i| match i {
                            Item::Fact(r) => Some(r.t),
                            _ => None,
                        })
                        .fold((i64::MAX, i64::MIN), |(a, z), t| (a.min(t), z.max(t)));
                    map.batches.push(Span {
                        offset: b.offset,
                        end: b.end(),
                        generation: g,
                        first: b.commit.first_sequence,
                        last: b.commit.last_sequence,
                        min_t,
                        max_t,
                    });
                    if let [Item::Entry(crate::entries::Entry::Recovery(r))] =
                        content.items.as_slice()
                    {
                        map.recovery = true;
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
                    take(content, g, b.commit.first_sequence, facts);
                }
            }
            map.resume = Some(scanner.resume_point());
        }
        self.maps.insert(file_id, map);
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
