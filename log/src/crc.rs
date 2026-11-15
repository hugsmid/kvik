//! CRC32C (Castagnoli), the checksum of every kvik file (`format.md` A, common rules).
//!
//! All checksums go through this module, so the implementation can be swapped in one place.

use std::collections::VecDeque;

use crc::{CRC_32_ISCSI, Crc, Digest, Table};

static CASTAGNOLI: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

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

/// `a · b` modulo the polynomial, both in the reflected bit order.
const fn mul(a: u32, mut b: u32) -> u32 {
    let mut m = 1u32 << 31;
    let mut p = 0;
    while m != 0 {
        if a & m != 0 {
            p ^= b;
        }
        m >>= 1;
        b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
    }
    p
}

/// `x^(8 · 2^k)` modulo the polynomial, for `k` in `0..64`.
static BYTE_POWERS: [u32; 64] = {
    // x^8 in the reflected bit order.
    let mut t = [0u32; 64];
    t[0] = 1 << 23;
    let mut k = 1;
    while k < 64 {
        t[k] = mul(t[k - 1], t[k - 1]);
        k += 1;
    }
    t
};

/// The CRC32C that `crc` (of some bytes `a`) contributes to the CRC32C of `a` followed by `len` more bytes:
/// `crc32c(a ‖ b) = crc32c_shift(crc32c(a), b.len()) ^ crc32c(b)`. Costs O(log `len`) and reads no bytes.
pub fn crc32c_shift(crc: u32, len: u64) -> u32 {
    let mut p = crc;
    let mut n = len;
    let mut k = 0;
    while n != 0 {
        if n & 1 != 0 {
            p = mul(BYTE_POWERS[k], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

/// Slice-by-8 tables for the register update of [`PrefixCrc`].
static SLICES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            j += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let c = t[k - 1][i];
            t[k][i] = (c >> 8) ^ t[0][(c & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

fn step8(reg: u32, word: [u8; 8]) -> u32 {
    let x = u64::from_le_bytes(word) ^ u64::from(reg);
    let b = |i: u32| ((x >> (8 * i)) & 0xFF) as usize;
    SLICES[7][b(0)]
        ^ SLICES[6][b(1)]
        ^ SLICES[5][b(2)]
        ^ SLICES[4][b(3)]
        ^ SLICES[3][b(4)]
        ^ SLICES[2][b(5)]
        ^ SLICES[1][b(6)]
        ^ SLICES[0][b(7)]
}

/// CRC32C of byte ranges of one file, derived from a table of prefix checksums at 8-byte offsets
/// (`format.md` A, common rules: scan bound, Q-110).
///
/// The table runs forward from the first range asked for; a range costs its unaligned tail (at most 7 bytes) and
/// O(log length) arithmetic, plus the bytes the table has to grow by. Asked in rising order, as the scans do, each
/// byte enters the table at most once. [`PrefixCrc::forget_below`] bounds its memory to the span still asked for.
#[derive(Clone)]
pub struct PrefixCrc<'a> {
    bytes: &'a [u8],
    base: u64,
    sums: VecDeque<u32>,
    reg: u32,
    checksummed: u64,
}

impl std::fmt::Debug for PrefixCrc<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixCrc")
            .field("base", &self.base)
            .field("entries", &self.sums.len())
            .field("checksummed", &self.checksummed)
            .finish()
    }
}

impl<'a> PrefixCrc<'a> {
    /// An empty table over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            base: 0,
            sums: VecDeque::new(),
            reg: !0,
            checksummed: 0,
        }
    }

    /// Bytes checksummed so far: the table's growth and the unaligned tails.
    pub fn checksummed(&self) -> u64 {
        self.checksummed
    }

    /// Drops the table below `offset`; ranges starting below it cost their whole length again.
    pub fn forget_below(&mut self, offset: u64) {
        while self.sums.len() > 1 && self.base < offset {
            self.sums.pop_front();
            self.base += 8;
        }
    }

    fn top(&self) -> u64 {
        self.base + 8 * (self.sums.len() as u64).saturating_sub(1)
    }

    /// CRC32C of the `len` bytes at `start`, or `None` if they do not lie in the file.
    pub fn range(&mut self, start: u64, len: u64) -> Option<u32> {
        let end = start.checked_add(len)?;
        if end > self.bytes.len() as u64 {
            return None;
        }
        if !start.is_multiple_of(8) {
            self.checksummed += len;
            return Some(crc32c(crate::bytes::slice(self.bytes, start, len)?));
        }
        let aligned = start + (len & !7);
        if self.sums.is_empty() || start < self.base || start > self.top() {
            self.base = start;
            self.sums.clear();
            self.sums.push_back(0);
            self.reg = !0;
        }
        while self.top() < aligned {
            let at = self.top();
            let word = crate::bytes::slice(self.bytes, at, 8)?;
            self.reg = step8(self.reg, word.try_into().ok()?);
            self.sums.push_back(!self.reg);
            self.checksummed += 8;
        }
        let index = |o: u64| ((o - self.base) / 8) as usize;
        let head =
            self.sums[index(aligned)] ^ crc32c_shift(self.sums[index(start)], aligned - start);
        let tail = end - aligned;
        if tail == 0 {
            return Some(head);
        }
        self.checksummed += tail;
        let rest = crc32c(crate::bytes::slice(self.bytes, aligned, tail)?);
        Some(crc32c_shift(head, tail) ^ rest)
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

    #[test]
    fn rising_ranges_enter_each_byte_into_the_table_once() {
        let bytes: Vec<u8> = (0..1u32 << 16).map(|i| (i * 7 + 3) as u8).collect();
        let mut t = PrefixCrc::new(&bytes);
        for s in (0..bytes.len() as u64 - 4_096).step_by(8) {
            assert_eq!(
                t.range(s, 4_096),
                Some(crc32c(&bytes[s as usize..s as usize + 4_096]))
            );
            t.forget_below(s);
        }
        assert!(t.checksummed() <= bytes.len() as u64);
    }
}
