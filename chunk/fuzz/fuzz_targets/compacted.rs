//! Arbitrary bytes as a compacted segment (`docs/format-compacted.md` B.1): no input may panic, read out of bounds or
//! allocate from an unchecked length; a file that opens extracts to a chunk index object that opens.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_chunk::fuzz::decode_compacted(data);
});
