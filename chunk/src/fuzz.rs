//! The entry point of the fuzz target (`chunk/fuzz`), also run over generated inputs by the stable test suite.

use crate::Block;
use crate::float::{decode_decimal, decode_xor, decode_xor_zstd, encode_decimal, encode_xor};
use crate::ints::{
    decode_delta, decode_plain, decode_run_length, encode_delta, encode_plain, encode_run_length,
};

/// Feeds `data` to the block decoder and, with a value count taken from its first two bytes, to every column
/// decoder. Panics only if a check fails (a bug), never for any input:
/// - whatever decodes is within the row limit and encodes again to bytes that decode to the same content;
/// - whatever decodes canonically is what the encoder writes for it (compressed XOR compared by its stream);
/// - a column payload that decodes gives values that its encoder writes back to the same payload whenever the
///   payload was in that encoder's form.
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
    let _ = Block::decode_canonical(data);

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
