//! Just enough of `format.md` Part A to write and scan log segments: the file header (A.1), blocks (A.2), a
//! facts block with plain columns (A.5), the commit block (A.7) and the durable mark (A.10).
//!
//! Not covered: entries blocks, run-length encoding, text values, the segment index and the trailer.

use crc::{CRC_32_ISCSI, Crc};

const CASTAGNOLI: Crc<u32, crc::Table<16>> = Crc::<u32, crc::Table<16>>::new(&CRC_32_ISCSI);

/// CRC32C (Castagnoli) of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    CASTAGNOLI.checksum(bytes)
}

/// Size of the file header (A.1).
pub const FILE_HEADER_LEN: usize = 64;
/// Size of a block header (A.2).
pub const BLOCK_HEADER_LEN: usize = 16;
/// Block type of a facts block (A.4).
pub const TYPE_FACTS: u8 = 0x01;
/// Block type of a commit block (A.4).
pub const TYPE_COMMIT: u8 = 0x0F;
/// Length of a commit block body (A.7).
pub const COMMIT_BODY_LEN: usize = 48;
/// Largest number of rows in one facts block (A.5).
pub const MAX_ROWS_PER_BLOCK: usize = 65_536;
/// Bytes of payload per reading: the widths of the six fixed columns of A.5 (4 + 8 + 1 + 2 + 1 + 8).
pub const PAYLOAD_BYTES_PER_READING: u64 = 24;
/// Size of the mark file (A.10).
pub const MARK_LEN: usize = 64;

/// One reading as the spike records it: an `f64` value of good quality with no flags.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// Series id.
    pub series_id: u32,
    /// Event time, nanoseconds since the Unix epoch.
    pub t: i64,
    /// Value.
    pub value: f64,
}

