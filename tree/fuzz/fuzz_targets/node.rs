//! Arbitrary bytes as a stored node (`docs/format-tree.md` T.6): no input may panic or allocate from an unchecked
//! length, and whatever decodes must encode again to the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_tree::fuzz::decode_node(data);
});
