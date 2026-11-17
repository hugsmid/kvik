//! A point lookup in the local log: the facts of given series at given event times, read up to the durable mark
//! through the segment indexes of sealed segments (`format.md` A.8) and a scan of every other segment.
//!
//! A lookup reads what a refresh would accept (the reader rules of the module documentation): the mark read again
//! first, the mark's segment only below its offset, no batch above the mark's last position, and a recovery entry
//! voiding what precedes it in its segment above its `after`. It never moves what [`LogReader::refresh`] has taken.
//! Damage is never read as absence: a range is reported complete only if every segment that may hold it was read
//! and every invalid region met in such a segment is one the caller knows to be lost.
//!
//! Memory: sealed segments' indexes and the maps of segments' batches are kept least recently used within
//! [`LOOKUP_CACHE_BYTES`]; a segment is read a part of [`PIECE`] bytes at a time (more only for a batch longer than
//! that), and a batch read again alone.

use std::collections::{BTreeMap, HashMap, HashSet};

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

/// The most bytes a reader keeps for sealed segments' indexes and the maps of segments' batches together.
pub const LOOKUP_CACHE_BYTES: usize = 8 << 20;
/// The bytes of a segment a lookup holds at a time, unless one batch is longer.
pub const PIECE: u64 = 4 << 20;

/// A sealed segment's index as the lookup uses it: `None` when the segment has none that reads.
type SealIndex = Option<Vec<IndexRecord>>;

/// What lookups have read of one segment: each complete batch's place, positions and range of event times, the
/// invalid regions met, and where a scan of what was appended since resumes. One record per batch.
#[derive(Clone, Debug, Default)]
struct SegmentMap {
    batches: Vec<Span>,
    /// Invalid regions met, by start and end offset.
    regions: Vec<(u64, u64)>,
    resume: Option<Resume>,
    /// A recovery entry was met: what precedes it in the segment may be void (module documentation), so the segment
    /// is read whole by every lookup.
    recovery: bool,
}

impl SegmentMap {
    fn bytes(&self) -> usize {
        64 + self.batches.len() * std::mem::size_of::<Span>() + self.regions.len() * 16
    }
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

#[derive(Debug)]
struct Cached<T> {
    value: T,
    used: u64,
    bytes: usize,
}

/// The lookup's caches, by segment, least recently used first to go, and its part size.
#[derive(Debug)]
pub(super) struct Caches {
    index: HashMap<u64, Cached<SealIndex>>,
    maps: HashMap<u64, Cached<SegmentMap>>,
    tick: u64,
    bytes: usize,
    /// [`LOOKUP_CACHE_BYTES`] and [`PIECE`]; other values in tests only.
    cap: usize,
    piece: u64,
}

impl Default for Caches {
    fn default() -> Self {
        Caches {
            index: HashMap::new(),
            maps: HashMap::new(),
            tick: 0,
            bytes: 0,
            cap: LOOKUP_CACHE_BYTES,
            piece: PIECE,
        }
    }
}

impl Caches {
    fn keep_listed(&mut self, listed: &HashSet<u64>) {
        let mut freed = 0;
        self.index.retain(|id, c| {
            let keep = listed.contains(id);
            if !keep {
                freed += c.bytes;
            }
            keep
        });
        self.maps.retain(|id, c| {
            let keep = listed.contains(id);
            if !keep {
                freed += c.bytes;
            }
            keep
        });
        self.bytes -= freed;
    }

    fn put_index(&mut self, file_id: u64, value: SealIndex) {
        self.tick += 1;
        let bytes = 64
            + value
                .as_ref()
                .map_or(0, |r| r.len() * std::mem::size_of::<IndexRecord>());
        self.bytes += bytes;
        if let Some(old) = self.index.insert(
            file_id,
            Cached {
                value,
                used: self.tick,
                bytes,
            },
        ) {
            self.bytes -= old.bytes;
        }
    }

    fn take_map(&mut self, file_id: u64) -> SegmentMap {
        match self.maps.remove(&file_id) {
            Some(c) => {
                self.bytes -= c.bytes;
                c.value
            }
            None => SegmentMap::default(),
        }
    }

    fn put_map(&mut self, file_id: u64, value: SegmentMap) {
        self.tick += 1;
        let bytes = value.bytes();
        self.bytes += bytes;
        self.maps.insert(
            file_id,
            Cached {
                value,
                used: self.tick,
                bytes,
            },
        );
    }

