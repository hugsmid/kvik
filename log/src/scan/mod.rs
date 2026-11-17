//! Reading a log segment: complete batches, invalid regions, the tail and the seal (`format.md` A.2, A.4, A.9).

use crate::batch::{Batch, BatchFault, BatchRead, Refusal, SegmentIdentity, read_batch};
use crate::block::{BLOCK_HEADER_LEN, block_type};
use crate::body::{Commit, END_MARKER_LEN, SegmentIndex, TRAILER_LEN, Trailer, decode_end_marker};
use crate::bytes::slice;
use crate::crc::crc32c;
use crate::error::{Element, FormatError};
use crate::header::{FileHeader, HEADER_LEN, kind};

#[doc(hidden)]
pub mod legacy;
mod reads;
mod resync;
mod walk;

pub(crate) use reads::Reads;
pub use reads::{FAILED_READ_BUDGET, FailedReads, HEADERS_PER_WORD, ScanStats};
use resync::{Chains, CommitSearch, readable_commits};
use walk::Walks;

/// How far a segment is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanLimit {
    /// A reader process reading the segment the durable mark points into: only below the mark's `offset`
    /// (`format.md` A.10). Nothing above it is looked at, so there is no tail and no seal; whatever below the mark
    /// is not a complete batch is corruption (`spec.md` §3 Recovery rule 2).
    Mark(u64),
    /// The whole file: recovery's scan of an unsealed segment, or a sealed segment, which is read whole.
    End,
}

/// What an invalid region was found to be (`format.md` A.14, lost-range field 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossCause {
    /// Cause 1: bytes that are not checksum-valid blocks forming a complete batch.
    Corruption,
    /// Cause 4: a checksum-valid data block whose body fails the checks of its type (A.2).
    InvalidContent,
}

/// An invalid region that is corruption, not a torn tail: it is followed by a complete batch, lies below the mark,
/// or lies before the seal blocks of a sealed segment (`spec.md` §3 Recovery rule 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvalidRegion {
    /// File offset where the region starts: just past the last kept batch, or the header.
    pub start: u64,
    /// File offset just past the region: the next complete batch, the limit or the seal blocks.
    pub end: u64,
    /// The last sequence of the batch kept before the region, if any.
    pub last_sequence_before: Option<u64>,
    /// The first sequence of the batch kept after the region, if any.
    pub first_sequence_after: Option<u64>,
    /// Why the region's first bytes were rejected.
    pub cause: LossCause,
}

/// A checksum-valid commit block found in a tail: one of the "readable commit blocks" a recovery entry counts
/// (`spec.md` §3 Recovery rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadableCommit {
    /// File offset of the commit block header.
    pub offset: u64,
    /// Its body.
    pub commit: Commit,
}

/// What follows the last complete batch of a segment read to its end, below any seal blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tail {
    /// Only zero bytes, or nothing, from `offset` to the end.
    Clean {
        /// Where the zeros start: just past the last complete batch, or the header.
        offset: u64,
    },
    /// Non-zero bytes that do not form a complete batch. For an unsealed segment this is a torn tail, or, below a
    /// valid mark, corruption at the tail; recovery tells them apart by the mark and the boot identity
    /// (`spec.md` §3 Recovery rules 1 to 3). In a segment that is not the last in use it is seal blocks or damage
    /// (A.4).
    Torn {
        /// Where the tail starts: just past the last complete batch, or the header.
        start: u64,
        /// Just past the last non-zero byte: the tail's length is `end - start`.
        end: u64,
        /// The first block of the tail could not be read for this cause.
        cause: LossCause,
        /// The checksum-valid commit blocks in the tail, in file order.
        commits: Vec<ReadableCommit>,
    },
}

/// Whether the seal reached its last step (`format.md` A.4 Sealing).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SealState {
    /// The file ends with the end marker: sealed.
    Sealed,
    /// The last non-zero bytes are a valid trailer and end marker but zeros follow: a crash between seal steps 1 and
    /// 2. Recovery finishes the seal from step 2.
    LengthPending,
}

