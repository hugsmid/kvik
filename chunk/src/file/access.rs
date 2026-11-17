//! Block access shared by the compacted segment and chunk index object readers: a block bound to its parent's
//! checksum, the header and the tail, the index root, and the checks of an index entry and the commit-time
//! directory against the trailer (`docs/format-compacted.md` B.1 A reader's checks).

use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader, MAX_BODY_LEN};
use kvik_log::body::{END_MARKER_LEN, decode_end_marker};
use kvik_log::crc::crc32c;
use kvik_log::header::{FileHeader, HEADER_LEN};

use super::block_type;
use super::desc::Class;
use super::error::{Fault, FileElement, FileError};
use super::layout::Identity;
use super::parts::{IndexEntry, RootRecord, TimesRecord, Trailer, decode_index_block};
use super::source::ReadAt;
use super::stats::Bounds;
use super::write::Fact;
use crate::{Block, flag, value_type};

/// Reads the block at `offset`, which its pointer says is of type `t`: valid (A.2), wholly below `limit`, and, when
/// `crc` is given, with the `body_crc32c` its parent records. Returns the header and body.
pub(crate) fn read_bound<S: ReadAt + ?Sized>(
    src: &S,
    offset: u64,
    t: u8,
    crc: Option<u32>,
    limit: u64,
) -> Result<(BlockHeader, Vec<u8>), FileError> {
    let fault = |f| FileError::invalid(offset, f);
    let body_at = offset
        .checked_add(BLOCK_HEADER_LEN as u64)
        .filter(|&e| e <= limit && offset.is_multiple_of(8))
        .ok_or(fault(Fault::BlockHeader))?;
    let raw = src.read_at(offset, BLOCK_HEADER_LEN)?;
    let raw: [u8; BLOCK_HEADER_LEN] = raw.try_into().map_err(|_| fault(Fault::BlockHeader))?;
    let h = BlockHeader::decode(&raw).ok_or(fault(Fault::BlockHeader))?;
    if h.block_type != t {
        return Err(fault(Fault::BlockType {
            expected: t,
            found: h.block_type,
        }));
    }
    if h.body_len > MAX_BODY_LEN || body_at + u64::from(h.body_len) > limit {
        return Err(fault(Fault::BlockLength));
    }
    if crc.is_some_and(|c| c != h.body_crc32c) {
        return Err(fault(Fault::Binding));
    }
    let body = src.read_at(body_at, h.body_len as usize)?;
    if crc32c(&body) != h.body_crc32c {
        return Err(fault(Fault::BlockChecksum));
    }
    Ok((h, body))
}

/// Reads and checks the A.1 header: magic and checksum, `kind`, `format_major`, then the identity.
pub(crate) fn read_header<S: ReadAt + ?Sized>(
    src: &S,
    kind: u8,
    id: &Identity,
) -> Result<FileHeader, FileError> {
    if src.len() < HEADER_LEN as u64 {
        return Err(FileError::invalid(0, Fault::Header));
    }
    let h = FileHeader::decode(&src.read_at(0, HEADER_LEN)?)
        .ok_or(FileError::invalid(0, Fault::Header))?;
    if h.kind != kind {
        return Err(FileError::Refused("kind"));
    }
    if h.format_major > kvik_log::FORMAT_MAJOR {
        return Err(FileError::Unsupported {
            offset: 0,
            element: FileElement::FormatMajor(h.format_major),
        });
    }
    if h.store_id != id.store_id {
        return Err(FileError::Refused("store_id"));
    }
    if h.file_id != id.file_id {
        return Err(FileError::Refused("file_id"));
    }
    if h.generation != id.generation {
        return Err(FileError::Refused("generation"));
    }
    Ok(h)
}

/// Reads the end marker and the trailer block it points at, which must end where the end marker starts.
pub(crate) fn read_tail<S: ReadAt + ?Sized>(src: &S) -> Result<(u64, Vec<u8>), FileError> {
    let len = src.len();
    let end = len
        .checked_sub(END_MARKER_LEN as u64)
        .filter(|&e| e >= HEADER_LEN as u64)
        .ok_or(FileError::invalid(len, Fault::EndMarker))?;
    let at = decode_end_marker(&src.read_at(end, END_MARKER_LEN)?)
        .filter(|&o| o >= HEADER_LEN as u64 && o < end)
        .ok_or(FileError::invalid(end, Fault::EndMarker))?;
    let (h, body) = read_bound(src, at, block_type::TRAILER, None, end)?;
    if at + h.block_len() != end {
        return Err(FileError::invalid(end, Fault::EndMarker));
    }
    Ok((at, body))
}

/// The index of a file: the root's records and where index blocks may lie.
#[derive(Clone, Debug)]
pub(crate) struct Index {
    pub(crate) records: Vec<RootRecord>,
    pub(crate) base: u64,
    pub(crate) limit: u64,
}

impl Index {
    /// Checks the root against the file: entry counts summing to `chunk_count`, the first block at the base and
    /// every block below the root.
    pub(crate) fn new(
        records: Vec<RootRecord>,
        base: u64,
        limit: u64,
        chunk_count: u32,
        at: u64,
    ) -> Result<Self, FileError> {
        let sum: u64 = records.iter().map(|r| u64::from(r.entry_count)).sum();
        let placed = records.first().is_none_or(|r| r.offset == 0)
            && records
                .iter()
                .all(|r| base.checked_add(r.offset).is_some_and(|o| o < limit));
        if sum != u64::from(chunk_count) || !placed {
            return Err(FileError::invalid(at, Fault::Mismatch("index root")));
        }
        Ok(Self {
            records,
            base,
            limit,
        })
    }

