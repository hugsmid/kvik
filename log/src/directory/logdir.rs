//! The log directory (`format.md` A.18, kind 15): a root and one group file per 256 sealed segments, so that a
//! reader finds the sealed segments, their sequence and commit-time ranges and whether they hold entries without
//! listing `log/` or opening a segment.
//!
//! A group is valid only while every segment it lists is the catalogue's: same id, generation, last sequence and
//! plaintext hash (or, until the manifest lists sealed log segments, length and `file_crc32c`). A replaced segment
//! therefore invalidates its group alone, a restore every file of the older generation, and a manifest rebuild the
//! root alone, whose manifest offset and epoch no longer name a record.

use sha2::{Digest, Sha256};

use super::{
    DerivedError, Frame, block_type, decode_frame, encode_frame, position_at, put_position,
    put_stamp, records, stamp_at,
};
use crate::batch::SegmentIdentity;
use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::crc::crc32c;
use crate::error::EncodeError;
use crate::header::kind;
use crate::mark::ResetStamp;
use crate::scan::{ScanLimit, scan};
use crate::writer::Position;

/// Sealed segments per group.
pub const GROUP_SEGMENTS: usize = 256;
/// A segment record's size in v1.
pub const SEGMENT_RECORD_LEN: u16 = 104;
/// A group entry's size in v1.
pub const GROUP_ENTRY_LEN: u16 = 72;
/// A lost-range entry's size in v1.
pub const LOST_ENTRY_LEN: u16 = 40;
/// A quarantined-file entry's size in v1.
pub const QUARANTINED_ENTRY_LEN: u16 = 16;
const GROUP_PREFIX: usize = 16;
const ROOT_PREFIX: usize = 96;
/// A residence: the hot tier.
pub const RESIDENCE_HOT: u8 = 1;

/// One sealed segment as a group lists it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SegmentRecord {
    /// Its `file_id`.
    pub file_id: u64,
    /// The generation it was created in.
    pub generation: u32,
    /// It holds an entries block (A.8 `has_entries`).
    pub has_entries: bool,
    /// Its first and last sequence (A.9 trailer).
    pub first_sequence: u64,
    /// Its last sequence.
    pub last_sequence: u64,
    /// Its first commit time.
    pub first_commit_time: i64,
    /// Its last commit time.
    pub last_commit_time: i64,
    /// Facts it holds: the sum of its segment index's counts.
    pub fact_count: u64,
    /// The file's length.
    pub len: u64,
    /// Its trailer's `file_crc32c`.
    pub file_crc32c: u32,
    /// SHA-256 of the whole file.
    pub sha256: [u8; 32],
}

impl SegmentRecord {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.file_id.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.push(u8::from(self.has_entries));
        out.extend_from_slice(&[0; 3]);
        for v in [self.first_sequence, self.last_sequence] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [self.first_commit_time, self.last_commit_time] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.fact_count.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.file_crc32c.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.sha256);
    }

    fn decode(b: &[u8]) -> Self {
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&b[72..104]);
        SegmentRecord {
            file_id: u64_at(b, 0),
            generation: u32_at(b, 8),
            has_entries: b[12] & 1 != 0,
            first_sequence: u64_at(b, 16),
            last_sequence: u64_at(b, 24),
            first_commit_time: i64_at(b, 32),
            last_commit_time: i64_at(b, 40),
            fact_count: u64_at(b, 48),
            len: u64_at(b, 56),
            file_crc32c: u32_at(b, 64),
            sha256,
        }
    }

    /// Its first position.
    pub fn first(&self) -> Position {
        Position {
            generation: self.generation,
            sequence: self.first_sequence,
        }
    }

    /// Its last position.
    pub fn last(&self) -> Position {
        Position {
            generation: self.generation,
            sequence: self.last_sequence,
        }
    }

    /// The record of a sealed segment from its bytes: `None` if the bytes are not a sealed segment holding a batch.
    pub fn of_segment(bytes: &[u8], id: &SegmentIdentity) -> Option<SegmentRecord> {
        let r = scan(bytes, id, ScanLimit::End).ok()?;
        let seal = r.seal?;
        let t = seal.trailer;
        if t.batch_count == 0 || seal.end != bytes.len() as u64 {
            return None;
        }
        let index = seal.index.flatten();
        Some(SegmentRecord {
            file_id: id.file_id,
            generation: id.generation,
            has_entries: index
                .as_ref()
                .is_some_and(|i| i.flags & crate::body::INDEX_HAS_ENTRIES != 0),
            first_sequence: t.first_sequence,
            last_sequence: t.last_sequence,
            first_commit_time: t.first_commit_time,
            last_commit_time: t.last_commit_time,
            fact_count: index.map_or(0, |i| {
                i.records.iter().map(|r| u64::from(r.fact_count)).sum()
            }),
            len: bytes.len() as u64,
            file_crc32c: t.file_crc32c,
            sha256: Sha256::digest(bytes).into(),
        })
    }
}

