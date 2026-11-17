//! The entry point of the fuzz target (`files/fuzz`), also run over generated inputs by the stable test suite.

use crate::desc::FileDescription;
use crate::image::read_with_stats;
use crate::record::{Record, is_canonical, record_type};

/// Feeds `data` to the manifest reader and to each record decoder, and checks what they report. Panics only if a
/// check fails (a bug); never for any input.
///
/// The store id is taken from the bytes where the header holds it, so mutated valid manifests reach the records.
pub fn decode_all(data: &[u8]) {
    let mut store_id = [0u8; 16];
    if let Some(s) = data.get(40..56) {
        store_id.copy_from_slice(s);
    }
    let (img, stats) = read_with_stats(data, &store_id);
    assert!(
        stats.within_bound(data.len() as u64),
        "{stats:?} for {} bytes",
        data.len()
    );
    if let Ok(img) = img {
        let len = data.len() as u64;
        let mut at = 64u64;
        let mut last = 0u64;
        let mut spans = img.corrupt.iter().peekable();
        for p in &img.records {
            if let Some(s) = spans.next_if(|s| s.end == p.offset) {
                assert!(s.offset == at && s.offset < s.end && s.after_epoch == last);
                assert_eq!(s.before_epoch, p.record.epoch);
                at = s.end;
            }
            assert_eq!(p.offset, at, "records and spans tile the file");
            assert!(p.record.epoch > last, "epochs rise");
            last = p.record.epoch;
            at = p.end;
            // A valid record encodes again and reads back as itself.
            let b = p.record.encode().expect("a valid record encodes");
            assert!(is_canonical(&b), "its encoding is canonical");
            let end = (p.end as usize).min(data.len());
            if is_canonical(&data[p.offset as usize..end]) {
                assert_eq!(&data[p.offset as usize..end], &b[..]);
            }
        }
        assert!(spans.next().is_none());
        assert_eq!(img.valid_end, at.min(len));
        assert_eq!(img.torn, len > img.valid_end);
    }
    for t in [
        record_type::EMPTY,
        record_type::PUBLISH,
        record_type::SEALED,
        record_type::LOST_RANGE,
        record_type::ACKNOWLEDGEMENT,
        record_type::RESTORE,
        record_type::REBUILD,
        0x50,
    ] {
        for skippable in [false, true] {
            if let Ok(r) = Record::decode(t, skippable, data)
                && let Ok(b) = r.encode()
            {
                let again =
                    Record::decode(t, skippable || b[1] & 1 != 0, &b[16..16 + data_len(&b)]);
                assert_eq!(again.as_ref(), Ok(&r), "a decoded record round-trips");
            }
        }
    }
    if let Ok(d) = FileDescription::decode(data) {
        assert_eq!(FileDescription::decode(&d.encode()), Ok(d));
    }
}

/// The body length of an encoded block.
fn data_len(block: &[u8]) -> usize {
    u32::from_le_bytes([block[4], block[5], block[6], block[7]]) as usize
}
