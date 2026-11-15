//! Bounded byte and bit readers, LEB128, zigzag and bit packing. Every read is checked against the end of its
//! payload; nothing here indexes without a bound or allocates from a value it reads.

use crate::error::ColumnError;

/// A cursor over one payload.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, at: 0 }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], ColumnError> {
        let end = self.at.checked_add(n).ok_or(ColumnError::Truncated)?;
        let b = self.buf.get(self.at..end).ok_or(ColumnError::Truncated)?;
        self.at = end;
        Ok(b)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ColumnError> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, ColumnError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ColumnError> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.bytes(4)?);
        Ok(u32::from_le_bytes(a))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ColumnError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.bytes(8)?);
        Ok(u64::from_le_bytes(a))
    }

    /// Minimal unsigned LEB128 of at most `max_len` bytes whose value fits `u64`.
    pub(crate) fn leb128(&mut self, max_len: usize) -> Result<u64, ColumnError> {
        let mut v: u64 = 0;
        for i in 0..max_len.min(10) {
            let b = self.u8().map_err(|_| ColumnError::Truncated)?;
            let low = u64::from(b & 0x7F);
            // The tenth byte may carry only bit 63.
            if i == 9 && low > 1 {
                return Err(ColumnError::Leb128);
            }
            v |= low << (7 * i);
            if b & 0x80 == 0 {
                if i > 0 && b == 0 {
                    return Err(ColumnError::Leb128);
                }
                return Ok(v);
            }
        }
        Err(ColumnError::Leb128)
    }

    pub(crate) fn position(&self) -> usize {
        self.at
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.at == self.buf.len()
    }

    pub(crate) fn finish(&self) -> Result<(), ColumnError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(ColumnError::TrailingBytes)
        }
    }
}

/// Appends `v` as minimal unsigned LEB128.
pub(crate) fn put_leb128(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// The length of `v` as minimal unsigned LEB128.
pub(crate) fn leb128_len(v: u64) -> usize {
    let bits = 64 - v.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

#[inline]
pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub(crate) fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// The number of bits needed for `v` (0 for 0).
#[inline]
pub(crate) fn bit_width(v: u64) -> u32 {
    64 - v.leading_zeros()
}

#[inline]
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Packs `values` at `width` bits each, least significant bit first, into `ceil(n × width / 8)` bytes; the
/// padding bits of the last byte are zero. Every value must fit `width`.
pub(crate) fn pack(out: &mut Vec<u8>, values: impl Iterator<Item = u64>, width: u32) {
    if width == 0 {
        return;
    }
    let mut acc: u128 = 0;
    let mut n: u32 = 0;
    for v in values {
        debug_assert!(v & !mask(width) == 0);
        acc |= u128::from(v) << n;
        n += width;
        while n >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            n -= 8;
        }
    }
    if n > 0 {
        out.push(acc as u8);
    }
}

/// Reads `count` values packed by [`pack`] at `width` bits from `r`, calling `f` for each; checks that the padding
/// bits are zero.
pub(crate) fn unpack(
    r: &mut Reader<'_>,
    count: usize,
    width: u32,
    mut f: impl FnMut(u64),
) -> Result<(), ColumnError> {
    if width > 64 {
        return Err(ColumnError::Width);
    }
    if width == 0 {
        for _ in 0..count {
            f(0);
        }
        return Ok(());
    }
    let total_bits = count as u64 * u64::from(width);
    let len = usize::try_from(total_bits.div_ceil(8)).map_err(|_| ColumnError::Truncated)?;
    let bytes = r.bytes(len)?;
    let mut acc: u128 = 0;
    let mut n: u32 = 0;
    let mut it = bytes.iter();
    let m = mask(width);
    for _ in 0..count {
        while n < width {
            // `len` covers every bit read here.
            let b = it.next().copied().unwrap_or(0);
            acc |= u128::from(b) << n;
            n += 8;
        }
        f((acc as u64) & m);
        acc >>= width;
        n -= width;
    }
    if acc != 0 {
        return Err(ColumnError::Padding);
    }
    Ok(())
}

/// Writes bit fields most significant bit first (the XOR stream, C.6).
#[derive(Default)]
pub(crate) struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    /// Writes the low `len` bits of `v` (`len <= 64`).
    pub(crate) fn write(&mut self, v: u64, len: u32) {
        if len == 0 {
            return;
        }
        let v = v & mask(len);
        let free = 64 - self.n;
        if len < free {
            self.acc = (self.acc << len) | v;
            self.n += len;
        } else {
            let hi = len - free;
            let top = if free == 64 { 0 } else { self.acc << free };
            self.acc = top | (v >> hi);
            self.buf.extend_from_slice(&self.acc.to_be_bytes());
            self.acc = v & mask(hi);
            self.n = hi;
        }
    }

    pub(crate) fn bit(&mut self, b: bool) {
        self.write(u64::from(b), 1);
    }

    /// The bytes written, the last one padded with zero bits.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            let bytes = self.n.div_ceil(8) as usize;
            let v = self.acc << (64 - self.n);
            self.buf.extend_from_slice(&v.to_be_bytes()[..bytes]);
        }
        self.buf
    }
}

