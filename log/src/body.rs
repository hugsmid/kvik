//! Bodies the segment container itself reads: the prefix of a data block, the commit block, the segment index, the
//! trailer and the end marker (`format.md` A.5 to A.9).
//!
//! The columns of a facts body and the entries of an entries body are read by the batch layer (roadmap 4.3); the
//! container needs only their common 24-byte prefix, to count sequences and compare `received_at` (A.7).

use crate::block::block_type;
use crate::bytes::{i64_at, put, u16_at, u32_at, u64_at};

/// Length of the prefix every facts and entries body starts with.
pub const DATA_PREFIX_LEN: usize = 24;
/// The most rows in one facts block.
pub const MAX_ROWS: u32 = 65_536;

/// The common prefix of a facts body (A.5) and an entries body (A.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DataPrefix {
    /// Sequence of the block's first entry.
    pub first_sequence: u64,
    /// The batch's receive time; every data block of a batch carries the same value.
    pub received_at: i64,
    /// `row_count` of a facts block, `entry_count` of an entries block: how many sequences the block takes.
    pub count: u32,
}

/// Why a checksum-valid block's body does not pass the checks of its type (`format.md` A.2: invalid content).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentFault;

impl DataPrefix {
    /// Reads the prefix of a facts or entries body and applies the checks the container is responsible for: the
    /// body holds the prefix, and a facts block has 1 to 65,536 rows. The checks of the columns and entries
    /// themselves (A.5, A.6) belong to the batch layer.
    pub fn decode(block_type: u8, body: &[u8]) -> Result<Self, ContentFault> {
        if body.len() < DATA_PREFIX_LEN {
            return Err(ContentFault);
        }
        let p = Self {
            first_sequence: u64_at(body, 0),
            received_at: i64_at(body, 8),
            count: u32_at(body, 16),
        };
        match block_type {
            block_type::FACTS if (1..=MAX_ROWS).contains(&p.count) => Ok(p),
            block_type::ENTRIES => Ok(p),
            _ => Err(ContentFault),
        }
    }

    /// Writes the prefix into the first 20 bytes of `body`; bytes 20 to 24 belong to the body's type.
    pub fn write(&self, body: &mut [u8]) {
        put(body, 0, &self.first_sequence.to_le_bytes());
        put(body, 8, &self.received_at.to_le_bytes());
        put(body, 16, &self.count.to_le_bytes());
    }
}

/// Length of a commit block body.
pub const COMMIT_LEN: usize = 48;

/// A commit block body (`format.md` A.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Commit {
    /// Equals the file header's.
    pub generation: u32,
    /// Data blocks in the batch.
    pub block_count: u32,
    /// The first data block's first sequence.
    pub first_sequence: u64,
    /// The last sequence counted, inclusive.
    pub last_sequence: u64,
    /// Commit time; not tested by a reader.
    pub commit_time: i64,
    /// Bytes from the batch's first block header to this block's header, padding included.
    pub batch_bytes: u64,
    /// CRC32C over `store_id`, `file_id`, the batch offset and the data block headers.
    pub batch_crc32c: u32,
}

impl Commit {
    /// The 48 bytes of this body.
    pub fn encode(&self) -> [u8; COMMIT_LEN] {
        let mut b = [0u8; COMMIT_LEN];
        put(&mut b, 0, &self.generation.to_le_bytes());
        put(&mut b, 4, &self.block_count.to_le_bytes());
        put(&mut b, 8, &self.first_sequence.to_le_bytes());
        put(&mut b, 16, &self.last_sequence.to_le_bytes());
        put(&mut b, 24, &self.commit_time.to_le_bytes());
        put(&mut b, 32, &self.batch_bytes.to_le_bytes());
        put(&mut b, 40, &self.batch_crc32c.to_le_bytes());
        b
    }

    /// The commit body in `body`, which must be exactly 48 bytes.
    pub fn decode(body: &[u8]) -> Result<Self, ContentFault> {
        if body.len() != COMMIT_LEN {
            return Err(ContentFault);
        }
        Ok(Self {
            generation: u32_at(body, 0),
            block_count: u32_at(body, 4),
            first_sequence: u64_at(body, 8),
            last_sequence: u64_at(body, 16),
            commit_time: i64_at(body, 24),
            batch_bytes: u64_at(body, 32),
            batch_crc32c: u32_at(body, 40),
        })
    }
}

