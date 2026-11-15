//! The entry point of the fuzz target (`fuzz/`), also run on stable by `tests/fuzz_inputs.rs`.

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
