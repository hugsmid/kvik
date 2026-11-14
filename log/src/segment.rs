//! The life cycle of a log segment as bytes (`format.md` A.4): created ahead of use and zero-filled, taken into use,
//! appended to, sealed, and zero-filled from a truncation offset. The writer's file operations (roadmap 4.4) apply
//! the same changes through the file-system seam; here they act on an in-memory image, which is enough to produce
//! every state recovery may find.

use crate::batch::{BatchBuilder, SegmentIdentity};
use crate::block::{block_type, encode_block};
use crate::body::{INDEX_HAS_ENTRIES, IndexRecord, SegmentIndex, Trailer, encode_end_marker};
use crate::crc::crc32c;
use crate::error::EncodeError;
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::scan::{HeaderState, ScanReport, SealState};

/// An in-memory image of one log segment and the writer's view of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentImage {
    id: SegmentIdentity,
    header: FileHeader,
    bytes: Vec<u8>,
    /// Where the next batch goes: just past the last batch.
    write_offset: u64,
    summary: Option<Summary>,
    has_entries: bool,
    sealed: bool,
}

/// What a trailer says about the batches written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Summary {
    entry_count: u64,
    first_sequence: u64,
    last_sequence: u64,
    first_commit_time: i64,
    last_commit_time: i64,
    batch_count: u64,
}

impl SegmentImage {
    /// A segment created ahead of use: a v1 header with `base_sequence = 0`, zero-filled to `len` bytes
    /// (`segment_size`, A.4; tests use shorter files). `len` is a multiple of 8 and holds the header.
    pub fn create_ahead(
        id: SegmentIdentity,
        created_at: i64,
        len: u64,
    ) -> Result<Self, EncodeError> {
        if len < HEADER_LEN as u64 || !len.is_multiple_of(8) {
            return Err(EncodeError::Misaligned(len));
        }
        let header = FileHeader::new(
            kind::LOG_SEGMENT,
            id.generation,
            id.file_id,
            created_at,
            id.store_id,
        );
        let mut bytes = vec![0u8; usize::try_from(len).map_err(|_| EncodeError::Misaligned(len))?];
        bytes[..HEADER_LEN].copy_from_slice(&header.encode());
        Ok(Self {
            id,
            header,
            bytes,
            write_offset: HEADER_LEN as u64,
            summary: None,
            has_entries: false,
            sealed: false,
        })
    }

    /// Takes the segment into use: rewrites the header in place with the real `base_sequence` (≥ 1).
    pub fn take_into_use(&mut self, base_sequence: u64) -> Result<(), EncodeError> {
        if base_sequence == 0 || self.header.base_sequence != 0 {
            return Err(EncodeError::State(
                "taken into use once, with a base sequence of at least 1",
            ));
        }
        self.header.base_sequence = base_sequence;
        self.bytes[..HEADER_LEN].copy_from_slice(&self.header.encode());
        Ok(())
    }

    /// Writes `batch` at the write offset and returns its offset. Its first sequence must be at least
    /// `base_sequence` for the first batch, and above the last batch's last sequence after that (a gap is allowed:
    /// a batch after a lost range, or a recovery batch). The batch must fit in the current length.
    pub fn append(&mut self, batch: &BatchBuilder, commit_time: i64) -> Result<u64, EncodeError> {
        if self.sealed || self.header.base_sequence == 0 {
            return Err(EncodeError::State(
                "appending needs a segment in use and not sealed",
            ));
        }
        let (first, last) = batch.sequences()?;
        let min = self
            .summary
            .map_or(self.header.base_sequence, |s| s.last_sequence + 1);
        if first < min {
            return Err(EncodeError::SequenceNotAbove { first, min });
        }
        let at = self.write_offset;
        let out = batch.encode(&self.id, at, commit_time)?;
        let left = self.bytes.len() as u64 - at;
        if out.len() as u64 > left {
            return Err(EncodeError::DoesNotFit {
                needed: out.len() as u64,
                left,
            });
        }
        self.bytes[at as usize..at as usize + out.len()].copy_from_slice(&out);
        self.write_offset += out.len() as u64;
        self.has_entries |= batch.block_types().any(|t| t == block_type::ENTRIES);
        self.summary = Some(match self.summary {
            None => Summary {
                entry_count: last + 1 - first,
                first_sequence: first,
                last_sequence: last,
                first_commit_time: commit_time,
                last_commit_time: commit_time,
                batch_count: 1,
            },
            Some(s) => Summary {
                entry_count: s.entry_count + (last + 1 - first),
                last_sequence: last,
                last_commit_time: commit_time,
                batch_count: s.batch_count + 1,
                ..s
            },
        });
        Ok(at)
    }

