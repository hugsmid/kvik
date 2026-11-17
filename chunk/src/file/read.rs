//! The positional reader of a compacted segment (`docs/format-compacted.md` B.1 A reader's checks): the header,
//! end marker and trailer at open, then each block a caller follows a pointer to, bound to the checksum its parent
//! records.

use std::collections::HashMap;

use kvik_log::block::BLOCK_HEADER_LEN;
use kvik_log::crc::crc32c;
use kvik_log::entries::EntriesBlock;
use kvik_log::header::{FileHeader, HEADER_LEN};

use super::access::{
    Index, check_entry, check_times, facts_of, read_bound, read_header, read_tail, section_block,
};
use super::desc::{Description, Input};
use super::error::{Fault, FileElement, FileError};
use super::layout::{EntryRow, Identity, Layout, TimedFact};
use super::parts::{
    Directory, IndexEntry, MARKER_LEN, RootRecord, TimesRecord, Trailer, decode_root,
    decode_times_directory, u32_at,
};
use super::source::ReadAt;
use super::times::{Batch, decode_times};
use super::{KIND_COMPACTED, Position, block_type};
use crate::{Block, DecodeError};

/// An open compacted segment.
pub struct CompactedFile<S: ReadAt> {
    src: S,
    header: FileHeader,
    description: Description,
    trailer: Trailer,
    trailer_offset: u64,
    description_end: u64,
    times: Vec<TimesRecord>,
    index: Index,
}

impl<S: ReadAt> std::fmt::Debug for CompactedFile<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactedFile")
            .field("header", &self.header)
            .field("description", &self.description)
            .field("trailer", &self.trailer)
            .finish_non_exhaustive()
    }
}

/// Commit-times blocks read so far by one call.
type TimesCache = HashMap<usize, Vec<Batch>>;

impl<S: ReadAt> CompactedFile<S> {
    /// Opens a compacted segment: the header against `id`, the end marker, the trailer, and the blocks the trailer
    /// binds (description, commit-time directory, index root); with `layout`, the trailer and description must
    /// equal it (the manifest's record of the file).
    pub fn open(src: S, id: &Identity, layout: Option<&Layout>) -> Result<Self, FileError> {
        let header = read_header(&src, KIND_COMPACTED, id)?;
        let (trailer_offset, body) = read_tail(&src)?;
        let trailer = Trailer::decode(&body).map_err(|b| b.at(trailer_offset))?;
        let t = &trailer;
        let mismatch = |what| FileError::invalid(trailer_offset, Fault::Mismatch(what));
        if t.index_root_offset >= trailer_offset {
            return Err(mismatch("trailer offsets"));
        }
        let first_section = if t.entries_offset != 0 {
            t.entries_offset
        } else {
            t.times_offset
        };
        let (dh, body) = read_bound(
            &src,
            HEADER_LEN as u64,
            block_type::DESCRIPTION,
            Some(t.description_crc32c),
            first_section,
        )?;
        let description = Description::decode(&body).map_err(|b| b.at(HEADER_LEN as u64))?;
        let description_end = HEADER_LEN as u64 + dh.block_len();
        if description.has_entries != (t.entries_offset != 0) {
            return Err(mismatch("description flags"));
        }
        if description.inputs.contains(&Input {
            kind: KIND_COMPACTED,
            file_id: id.file_id,
        }) {
            return Err(FileError::invalid(
                HEADER_LEN as u64,
                Fault::Body("description inputs"),
            ));
        }
        let (_, body) = read_bound(
            &src,
            t.times_dir_offset,
            block_type::TIMES_DIRECTORY,
            Some(t.times_dir_crc32c),
            trailer_offset,
        )?;
        let times = decode_times_directory(&body).map_err(|b| b.at(t.times_dir_offset))?;
        check_times(&times, t)
            .map_err(|w| FileError::invalid(t.times_dir_offset, Fault::Mismatch(w)))?;
        let (_, body) = read_bound(
            &src,
            t.index_root_offset,
            block_type::ROOT,
            Some(t.root_crc32c),
            trailer_offset,
        )?;
        let records = decode_root(&body).map_err(|b| b.at(t.index_root_offset))?;
        let index = Index::new(
            records,
            t.index_offset,
            t.index_root_offset,
            t.chunk_count,
            t.index_root_offset,
        )?;
        let file = Self {
            src,
            header,
            description,
            trailer,
            trailer_offset,
            description_end,
            times,
            index,
        };
        if let Some(expected) = layout
            && let Some(field) = file.layout().difference(expected)
        {
            return Err(FileError::Layout(field));
        }
        Ok(file)
    }