/// The seal blocks of a segment, found from its last non-zero bytes (A.4, A.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seal {
    /// Where the seal blocks start: the segment index block, or the trailer block if there is no index.
    pub start: u64,
    /// File offset of the trailer block header.
    pub trailer_offset: u64,
    /// The trailer.
    pub trailer: Trailer,
    /// The segment index, if the trailer names one; `None` inside if that block is not a valid index.
    pub index: Option<Option<SegmentIndex>>,
    /// File offset just past the end marker.
    pub end: u64,
    /// Whether the length was set.
    pub state: SealState,
}

impl Seal {
    /// Whether `file_crc32c` matches the bytes before the trailer block (A.9). Verified by the sealer, before upload
    /// or compaction, and by the scrub; not at open and not on reads.
    pub fn file_crc_matches(&self, bytes: &[u8]) -> bool {
        slice(bytes, 0, self.trailer_offset).is_some_and(|b| crc32c(b) == self.trailer.file_crc32c)
    }
}

/// The header as the scan found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeaderState {
    /// A valid log segment header of this store, agreeing with the file name.
    Valid(FileHeader),
    /// Short, wrong magic or wrong checksum. Batches are still read, checked against the identity from the name.
    Invalid,
}

/// The result of reading one log segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport<'a> {
    /// The header.
    pub header: HeaderState,
    /// The complete batches, in file order.
    pub batches: Vec<Batch<'a>>,
    /// Invalid regions that are corruption, in file order.
    pub regions: Vec<InvalidRegion>,
    /// What follows the last batch; `None` when read up to the mark or when sealed.
    pub tail: Option<Tail>,
    /// The seal blocks, when read to the end and found.
    pub seal: Option<Seal>,
}

impl ScanReport<'_> {
    /// Whether the segment is in use: a valid header with `base_sequence ≥ 1`, or a complete batch (A.4).
    pub fn in_use(&self) -> bool {
        matches!(self.header, HeaderState::Valid(h) if h.base_sequence >= 1)
            || !self.batches.is_empty()
    }

    /// The last sequence of the last complete batch.
    pub fn last_sequence(&self) -> Option<u64> {
        self.batches.last().map(|b| b.commit.last_sequence)
    }
}

/// Reads the log segment in `bytes`, checking its batches against `id`: [`Scanner`] run to its end.
///
/// Returns a version refusal for a header of a newer major version, of another kind or of another store, a valid
/// header that disagrees with `id`, or an unknown critical block in a checksum-valid batch. Damage is never an
/// error: it is reported in [`ScanReport::regions`] and [`ScanReport::tail`].
pub fn scan<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
) -> Result<ScanReport<'a>, FormatError> {
    scan_with_stats(bytes, id, limit).0
}

/// [`scan`], with the work it did; [`ScanStats::within_bound`] holds for every input (`format.md` A, common rules).
pub fn scan_with_stats<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    let from = Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    };
    collect(Scanner::resume_inner(bytes, id, limit, from, true, false))
}

/// [`scan`] to the end with any seal blocks ignored: their bytes are read as whatever precedes them (a torn batch or
/// a value). Recovery uses it for an `.open` file whose seal fails the structural rule of A.4 (the seal blocks start
/// exactly at the end of the last complete batch, and the trailer's `batch_count` and `last_sequence` equal the
/// scan's), or that is the last in use (Q-109).
pub(crate) fn scan_ignoring_seal<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
) -> Result<ScanReport<'a>, FormatError> {
    ignoring_seal(bytes, id, false).0
}

fn ignoring_seal<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    legacy: bool,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    let from = Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    };
    collect(Scanner::resume_inner(
        bytes,
        id,
        ScanLimit::End,
        from,
        false,
        legacy,
    ))
}

fn collect<'a>(
    s: Result<Scanner<'a>, (FormatError, ScanStats)>,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    let mut s = match s {
        Ok(s) => s,
        Err((e, stats)) => return (Err(e), stats),
    };
    let mut batches = Vec::new();
    let mut regions = Vec::new();
    for found in &mut s {
        match found {
            Ok(Found::Batch(b)) => batches.push(b),
            Ok(Found::Region(r)) => regions.push(r),
            Err(e) => return (Err(e), s.stats()),
        }
    }
    let stats = s.stats();
    let report = ScanReport {
        header: s.header,
        tail: s.tail().cloned(),
        seal: s.seal,
        batches,
        regions,
    };
    (Ok(report), stats)
}

