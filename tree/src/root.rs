//! The tree root (`kind = 19`, `format-tree.md` T.12): the live parts, the checkpoint K, what the parts cover, the
//! floors and the retired files. An A.1 header and one block that ends the file, as the derived files of A.18.

use std::collections::BTreeSet;

use kvik_log::block::encode_block;
use kvik_log::header::FileHeader;
use kvik_log::mark::ResetStamp;
use kvik_log::writer::Position;

use crate::frame::{
    Cursor, EncodeError, FileError, LEVEL_RECORD, Refusal, block, check_header_fields, expect_type,
    fail, header, header_len, put_levels, put_position, width_of,
};
use crate::level::Level;
use crate::part::Class;

/// File kind of a tree root (`format.md` A.1).
pub const KIND: u8 = 19;
/// The root's one block type (T.12).
pub const ROOT_BLOCK: u8 = 0x64;

const HEAD: usize = 88;
const SIZES: [usize; 5] = [LEVEL_RECORD, 80, 24, 24, 16];

/// A live part as the root names it (T.12): its trailer's identity and extent, and its length and checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartRef {
    /// The part's `file_id`.
    pub file_id: u64,
    /// The part's `seq`.
    pub seq: u64,
    /// The part's class.
    pub class: Class,
    /// The part's `file_crc32c`.
    pub file_crc32c: u32,
    /// Period start.
    pub period_start: i64,
    /// Period length in nanoseconds.
    pub period_len: u64,
    /// Least entry start, 0 for an empty part.
    pub least_start: i64,
    /// Greatest entry end, 0 for an empty part.
    pub greatest_end: i64,
    /// Least series id.
    pub least_series: u64,
    /// Greatest series id.
    pub greatest_series: u64,
    /// File length in bytes.
    pub byte_len: u64,
}

/// A time range at one level for which the visible parts are authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Covered {
    /// Level id.
    pub level: u8,
    /// Start, inclusive, on the level's width.
    pub from: i64,
    /// End, exclusive, on the level's width.
    pub to: i64,
}

/// A series floor: entries of parts with a lower `seq` are invisible for the series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floor {
    /// Series id.
    pub series: u64,
    /// Least visible part `seq`.
    pub seq: u64,
    /// Open floor: the series is not served from the tree.
    pub rebuilding: bool,
}

/// A part no longer live, kept until its grace time has passed (design 7.1b invariant T6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retired {
    /// The part's `file_id`.
    pub file_id: u64,
    /// When it was retired, on the commit clock.
    pub retired_at: i64,
}

/// A decoded tree root.
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    /// The A.1 header: kind 19, `file_id` equal to `seq`.
    pub header: FileHeader,
    /// Root sequence, rising by one per root.
    pub seq: u64,
    /// The checkpoint K: visible nodes are those of the current view at K.
    pub checkpoint: Position,
    /// The mark's reset stamp when written.
    pub reset: ResetStamp,
    /// The next tree part `file_id`.
    pub next_file_id: u64,
    /// Global floor: entries of parts with a lower `seq` are invisible.
    pub floor: u64,
    /// The level table.
    pub levels: Vec<Level>,
    /// Live parts in ascending `file_id`.
    pub parts: Vec<PartRef>,
    /// Covered ranges in ascending level and start.
    pub covered: Vec<Covered>,
    /// Floors in ascending series.
    pub floors: Vec<Floor>,
    /// Retired parts in ascending `file_id`.
    pub retired: Vec<Retired>,
}

