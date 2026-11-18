//! The fixed-layout bodies of a compacted segment (`docs/format-compacted.md` B.1): chunk directory, chunk index,
//! index root, commit-time directory, generation marker and trailer; and the chunk index object's own (B.2).

use super::error::{Bad, FileElement};
use super::stats::total_order;
use super::{INDEX_BLOCK_ENTRIES, MAX_CHUNK_BLOCKS, Position};

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or_default())
}
pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or_default())
}
pub(crate) fn i64_at(b: &[u8], at: usize) -> i64 {
    u64_at(b, at) as i64
}
pub(crate) fn position_at(b: &[u8], at: usize) -> Position {
    (u32_at(b, at), u64_at(b, at + 4))
}
pub(crate) fn put_position(out: &mut Vec<u8>, p: Position) {
    out.extend_from_slice(&p.0.to_le_bytes());
    out.extend_from_slice(&p.1.to_le_bytes());
}

/// Reads a record array's header and checks `body_len = head + record_size × count`, `record_size ≥ min`.
fn records(body: &[u8], head: usize, size: usize, count: usize, min: usize) -> Result<(), Bad> {
    if size < min
        || size
            .checked_mul(count)
            .and_then(|n| n.checked_add(head))
            .is_none_or(|n| n != body.len())
    {
        return Err(Bad::body("record array length"));
    }
    Ok(())
}

/// Chunk flag bit 2: the chunk holds facts set aside by a restore.
pub const SET_ASIDE: u16 = 1 << 2;
/// Chunk flag bit 3: the directory carries a summary.
pub const HAS_SUMMARY: u16 = 1 << 3;
/// Index flag bit 0: the entry carries value bounds.
pub const HAS_BOUNDS: u16 = 1;
/// Index flag bit 1: a value word is a NaN.
pub const HAS_NAN: u16 = 1 << 1;
const DIRECTORY_DEFINED: u16 = SET_ASIDE | HAS_SUMMARY;
const INDEX_DEFINED: u16 = HAS_BOUNDS | HAS_NAN | SET_ASIDE | HAS_SUMMARY;

/// One data block of a chunk, as its directory records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockRecord {
    /// `t` of the block's first row.
    pub first_t: i64,
    /// `t` of the block's last row.
    pub last_t: i64,
    /// Offset of the block's header from the chunk's first byte.
    pub offset: u32,
    /// The data block's `body_len`.
    pub body_len: u32,
    /// Rows in the block.
    pub row_count: u16,
    /// The data block's `body_crc32c`.
    pub body_crc32c: u32,
}

/// A chunk directory (`0x31`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Directory {
    /// The series.
    pub series_id: u32,
    /// Bit 2 set aside, bit 3 summary.
    pub flags: u16,
    /// Rows in the chunk.
    pub row_count: u32,
    /// One record per data block.
    pub blocks: Vec<BlockRecord>,
    /// The summary bytes (empty in a v1 file).
    pub summary: Vec<u8>,
}

const DIRECTORY_HEAD: usize = 24;
const BLOCK_RECORD: usize = 32;

