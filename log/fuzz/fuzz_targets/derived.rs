//! Arbitrary bytes as each derived file (`format.md` A.18): no input may panic, read out of bounds or allocate more
//! than its declared, checked lengths.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_log::fuzz::decode_derived(data);
});
