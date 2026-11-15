//! Integer column encodings: plain (encoding 0), run-length (encoding 1) and delta with frame-of-reference bit
//! packing (encoding 2), `docs/format-chunk.md` C.3–C.5.
//!
//! The decoders take the value count from the caller, never from the payload, and reserve at most
//! [`MAX_ROWS`] values up front, so a payload cannot make them allocate more than its own values.

use crate::MAX_ROWS;
use crate::bytes::{Reader, bit_width, pack, put_leb128, unpack, unzigzag, zigzag};
use crate::error::ColumnError;

/// Values per frame of reference in the delta encoding.
pub const FRAME: usize = 128;

/// The longest run a 3-byte LEB128 run length can hold.
const MAX_RUN: u64 = (1 << 21) - 1;

/// A fixed-width little-endian value of a plain or run-length column.
pub trait Fixed: Copy + PartialEq + Default {
    /// Its width in bytes.
    const WIDTH: usize;
    /// Appends it, little-endian.
    fn put(self, out: &mut Vec<u8>);
    /// Reads it from exactly [`Self::WIDTH`] bytes.
    fn get(b: &[u8]) -> Self;
}

macro_rules! fixed {
    ($t:ty, $w:expr) => {
        impl Fixed for $t {
            const WIDTH: usize = $w;
            fn put(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn get(b: &[u8]) -> Self {
                let mut a = [0u8; $w];
                a.copy_from_slice(b);
                <$t>::from_le_bytes(a)
            }
        }
    };
}
fixed!(u8, 1);
fixed!(u16, 2);
fixed!(u32, 4);
fixed!(u64, 8);
fixed!(i64, 8);

fn reserve<T>(n: usize) -> Vec<T> {
    Vec::with_capacity(n.min(MAX_ROWS))
}

/// Encoding 0: the values, little-endian, one after another.
pub fn encode_plain<T: Fixed>(values: &[T]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * T::WIDTH);
    for &v in values {
        v.put(&mut out);
    }
    out
}

/// Reverses [`encode_plain`] for `n` values.
pub fn decode_plain<T: Fixed>(payload: &[u8], n: usize) -> Result<Vec<T>, ColumnError> {
    if Some(payload.len()) != n.checked_mul(T::WIDTH) {
        return Err(ColumnError::Length);
    }
    Ok(payload.chunks_exact(T::WIDTH).map(T::get).collect())
}

/// Encoding 1: pairs of a run length (minimal unsigned LEB128, at most 3 bytes, at least 1) and one value; adjacent
/// runs hold different values. Runs longer than 2^21 − 1 are split, which never happens within a block.
pub fn encode_run_length<T: Fixed>(values: &[T]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let v = values[i];
        let n = values[i..].iter().take_while(|&&x| x == v).count();
        let run = (n as u64).min(MAX_RUN);
        put_leb128(&mut out, run);
        v.put(&mut out);
        i += run as usize;
    }
    out
}

/// Reverses [`encode_run_length`] for `n` values, rejecting every other form.
pub fn decode_run_length<T: Fixed>(payload: &[u8], n: usize) -> Result<Vec<T>, ColumnError> {
    let mut out: Vec<T> = reserve(n);
    let mut r = Reader::new(payload);
    let mut last: Option<T> = None;
    while !r.is_empty() {
        let run = r.leb128(3).map_err(|_| ColumnError::RunLength)?;
        let v = T::get(r.bytes(T::WIDTH)?);
        if run == 0 || last == Some(v) || out.len() as u64 + run > n as u64 {
            return Err(ColumnError::RunLength);
        }
        out.extend(std::iter::repeat_n(v, run as usize));
        last = Some(v);
    }
    if out.len() != n {
        return Err(ColumnError::RunLength);
    }
    Ok(out)
}

/// Encoding 2: the first value (8 bytes little-endian), then the differences between consecutive values (wrapping)
/// in frames of [`FRAME`]: per frame the smallest difference as zigzag LEB128, a bit width (one byte, 0–64) and
/// each difference minus the smallest, packed least significant bit first at that width, padded to a byte with zero
/// bits. Any `i64` sequence round-trips, `u64` columns through `as i64`.
pub fn encode_delta(values: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(&first) = values.first() else {
        return out;
    };
    out.extend_from_slice(&first.to_le_bytes());
    let mut d = [0i64; FRAME];
    let mut i = 1;
    while i < values.len() {
        let m = (values.len() - i).min(FRAME);
        for (k, x) in d[..m].iter_mut().enumerate() {
            *x = values[i + k].wrapping_sub(values[i + k - 1]);
        }
        let d = &d[..m];
        let min = d.iter().copied().min().unwrap_or(0);
        let width = d
            .iter()
            .map(|&x| bit_width(x.wrapping_sub(min) as u64))
            .max()
            .unwrap_or(0);
        put_leb128(&mut out, zigzag(min));
        out.push(width as u8);
        pack(
            &mut out,
            d.iter().map(|&x| x.wrapping_sub(min) as u64),
            width,
        );
        i += m;
    }
    out
}

