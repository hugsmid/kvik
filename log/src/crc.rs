//! CRC32C (Castagnoli), the checksum of every kvik file (`format.md` A, common rules).
//!
//! All checksums go through this module, so the implementation can be swapped in one place.

use crc::{CRC_32_ISCSI, Crc, Table};

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

/// An incremental CRC32C over several slices: short slices, such as the header fields a batch checksum covers, by
/// slice-by-8 steps on the register, long ones by the crate's tables.
pub(crate) struct Crc32c(u32);

impl Crc32c {
    pub(crate) fn new() -> Self {
        Self(!0)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        BYTES_HASHED.with(|c| c.set(c.get() + bytes.len() as u64));
        if bytes.len() >= 64 {
            let mut d = CASTAGNOLI.digest_with_initial(self.0.reverse_bits());
            d.update(bytes);
            self.0 = !d.finalize();
            return;
        }
        let (words, rest) = bytes.as_chunks::<8>();
        for &w in words {
            self.0 = step8(self.0, w);
        }
        for &b in rest {
            self.0 = (self.0 >> 8) ^ SLICES[0][((self.0 ^ u32::from(b)) & 0xFF) as usize];
        }
    }

    pub(crate) fn finish(self) -> u32 {
        !self.0
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
static BYTE_POWERS: [u32; 64] = POWERS;

/// Hex digits of a shift length that [`SHIFT_TABLES`] covers: lengths below 4 GiB.
const SHIFT_DIGITS: usize = 8;

/// `x^(8 · len)` modulo the polynomial.
const fn byte_power(len: u64) -> u32 {
    // 1 in the reflected bit order.
    let mut p = 1 << 31;
    let mut n = len;
    let mut k = 0;
    while n != 0 {
        if n & 1 != 0 {
            p = mul(POWERS[k], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

const POWERS: [u32; 64] = {
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

/// Nibble tables of multiplication by `x^(8 · d · 16^k)`, for `k` in `0..SHIFT_DIGITS` and `d` in `1..16`: a shift
/// applies one per non-zero hex digit of its length, at most 8 lookups each (Q-873).
static SHIFT_TABLES: [[[[u32; 16]; 8]; 15]; SHIFT_DIGITS] = {
    let mut t = [[[[0u32; 16]; 8]; 15]; SHIFT_DIGITS];
    let mut k = 0;
    while k < SHIFT_DIGITS {
        let mut d = 1;
        while d < 16 {
            let a = byte_power((d as u64) << (4 * k));
            let mut nib = 0;
            while nib < 8 {
                let mut bit = 0;
                let mut cols = [0u32; 4];
                while bit < 4 {
                    cols[bit] = mul(a, 1 << (4 * nib + bit));
                    bit += 1;
                }
                let mut v = 1;
                while v < 16 {
                    let low = v & (v - 1);
                    t[k][d - 1][nib][v] =
                        t[k][d - 1][nib][low] ^ cols[(v ^ low).trailing_zeros() as usize];
                    v += 1;
                }
                nib += 1;
            }
            d += 1;
        }
        k += 1;
    }
    t
};

fn apply(t: &[[u32; 16]; 8], p: u32) -> u32 {
    let n = |i: u32| ((p >> (4 * i)) & 15) as usize;
    (t[0][n(0)] ^ t[1][n(1)])
        ^ (t[2][n(2)] ^ t[3][n(3)])
        ^ ((t[4][n(4)] ^ t[5][n(5)]) ^ (t[6][n(6)] ^ t[7][n(7)]))
}

/// The CRC32C that `crc` (of some bytes `a`) contributes to the CRC32C of `a` followed by `len` more bytes:
/// `crc32c(a ‖ b) = crc32c_shift(crc32c(a), b.len()) ^ crc32c(b)`. Reads no bytes: below 4 GiB, at most 8 table
/// applications (Q-873).
pub fn crc32c_shift(crc: u32, len: u64) -> u32 {
    let mut p = crc;
    let mut n = len;
    for table in &SHIFT_TABLES {
        if n == 0 {
            return p;
        }
        let d = (n & 15) as usize;
        if d != 0 {
            p = apply(&table[d - 1], p);
        }
        n >>= 4;
    }
    let mut k = 4 * SHIFT_DIGITS;
    while n != 0 {
        if n & 1 != 0 {
            p = mul(BYTE_POWERS[k], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

/// [`crc32c_shift`] by squaring, one bit of `len` at a time: the reference for its tables.
#[doc(hidden)]
pub fn crc32c_shift_by_squaring(crc: u32, len: u64) -> u32 {
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

/// CRC32C of the first 12 bytes of a block header: one slice-by-8 step and one slice-by-4 step.
pub(crate) fn header_crc32c(b: &[u8; 16]) -> u32 {
    #[cfg(test)]
    BYTES_HASHED.with(|c| c.set(c.get() + 12));
    let mut first = [0u8; 8];
    first.copy_from_slice(&b[..8]);
    let reg = step8(!0, first) ^ u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    let x = |i: u32| ((reg >> (8 * i)) & 0xFF) as usize;
    !(SLICES[3][x(0)] ^ SLICES[2][x(1)] ^ SLICES[1][x(2)] ^ SLICES[0][x(3)])
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
/// O(log length) arithmetic, plus the bytes the table has to grow by. Asked at rising starts, as the scans do, each
/// byte enters the table at most once. [`PrefixCrc::forget_below`] bounds its memory to the span still asked for.
#[derive(Clone)]
pub struct PrefixCrc<'a> {
    bytes: &'a [u8],
    /// The offset of `sums[front]`.
    base: u64,
    sums: Vec<u32>,
    /// Entries forgotten at the front of `sums`, dropped once they are half of it.
    front: usize,
    reg: u32,
    checksummed: u64,
}

impl std::fmt::Debug for PrefixCrc<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixCrc")
            .field("base", &self.base)
            .field("entries", &(self.sums.len() - self.front))
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
            sums: Vec::new(),
            front: 0,
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
        let live = (self.sums.len() - self.front) as u64;
        let k = (offset.saturating_sub(self.base).div_ceil(8)).min(live.saturating_sub(1));
        self.front += k as usize;
        self.base += 8 * k;
        if self.front > 4_096 && 2 * self.front > self.sums.len() {
            self.sums.drain(..self.front);
            self.front = 0;
        }
    }

    fn top(&self) -> u64 {
        self.base + 8 * ((self.sums.len() - self.front) as u64).saturating_sub(1)
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
        if self.sums.len() == self.front || start < self.base {
            self.base = start;
            self.sums.clear();
            self.front = 0;
            self.sums.push(0);
            self.reg = !0;
        }
        let top = self.top();
        if top < aligned {
            let words = crate::bytes::slice(self.bytes, top, aligned - top)?;
            self.sums.reserve(words.len() / 8);
            let mut reg = self.reg;
            for &word in words.as_chunks::<8>().0 {
                reg = step8(reg, word);
                self.sums.push(!reg);
            }
            self.reg = reg;
            self.checksummed += aligned - top;
            #[cfg(test)]
            BYTES_HASHED.with(|c| c.set(c.get() + aligned - top));
        }
        let index = |o: u64| self.front + ((o - self.base) / 8) as usize;
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
    fn incremental_over_short_and_long_slices_in_any_order_equals_whole() {
        let bytes: Vec<u8> = (0..4_096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        for cuts in [
            &[0usize, 1, 9, 73, 80, 200, 1_000][..],
            &[5, 6, 70, 71, 140, 4_000],
            &[64, 128, 129],
        ] {
            let mut c = Crc32c::new();
            let mut at = 0;
            for &cut in cuts.iter().chain(&[bytes.len()]) {
                c.update(&bytes[at..cut]);
                at = cut;
            }
            assert_eq!(c.finish(), crc32c(&bytes), "{cuts:?}");
        }
    }

    #[test]
    fn the_table_shift_equals_the_shift_by_squaring_for_every_digit_and_beyond_4_gib() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for k in 0..64 {
            for d in 0..16u64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let len = (d << (4 * (k % 16))) | (x >> (64 - k.max(1)));
                let crc = x as u32;
                assert_eq!(
                    crc32c_shift(crc, len),
                    crc32c_shift_by_squaring(crc, len),
                    "{len}"
                );
            }
        }
        for len in [
            0,
            1,
            7,
            8,
            15,
            16,
            255,
            256,
            u64::from(u32::MAX),
            1 << 32,
            u64::MAX,
        ] {
            assert_eq!(
                crc32c_shift(!0, len),
                crc32c_shift_by_squaring(!0, len),
                "{len}"
            );
        }
    }

    #[test]
    fn the_header_checksum_equals_crc32c_of_the_first_12_bytes() {
        let mut b = [0u8; 16];
        for i in 0..4096u32 {
            for (j, v) in b.iter_mut().enumerate() {
                *v = (i.wrapping_mul(2_654_435_761) >> (j % 24)) as u8 ^ (j as u8).wrapping_mul(37);
            }
            assert_eq!(header_crc32c(&b), crc32c(&b[..12]));
        }
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

    #[test]
    fn forgetting_below_an_offset_keeps_the_table_from_that_offset() {
        let bytes: Vec<u8> = (0..256u32).map(|i| (i * 13 + 1) as u8).collect();
        let mut t = PrefixCrc::new(&bytes);
        assert_eq!(t.range(0, 128), Some(crc32c(&bytes[..128])));
        t.forget_below(64);
        assert_eq!(t.range(64, 32), Some(crc32c(&bytes[64..96])));
        assert_eq!(t.checksummed(), 128);
    }

    #[test]
    fn a_range_off_an_8_byte_boundary_is_checksummed_directly_and_counted() {
        let bytes: Vec<u8> = (0..64u32).map(|i| (i * 7 + 5) as u8).collect();
        let mut t = PrefixCrc::new(&bytes);
        assert_eq!(t.range(3, 10), Some(crc32c(&bytes[3..13])));
        assert_eq!(t.checksummed(), 10);
    }
}