impl Directory {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(DIRECTORY_HEAD + BLOCK_RECORD * self.blocks.len());
        b.extend_from_slice(&self.series_id.to_le_bytes());
        b.extend_from_slice(&self.flags.to_le_bytes());
        b.extend_from_slice(&(self.blocks.len() as u16).to_le_bytes());
        b.extend_from_slice(&self.row_count.to_le_bytes());
        b.extend_from_slice(&(self.summary.len() as u32).to_le_bytes());
        b.extend_from_slice(&(BLOCK_RECORD as u16).to_le_bytes());
        b.extend_from_slice(&[0; 6]);
        for r in &self.blocks {
            b.extend_from_slice(&r.first_t.to_le_bytes());
            b.extend_from_slice(&r.last_t.to_le_bytes());
            b.extend_from_slice(&r.offset.to_le_bytes());
            b.extend_from_slice(&r.body_len.to_le_bytes());
            b.extend_from_slice(&r.row_count.to_le_bytes());
            b.extend_from_slice(&[0; 2]);
            b.extend_from_slice(&r.body_crc32c.to_le_bytes());
        }
        b.extend_from_slice(&self.summary);
        b
    }

    /// Decodes a body: undefined flag bits refused; 1 to 64 records of at least 32 bytes; a summary only with
    /// bit 3; the records' row counts 1 to 4,096 summing to `row_count`.
    pub(crate) fn decode(body: &[u8]) -> Result<Self, Bad> {
        if body.len() < DIRECTORY_HEAD {
            return Err(Bad::body("chunk directory"));
        }
        let flags = u16_at(body, 4);
        if flags & !DIRECTORY_DEFINED != 0 {
            return Err(Bad::Unsupported(FileElement::DirectoryFlags(
                flags & !DIRECTORY_DEFINED,
            )));
        }
        let count = usize::from(u16_at(body, 6));
        let summary_len = u32_at(body, 12) as usize;
        let size = usize::from(u16_at(body, 16));
        if !(1..=MAX_CHUNK_BLOCKS).contains(&count) || (summary_len > 0 && flags & HAS_SUMMARY == 0)
        {
            return Err(Bad::body("chunk directory"));
        }
        let records_len = body
            .len()
            .checked_sub(summary_len)
            .ok_or(Bad::body("chunk directory"))?;
        records(
            &body[..records_len],
            DIRECTORY_HEAD,
            size,
            count,
            BLOCK_RECORD,
        )?;
        let blocks: Vec<BlockRecord> = (0..count)
            .map(|i| {
                let r = &body[DIRECTORY_HEAD + size * i..];
                BlockRecord {
                    first_t: i64_at(r, 0),
                    last_t: i64_at(r, 8),
                    offset: u32_at(r, 16),
                    body_len: u32_at(r, 20),
                    row_count: u16_at(r, 24),
                    body_crc32c: u32_at(r, 28),
                }
            })
            .collect();
        let row_count = u32_at(body, 8);
        if blocks
            .iter()
            .any(|r| !(1..=crate::MAX_ROWS as u16).contains(&r.row_count) || r.first_t > r.last_t)
            || blocks.iter().map(|r| u64::from(r.row_count)).sum::<u64>() != u64::from(row_count)
        {
            return Err(Bad::body("chunk directory records"));
        }
        Ok(Self {
            series_id: u32_at(body, 0),
            flags,
            row_count,
            blocks,
            summary: body[records_len..].to_vec(),
        })
    }
}

/// Value bounds as stored: the type (1–4, A.5), the decimal scale and the two words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub(crate) struct RawBounds {
    pub(crate) value_type: u8,
    pub(crate) scale: u8,
    pub(crate) min: u64,
    pub(crate) max: u64,
}

/// A chunk index entry (96 bytes in v1): a bound on `t`, position and value for one chunk, and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexEntry {
    /// The series.
    pub series_id: u32,
    /// Bit 0 bounds, bit 1 NaN, bit 2 set aside, bit 3 summary.
    pub flags: u16,
    /// The bounds' value type, 0 without bounds.
    pub value_type: u8,
    /// The decimal bounds' scale, else 0.
    pub scale: u8,
    /// The least `t`.
    pub min_t: i64,
    /// The greatest `t`.
    pub max_t: i64,
    /// The greatest reach of a row (`format.md` A.8).
    pub reach: i64,
    /// The least position.
    pub first: Position,
    /// The greatest position.
    pub last: Position,
    /// File offset of the chunk's first block header.
    pub chunk_offset: u64,
    /// Bytes of the chunk through the end of its directory block, padding included.
    pub chunk_len: u32,
    /// Bytes of the directory block, the chunk's last.
    pub dir_len: u32,
    /// Rows in the chunk.
    pub row_count: u32,
    /// The directory block's `body_crc32c`.
    pub dir_crc32c: u32,
    /// The least value word.
    pub min_value: u64,
    /// The greatest value word.
    pub max_value: u64,
}

/// Bytes of an index entry in v1.
pub(crate) const ENTRY_LEN: usize = 96;
const INDEX_HEAD: usize = 8;

