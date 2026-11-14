//! Shared test helpers: hand-laid facts and entries bodies (`format.md` A.5, A.6) and small segments.
//!
//! The batch layer that encodes these bodies is roadmap 4.3; until then the bodies used by the container tests and
//! the golden files are laid out here by hand, following A.5 and A.6 exactly (plain encodings only: with at most two
//! rows per block, run-length is never smaller than plain, so plain is the canonical choice).

#![allow(dead_code)]

use kvik_log::batch::{BatchBuilder, SegmentIdentity};
use kvik_log::block::block_type;
use kvik_log::body::DataPrefix;
use kvik_log::segment::SegmentImage;

/// The identity used by most tests.
pub const ID: SegmentIdentity = SegmentIdentity {
    store_id: STORE,
    file_id: 1,
    generation: 0,
};
/// A store id: the bytes 1 to 16.
pub const STORE: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
/// 1 second in nanoseconds.
pub const SECOND: i64 = 1_000_000_000;
/// A fixed time: 2026-01-01T00:00:00Z.
pub const T0: i64 = 1_767_225_600 * SECOND;

fn pad_to(b: &mut Vec<u8>, align: usize) {
    b.resize(b.len().div_ceil(align) * align, 0);
}

/// A facts body (A.5) of `f64` rows `(series_id, t, value)`, columns 1, 2, 5 and 6 in plain encoding.
pub fn facts_body(first_sequence: u64, received_at: i64, rows: &[(u32, i64, f64)]) -> Vec<u8> {
    assert!(!rows.is_empty());
    let n = rows.len();
    let columns: [(u8, Vec<u8>); 4] = [
        (1, rows.iter().flat_map(|r| r.0.to_le_bytes()).collect()),
        (2, rows.iter().flat_map(|r| r.1.to_le_bytes()).collect()),
        (5, vec![1u8; n]),
        (
            6,
            rows.iter()
                .flat_map(|r| r.2.to_bits().to_le_bytes())
                .collect(),
        ),
    ];
    let mut body = vec![0u8; 24];
    DataPrefix {
        first_sequence,
        received_at,
        count: n as u32,
    }
    .write(&mut body);
    body[20] = columns.len() as u8;
    body[21] = 12;
    let dir_at = body.len();
    body.resize(dir_at + 12 * columns.len(), 0);
    pad_to(&mut body, 8);
    for (i, (column_id, payload)) in columns.iter().enumerate() {
        pad_to(&mut body, 8);
        let offset = body.len() as u32;
        let e = dir_at + 12 * i;
        body[e] = *column_id;
        body[e + 1] = 0; // plain
        body[e + 4..e + 8].copy_from_slice(&offset.to_le_bytes());
        body[e + 8..e + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    body
}

/// An entry: its type and its fields `(field_id, value)`, in ascending id order.
pub type Entry = (u8, Vec<(u16, Vec<u8>)>);

/// An entries body (A.6).
pub fn entries_body(first_sequence: u64, received_at: i64, entries: &[Entry]) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    DataPrefix {
        first_sequence,
        received_at,
        count: entries.len() as u32,
    }
    .write(&mut body);
    for (entry_type, fields) in entries {
        let mut f = Vec::new();
        for (id, value) in fields {
            f.extend_from_slice(&id.to_le_bytes());
            f.extend_from_slice(&(value.len() as u16).to_le_bytes());
            f.extend_from_slice(value);
            pad_to(&mut f, 4);
        }
        body.extend_from_slice(&[*entry_type, 0, 0, 0]);
        body.extend_from_slice(&(f.len() as u32).to_le_bytes());
        body.extend_from_slice(&f);
        pad_to(&mut body, 8);
    }
    body
}

/// A series definition (A.6, entry type 1) of a gauge with `f64` values, every applicable field carried.
pub fn definition(id: u32, name: &str, unit: &str) -> Entry {
    let i64f = |v: i64| v.to_le_bytes().to_vec();
    (
        1,
        vec![
            (1, id.to_le_bytes().to_vec()),
            (2, name.as_bytes().to_vec()),
            (3, vec![1]), // kind: gauge
            (4, vec![1]), // value type: f64
            (6, unit.as_bytes().to_vec()),
            (7, i64f(10 * SECOND)),     // expected interval
            (8, i64f(30 * SECOND)),     // staleness limit: 3 × the interval
            (9, vec![0]),               // record on change: no
            (12, i64f(3_600 * SECOND)), // lateness bound
            (14, vec![0]),              // not versioned
            (15, vec![0]),              // observed
            (23, vec![0]),              // retention class: long-term
            (27, vec![0]),              // staleness limit follows the interval
        ],
    )
}

/// An alias entry (A.6, entry type 3).
pub fn alias(id: u32, name: &str) -> Entry {
    (
        3,
        vec![
            (1, id.to_le_bytes().to_vec()),
            (2, name.as_bytes().to_vec()),
        ],
    )
}

/// A batch of one facts block.
pub fn facts_batch(
    first_sequence: u64,
    received_at: i64,
    rows: &[(u32, i64, f64)],
) -> BatchBuilder {
    let mut b = BatchBuilder::new();
    b.push(
        block_type::FACTS,
        facts_body(first_sequence, received_at, rows),
    )
    .unwrap();
    b
}

/// A batch of one entries block.
pub fn entries_batch(first_sequence: u64, received_at: i64, entries: &[Entry]) -> BatchBuilder {
    let mut b = BatchBuilder::new();
    b.push(
        block_type::ENTRIES,
        entries_body(first_sequence, received_at, entries),
    )
    .unwrap();
    b
}

/// The three batches most segment tests hold: a definition (sequence 1), two readings (2 and 3), and an alias with
/// a reading (4 and 5) in two blocks.
pub fn three_batches() -> Vec<(BatchBuilder, i64)> {
    let r0 = T0 + SECOND;
    let mut third = entries_batch(4, r0 + 20 * SECOND, &[alias(1, "room/temp")]);
    third
        .push(
            block_type::FACTS,
            facts_body(5, r0 + 20 * SECOND, &[(1, T0 + 20 * SECOND, 21.25)]),
        )
        .unwrap();
    vec![
        (
            entries_batch(1, r0, &[definition(1, "room/temperature", "degC")]),
            r0 + 1_000,
        ),
        (
            facts_batch(
                2,
                r0 + 10 * SECOND,
                &[(1, T0, 20.5), (1, T0 + 10 * SECOND, 20.75)],
            ),
            r0 + 10 * SECOND + 1_000,
        ),
        (third, r0 + 20 * SECOND + 1_000),
    ]
}

/// A segment in use holding `batches`, zero-filled to `len`; returns it and the batch offsets.
pub fn segment(
    id: SegmentIdentity,
    len: u64,
    base: u64,
    batches: &[(BatchBuilder, i64)],
) -> (SegmentImage, Vec<u64>) {
    let mut s = SegmentImage::create_ahead(id, T0, len).unwrap();
    s.take_into_use(base).unwrap();
    let offsets = batches
        .iter()
        .map(|(b, t)| s.append(b, *t).unwrap())
        .collect();
    (s, offsets)
}

/// The bytes of a batch at `offset`, built straight from the tables of A.2 and A.7 without the crate's batch
/// encoder and without any validation: blocks `(type, flags, body)`, then a commit block with the given sequences.
/// `block_count`, `batch_bytes` and `batch_crc32c` are computed as A.7 defines them.
pub fn raw_batch(
    id: &SegmentIdentity,
    offset: u64,
    blocks: &[(u8, u8, Vec<u8>)],
    first_sequence: u64,
    last_sequence: u64,
    commit_time: i64,
) -> Vec<u8> {
    raw_batch_reserved(
        id,
        offset,
        blocks,
        first_sequence,
        last_sequence,
        commit_time,
        0,
    )
}

/// [`raw_batch`] with `reserved` in bytes 2..4 of every block header, as a later minor version might write them.
pub fn raw_batch_reserved(
    id: &SegmentIdentity,
    offset: u64,
    blocks: &[(u8, u8, Vec<u8>)],
    first_sequence: u64,
    last_sequence: u64,
    commit_time: i64,
    reserved: u16,
) -> Vec<u8> {
    use kvik_log::crc::crc32c;
    let mut out = Vec::new();
    let mut covered = Vec::new();
    covered.extend_from_slice(&id.store_id);
    covered.extend_from_slice(&id.file_id.to_le_bytes());
    covered.extend_from_slice(&offset.to_le_bytes());
    let block = |t: u8, flags: u8, body: &[u8], out: &mut Vec<u8>| -> [u8; 16] {
        let mut h = [0u8; 16];
        h[0] = t;
        h[1] = flags;
        h[2..4].copy_from_slice(&reserved.to_le_bytes());
        h[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        h[8..12].copy_from_slice(&crc32c(body).to_le_bytes());
        let c = crc32c(&h[..12]);
        h[12..16].copy_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(8) * 8, 0);
        h
    };
    for (t, flags, body) in blocks {
        let h = block(*t, *flags, body, &mut out);
        covered.extend_from_slice(&h);
    }
    let mut commit = Vec::new();
    commit.extend_from_slice(&id.generation.to_le_bytes());
    commit.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    commit.extend_from_slice(&first_sequence.to_le_bytes());
    commit.extend_from_slice(&last_sequence.to_le_bytes());
    commit.extend_from_slice(&commit_time.to_le_bytes());
    commit.extend_from_slice(&(out.len() as u64).to_le_bytes());
    commit.extend_from_slice(&crc32c(&covered).to_le_bytes());
    commit.extend_from_slice(&[0; 4]);
    block(0x0F, 0, &commit, &mut out);
    out
}

/// Writes `bytes` into `image` at `offset`.
pub fn put(image: &mut [u8], offset: u64, bytes: &[u8]) {
    image[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
}

/// A facts body laid out like the canonical encoder (prefix, directory of 12-byte entries, payloads aligned to 8 in
/// directory order) but with any columns `(id, encoding, payload)` and any directory values, for tests of the checks.
pub fn facts_raw(row_count: u32, columns: &[(u8, u8, Vec<u8>)]) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    DataPrefix {
        first_sequence: 1,
        received_at: T0,
        count: row_count,
    }
    .write(&mut body);
    body[20] = columns.len() as u8;
    body[21] = 12;
    let dir = body.len();
    body.resize(dir + 12 * columns.len(), 0);
    for (i, (id, enc, payload)) in columns.iter().enumerate() {
        pad_to(&mut body, 8);
        let e = dir + 12 * i;
        body[e] = *id;
        body[e + 1] = *enc;
        let offset = body.len() as u32;
        body[e + 4..e + 8].copy_from_slice(&offset.to_le_bytes());
        body[e + 8..e + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    body
}