    /// Index block `i`, bound to its root record.
    pub(crate) fn block<S: ReadAt + ?Sized>(
        &self,
        src: &S,
        i: usize,
    ) -> Result<Vec<IndexEntry>, FileError> {
        let r = self.records[i];
        let at = self.base + r.offset;
        let (_, body) = read_bound(src, at, block_type::INDEX, Some(r.body_crc32c), self.limit)?;
        let entries = decode_index_block(&body).map_err(|b| b.at(at))?;
        let (first, last) = (&entries[0], &entries[entries.len() - 1]);
        if entries.len() != r.entry_count as usize
            || first.series_id != r.first_series_id
            || last.series_id != r.last_series_id
            || first.min_t != r.first_min_t
        {
            return Err(FileError::invalid(at, Fault::Mismatch("index block")));
        }
        Ok(entries)
    }

    /// The blocks that may hold entries of `series_id`.
    pub(crate) fn blocks_of(&self, series_id: u32) -> impl Iterator<Item = usize> + '_ {
        self.records
            .iter()
            .enumerate()
            .filter(move |(_, r)| r.first_series_id <= series_id && series_id <= r.last_series_id)
            .map(|(i, _)| i)
    }
}

/// A valid block of type `t` at `at` inside an in-memory section that starts at file offset `base`: header, body
/// and the offset of the next block.
pub(crate) fn section_block(
    section: &[u8],
    base: u64,
    at: usize,
    t: u8,
) -> Result<(BlockHeader, &[u8], usize), FileError> {
    let here = base + at as u64;
    let fault = |f| FileError::invalid(here, f);
    let raw: &[u8; BLOCK_HEADER_LEN] = section
        .get(at..at + BLOCK_HEADER_LEN)
        .and_then(|r| r.try_into().ok())
        .ok_or(fault(Fault::BlockHeader))?;
    let h = BlockHeader::decode(raw).ok_or(fault(Fault::BlockHeader))?;
    if h.block_type != t {
        return Err(fault(Fault::BlockType {
            expected: t,
            found: h.block_type,
        }));
    }
    let start = at + BLOCK_HEADER_LEN;
    let body = section
        .get(start..start + h.body_len as usize)
        .filter(|_| h.body_len <= MAX_BODY_LEN)
        .ok_or(fault(Fault::BlockLength))?;
    if crc32c(body) != h.body_crc32c {
        return Err(fault(Fault::BlockChecksum));
    }
    let next = at + h.block_len() as usize;
    if next > section.len() {
        return Err(fault(Fault::BlockLength));
    }
    Ok((h, body, next))
}

/// Checks an index entry against its segment: its chunk inside the chunk section, its ranges inside the trailer's,
/// set-aside chunks in revision files only.
pub(crate) fn check_entry(
    e: &IndexEntry,
    t: &Trailer,
    description_end: u64,
    class: Class,
    at: u64,
) -> Result<(), FileError> {
    let chunks_end = if t.entries_offset != 0 {
        t.entries_offset
    } else {
        t.times_offset
    };
    let placed = e.chunk_offset >= description_end
        && e.chunk_offset.is_multiple_of(8)
        && e.dir_len.is_multiple_of(8)
        && e.chunk_offset
            .checked_add(u64::from(e.chunk_len))
            .is_some_and(|end| end <= chunks_end);
    let inside = t.first <= e.first
        && e.last <= t.last
        && t.min_t <= e.min_t
        && e.reach <= t.max_reach
        && u64::from(e.row_count) <= t.fact_count;
    if !placed || !inside || (e.set_aside() && class != Class::Revision) {
        return Err(FileError::invalid(at, Fault::Mismatch("chunk index entry")));
    }
    Ok(())
}

/// Checks the commit-time directory against the trailer: as many blocks, the first at `times_offset`, every one
/// below the directory, commit times and positions spanning the trailer's.
pub(crate) fn check_times(times: &[TimesRecord], t: &Trailer) -> Result<(), &'static str> {
    let first = &times[0];
    let last = &times[times.len() - 1];
    let ok = times.len() == t.times_count as usize
        && first.offset == t.times_offset
        && times
            .iter()
            .all(|r| r.offset.is_multiple_of(8) && r.offset < t.times_dir_offset)
        && times.iter().map(|r| r.first_commit_time).min() == Some(t.first_commit_time)
        && times.iter().map(|r| r.last_commit_time).max() == Some(t.last_commit_time)
        && (first.generation, first.first_sequence) <= t.first
        && (last.generation, last.last_sequence) >= t.last;
    if ok {
        Ok(())
    } else {
        Err("commit-time directory")
    }
}

/// The facts of a decoded data block of `series_id`.
pub(crate) fn facts_of(series_id: u32, b: &Block) -> Vec<Fact> {
    let mut at = 0usize;
    (0..b.len())
        .map(|i| {
            let has_text = b.value_type[i] == value_type::TEXT && b.flags[i] & flag::NO_VALUE == 0;
            let text = if has_text {
                let n = b.value[i] as usize;
                let t = b.heap[at..at + n].to_vec();
                at += n;
                t
            } else {
                Vec::new()
            };
            Fact {
                series_id,
                position: (b.generation[i], b.sequence[i]),
                t: b.t[i],
                quality: b.quality[i],
                flags: b.flags[i],
                value_type: b.value_type[i],
                value: b.value[i],
                text,
                changed_at: b.changed_at[i],
                scale: b.scale[i],
            }
        })
        .collect()
}

impl IndexEntry {
    /// The chunk's value bounds, if it has them.
    pub fn bounds(&self) -> Option<Bounds> {
        self.raw_bounds().and_then(Bounds::from_raw)
    }
}