impl IndexEntry {
    pub(crate) fn encode_into(&self, b: &mut Vec<u8>) {
        b.extend_from_slice(&self.series_id.to_le_bytes());
        b.extend_from_slice(&self.flags.to_le_bytes());
        b.push(self.value_type);
        b.push(self.scale);
        b.extend_from_slice(&self.min_t.to_le_bytes());
        b.extend_from_slice(&self.max_t.to_le_bytes());
        b.extend_from_slice(&self.reach.to_le_bytes());
        b.extend_from_slice(&self.first.0.to_le_bytes());
        b.extend_from_slice(&self.last.0.to_le_bytes());
        b.extend_from_slice(&self.first.1.to_le_bytes());
        b.extend_from_slice(&self.last.1.to_le_bytes());
        b.extend_from_slice(&self.chunk_offset.to_le_bytes());
        b.extend_from_slice(&self.chunk_len.to_le_bytes());
        b.extend_from_slice(&self.dir_len.to_le_bytes());
        b.extend_from_slice(&self.row_count.to_le_bytes());
        b.extend_from_slice(&self.dir_crc32c.to_le_bytes());
        b.extend_from_slice(&self.min_value.to_le_bytes());
        b.extend_from_slice(&self.max_value.to_le_bytes());
    }

    fn decode(r: &[u8]) -> Result<Self, Bad> {
        let flags = u16_at(r, 4);
        if flags & !INDEX_DEFINED != 0 {
            return Err(Bad::Unsupported(FileElement::IndexFlags(
                flags & !INDEX_DEFINED,
            )));
        }
        let e = Self {
            series_id: u32_at(r, 0),
            flags,
            value_type: r[6],
            scale: r[7],
            min_t: i64_at(r, 8),
            max_t: i64_at(r, 16),
            reach: i64_at(r, 24),
            first: (u32_at(r, 32), u64_at(r, 40)),
            last: (u32_at(r, 36), u64_at(r, 48)),
            chunk_offset: u64_at(r, 56),
            chunk_len: u32_at(r, 64),
            dir_len: u32_at(r, 68),
            row_count: u32_at(r, 72),
            dir_crc32c: u32_at(r, 76),
            min_value: u64_at(r, 80),
            max_value: u64_at(r, 88),
        };
        let bounded = flags & HAS_BOUNDS != 0;
        let ordered = match e.value_type {
            1 => total_order(e.min_value) <= total_order(e.max_value),
            2 => (e.min_value as i64) <= (e.max_value as i64),
            _ => e.min_value <= e.max_value,
        };
        if e.min_t > e.max_t
            || e.reach < e.max_t
            || (flags & HAS_NAN != 0 && e.value_type > 1)
            || !ordered
            || e.first > e.last
            || e.row_count == 0
            || bounded != (e.value_type != 0)
            || e.value_type > 4
            || (e.value_type != 2 && e.scale != 0)
            || e.scale > 18
            || e.dir_len >= e.chunk_len
        {
            return Err(Bad::body("chunk index entry"));
        }
        Ok(e)
    }

    /// The flags that the chunk's directory repeats.
    pub(crate) fn chunk_flags(&self) -> u16 {
        self.flags & DIRECTORY_DEFINED
    }

    pub(crate) fn raw_bounds(&self) -> Option<RawBounds> {
        (self.flags & HAS_BOUNDS != 0).then_some(RawBounds {
            value_type: self.value_type,
            scale: self.scale,
            min: self.min_value,
            max: self.max_value,
        })
    }

    /// Whether the chunk holds facts set aside by a restore.
    pub fn set_aside(&self) -> bool {
        self.flags & SET_ASIDE != 0
    }

    /// Whether a value word of the chunk is a NaN: a row that satisfies no ordered comparison exists.
    pub fn has_nan(&self) -> bool {
        self.flags & HAS_NAN != 0
    }
}

pub(crate) fn encode_index_block(entries: &[IndexEntry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(INDEX_HEAD + ENTRY_LEN * entries.len());
    b.extend_from_slice(&(ENTRY_LEN as u16).to_le_bytes());
    b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    for e in entries {
        e.encode_into(&mut b);
    }
    b
}

/// Decodes an index block: 1 to 1,024 records of at least 96 bytes, sorted by series, `min_t` and first position.
pub(crate) fn decode_index_block(body: &[u8]) -> Result<Vec<IndexEntry>, Bad> {
    if body.len() < INDEX_HEAD {
        return Err(Bad::body("chunk index block"));
    }
    let size = usize::from(u16_at(body, 0));
    let count = usize::from(u16_at(body, 2));
    if !(1..=INDEX_BLOCK_ENTRIES).contains(&count) {
        return Err(Bad::body("chunk index block"));
    }
    records(body, INDEX_HEAD, size, count, ENTRY_LEN)?;
    let entries = (0..count)
        .map(|i| IndexEntry::decode(&body[INDEX_HEAD + size * i..]))
        .collect::<Result<Vec<_>, _>>()?;
    if !entries
        .windows(2)
        .all(|w| sort_key(&w[0]) < sort_key(&w[1]))
    {
        return Err(Bad::body("chunk index order"));
    }
    Ok(entries)
}

/// The order of the index: series, then `min_t`, then first position.
pub(crate) fn sort_key(e: &IndexEntry) -> (u32, i64, Position) {
    (e.series_id, e.min_t, e.first)
}

/// One index block, as the root records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootRecord {
    /// Series of the block's first entry.
    pub first_series_id: u32,
    /// Series of the block's last entry.
    pub last_series_id: u32,
    /// `min_t` of the block's first entry.
    pub first_min_t: i64,
    /// Offset of the block's header from the first index block's header.
    pub offset: u64,
    /// Entries in the block.
    pub entry_count: u32,
    /// The index block's `body_crc32c`.
    pub body_crc32c: u32,
}

