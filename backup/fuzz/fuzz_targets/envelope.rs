//! Arbitrary bytes as a sealed object and as a plaintext (`format.md` A.17): no input may panic, and nothing is
//! allocated beyond the input's own length.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kvik_backup::envelope::fuzz::open_all(data);
});
