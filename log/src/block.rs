//! Block framing: the 16-byte block header and its body (`format.md` A.2).

use crate::bytes::{put, slice, u32_at};
use crate::crc::crc32c;
use crate::error::EncodeError;

/// Length of a block header.
pub const BLOCK_HEADER_LEN: usize = 16;
/// The largest `body_len`: 16 MiB.
pub const MAX_BODY_LEN: u32 = 16 << 20;
/// Block flag bit 0: a reader that does not know the type may skip the block.
pub const FLAG_SKIPPABLE: u8 = 1;

/// Block types of a log segment (`format.md` A.4).
pub mod block_type {
    /// Readings, in arrival order (`format.md` A.5).
    pub const FACTS: u8 = 0x01;
    /// Definitions and the other entries (`format.md` A.6).
    pub const ENTRIES: u8 = 0x02;
    /// Ends a batch (`format.md` A.7).
    pub const COMMIT: u8 = 0x0F;
    /// Written at seal: which series the segment holds (`format.md` A.8). Skippable.
    pub const SEGMENT_INDEX: u8 = 0x10;
    /// Written at seal: marks the file as sealed (`format.md` A.9).
    pub const TRAILER: u8 = 0x7F;

    /// Whether a log segment reader of this version knows `t`.
    pub fn is_known(t: u8) -> bool {
        matches!(t, FACTS | ENTRIES | COMMIT | SEGMENT_INDEX | TRAILER)
    }
}

/// A block header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockHeader {
    /// Block type; 0 is never valid.
    pub block_type: u8,
    /// Flags ([`FLAG_SKIPPABLE`]); the other bits are reserved.
    pub flags: u8,
    /// Body length, padding excluded.
    pub body_len: u32,
    /// CRC32C of the body.
    pub body_crc32c: u32,
}

impl BlockHeader {
    /// The header of a block holding `body`.
    pub fn for_body(block_type: u8, flags: u8, body: &[u8]) -> Result<Self, EncodeError> {
        if block_type == 0 {
            return Err(EncodeError::BlockType(0));
        }
        let body_len = u32::try_from(body.len())
            .ok()
            .filter(|&n| n <= MAX_BODY_LEN)
            .ok_or(EncodeError::BodyTooLong(body.len()))?;
        Ok(Self {
            block_type,
            flags,
            body_len,
            body_crc32c: crc32c(body),
        })
    }

    /// Whether the `skippable` flag is set.
    pub fn skippable(&self) -> bool {
        self.flags & FLAG_SKIPPABLE != 0
    }

    /// Header, body and padding: the distance to the next block.
    pub fn block_len(&self) -> u64 {
        BLOCK_HEADER_LEN as u64 + u64::from(self.body_len).div_ceil(8) * 8
    }

    /// The 16 bytes of this header, `header_crc32c` included.
    pub fn encode(&self) -> [u8; BLOCK_HEADER_LEN] {
        let mut b = [0u8; BLOCK_HEADER_LEN];
        b[0] = self.block_type;
        b[1] = self.flags;
        put(&mut b, 4, &self.body_len.to_le_bytes());
        put(&mut b, 8, &self.body_crc32c.to_le_bytes());
        let crc = crc32c(&b[..12]);
        put(&mut b, 12, &crc.to_le_bytes());
        b
    }

    /// The header in `b`, or `None` if its checksum does not match. Nothing else is judged.
    pub fn decode(b: &[u8; BLOCK_HEADER_LEN]) -> Option<Self> {
        if u32_at(b, 12) != crc32c(&b[..12]) {
            return None;
        }
        Some(Self {
            block_type: b[0],
            flags: b[1],
            body_len: u32_at(b, 4),
            body_crc32c: u32_at(b, 8),
        })
    }
}

/// Appends a block (header, body, zero padding to 8) to `out`.
pub fn encode_block(
    block_type: u8,
    flags: u8,
    body: &[u8],
    out: &mut Vec<u8>,
) -> Result<BlockHeader, EncodeError> {
    let h = BlockHeader::for_body(block_type, flags, body)?;
    out.extend_from_slice(&h.encode());
    out.extend_from_slice(body);
    out.resize(out.len() + (8 - body.len() % 8) % 8, 0);
    Ok(h)
}

/// A checksum-valid block found in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawBlock<'a> {
    /// File offset of the block header.
    pub offset: u64,
    /// The header.
    pub header: BlockHeader,
    /// The 16 header bytes as read, reserved bytes included: what `batch_crc32c` covers (A.7).
    pub header_bytes: [u8; BLOCK_HEADER_LEN],
    /// The `body_len` bytes of the body.
    pub body: &'a [u8],
}

impl RawBlock<'_> {
    /// File offset of the next block.
    pub fn next_offset(&self) -> u64 {
        self.offset + self.header.block_len()
    }
}

/// Why no valid block could be read at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockFault {
    /// Sixteen zero bytes.
    Zero,
    /// The header does not fit below the limit, its checksum does not match, or its type is 0.
    Header,
    /// `body_len` above 16 MiB, or the body does not fit below the limit.
    Length,
    /// The body checksum does not match.
    Body,
}

