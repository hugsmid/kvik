//! Arbitrary bytes as each body of a compacted segment and a chunk index object (`docs/format-compacted.md` B.1, B.2):
//! whatever decodes encodes and decodes again to itself.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_chunk::fuzz::decode_file_bodies(data);
});