const ROOT_HEAD: usize = 8;
const ROOT_RECORD: usize = 32;

pub(crate) fn encode_root(records: &[RootRecord]) -> Vec<u8> {
    let mut b = Vec::with_capacity(ROOT_HEAD + ROOT_RECORD * records.len());
    b.extend_from_slice(&(ROOT_RECORD as u16).to_le_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for r in records {
        b.extend_from_slice(&r.first_series_id.to_le_bytes());
        b.extend_from_slice(&r.last_series_id.to_le_bytes());
        b.extend_from_slice(&r.first_min_t.to_le_bytes());
        b.extend_from_slice(&r.offset.to_le_bytes());
        b.extend_from_slice(&r.entry_count.to_le_bytes());
        b.extend_from_slice(&r.body_crc32c.to_le_bytes());
    }
    b
}

/// Decodes the root: records of at least 32 bytes, each naming 1 to 1,024 entries, in index order with rising
/// offsets.
pub(crate) fn decode_root(body: &[u8]) -> Result<Vec<RootRecord>, Bad> {
    if body.len() < ROOT_HEAD {
        return Err(Bad::body("index root"));
    }
    let size = usize::from(u16_at(body, 0));
    let count = u32_at(body, 4) as usize;
    records(body, ROOT_HEAD, size, count, ROOT_RECORD)?;
    let recs: Vec<RootRecord> = (0..count)
        .map(|i| {
            let r = &body[ROOT_HEAD + size * i..];
            RootRecord {
                first_series_id: u32_at(r, 0),
                last_series_id: u32_at(r, 4),
                first_min_t: i64_at(r, 8),
                offset: u64_at(r, 16),
                entry_count: u32_at(r, 24),
                body_crc32c: u32_at(r, 28),
            }
        })
        .collect();
    let ok = recs.iter().all(|r| {
        (1..=INDEX_BLOCK_ENTRIES as u32).contains(&r.entry_count)
            && r.first_series_id <= r.last_series_id
    }) && recs.windows(2).all(|w| {
        (w[0].last_series_id, w[0].offset) <= (w[1].first_series_id, w[1].offset)
            && w[0].offset < w[1].offset
    });
    if !ok {
        return Err(Bad::body("index root records"));
    }
    Ok(recs)
}

/// One commit-times block, as the commit-time directory records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimesRecord {
    /// The generation of the block's rows.
    pub generation: u32,
    /// The block's `body_crc32c`.
    pub body_crc32c: u32,
    /// The first row's `first_sequence`.
    pub first_sequence: u64,
    /// The last row's last sequence.
    pub last_sequence: u64,
    /// The first row's commit time.
    pub first_commit_time: i64,
    /// The last row's commit time.
    pub last_commit_time: i64,
    /// File offset of the block's header in the compacted segment.
    pub offset: u64,
}

const TIMES_DIR_HEAD: usize = 8;
const TIMES_RECORD: usize = 48;