/// A group: the records of up to 256 consecutive sealed segments, in position order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Its number: it holds the sealed segments `256 × number` to `256 × number + 255`, counted in position order.
    pub number: u32,
    /// The records.
    pub segments: Vec<SegmentRecord>,
}

impl Group {
    /// Its body (A.18).
    pub fn encode_body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(
            GROUP_PREFIX + self.segments.len() * usize::from(SEGMENT_RECORD_LEN),
        );
        b.extend_from_slice(&self.number.to_le_bytes());
        b.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        b.extend_from_slice(&SEGMENT_RECORD_LEN.to_le_bytes());
        b.extend_from_slice(&[0; 6]);
        for s in &self.segments {
            s.encode(&mut b);
        }
        b
    }

    /// The bytes of the file.
    pub fn encode(&self, frame: Frame, store_id: [u8; 16]) -> Result<Vec<u8>, EncodeError> {
        encode_frame(
            kind::LOG_DIRECTORY,
            block_type::GROUP,
            frame,
            store_id,
            &self.encode_body(),
        )
    }

    /// Decodes a group file: its frame, the group and its body's CRC32C (what the root's entry names).
    pub fn decode(
        bytes: &[u8],
        store_id: &[u8; 16],
    ) -> Result<(Frame, (Group, u32)), DerivedError> {
        let (frame, body) = decode_frame(bytes, store_id, kind::LOG_DIRECTORY, block_type::GROUP)?;
        if body.len() < GROUP_PREFIX {
            return Err(DerivedError::Invalid("group body"));
        }
        let count = u32_at(body, 4);
        if count as usize > GROUP_SEGMENTS {
            return Err(DerivedError::Invalid("group count"));
        }
        let segments: Vec<_> = records(
            body,
            GROUP_PREFIX,
            count,
            u16_at(body, 8),
            usize::from(SEGMENT_RECORD_LEN),
        )?
        .map(SegmentRecord::decode)
        .collect();
        let ordered = segments.windows(2).all(|w| w[0].last() < w[1].first());
        let ranges = segments.iter().all(|s| {
            s.first_sequence <= s.last_sequence && s.first_commit_time <= s.last_commit_time
        });
        if !ordered || !ranges {
            return Err(DerivedError::Invalid("group records"));
        }
        let g = Group {
            number: u32_at(body, 0),
            segments,
        };
        Ok((frame, (g, crc32c(body))))
    }

    /// Its entry in the root.
    pub fn entry(&self) -> GroupEntry {
        let first = self.segments.first();
        let last = self.segments.last();
        GroupEntry {
            number: self.number,
            segment_count: self.segments.len() as u32,
            first_file_id: first.map_or(0, |s| s.file_id),
            last_file_id: last.map_or(0, |s| s.file_id),
            first: first.map_or(Position::default(), SegmentRecord::first),
            last: last.map_or(Position::default(), SegmentRecord::last),
            first_commit_time: first.map_or(0, |s| s.first_commit_time),
            last_commit_time: last.map_or(0, |s| s.last_commit_time),
            residence: RESIDENCE_HOT,
            body_crc32c: crc32c(&self.encode_body()),
        }
    }
}