/// Where a scan starts again: just past the last batch or region read, and the last kept sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Resume {
    /// File offset to read from (at least the header's end).
    pub offset: u64,
    /// The last sequence of the last batch kept before `offset`; `None` at the start of the file.
    pub last_sequence: Option<u64>,
}

/// One finding of a scan, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found<'a> {
    /// A complete batch.
    Batch(Batch<'a>),
    /// An invalid region that is corruption.
    Region(InvalidRegion),
}

/// A scan of one log segment as an iterator of [`Found`] items, resumable from a [`Resume`] point: a reader
/// that tails a segment as the mark moves, or a large segment, reads each part once.
#[derive(Clone, Debug)]
pub struct Scanner<'a> {
    bytes: &'a [u8],
    id: SegmentIdentity,
    header: HeaderState,
    seal: Option<Seal>,
    area_end: u64,
    bounded: bool,
    base: u64,
    pos: u64,
    last: Option<u64>,
    pending: Option<Batch<'a>>,
    tail: Option<Tail>,
    done: bool,
    reads: Reads<'a>,
    chains: Chains,
    walks: Walks,
    legacy: bool,
    /// Set at the first boundary read that finds no complete batch: from there on the scan takes the bounded path
    /// (prefix table, remembered verdicts); before it, each batch is read once with direct checksums (Q-231).
    damaged: bool,
    nonzero_end: Option<u64>,
}

impl<'a> Scanner<'a> {
    /// A scan from the start of the segment.
    pub fn new(
        bytes: &'a [u8],
        id: &SegmentIdentity,
        limit: ScanLimit,
    ) -> Result<Self, FormatError> {
        Self::resume(
            bytes,
            id,
            limit,
            Resume {
                offset: HEADER_LEN as u64,
                last_sequence: None,
            },
        )
    }

    /// A scan from `from`, typically the [`Scanner::resume_point`] of an earlier scan of the same file with a lower
    /// limit. The header is read again; with `last_sequence: None` its `base_sequence` bounds the first batch. A
    /// limit or resume point off an 8-byte boundary, or a resume point outside the header's end and the limit, is
    /// refused ([`FormatError::ScanPoint`]).
    pub fn resume(
        bytes: &'a [u8],
        id: &SegmentIdentity,
        limit: ScanLimit,
        from: Resume,
    ) -> Result<Self, FormatError> {
        Self::resume_inner(bytes, id, limit, from, true, false).map_err(|(e, _)| e)
    }

    fn resume_inner(
        bytes: &'a [u8],
        id: &SegmentIdentity,
        limit: ScanLimit,
        from: Resume,
        find_seals: bool,
        legacy: bool,
    ) -> Result<Self, (FormatError, ScanStats)> {
        let mut reads = Reads::new(bytes);
        let fail = |e, r: &Reads<'_>| Err((e, r.stats()));
        let header = match read_header(bytes, id) {
            Ok(h) => h,
            Err(e) => return fail(e, &reads),
        };
        if let ScanLimit::Mark(m) = limit
            && !m.is_multiple_of(8)
        {
            return fail(FormatError::ScanPoint { offset: m }, &reads);
        }
        let base = match header {
            HeaderState::Valid(h) => h.base_sequence,
            HeaderState::Invalid => 0,
        };
        let (area_end, seal) = match limit {
            ScanLimit::Mark(m) => (m, None),
            ScanLimit::End => match find_seals.then(|| find_seal(&mut reads)).flatten() {
                Some(s) => (s.start, Some(s)),
                None => (bytes.len() as u64, None),
            },
        };
        // Below the mark and before seal blocks, everything is durable: what is not a complete batch is
        // corruption.
        let bounded = matches!(limit, ScanLimit::Mark(_)) || seal.is_some();
        let start = HEADER_LEN as u64;
        if !from.offset.is_multiple_of(8)
            || from.offset < start
            || from.offset > area_end.max(start)
        {
            return fail(
                FormatError::ScanPoint {
                    offset: from.offset,
                },
                &reads,
            );
        }
        Ok(Self {
            bytes,
            id: *id,
            header,
            seal,
            area_end,
            bounded,
            base,
            pos: from.offset,
            last: from.last_sequence,
            pending: None,
            tail: None,
            done: false,
            reads,
            chains: Chains::default(),
            walks: Walks::default(),
            legacy,
            damaged: false,
            nonzero_end: None,
        })
    }

