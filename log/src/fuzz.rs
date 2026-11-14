//! The entry point of the fuzz targets (`log/fuzz`), also run over generated inputs by the stable test suite.

use crate::batch::SegmentIdentity;
use crate::block::{BLOCK_HEADER_LEN, block_type};
use crate::body::{Commit, DataPrefix, SegmentIndex, Trailer, decode_end_marker};
use crate::bytes::{u32_at, u64_at};
use crate::header::{FileHeader, HEADER_LEN};
use crate::mark::read_mark;
use crate::scan::{ScanLimit, ScanReport, Tail, scan};

/// Feeds `data` to every decoder of this crate as a log segment, a mark file and each body type, and checks what a
/// scan reports. Panics only if a check fails (a bug); never for any input.
///
/// The segment identity is taken from the bytes where the header would be, so mutated valid segments reach the
/// batch checks.
pub fn decode_all(data: &[u8]) {
    let id = if data.len() >= HEADER_LEN {
        let mut store_id = [0u8; 16];
        store_id.copy_from_slice(&data[40..56]);
        SegmentIdentity {
            store_id,
            file_id: u64_at(data, 16),
            generation: u32_at(data, 12),
        }
    } else {
        SegmentIdentity {
            store_id: [0; 16],
            file_id: 0,
            generation: 0,
        }
    };
    let len = data.len() as u64;
    let marks = [
        len,
        (len / 2) & !7,
        data.get(..8).map_or(0, |_| u64_at(data, 0) % (len + 64)),
    ];
    if let Ok(r) = scan(data, &id, ScanLimit::End) {
        check(&r, len, true);
        if let Some(s) = &r.seal {
            assert!(s.end <= len && s.trailer_offset < s.end && s.start <= s.trailer_offset);
            let _ = s.file_crc_matches(data);
        }
    }
    for m in marks {
        if let Ok(r) = scan(data, &id, ScanLimit::Mark(m)) {
            check(&r, m, false);
        }
    }
    let _ = FileHeader::decode(data);
    let _ = read_mark(data, &id.store_id);
    // As a mark file: the store id where slot 0 holds it.
    if let Some(store) = data.get(16..32) {
        let mut store_id = [0u8; 16];
        store_id.copy_from_slice(store);
        let _ = read_mark(data, &store_id);
    }
    let body = data.get(BLOCK_HEADER_LEN.min(data.len())..).unwrap_or(&[]);
    let _ = DataPrefix::decode(block_type::FACTS, body);
    let _ = DataPrefix::decode(block_type::ENTRIES, body);
    let _ = Commit::decode(body);
    let _ = SegmentIndex::decode(body);
    let _ = Trailer::decode(body);
    let _ = decode_end_marker(body);
}

/// Batches and regions are in file order, do not overlap and lie below `limit`; sequences increase.
fn check(r: &ScanReport<'_>, limit: u64, to_end: bool) {
    let mut spans: Vec<(u64, u64)> = r.batches.iter().map(|b| (b.offset, b.end())).collect();
    spans.extend(r.regions.iter().map(|g| (g.start, g.end)));
    spans.sort_unstable();
    let mut at = HEADER_LEN as u64;
    for (s, e) in spans {
        assert!(
            s >= at && s < e && e <= limit.max(HEADER_LEN as u64),
            "span {s}..{e} after {at}, limit {limit}"
        );
        at = e;
    }
    for w in r.batches.windows(2) {
        assert!(w[1].commit.first_sequence > w[0].commit.last_sequence);
    }
    for b in &r.batches {
        assert_eq!(b.end() % 8, b.offset % 8);
        assert_eq!(
            b.commit.last_sequence + 1 - b.commit.first_sequence,
            b.entry_count()
        );
    }
    match (&r.tail, to_end) {
        (Some(Tail::Torn { start, end, .. }), true) => assert!(start < end && *end <= limit),
        (Some(Tail::Clean { offset }), true) => assert!(*offset <= limit.max(HEADER_LEN as u64)),
        (None, _) | (Some(_), false) => {}
    }
}