    /// The file header.
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// The description.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The trailer.
    pub fn trailer(&self) -> &Trailer {
        &self.trailer
    }

    /// The commit-time directory.
    pub fn times_directory(&self) -> &[TimesRecord] {
        &self.times
    }

    /// The index root's records.
    pub fn root(&self) -> &[RootRecord] {
        &self.index.records
    }

    /// The source.
    pub fn source(&self) -> &S {
        &self.src
    }

    pub(crate) fn trailer_offset(&self) -> u64 {
        self.trailer_offset
    }

    /// The layout the manifest records for this file.
    pub fn layout(&self) -> Layout {
        let id = Identity {
            store_id: self.header.store_id,
            generation: self.header.generation,
            file_id: self.header.file_id,
        };
        Layout::of(&id, &self.description, &self.trailer)
    }

    fn index_block(&self, i: usize) -> Result<Vec<IndexEntry>, FileError> {
        let entries = self.index.block(&self.src, i)?;
        let at = self.index.base + self.index.records[i].offset;
        for e in &entries {
            check_entry(
                e,
                &self.trailer,
                self.description_end,
                self.description.class,
                at,
            )?;
        }
        Ok(entries)
    }

    /// The index entries of one series, in index order: one read of each index block the root names for it.
    pub fn series(&self, series_id: u32) -> Result<Vec<IndexEntry>, FileError> {
        let mut out = Vec::new();
        for i in self.index.blocks_of(series_id) {
            out.extend(
                self.index_block(i)?
                    .into_iter()
                    .filter(|e| e.series_id == series_id),
            );
        }
        Ok(out)
    }

    /// Every index entry, in index order.
    pub fn index(&self) -> Result<Vec<IndexEntry>, FileError> {
        let mut out = Vec::with_capacity(self.trailer.chunk_count as usize);
        for i in 0..self.index.records.len() {
            out.extend(self.index_block(i)?);
        }
        Ok(out)
    }

    /// A chunk's directory, bound to its index entry: the same series, row count and flags, its block records
    /// laid end to end from the chunk's first byte to the directory.
    pub fn directory(&self, e: &IndexEntry) -> Result<Directory, FileError> {
        let (at, end) = chunk_span(e)?;
        let (h, body) = read_bound(
            &self.src,
            at,
            block_type::DIRECTORY,
            Some(e.dir_crc32c),
            end,
        )?;
        let mismatch = |w| FileError::invalid(at, Fault::Mismatch(w));
        if h.block_len() != u64::from(e.dir_len) {
            return Err(FileError::invalid(at, Fault::BlockLength));
        }
        let d = Directory::decode(&body).map_err(|b| b.at(at))?;
        if d.series_id != e.series_id || d.row_count != e.row_count {
            return Err(mismatch("chunk directory"));
        }
        if d.flags != e.chunk_flags() {
            return Err(mismatch("chunk flags"));
        }
        let mut next = 0u64;
        for r in &d.blocks {
            if u64::from(r.offset) != next {
                return Err(mismatch("block records"));
            }
            next = next + BLOCK_HEADER_LEN as u64 + u64::from(r.body_len).div_ceil(8) * 8;
        }
        if next != u64::from(e.chunk_len - e.dir_len) {
            return Err(mismatch("block records"));
        }
        Ok(d)
    }

    /// Data block `i` of a chunk, bound to its directory record: its rows, first and last `t` the record's.
    pub fn data_block(&self, e: &IndexEntry, d: &Directory, i: usize) -> Result<Block, FileError> {
        self.data_block_raw(e, d, i).map(|(b, _)| b)
    }