    /// The work done so far.
    pub fn stats(&self) -> ScanStats {
        self.reads.stats()
    }

    /// The header.
    pub fn header(&self) -> HeaderState {
        self.header
    }

    /// The seal blocks, when read to the end and found.
    pub fn seal(&self) -> Option<&Seal> {
        self.seal.as_ref()
    }

    /// Where a later scan continues: past everything returned so far.
    pub fn resume_point(&self) -> Resume {
        Resume {
            offset: self.pos,
            last_sequence: self.last,
        }
    }

    /// What follows the last batch, once the iterator is exhausted; `None` before that, when read up to the mark
    /// and when sealed.
    pub fn tail(&self) -> Option<&Tail> {
        if self.bounded {
            None
        } else {
            self.tail.as_ref()
        }
    }

    fn finish(&mut self, tail: Tail) {
        self.done = true;
        self.tail = Some(tail);
    }

    /// Just past the last non-zero byte of the area at or above `pos`, or `pos`; found once per scan.
    fn nonzero_end(&mut self, pos: u64) -> u64 {
        let (bytes, area_end) = (self.bytes, self.area_end);
        let end = *self
            .nonzero_end
            .get_or_insert_with(|| last_non_zero_end(bytes, 0, area_end));
        end.max(pos)
    }

    fn keep(&mut self, b: Batch<'a>) -> Found<'a> {
        self.pos = b.end();
        self.last = Some(b.commit.last_sequence);
        Found::Batch(b)
    }
}

impl<'a> Iterator for Scanner<'a> {
    type Item = Result<Found<'a>, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(b) = self.pending.take() {
            return Some(Ok(self.keep(b)));
        }
        if self.done {
            return None;
        }
        let (bytes, pos, area_end) = (self.bytes, self.pos, self.area_end);
        if pos >= area_end {
            self.finish(Tail::Clean { offset: area_end });
            return None;
        }
        let last = self.last;
        let min_first = last.map_or(self.base, |l| l.saturating_add(1));
        self.reads.forget_below(pos);
        let read = if self.legacy || !self.damaged {
            read_batch(
                &mut self.reads,
                &self.id,
                pos,
                area_end,
                min_first,
                Refusal::AtOnce,
            )
        } else {
            self.walks
                .read(&mut self.reads, &self.id, pos, area_end, min_first)
        };
        let fault = match read {
            BatchRead::Complete(b) => return Some(Ok(self.keep(b))),
            BatchRead::Refused(e) => {
                self.done = true;
                return Some(Err(e));
            }
            BatchRead::Incomplete { fault } => fault,
        };
        // Sixteen zero bytes at a block boundary are the end of data only if nothing but zeros follows up to the
        // limit; below the mark or the seal blocks they are never the end (Q-105 (a)).
        if !self.bounded && all_zero(bytes, pos, area_end) {
            self.finish(Tail::Clean { offset: pos });
            return None;
        }
        let cause = match fault {
            BatchFault::Content => LossCause::InvalidContent,
            _ => LossCause::Corruption,
        };
        // One failed direct read per scan at most: a crafted file cannot make the scan leave the bounded path.
        if !self.legacy && !self.damaged {
            self.damaged = true;
            self.reads.use_table();
        }
        // An invalid region starts at `pos` (zeros below the limit included, Q-105 (a)); resynchronise (A.2).
        let end = self.nonzero_end(pos);
        let mut search = (!self.bounded && !self.legacy).then(|| CommitSearch::new(pos, end));
        if self.legacy {
            self.chains = Chains::reference();
        }
        let found = self.chains.resync(
            &mut self.reads,
            &self.id,
            pos,
            area_end,
            min_first,
            search.as_mut(),
        );
        match found {
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
            Ok(Some(b)) => {
                let region = InvalidRegion {
                    start: pos,
                    end: b.offset,
                    last_sequence_before: last,
                    first_sequence_after: Some(b.commit.first_sequence),
                    cause,
                };
                self.pos = b.offset;
                self.pending = Some(b);
                Some(Ok(Found::Region(region)))
            }
            Ok(None) if self.bounded => {
                self.pos = area_end;
                self.finish(Tail::Clean { offset: area_end });
                Some(Ok(Found::Region(InvalidRegion {
                    start: pos,
                    end: area_end,
                    last_sequence_before: last,
                    first_sequence_after: None,
                    cause,
                })))
            }
            Ok(None) => {
                let commits = match search {
                    Some(s) => s.below(end),
                    None => readable_commits(&mut self.reads, pos, end, area_end),
                };
                self.finish(Tail::Torn {
                    start: pos,
                    end,
                    cause,
                    commits,
                });
                None
            }
        }
    }
}

