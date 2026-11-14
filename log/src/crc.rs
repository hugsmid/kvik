//! CRC32C (Castagnoli), the checksum of every kvik file (`format.md` A, common rules).
//!
//! All checksums go through this module, so the implementation can be swapped in one place.

use crc::{CRC_32_ISCSI, Crc, Digest, Table};

static CASTAGNOLI: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);

/// CRC32C of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    #[cfg(test)]
    BYTES_HASHED.with(|c| c.set(c.get() + bytes.len() as u64));
    CASTAGNOLI.checksum(bytes)
}

#[cfg(test)]
thread_local! {
    /// Bytes checksummed on this thread, for tests of how much a scan hashes.
    pub(crate) static BYTES_HASHED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// An incremental CRC32C over several slices.
pub(crate) struct Crc32c(Digest<'static, u32, Table<16>>);

impl Crc32c {
    pub(crate) fn new() -> Self {
        Self(CASTAGNOLI.digest())
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        BYTES_HASHED.with(|c| c.set(c.get() + bytes.len() as u64));
        self.0.update(bytes);
    }

    pub(crate) fn finish(self) -> u32 {
        self.0.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer() {
        // The check value of CRC-32C (RFC 3720, appendix B.4).
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    }

    #[test]
    fn incremental_equals_whole() {
        let mut c = Crc32c::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), crc32c(b"123456789"));
    }
}