    pub(crate) fn data_block_raw(
        &self,
        e: &IndexEntry,
        d: &Directory,
        i: usize,
    ) -> Result<(Block, Vec<u8>), FileError> {
        let (limit, _) = chunk_span(e)?;
        let r = *d
            .blocks
            .get(i)
            .ok_or(FileError::invalid(limit, Fault::Mismatch("block records")))?;
        let at = e
            .chunk_offset
            .checked_add(u64::from(r.offset))
            .ok_or(FileError::invalid(limit, Fault::Mismatch("block records")))?;
        let (h, body) = read_bound(&self.src, at, block_type::DATA, Some(r.body_crc32c), limit)?;
        if h.body_len != r.body_len {
            return Err(FileError::invalid(at, Fault::BlockLength));
        }
        let b = Block::decode(&body).map_err(|err| match err {
            DecodeError::Unsupported(el) => FileError::Unsupported {
                offset: at,
                element: FileElement::Data(el),
            },
            other => FileError::invalid(at, Fault::Data(other)),
        })?;
        if b.len() != usize::from(r.row_count)
            || b.t.first() != Some(&r.first_t)
            || b.t.last() != Some(&r.last_t)
        {
            return Err(FileError::invalid(at, Fault::Mismatch("data block record")));
        }
        Ok((b, body))
    }

    /// Every fact of a chunk, with its batch's receive and commit times.
    pub fn read_chunk(&self, e: &IndexEntry) -> Result<Vec<TimedFact>, FileError> {
        let d = self.directory(e)?;
        let mut cache = TimesCache::new();
        let mut out = Vec::with_capacity(e.row_count as usize);
        for i in 0..d.blocks.len() {
            let b = self.data_block(e, &d, i)?;
            for fact in facts_of(e.series_id, &b) {
                let batch = self.batch_cached(&mut cache, fact.position)?;
                out.push(TimedFact {
                    fact,
                    received_at: batch.received_at,
                    commit_time: batch.commit_time,
                });
            }
        }
        Ok(out)
    }

    /// Commit-times block `i`, bound to its directory record: the generation, the first and last row's sequences
    /// and commit times the record's.
    pub fn times_block(&self, i: usize) -> Result<Vec<Batch>, FileError> {
        let r = self.times[i];
        let (_, body) = read_bound(
            &self.src,
            r.offset,
            block_type::TIMES,
            Some(r.body_crc32c),
            self.trailer.times_dir_offset,
        )?;
        let (g, rows) = decode_times(&body).map_err(|b| b.at(r.offset))?;
        let (a, z) = (rows[0], rows[rows.len() - 1]);
        if g != r.generation
            || a.first_sequence != r.first_sequence
            || z.last_sequence != r.last_sequence
            || a.commit_time != r.first_commit_time
            || z.commit_time != r.last_commit_time
        {
            return Err(FileError::invalid(
                r.offset,
                Fault::Mismatch("commit-times record"),
            ));
        }
        Ok(rows)
    }