    /// Drops the least recently used entries until the caches hold at most [`LOOKUP_CACHE_BYTES`].
    fn trim(&mut self) {
        if self.bytes <= self.cap {
            return;
        }
        let mut all: Vec<(u64, bool, u64)> = self
            .index
            .iter()
            .map(|(id, c)| (c.used, true, *id))
            .chain(self.maps.iter().map(|(id, c)| (c.used, false, *id)))
            .collect();
        all.sort_unstable();
        for (_, is_index, id) in all {
            if self.bytes <= self.cap {
                break;
            }
            let freed = if is_index {
                self.index.remove(&id).map_or(0, |c| c.bytes)
            } else {
                self.maps.remove(&id).map_or(0, |c| c.bytes)
            };
            self.bytes -= freed;
        }
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(super) fn limit(&mut self, cap: usize, piece: u64) {
        self.cap = cap;
        self.piece = piece;
    }
}

/// A buffer of `len` bytes, zeroed lazily by the allocator, holding the segment's header and the bytes from `from` to
/// `to` at their file offsets, as the scanner reads them; only those bytes are touched.
fn part(f: &dyn crate::fs::FsFile, len: u64, from: u64, to: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; usize::try_from(len).ok()?];
    let head = buf.len().min(HEADER_LEN);
    read_at_full(f, &mut buf[..head], 0).ok()?;
    let (a, z) = (usize::try_from(from).ok()?, usize::try_from(to).ok()?);
    if a < z {
        let got = read_at_full(f, buf.get_mut(a..z)?, from).ok()?;
        buf[a + got..z].fill(0);
    }
    Some(buf)
}

/// The batch at `s`, read into `buf` (the segment's header in place, zeroed lazily elsewhere) and checked again
/// alone; `None` if it no longer reads as the same complete batch.
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
    let got = read_at_full(f, buf.get_mut(a..z)?, s.offset).ok()?;
    if got != z - a {
        return None;
    }
    let buf = &buf[..z];
    let from = Resume {
        offset: s.offset,
        last_sequence: Some(s.first.saturating_sub(1)),
    };
    let mut scanner = Scanner::resume(buf, id, ScanLimit::Mark(s.end), from).ok()?;
    match scanner.next()? {
        Ok(Found::Batch(b))
            if b.offset == s.offset && b.end() == s.end && b.commit.first_sequence == s.first =>
        {
            BatchContent::decode(&b).ok()
        }
        _ => None,
    }
}

/// What one segment's read is for: the wanted ranges by series, the mark, and the regions taken as lost.
struct Query<'a> {
    by_series: BTreeMap<u32, Vec<(i64, i64)>>,
    ranges: Vec<(i64, i64)>,
    generation: u32,
    last_sequence: u64,
    lost: &'a dyn Fn(u64, u64, u64) -> bool,
}