/// Rounds `n` up to a multiple of 8.
pub fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// Encodes the 64-byte file header of a log segment (A.1, `kind = 1`).
pub fn log_segment_header(
    generation: u32,
    file_id: u64,
    base_sequence: u64,
    created_at: i64,
    store_id: [u8; 16],
) -> [u8; FILE_HEADER_LEN] {
    let mut h = [0u8; FILE_HEADER_LEN];
    h[0..4].copy_from_slice(b"KVIK");
    h[4] = 1;
    h[5] = 1;
    h[12..16].copy_from_slice(&generation.to_le_bytes());
    h[16..24].copy_from_slice(&file_id.to_le_bytes());
    h[24..32].copy_from_slice(&base_sequence.to_le_bytes());
    h[32..40].copy_from_slice(&created_at.to_le_bytes());
    h[40..56].copy_from_slice(&store_id);
    let crc = crc32c(&h[0..60]);
    h[60..64].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Appends one block (header, body, zero padding to 8) to `out` and returns the body checksum.
fn push_block(out: &mut Vec<u8>, block_type: u8, body: &[u8]) -> u32 {
    let body_crc = crc32c(body);
    let mut header = [0u8; BLOCK_HEADER_LEN];
    header[0] = block_type;
    header[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
    header[8..12].copy_from_slice(&body_crc.to_le_bytes());
    let header_crc = crc32c(&header[0..12]);
    header[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    out.resize(out.len() + (pad8(body.len()) - body.len()), 0);
    body_crc
}

fn align8(buf: &mut Vec<u8>) {
    buf.resize(pad8(buf.len()), 0);
}

/// Encodes a facts block body (A.5) with every column in plain encoding.
fn facts_body(first_sequence: u64, received_at: i64, rows: &[Reading]) -> Vec<u8> {
    const COLUMNS: usize = 6;
    let n = rows.len();
    let mut body = Vec::with_capacity(24 + 12 * COLUMNS + n * 32);
    body.extend_from_slice(&first_sequence.to_le_bytes());
    body.extend_from_slice(&received_at.to_le_bytes());
    body.extend_from_slice(&(n as u32).to_le_bytes());
    body.push(COLUMNS as u8);
    body.extend_from_slice(&[0; 3]);
    let dir_start = body.len();
    body.resize(dir_start + 12 * COLUMNS, 0);
    align8(&mut body);

    let mut directory = [(0u8, 0u32, 0u32); COLUMNS];
    let mut column = |id: u8, body: &mut Vec<u8>, write: &dyn Fn(&mut Vec<u8>)| {
        let start = body.len();
        write(body);
        directory[id as usize - 1] = (id, start as u32, (body.len() - start) as u32);
        align8(body);
    };
    column(1, &mut body, &|b| {
        rows.iter()
            .for_each(|r| b.extend_from_slice(&r.series_id.to_le_bytes()))
    });
    column(2, &mut body, &|b| {
        rows.iter()
            .for_each(|r| b.extend_from_slice(&r.t.to_le_bytes()))
    });
    column(3, &mut body, &|b| b.resize(b.len() + n, 0)); // quality: good
    column(4, &mut body, &|b| b.resize(b.len() + 2 * n, 0)); // flags: none
    column(5, &mut body, &|b| b.resize(b.len() + n, 1)); // value type: f64
    column(6, &mut body, &|b| {
        rows.iter()
            .for_each(|r| b.extend_from_slice(&r.value.to_bits().to_le_bytes()))
    });

    for (i, (id, offset, length)) in directory.iter().enumerate() {
        let e = dir_start + 12 * i;
        body[e] = *id;
        body[e + 1] = 0; // plain
        body[e + 4..e + 8].copy_from_slice(&offset.to_le_bytes());
        body[e + 8..e + 12].copy_from_slice(&length.to_le_bytes());
    }
    body
}

/// An encoded batch, ready to be written at the end of the segment.
#[derive(Debug)]
pub struct EncodedBatch {
    /// Data blocks followed by the commit block, each padded to 8 bytes.
    pub bytes: Vec<u8>,
    /// Sequence of the batch's first entry.
    pub first_sequence: u64,
    /// Sequence of the batch's last entry (inclusive).
    pub last_sequence: u64,
}

/// Encodes `rows` as one batch: facts blocks of at most [`MAX_ROWS_PER_BLOCK`] rows, then the commit block.
///
/// `rows` must not be empty.
pub fn encode_batch(
    generation: u32,
    first_sequence: u64,
    commit_time: i64,
    received_at: i64,
    rows: &[Reading],
) -> EncodedBatch {
    assert!(!rows.is_empty(), "a batch holds at least one entry");
    let mut bytes = Vec::with_capacity(rows.len() * 32 + 256);
    let mut body_crcs = Vec::new();
    let mut sequence = first_sequence;
    for chunk in rows.chunks(MAX_ROWS_PER_BLOCK) {
        let body = facts_body(sequence, received_at, chunk);
        body_crcs.extend_from_slice(&push_block(&mut bytes, TYPE_FACTS, &body).to_le_bytes());
        sequence += chunk.len() as u64;
    }
    let last_sequence = sequence - 1;
    let mut commit = [0u8; COMMIT_BODY_LEN];
    commit[0..4].copy_from_slice(&generation.to_le_bytes());
    commit[4..8].copy_from_slice(&((body_crcs.len() / 4) as u32).to_le_bytes());
    commit[8..16].copy_from_slice(&first_sequence.to_le_bytes());
    commit[16..24].copy_from_slice(&last_sequence.to_le_bytes());
    commit[24..32].copy_from_slice(&commit_time.to_le_bytes());
    commit[32..40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    commit[40..44].copy_from_slice(&crc32c(&body_crcs).to_le_bytes());
    push_block(&mut bytes, TYPE_COMMIT, &commit);
    EncodedBatch {
        bytes,
        first_sequence,
        last_sequence,
    }
}

/// A complete batch found by [`scan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchExtent {
    /// Offset of the batch's first block header.
    pub start: u64,
    /// Offset just past the commit block, including padding.
    pub end: u64,
    /// First sequence.
    pub first_sequence: u64,
    /// Last sequence (inclusive).
    pub last_sequence: u64,
}

/// Why [`scan`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEnd {
    /// Sixteen zero bytes at a block boundary: unwritten preallocated space.
    EndOfData,
    /// The end of the bytes was reached at a block boundary.
    EndOfFile,
    /// A block or batch failed a check; nothing from this offset on is used.
    Invalid,
}

/// The result of scanning a segment.
#[derive(Debug)]
pub struct Scan {
    /// Complete batches, in order.
    pub batches: Vec<BatchExtent>,
    /// Offset just past the last complete batch (the header length if there is none).
    pub valid_end: u64,
    /// Why the scan stopped.
    pub end: ScanEnd,
    /// Offset at which the scan stopped.
    pub stopped_at: u64,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("eight bytes"))
}

/// Scans a log segment (`bytes` starts with the file header) and returns its complete batches (A.2, A.7).
///
/// `first_sequence` is the sequence the first batch must start with (`base_sequence`).
pub fn scan(bytes: &[u8], first_sequence: u64) -> Scan {
    let mut at = FILE_HEADER_LEN;
    let mut batches = Vec::new();
    let mut valid_end = FILE_HEADER_LEN as u64;
    let mut next_sequence = first_sequence;
    let mut batch_start = at;
    let mut body_crcs: Vec<u8> = Vec::new();
    let mut block_count = 0u32;
    let end = loop {
        if at == bytes.len() {
            break ScanEnd::EndOfFile;
        }
        if bytes.len() - at < BLOCK_HEADER_LEN {
            break ScanEnd::Invalid;
        }
        let header = &bytes[at..at + BLOCK_HEADER_LEN];
        if header.iter().all(|&b| b == 0) {
            break ScanEnd::EndOfData;
        }
        if crc32c(&header[0..12]) != u32_at(header, 12) {
            break ScanEnd::Invalid;
        }
        let block_type = header[0];
        let body_len = u32_at(header, 4) as usize;
        let body_start = at + BLOCK_HEADER_LEN;
        if body_len > 16 << 20 || bytes.len() - body_start < pad8(body_len) {
            break ScanEnd::Invalid;
        }
        let body = &bytes[body_start..body_start + body_len];
        let body_crc = u32_at(header, 8);
        if crc32c(body) != body_crc {
            break ScanEnd::Invalid;
        }
        let next = body_start + pad8(body_len);
        match block_type {
            TYPE_FACTS => {
                if body_len < 24
                    || u64_at(body, 0)
                        != next_sequence + rows_before(&body_crcs, bytes, batch_start)
                {
                    break ScanEnd::Invalid;
                }
                body_crcs.extend_from_slice(&body_crc.to_le_bytes());
                block_count += 1;
            }
            TYPE_COMMIT => {
                let ok = body_len == COMMIT_BODY_LEN
                    && u32_at(body, 4) == block_count
                    && u64_at(body, 8) == next_sequence
                    && u64_at(body, 32) == (at - batch_start) as u64
                    && u32_at(body, 40) == crc32c(&body_crcs)
                    && block_count > 0;
                if !ok {
                    break ScanEnd::Invalid;
                }
                let last = u64_at(body, 16);
                batches.push(BatchExtent {
                    start: batch_start as u64,
                    end: next as u64,
                    first_sequence: next_sequence,
                    last_sequence: last,
                });
                next_sequence = last + 1;
                valid_end = next as u64;
                batch_start = next;
                body_crcs.clear();
                block_count = 0;
            }
            _ => break ScanEnd::Invalid,
        }
        at = next;
    };
    Scan {
        batches,
        valid_end,
        end,
        stopped_at: at as u64,
    }
}

/// Rows held by the facts blocks already seen in the current batch.
fn rows_before(body_crcs: &[u8], bytes: &[u8], batch_start: usize) -> u64 {
    let mut rows = 0u64;
    let mut at = batch_start;
    for _ in 0..body_crcs.len() / 4 {
        let body_len = u32_at(bytes, at + 4) as usize;
        rows += u64::from(u32_at(bytes, at + BLOCK_HEADER_LEN + 16));
        at += BLOCK_HEADER_LEN + pad8(body_len);
    }
    rows
}

/// The fields of the durable mark (A.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Sequence lock; odd while the writer is updating.
    pub seq: u32,
    /// Boot identifier of the machine when the mark was written.
    pub boot_id: [u8; 16],
    /// Generation.
    pub generation: u32,
    /// Segment the mark points into.
    pub file_id: u64,
    /// Offset just past the last durable commit block.
    pub offset: u64,
    /// Last durable sequence.
    pub last_sequence: u64,
}

