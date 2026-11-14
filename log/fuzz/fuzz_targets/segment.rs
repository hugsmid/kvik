//! Arbitrary bytes as a log segment, a mark file and every body type (`format.md` A.11): no input may panic, read
//! out of bounds or allocate more than its declared lengths.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_log::fuzz::decode_all(data);
});
