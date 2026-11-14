//! Reading a log segment: complete batches, invalid regions, the tail and the seal (`format.md` A.2, A.4, A.9).

use crate::batch::{Batch, BatchFault, BatchRead, Refusal, SegmentIdentity, read_batch};
use crate::block::{BLOCK_HEADER_LEN, BlockHeader, block_type, read_block};
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

/// Reads the log segment in `bytes`, checking its batches against `id`.
///
/// Returns a version refusal for a header of a newer major version, of another kind or of another store, a valid
/// header that disagrees with `id`, or an unknown critical block in a checksum-valid batch. Damage is never an
/// error: it is reported in [`ScanReport::regions`] and [`ScanReport::tail`].
pub fn scan<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
) -> Result<ScanReport<'a>, FormatError> {
    let header = read_header(bytes, id)?;
    let base = match header {
        HeaderState::Valid(h) => h.base_sequence,
        HeaderState::Invalid => 0,
    };
    let len = bytes.len() as u64;
    let (area_end, seal) = match limit {
        ScanLimit::Mark(m) => (m, None),
        ScanLimit::End => match find_seal(bytes) {
            Some(s) => (s.start, Some(s)),
            None => (len, None),
        },
    };
    // Below the mark and before seal blocks, everything is durable: what is not a complete batch is corruption.
    let bounded = matches!(limit, ScanLimit::Mark(_)) || seal.is_some();
    let mut batches: Vec<Batch<'a>> = Vec::new();
    let mut regions = Vec::new();
    let mut pos = HEADER_LEN as u64;
    let tail = loop {
        if pos >= area_end {
            break Tail::Clean { offset: area_end };
        }
        let last = batches.last().map(|b: &Batch<'_>| b.commit.last_sequence);
        let min_first = last.map_or(base, |l| l.saturating_add(1));
        let fault = match read_batch(bytes, id, pos, area_end, min_first, Refusal::AtOnce) {
            BatchRead::Complete(b) => {
                pos = b.end();
                batches.push(b);
                continue;
            }
            BatchRead::Refused(e) => return Err(e),
            BatchRead::Incomplete { fault } => fault,
        };
        // Sixteen zero bytes at a block boundary are the end of data only if nothing but zeros follows up to the
        // limit; below the mark or the seal blocks they are never the end (Q-105 (a)).
        if !bounded && all_zero(bytes, pos, area_end) {
            break Tail::Clean { offset: pos };
        }
        let cause = match fault {
            BatchFault::Content => LossCause::InvalidContent,
            _ => LossCause::Corruption,
        };
        // An invalid region starts at `pos` (zeros below the limit included, Q-105 (a)); resynchronise (A.2).
        match resync(bytes, id, pos, area_end, min_first)? {
            Some(b) => {
                regions.push(InvalidRegion {
                    start: pos,
                    end: b.offset,
                    last_sequence_before: last,
                    first_sequence_after: Some(b.commit.first_sequence),
                    cause,
                });
                pos = b.end();
                batches.push(b);
            }
            None if bounded => {
                regions.push(InvalidRegion {
                    start: pos,
                    end: area_end,
                    last_sequence_before: last,
                    first_sequence_after: None,
                    cause,
                });
                break Tail::Clean { offset: area_end };
            }
            None => {
                let end = last_non_zero_end(bytes, pos, area_end);
                break Tail::Torn {
                    start: pos,
                    end,
                    cause,
                    commits: readable_commits(bytes, pos, end, area_end),
                };
            }
        }
    };
    let tail = (!bounded).then_some(tail);
    Ok(ScanReport {
        header,
        batches,
        regions,
        tail,
        seal,
    })
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
fn resync<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    start: u64,
    limit: u64,
    min_first: u64,
) -> Result<Option<Batch<'a>>, FormatError> {
    let mut o = start + 8;
    while o + (2 * BLOCK_HEADER_LEN + COMMIT_LEN) as u64 <= limit {
        let Some(head) = slice(bytes, o, BLOCK_HEADER_LEN as u64) else {
            break;
        };
        // A zero word cannot start a block header (type 0 is never valid), and a header that fails its checksum
        // cannot start a batch: both are passed without reading further.
        let head: &[u8; BLOCK_HEADER_LEN] = head.try_into().unwrap_or(&[0; BLOCK_HEADER_LEN]);
        if u64_at(head, 0) == 0 || BlockHeader::decode(head).is_none() {
            o += 8;
            continue;
        }
        match read_batch(bytes, id, o, limit, min_first, Refusal::IfBatchVerifies) {
            BatchRead::Complete(b) => return Ok(Some(b)),
            BatchRead::Refused(e) => return Err(e),
            BatchRead::Incomplete { .. } => o += 8,
        }
    }
    Ok(None)
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
        if let Ok(b) = read_block(bytes, o, limit)
            && b.header.block_type == block_type::COMMIT
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