impl Mark {
    /// Encodes the whole 64-byte mark.
    pub fn encode(&self) -> [u8; MARK_LEN] {
        let mut m = [0u8; MARK_LEN];
        m[0..8].copy_from_slice(b"KVIKMARK");
        m[8..12].copy_from_slice(&1u32.to_le_bytes());
        m[12..16].copy_from_slice(&self.seq.to_le_bytes());
        m[16..32].copy_from_slice(&self.boot_id);
        m[32..36].copy_from_slice(&self.generation.to_le_bytes());
        m[40..48].copy_from_slice(&self.file_id.to_le_bytes());
        m[48..56].copy_from_slice(&self.offset.to_le_bytes());
        m[56..64].copy_from_slice(&self.last_sequence.to_le_bytes());
        m
    }

    /// Decodes a mark; `None` if it is missing, malformed or half-updated (odd `seq`).
    pub fn decode(bytes: &[u8]) -> Option<Mark> {
        if bytes.len() < MARK_LEN || &bytes[0..8] != b"KVIKMARK" || u32_at(bytes, 8) != 1 {
            return None;
        }
        let seq = u32_at(bytes, 12);
        if seq % 2 == 1 {
            return None;
        }
        Some(Mark {
            seq,
            boot_id: bytes[16..32].try_into().expect("sixteen bytes"),
            generation: u32_at(bytes, 32),
            file_id: u64_at(bytes, 40),
            offset: u64_at(bytes, 48),
            last_sequence: u64_at(bytes, 56),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize, first_series: u32) -> Vec<Reading> {
        (0..n)
            .map(|i| Reading {
                series_id: first_series + i as u32,
                t: 1_000 * i as i64,
                value: i as f64 * 0.5,
            })
            .collect()
    }

    fn segment(batches: &[usize]) -> (Vec<u8>, Vec<BatchExtent>) {
        let mut bytes = log_segment_header(1, 7, 1, 0, [9; 16]).to_vec();
        let mut extents = Vec::new();
        let mut seq = 1;
        for (i, &n) in batches.iter().enumerate() {
            let start = bytes.len() as u64;
            let b = encode_batch(1, seq, i as i64, i as i64, &rows(n, 0));
            bytes.extend_from_slice(&b.bytes);
            extents.push(BatchExtent {
                start,
                end: bytes.len() as u64,
                first_sequence: seq,
                last_sequence: b.last_sequence,
            });
            seq = b.last_sequence + 1;
        }
        (bytes, extents)
    }

    #[test]
    fn header_checksum_covers_first_sixty_bytes() {
        let h = log_segment_header(3, 4, 5, 6, [1; 16]);
        assert_eq!(&h[0..4], b"KVIK");
        assert_eq!(u32_at(&h, 60), crc32c(&h[0..60]));
    }

    #[test]
    fn scan_finds_every_batch_and_stops_at_zeroes() {
        let (mut bytes, extents) = segment(&[1, 3, 100, 70_000]);
        bytes.resize(bytes.len() + 4096, 0);
        let s = scan(&bytes, 1);
        assert_eq!(s.batches, extents);
        assert_eq!(s.end, ScanEnd::EndOfData);
        assert_eq!(s.batches[3].last_sequence, 1 + 3 + 100 + 70_000);
    }

    #[test]
    fn truncation_at_every_offset_of_the_last_batch_keeps_exactly_the_earlier_batches() {
        let (bytes, extents) = segment(&[2, 5, 3]);
        let last = extents[2];
        for cut in last.start..last.end {
            let s = scan(&bytes[..cut as usize], 1);
            assert_eq!(s.batches, extents[..2], "cut at {cut}");
            assert_eq!(s.valid_end, last.start);
        }
        assert_eq!(scan(&bytes, 1).batches, extents);
    }

    #[test]
    fn a_flipped_byte_ends_the_readable_part() {
        let (bytes, extents) = segment(&[2, 5, 3]);
        for at in extents[1].start..extents[1].end {
            let mut damaged = bytes.clone();
            damaged[at as usize] ^= 0x40;
            let s = scan(&damaged, 1);
            assert_eq!(s.batches, extents[..1], "flip at {at}");
        }
    }

    #[test]
    fn a_batch_out_of_sequence_is_not_complete() {
        let (bytes, extents) = segment(&[2, 5]);
        assert_eq!(scan(&bytes, 2).batches, []);
        assert_eq!(scan(&bytes, 1).batches, extents);
    }

    #[test]
    fn mark_round_trips_and_an_odd_lock_is_refused() {
        let m = Mark {
            seq: 4,
            boot_id: [7; 16],
            generation: 1,
            file_id: 2,
            offset: 4096,
            last_sequence: 99,
        };
        assert_eq!(Mark::decode(&m.encode()), Some(m));
        assert_eq!(Mark::decode(&Mark { seq: 5, ..m }.encode()), None);
        assert_eq!(Mark::decode(&[0; MARK_LEN]), None);
    }
}