#[cfg(test)]
thread_local! {
    /// Calls of [`read_block`] on this thread, for tests of how much a scan reads.
    pub(crate) static BLOCK_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The 16 header bytes at `offset`, if they lie below `limit`, are not all zero, and hold a valid header checksum.
fn header_bytes(
    bytes: &[u8],
    offset: u64,
    limit: u64,
) -> Result<&[u8; BLOCK_HEADER_LEN], BlockFault> {
    let header_end = offset
        .checked_add(BLOCK_HEADER_LEN as u64)
        .ok_or(BlockFault::Header)?;
    if header_end > limit {
        return Err(BlockFault::Header);
    }
    let raw = slice(bytes, offset, BLOCK_HEADER_LEN as u64).ok_or(BlockFault::Header)?;
    raw.try_into().map_err(|_| BlockFault::Header)
}

/// The header of the block at `offset`, without reading or checking its body: a cheap test before
/// [`read_block`]. Type 0 is not a header.
pub(crate) fn peek_header(bytes: &[u8], offset: u64, limit: u64) -> Option<BlockHeader> {
    let raw = header_bytes(bytes, offset, limit).ok()?;
    BlockHeader::decode(raw).filter(|h| h.block_type != 0)
}

/// Reads the block at `offset`, accepting it only if it lies wholly below `limit`.
pub(crate) fn read_block(
    bytes: &[u8],
    offset: u64,
    limit: u64,
) -> Result<RawBlock<'_>, BlockFault> {
    #[cfg(test)]
    BLOCK_READS.with(|c| c.set(c.get() + 1));
    let header_end = offset
        .checked_add(BLOCK_HEADER_LEN as u64)
        .ok_or(BlockFault::Header)?;
    let raw = header_bytes(bytes, offset, limit)?;
    if raw.iter().all(|&b| b == 0) {
        return Err(BlockFault::Zero);
    }
    // Type 0 is never valid (A.2).
    let header = BlockHeader::decode(raw)
        .filter(|h| h.block_type != 0)
        .ok_or(BlockFault::Header)?;
    if header.body_len > MAX_BODY_LEN || header_end + u64::from(header.body_len) > limit {
        return Err(BlockFault::Length);
    }
    let body = slice(bytes, header_end, u64::from(header.body_len)).ok_or(BlockFault::Length)?;
    if crc32c(body) != header.body_crc32c {
        return Err(BlockFault::Body);
    }
    // Padding is not checked (`format.md` A, common rules).
    Ok(RawBlock {
        offset,
        header,
        header_bytes: *raw,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let mut out = Vec::new();
        let h = encode_block(block_type::FACTS, 0, &[1, 2, 3], &mut out).unwrap();
        assert_eq!(out.len(), 24);
        assert_eq!(out[0], 0x01);
        assert_eq!(&out[4..8], &3u32.to_le_bytes());
        assert_eq!(&out[8..12], &crc32c(&[1, 2, 3]).to_le_bytes());
        assert_eq!(&out[12..16], &crc32c(&out[..12]).to_le_bytes());
        assert_eq!(&out[16..24], &[1, 2, 3, 0, 0, 0, 0, 0]);
        assert_eq!(h.block_len(), 24);
        let b = read_block(&out, 0, 24).unwrap();
        assert_eq!(b.body, &[1, 2, 3]);
        assert_eq!(b.next_offset(), 24);
        // The body must lie below the limit; its padding need not.
        assert!(read_block(&out, 0, 19).is_ok());
        assert_eq!(read_block(&out, 0, 18), Err(BlockFault::Length));
        assert_eq!(read_block(&out, 0, 15), Err(BlockFault::Header));
        assert_eq!(read_block(&[0; 16], 0, 16), Err(BlockFault::Zero));
        // Reserved header bytes are not tested, and are kept as read.
        let mut r = out.clone();
        r[2] = 0x5A;
        let crc = crc32c(&r[..12]);
        r[12..16].copy_from_slice(&crc.to_le_bytes());
        let b = read_block(&r, 0, 24).unwrap();
        assert_eq!(b.header_bytes[..], r[..16]);
    }

    #[test]
    fn type_zero_and_long_bodies_are_refused() {
        assert_eq!(
            BlockHeader::for_body(0, 0, &[]),
            Err(EncodeError::BlockType(0))
        );
        let mut h = BlockHeader::for_body(1, 0, &[]).unwrap();
        h.body_len = MAX_BODY_LEN + 1;
        let b = h.encode();
        assert_eq!(read_block(&b, 0, u64::MAX), Err(BlockFault::Length));
        // A checksum-valid header of type 0, skippable or not, is not a block.
        for flags in [0, FLAG_SKIPPABLE] {
            let mut h = BlockHeader::for_body(1, flags, &[]).unwrap();
            h.block_type = 0;
            assert_eq!(read_block(&h.encode(), 0, 16), Err(BlockFault::Header));
        }
    }
}
