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
use crate::content::{BatchContent, Taken};
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

/// What [`LogReader::lookup_within`] keeps: at most `budget` bytes of facts (the records their vector has room for and
/// their texts), at positions above `above` and at or below `upto` only.
#[derive(Clone, Copy, Debug)]
pub struct Within {
    /// The bytes the facts may take.
    pub budget: u64,
    /// The position facts are kept above.
    pub above: Option<Position>,
    /// The position facts are kept at or below.
    pub upto: Option<Position>,
    /// The facts expected ([`LogReader::estimate`]): room for them is taken from the budget at once, so that the
    /// facts' vector need not grow by doubling.
    pub expect: u64,
}

impl Default for Within {
    fn default() -> Self {
        Within {
            budget: u64::MAX,
            above: None,
            upto: None,
            expect: 0,
        }
    }
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
    /// The mark's reset stamp: facts at or below a cut taken under another stamp may differ from the cut's.
    pub stamp: crate::mark::ResetStamp,
    /// Segments read for the lookup.
    pub segments_read: u64,
    /// Whether the lease was renewed when due ([`LogReader::keep_alive`]).
    pub lease_kept: bool,
    /// The facts found would have passed the byte budget ([`LogReader::lookup_within`]): `facts` is partial.
    pub overflowed: bool,
    /// The file ids of the segments listed up to the mark, ascending ([`LogReader::lookup_within`] only).
    pub listed: Vec<u64>,
}

/// What the facts of one lookup may take: the records the facts' vector has room for and each kept text, so that
/// its growth is counted before it happens; once passed, no further fact is kept.
#[derive(Debug)]
pub(super) struct Budget {
    room: std::cell::Cell<u64>,
    over: std::cell::Cell<bool>,
}

impl Budget {
    pub(super) fn new(bytes: u64) -> Self {
        Budget {
            room: std::cell::Cell::new(bytes),
            over: std::cell::Cell::new(false),
        }
    }

    pub(super) fn over(&self) -> bool {
        self.over.get()
    }

    fn charge(&self, b: u64) -> bool {
        if self.over.get() || b > self.room.get() {
            self.over.set(true);
            return false;
        }
        self.room.set(self.room.get() - b);
        true
    }

    /// Pushes `f` if the budget has room for it, and for the vector's growth first when it is full: room for the old
    /// buffer again and the new one, as both are held while it moves.
    fn push(&self, facts: &mut Vec<FoundFact>, f: FoundFact) {
        if facts.len() == facts.capacity() {
            let record = std::mem::size_of::<FoundFact>() as u64;
            let (old, more) = (facts.capacity() as u64, facts.capacity().max(1));
            if (old + more as u64).saturating_mul(record) > self.room.get() {
                self.over.set(true);
            }
            if !self.charge(more as u64 * record) {
                return;
            }
            facts.reserve_exact(more);
        }
        let text = match &f.row.value {
            crate::facts::Value::Text(s) => s.len() as u64,
            _ => 0,
        };
        if self.charge(text) {
            facts.push(f);
        }
    }
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
    /// Per series, by id, its facts in `batches` and their least and greatest `t`: an A.8 index of what is mapped,
    /// for [`LogReader::estimate`].
    tally: Vec<IndexRecord>,
}

impl SegmentMap {
    fn bytes(&self) -> usize {
        64 + self.batches.len() * std::mem::size_of::<Span>()
            + self.regions.len() * 16
            + self.recoveries.len() * std::mem::size_of::<(usize, Position)>()
            + self.tally.len() * std::mem::size_of::<IndexRecord>()
    }
}