/// A group as the root names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupEntry {
    /// The group's number.
    pub number: u32,
    /// Segments in it.
    pub segment_count: u32,
    /// Its first segment's id.
    pub first_file_id: u64,
    /// Its last segment's id.
    pub last_file_id: u64,
    /// Its first segment's first position.
    pub first: Position,
    /// Its last segment's last position.
    pub last: Position,
    /// Its first commit time.
    pub first_commit_time: i64,
    /// Its last commit time.
    pub last_commit_time: i64,
    /// Where its segments are: 1 hot (8.1 adds the others).
    pub residence: u8,
    /// CRC32C of the group file's body: which generation of the group this root was written with.
    pub body_crc32c: u32,
}

impl GroupEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.number.to_le_bytes());
        out.extend_from_slice(&self.segment_count.to_le_bytes());
        out.extend_from_slice(&self.first_file_id.to_le_bytes());
        out.extend_from_slice(&self.last_file_id.to_le_bytes());
        put_position(out, self.first);
        put_position(out, self.last);
        out.extend_from_slice(&self.first_commit_time.to_le_bytes());
        out.extend_from_slice(&self.last_commit_time.to_le_bytes());
        out.push(self.residence);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&self.body_crc32c.to_le_bytes());
    }

    fn decode(b: &[u8]) -> Self {
        GroupEntry {
            number: u32_at(b, 0),
            segment_count: u32_at(b, 4),
            first_file_id: u64_at(b, 8),
            last_file_id: u64_at(b, 16),
            first: position_at(b, 24),
            last: position_at(b, 36),
            first_commit_time: i64_at(b, 48),
            last_commit_time: i64_at(b, 56),
            residence: b[64],
            body_crc32c: u32_at(b, 68),
        }
    }
}

/// A lost range as the root carries it (the manifest's lost-range records as of its epoch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LostEntry {
    /// The first lost position.
    pub first: Position,
    /// The last lost position.
    pub last: Position,
    /// The segment.
    pub file_id: u64,
    /// The cause (A.14 field 7).
    pub cause: u8,
}

/// A quarantined file (roadmap 8.1); none are written before the quarantine record is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QuarantinedEntry {
    /// Its kind.
    pub kind: u8,
    /// The generation it was created in.
    pub generation: u32,
    /// Its `file_id`.
    pub file_id: u64,
}

/// The root of the log directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// The epoch of the manifest record the root is complete through.
    pub epoch: u64,
    /// The offset just past that record: a reader parses the manifest from here.
    pub manifest_end: u64,
    /// CRC32C of that record's block bytes, so that the record is matched by content.
    pub manifest_crc32c: u32,
    /// Every sealed segment with a last position up to this one is listed; at or below the last synced mark.
    pub through: Position,
    /// Sealed segments listed, in all groups.
    pub segment_count: u64,
    /// The reset stamp of the mark when it was written; `None` before any reset.
    pub stamp: Option<ResetStamp>,
    /// C(E), the highest position held by a live compacted file (roadmap 6.3e); `None` until compaction publishes.
    pub compacted_through: Option<Position>,
    /// The groups, by number from 0.
    pub groups: Vec<GroupEntry>,
    /// Lost ranges.
    pub lost: Vec<LostEntry>,
    /// Quarantined files.
    pub quarantined: Vec<QuarantinedEntry>,
}

