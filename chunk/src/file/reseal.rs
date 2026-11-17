//! Re-sealing an edited file: every block's checksums, then each parent's record of its child's `body_crc32c`,
//! bottom up to the trailer. Offsets are never changed. Tests and the structure-aware fuzz entry use it to build
//! files that are checksum-valid and inconsistent, which only a writer's mistake or a crafted object makes, so
//! that the reader's checks behind the checksum chain are reached. It never panics, whatever the bytes.

use kvik_log::crc::crc32c;

/// A sum that saturates, so an offset read from the file can only miss the buffer, never overflow.
fn sum(parts: &[usize]) -> usize {
    parts.iter().fold(0usize, |a, &p| a.saturating_add(p))
}

fn get(b: &[u8], at: usize, n: usize) -> Option<&[u8]> {
    b.get(at..at.checked_add(n)?)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    get(b, at, 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    get(b, at, 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
}

fn usize_at(b: &[u8], at: usize) -> Option<usize> {
    u64_at(b, at).and_then(|v| usize::try_from(v).ok())
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = at.checked_add(4).and_then(|e| b.get_mut(at..e)) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// The body of the block whose header is at `at`, by its stated `body_len`.
fn body(b: &[u8], at: usize) -> Option<&[u8]> {
    if at < 64 {
        return None;
    }
    let len = u32_at(b, at.checked_add(4)?)? as usize;
    get(b, at.checked_add(16)?, len)
}

/// The `body_crc32c` of the block at `at` as its body now is.
fn crc_of(b: &[u8], at: usize) -> Option<u32> {
    body(b, at).map(crc32c)
}

/// Re-seals the block at `at`: its `body_crc32c`, then its `header_crc32c`. Returns the body's checksum.
fn seal(b: &mut [u8], at: usize) -> Option<u32> {
    let crc = crc_of(b, at)?;
    put_u32(b, at.checked_add(8)?, crc);
    let h = crc32c(get(b, at, 12)?);
    put_u32(b, sum(&[at, 12]), h);
    Some(crc)
}

/// The block headers from offset 64 to `end`, by their stated lengths.
fn walk(b: &[u8], end: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 64usize;
    while let Some(len) = at
        .checked_add(16)
        .filter(|&e| e <= end)
        .and_then(|_| u32_at(b, sum(&[at, 4])))
    {
        out.push(at);
        let next = (len as usize)
            .div_ceil(8)
            .checked_mul(8)
            .and_then(|l| l.checked_add(sum(&[at, 16])));
        match next {
            Some(n) if n <= end => at = n,
            _ => break,
        }
    }
    out
}

/// The offset of the trailer block, from the end marker.
fn trailer_at(b: &[u8]) -> Option<usize> {
    usize_at(b, b.len().checked_sub(16)?)
}

fn seal_all(b: &mut [u8]) -> Option<usize> {
    let end = b.len().checked_sub(16)?;
    for at in walk(b, end) {
        seal(b, at);
    }
    trailer_at(b)
}

/// Re-seals a compacted segment (`docs/format-compacted.md` B.1) after its bodies were edited.
pub fn reseal_segment(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    let _ = reseal_segment_in(&mut b);
    b
}

fn reseal_segment_in(b: &mut [u8]) -> Option<()> {
    let trailer = seal_all(b)?;
    let t = trailer.checked_add(16)?;
    let entries_offset = usize_at(b, sum(&[t, 80]))?;
    let times_offset = usize_at(b, sum(&[t, 88]))?;
    let times_dir = usize_at(b, sum(&[t, 96]))?;
    let index_offset = usize_at(b, sum(&[t, 104]))?;
    let root = usize_at(b, sum(&[t, 112]))?;
    let root_records = |b: &[u8]| -> Vec<(usize, usize)> {
        let Some(rb) = body(b, root) else {
            return Vec::new();
        };
        let size = rb
            .get(..2)
            .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
        let count = u32_at(rb, 4).unwrap_or(0) as usize;
        (0..count.min(rb.len() / size.max(1)))
            .filter_map(|i| {
                let r = 8usize.checked_add(size.checked_mul(i)?)?;
                let off = usize_at(rb, sum(&[r, 16]))?;
                Some((r, index_offset.checked_add(off)?))
            })
            .collect()
    };
    for (_, block) in root_records(b) {
        let Some(ib) = body(b, block).map(<[u8]>::to_vec) else {
            continue;
        };
        let size = ib
            .get(..2)
            .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
        let count = ib
            .get(2..4)
            .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
        for i in 0..count.min(ib.len() / size.max(1)) {
            let Some(e) = size.checked_mul(i).and_then(|x| x.checked_add(8)) else {
                continue;
            };
            let entry_at = sum(&[block, 16, e]);
            let (Some(chunk), Some(chunk_len), Some(dir_len)) = (
                usize_at(&ib, sum(&[e, 56])),
                u32_at(&ib, sum(&[e, 64])),
                u32_at(&ib, sum(&[e, 68])),
            ) else {
                continue;
            };
            let Some(dir) = chunk
                .checked_add(chunk_len as usize)
                .and_then(|end| end.checked_sub(dir_len as usize))
            else {
                continue;
            };
            if let Some(db) = body(b, dir).map(<[u8]>::to_vec) {
                let rsize = db
                    .get(16..18)
                    .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
                let blocks = db
                    .get(6..8)
                    .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
                for k in 0..blocks.min(db.len() / rsize.max(1)) {
                    let Some(r) = rsize.checked_mul(k).and_then(|x| x.checked_add(24)) else {
                        continue;
                    };
                    let Some(data) =
                        u32_at(&db, sum(&[r, 16])).and_then(|o| chunk.checked_add(o as usize))
                    else {
                        continue;
                    };
                    if let Some(c) = crc_of(b, data) {
                        put_u32(b, sum(&[dir, 16, r, 28]), c);
                    }
                }
                if let Some(c) = seal(b, dir) {
                    put_u32(b, sum(&[entry_at, 76]), c);
                }
            }
        }
        seal(b, block);
    }
    for (r, block) in root_records(b) {
        if let Some(c) = crc_of(b, block) {
            put_u32(b, sum(&[root, 16, r, 28]), c);
        }
    }
    let root_crc = seal(b, root);
    if let Some(db) = body(b, times_dir).map(<[u8]>::to_vec) {
        let size = db
            .get(..2)
            .map_or(0, |v| u16::from_le_bytes([v[0], v[1]]) as usize);
        let count = u32_at(&db, 4).unwrap_or(0) as usize;
        for i in 0..count.min(db.len() / size.max(1)) {
            let Some(r) = size.checked_mul(i).and_then(|x| x.checked_add(8)) else {
                continue;
            };
            if let Some(c) = usize_at(&db, sum(&[r, 40])).and_then(|o| crc_of(b, o)) {
                put_u32(b, sum(&[times_dir, 16, r, 4]), c);
            }
        }
    }
    let dir_crc = seal(b, times_dir);
    let entries_crc = if entries_offset == 0 {
        Some(0)
    } else {
        b.get(entries_offset..times_offset).map(crc32c)
    };
    for (at, v) in [
        (124, root_crc),
        (128, dir_crc),
        (132, crc_of(b, 64)),
        (136, entries_crc),
    ] {
        if let Some(v) = v {
            put_u32(b, sum(&[t, at]), v);
        }
    }
    let file = crc32c(b.get(..trailer)?);
    put_u32(b, sum(&[t, 120]), file);
    seal(b, trailer);
    Some(())
}

/// Re-seals a chunk index object (B.2) after its bodies were edited: every block, the trailer's record of the
/// root's checksum and `file_crc32c`. The segment's trailer inside `0x38` is left as edited.
pub fn reseal_index_object(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    let _ = reseal_index_object_in(&mut b);
    b
}

fn reseal_index_object_in(b: &mut [u8]) -> Option<()> {
    let trailer = seal_all(b)?;
    let t = trailer.checked_add(16)?;
    if let Some(c) = usize_at(b, sum(&[t, 8])).and_then(|root| crc_of(b, root)) {
        put_u32(b, sum(&[t, 28]), c);
    }
    let file = crc32c(b.get(..trailer)?);
    put_u32(b, sum(&[t, 24]), file);
    seal(b, trailer);
    Some(())
}

/// Applies `edit` to the body of every block of type `ty` (with its offset and the count of earlier ones of that
/// type), keeping its length, then re-seals with `reseal`.
pub fn edit_blocks(
    bytes: &[u8],
    ty: u8,
    mut edit: impl FnMut(usize, usize, &mut [u8]),
    reseal: fn(&[u8]) -> Vec<u8>,
) -> Vec<u8> {
    let mut b = bytes.to_vec();
    let Some(end) = b.len().checked_sub(16) else {
        return b;
    };
    let mut n = 0;
    for at in walk(&b, end) {
        if b.get(at) != Some(&ty) {
            continue;
        }
        let Some(len) = u32_at(&b, sum(&[at, 4])).map(|l| l as usize) else {
            continue;
        };
        let start = sum(&[at, 16]);
        if let Some(body) = start.checked_add(len).and_then(|e| b.get_mut(start..e)) {
            edit(at, n, body);
            n += 1;
        }
    }
    reseal(&b)
}
