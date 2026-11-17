//! The entry point of the fuzz target (`chunk/fuzz`), also run over generated inputs by the stable test suite.

use crate::block::directory;
use crate::float::{
    decode_decimal, decode_xor, decode_xor_zstd, decompress_xor, encode_decimal, encode_xor,
};
use crate::ints::{
    decode_delta, decode_plain, decode_run_length, encode_delta, encode_plain, encode_run_length,
};
use crate::{Block, column, encoding};

/// Feeds `data` to the block decoder and every column decoder; panics only when a round-trip or canonical check fails.
pub fn decode_all(data: &[u8]) {
    if let Ok(b) = Block::decode(data) {
        assert!(!b.is_empty() && b.len() <= crate::MAX_ROWS);
        let again = b.encode().expect("a decoded block encodes");
        assert_eq!(
            Block::decode(&again).as_ref(),
            Ok(&b),
            "re-encoded block decodes to the same rows"
        );
        assert_eq!(
            Block::decode_canonical(&again).as_ref(),
            Ok(&b),
            "encoder output is canonical"
        );
    }
    if let Ok(b) = Block::decode_canonical(data) {
        let again = b.encode().expect("a canonical block encodes");
        assert!(
            equivalent_canonical(data, &again),
            "a canonical body is encoder output, up to the XOR class"
        );
    }

    let n = data.get(..2).map_or(0, |b| {
        usize::from(u16::from_le_bytes([b[0], b[1]])) % (crate::MAX_ROWS + 1)
    });
    let payload = data.get(2..).unwrap_or_default();
    if let Ok(v) = decode_delta(payload, n) {
        assert_eq!(v.len(), n);
        assert_eq!(decode_delta(&encode_delta(&v), n).as_ref(), Ok(&v));
    }
    if let Ok(v) = decode_plain::<u64>(payload, n) {
        assert_eq!(encode_plain(&v), payload);
    }
    if let Ok(v) = decode_run_length::<u8>(payload, n) {
        assert_eq!(encode_run_length(&v), payload);
    }
    if let Ok(v) = decode_run_length::<u16>(payload, n) {
        assert_eq!(encode_run_length(&v), payload);
    }
    if let Ok(v) = decode_run_length::<u32>(payload, n) {
        assert_eq!(encode_run_length(&v), payload);
    }
    if let Ok(v) = decode_run_length::<u64>(payload, n) {
        assert_eq!(encode_run_length(&v), payload);
    }
    if let Ok(v) = decode_decimal(payload, n) {
        assert_eq!(v.len(), n);
        let e = payload[0];
        assert_eq!(decode_decimal(&encode_decimal(&v, e), n).as_ref(), Ok(&v));
    }
    if let Ok(v) = decode_xor(payload, n) {
        assert_eq!(v.len(), n);
        assert_eq!(decode_xor(&encode_xor(&v), n).as_ref(), Ok(&v));
    }
    if let Ok(v) = decode_xor_zstd(payload, n) {
        assert_eq!(v.len(), n);
    }
}

/// Whether two bodies are the same canonical body: identical bytes, or identical but for a value column in encoding
/// 4 or 5 holding the same XOR stream in each (`docs/format-chunk.md` C.10, Q-150).
pub fn equivalent_canonical(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    let (Some(ca), Some(cb)) = (directory(a), directory(b)) else {
        return false;
    };
    let Ok(block) = Block::decode(a) else {
        return false;
    };
    let words = (0..block.len())
        .filter(|&i| block.flags[i] & crate::flag::NO_VALUE == 0)
        .count();
    let stream = |enc: u8, p: &[u8]| match enc {
        encoding::XOR => Some(p.to_vec()),
        encoding::XOR_ZSTD => decompress_xor(p, words).ok(),
        _ => None,
    };
    a.get(..2) == b.get(..2)
        && ca.len() == cb.len()
        && ca.iter().zip(&cb).all(|(x, y)| {
            x == y
                || (x.0 == column::VALUE
                    && y.0 == column::VALUE
                    && stream(x.1, x.2).is_some_and(|s| Some(s) == stream(y.1, y.2)))
        })
}

/// The identity a file's own header claims, so that a fuzzed file reaches the checks after the header.
fn claimed_identity(data: &[u8]) -> crate::file::Identity {
    let h = kvik_log::header::FileHeader::decode(data);
    crate::file::Identity {
        store_id: h.map_or([0; 16], |h| h.store_id),
        generation: h.map_or(0, |h| h.generation),
        file_id: h.map_or(0, |h| h.file_id),
    }
}

/// Feeds `data` to the compacted segment reader on every path it has, and to the canonical checks; a file that
/// opens extracts to a chunk index object that opens with the same layout and index.
pub fn decode_compacted(data: &[u8]) {
    use crate::file::{CompactedFile, IndexObject, Reach, SeriesInfo, check_canonical, extract};
    let id = claimed_identity(data);
    let Ok(f) = CompactedFile::open(data, &id, None) else {
        return;
    };
    let layout = f.layout();
    assert!(CompactedFile::open(data, &id, Some(&layout)).is_ok());
    if let Ok(index) = f.index() {
        for e in index.iter().take(64) {
            if let Ok(rows) = f.read_chunk(e) {
                assert_eq!(rows.len(), e.row_count as usize);
            }
            let _ = f.series(e.series_id);
        }
        if let Ok(object) = extract(&f, [1; 32]) {
            let o = IndexObject::open(object.as_slice(), &id).expect("an extraction opens");
            assert_eq!(o.layout(), &layout);
            assert_eq!(o.index().ok().as_ref(), Some(&index));
        }
    }
    let _ = f.entries();
    let _ = f.batches();
    let t = f.trailer();
    for at in [i64::MIN, t.first_commit_time, t.last_commit_time, i64::MAX] {
        let _ = f.as_of(at);
    }
    let _ = check_canonical(data, &id, &|_| SeriesInfo {
        name: None,
        reach: Reach::Point,
    });
}

/// Feeds `data` to the chunk index object reader.
pub fn decode_index_object(data: &[u8]) {
    let id = claimed_identity(data);
    if let Ok(o) = crate::file::IndexObject::open(data, &id)
        && let Ok(index) = o.index()
    {
        for e in index.iter().take(64) {
            let _ = o.series(e.series_id);
        }
    }
}

/// Feeds `data` to every body decoder of a compacted segment and a chunk index object.
pub fn decode_file_bodies(data: &[u8]) {
    crate::file::fuzz_bodies(data);
}

/// Re-seals `data` as a compacted segment and feeds it to [`decode_compacted`], so that edits get past the
/// checksum chain to the checks behind it.
pub fn decode_compacted_resealed(data: &[u8]) {
    decode_compacted(&crate::file::reseal::reseal_segment(data));
}

/// Re-seals `data` as a chunk index object and feeds it to [`decode_index_object`].
pub fn decode_index_object_resealed(data: &[u8]) {
    decode_index_object(&crate::file::reseal::reseal_index_object(data));
}
