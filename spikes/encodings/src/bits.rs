//! Bit streams (most significant bit first), LEB128 and zigzag helpers.

/// Writes a stream of bit fields, most significant bit first.
#[derive(Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    n: u32,
}

fn mask(len: u32) -> u64 {
    if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    }
}

impl BitWriter {
    /// A new, empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the low `len` bits of `v` (`len <= 64`).
    pub fn write(&mut self, v: u64, len: u32) {
        if len == 0 {
            return;
        }
        let v = v & mask(len);
        let free = 64 - self.n;
        if len <= free {
            self.acc = if len == 64 { v } else { (self.acc << len) | v };
            self.n += len;
        } else {
            let hi = len - free;
            self.acc = (self.acc << free) | (v >> hi);
            self.buf.extend_from_slice(&self.acc.to_be_bytes());
            self.acc = v & mask(hi);
            self.n = hi;
        }
        if self.n == 64 {
            self.buf.extend_from_slice(&self.acc.to_be_bytes());
            self.acc = 0;
            self.n = 0;
        }
    }

    /// Writes one bit.
    pub fn bit(&mut self, b: bool) {
        self.write(u64::from(b), 1);
    }

    /// Finishes the stream, padding the last byte with zeros.
    pub fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            let bytes = self.n.div_ceil(8) as usize;
            let v = self.acc << (64 - self.n);
            self.buf.extend_from_slice(&v.to_be_bytes()[..bytes]);
        }
        self.buf
    }
}

/// Reads a stream written by [`BitWriter`].
pub struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    /// Reads `len` bits (`len <= 64`).
    #[inline]
    pub fn read(&mut self, len: u32) -> u64 {
        if len == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let off = (self.pos & 7) as u32;
        let w = if byte + 16 <= self.buf.len() {
            u128::from_be_bytes(self.buf[byte..byte + 16].try_into().expect("16 bytes"))
        } else {
            let mut tmp = [0u8; 16];
            let avail = self.buf.len().saturating_sub(byte).min(16);
            tmp[..avail].copy_from_slice(&self.buf[byte..byte + avail]);
            u128::from_be_bytes(tmp)
        };
        self.pos += len as usize;
        ((w << off) >> (128 - len)) as u64
    }

    /// Reads one bit.
    #[inline]
    pub fn bit(&mut self) -> bool {
        self.read(1) == 1
    }

    /// Counts leading one bits, consuming them and the terminating zero, up to
    /// `max` ones (no terminator is read after `max` ones).
    #[inline]
    pub fn ones(&mut self, max: u32) -> u32 {
        let mut n = 0;
        while n < max && self.bit() {
            n += 1;
        }
        n
    }
}

/// Zigzag-encodes a signed integer.
#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Reverses [`zigzag`].
#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Appends `v` as unsigned LEB128.
#[inline]
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads unsigned LEB128 at `*pos`, advancing it.
#[inline]
pub fn get_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
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
        let mut w = BitWriter::new();
        for &(v, l) in &fields {
            w.write(v, l);
        }
        let buf = w.finish();
        let mut r = BitReader::new(&buf);
        for &(v, l) in &fields {
            assert_eq!(r.read(l), v);
        }
    }

    #[test]
    fn varint_and_zigzag() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN, 1 << 40] {
            assert_eq!(unzigzag(zigzag(v)), v);
            let mut b = Vec::new();
            put_varint(&mut b, zigzag(v));
            let mut p = 0;
            assert_eq!(unzigzag(get_varint(&b, &mut p)), v);
            assert_eq!(p, b.len());
        }
    }
}
