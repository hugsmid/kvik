//! `f64` value encodings, `docs/format-chunk.md` C.6–C.8: scaled decimal with per-value exceptions (encoding 3),
//! XOR (encoding 4) and XOR compressed with zstd (encoding 5). Each round-trips every one of the 2^64 bit patterns
//! (NaN payloads, −0.0, subnormals and infinities included): a value that is not an exact scaled decimal is stored
//! as an exception, with its 64 bits.

use std::cell::RefCell;

use crate::bytes::{BitReader, BitWriter, Reader};
use crate::error::{ColumnError, EncodeError};
use crate::ints::{decode_delta, delta_len, encode_delta};

/// The highest decimal exponent (the highest decimal scale, `spec.md` §2.1).
pub const MAX_EXPONENT: u8 = 18;

/// Scaled integers stay below this magnitude, so each converts to `f64` exactly.
const MANTISSA_LIMIT: f64 = 9_007_199_254_740_992.0; // 2^53

/// 10^e for e in 0..=18, each exact in `f64`.
const POW10: [f64; 19] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18,
];

/// The zstd level of the compressed XOR fallback (as measured in roadmap 2.1).
pub const ZSTD_LEVEL: i32 = 3;

/// The scaled integer `n` with `n / 10^e == v` exactly, if `v` (as bits) is such a decimal: `n` is
/// `round(v × 10^e)` (`f64` multiplication, then rounding half away from zero), `|n| < 2^53`, and the `f64`
/// division `n / 10^e` gives back the same 64 bits. −0.0, NaN and infinities never fit.
pub fn scaled(bits: u64, e: u8) -> Option<i64> {
    let p = POW10[usize::from(e.min(MAX_EXPONENT))];
    let x = f64::from_bits(bits) * p;
    if !x.is_finite() {
        return None;
    }
    let n = x.round();
    if n.abs() >= MANTISSA_LIMIT {
        return None;
    }
    let n = n as i64;
    ((n as f64 / p).to_bits() == bits).then_some(n)
}

/// The smallest exponent at which `bits` fits, if any.
pub fn min_exponent(bits: u64) -> Option<u8> {
    (0..=MAX_EXPONENT).find(|&e| scaled(bits, e).is_some())
}

/// Encoding 3 at exponent `e`: `e` (one byte), the exception count (`u16`), each exception's index (`u16`,
/// strictly ascending) and bits (`u64`), then the scaled integers of the other values in encoding 2.
///
/// # Panics
/// If `values` holds more than 65,535 values (indexes are `u16`; a block holds at most 4,096) or `e` is above 18.
pub fn encode_decimal(values: &[u64], e: u8) -> Vec<u8> {
    assert!(
        values.len() <= usize::from(u16::MAX),
        "too many values for encoding 3"
    );
    assert!(e <= MAX_EXPONENT, "decimal exponent above 18");
    let (ns, exceptions) = split(values, e);
    let mut out = Vec::with_capacity(3 + exceptions.len() * 10 + delta_len(&ns));
    out.push(e);
    out.extend_from_slice(&(exceptions.len() as u16).to_le_bytes());
    for &(i, b) in &exceptions {
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
    }
    out.extend_from_slice(&encode_delta(&ns));
    out
}

fn split(values: &[u64], e: u8) -> (Vec<i64>, Vec<(u16, u64)>) {
    let mut ns = Vec::with_capacity(values.len());
    let mut exceptions = Vec::new();
    for (i, &b) in values.iter().enumerate() {
        match scaled(b, e) {
            Some(n) => ns.push(n),
            None => exceptions.push((i as u16, b)),
        }
    }
    (ns, exceptions)
}

/// The best decimal exponent for `values` and the exception count and payload length it gives: among the distinct
/// smallest exponents of the values, the one with the shortest payload, the lowest on a tie. `None` when no value
/// fits any exponent.
pub fn best_decimal(values: &[u64]) -> Option<(u8, usize, usize)> {
    let mut seen = [false; 19];
    for &b in values {
        if let Some(e) = min_exponent(b) {
            seen[usize::from(e)] = true;
        }
    }
    let mut best: Option<(u8, usize, usize)> = None;
    for e in (0..=MAX_EXPONENT).filter(|&e| seen[usize::from(e)]) {
        let (ns, exceptions) = split(values, e);
        let len = 3 + exceptions.len() * 10 + delta_len(&ns);
        if best.is_none_or(|(_, _, l)| len < l) {
            best = Some((e, exceptions.len(), len));
        }
    }
    best
}