impl Root {
    /// The canonical bytes of this root (T.12); refuses whatever a reader would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        check_header_fields(&self.header, KIND)?;
        let counts = [
            self.levels.len(),
            self.parts.len(),
            self.covered.len(),
            self.floors.len(),
            self.retired.len(),
        ];
        let mut b =
            Vec::with_capacity(HEAD + counts.iter().zip(SIZES).map(|(c, s)| c * s).sum::<usize>());
        b.extend_from_slice(&self.seq.to_le_bytes());
        put_position(&mut b, self.checkpoint);
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&self.reset.encode());
        b.extend_from_slice(&self.next_file_id.to_le_bytes());
        b.extend_from_slice(&self.floor.to_le_bytes());
        for c in counts {
            let c =
                u32::try_from(c).map_err(|_| EncodeError::Shape("an array above u32 records"))?;
            b.extend_from_slice(&c.to_le_bytes());
        }
        for s in SIZES {
            b.extend_from_slice(&(s as u16).to_le_bytes());
        }
        b.extend_from_slice(&[0; 2]);
        put_levels(&mut b, &self.levels);
        for p in &self.parts {
            b.extend_from_slice(&p.file_id.to_le_bytes());
            b.extend_from_slice(&p.seq.to_le_bytes());
            b.push(p.class as u8);
            b.extend_from_slice(&[0; 3]);
            b.extend_from_slice(&p.file_crc32c.to_le_bytes());
            for v in [
                p.period_start as u64,
                p.period_len,
                p.least_start as u64,
                p.greatest_end as u64,
            ] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            for v in [p.least_series, p.greatest_series, p.byte_len] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        for c in &self.covered {
            b.push(c.level);
            b.extend_from_slice(&[0; 7]);
            b.extend_from_slice(&c.from.to_le_bytes());
            b.extend_from_slice(&c.to.to_le_bytes());
        }
        for f in &self.floors {
            b.extend_from_slice(&f.series.to_le_bytes());
            b.extend_from_slice(&f.seq.to_le_bytes());
            b.push(u8::from(f.rebuilding));
            b.extend_from_slice(&[0; 7]);
        }
        for r in &self.retired {
            b.extend_from_slice(&r.file_id.to_le_bytes());
            b.extend_from_slice(&r.retired_at.to_le_bytes());
        }
        let mut out = self.header.encode().to_vec();
        encode_block(ROOT_BLOCK, 0, &b, &mut out)
            .map_err(|_| EncodeError::Shape("a root body above 16 MiB"))?;
        if Root::decode(&out)? != *self {
            return Err(EncodeError::Shape("a root that does not survive decoding"));
        }
        Ok(out)
    }

    /// The root in `bytes`, with every check of T.12; whatever is accepted encodes to the same bytes.
    pub fn decode(bytes: &[u8]) -> Result<Root, FileError> {
        let h = header(bytes, KIND)?;
        let blk = block(bytes, header_len(), bytes.len() as u64)?;
        expect_type(&blk, ROOT_BLOCK, &[ROOT_BLOCK])?;
        if blk.next != bytes.len() as u64 {
            return fail(blk.next, Refusal::BlockOrder);
        }
        let mut c = Cursor::new(blk.body, blk.at + 16);
        let seq = c.u64()?;
        if seq != h.file_id {
            return fail(16, Refusal::RootSeq);
        }
        let checkpoint = c.position()?;
        c.reserved(4)?;
        let reset = ResetStamp {
            sequence: c.u64()?,
            nonce: c.u64()?,
        };
        let next_file_id = c.u64()?;
        let floor = c.u64()?;
        let mut counts = [0usize; 5];
        for n in &mut counts {
            *n = c.u32()? as usize;
        }
        for v1 in SIZES {
            let at = c.at();
            crate::part::record_size(c.u16()? as usize, v1, at)?;
        }
        c.reserved(2)?;
        let need = counts
            .iter()
            .zip(SIZES)
            .try_fold(HEAD as u64, |a, (n, s)| a.checked_add(*n as u64 * s as u64));
        if need != Some(blk.body.len() as u64) {
            return fail(c.at(), Refusal::ArrayLength);
        }
        let levels = crate::frame::levels(&mut c, counts[0])?;

        let mut parts: Vec<PartRef> = Vec::with_capacity(counts[1]);
        let mut seqs = BTreeSet::new();
        for _ in 0..counts[1] {
            let at = c.at();
            let file_id = c.u64()?;
            let seq = c.u64()?;
            let class = c.u8()?;
            c.reserved(3)?;
            let p = PartRef {
                file_id,
                seq,
                class: Class::from_u8(class).ok_or(FileError {
                    at: at + 16,
                    refusal: Refusal::UnknownClass,
                })?,
                file_crc32c: c.u32()?,
                period_start: c.i64()?,
                period_len: c.u64()?,
                least_start: c.i64()?,
                greatest_end: c.i64()?,
                least_series: c.u64()?,
                greatest_series: c.u64()?,
                byte_len: c.u64()?,
            };
            if parts.last().is_some_and(|q| q.file_id >= file_id) || !seqs.insert(seq) {
                return fail(at, Refusal::PartOrder);
            }
            if file_id >= next_file_id {
                return fail(at, Refusal::FileId);
            }
            let period_ok = i64::try_from(p.period_len)
                .ok()
                .filter(|l| *l > 0)
                .and_then(|l| p.period_start.checked_add(l));
            if period_ok.is_none() {
                return fail(at + 24, Refusal::Period);
            }
            parts.push(p);
        }

        let mut covered: Vec<Covered> = Vec::with_capacity(counts[2]);
        for _ in 0..counts[2] {
            let at = c.at();
            let level = c.u8()?;
            c.reserved(7)?;
            let r = Covered {
                level,
                from: c.i64()?,
                to: c.i64()?,
            };
            let Some(width) = width_of(&levels, level) else {
                return fail(at, Refusal::UnknownLevel);
            };
            if r.from.rem_euclid(width) != 0 || r.to.rem_euclid(width) != 0 {
                return fail(at + 8, Refusal::Alignment);
            }
            let after = covered
                .last()
                .is_none_or(|q| q.level < level || (q.level == level && q.to < r.from));
            if r.from >= r.to || !after {
                return fail(at + 8, Refusal::CoveredRange);
            }
            covered.push(r);
        }

        let mut floors: Vec<Floor> = Vec::with_capacity(counts[3]);
        for _ in 0..counts[3] {
            let at = c.at();
            let series = c.u64()?;
            let seq = c.u64()?;
            let flags = c.u8()?;
            c.reserved(7)?;
            if flags & !1 != 0 {
                return fail(at + 16, Refusal::FloorFlag);
            }
            if floors.last().is_some_and(|f| f.series >= series) {
                return fail(at, Refusal::FloorOrder);
            }
            floors.push(Floor {
                series,
                seq,
                rebuilding: flags & 1 != 0,
            });
        }

        let mut retired: Vec<Retired> = Vec::with_capacity(counts[4]);
        for _ in 0..counts[4] {
            let at = c.at();
            let r = Retired {
                file_id: c.u64()?,
                retired_at: c.i64()?,
            };
            let live = parts
                .binary_search_by_key(&r.file_id, |p| p.file_id)
                .is_ok();
            if live || retired.last().is_some_and(|q| q.file_id >= r.file_id) {
                return fail(at, Refusal::RetiredOrder);
            }
            if r.file_id >= next_file_id {
                return fail(at, Refusal::FileId);
            }
            retired.push(r);
        }
        debug_assert!(c.done());
        Ok(Root {
            header: h,
            seq,
            checkpoint,
            reset,
            next_file_id,
            floor,
            levels,
            parts,
            covered,
            floors,
            retired,
        })
    }
}
