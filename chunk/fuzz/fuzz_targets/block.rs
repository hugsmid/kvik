//! Arbitrary bytes as a chunk block body and as every column payload (`docs/format-chunk.md` C.11): no input may
//! panic, read out of bounds or allocate more than its row count allows.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_chunk::fuzz::decode_all(data);
});
