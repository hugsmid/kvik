//! Arbitrary bytes as a chunk index object (`docs/format-compacted.md` B.2): no input may panic, read out of bounds or
//! allocate from an unchecked length.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_chunk::fuzz::decode_index_object(data);
});