/// Counts a fact of `series` at `t` in `tally`.
fn note(tally: &mut Vec<IndexRecord>, series_id: u32, t: i64) {
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
    commit_time: i64,
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
    /// Segments a lookup took a fact from: one gone with no record naming its removal makes every later lookup of
    /// this reader incomplete (Q-951, Q-1066).
    stood: std::collections::BTreeSet<u64>,
    /// The segments the last lookup listed.
    listed: Option<Vec<u64>>,
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
            stood: std::collections::BTreeSet::new(),
            listed: None,
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
    /// Keeps the caches of listed segments only; returns whether a segment a lookup took a fact from is unlisted.
    pub(super) fn keep_listed(&mut self, listed: &HashSet<u64>) -> bool {
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
        !self.stood.iter().all(|id| listed.contains(id))
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

    /// The last position of the mapped batches of a segment, up to the mark, whose commit time is at or before `at`;
    /// positions a later recovery entry of the segment voids are passed over, as they are given again to batches
    /// committed after it (Q-1064).
    pub(super) fn last_committed(&self, file_id: u64, at: i64, q: &Query<'_>) -> Option<Position> {
        let m = &self.maps.get(&file_id)?.value;
        m.batches
            .iter()
            .enumerate()
            .take_while(|(_, s)| !(s.generation == q.generation && s.last > q.last_sequence))
            .filter(|(_, s)| s.commit_time <= at)
            .filter_map(|(j, s)| {
                let kept = m
                    .recoveries
                    .iter()
                    .filter(|(k, after)| *k > j && after.generation == s.generation)
                    .map(|(_, after)| after.sequence)
                    .fold(s.last, u64::min);
                (kept >= s.first).then_some(Position {
                    generation: s.generation,
                    sequence: kept,
                })
            })
            .last()
    }

    /// Drops the least recently used entries until the caches hold at most [`LOOKUP_CACHE_BYTES`].
    pub(super) fn trim(&mut self) {
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

/// `take` of the batch at `s`, read into a buffer of its own length and checked again alone; `None` if it no longer
/// reads as the same complete batch.
fn read_batch_at<R>(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    s: &Span,
    take: impl FnOnce(&crate::batch::Batch<'_>) -> Option<R>,
) -> Option<R> {
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
        false,
    ) {
        BatchRead::Complete(b, _) if b.end() == s.end && b.commit.first_sequence == s.first => {
            take(&b)
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

/// The wanted ranges by series: in a table by id when the ids are small, as every row of a read is looked up.
pub(super) enum BySeries {
    Table(Vec<u16>, Vec<Vec<(i64, i64)>>),
    Map(BTreeMap<u32, Vec<(i64, i64)>>),
}

/// The greatest series id [`BySeries`] keeps a table for.
const TABLE_IDS: u32 = 4_096;

impl BySeries {
    pub(super) fn new(map: BTreeMap<u32, Vec<(i64, i64)>>) -> Self {
        match map.keys().next_back() {
            Some(&max) if max <= TABLE_IDS => {
                let mut table = vec![u16::MAX; max as usize + 1];
                let mut lists = Vec::with_capacity(map.len());
                for (k, (id, ranges)) in map.into_iter().enumerate() {
                    table[id as usize] = k as u16;
                    lists.push(ranges);
                }
                BySeries::Table(table, lists)
            }
            _ => BySeries::Map(map),
        }
    }

    fn wants(&self, series: u32, t: i64) -> bool {
        let ranges = match self {
            BySeries::Table(table, lists) => match table.get(series as usize) {
                Some(&k) if k != u16::MAX => &lists[usize::from(k)],
                _ => return false,
            },
            BySeries::Map(map) => match map.get(&series) {
                Some(r) => r,
                None => return false,
            },
        };
        ranges.iter().any(|(a, z)| (*a..=*z).contains(&t))
    }
}

/// What one segment's read is for: the wanted ranges by series, the mark, and the regions taken as lost.
pub(super) struct Query<'a> {
    pub(super) by_series: BySeries,
    pub(super) ranges: Vec<(i64, i64)>,
    pub(super) generation: u32,
    pub(super) last_sequence: u64,
    pub(super) lost: &'a dyn Fn(u64, u64, u64) -> bool,
    pub(super) budget: &'a Budget,
    /// Facts are kept above this position and at or below `upto` only.
    pub(super) above: Option<Position>,
    pub(super) upto: Option<Position>,
}

impl Query<'_> {
    fn may_hold(&self, s: &Span) -> bool {
        let pos = |sequence| Position {
            generation: s.generation,
            sequence,
        };
        self.ranges
            .iter()
            .any(|(a, z)| *a <= s.max_t && s.min_t <= *z)
            && self.above.is_none_or(|a| pos(s.last) > a)
            && self.upto.is_none_or(|u| pos(s.first) <= u)
    }

    /// Keeps the wanted facts of a complete batch whose bodies were checked, within the budget, counting every fact
    /// in `tally` if given; `None` if a body does not decode.
    fn take(
        &self,
        b: &crate::batch::Batch<'_>,
        facts: &mut Vec<FoundFact>,
        tally: Option<&mut Vec<IndexRecord>>,
    ) -> Option<Taken> {
        let g = b.commit.generation;
        let tally = tally.map(std::cell::RefCell::new);
        let wants = |series: u32, t: i64| {
            if let Some(c) = &tally {
                note(&mut c.borrow_mut(), series, t);
            }
            self.by_series.wants(series, t)
        };
        let mut keep = |sequence: u64, received_at: i64, row: FactRow| {
            let position = Position {
                generation: g,
                sequence,
            };
            if self.above.is_none_or(|a| position > a) && self.upto.is_none_or(|u| position <= u) {
                self.budget.push(
                    facts,
                    FoundFact {
                        position,
                        received_at,
                        row,
                    },
                );
            }
        };
        BatchContent::take_wanted(b, &wants, &mut keep).ok()
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
        self.lookup_within(wanted, lost, Within::default())
    }

    /// [`LogReader::lookup`] within `within`: a lookup that would keep more than its budget stops and says so in
    /// [`Lookup::overflowed`], its facts partial.
    pub fn lookup_within(
        &mut self,
        wanted: &[Wanted],
        lost: &dyn Fn(u64, u64, u64) -> bool,
        within: Within,
    ) -> Result<Lookup, ReaderError> {
        let budget = Budget::new(within.budget);
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
            stamp: mark.stamp,
            segments_read: 0,
            overflowed: false,
            listed: Vec::new(),
        };
        if wanted.is_empty() {
            return Ok(out);
        }
        if within.expect > 0 {
            let record = std::mem::size_of::<FoundFact>() as u64;
            let n = within.expect.min(within.budget / record);
            if budget.charge(n * record) {
                out.facts.reserve_exact(usize::try_from(n).unwrap_or(0));
            }
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
        out.listed = names.iter().map(|n| n.file_id).collect();
        self.caches.listed = Some(out.listed.clone());
        if self.caches.keep_listed(&listed) {
            out.complete.fill(false);
            return Ok(out);
        }
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
                by_series: BySeries::new(by_series),
                ranges: candidates
                    .iter()
                    .map(|&i| (wanted[i].from, wanted[i].to))
                    .collect(),
                generation: mark.generation,
                last_sequence: mark.last_sequence,
                lost,
                budget: &budget,
                above: within.above,
                upto: within.upto,
            };
            let before = out.facts.len();
            let read = self.lookup_segment(name, limit, &query, &mut out.facts);
            out.segments_read += 1;
            if out.facts.len() > before {
                self.caches.stood.insert(name.file_id);
            }
            if !read {
                for i in candidates {
                    out.complete[i] = false;
                }
            }
            if budget.over() {
                out.overflowed = true;
                self.caches.trim();
                break;
            }
            // After each segment, so that a lookup over many segments holds at most the bound too.
            self.caches.trim();
        }
        Ok(out)
    }

    /// Reads one segment for `query`; returns whether every batch that may hold a wanted fact was read and only lost
    /// regions were met. Batches a lookup has read before are read again only when their event times may hold a
    /// wanted key ([`SegmentMap`]); batches appended since are scanned a part at a time and added to the map.
    pub(super) fn lookup_segment(
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
            let at = facts.len();
            if read_batch_at(&*f, &id, s, |b| query.take(b, facts, None)).is_none() {
                facts.truncate(at);
                stale = true;
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

    /// About how many facts a lookup of `wanted` would find in the segments the last lookup listed, from the sealed
    /// segments' indexes and the tallies of the segments lookups mapped (segments listed since, and facts appended
    /// since a map's last scan, are not counted); `None` before a lookup has listed them, or when a listed segment has
    /// neither. A guide for sizing lookups, never a bound.
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

    /// A sealed segment's index, read from its seal blocks.
    fn read_seal_index(&self, name: SegmentName) -> SealIndex {
        let (f, trailer, trailer_offset) = self.read_trailer(name)?;
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

    /// A sealed segment's trailer with the open file and the trailer's offset; `None` if it does not read.
    pub(super) fn read_trailer(
        &self,
        name: SegmentName,
    ) -> Option<(Box<dyn crate::fs::FsFile>, Trailer, u64)> {
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
        Some((f, trailer, trailer_offset))
    }
}

mod append;
use append::{scan_new, void_above};
