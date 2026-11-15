//! Field lists (`format.md` A.6 framing) and the little-endian helpers the record codecs share.

use kvik_log::manifest::RecordPosition;

/// One field as read: its id and its value.
pub(crate) type Field<'a> = (u16, &'a [u8]);

/// The fields of a field list, in order, or `None` if a field header or value does not fit. Padding after a value
/// is not read (`format.md` A, common rules); a list may end without the last value's padding.
pub(crate) fn parse(body: &[u8]) -> Option<Vec<Field<'_>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let h = body.get(at..at.checked_add(4)?)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        let start = at + 4;
        out.push((id, body.get(start..start.checked_add(len)?)?));
        at = (start + len).div_ceil(4) * 4;
    }
    Some(out)
}

/// Appends one field, its value padded to 4. `None` if the value is longer than a field can hold.
#[must_use]
pub(crate) fn push(out: &mut Vec<u8>, id: u16, value: &[u8]) -> Option<()> {
    let len = u16::try_from(value.len()).ok()?;
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
    Some(())
}

pub(crate) fn u32_of(v: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(v.try_into().ok()?))
}

pub(crate) fn u64_of(v: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(v.try_into().ok()?))
}

pub(crate) fn u64_at(v: &[u8], at: usize) -> u64 {
    v.get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

pub(crate) fn u32_at(v: &[u8], at: usize) -> u32 {
    v.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// A 12-byte position: generation, then sequence.
pub(crate) fn position_of(v: &[u8]) -> Option<RecordPosition> {
    (v.len() == 12).then(|| (u32_at(v, 0), u64_at(v, 4)))
}

pub(crate) fn position_bytes(p: RecordPosition) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..4].copy_from_slice(&p.0.to_le_bytes());
    b[4..].copy_from_slice(&p.1.to_le_bytes());
    b
}