impl Query<'_> {
    fn may_hold(&self, s: &Span) -> bool {
        self.ranges
            .iter()
            .any(|(a, z)| *a <= s.max_t && s.min_t <= *z)
    }

    fn take(&self, content: BatchContent, g: u32, first_sequence: u64, facts: &mut Vec<FoundFact>) {
        let mut sequence = first_sequence;
        for item in content.items {
            let p = Position {
                generation: g,
                sequence,
            };
            sequence += item.sequences();
            if let Item::Fact(row) = item
                && self
                    .by_series
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
        let listed: HashSet<u64> = names.iter().map(|n| n.file_id).collect();
        self.caches.keep_listed(&listed);
        for name in names {
            if name.sealed && !self.caches.index.contains_key(&name.file_id) {
                let index = self.read_seal_index(name);
                self.caches.put_index(name.file_id, index);
            }
            let candidates: Vec<usize> = match self.caches.index.get_mut(&name.file_id) {
                Some(c) if name.sealed => {
                    self.caches.tick += 1;
                    c.used = self.caches.tick;
                    (0..wanted.len())
                        .filter(|&i| {
                            let w = &wanted[i];
                            c.value.as_ref().is_none_or(|recs| {
                                recs.binary_search_by_key(&w.series_id, |r| r.series_id)
                                    .is_ok_and(|k| recs[k].min_t <= w.to && w.from <= recs[k].max_t)
                            })
                        })
                        .collect()
                }
                _ => (0..wanted.len()).collect(),
            };
            if candidates.is_empty() {
                continue;
            }
            let limit = if name.file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let mut by_series: BTreeMap<u32, Vec<(i64, i64)>> = BTreeMap::new();
            for &i in &candidates {
                let w = &wanted[i];
                by_series
                    .entry(w.series_id)
                    .or_default()
                    .push((w.from, w.to));
            }
            let query = Query {
                by_series,
                ranges: candidates
                    .iter()
                    .map(|&i| (wanted[i].from, wanted[i].to))
                    .collect(),
                generation: mark.generation,
                last_sequence: mark.last_sequence,
                lost,
            };
            let read = self.lookup_segment(name, limit, &query, &mut out.facts);
            out.segments_read += 1;
            if !read {
                for i in candidates {
                    out.complete[i] = false;
                }
            }
        }
        self.caches.trim();
        Ok(out)
    }

    /// Reads one segment for `query`; returns whether every batch that may hold a wanted fact was read and only lost
    /// regions were met. Batches a lookup has read before are read again only when their event times may hold a
    /// wanted key ([`SegmentMap`]); batches appended since are scanned a part at a time and added to the map.
    fn lookup_segment(
        &mut self,
        name: SegmentName,
        limit: ScanLimit,
        query: &Query<'_>,
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
        let id = SegmentIdentity {
            store_id: self.store_id,
            file_id,
            generation: n.generation,
        };
        let first = facts.len();
        let mut map = self.caches.take_map(file_id);
        if map.recovery || map.resume.is_some_and(|r| r.offset > len) {
            map = SegmentMap::default();
        }
        let mut stale = false;
        // One buffer for the batches read again: allocated once, touched only where they lie.
        let mut again: Option<Vec<u8>> = None;
        for s in map.batches.iter().filter(|s| query.may_hold(s)) {
            if s.generation == query.generation && s.last > query.last_sequence {
                break;
            }
            if again.is_none() {
                again = part(&*f, len, 0, 0);
            }
            let Some(buf) = again.as_mut() else {
                stale = true;
                break;
            };
            match read_batch_at(&*f, &id, s, buf) {
                Some(content) => query.take(content, s.generation, s.first, facts),
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
        let read = scan_new(
            &*f,
            &id,
            len,
            limit,
            self.caches.piece,
            query,
            &mut map,
            first,
            facts,
        );
        let whole = read.is_some()
            && map
                .regions
                .iter()
                .all(|(a, z)| (query.lost)(file_id, *a, *z));
        self.caches.put_map(file_id, map);
        whole
    }

    /// A sealed segment's index, read from its seal blocks.
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

/// Scans what follows the map's resume point up to the limit, a part at a time, adding batches and regions to the
/// map and the wanted facts to `facts`; `None` if the segment could not be read. A part ends at [`PIECE`] bytes past
/// where it starts; a region running to a part's end that may be a batch cut by it is read again in a longer part.
#[allow(clippy::too_many_arguments)]
fn scan_new(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    len: u64,
    limit: ScanLimit,
    part_bytes: u64,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> Option<()> {
    let mut from = map.resume.unwrap_or(Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    });
    let mut piece = part_bytes;
    loop {
        let to = from
            .offset
            .saturating_add(piece)
            .next_multiple_of(8)
            .min(len);
        let last = to >= len;
        if !last && from.offset >= to {
            return Some(());
        }
        let lim = if last { limit } else { ScanLimit::Mark(to) };
        let buf = part(
            f,
            if last { len } else { to },
            from.offset,
            if last { len } else { to },
        )?;
        let mut scanner = Scanner::resume(&buf, id, lim, from).ok()?;
        let mut found = Vec::new();
        let mut before = Vec::new();
        loop {
            before.push(scanner.resume_point());
            match scanner.next() {
                None => break,
                Some(Err(_)) => return None,
                Some(Ok(x)) => found.push(x),
            }
        }
        before.push(scanner.resume_point());
        // A region running to the end of a part that is not the last may be a batch the part cut.
        let cut = !last && matches!(found.last(), Some(Found::Region(r)) if r.end == to);
        if cut && found.len() == 1 {
            piece = piece.saturating_mul(2);
            continue;
        }
        let keep = if cut { found.len() - 1 } else { found.len() };
        for (k, x) in found.into_iter().take(keep).enumerate() {
            match x {
                Found::Region(r) => map.regions.push((r.start, r.end)),
                Found::Batch(b) => {
                    let g = b.commit.generation;
                    if g == query.generation && b.commit.last_sequence > query.last_sequence {
                        map.resume = Some(before[k]);
                        return Some(());
                    }
                    let Ok(content) = BatchContent::decode(&b) else {
                        // Kept as a region, so that every later lookup of the segment is incomplete too.
                        map.regions.push((b.offset, b.end()));
                        map.resume = Some(before[k + 1]);
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
                    query.take(content, g, b.commit.first_sequence, facts);
                }
            }
            map.resume = Some(before[k + 1]);
        }
        if last && !cut {
            map.resume = Some(*before.last().expect("pushed above"));
            return Some(());
        }
        from = map.resume.unwrap_or(from);
        piece = part_bytes;
    }
}
