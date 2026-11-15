//! The durable mark, `log/MARK` (`format.md` A.10): two 128-byte slots, the valid one with the higher counter wins.
//!
//! This module encodes and decodes slots and picks the mark from the file's bytes; reading the file (positional
//! reads, the retries of a reader process) and writing it belong to the commit path and the reader (roadmap 4.4,
//! 4.6).

use crate::bytes::{put, u32_at, u64_at};
use crate::crc::crc32c;
use crate::error::{Element, FormatError};

/// Length of the mark file.
pub const MARK_LEN: usize = 4_224;
/// Length of one slot.
pub const SLOT_LEN: usize = 128;
/// File offsets of the two slots, in separate sectors.
pub const SLOT_OFFSETS: [usize; 2] = [0, 4_096];
/// `magic`: ASCII `KVIKMARK`.
pub const MARK_MAGIC: [u8; 8] = *b"KVIKMARK";
/// The mark version this crate reads and writes.
pub const MARK_VERSION: u32 = 3;

/// A reset stamp (`spec.md` §3 Recovery, §7 Readers): the position of the store's last reset point written by a
/// start (a recovery entry), or of a restore, and a random nonce, never zero, that tells two resets at one position
/// apart. Zero, the default, means no reset since the store was created. The mark carries the current one; each
/// recovery entry carries its own (`format.md` A.6 field 41, A.10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResetStamp {
    /// The reset's sequence: the recovery entry's position F, or the first position of a restored generation.
    pub sequence: u64,
    /// Random, never zero once a reset has happened.
    pub nonce: u64,
}

impl ResetStamp {
    /// The 16 bytes: `sequence` then `nonce`.
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.sequence.to_le_bytes());
        b[8..].copy_from_slice(&self.nonce.to_le_bytes());
        b
    }

    /// The stamp in 16 bytes; `None` for another length.
    pub fn decode(b: &[u8]) -> Option<Self> {
        (b.len() == 16).then(|| Self {
            sequence: u64_at(b, 0),
            nonce: u64_at(b, 8),
        })
    }

    /// A new stamp for a reset at `sequence`, with a nonce from the operating system's randomness (std's hash
    /// keys); never zero.
    pub fn new_at(sequence: u64) -> Self {
        use std::hash::BuildHasher;
        let mut n = 0u64;
        let mut salt = 0u64;
        while n == 0 {
            n = std::collections::hash_map::RandomState::new().hash_one((sequence, salt));
            salt += 1;
        }
        Self { sequence, nonce: n }
    }
}

/// One mark slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MarkSlot {
    /// The current generation.
    pub generation: u32,
    /// Must equal the store descriptor's.
    pub store_id: [u8; 16],
    /// Boot identity of the writer; zero where the platform has none.
    pub boot_id: [u8; 16],
    /// 1 at creation, +1 per update.
    pub counter: u64,
    /// The segment the mark points into.
    pub file_id: u64,
    /// Just past the last durable commit block, padding included.
    pub offset: u64,
    /// The last durable sequence.
    pub last_sequence: u64,
    /// The highest series id assigned.
    pub max_series_id: u32,
    /// The current reset stamp, in bytes 84..100 (formerly reserved, so the version is unchanged).
    pub stamp: ResetStamp,
}

/// One slot as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotRead {
    /// Valid: magic, checksum, known version and store id.
    Valid(MarkSlot),
    /// Torn, damaged, never written, or of another store.
    Invalid,
    /// Valid magic and checksum, unknown version: a version refusal (A.10).
    UnknownVersion(u32),
}

impl MarkSlot {
    /// The 128 bytes of this slot, version 3, `crc32c` included.
    pub fn encode(&self) -> [u8; SLOT_LEN] {
        let mut b = [0u8; SLOT_LEN];
        put(&mut b, 0, &MARK_MAGIC);
        put(&mut b, 8, &MARK_VERSION.to_le_bytes());
        put(&mut b, 12, &self.generation.to_le_bytes());
        put(&mut b, 16, &self.store_id);
        put(&mut b, 32, &self.boot_id);
        put(&mut b, 48, &self.counter.to_le_bytes());
        put(&mut b, 56, &self.file_id.to_le_bytes());
        put(&mut b, 64, &self.offset.to_le_bytes());
        put(&mut b, 72, &self.last_sequence.to_le_bytes());
        put(&mut b, 80, &self.max_series_id.to_le_bytes());
        put(&mut b, 84, &self.stamp.encode());
        let crc = crc32c(&b[..124]);
        put(&mut b, 124, &crc.to_le_bytes());
        b
    }

    /// The slot in `b` (128 bytes or more; the rest is ignored), judged for the store `store_id`.
    pub fn decode(b: &[u8], store_id: &[u8; 16]) -> SlotRead {
        let Some(b) = b.get(..SLOT_LEN) else {
            return SlotRead::Invalid;
        };
        if b[..8] != MARK_MAGIC || u32_at(b, 124) != crc32c(&b[..124]) {
            return SlotRead::Invalid;
        }
        let version = u32_at(b, 8);
        if version != MARK_VERSION {
            return SlotRead::UnknownVersion(version);
        }
        if b[16..32] != store_id[..] {
            return SlotRead::Invalid;
        }
        let mut boot_id = [0u8; 16];
        boot_id.copy_from_slice(&b[32..48]);
        SlotRead::Valid(Self {
            generation: u32_at(b, 12),
            store_id: *store_id,
            boot_id,
            counter: u64_at(b, 48),
            file_id: u64_at(b, 56),
            offset: u64_at(b, 64),
            last_sequence: u64_at(b, 72),
            max_series_id: u32_at(b, 80),
            // A zero nonce is no stamp, whatever the sequence (A.10).
            stamp: ResetStamp::decode(&b[84..100])
                .filter(|st| st.nonce != 0)
                .unwrap_or_default(),
        })
    }
}