    /// Seal step 1 (A.4): at the write offset, writes the segment index block holding `records` (sorted by series
    /// id), the trailer block and the end marker, growing the image if needed. Returns the end of the end marker,
    /// the length step 2 sets. An empty segment is never sealed.
    pub fn write_seal_blocks(&mut self, records: Vec<IndexRecord>) -> Result<u64, EncodeError> {
        let Some(s) = self.summary.filter(|_| !self.sealed) else {
            return Err(EncodeError::State("sealing needs at least one batch, once"));
        };
        let index = SegmentIndex {
            flags: if self.has_entries {
                INDEX_HAS_ENTRIES
            } else {
                0
            },
            records,
        };
        let index_offset = self.write_offset;
        let mut seal = Vec::new();
        encode_block(
            block_type::SEGMENT_INDEX,
            crate::block::FLAG_SKIPPABLE,
            &index.encode(),
            &mut seal,
        )?;
        let trailer_offset = index_offset + seal.len() as u64;
        let at = index_offset as usize;
        let needed = at + seal.len();
        if self.bytes.len() < needed {
            self.bytes.resize(needed, 0);
        }
        self.bytes[at..needed].copy_from_slice(&seal);
        let trailer = Trailer {
            entry_count: s.entry_count,
            first_sequence: s.first_sequence,
            last_sequence: s.last_sequence,
            first_commit_time: s.first_commit_time,
            last_commit_time: s.last_commit_time,
            batch_count: s.batch_count,
            index_offset,
            file_crc32c: crc32c(&self.bytes[..needed]),
        };
        let mut rest = Vec::new();
        encode_block(block_type::TRAILER, 0, &trailer.encode(), &mut rest)?;
        rest.extend_from_slice(&encode_end_marker(trailer_offset));
        let end = needed + rest.len();
        if self.bytes.len() < end {
            self.bytes.resize(end, 0);
        }
        self.bytes[needed..end].copy_from_slice(&rest);
        self.sealed = true;
        Ok(end as u64)
    }

    /// Seal step 2, or any length change: sets the image's length, dropping bytes or adding zeros.
    pub fn set_len(&mut self, len: u64) {
        self.bytes.resize(len as usize, 0);
    }

    /// Truncation is zero-filling (A.4): zeros from `offset` (a multiple of 8, at least the header's end) to the
    /// end. The writer's view moves back to `offset` only if it lay beyond it; batch summaries are not rewound, so
    /// this is for producing recovery states, not for continuing to write.
    pub fn zero_fill_from(&mut self, offset: u64) -> Result<(), EncodeError> {
        if offset < HEADER_LEN as u64 || !offset.is_multiple_of(8) {
            return Err(EncodeError::Misaligned(offset));
        }
        let from = (offset as usize).min(self.bytes.len());
        self.bytes[from..].fill(0);
        self.write_offset = self.write_offset.min(offset);
        Ok(())
    }

    /// The segment's identity.
    pub fn identity(&self) -> SegmentIdentity {
        self.id
    }

    /// Where the next batch goes.
    pub fn write_offset(&self) -> u64 {
        self.write_offset
    }

    /// The image.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The image, for injecting damage in tests.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// The image, by value.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A state recovery may find an `.open` segment in (A.4, "States recovery may find").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpenState {
    /// Its last non-zero bytes are a valid trailer and end marker: finish the seal from step 2; never truncated.
    SealWritten {
        /// The length was not yet set (step 2 not done).
        length_pending: bool,
    },
    /// In use, not the last in use, no valid trailer: seal again from step 1. What follows its last complete batch
    /// is seal blocks or damage, never a torn batch.
    AwaitingSeal,
    /// In use and the last in use: `spec.md` §3 Recovery rules 1 to 3 apply to it.
    LastInUse,
    /// Not in use: kept as the segment created ahead if `keep`, else deleted.
    Unused {
        /// Its header is valid and its length is `segment_size`.
        keep: bool,
    },
}

/// What recovery does with an `.open` segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpenAction {
    /// The header is invalid or has `base_sequence = 0` but a complete batch was found: rewrite it from the file
    /// name, the store descriptor and the first batch before anything else.
    pub rewrite_header: bool,
    /// The state.
    pub state: OpenState,
}

/// Classifies an `.open` segment from its scan to the end ([`crate::scan::ScanLimit::End`]). `last_in_use` says
/// whether it has the highest `file_id` among the segments in use ([`ScanReport::in_use`]); `file_len` and
/// `segment_size` decide whether an unused segment is kept.
pub fn classify_open(
    report: &ScanReport<'_>,
    last_in_use: bool,
    file_len: u64,
    segment_size: u64,
) -> OpenAction {
    if let Some(seal) = &report.seal {
        return OpenAction {
            rewrite_header: false,
            state: OpenState::SealWritten {
                length_pending: seal.state == SealState::LengthPending,
            },
        };
    }
    let header_valid = matches!(report.header, HeaderState::Valid(_));
    if !report.in_use() {
        return OpenAction {
            rewrite_header: false,
            state: OpenState::Unused {
                keep: header_valid && file_len == segment_size,
            },
        };
    }
    let base_known = matches!(report.header, HeaderState::Valid(h) if h.base_sequence >= 1);
    OpenAction {
        rewrite_header: !base_known && !report.batches.is_empty(),
        state: if last_in_use {
            OpenState::LastInUse
        } else {
            OpenState::AwaitingSeal
        },
    }
}