/// Segment index flag bit 0: the segment holds at least one entries block.
pub const INDEX_HAS_ENTRIES: u16 = 1;
/// Size of a segment index record in v1.
pub const INDEX_RECORD_LEN: u16 = 24;

/// One series in a segment index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexRecord {
    /// The series.
    pub series_id: u32,
    /// Facts of the series in the segment.
    pub fact_count: u32,
    /// The least `t`.
    pub min_t: i64,
    /// The greatest reach of a reading (A.8).
    pub max_t: i64,
}

/// A segment index body (`format.md` A.8).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SegmentIndex {
    /// Flags ([`INDEX_HAS_ENTRIES`]).
    pub flags: u16,
    /// Records, sorted by series id.
    pub records: Vec<IndexRecord>,
}

impl SegmentIndex {
    /// The body, with v1 records of 24 bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + self.records.len() * usize::from(INDEX_RECORD_LEN));
        b.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        b.extend_from_slice(&INDEX_RECORD_LEN.to_le_bytes());
        b.extend_from_slice(&self.flags.to_le_bytes());
        for r in &self.records {
            b.extend_from_slice(&r.series_id.to_le_bytes());
            b.extend_from_slice(&r.fact_count.to_le_bytes());
            b.extend_from_slice(&r.min_t.to_le_bytes());
            b.extend_from_slice(&r.max_t.to_le_bytes());
        }
        b
    }

    /// The index in `body`. Records of more than 24 bytes have their tail ignored (A.8); the body must hold
    /// exactly `series_count` records.
    pub fn decode(body: &[u8]) -> Result<Self, ContentFault> {
        if body.len() < 8 {
            return Err(ContentFault);
        }
        let count = u64::from(u32_at(body, 0));
        let size = u64::from(u16_at(body, 4));
        if size < u64::from(INDEX_RECORD_LEN) || 8 + count * size != body.len() as u64 {
            return Err(ContentFault);
        }
        let size = size as usize;
        let records = body[8..]
            .chunks_exact(size)
            .map(|r| IndexRecord {
                series_id: u32_at(r, 0),
                fact_count: u32_at(r, 4),
                min_t: i64_at(r, 8),
                max_t: i64_at(r, 16),
            })
            .collect();
        Ok(Self {
            flags: u16_at(body, 6),
            records,
        })
    }
}

/// Length of a trailer body.
pub const TRAILER_LEN: usize = 64;

/// A trailer body (`format.md` A.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Trailer {
    /// Entries in the segment.
    pub entry_count: u64,
    /// The first batch's first sequence.
    pub first_sequence: u64,
    /// The last batch's last sequence.
    pub last_sequence: u64,
    /// The first batch's commit time.
    pub first_commit_time: i64,
    /// The last batch's commit time.
    pub last_commit_time: i64,
    /// Complete batches in the segment.
    pub batch_count: u64,
    /// File offset of the segment index block header; 0 if there is none.
    pub index_offset: u64,
    /// CRC32C of every byte from offset 0 to the start of the trailer's block header.
    pub file_crc32c: u32,
}

impl Trailer {
    /// The 64 bytes of this body.
    pub fn encode(&self) -> [u8; TRAILER_LEN] {
        let mut b = [0u8; TRAILER_LEN];
        put(&mut b, 0, &self.entry_count.to_le_bytes());
        put(&mut b, 8, &self.first_sequence.to_le_bytes());
        put(&mut b, 16, &self.last_sequence.to_le_bytes());
        put(&mut b, 24, &self.first_commit_time.to_le_bytes());
        put(&mut b, 32, &self.last_commit_time.to_le_bytes());
        put(&mut b, 40, &self.batch_count.to_le_bytes());
        put(&mut b, 48, &self.index_offset.to_le_bytes());
        put(&mut b, 56, &self.file_crc32c.to_le_bytes());
        b
    }

    /// The trailer in `body`, which must be exactly 64 bytes.
    pub fn decode(body: &[u8]) -> Result<Self, ContentFault> {
        if body.len() != TRAILER_LEN {
            return Err(ContentFault);
        }
        Ok(Self {
            entry_count: u64_at(body, 0),
            first_sequence: u64_at(body, 8),
            last_sequence: u64_at(body, 16),
            first_commit_time: i64_at(body, 24),
            last_commit_time: i64_at(body, 32),
            batch_count: u64_at(body, 40),
            index_offset: u64_at(body, 48),
            file_crc32c: u32_at(body, 56),
        })
    }
}

