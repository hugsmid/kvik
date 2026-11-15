//! Arbitrary bytes as a manifest and as each record body (`format.md` A.11, A.14): no input may panic, read out of
//! bounds or allocate more than its declared lengths.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_files::fuzz::decode_all(data);
});