/// The length of [`encode_delta`]'s output, without building it.
pub fn delta_len(values: &[i64]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let mut len = 8;
    let mut i = 1;
    while i < values.len() {
        let m = (values.len() - i).min(FRAME);
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        for k in i..i + m {
            let x = values[k].wrapping_sub(values[k - 1]);
            min = min.min(x);
            max = max.max(x);
        }
        let width = bit_width(max.wrapping_sub(min) as u64) as usize;
        len += crate::bytes::leb128_len(zigzag(min)) + 1 + (m * width).div_ceil(8);
        i += m;
    }
    len
}

/// Reverses [`encode_delta`] for `n` values. Checks the payload's structure (lengths, widths, LEB128 form, zero
/// padding, no bytes left over); whether the smallest difference and the width are the least possible is a
/// canonical check.
pub fn decode_delta(payload: &[u8], n: usize) -> Result<Vec<i64>, ColumnError> {
    let mut out: Vec<i64> = reserve(n);
    let mut r = Reader::new(payload);
    if n == 0 {
        r.finish()?;
        return Ok(out);
    }
    let mut v = r.u64()? as i64;
    out.push(v);
    let mut left = n - 1;
    while left > 0 {
        let min = unzigzag(r.leb128(10)?);
        let width = u32::from(r.u8()?);
        if width > 64 {
            return Err(ColumnError::Width);
        }
        let m = left.min(FRAME);
        unpack(&mut r, m, width, |x| {
            v = v.wrapping_add(min.wrapping_add(x as i64));
            out.push(v);
        })?;
        left -= m;
    }
    r.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_round_trips_extremes() {
        let xs: Vec<i64> = vec![0, 1, 3, 1 << 40, -5, i64::MIN, i64::MAX, 7, 7, 7];
        let e = encode_delta(&xs);
        assert_eq!(e.len(), delta_len(&xs));
        assert_eq!(decode_delta(&e, xs.len()).unwrap(), xs);
        assert!(encode_delta(&[]).is_empty());
        assert_eq!(decode_delta(&[], 0).unwrap(), Vec::<i64>::new());
        assert_eq!(decode_delta(&[0], 0), Err(ColumnError::TrailingBytes));
    }

    #[test]
    fn delta_frame_boundaries() {
        for n in [1usize, 2, 127, 128, 129, 130, 257, 4095, 4096] {
            let xs: Vec<i64> = (0..n as i64)
                .map(|i| 1_000_000_000 * i + (i * 7919) % 13)
                .collect();
            let e = encode_delta(&xs);
            assert_eq!(e.len(), delta_len(&xs), "{n}");
            assert_eq!(decode_delta(&e, n).unwrap(), xs, "{n}");
            assert!(decode_delta(&e[..e.len() - 1], n).is_err(), "{n}");
        }
    }

    #[test]
    fn delta_layout() {
        // A perfect grid: first value, then one frame with min 10 (zigzag 20) and width 0.
        assert_eq!(encode_delta(&[5, 15, 25]), [5, 0, 0, 0, 0, 0, 0, 0, 20, 0]);
        // Width above 64 is rejected.
        assert_eq!(
            decode_delta(&[5, 0, 0, 0, 0, 0, 0, 0, 20, 65], 2),
            Err(ColumnError::Width)
        );
    }

    #[test]
    fn run_length_rules() {
        let v: Vec<u8> = [vec![3u8; 200], vec![1u8; 1], vec![3u8; 2]].concat();
        let e = encode_run_length(&v);
        assert_eq!(e, [0xC8, 0x01, 3, 1, 1, 2, 3]);
        assert_eq!(decode_run_length::<u8>(&e, 203).unwrap(), v);
        let bad: [&[u8]; 6] = [
            &[0, 1, 2, 2],
            &[0x82, 0x00, 1],
            &[1, 5, 1, 5],
            &[2, 5],
            &[4, 5],
            &[2],
        ];
        let n = [2usize, 2, 2, 3, 3, 2];
        for (b, n) in bad.iter().zip(n) {
            assert!(decode_run_length::<u8>(b, n).is_err(), "{b:?}");
        }
        assert!(decode_run_length::<u8>(&[0x81, 0x80, 0x80, 0x00, 1], 1).is_err());
        let w = vec![7u64; 4096];
        assert_eq!(
            decode_run_length::<u64>(&encode_run_length(&w), 4096).unwrap(),
            w
        );
    }

    #[test]
    fn plain_length_checked() {
        assert_eq!(decode_plain::<u16>(&[1, 0, 2], 2), Err(ColumnError::Length));
        assert_eq!(
            decode_plain::<u16>(&encode_plain(&[1u16, 2]), 2).unwrap(),
            [1, 2]
        );
    }
}
