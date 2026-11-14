//! Run-length encoding of fixed-width columns (`format.md` A.5, encoding 1): pairs of a run length, as minimal
//! unsigned LEB128 of at most 3 bytes, and one value; runs at least 1 long, adjacent runs different, lengths summing
//! to `row_count`.

/// Values of a fixed-width column.
pub(crate) trait Fixed: Copy + PartialEq {
    const WIDTH: usize;
    fn put(self, out: &mut Vec<u8>);
    fn get(b: &[u8]) -> Self;
}

impl Fixed for u8 {
    const WIDTH: usize = 1;
    fn put(self, out: &mut Vec<u8>) {
        out.push(self);
    }
    fn get(b: &[u8]) -> Self {
        b[0]
    }
}

impl Fixed for u16 {
    const WIDTH: usize = 2;
    fn put(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        u16::from_le_bytes([b[0], b[1]])
    }
}

/// The longest run a 3-byte LEB128 can hold; above every `row_count`.
const MAX_RUN: u32 = (1 << 21) - 1;

pub(crate) fn encode<T: Fixed>(values: &[T]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let v = values[i];
        let n = values[i..].iter().take_while(|&&x| x == v).count();
        let mut run = n as u32;
        debug_assert!(run <= MAX_RUN);
        loop {
            let byte = (run & 0x7F) as u8;
            run >>= 7;
            if run == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        v.put(&mut out);
        i += n;
    }
    out
}

/// Decodes `rows` values, or `None` if the payload is not in canonical run-length form.
pub(crate) fn decode<T: Fixed>(payload: &[u8], rows: u32) -> Option<Vec<T>> {
    let mut out: Vec<T> = Vec::with_capacity(rows as usize);
    let mut at = 0;
    let mut last: Option<T> = None;
    while at < payload.len() {
        let mut run: u32 = 0;
        let mut len = 0;
        loop {
            let b = *payload.get(at + len)?;
            run |= u32::from(b & 0x7F) << (7 * len);
            len += 1;
            if b & 0x80 == 0 {
                // Minimal: no final zero byte after the first.
                if len > 1 && b == 0 {
                    return None;
                }
                break;
            }
            if len == 3 {
                return None;
            }
        }
        at += len;
        let v = T::get(payload.get(at..at + T::WIDTH)?);
        at += T::WIDTH;
        if run == 0 || last == Some(v) || out.len() as u64 + u64::from(run) > u64::from(rows) {
            return None;
        }
        out.extend(std::iter::repeat_n(v, run as usize));
        last = Some(v);
    }
    (out.len() as u64 == u64::from(rows)).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_layout() {
        let v: Vec<u8> = [vec![3u8; 200], vec![1u8; 1], vec![3u8; 2]].concat();
        let e = encode(&v);
        assert_eq!(e, [0xC8, 0x01, 3, 1, 1, 2, 3]);
        assert_eq!(decode::<u8>(&e, 203), Some(v));
        let w: Vec<u16> = vec![0x0102; 65_536];
        let e = encode(&w);
        assert_eq!(e, [0x80, 0x80, 0x04, 0x02, 0x01]);
        assert_eq!(decode::<u16>(&e, 65_536), Some(w));
    }

    #[test]
    fn non_canonical_forms_are_rejected() {
        assert_eq!(decode::<u8>(&[0, 1, 2, 2], 2), None); // a zero run
        assert_eq!(decode::<u8>(&[0x82, 0x00, 1], 2), None); // non-minimal LEB128
        assert_eq!(decode::<u8>(&[0x81, 0x80, 0x80, 0x00, 1], 1), None); // longer than 3 bytes
        assert_eq!(decode::<u8>(&[1, 5, 1, 5], 2), None); // equal adjacent runs
        assert_eq!(decode::<u8>(&[2, 5], 3), None); // short of row_count
        assert_eq!(decode::<u8>(&[4, 5], 3), None); // past row_count
        assert_eq!(decode::<u8>(&[2], 2), None); // value missing
        assert_eq!(decode::<u16>(&[2, 1], 2), None); // value cut short
        assert_eq!(decode::<u8>(&[2, 5, 1, 6], 3), Some(vec![5, 5, 6]));
    }
}
