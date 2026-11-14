//! Candidate own chunk encodings, in a blocked container.
//!
//! A chunk is a header, a block directory and blocks of [`BLOCK_ROWS`] rows:
//!
//! ```text
//! chunk  = rows: u32, kind: u8, block_count: u32,
//!          block_count × (first_t: i64, last_t: i64, offset: u32),
//!          blocks
//! block  = 5 × (length: u32, payload)        t, quality, flags, value, position
//! zstd payload = raw_length: u32, zstd frame
//! ```
//!
//! Each block decodes on its own, so a range scan decodes only the blocks
//! whose `[first_t, last_t]` overlaps the range. The value column holds values
//! only for rows without [`FLAG_NO_VALUE`]; the flags column says which.
//!
//! Column encodings by [`Scheme`] (`t` and position by [`Ints`] unless plain):
//!
//! | Column | `Plain` | `Xor` | `Decimal` |
//! |---|---|---|---|
//! | `t`, position | little-endian | [`Ints::Dod`] or [`Ints::Delta`] | as `Xor` |
//! | value (f64) | little-endian bits | XOR with the previous value | scaled integer, deltas bit-packed per 128; XOR fallback |
//! | value (state) | little-endian | run-length | run-length |
//! | quality, flags | little-endian | run-length | run-length |

use std::cell::RefCell;

use crate::bits::{BitReader, BitWriter, get_varint, put_varint, unzigzag, zigzag};
use crate::{BLOCK_ROWS, Chunk, Codec, ColumnSizes, FLAG_NO_VALUE, ValueKind, ZSTD_LEVEL};

/// How columns are encoded before optional zstd.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// Fixed-width little-endian columns.
    Plain,
    /// Delta-of-delta, Gorilla-style XOR, run-length.
    Xor,
    /// As `Xor`, but f64 values as scaled decimals where that is exact.
    Decimal,
}

/// How `t` and position are encoded (except in [`Scheme::Plain`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ints {
    /// Delta-of-delta with Gorilla-style bucketed bit codes.
    Dod,
    /// Delta, frame-of-reference bit-packed per 128 values.
    Delta,
}

/// An own encoding: a scheme and whether each column payload is then
/// compressed with zstd.
#[derive(Clone, Copy, Debug)]
pub struct Own {
    /// Column scheme.
    pub scheme: Scheme,
    /// Integer coding for `t` and position.
    pub ints: Ints,
    /// Compress each column payload of each block with zstd.
    pub zstd: bool,
    name: &'static str,
}

impl Own {
    /// (a) Plain little-endian columns, zstd.
    pub const PLAIN_ZSTD: Own = Own {
        scheme: Scheme::Plain,
        ints: Ints::Delta,
        zstd: true,
        name: "plain + zstd",
    };
    /// (b) Delta-of-delta (Gorilla buckets), XOR, run-length.
    pub const DOD_XOR_RLE: Own = Own {
        scheme: Scheme::Xor,
        ints: Ints::Dod,
        zstd: false,
        name: "dod/XOR/RLE",
    };
    /// (b) Delta (bit-packed), XOR, run-length.
    pub const DELTA_XOR_RLE: Own = Own {
        scheme: Scheme::Xor,
        ints: Ints::Delta,
        zstd: false,
        name: "delta/XOR/RLE",
    };
    /// (b) with zstd.
    pub const DELTA_XOR_RLE_ZSTD: Own = Own {
        scheme: Scheme::Xor,
        ints: Ints::Delta,
        zstd: true,
        name: "delta/XOR/RLE + zstd",
    };
    /// (d) Delta (bit-packed), scaled decimal, run-length.
    pub const DELTA_DECIMAL_RLE: Own = Own {
        scheme: Scheme::Decimal,
        ints: Ints::Delta,
        zstd: false,
        name: "delta/decimal/RLE",
    };
    /// (d) with zstd.
    pub const DELTA_DECIMAL_RLE_ZSTD: Own = Own {
        scheme: Scheme::Decimal,
        ints: Ints::Delta,
        zstd: true,
        name: "delta/decimal/RLE + zstd",
    };
}

/// A set of encoded chunks, as one file would hold them.
pub struct OwnFile {
    chunks: Vec<Vec<u8>>,
    sizes: Vec<ColumnSizes>,
}

/// Per-chunk directory entry in a file (offset, length, series, rows).
const FILE_DIRECTORY_ENTRY: u64 = 24;
const CHUNK_HEADER: usize = 9;
const BLOCK_ENTRY: usize = 20;

