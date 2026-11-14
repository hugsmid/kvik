//! Little-endian field access on byte slices whose length the caller has checked.

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

pub(crate) fn i64_at(b: &[u8], at: usize) -> i64 {
    u64_at(b, at) as i64
}

pub(crate) fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

/// The slice `b[start..start + len]`, or `None` if it is not inside `b`.
pub(crate) fn slice(b: &[u8], start: u64, len: u64) -> Option<&[u8]> {
    let start = usize::try_from(start).ok()?;
    let len = usize::try_from(len).ok()?;
    b.get(start..start.checked_add(len)?)
}
