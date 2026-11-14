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
    /// The header does not fit below the limit, or its checksum does not match.
    Header,
    /// `body_len` above 16 MiB, or the body does not fit below the limit.
    Length,
    /// The body checksum does not match.
    Body,
}

/// Reads the block at `offset`, accepting it only if it lies wholly below `limit`.
pub(crate) fn read_block(
    bytes: &[u8],
    offset: u64,
    limit: u64,
) -> Result<RawBlock<'_>, BlockFault> {
    let header_end = offset
        .checked_add(BLOCK_HEADER_LEN as u64)
        .ok_or(BlockFault::Header)?;
    if header_end > limit {
        return Err(BlockFault::Header);
    }
    let raw = slice(bytes, offset, BLOCK_HEADER_LEN as u64).ok_or(BlockFault::Header)?;
    let raw: &[u8; BLOCK_HEADER_LEN] = raw.try_into().map_err(|_| BlockFault::Header)?;
    if raw.iter().all(|&b| b == 0) {
        return Err(BlockFault::Zero);
    }
    let header = BlockHeader::decode(raw).ok_or(BlockFault::Header)?;
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
    }
}