/// The two slots of a mark file. A slot beyond the end of `file` is invalid.
pub fn read_slots(file: &[u8], store_id: &[u8; 16]) -> [SlotRead; 2] {
    SLOT_OFFSETS.map(|at| {
        file.get(at..)
            .map_or(SlotRead::Invalid, |b| MarkSlot::decode(b, store_id))
    })
}

/// The mark: the valid slot with the higher counter (slot 0 on a tie), with its slot number; `None` if neither slot
/// is valid (a reader retries; for recovery the mark is unreadable). A slot of unknown version is refused, for a
/// writer and a reader alike.
pub fn read_mark(
    file: &[u8],
    store_id: &[u8; 16],
) -> Result<Option<(usize, MarkSlot)>, FormatError> {
    let slots = read_slots(file, store_id);
    let mut best: Option<(usize, MarkSlot)> = None;
    for (i, s) in slots.into_iter().enumerate() {
        match s {
            SlotRead::UnknownVersion(v) => {
                return Err(FormatError::Unsupported {
                    offset: SLOT_OFFSETS[i] as u64,
                    element: Element::MarkVersion(v),
                });
            }
            SlotRead::Valid(m) if best.is_none_or(|(_, b)| m.counter > b.counter) => {
                best = Some((i, m))
            }
            _ => {}
        }
    }
    Ok(best)
}

/// The slot the next update writes: the one that does not hold the mark (A.10).
pub fn slot_to_write(file: &[u8], store_id: &[u8; 16]) -> Result<usize, FormatError> {
    Ok(match read_mark(file, store_id)? {
        Some((i, _)) => 1 - i,
        None => 0,
    })
}

/// A mark file holding the given slots; an absent slot is zero.
pub fn encode_mark_file(slots: [Option<MarkSlot>; 2]) -> Vec<u8> {
    let mut b = vec![0u8; MARK_LEN];
    for (s, at) in slots.iter().zip(SLOT_OFFSETS) {
        if let Some(s) = s {
            b[at..at + SLOT_LEN].copy_from_slice(&s.encode());
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(counter: u64) -> MarkSlot {
        MarkSlot {
            generation: 1,
            store_id: [9; 16],
            boot_id: [3; 16],
            counter,
            file_id: 4,
            offset: 640,
            last_sequence: 12,
            max_series_id: 2,
            stamp: ResetStamp {
                sequence: 11,
                nonce: 0x0102,
            },
        }
    }

    #[test]
    fn layout() {
        let b = slot(7).encode();
        assert_eq!(&b[..8], b"KVIKMARK");
        assert_eq!(&b[8..12], &3u32.to_le_bytes());
        assert_eq!(&b[48..56], &7u64.to_le_bytes());
        assert_eq!(&b[64..72], &640u64.to_le_bytes());
        assert_eq!(&b[84..92], &11u64.to_le_bytes());
        assert_eq!(&b[92..100], &0x0102u64.to_le_bytes());
        assert_eq!(&b[100..124], &[0; 24]);
        assert_eq!(&b[124..], &crc32c(&b[..124]).to_le_bytes());
    }

    #[test]
    fn a_zero_nonce_is_no_stamp() {
        let mut s = slot(7);
        s.stamp.nonce = 0;
        let MarkSlot { stamp, .. } = match MarkSlot::decode(&s.encode(), &[9; 16]) {
            SlotRead::Valid(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(stamp, ResetStamp::default());
    }

    #[test]
    fn higher_counter_wins_and_torn_slot_falls_back() {
        let f = encode_mark_file([Some(slot(7)), Some(slot(8))]);
        assert_eq!(read_mark(&f, &[9; 16]), Ok(Some((1, slot(8)))));
        assert_eq!(slot_to_write(&f, &[9; 16]), Ok(0));
        let mut torn = f.clone();
        torn[4_096 + 50] ^= 0xFF;
        assert_eq!(read_mark(&torn, &[9; 16]), Ok(Some((0, slot(7)))));
        assert_eq!(slot_to_write(&torn, &[9; 16]), Ok(1));
        assert_eq!(read_mark(&f, &[8; 16]), Ok(None));
        assert_eq!(read_mark(&f[..4_100], &[9; 16]), Ok(Some((0, slot(7)))));
        assert_eq!(read_mark(&[], &[9; 16]), Ok(None));
    }

    #[test]
    fn unknown_version_is_refused() {
        let mut f = encode_mark_file([Some(slot(7)), None]);
        f[8..12].copy_from_slice(&4u32.to_le_bytes());
        let crc = crc32c(&f[..124]);
        f[124..128].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            read_mark(&f, &[9; 16]),
            Err(FormatError::Unsupported {
                offset: 0,
                element: Element::MarkVersion(4)
            })
        );
    }
}