pub(crate) fn encode_times_directory(records: &[TimesRecord]) -> Vec<u8> {
    let mut b = Vec::with_capacity(TIMES_DIR_HEAD + TIMES_RECORD * records.len());
    b.extend_from_slice(&(TIMES_RECORD as u16).to_le_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for r in records {
        b.extend_from_slice(&r.generation.to_le_bytes());
        b.extend_from_slice(&r.body_crc32c.to_le_bytes());
        b.extend_from_slice(&r.first_sequence.to_le_bytes());
        b.extend_from_slice(&r.last_sequence.to_le_bytes());
        b.extend_from_slice(&r.first_commit_time.to_le_bytes());
        b.extend_from_slice(&r.last_commit_time.to_le_bytes());
        b.extend_from_slice(&r.offset.to_le_bytes());
    }
    b
}

/// Decodes the commit-time directory: at least one record of at least 48 bytes; records ascending by position
/// and not overlapping, offsets rising, commit times not decreasing within a generation.
pub(crate) fn decode_times_directory(body: &[u8]) -> Result<Vec<TimesRecord>, Bad> {
    if body.len() < TIMES_DIR_HEAD {
        return Err(Bad::body("commit-time directory"));
    }
    let size = usize::from(u16_at(body, 0));
    let count = u32_at(body, 4) as usize;
    records(body, TIMES_DIR_HEAD, size, count, TIMES_RECORD)?;
    let recs: Vec<TimesRecord> = (0..count)
        .map(|i| {
            let r = &body[TIMES_DIR_HEAD + size * i..];
            TimesRecord {
                generation: u32_at(r, 0),
                body_crc32c: u32_at(r, 4),
                first_sequence: u64_at(r, 8),
                last_sequence: u64_at(r, 16),
                first_commit_time: i64_at(r, 24),
                last_commit_time: i64_at(r, 32),
                offset: u64_at(r, 40),
            }
        })
        .collect();
    let ok = !recs.is_empty()
        && recs.iter().all(|r| {
            r.first_sequence <= r.last_sequence && r.first_commit_time <= r.last_commit_time
        })
        && recs.windows(2).all(|w| {
            let (a, b) = (&w[0], &w[1]);
            a.offset < b.offset
                && (a.generation < b.generation
                    || (a.generation == b.generation
                        && a.last_sequence < b.first_sequence
                        && a.last_commit_time <= b.first_commit_time))
        });
    if !ok {
        return Err(Bad::body("commit-time directory records"));
    }
    Ok(recs)
}

/// The body of a generation marker (`0x37`).
pub(crate) const MARKER_LEN: usize = 8;

pub(crate) fn encode_marker(generation: u32, block_count: u32) -> [u8; MARKER_LEN] {
    let mut b = [0u8; MARKER_LEN];
    b[..4].copy_from_slice(&generation.to_le_bytes());
    b[4..].copy_from_slice(&block_count.to_le_bytes());
    b
}

/// A trailer (`0x7F`): the file's counts and ranges, where its sections are, and the checksums that bind the
/// blocks the reader reaches first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Trailer {
    /// Positions held: facts and entries.
    pub entry_count: u64,
    /// Facts held.
    pub fact_count: u64,
    /// The least position held.
    pub first: Position,
    /// The greatest position held.
    pub last: Position,
    /// The least commit time of a batch row.
    pub first_commit_time: i64,
    /// The greatest commit time of a batch row.
    pub last_commit_time: i64,
    /// The least `t` of a fact; 0 with none.
    pub min_t: i64,
    /// The greatest reach of a fact; 0 with none.
    pub max_reach: i64,
    /// Chunks.
    pub chunk_count: u32,
    /// Commit-times blocks.
    pub times_count: u32,
    /// The first generation marker; 0 with no entries.
    pub entries_offset: u64,
    /// The first commit-times block.
    pub times_offset: u64,
    /// The commit-time directory.
    pub times_dir_offset: u64,
    /// The first index block; 0 with no chunk.
    pub index_offset: u64,
    /// The index root.
    pub index_root_offset: u64,
    /// CRC32C over every byte from offset 0 to the trailer's block header.
    pub file_crc32c: u32,
    /// The index root's `body_crc32c`.
    pub root_crc32c: u32,
    /// The commit-time directory's `body_crc32c`.
    pub times_dir_crc32c: u32,
    /// The description's `body_crc32c`.
    pub description_crc32c: u32,
    /// CRC32C over the entries section, from the first marker's header to the first commit-times block's; 0 with
    /// no entries.
    pub entries_crc32c: u32,
}

/// Bytes of a v1 trailer body.
pub(crate) const TRAILER_LEN: usize = 144;

