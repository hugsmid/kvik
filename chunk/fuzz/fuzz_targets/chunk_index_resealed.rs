//! Arbitrary bytes re-sealed as a file (`docs/format-compacted.md`), so that edits reach the reader's checks behind
//! the checksum chain: no input may panic, read out of bounds or allocate from an unchecked length.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_chunk::fuzz::decode_index_object_resealed(data);
});
