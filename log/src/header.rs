//! The 64-byte file header of every kvik file except the mark (`format.md` A.1).

use crate::bytes::{i64_at, put, u32_at, u64_at};
use crate::crc::crc32c;

/// Length of the file header.
pub const HEADER_LEN: usize = 64;
/// `magic`: ASCII `KVIK`.
pub const MAGIC: [u8; 4] = *b"KVIK";

/// File kinds (`format.md` A.1). Only the kinds this crate writes are listed.
pub mod kind {
    /// A log segment (`format.md` A.4).
    pub const LOG_SEGMENT: u8 = 1;
}

/// A file header. Reserved bytes are written as zero and not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileHeader {
    /// File kind ([`kind`]).
    pub kind: u8,
    /// Major format version.
    pub format_major: u8,
    /// Minor format version of the writer that created the file; diagnostic only (`format.md` A.3).
    pub format_minor: u8,
    /// Reserved flags; zero in v1.
    pub flags: u32,
    /// Generation in which the file was created.
    pub generation: u32,
    /// Unique per store and kind (`format.md` A.12).
    pub file_id: u64,
    /// Log segments: the sequence of the first entry this file may hold; 0 while a segment created ahead of use is
    /// not yet in use (`format.md` A.4). Other kinds: 0.
    pub base_sequence: u64,
    /// Creation time, nanoseconds since the Unix epoch.
    pub created_at: i64,
    /// The store's id (`format.md` A.12).
    pub store_id: [u8; 16],
}

impl FileHeader {
    /// A v1 header of `kind` with the given identity, `base_sequence` 0.
    pub fn new(
        kind: u8,
        generation: u32,
        file_id: u64,
        created_at: i64,
        store_id: [u8; 16],
    ) -> Self {
        Self {
            kind,
            format_major: crate::FORMAT_MAJOR,
            format_minor: crate::FORMAT_MINOR,
            flags: 0,
            generation,
            file_id,
            base_sequence: 0,
            created_at,
            store_id,
        }
    }

    /// The 64 bytes of this header, `header_crc32c` included.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        put(&mut b, 0, &MAGIC);
        b[4] = self.kind;
        b[5] = self.format_major;
        b[6] = self.format_minor;
        put(&mut b, 8, &self.flags.to_le_bytes());
        put(&mut b, 12, &self.generation.to_le_bytes());
        put(&mut b, 16, &self.file_id.to_le_bytes());
        put(&mut b, 24, &self.base_sequence.to_le_bytes());
        put(&mut b, 32, &self.created_at.to_le_bytes());
        put(&mut b, 40, &self.store_id);
        let crc = crc32c(&b[..60]);
        put(&mut b, 60, &crc.to_le_bytes());
        b
    }

    /// The header at the start of `bytes`, or `None` if it is short, its checksum does not match or its magic is
    /// wrong. Kind and version are not judged here.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let b = bytes.get(..HEADER_LEN)?;
        if u32_at(b, 60) != crc32c(&b[..60]) || b[..4] != MAGIC {
            return None;
        }
        let mut store_id = [0u8; 16];
        store_id.copy_from_slice(&b[40..56]);
        Some(Self {
            kind: b[4],
            format_major: b[5],
            format_minor: b[6],
            flags: u32_at(b, 8),
            generation: u32_at(b, 12),
            file_id: u64_at(b, 16),
            base_sequence: u64_at(b, 24),
            created_at: i64_at(b, 32),
            store_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_round_trip() {
        let mut h = FileHeader::new(kind::LOG_SEGMENT, 3, 0x0102_0304_0506_0708, -5, [7; 16]);
        h.base_sequence = 42;
        let b = h.encode();
        // Offsets from the A.1 table, checked independently of `decode`.
        assert_eq!(&b[0..4], b"KVIK");
        assert_eq!(b[4], 1);
        assert_eq!(b[5], 1);
        assert_eq!(b[6], 0);
        assert_eq!(&b[12..16], &3u32.to_le_bytes());
        assert_eq!(&b[16..24], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&b[24..32], &42u64.to_le_bytes());
        assert_eq!(&b[32..40], &(-5i64).to_le_bytes());
        assert_eq!(&b[40..56], &[7; 16]);
        assert_eq!(&b[56..60], &[0; 4]);
        assert_eq!(&b[60..64], &crc32c(&b[..60]).to_le_bytes());
        assert_eq!(FileHeader::decode(&b), Some(h));
    }

    #[test]
    fn damage_is_invalid() {
        let b = FileHeader::new(kind::LOG_SEGMENT, 0, 1, 0, [1; 16]).encode();
        for i in 0..HEADER_LEN * 8 {
            let mut d = b;
            d[i / 8] ^= 1 << (i % 8);
            assert_eq!(FileHeader::decode(&d), None, "bit {i}");
        }
        assert_eq!(FileHeader::decode(&b[..63]), None);
    }
}