impl Trailer {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(TRAILER_LEN);
        b.extend_from_slice(&self.entry_count.to_le_bytes());
        b.extend_from_slice(&self.fact_count.to_le_bytes());
        put_position(&mut b, self.first);
        put_position(&mut b, self.last);
        for v in [
            self.first_commit_time,
            self.last_commit_time,
            self.min_t,
            self.max_reach,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.chunk_count.to_le_bytes());
        b.extend_from_slice(&self.times_count.to_le_bytes());
        for v in [
            self.entries_offset,
            self.times_offset,
            self.times_dir_offset,
            self.index_offset,
            self.index_root_offset,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            self.file_crc32c,
            self.root_crc32c,
            self.times_dir_crc32c,
            self.description_crc32c,
            self.entries_crc32c,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&[0; 4]);
        b
    }

    /// Decodes a body of at least 144 bytes, ignoring a longer body's tail, and checks its counts and ranges.
    pub(crate) fn decode(b: &[u8]) -> Result<Self, Bad> {
        if b.len() < TRAILER_LEN {
            return Err(Bad::body("trailer length"));
        }
        let t = Self {
            entry_count: u64_at(b, 0),
            fact_count: u64_at(b, 8),
            first: position_at(b, 16),
            last: position_at(b, 28),
            first_commit_time: i64_at(b, 40),
            last_commit_time: i64_at(b, 48),
            min_t: i64_at(b, 56),
            max_reach: i64_at(b, 64),
            chunk_count: u32_at(b, 72),
            times_count: u32_at(b, 76),
            entries_offset: u64_at(b, 80),
            times_offset: u64_at(b, 88),
            times_dir_offset: u64_at(b, 96),
            index_offset: u64_at(b, 104),
            index_root_offset: u64_at(b, 112),
            file_crc32c: u32_at(b, 120),
            root_crc32c: u32_at(b, 124),
            times_dir_crc32c: u32_at(b, 128),
            description_crc32c: u32_at(b, 132),
            entries_crc32c: u32_at(b, 136),
        };
        let no_fact = t.fact_count == 0;
        let ok = t.entry_count >= 1
            && t.fact_count <= t.entry_count
            && t.first <= t.last
            && t.first_commit_time <= t.last_commit_time
            && (u64::from(t.chunk_count) <= t.fact_count)
            && (no_fact == (t.chunk_count == 0))
            && (!no_fact || (t.min_t == 0 && t.max_reach == 0))
            && t.times_count >= 1
            && (no_fact == (t.index_offset == 0))
            && (t.entries_offset != 0 || t.entries_crc32c == 0);
        if !ok {
            return Err(Bad::body("trailer counts"));
        }
        if t.times_offset == 0 || t.times_dir_offset == 0 || t.index_root_offset == 0 {
            return Err(Bad::body("trailer offsets"));
        }
        let mut at = [
            t.entries_offset,
            t.times_offset,
            t.times_dir_offset,
            t.index_offset,
            t.index_root_offset,
        ]
        .into_iter()
        .filter(|&o| o != 0);
        let mut prev = 0u64;
        if !at.all(|o| {
            let ok = o > prev && o.is_multiple_of(8);
            prev = o;
            ok
        }) {
            return Err(Bad::body("trailer offsets"));
        }
        Ok(t)
    }
}

/// Bytes of a chunk index object's trailer body in v1 (B.2).
pub(crate) const INDEX_TRAILER_LEN: usize = 32;

/// A chunk index object's trailer: where its blocks are, its own checksum and the root's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndexTrailer {
    pub(crate) index_offset: u64,
    pub(crate) index_root_offset: u64,
    pub(crate) times_dir_offset: u64,
    pub(crate) file_crc32c: u32,
    pub(crate) root_crc32c: u32,
}

impl IndexTrailer {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(INDEX_TRAILER_LEN);
        for v in [
            self.index_offset,
            self.index_root_offset,
            self.times_dir_offset,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.file_crc32c.to_le_bytes());
        b.extend_from_slice(&self.root_crc32c.to_le_bytes());
        b
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self, Bad> {
        if b.len() < INDEX_TRAILER_LEN {
            return Err(Bad::body("index object trailer"));
        }
        Ok(Self {
            index_offset: u64_at(b, 0),
            index_root_offset: u64_at(b, 8),
            times_dir_offset: u64_at(b, 16),
            file_crc32c: u32_at(b, 24),
            root_crc32c: u32_at(b, 28),
        })
    }
}