thread_local! {
    static ZSTD: RefCell<(zstd::bulk::Compressor<'static>, zstd::bulk::Decompressor<'static>)> =
        RefCell::new((
            zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("zstd compressor"),
            zstd::bulk::Decompressor::new().expect("zstd decompressor"),
        ));
}

fn compress(raw: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    let frame = ZSTD.with(|z| z.borrow_mut().0.compress(raw).expect("zstd compress"));
    out.extend_from_slice(&frame);
}

fn decompress(payload: &[u8], out: &mut Vec<u8>) {
    let raw_len = u32_at(payload, 0) as usize;
    out.clear();
    out.reserve(raw_len);
    let n = ZSTD.with(|z| {
        z.borrow_mut()
            .1
            .decompress_to_buffer(&payload[4..], out)
            .expect("zstd decompress")
    });
    assert_eq!(n, raw_len);
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

// ---- column codecs -------------------------------------------------------

fn write_bucket(w: &mut BitWriter, z: u64) {
    if z == 0 {
        w.write(0, 1);
    } else if z < 1 << 7 {
        w.write(0b10, 2);
        w.write(z, 7);
    } else if z < 1 << 9 {
        w.write(0b110, 3);
        w.write(z, 9);
    } else if z < 1 << 12 {
        w.write(0b1110, 4);
        w.write(z, 12);
    } else if z < 1 << 32 {
        w.write(0b11110, 5);
        w.write(z, 32);
    } else {
        w.write(0b11111, 5);
        w.write(z, 64);
    }
}

#[inline]
fn read_bucket(r: &mut BitReader<'_>) -> u64 {
    match r.ones(5) {
        0 => 0,
        1 => r.read(7),
        2 => r.read(9),
        3 => r.read(12),
        4 => r.read(32),
        _ => r.read(64),
    }
}

/// Delta-of-delta: the first value raw, then bucketed zigzag codes.
pub fn dod_encode(xs: &[i64]) -> Vec<u8> {
    let mut w = BitWriter::new();
    let Some((&first, rest)) = xs.split_first() else {
        return w.finish();
    };
    w.write(first as u64, 64);
    let (mut prev, mut prev_delta) = (first, 0i64);
    for &x in rest {
        let d = x.wrapping_sub(prev);
        write_bucket(&mut w, zigzag(d.wrapping_sub(prev_delta)));
        prev = x;
        prev_delta = d;
    }
    w.finish()
}

/// Reverses [`dod_encode`], appending `n` values to `out`.
pub fn dod_decode(buf: &[u8], n: usize, out: &mut Vec<i64>) {
    if n == 0 {
        return;
    }
    let mut r = BitReader::new(buf);
    let mut v = r.read(64) as i64;
    out.push(v);
    let mut d = 0i64;
    for _ in 1..n {
        d = d.wrapping_add(unzigzag(read_bucket(&mut r)));
        v = v.wrapping_add(d);
        out.push(v);
    }
}

/// Gorilla-style XOR of each value with the previous one.
pub fn xor_encode(vs: &[u64]) -> Vec<u8> {
    let mut w = BitWriter::new();
    let Some((&first, rest)) = vs.split_first() else {
        return w.finish();
    };
    w.write(first, 64);
    let mut prev = first;
    let (mut lead, mut trail) = (u32::MAX, 0u32);
    for &v in rest {
        let x = v ^ prev;
        prev = v;
        if x == 0 {
            w.bit(false);
            continue;
        }
        w.bit(true);
        let l = x.leading_zeros().min(31);
        let tz = x.trailing_zeros();
        if lead != u32::MAX && l >= lead && tz >= trail {
            w.bit(false);
            w.write(x >> trail, 64 - lead - trail);
        } else {
            w.bit(true);
            let sig = 64 - l - tz;
            w.write(u64::from(l), 5);
            w.write(u64::from(sig - 1), 6);
            w.write(x >> tz, sig);
            lead = l;
            trail = tz;
        }
    }
    w.finish()
}

/// Reverses [`xor_encode`], appending `n` values to `out`.
pub fn xor_decode(buf: &[u8], n: usize, out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    let mut r = BitReader::new(buf);
    let mut v = r.read(64);
    out.push(v);
    let (mut lead, mut trail) = (0u32, 0u32);
    for _ in 1..n {
        if r.bit() {
            let x = if r.bit() {
                lead = r.read(5) as u32;
                let sig = r.read(6) as u32 + 1;
                trail = 64 - lead - sig;
                r.read(sig) << trail
            } else {
                r.read(64 - lead - trail) << trail
            };
            v ^= x;
        }
        out.push(v);
    }
}

/// Run-length: pairs of LEB128 run length and LEB128 value.
pub fn rle_encode(vs: impl IntoIterator<Item = u64>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = vs.into_iter();
    let Some(mut cur) = it.next() else {
        return out;
    };
    let mut run = 1u64;
    for v in it {
        if v == cur {
            run += 1;
        } else {
            put_varint(&mut out, run);
            put_varint(&mut out, cur);
            cur = v;
            run = 1;
        }
    }
    put_varint(&mut out, run);
    put_varint(&mut out, cur);
    out
}

/// Reverses [`rle_encode`], calling `push` for each of `n` values.
pub fn rle_decode(buf: &[u8], n: usize, mut push: impl FnMut(u64, usize)) {
    let mut pos = 0;
    let mut done = 0;
    while done < n {
        let run = get_varint(buf, &mut pos) as usize;
        let v = get_varint(buf, &mut pos);
        push(v, run);
        done += run;
    }
}

const POW10: [f64; 16] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
];
const MINIBLOCK: usize = 128;

