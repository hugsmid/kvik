//! Reading a log segment: complete batches, invalid regions, the tail and the seal (`format.md` A.2, A.4, A.9).

use std::collections::HashMap;

use crate::batch::{
    Batch, BatchFault, BatchRead, MAX_BATCH_DATA, Refusal, SegmentIdentity, read_batch,
};
use crate::block::{BLOCK_HEADER_LEN, block_type, peek_header, read_block};
use crate::body::{
    COMMIT_LEN, Commit, END_MARKER_LEN, SegmentIndex, TRAILER_LEN, Trailer, decode_end_marker,
};
use crate::bytes::{slice, u64_at};
use crate::crc::crc32c;
use crate::error::{Element, FormatError};
use crate::header::{FileHeader, HEADER_LEN, kind};

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
    let mut s = Scanner::new(bytes, id, limit)?;
    let mut batches = Vec::new();
    let mut regions = Vec::new();
    for found in &mut s {
        match found? {
            Found::Batch(b) => batches.push(b),
            Found::Region(r) => regions.push(r),
        }
    }
    Ok(ScanReport {
        header: s.header,
        tail: s.tail().cloned(),
        seal: s.seal,
        batches,
        regions,
    })
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
    /// limit. The header is read again; with `last_sequence: None` its `base_sequence` bounds the first batch.
    pub fn resume(
        bytes: &'a [u8],
        id: &SegmentIdentity,
        limit: ScanLimit,
        from: Resume,
    ) -> Result<Self, FormatError> {
        let header = read_header(bytes, id)?;
        let base = match header {
            HeaderState::Valid(h) => h.base_sequence,
            HeaderState::Invalid => 0,
        };
        let (area_end, seal) = match limit {
            ScanLimit::Mark(m) => (m, None),
            ScanLimit::End => match find_seal(bytes) {
                Some(s) => (s.start, Some(s)),
                None => (bytes.len() as u64, None),
            },
        };
        // Below the mark and before seal blocks, everything is durable: what is not a complete batch is
        // corruption.
        let bounded = matches!(limit, ScanLimit::Mark(_)) || seal.is_some();
        Ok(Self {
            bytes,
            id: *id,
            header,
            seal,
            area_end,
            bounded,
            base,
            pos: from.offset.max(HEADER_LEN as u64),
            last: from.last_sequence,
            pending: None,
            tail: None,
            done: false,
        })
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
        let fault = match read_batch(bytes, &self.id, pos, area_end, min_first, Refusal::AtOnce) {
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
        // An invalid region starts at `pos` (zeros below the limit included, Q-105 (a)); resynchronise (A.2).
        match resync(bytes, &self.id, pos, area_end, min_first) {
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
                let end = last_non_zero_end(bytes, pos, area_end);
                let commits = readable_commits(bytes, pos, end, area_end);
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

/// Steps forward 8 bytes at a time from an invalid region at `start` and returns the first complete batch (A.2).
///
/// Each chain of consecutive checksum-valid blocks is walked once: the commit block (or fault) it ends in is
/// remembered for every block start on it, and a candidate offset `o` is read in full only when that commit's
/// `batch_bytes` is the distance from `o`. So the blocks read grow linearly with the region, also when it holds a
/// long chain of block images without a commit (for example inside text values).
fn resync<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    start: u64,
    limit: u64,
    min_first: u64,
) -> Result<Option<Batch<'a>>, FormatError> {
    let mut chains: HashMap<u64, Option<(u64, Commit)>> = HashMap::new();
    let mut o = start + 8;
    while o + (2 * BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
        // A zero word cannot start a block header (type 0 is never valid), and a header that fails its checksum
        // cannot start a batch: both are passed without reading further.
        if slice(bytes, o, 8).is_none_or(|w| u64_at(w, 0) == 0)
            || peek_header(bytes, o, limit).is_none()
        {
            o += 8;
            continue;
        }
        let ends_here = chain_end(bytes, o, limit, &mut chains)
            .is_some_and(|(at, c)| at > o && c.batch_bytes == at - o && at - o <= MAX_BATCH_DATA);
        if ends_here {
            match read_batch(bytes, id, o, limit, min_first, Refusal::IfBatchVerifies) {
                BatchRead::Complete(b) => return Ok(Some(b)),
                BatchRead::Refused(e) => return Err(e),
                BatchRead::Incomplete { .. } => {}
            }
        }
        o += 8;
    }
    Ok(None)
}

/// The commit block that the chain of checksum-valid blocks from `o` ends in, with its offset, or `None` if the
/// chain ends in anything else. Every block start walked is remembered in `chains`.
fn chain_end(
    bytes: &[u8],
    o: u64,
    limit: u64,
    chains: &mut HashMap<u64, Option<(u64, Commit)>>,
) -> Option<(u64, Commit)> {
    let mut walked = Vec::new();
    let mut p = o;
    let end = loop {
        if let Some(known) = chains.get(&p) {
            break *known;
        }
        let Ok(b) = read_block(bytes, p, limit) else {
            break None;
        };
        walked.push(p);
        match b.header.block_type {
            block_type::COMMIT => break Commit::decode(b.body).ok().map(|c| (p, c)),
            block_type::SEGMENT_INDEX | block_type::TRAILER => break None,
            _ => p = b.next_offset(),
        }
    };
    for w in walked {
        chains.insert(w, end);
    }
    end
}

/// The seal blocks, if the last non-zero bytes of the file are a valid trailer block and end marker (A.4, A.9).
fn find_seal(bytes: &[u8]) -> Option<Seal> {
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
    let block = read_block(bytes, trailer_offset, marker_start).ok()?;
    if block.header.block_type != block_type::TRAILER {
        return None;
    }
    let trailer = Trailer::decode(block.body).ok()?;
    let (start, index) = match trailer.index_offset {
        0 => (trailer_offset, None),
        at if at >= HEADER_LEN as u64 && at < trailer_offset && at.is_multiple_of(8) => {
            let index = read_block(bytes, at, trailer_offset)
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

/// Checksum-valid commit blocks starting at 8-byte steps in `[from, to)` and ending below `limit`.
fn readable_commits(bytes: &[u8], from: u64, to: u64, limit: u64) -> Vec<ReadableCommit> {
    let mut out = Vec::new();
    let mut o = from;
    while o < to && o + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
        // Type and length are tested on the header before the body checksum is computed.
        if peek_header(bytes, o, limit).is_some_and(|h| {
            h.block_type == block_type::COMMIT && h.body_len as usize == COMMIT_LEN
        }) && let Ok(b) = read_block(bytes, o, limit)
            && let Ok(commit) = Commit::decode(b.body)
        {
            out.push(ReadableCommit { offset: o, commit });
            o = b.next_offset();
            continue;
        }
        o += 8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BLOCK_READS, FLAG_SKIPPABLE, encode_block};

    /// A segment holding `n` checksum-valid header-only blocks and no commit block: a torn batch whose body
    /// holds block images, as crafted text values could.
    fn chain(n: usize) -> (Vec<u8>, SegmentIdentity) {
        let id = SegmentIdentity {
            store_id: [3; 16],
            file_id: 1,
            generation: 0,
        };
        let mut h = FileHeader::new(kind::LOG_SEGMENT, 0, 1, 0, id.store_id);
        h.base_sequence = 1;
        let mut b = h.encode().to_vec();
        for _ in 0..n {
            encode_block(0x50, FLAG_SKIPPABLE, &[], &mut b).unwrap();
        }
        b.resize(b.len() + 256, 0);
        (b, id)
    }

    #[test]
    fn resynchronising_over_a_chain_of_block_images_is_linear() {
        for n in [1_000, 2_000, 4_000, 16_000] {
            let (bytes, id) = chain(n);
            BLOCK_READS.with(|c| c.set(0));
            let r = scan(&bytes, &id, ScanLimit::End).unwrap();
            let reads = BLOCK_READS.with(|c| c.get());
            assert!(r.batches.is_empty());
            assert!(matches!(r.tail, Some(Tail::Torn { start: 64, .. })));
            assert!(
                reads <= 3 * n as u64 + 64,
                "{reads} block reads for {n} blocks"
            );
        }
    }

    #[test]
    fn a_resumed_scan_continues_where_the_last_stopped() {
        let (mut bytes, id) = chain(0);
        bytes.truncate(64);
        let mut builder = crate::batch::BatchBuilder::new();
        let mut body = vec![0u8; 24];
        crate::body::DataPrefix {
            first_sequence: 1,
            received_at: 0,
            count: 1,
        }
        .write(&mut body);
        builder.push(block_type::ENTRIES, body.clone()).unwrap();
        let first = builder.encode(&id, 64, 0).unwrap();
        bytes.extend_from_slice(&first);
        let at = bytes.len() as u64;
        let mut builder = crate::batch::BatchBuilder::new();
        crate::body::DataPrefix {
            first_sequence: 2,
            received_at: 0,
            count: 1,
        }
        .write(&mut body);
        builder.push(block_type::ENTRIES, body).unwrap();
        bytes.extend_from_slice(&builder.encode(&id, at, 0).unwrap());
        let mut s = Scanner::new(&bytes, &id, ScanLimit::Mark(at)).unwrap();
        assert!(matches!(s.next(), Some(Ok(Found::Batch(_)))));
        assert!(s.next().is_none());
        let point = s.resume_point();
        assert_eq!(
            point,
            Resume {
                offset: at,
                last_sequence: Some(1)
            }
        );
        let mut s =
            Scanner::resume(&bytes, &id, ScanLimit::Mark(bytes.len() as u64), point).unwrap();
        assert!(
            matches!(s.next(), Some(Ok(Found::Batch(b))) if b.offset == at && b.commit.first_sequence == 2)
        );
        assert!(s.next().is_none());
        // A resumed scan still requires the sequence to rise.
        let mut s = Scanner::resume(
            &bytes,
            &id,
            ScanLimit::Mark(bytes.len() as u64),
            Resume {
                offset: at,
                last_sequence: Some(2),
            },
        )
        .unwrap();
        assert!(matches!(s.next(), Some(Ok(Found::Region(_)))));
    }
}