impl Root {
    /// Its body (A.18). The compacted files' catalogue is empty in v1, its record size 0 (roadmap 6.3e lays it out).
    pub fn encode_body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.epoch.to_le_bytes());
        b.extend_from_slice(&self.manifest_end.to_le_bytes());
        b.extend_from_slice(&self.through.generation.to_le_bytes());
        b.extend_from_slice(&self.manifest_crc32c.to_le_bytes());
        b.extend_from_slice(&self.through.sequence.to_le_bytes());
        b.extend_from_slice(&self.segment_count.to_le_bytes());
        put_stamp(&mut b, self.stamp);
        let ce = self.compacted_through.unwrap_or_default();
        b.extend_from_slice(&ce.generation.to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&ce.sequence.to_le_bytes());
        for n in [
            self.groups.len(),
            self.lost.len(),
            self.quarantined.len(),
            0,
        ] {
            b.extend_from_slice(&(n as u32).to_le_bytes());
        }
        for n in [GROUP_ENTRY_LEN, LOST_ENTRY_LEN, QUARANTINED_ENTRY_LEN, 0] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        for g in &self.groups {
            g.encode(&mut b);
        }
        for l in &self.lost {
            put_position(&mut b, l.first);
            put_position(&mut b, l.last);
            b.extend_from_slice(&l.file_id.to_le_bytes());
            b.push(l.cause);
            b.extend_from_slice(&[0; 7]);
        }
        for q in &self.quarantined {
            b.push(q.kind);
            b.extend_from_slice(&[0; 3]);
            b.extend_from_slice(&q.generation.to_le_bytes());
            b.extend_from_slice(&q.file_id.to_le_bytes());
        }
        b
    }

    /// The bytes of the file.
    pub fn encode(&self, frame: Frame, store_id: [u8; 16]) -> Result<Vec<u8>, EncodeError> {
        encode_frame(
            kind::LOG_DIRECTORY,
            block_type::ROOT,
            frame,
            store_id,
            &self.encode_body(),
        )
    }

    /// Decodes a root file (A.18 checks).
    pub fn decode(bytes: &[u8], store_id: &[u8; 16]) -> Result<(Frame, Root), DerivedError> {
        let (frame, body) = decode_frame(bytes, store_id, kind::LOG_DIRECTORY, block_type::ROOT)?;
        if body.len() < ROOT_PREFIX {
            return Err(DerivedError::Invalid("root body"));
        }
        let counts = [72, 76, 80, 84].map(|at| u32_at(body, at));
        let sizes = [88, 90, 92, 94].map(|at| u16_at(body, at));
        let mut at = ROOT_PREFIX;
        let mut take = |i: usize, min: u16| -> Result<Vec<&[u8]>, DerivedError> {
            let v: Vec<_> = records(body, at, counts[i], sizes[i], usize::from(min))?.collect();
            at += v.len() * usize::from(sizes[i]);
            Ok(v)
        };
        let groups: Vec<_> = take(0, GROUP_ENTRY_LEN)?
            .into_iter()
            .map(GroupEntry::decode)
            .collect();
        let lost: Vec<_> = take(1, LOST_ENTRY_LEN)?
            .into_iter()
            .map(|b| LostEntry {
                first: position_at(b, 0),
                last: position_at(b, 12),
                file_id: u64_at(b, 24),
                cause: b[32],
            })
            .collect();
        let quarantined: Vec<_> = take(2, QUARANTINED_ENTRY_LEN)?
            .into_iter()
            .map(|b| QuarantinedEntry {
                kind: b[0],
                generation: u32_at(b, 4),
                file_id: u64_at(b, 8),
            })
            .collect();
        take(3, 0)?;
        if at != body.len() {
            return Err(DerivedError::Invalid("bytes after the records"));
        }
        let ce = Position {
            generation: u32_at(body, 56),
            sequence: u64_at(body, 64),
        };
        let root = Root {
            epoch: u64_at(body, 0),
            manifest_end: u64_at(body, 8),
            manifest_crc32c: u32_at(body, 20),
            through: Position {
                generation: u32_at(body, 16),
                sequence: u64_at(body, 24),
            },
            segment_count: u64_at(body, 32),
            stamp: stamp_at(body, 40),
            compacted_through: (ce.sequence != 0).then_some(ce),
            groups,
            lost,
            quarantined,
        };
        root.check()?;
        Ok((frame, root))
    }

    /// The structural checks of A.18: groups numbered from 0, every group but the last full, counts summing to
    /// `segment_count`, each group joining the one before it, the last group ending at `through`.
    fn check(&self) -> Result<(), DerivedError> {
        let bad = || Err(DerivedError::Invalid("root groups"));
        let mut sum: u64 = 0;
        for (i, g) in self.groups.iter().enumerate() {
            let full = g.segment_count as usize == GROUP_SEGMENTS;
            let last = i + 1 == self.groups.len();
            if g.number as usize != i
                || g.segment_count == 0
                || g.segment_count as usize > GROUP_SEGMENTS
                || (!last && !full)
                || g.first > g.last
            {
                return bad();
            }
            if i > 0 && !joins(self.groups[i - 1].last, g.first, &self.lost) {
                return bad();
            }
            sum += u64::from(g.segment_count);
        }
        let end = self.groups.last().map_or(Position::default(), |g| g.last);
        if sum != self.segment_count || end != self.through {
            return bad();
        }
        Ok(())
    }
}