/// Reads a stream written by [`BitWriter`]; a read past the end is an error.
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    pos: u64,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// Reads `len` bits (`len <= 64`).
    pub(crate) fn read(&mut self, len: u32) -> Result<u64, ColumnError> {
        if len == 0 {
            return Ok(0);
        }
        let end = self.pos + u64::from(len);
        if end > self.buf.len() as u64 * 8 {
            return Err(ColumnError::Truncated);
        }
        let byte = (self.pos >> 3) as usize;
        let off = (self.pos & 7) as u32;
        let mut tmp = [0u8; 16];
        let avail = (self.buf.len() - byte).min(16);
        tmp[..avail].copy_from_slice(&self.buf[byte..byte + avail]);
        let w = u128::from_be_bytes(tmp);
        self.pos = end;
        Ok(((w << off) >> (128 - len)) as u64)
    }

    pub(crate) fn bit(&mut self) -> Result<bool, ColumnError> {
        Ok(self.read(1)? == 1)
    }

    /// Checks that fewer than 8 bits are left and that they are zero.
    pub(crate) fn finish(&self) -> Result<(), ColumnError> {
        let total = self.buf.len() as u64 * 8;
        if total - self.pos >= 8 {
            return Err(ColumnError::TrailingBytes);
        }
        let rest = (total - self.pos) as u32;
        if rest > 0 {
            let last = self.buf[self.buf.len() - 1];
            if last & ((1u8 << rest) - 1) != 0 {
                return Err(ColumnError::Padding);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip() {
        let fields: Vec<(u64, u32)> = (0..2000u64)
            .map(|i| {
                let len = (i * 7 % 64 + 1) as u32;
                (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) & mask(len), len)
            })
            .collect();
        let mut w = BitWriter::default();
        for &(v, l) in &fields {
            w.write(v, l);
        }
        let buf = w.finish();
        let mut r = BitReader::new(&buf);
        for &(v, l) in &fields {
            assert_eq!(r.read(l).unwrap(), v);
        }
        r.finish().unwrap();
        assert_eq!(r.read(8), Err(ColumnError::Truncated));
    }

    #[test]
    fn leb128_rules() {
        for v in [0u64, 1, 127, 128, 300, 1 << 35, u64::MAX] {
            let mut b = Vec::new();
            put_leb128(&mut b, v);
            assert_eq!(b.len(), leb128_len(v));
            let mut r = Reader::new(&b);
            assert_eq!(r.leb128(10).unwrap(), v);
            assert!(r.is_empty());
        }
        assert_eq!(
            Reader::new(&[0x80, 0x00]).leb128(10),
            Err(ColumnError::Leb128)
        );
        assert_eq!(Reader::new(&[0x80]).leb128(10), Err(ColumnError::Truncated));
        assert_eq!(
            Reader::new(&[0x80, 0x80, 0x01]).leb128(2),
            Err(ColumnError::Leb128)
        );
        let mut over = vec![0xFF; 9];
        over.push(0x02);
        assert_eq!(Reader::new(&over).leb128(10), Err(ColumnError::Leb128));
    }

    #[test]
    fn pack_round_trip_and_padding() {
        for width in 0..=64u32 {
            let vals: Vec<u64> = (0..37u64)
                .map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) & mask(width))
                .collect();
            let mut b = Vec::new();
            pack(&mut b, vals.iter().copied(), width);
            assert_eq!(b.len() as u64, (37 * u64::from(width)).div_ceil(8));
            let mut got = Vec::new();
            let mut r = Reader::new(&b);
            unpack(&mut r, vals.len(), width, |v| got.push(v)).unwrap();
            assert!(r.is_empty());
            assert_eq!(got, vals);
        }
        let mut r = Reader::new(&[0b1000_0001]);
        assert_eq!(unpack(&mut r, 1, 3, |_| {}), Err(ColumnError::Padding));
    }

    #[test]
    fn zigzag_round_trip() {
        for v in [0i64, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }
}