/// Reverses [`encode_decimal`] for `n` values (`n ≤ 65,535`).
pub fn decode_decimal(payload: &[u8], n: usize) -> Result<Vec<u64>, ColumnError> {
    let mut r = Reader::new(payload);
    let e = r.u8()?;
    if e > MAX_EXPONENT {
        return Err(ColumnError::Exponent);
    }
    let count = usize::from(r.u16()?);
    if count > n {
        return Err(ColumnError::ExceptionCount);
    }
    let table = r.bytes(count * 10)?;
    let mut exceptions: Vec<(usize, u64)> = Vec::with_capacity(count);
    for c in table.as_chunks::<10>().0 {
        let i = usize::from(u16::from_le_bytes([c[0], c[1]]));
        if i >= n || exceptions.last().is_some_and(|&(p, _)| i <= p) {
            return Err(ColumnError::ExceptionIndex);
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&c[2..]);
        exceptions.push((i, u64::from_le_bytes(b)));
    }
    let ns = decode_delta(&payload[3 + count * 10..], n - count)?;
    if ns.iter().any(|&v| (v as f64).abs() >= MANTISSA_LIMIT) {
        return Err(ColumnError::Mantissa);
    }
    let p = POW10[usize::from(e)];
    let mut out = Vec::with_capacity(n);
    let mut ns = ns.into_iter();
    let mut exceptions = exceptions.into_iter().peekable();
    for row in 0..n {
        if let Some((_, b)) = exceptions.next_if(|&(i, _)| i == row) {
            out.push(b);
        } else if let Some(v) = ns.next() {
            // n − count scaled integers for the n − count rows that are not exceptions.
            out.push((v as f64 / p).to_bits());
        }
    }
    Ok(out)
}

/// The longest XOR stream of `n` values: 64 bits, then at most 77 bits per value.
pub fn max_xor_len(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        (64 + (n - 1) * 77).div_ceil(8)
    }
}

/// Encoding 4: the first value's 64 bits, then per value the XOR with the previous one, most significant bit first:
/// `0` if zero; `10` and the bits inside the previous window when the leading and trailing zeros cover it; else
/// `11`, the leading zeros (5 bits, at most 31), the meaningful length minus one (6 bits) and the meaningful bits.
/// The last byte is padded with zero bits.
pub fn encode_xor(values: &[u64]) -> Vec<u8> {
    let mut w = BitWriter::default();
    let Some((&first, rest)) = values.split_first() else {
        return w.finish();
    };
    w.write(first, 64);
    let mut prev = first;
    let mut window: Option<(u32, u32)> = None;
    for &v in rest {
        let x = v ^ prev;
        prev = v;
        if x == 0 {
            w.bit(false);
            continue;
        }
        w.bit(true);
        let lead = x.leading_zeros().min(31);
        let trail = x.trailing_zeros();
        match window {
            Some((l, t)) if lead >= l && trail >= t => {
                w.bit(false);
                w.write(x >> t, 64 - l - t);
            }
            _ => {
                w.bit(true);
                let sig = 64 - lead - trail;
                w.write(u64::from(lead), 5);
                w.write(u64::from(sig - 1), 6);
                w.write(x >> trail, sig);
                window = Some((lead, trail));
            }
        }
    }
    w.finish()
}

/// Reverses [`encode_xor`] for `n` values.
pub fn decode_xor(payload: &[u8], n: usize) -> Result<Vec<u64>, ColumnError> {
    let mut out = Vec::with_capacity(n.min(crate::MAX_ROWS));
    let mut r = BitReader::new(payload);
    if n == 0 {
        r.finish()?;
        return Ok(out);
    }
    let mut v = r.read(64)?;
    out.push(v);
    let mut window: Option<(u32, u32)> = None;
    for _ in 1..n {
        if r.bit()? {
            let x = if r.bit()? {
                let lead = r.read(5)? as u32;
                let sig = r.read(6)? as u32 + 1;
                if lead + sig > 64 {
                    return Err(ColumnError::XorWindow);
                }
                let trail = 64 - lead - sig;
                window = Some((lead, trail));
                r.read(sig)? << trail
            } else {
                let (l, t) = window.ok_or(ColumnError::XorWindow)?;
                r.read(64 - l - t)? << t
            };
            v ^= x;
        }
        out.push(v);
    }
    r.finish()?;
    Ok(out)
}

thread_local! {
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
}

/// The magic number of a zstd frame (RFC 8878 §3.1.1), little-endian.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

fn compress(raw: &[u8]) -> Result<Vec<u8>, EncodeError> {
    COMPRESSOR.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c =
                Some(zstd::bulk::Compressor::new(ZSTD_LEVEL).map_err(|_| EncodeError::Compressor)?);
        }
        let c = c.as_mut().ok_or(EncodeError::Compressor)?;
        c.compress(raw).map_err(|_| EncodeError::Compressor)
    })
}