    fn batch_cached(&self, cache: &mut TimesCache, p: Position) -> Result<Batch, FileError> {
        let none = || FileError::invalid(self.trailer.times_offset, Fault::NoBatch(p.0, p.1));
        let i = self
            .times
            .partition_point(|r| (r.generation, r.last_sequence) < p);
        let r = self.times.get(i).ok_or_else(none)?;
        if r.generation != p.0 || r.first_sequence > p.1 {
            return Err(none());
        }
        let rows = match cache.entry(i) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => v.insert(self.times_block(i)?),
        };
        let j = rows.partition_point(|b| b.last_sequence < p.1);
        rows.get(j)
            .filter(|b| b.holds(p.0, p.1))
            .copied()
            .ok_or_else(none)
    }

    /// The batch row that holds `p`: the directory picks one block, which is read.
    pub fn batch(&self, p: Position) -> Result<Batch, FileError> {
        self.batch_cached(&mut TimesCache::new(), p)
    }

    /// Every batch row, in file order.
    pub fn batches(&self) -> Result<Vec<Batch>, FileError> {
        let mut out = Vec::new();
        for i in 0..self.times.len() {
            out.extend(self.times_block(i)?);
        }
        Ok(out)
    }

    /// This file's answer to as-of time `t`: the greatest position of a batch row whose commit time is at or
    /// below `t`, reading at most one commit-times block a generation.
    pub fn as_of(&self, t: i64) -> Result<Option<Position>, FileError> {
        let mut best = None;
        let mut i = 0;
        while i < self.times.len() {
            let g = self.times[i].generation;
            let end = i + self.times[i..]
                .iter()
                .take_while(|r| r.generation == g)
                .count();
            let k = self.times[i..end].partition_point(|r| r.first_commit_time <= t);
            if k > 0 {
                let r = self.times[i + k - 1];
                let last = if r.last_commit_time <= t {
                    r.last_sequence
                } else {
                    let rows = self.times_block(i + k - 1)?;
                    let n = rows.partition_point(|b| b.commit_time <= t);
                    rows[n - 1].last_sequence
                };
                best = best.max(Some((g, last)));
            }
            i = end;
        }
        Ok(best)
    }

    /// The non-fact entries, read whole and bound to the trailer's checksum of the section: per generation a
    /// marker and its entries blocks; positions rising strictly inside the trailer's range, each inside a batch
    /// row whose `received_at` its block carries; as many as the trailer counts.
    pub fn entries(&self) -> Result<Vec<EntryRow>, FileError> {
        let t = &self.trailer;
        if t.entries_offset == 0 {
            return Ok(Vec::new());
        }
        let base = t.entries_offset;
        let section = self.src.read_at(base, (t.times_offset - base) as usize)?;
        if crc32c(&section) != t.entries_crc32c {
            return Err(FileError::invalid(base, Fault::Binding));
        }
        let mut cache = TimesCache::new();
        let mut out: Vec<EntryRow> = Vec::new();
        let mut at = 0usize;
        let mut last_gen: Option<u32> = None;
        let mut count = 0u64;
        let mut prev: Option<Position> = None;
        while at < section.len() {
            let (_, marker, next) = section_block(&section, base, at, block_type::MARKER)?;
            let here = base + at as u64;
            if marker.len() < MARKER_LEN {
                return Err(FileError::invalid(here, Fault::Body("generation marker")));
            }
            let g = u32_at(marker, 0);
            let blocks = u32_at(marker, 4);
            if blocks == 0 || last_gen.is_some_and(|l| l >= g) {
                return Err(FileError::invalid(here, Fault::Body("generation marker")));
            }
            last_gen = Some(g);
            at = next;
            for _ in 0..blocks {
                let (_, body, next) = section_block(&section, base, at, block_type::ENTRIES)?;
                let here = base + at as u64;
                let b = EntriesBlock::decode(body).map_err(|e| match e {
                    kvik_log::BodyError::Unsupported(el) => FileError::Unsupported {
                        offset: here,
                        element: FileElement::Entries(el),
                    },
                    kvik_log::BodyError::Invalid(c) => FileError::invalid(here, Fault::Entries(c)),
                    kvik_log::BodyError::NonCanonical => {
                        FileError::invalid(here, Fault::Body("entries block"))
                    }
                })?;
                let mut s = b.first_sequence;
                for entry in b.entries {
                    let p = (g, s);
                    let n = entry.sequences();
                    if prev.is_some_and(|q| q >= p) || p < t.first || p > t.last {
                        return Err(FileError::invalid(here, Fault::Mismatch("entry position")));
                    }
                    let batch = self.batch_cached(&mut cache, p)?;
                    if batch.received_at != b.received_at {
                        return Err(FileError::invalid(
                            here,
                            Fault::Mismatch("entry receive time"),
                        ));
                    }
                    let end = n
                        .checked_sub(1)
                        .and_then(|k| s.checked_add(k))
                        .ok_or(FileError::invalid(here, Fault::Mismatch("entry position")))?;
                    prev = Some((g, end));
                    count += n;
                    out.push(EntryRow {
                        position: p,
                        received_at: batch.received_at,
                        commit_time: batch.commit_time,
                        entry,
                    });
                    s = end.wrapping_add(1);
                }
                at = next;
            }
        }
        if prev.is_some_and(|p| p > t.last) || count != t.entry_count - t.fact_count {
            return Err(FileError::invalid(base, Fault::Mismatch("entry count")));
        }
        Ok(out)
    }
}

/// The offsets of a chunk's directory block and of the chunk's end.
fn chunk_span(e: &IndexEntry) -> Result<(u64, u64), FileError> {
    e.chunk_offset
        .checked_add(u64::from(e.chunk_len))
        .filter(|_| e.dir_len < e.chunk_len)
        .map(|end| (end - u64::from(e.dir_len), end))
        .ok_or(FileError::invalid(
            e.chunk_offset,
            Fault::Mismatch("chunk index entry"),
        ))
}