fn read_header(bytes: &[u8], id: &SegmentIdentity) -> Result<HeaderState, FormatError> {
    let Some(h) = FileHeader::decode(bytes) else {
        return Ok(HeaderState::Invalid);
    };
    if h.format_major > crate::FORMAT_MAJOR {
        return Err(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(h.format_major),
        });
    }
    if h.kind != kind::LOG_SEGMENT {
        return Err(FormatError::WrongKind {
            found: h.kind,
            expected: kind::LOG_SEGMENT,
        });
    }
    if h.store_id != id.store_id {
        return Err(FormatError::StoreIdMismatch);
    }
    if h.file_id != id.file_id {
        return Err(FormatError::HeaderMismatch {
            field: "file_id",
            in_header: h.file_id,
            expected: id.file_id,
        });
    }
    if h.generation != id.generation {
        return Err(FormatError::HeaderMismatch {
            field: "generation",
            in_header: u64::from(h.generation),
            expected: u64::from(id.generation),
        });
    }
    Ok(HeaderState::Valid(h))
}

/// The seal blocks, if the last non-zero bytes of the file are a valid trailer block and end marker (A.4, A.9).
fn find_seal(reads: &mut Reads<'_>) -> Option<Seal> {
    let bytes = reads.bytes;
    let len = bytes.len() as u64;
    let last = bytes.iter().rposition(|&b| b != 0)? as u64;
    // The end marker ends in one zero byte and its end is a multiple of 8.
    let end = (last + 1).div_ceil(8) * 8;
    let marker_start = end.checked_sub(END_MARKER_LEN as u64)?;
    let trailer_offset = decode_end_marker(slice(bytes, marker_start, END_MARKER_LEN as u64)?)?;
    if trailer_offset < HEADER_LEN as u64
        || trailer_offset.checked_add((BLOCK_HEADER_LEN + TRAILER_LEN) as u64)? != marker_start
    {
        return None;
    }
    let block = reads.block(trailer_offset, marker_start).ok()?;
    if block.header.block_type != block_type::TRAILER {
        return None;
    }
    let trailer = Trailer::decode(block.body).ok()?;
    let (start, index) = match trailer.index_offset {
        0 => (trailer_offset, None),
        at if at >= HEADER_LEN as u64 && at < trailer_offset && at.is_multiple_of(8) => {
            let index = reads
                .block(at, trailer_offset)
                .ok()
                .filter(|b| b.header.block_type == block_type::SEGMENT_INDEX)
                .and_then(|b| SegmentIndex::decode(b.body).ok());
            (at, Some(index))
        }
        _ => return None,
    };
    let state = if end == len {
        SealState::Sealed
    } else {
        SealState::LengthPending
    };
    Some(Seal {
        start,
        trailer_offset,
        trailer,
        index,
        end,
        state,
    })
}

fn all_zero(bytes: &[u8], from: u64, to: u64) -> bool {
    let to = to.min(bytes.len() as u64);
    from >= to || slice(bytes, from, to - from).is_some_and(|b| b.iter().all(|&x| x == 0))
}

/// Just past the last non-zero byte in `[from, to)`, or `from` if there is none.
fn last_non_zero_end(bytes: &[u8], from: u64, to: u64) -> u64 {
    let to = to.min(bytes.len() as u64);
    if from >= to {
        return from;
    }
    slice(bytes, from, to - from)
        .and_then(|b| b.iter().rposition(|&x| x != 0))
        .map_or(from, |i| from + i as u64 + 1)
}

#[cfg(test)]
mod tests;
