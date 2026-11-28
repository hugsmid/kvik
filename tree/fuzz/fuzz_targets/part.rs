//! Arbitrary bytes as a tree part (`docs/format-tree.md` T.13), raw and with checksums recomputed so mutations reach
//! the body checks: no input may panic, and whatever decodes must encode again to the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_tree::fuzz::decode_part(data);
    let mut framed = data.to_vec();
    kvik_tree::fuzz::reframe(&mut framed);
    kvik_tree::fuzz::decode_part(&framed);
});