#[inline]
fn scaled(bits: u64, e: usize) -> Option<i64> {
    let v = f64::from_bits(bits);
    let n = (v * POW10[e]).round();
    if n.is_nan() || n.abs() >= 9.0e15 {
        return None;
    }
    let n = n as i64;
    ((n as f64 / POW10[e]).to_bits() == bits).then_some(n)
}

fn write_varbits(w: &mut BitWriter, v: u64) {
    let len = 64 - v.leading_zeros();
    w.write(u64::from(len), 7);
    w.write(v, len);
}

fn read_varbits(r: &mut BitReader<'_>) -> u64 {
    let len = r.read(7) as u32;
    r.read(len)
}

/// Scaled-decimal encoding: if every value is `n / 10^e` for an integer `n`
/// (bit-exact, same `e` for the block), a tag byte `1`, then `e`, the first
/// `n` and the deltas, frame-of-reference bit-packed per 128. Otherwise a tag
/// byte `0` and [`xor_encode`].
pub fn decimal_encode(vs: &[u64]) -> Vec<u8> {
    let found = (0..POW10.len()).find_map(|e| {
        let ns: Option<Vec<i64>> = vs.iter().map(|&b| scaled(b, e)).collect();
        ns.map(|ns| (e, ns))
    });
    let Some((e, ns)) = found else {
        let mut out = vec![0u8];
        out.extend(xor_encode(vs));
        return out;
    };
    let mut w = BitWriter::new();
    w.write(1, 8);
    w.write(e as u64, 8);
    pack_deltas(&mut w, &ns);
    w.finish()
}

/// Reverses [`decimal_encode`], appending `n` values to `out`.
pub fn decimal_decode(buf: &[u8], n: usize, out: &mut Vec<u64>) {
    if buf[0] == 0 {
        return xor_decode(&buf[1..], n, out);
    }
    let mut r = BitReader::new(&buf[1..]);
    let p = POW10[r.read(8) as usize];
    unpack_deltas(&mut r, n, |v| out.push((v as f64 / p).to_bits()));
}

/// Writes the first value, then the deltas frame-of-reference bit-packed per
/// [`MINIBLOCK`]: per mini-block the minimum delta and a bit width.
fn pack_deltas(w: &mut BitWriter, ns: &[i64]) {
    let Some(&first) = ns.first() else {
        return;
    };
    w.write(zigzag(first), 64);
    let mut deltas = [0i64; MINIBLOCK];
    let mut i = 1;
    while i < ns.len() {
        let m = (ns.len() - i).min(MINIBLOCK);
        let d = &mut deltas[..m];
        for (k, x) in d.iter_mut().enumerate() {
            *x = ns[i + k].wrapping_sub(ns[i + k - 1]);
        }
        i += m;
        let min = *d.iter().min().expect("non-empty");
        let max = *d.iter().max().expect("non-empty");
        let width = 64 - (max.wrapping_sub(min) as u64).leading_zeros();
        write_varbits(w, zigzag(min));
        w.write(u64::from(width), 7);
        for &x in d.iter() {
            w.write(x.wrapping_sub(min) as u64, width);
        }
    }
}