/// Encoding 5 for an XOR stream: its length (`u32`, little-endian), then the stream as one zstd frame (RFC 8878)
/// written at [`ZSTD_LEVEL`], with no dictionary and no content checksum. A compressor failure is an error, never a
/// silent fall back to another encoding.
pub fn encode_xor_zstd(xor: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let frame = compress(xor)?;
    if !frame_is_allowed(&frame) {
        return Err(EncodeError::Compressor);
    }
    let mut out = Vec::with_capacity(4 + frame.len());
    out.extend_from_slice(&(xor.len() as u32).to_le_bytes());
    out.extend_from_slice(&frame);
    Ok(out)
}

/// Whether `frame` is exactly one zstd frame (not a skippable frame) with no dictionary id and no content
/// checksum, and nothing after it (`docs/format-chunk.md` C.8).
fn frame_is_allowed(frame: &[u8]) -> bool {
    // The frame header descriptor follows the magic: bit 2 is the content checksum flag, bits 0–1 the dictionary
    // id flag (RFC 8878 §3.1.1.1.1).
    frame.len() > 4
        && frame[..4] == ZSTD_MAGIC
        && frame[4] & 0b0000_0111 == 0
        && zstd::zstd_safe::find_frame_compressed_size(frame) == Ok(frame.len())
}

/// The XOR stream inside an encoding 5 payload of `n` values: its declared length must not exceed
/// [`max_xor_len`]`(n)`, which bounds the buffer; the rest must be exactly one zstd frame with no dictionary and no
/// content checksum, and decompress to exactly the declared length.
pub fn decompress_xor(payload: &[u8], n: usize) -> Result<Vec<u8>, ColumnError> {
    let mut r = Reader::new(payload);
    let raw_len = r.u32()? as usize;
    if raw_len > max_xor_len(n) {
        return Err(ColumnError::RawLength);
    }
    let frame = &payload[4..];
    if !frame_is_allowed(frame) {
        return Err(ColumnError::Frame);
    }
    let mut raw = vec![0u8; raw_len];
    match zstd::bulk::decompress_to_buffer(frame, &mut raw[..]) {
        Ok(k) if k == raw_len => Ok(raw),
        _ => Err(ColumnError::Compressed),
    }
}

