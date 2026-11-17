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
//! [`LOOKUP_CACHE_BYTES`], also during one lookup; a segment is read a part of [`PIECE`] bytes at a time (more only
//! for a batch longer than that), and each batch read again, or appended since, is read into a buffer of its own
//! length.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::files::read_at_full;
use super::{LogReader, ReaderError};
use crate::batch::{BatchRead, Refusal, SegmentIdentity, read_batch_in};
use crate::block::{BLOCK_HEADER_LEN, block_type, read_block};
use crate::body::{
    END_MARKER_LEN, IndexRecord, SegmentIndex, TRAILER_LEN, Trailer, decode_end_marker,
};
use crate::content::{BatchContent, Item};
use crate::facts::FactRow;
use crate::header::HEADER_LEN;
use crate::name::SegmentName;
use crate::scan::{Found, HeaderState, InvalidRegion, Resume, ScanLimit, Scanner, read_header};
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
    /// The recovery entries met, by the index of their batch in `batches`, with their `after`: a lookup that reads
    /// the batches again voids at each what it took above its `after` from the batches before it, as a scan does
    /// (Q-831).
    recoveries: Vec<(usize, Position)>,
}

impl SegmentMap {
    fn bytes(&self) -> usize {
        64 + self.batches.len() * std::mem::size_of::<Span>()
            + self.regions.len() * 16
            + self.recoveries.len() * std::mem::size_of::<(usize, Position)>()
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
    /// The most bytes held at any time, and the largest entry.
    #[cfg(test)]
    peak: (usize, usize),
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
            #[cfg(test)]
            peak: (0, 0),
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
        #[cfg(test)]
        {
            self.peak = (self.peak.0.max(self.bytes), self.peak.1.max(bytes));
        }
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
    pub(super) fn peak(&self) -> (usize, usize) {
        self.peak
    }

    #[cfg(test)]
    pub(super) fn limit(&mut self, cap: usize, piece: u64) {
        self.cap = cap;
        self.piece = piece;
    }
}

#[cfg(test)]
thread_local! {
    /// Bytes of the buffers lookups on this thread allocated to read segments.
    pub(super) static BUFFER_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn buffer(len: usize) -> Vec<u8> {
    #[cfg(test)]
    BUFFER_BYTES.with(|c| c.set(c.get() + len as u64));
    vec![0u8; len]
}

/// A buffer of `len` bytes, zeroed lazily by the allocator, holding the segment's header and the bytes from `from` to
/// `to` at their file offsets, as the scanner reads them; only those bytes are touched.
fn part(f: &dyn crate::fs::FsFile, len: u64, from: u64, to: u64) -> Option<Vec<u8>> {
    let mut buf = buffer(usize::try_from(len).ok()?);
    let head = buf.len().min(HEADER_LEN);
    read_at_full(f, &mut buf[..head], 0).ok()?;
    let (a, z) = (usize::try_from(from).ok()?, usize::try_from(to).ok()?);
    if a < z {
        let got = read_at_full(f, buf.get_mut(a..z)?, from).ok()?;
        buf[a + got..z].fill(0);
    }
    Some(buf)
}

/// The batch at `s`, read into a buffer of its own length and checked again alone; `None` if it no longer reads as
/// the same complete batch.
fn read_batch_at(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    s: &Span,
) -> Option<BatchContent> {
    let mut buf = buffer(usize::try_from(s.end - s.offset).ok()?);
    if read_at_full(f, &mut buf, s.offset).ok()? != buf.len() {
        return None;
    }
    match read_batch_in(
        &buf,
        s.offset,
        id,
        s.offset,
        s.end,
        s.first,
        Refusal::AtOnce,
        true,
    ) {
        BatchRead::Complete(b, bodies)
            if b.end() == s.end && b.commit.first_sequence == s.first =>
        {
            BatchContent::from_bodies(s.first, bodies).ok()
        }
        _ => None,
    }
}

/// The segment's header as a scan reads it: `None` if it refuses the segment.
fn header(f: &dyn crate::fs::FsFile, id: &SegmentIdentity) -> Option<HeaderState> {
    let mut h = vec![0u8; HEADER_LEN];
    let got = read_at_full(f, &mut h, 0).ok()?;
    h.truncate(got);
    read_header(&h, id).ok()
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
            // After each segment, so that a lookup over many segments holds at most the bound too.
            self.caches.trim();
        }
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
        if map.resume.is_some_and(|r| r.offset > len) {
            map = SegmentMap::default();
        }
        let mut stale = map.batches.iter().any(|s| query.may_hold(s)) && header(&*f, &id).is_none();
        let mut recoveries = map.recoveries.iter().peekable();
        // In file order, as a scan takes them: it stops at the first batch above the mark's last position.
        for (k, s) in map.batches.iter().enumerate() {
            if stale || (s.generation == query.generation && s.last > query.last_sequence) {
                break;
            }
            if let Some((_, after)) = recoveries.next_if(|(at, _)| *at == k) {
                void_above(facts, first, *after);
                continue;
            }
            if !query.may_hold(s) {
                continue;
            }
            match read_batch_at(&*f, &id, s) {
                Some(content) => query.take(content, s.generation, s.first, facts),
                None => stale = true,
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
    // Every part's scan reads the header first and refuses the segment on it; complete batches up to the mark are
    // then read into a buffer of their own length, and only what is not one is scanned in a buffer from the file's
    // start.
    let header = header(f, id)?;
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
        if let ScanLimit::Mark(m) = lim
            && from.offset <= m
        {
            match take_complete(f, id, header, from, m, query, map, first, facts)? {
                Taken::Stopped => return Some(()),
                Taken::To(p) if p.offset == m => {
                    from = p;
                    if last {
                        map.resume = Some(from);
                        return Some(());
                    }
                    piece = part_bytes;
                    continue;
                }
                Taken::To(p) => from = p,
            }
        }
        let to = from
            .offset
            .saturating_add(piece)
            .next_multiple_of(8)
            .min(len)
            .max(to);
        let last = to >= len;
        let lim = if last { limit } else { ScanLimit::Mark(to) };
        let buf = part(
            f,
            if last { len } else { to },
            from.offset,
            if last { len } else { to },
        )?;
        let mut scanner = Scanner::resume(&buf, id, lim, from).ok()?.keeping_content();
        // Each finding is taken as it is read; only a region is held back, since one running to the end of a part
        // that is not the last may be a batch the part cut.
        let mut held: Option<(InvalidRegion, Resume)> = None;
        let mut found = 0usize;
        loop {
            let before = scanner.resume_point();
            let x = match scanner.next() {
                None => break,
                Some(Err(_)) => return None,
                Some(Ok(x)) => x,
            };
            let after = scanner.resume_point();
            found += 1;
            if let Some((r, at)) = held.take() {
                map.regions.push((r.start, r.end));
                map.resume = Some(at);
            }
            match x {
                Found::Region(r) => held = Some((r, after)),
                Found::Batch(b) => {
                    let kept = scanner.take_content();
                    if take_found(&b, kept, before, after, query, map, first, facts) {
                        return Some(());
                    }
                }
            }
        }
        let end = scanner.resume_point();
        let cut = !last && held.as_ref().is_some_and(|(r, _)| r.end == to);
        if cut && found == 1 {
            piece = piece.saturating_mul(2);
            continue;
        }
        if let Some((r, at)) = held.filter(|_| !cut) {
            map.regions.push((r.start, r.end));
            map.resume = Some(at);
        }
        if last && !cut {
            map.resume = Some(end);
            return Some(());
        }
        from = map.resume.unwrap_or(from);
        piece = part_bytes;
    }
}

/// How far [`take_complete`] took the batches it read.
enum Taken {
    /// Up to this point: the limit, or the start of what is not a complete batch.
    To(Resume),
    /// At a batch above the mark's last position.
    Stopped,
}

/// Takes the complete batches from `from` up to `limit` as a scan to the mark would, each read into a buffer of its own
/// length; `None` on a refusal.
#[allow(clippy::too_many_arguments)]
fn take_complete(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    header: HeaderState,
    mut from: Resume,
    limit: u64,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> Option<Taken> {
    let mut buf = buffer(usize::try_from(limit - from.offset).ok()?);
    let got = read_at_full(f, &mut buf, from.offset).ok()?;
    buf[got..].fill(0);
    let base = from.offset;
    let header_first = match header {
        HeaderState::Valid(h) => h.base_sequence,
        HeaderState::Invalid => 0,
    };
    while from.offset < limit {
        let min_first = from
            .last_sequence
            .map_or(header_first, |l| l.saturating_add(1));
        let (b, bodies) = match read_batch_in(
            &buf,
            base,
            id,
            from.offset,
            limit,
            min_first,
            Refusal::AtOnce,
            true,
        ) {
            BatchRead::Complete(b, bodies) => (b, bodies),
            BatchRead::Refused(_) => return None,
            BatchRead::Incomplete { .. } => break,
        };
        let after = Resume {
            offset: b.end(),
            last_sequence: Some(b.commit.last_sequence),
        };
        let content = BatchContent::from_bodies(b.commit.first_sequence, bodies).ok();
        if take_found(&b, content, from, after, query, map, first, facts) {
            return Some(Taken::Stopped);
        }
        from = after;
    }
    Some(Taken::To(from))
}

/// Drops the facts this segment's read took, from index `first` of `facts`, above a recovery entry's `after`.
fn void_above(facts: &mut Vec<FoundFact>, first: usize, after: Position) {
    let mut j = 0;
    facts.retain(|x| {
        j += 1;
        j <= first || x.position <= after
    });
}

/// Takes a complete batch a scan found into the map and the wanted facts; returns whether the scan stops at it, being
/// above the mark's last position.
#[allow(clippy::too_many_arguments)]
fn take_found(
    b: &crate::batch::Batch<'_>,
    kept: Option<BatchContent>,
    before: Resume,
    after: Resume,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> bool {
    let g = b.commit.generation;
    if g == query.generation && b.commit.last_sequence > query.last_sequence {
        map.resume = Some(before);
        return true;
    }
    let Some(content) = kept.or_else(|| BatchContent::decode(b).ok()) else {
        // A guard: the scanner ran the same body checks, so a complete batch decodes. Kept as a region, so that every
        // later lookup of the segment is incomplete too.
        map.regions.push((b.offset, b.end()));
        map.resume = Some(after);
        return false;
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
    if let Some(r) = content.recovery() {
        let after = Position {
            generation: g,
            sequence: r.after,
        };
        map.recoveries.push((map.batches.len() - 1, after));
        void_above(facts, first, after);
    }
    query.take(content, g, b.commit.first_sequence, facts);
    map.resume = Some(after);
    false
}