/// Reverses [`pack_deltas`] for `n` values.
#[inline]
fn unpack_deltas(r: &mut BitReader<'_>, n: usize, mut push: impl FnMut(i64)) {
    if n == 0 {
        return;
    }
    let mut v = unzigzag(r.read(64));
    push(v);
    let mut left = n - 1;
    while left > 0 {
        let min = unzigzag(read_varbits(r));
        let width = r.read(7) as u32;
        let m = left.min(MINIBLOCK);
        if width == 0 {
            for _ in 0..m {
                v = v.wrapping_add(min);
                push(v);
            }
        } else {
            for _ in 0..m {
                v = v.wrapping_add(min.wrapping_add(r.read(width) as i64));
                push(v);
            }
        }
        left -= m;
    }
}

/// Delta coding: the first value, then the deltas frame-of-reference
/// bit-packed per 128 (per mini-block the minimum delta and a bit width).
pub fn delta_encode(xs: &[i64]) -> Vec<u8> {
    let mut w = BitWriter::new();
    pack_deltas(&mut w, xs);
    w.finish()
}

/// Reverses [`delta_encode`], appending `n` values to `out`.
pub fn delta_decode(buf: &[u8], n: usize, out: &mut Vec<i64>) {
    let mut r = BitReader::new(buf);
    unpack_deltas(&mut r, n, |v| out.push(v));
}

// ---- chunk container -----------------------------------------------------

impl Own {
    fn encode_t(&self, xs: &[i64]) -> Vec<u8> {
        match self.scheme {
            Scheme::Plain => xs.iter().flat_map(|x| x.to_le_bytes()).collect(),
            _ if self.ints == Ints::Dod => dod_encode(xs),
            _ => delta_encode(xs),
        }
    }