/// Reverses [`encode_xor_zstd`] and [`encode_xor`] for `n` values.
pub fn decode_xor_zstd(payload: &[u8], n: usize) -> Result<Vec<u64>, ColumnError> {
    decode_xor(&decompress_xor(payload, n)?, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(vs: &[f64]) -> Vec<u64> {
        vs.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn scaled_rules() {
        assert_eq!(scaled(21.37f64.to_bits(), 2), Some(2137));
        assert_eq!(scaled(21.37f64.to_bits(), 1), None);
        assert_eq!(min_exponent(21.37f64.to_bits()), Some(2));
        assert_eq!(min_exponent(0.0f64.to_bits()), Some(0));
        // 1/3 is 0.3333333333333333 exactly at 16 places; 0.1 + 0.2 needs 17 digits.
        assert_eq!(min_exponent((1.0f64 / 3.0).to_bits()), Some(16));
        for v in [
            -0.0f64,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            0.1 + 0.2,
        ] {
            assert_eq!(min_exponent(v.to_bits()), None, "{v}");
        }
        assert_eq!(min_exponent(9.0e15f64.to_bits()), Some(0));
        assert_eq!(min_exponent(1.0e16f64.to_bits()), None);
        assert_eq!(min_exponent(1e-18f64.to_bits()), Some(18));
    }

    #[test]
    fn decimal_with_exceptions_round_trips() {
        let mut vs = bits(&[21.37, 21.38, -0.0, 0.1 + 0.2, 21.4, 0.0]);
        vs.push(0x7FF8_0000_DEAD_BEEF); // a NaN payload
        vs.push(0xFFF0_0000_0000_0001); // a signalling NaN
        let (e, count, len) = best_decimal(&vs).unwrap();
        assert_eq!((e, count), (2, 4));
        let enc = encode_decimal(&vs, e);
        assert_eq!(enc.len(), len);
        assert_eq!(decode_decimal(&enc, vs.len()).unwrap(), vs);
        assert!(decode_decimal(&enc, vs.len() + 1).is_err());
        assert!(decode_decimal(&enc[..enc.len() - 1], vs.len()).is_err());
    }

    #[test]
    fn decimal_rejects_bad_tables() {
        let vs = bits(&[1.5, 0.1 + 0.2, 2.5, 0.1 + 0.7]);
        let enc = encode_decimal(&vs, 1);
        // Exponent above 18.
        let mut b = enc.clone();
        b[0] = 19;
        assert_eq!(decode_decimal(&b, 4), Err(ColumnError::Exponent));
        // More exceptions than values.
        let mut b = enc.clone();
        b[1] = 5;
        assert_eq!(decode_decimal(&b, 4), Err(ColumnError::ExceptionCount));
        // Indexes out of order (3 then 1).
        let mut b = enc.clone();
        b[3] = 3;
        b[13] = 1;
        assert_eq!(decode_decimal(&b, 4), Err(ColumnError::ExceptionIndex));
        // Index at or above n.
        let mut b = enc.clone();
        b[13] = 9;
        assert_eq!(decode_decimal(&b, 4), Err(ColumnError::ExceptionIndex));
        // Repeated index.
        let mut b = enc.clone();
        b[13] = 1;
        assert_eq!(decode_decimal(&b, 4), Err(ColumnError::ExceptionIndex));
    }

    #[test]
    fn decimal_rejects_large_mantissa() {
        let mut enc = vec![0u8, 0, 0];
        enc.extend_from_slice(&encode_delta(&[1 << 53]));
        assert_eq!(decode_decimal(&enc, 1), Err(ColumnError::Mantissa));
    }

    #[test]
    fn xor_round_trips_and_checks() {
        let vs = bits(&[
            21.37,
            21.37,
            21.38,
            -0.0,
            0.0,
            1e300,
            f64::MIN_POSITIVE,
            5.0,
            f64::NAN,
        ]);
        let enc = encode_xor(&vs);
        assert!(enc.len() <= max_xor_len(vs.len()));
        assert_eq!(decode_xor(&enc, vs.len()).unwrap(), vs);
        let mut long = enc.clone();
        long.push(0);
        assert_eq!(decode_xor(&long, vs.len()), Err(ColumnError::TrailingBytes));
        // `10` before any window: first value 0, then bits 1, 0.
        let mut b = vec![0u8; 8];
        b.push(0b1000_0000);
        assert_eq!(decode_xor(&b, 2), Err(ColumnError::XorWindow));
        // `11` with 31 leading zeros and 64 meaningful bits.
        let mut w = BitWriter::default();
        w.write(0, 64);
        w.write(0b11, 2);
        w.write(31, 5);
        w.write(63, 6);
        w.write(0, 64);
        assert_eq!(decode_xor(&w.finish(), 2), Err(ColumnError::XorWindow));
        // Worst case stays inside the bound.
        let worst: Vec<u64> = (0..4096u64)
            .map(|i| if i % 2 == 0 { 1 } else { 1 << 63 })
            .collect();
        assert!(encode_xor(&worst).len() <= max_xor_len(4096));
    }

    #[test]
    fn xor_zstd_round_trips_and_bounds() {
        let vs: Vec<u64> = (0..4096)
            .map(|i| (f64::from(i % 5) / 9.0 + 1.0).to_bits())
            .collect();
        let raw = encode_xor(&vs);
        let enc = encode_xor_zstd(&raw).unwrap();
        assert!(enc.len() < raw.len());
        assert_eq!(decode_xor_zstd(&enc, vs.len()).unwrap(), vs);
        let mut big = enc.clone();
        big[..4].copy_from_slice(&(max_xor_len(4096) as u32 + 1).to_le_bytes());
        assert_eq!(decode_xor_zstd(&big, 4096), Err(ColumnError::RawLength));
        let mut short = enc.clone();
        short[..4].copy_from_slice(&(raw.len() as u32 - 1).to_le_bytes());
        assert_eq!(decode_xor_zstd(&short, 4096), Err(ColumnError::Compressed));
        assert_eq!(decode_xor_zstd(&enc[..10], 4096), Err(ColumnError::Frame));
        // A trailing skippable frame (magic 0x184D2A50, length 1), a second frame, a frame with a checksum: rejected.
        let mut skip = enc.clone();
        skip.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 1, 0, 0, 0, 0]);
        assert_eq!(decode_xor_zstd(&skip, 4096), Err(ColumnError::Frame));
        let mut two = enc.clone();
        two.extend_from_slice(&enc[4..]);
        assert_eq!(decode_xor_zstd(&two, 4096), Err(ColumnError::Frame));
        let mut c = zstd::bulk::Compressor::new(3).unwrap();
        c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        let mut sum = enc[..4].to_vec();
        sum.extend_from_slice(&c.compress(&raw).unwrap());
        assert_eq!(decode_xor_zstd(&sum, 4096), Err(ColumnError::Frame));
        // Another level is another valid frame of the same stream.
        let mut other = enc[..4].to_vec();
        other.extend_from_slice(&zstd::bulk::compress(&raw, 19).unwrap());
        assert_eq!(decode_xor_zstd(&other, 4096).unwrap(), vs);
    }
}
