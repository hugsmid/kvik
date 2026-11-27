//! The entry points of the fuzz targets (`fuzz/`), also run on stable by `tests/fuzz_inputs.rs`.

use crate::{decode, encode};

/// Decodes arbitrary bytes as a stored node; whatever decodes must encode again to exactly the same bytes
/// (`format-tree.md` T.5: one byte sequence per node).
///
/// # Panics
/// Panics when a decoded node does not encode, or encodes to other bytes.
pub fn decode_node(data: &[u8]) {
    if let Ok(node) = decode(data) {
        let again = encode(&node).expect("a decoded node is storable");
        assert_eq!(again, data, "a decoded node encodes to other bytes");
    }
}

/// Decodes arbitrary bytes as a tree part; whatever decodes must encode again to the same bytes (`format-tree.md`
/// T.11).
///
/// # Panics
/// Panics when a decoded part does not encode, or encodes to other bytes.
pub fn decode_part(data: &[u8]) {
    if let Ok(part) = crate::part::Part::decode(data) {
        let again = part.encode().expect("a decoded part is encodable");
        assert_eq!(again, data, "a decoded part encodes to other bytes");
    }
}

/// Decodes arbitrary bytes as a tree root; whatever decodes must encode again to the same bytes (T.12).
///
/// # Panics
/// Panics when a decoded root does not encode, or encodes to other bytes.
pub fn decode_root(data: &[u8]) {
    if let Ok(root) = crate::root::Root::decode(data) {
        let again = root.encode().expect("a decoded root is encodable");
        assert_eq!(again, data, "a decoded root encodes to other bytes");
    }
}

/// Recomputes the header checksum, every block's checksums and a part trailer's `file_crc32c` in place, walking the
/// blocks by their stated lengths, so that mutated files reach the body checks. Never panics.
pub fn reframe(data: &mut [u8]) {
    use kvik_log::crc::crc32c;
    if data.len() < 64 {
        return;
    }
    let crc = crc32c(&data[..60]);
    data[60..64].copy_from_slice(&crc.to_le_bytes());
    let limit = if data.ends_with(b"KVIKEND\0") {
        data.len() - 16
    } else {
        data.len()
    };
    let mut at = 64usize;
    while at + 16 <= limit {
        let len = u32::from_le_bytes(data[at + 4..at + 8].try_into().expect("4")) as usize;
        let body = at + 16;
        let Some(end) = body.checked_add(len).filter(|e| *e <= limit) else {
            return;
        };
        if data[at] == crate::part::block_type::TRAILER && len >= 100 {
            let crc = crc32c(&data[..at]);
            data[body + 96..body + 100].copy_from_slice(&crc.to_le_bytes());
        }
        let crc = crc32c(&data[body..end]);
        data[at + 8..at + 12].copy_from_slice(&crc.to_le_bytes());
        let crc = crc32c(&data[at..at + 12]);
        data[at + 12..at + 16].copy_from_slice(&crc.to_le_bytes());
        at = body + len.div_ceil(8) * 8;
    }
}