/// Length of the end marker of a sealed segment.
pub const END_MARKER_LEN: usize = 16;
/// The last 8 bytes of a sealed segment.
pub const END_MAGIC: [u8; 8] = *b"KVIKEND\0";

/// The end marker pointing at the trailer block header at `trailer_offset`.
pub fn encode_end_marker(trailer_offset: u64) -> [u8; END_MARKER_LEN] {
    let mut b = [0u8; END_MARKER_LEN];
    put(&mut b, 0, &trailer_offset.to_le_bytes());
    put(&mut b, 8, &END_MAGIC);
    b
}

/// The `trailer_offset` of an end marker, or `None` if `b` is not one.
pub fn decode_end_marker(b: &[u8]) -> Option<u64> {
    (b.len() == END_MARKER_LEN && b[8..] == END_MAGIC).then(|| u64_at(b, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_layout() {
        let c = Commit {
            generation: 2,
            block_count: 3,
            first_sequence: 10,
            last_sequence: 19,
            commit_time: 77,
            batch_bytes: 512,
            batch_crc32c: 0xDEAD_BEEF,
        };
        let b = c.encode();
        assert_eq!(&b[32..40], &512u64.to_le_bytes());
        assert_eq!(&b[40..44], &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(&b[44..48], &[0; 4]);
        assert_eq!(Commit::decode(&b), Ok(c));
        assert_eq!(Commit::decode(&b[..47]), Err(ContentFault));
    }

    #[test]
    fn data_prefix_checks() {
        let mut body = [0u8; 24];
        DataPrefix {
            first_sequence: 1,
            received_at: 5,
            count: 0,
        }
        .write(&mut body);
        assert_eq!(
            DataPrefix::decode(block_type::FACTS, &body),
            Err(ContentFault)
        );
        assert!(DataPrefix::decode(block_type::ENTRIES, &body).is_ok());
        DataPrefix {
            first_sequence: 1,
            received_at: 5,
            count: MAX_ROWS + 1,
        }
        .write(&mut body);
        assert_eq!(
            DataPrefix::decode(block_type::FACTS, &body),
            Err(ContentFault)
        );
        DataPrefix {
            first_sequence: 1,
            received_at: 5,
            count: MAX_ROWS,
        }
        .write(&mut body);
        assert!(DataPrefix::decode(block_type::FACTS, &body).is_ok());
        assert_eq!(
            DataPrefix::decode(block_type::FACTS, &body[..23]),
            Err(ContentFault)
        );
        assert_eq!(
            DataPrefix::decode(block_type::COMMIT, &body),
            Err(ContentFault)
        );
    }

    #[test]
    fn index_with_longer_records() {
        let idx = SegmentIndex {
            flags: INDEX_HAS_ENTRIES,
            records: vec![IndexRecord {
                series_id: 1,
                fact_count: 2,
                min_t: 3,
                max_t: 4,
            }],
        };
        let b = idx.encode();
        assert_eq!(b.len(), 32);
        assert_eq!(SegmentIndex::decode(&b), Ok(idx.clone()));
        // A later minor version's 32-byte records: the tail is ignored.
        let mut long = b[..8].to_vec();
        long[4..6].copy_from_slice(&32u16.to_le_bytes());
        long.extend_from_slice(&b[8..]);
        long.extend_from_slice(&[9; 8]);
        assert_eq!(SegmentIndex::decode(&long), Ok(idx));
        assert_eq!(SegmentIndex::decode(&b[..31]), Err(ContentFault));
        let mut short = b.clone();
        short[4..6].copy_from_slice(&23u16.to_le_bytes());
        assert_eq!(SegmentIndex::decode(&short), Err(ContentFault));
    }

    #[test]
    fn end_marker() {
        let m = encode_end_marker(1234);
        assert_eq!(&m[8..], b"KVIKEND\0");
        assert_eq!(decode_end_marker(&m), Some(1234));
        assert_eq!(decode_end_marker(&m[1..]), None);
    }
}