pub use super::catalogue::{
    Catalogue, Listed, LogDirectory, catalogue_from_disk, group, known_records, load, ordered,
    read_trailer,
};

/// Whether a segment starting at `next` follows one ending at `prev` with no sealed segment between (A.18
/// Contiguity): `next` is the position after `prev`, or a lost range covers every position between them, or `next`
/// starts a later generation.
pub fn joins(prev: Position, next: Position, lost: &[LostEntry]) -> bool {
    if next.generation != prev.generation {
        return next.generation > prev.generation;
    }
    if next.sequence <= prev.sequence {
        return false;
    }
    let (from, to) = (prev.sequence + 1, next.sequence - 1);
    from > to
        || lost.iter().any(|l| {
            l.first.generation == prev.generation
                && l.first.sequence <= from
                && l.last.sequence >= to
        })
}

/// Whether a group's records follow one another with no sealed segment between them.
pub fn contiguous(g: &Group, lost: &[LostEntry]) -> bool {
    g.segments
        .windows(2)
        .all(|w| joins(w[0].last(), w[1].first(), lost))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(file_id: u64, first: u64, last: u64) -> SegmentRecord {
        SegmentRecord {
            file_id,
            generation: 0,
            has_entries: file_id.is_multiple_of(2),
            first_sequence: first,
            last_sequence: last,
            first_commit_time: first as i64 * 10,
            last_commit_time: last as i64 * 10,
            fact_count: last - first + 1,
            len: 4096,
            file_crc32c: file_id as u32,
            sha256: [file_id as u8; 32],
        }
    }

    fn frame() -> Frame {
        Frame {
            generation: 0,
            file_id: 3,
            written_at: 9,
        }
    }

    #[test]
    fn a_group_whose_records_are_out_of_order_is_invalid() {
        let g = Group {
            number: 0,
            segments: vec![seg(2, 6, 9), seg(1, 1, 5)],
        };
        let bytes = g.encode(frame(), [1; 16]).unwrap();
        assert!(Group::decode(&bytes, &[1; 16]).is_err());
    }

    #[test]
    fn a_group_round_trips_and_its_entry_names_its_body() {
        let g = Group {
            number: 0,
            segments: vec![seg(1, 1, 5), seg(2, 6, 9)],
        };
        let bytes = g.encode(frame(), [1; 16]).unwrap();
        let (f, (back, crc)) = Group::decode(&bytes, &[1; 16]).unwrap();
        assert_eq!((f, &back), (frame(), &g));
        assert_eq!(g.entry().body_crc32c, crc);
    }

    #[test]
    fn a_root_round_trips_and_refuses_a_gap_in_its_groups() {
        let g = Group {
            number: 0,
            segments: vec![seg(1, 1, 5)],
        };
        let root = Root {
            epoch: 2,
            manifest_end: 200,
            manifest_crc32c: 0xabcd,
            through: Position {
                generation: 0,
                sequence: 5,
            },
            segment_count: 1,
            stamp: Some(ResetStamp {
                sequence: 3,
                nonce: 7,
            }),
            compacted_through: None,
            groups: vec![g.entry()],
            lost: vec![LostEntry {
                first: Position {
                    generation: 0,
                    sequence: 2,
                },
                last: Position {
                    generation: 0,
                    sequence: 2,
                },
                file_id: 1,
                cause: 1,
            }],
            quarantined: vec![],
        };
        let bytes = root.encode(frame(), [1; 16]).unwrap();
        assert_eq!(
            Root::decode(&bytes, &[1; 16]).unwrap(),
            (frame(), root.clone())
        );
        let mut skewed = root.clone();
        skewed.groups[0].number = 1;
        let bytes = skewed.encode(frame(), [1; 16]).unwrap();
        assert!(Root::decode(&bytes, &[1; 16]).is_err());
        let mut behind = root;
        behind.through.sequence = 9;
        let bytes = behind.encode(frame(), [1; 16]).unwrap();
        assert!(Root::decode(&bytes, &[1; 16]).is_err());
    }
}