    fn decode_t(&self, buf: &[u8], n: usize, out: &mut Vec<i64>) {
        match self.scheme {
            Scheme::Plain => out.extend(
                buf.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| i64::from_le_bytes(*b)),
            ),
            _ if self.ints == Ints::Dod => dod_decode(buf, n, out),
            _ => delta_decode(buf, n, out),
        }
    }

    fn encode_values(&self, kind: ValueKind, vs: &[u64]) -> Vec<u8> {
        match (self.scheme, kind) {
            (Scheme::Plain, _) => vs.iter().flat_map(|x| x.to_le_bytes()).collect(),
            (_, ValueKind::State) => rle_encode(vs.iter().copied()),
            (Scheme::Xor, ValueKind::F64) => xor_encode(vs),
            (Scheme::Decimal, ValueKind::F64) => decimal_encode(vs),
        }
    }

    fn decode_values(&self, kind: ValueKind, buf: &[u8], n: usize, out: &mut Vec<u64>) {
        match (self.scheme, kind) {
            (Scheme::Plain, _) => out.extend(
                buf.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| u64::from_le_bytes(*b)),
            ),
            (_, ValueKind::State) => rle_decode(buf, n, |v, run| out.extend((0..run).map(|_| v))),
            (Scheme::Xor, ValueKind::F64) => xor_decode(buf, n, out),
            (Scheme::Decimal, ValueKind::F64) => decimal_decode(buf, n, out),
        }
    }

    fn put_column(&self, raw: Vec<u8>, out: &mut Vec<u8>) -> u64 {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        if self.zstd {
            compress(&raw, out);
        } else {
            out.extend_from_slice(&raw);
        }
        let len = out.len() - start - 4;
        out[start..start + 4].copy_from_slice(&(len as u32).to_le_bytes());
        len as u64
    }

    fn encode_block(
        &self,
        c: &Chunk,
        rows: std::ops::Range<usize>,
        out: &mut Vec<u8>,
        s: &mut ColumnSizes,
    ) {
        let t = &c.t[rows.clone()];
        let flags = &c.flags[rows.clone()];
        let values: Vec<u64> = rows
            .clone()
            .filter(|&i| c.flags[i] & FLAG_NO_VALUE == 0)
            .map(|i| c.value[i])
            .collect();
        let quality = &c.quality[rows.clone()];
        let position = &c.position[rows];
        s.t += self.put_column(self.encode_t(t), out);
        let q = match self.scheme {
            Scheme::Plain => quality.to_vec(),
            _ => rle_encode(quality.iter().map(|&q| u64::from(q))),
        };
        s.quality_flags += self.put_column(q, out);
        let f = match self.scheme {
            Scheme::Plain => flags.iter().flat_map(|f| f.to_le_bytes()).collect(),
            _ => rle_encode(flags.iter().map(|&f| u64::from(f))),
        };
        s.quality_flags += self.put_column(f, out);
        s.value += self.put_column(self.encode_values(c.kind, &values), out);
        let p: Vec<i64> = position.iter().map(|&p| p as i64).collect();
        s.position += self.put_column(self.encode_t(&p), out);
        s.overhead += 5 * 4;
    }

    /// Encodes one chunk; returns the bytes and their split by column.
    pub fn encode_chunk(&self, c: &Chunk) -> (Vec<u8>, ColumnSizes) {
        let n = c.len();
        let blocks = n.div_ceil(BLOCK_ROWS);
        let mut out = Vec::with_capacity(CHUNK_HEADER + blocks * BLOCK_ENTRY + n * 4);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.push(match c.kind {
            ValueKind::F64 => 0,
            ValueKind::State => 1,
        });
        out.extend_from_slice(&(blocks as u32).to_le_bytes());
        let dir = out.len();
        out.resize(dir + blocks * BLOCK_ENTRY, 0);
        let mut s = ColumnSizes {
            overhead: (CHUNK_HEADER + blocks * BLOCK_ENTRY) as u64,
            ..ColumnSizes::default()
        };
        for b in 0..blocks {
            let rows = b * BLOCK_ROWS..((b + 1) * BLOCK_ROWS).min(n);
            let e = dir + b * BLOCK_ENTRY;
            out[e..e + 8].copy_from_slice(&c.t[rows.start].to_le_bytes());
            out[e + 8..e + 16].copy_from_slice(&c.t[rows.end - 1].to_le_bytes());
            let offset = out.len() as u32;
            out[e + 16..e + 20].copy_from_slice(&offset.to_le_bytes());
            self.encode_block(c, rows, &mut out, &mut s);
        }
        debug_assert_eq!(s.total(), out.len() as u64);
        (out, s)
    }

    fn column<'a>(&self, buf: &'a [u8], pos: &mut usize, scratch: &'a mut Vec<u8>) -> &'a [u8] {
        let len = u32_at(buf, *pos) as usize;
        let payload = &buf[*pos + 4..*pos + 4 + len];
        *pos += 4 + len;
        if self.zstd {
            decompress(payload, scratch);
            scratch
        } else {
            payload
        }
    }

    fn decode_block(&self, buf: &[u8], kind: ValueKind, offset: usize, n: usize, out: &mut Chunk) {
        let mut pos = offset;
        let mut scratch = Vec::new();
        let t = self.column(buf, &mut pos, &mut scratch);
        self.decode_t(t, n, &mut out.t);
        let q = self.column(buf, &mut pos, &mut scratch);
        match self.scheme {
            Scheme::Plain => out.quality.extend_from_slice(q),
            _ => rle_decode(q, n, |v, run| out.quality.extend((0..run).map(|_| v as u8))),
        }
        let f = self.column(buf, &mut pos, &mut scratch);
        let flags_start = out.flags.len();
        match self.scheme {
            Scheme::Plain => out
                .flags
                .extend(f.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b))),
            _ => rle_decode(f, n, |v, run| out.flags.extend((0..run).map(|_| v as u16))),
        }
        let flags = &out.flags[flags_start..];
        let vbuf = self.column(buf, &mut pos, &mut scratch);
        let missing = flags.iter().filter(|&&f| f & FLAG_NO_VALUE != 0).count();
        if missing == 0 {
            self.decode_values(kind, vbuf, n, &mut out.value);
        } else {
            let mut present = Vec::with_capacity(n - missing);
            self.decode_values(kind, vbuf, n - missing, &mut present);
            let mut it = present.into_iter();
            for &f in flags {
                out.value.push(if f & FLAG_NO_VALUE != 0 {
                    0
                } else {
                    it.next().expect("value for every row with a value")
                });
            }
        }
        let p = self.column(buf, &mut pos, &mut scratch);
        let start = out.position.len();
        let mut tmp = Vec::with_capacity(n);
        self.decode_t(p, n, &mut tmp);
        out.position.extend(tmp.into_iter().map(|p| p as u64));
        debug_assert_eq!(out.position.len() - start, n);
    }

    fn header(buf: &[u8]) -> (usize, ValueKind, usize) {
        let n = u32_at(buf, 0) as usize;
        let kind = if buf[4] == 1 {
            ValueKind::State
        } else {
            ValueKind::F64
        };
        (n, kind, u32_at(buf, 5) as usize)
    }

    fn block_rows(n: usize, b: usize) -> usize {
        ((b + 1) * BLOCK_ROWS).min(n) - b * BLOCK_ROWS
    }

    /// Decodes a whole chunk.
    pub fn decode_chunk(&self, buf: &[u8]) -> Chunk {
        let (n, kind, blocks) = Self::header(buf);
        let mut out = Chunk::new(kind);
        out.t.reserve(n);
        out.value.reserve(n);
        out.quality.reserve(n);
        out.flags.reserve(n);
        out.position.reserve(n);
        for b in 0..blocks {
            let e = CHUNK_HEADER + b * BLOCK_ENTRY;
            let offset = u32_at(buf, e + 16) as usize;
            self.decode_block(buf, kind, offset, Self::block_rows(n, b), &mut out);
        }
        out
    }

    /// Decodes the rows with `from <= t < to`.
    pub fn decode_chunk_range(&self, buf: &[u8], from: i64, to: i64) -> Chunk {
        let (n, kind, blocks) = Self::header(buf);
        let entry = |b: usize| CHUNK_HEADER + b * BLOCK_ENTRY;
        // First block whose last_t >= from.
        let (mut lo, mut hi) = (0, blocks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if i64_at(buf, entry(mid) + 8) < from {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut tmp = Chunk::new(kind);
        let mut b = lo;
        while b < blocks && i64_at(buf, entry(b)) < to {
            let offset = u32_at(buf, entry(b) + 16) as usize;
            self.decode_block(buf, kind, offset, Self::block_rows(n, b), &mut tmp);
            b += 1;
        }
        tmp.slice_t(from, to)
    }
}

impl Codec for Own {
    type Encoded = OwnFile;

    fn name(&self) -> &'static str {
        self.name
    }

    fn encode(&self, chunks: &[&Chunk]) -> OwnFile {
        let (chunks, sizes) = chunks
            .iter()
            .map(|c| {
                let (b, mut s) = self.encode_chunk(c);
                s.overhead += FILE_DIRECTORY_ENTRY;
                (b, s)
            })
            .unzip();
        OwnFile { chunks, sizes }
    }

    fn sizes(&self, encoded: &OwnFile) -> Vec<ColumnSizes> {
        encoded.sizes.clone()
    }

    fn decode(&self, encoded: &OwnFile, index: usize) -> Chunk {
        self.decode_chunk(&encoded.chunks[index])
    }

    fn decode_range(&self, encoded: &OwnFile, index: usize, from: i64, to: i64) -> Chunk {
        self.decode_chunk_range(&encoded.chunks[index], from, to)
    }

    fn i64_column_size(&self, columns: &[&[i64]]) -> u64 {
        let mut total = 0;
        let mut out = Vec::new();
        for col in columns {
            for block in col.chunks(BLOCK_ROWS) {
                out.clear();
                total += self.put_column(self.encode_t(block), &mut out) + 4;
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_codecs_round_trip() {
        let xs: Vec<i64> = vec![0, 1, 3, 1 << 40, -5, i64::MIN, i64::MAX, 7, 7, 7];
        let mut out = Vec::new();
        dod_decode(&dod_encode(&xs), xs.len(), &mut out);
        assert_eq!(out, xs);
        let mut out = Vec::new();
        delta_decode(&delta_encode(&xs), xs.len(), &mut out);
        assert_eq!(out, xs);

        let vs: Vec<u64> = [
            21.37f64,
            21.37,
            21.38,
            -0.0,
            0.0,
            1e300,
            f64::MIN_POSITIVE,
            5.0,
        ]
        .iter()
        .map(|v| v.to_bits())
        .collect();
        let mut out = Vec::new();
        xor_decode(&xor_encode(&vs), vs.len(), &mut out);
        assert_eq!(out, vs);
        // -0.0 forces the XOR fallback.
        let mut out = Vec::new();
        decimal_decode(&decimal_encode(&vs), vs.len(), &mut out);
        assert_eq!(out, vs);

        let dec: Vec<u64> = (0..1000)
            .map(|i| (f64::from(i * 7 % 300 - 100) / 100.0).to_bits())
            .collect();
        let enc = decimal_encode(&dec);
        assert_eq!(enc[0], 1, "exact at two decimals");
        let mut out = Vec::new();
        decimal_decode(&enc, dec.len(), &mut out);
        assert_eq!(out, dec);

        let mut out = Vec::new();
        let rle = [3u64, 3, 3, 0, 9, 9];
        rle_decode(&rle_encode(rle), rle.len(), |v, run| {
            out.extend((0..run).map(|_| v))
        });
        assert_eq!(out, rle);
    }
}
