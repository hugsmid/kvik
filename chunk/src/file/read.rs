//! The positional reader of a compacted segment (`docs/format-compacted.md` B.1 A reader's checks): the header,
//! end marker and trailer at open, then each block a caller follows a pointer to, bound to the checksum its parent
//! records.

use std::collections::HashMap;

use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader, MAX_BODY_LEN};
use kvik_log::body::{END_MARKER_LEN, decode_end_marker};
use kvik_log::crc::crc32c;
use kvik_log::entries::{EntriesBlock, Entry};
use kvik_log::header::{FileHeader, HEADER_LEN};

use super::desc::{Class, Description, Input, Level};
use super::error::{Fault, FileElement, FileError};
use super::parts::{
    Directory, IndexEntry, MARKER_LEN, RootRecord, TimesRecord, Trailer, decode_index_block,
    decode_root, decode_times_directory, u32_at,
};
use super::source::ReadAt;
use super::stats::Bounds;
use super::times::{Batch, decode_times};
use super::write::Fact;
use super::{KIND_COMPACTED, Position, block_type};
use crate::{Block, DecodeError, flag, value_type};

/// What a file's name and the store say it is: the store id, and the generation and `file_id` of its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    /// The store's id (`format.md` A.12).
    pub store_id: [u8; 16],
    /// The generation the file was created in.
    pub generation: u32,
    /// Its `file_id`.
    pub file_id: u64,
}

/// What the manifest's publish record says about a compacted file beyond its file description (provisional:
/// `model/OPEN-QUESTIONS.md` Q-810). Everything here is in the file's description and trailer, so a rebuild
/// restates it from the file or its chunk index object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Layout {
    /// The generation it was created in.
    pub generation: u32,
    /// Its `file_id`.
    pub file_id: u64,
    /// Its class.
    pub class: Class,
    /// Its level.
    pub level: Level,
    /// Its period's start.
    pub period_start: i64,
    /// Its period's length.
    pub period_len: i64,
    /// The run id.
    pub run_id: u64,
    /// The number of outputs published together.
    pub run_count: u32,
    /// Its inputs.
    pub inputs: Vec<Input>,
    /// The least and greatest position it holds.
    pub range: (Position, Position),
    /// The least `t` of a fact; 0 with none.
    pub min_t: i64,
    /// The greatest reach of a fact; 0 with none.
    pub max_reach: i64,
    /// The least commit time of its batch rows.
    pub first_commit_time: i64,
    /// The greatest commit time of its batch rows.
    pub last_commit_time: i64,
    /// Facts held.
    pub fact_count: u64,
    /// Positions held.
    pub entry_count: u64,
    /// It holds entries.
    pub has_entries: bool,
    /// It holds set-aside chunks.
    pub has_set_aside: bool,
    /// A split part's first and last series name.
    pub name_range: Option<(String, String)>,
}

impl Layout {
    pub(crate) fn of(id: &Identity, d: &Description, t: &Trailer) -> Self {
        Self {
            generation: id.generation,
            file_id: id.file_id,
            class: d.class,
            level: d.level,
            period_start: d.period_start,
            period_len: d.period_len,
            run_id: d.run_id,
            run_count: d.run_count,
            inputs: d.inputs.clone(),
            range: (t.first, t.last),
            min_t: t.min_t,
            max_reach: t.max_reach,
            first_commit_time: t.first_commit_time,
            last_commit_time: t.last_commit_time,
            fact_count: t.fact_count,
            entry_count: t.entry_count,
            has_entries: d.has_entries,
            has_set_aside: d.has_set_aside,
            name_range: d.name_range.clone(),
        }
    }

    /// The first field in which `self` and `other` differ.
    pub(crate) fn difference(&self, o: &Self) -> Option<&'static str> {
        [
            ("generation", self.generation == o.generation),
            ("file_id", self.file_id == o.file_id),
            ("class", self.class == o.class),
            ("level", self.level == o.level),
            (
                "period",
                (self.period_start, self.period_len) == (o.period_start, o.period_len),
            ),
            (
                "run",
                (self.run_id, self.run_count) == (o.run_id, o.run_count),
            ),
            ("inputs", self.inputs == o.inputs),
            ("position range", self.range == o.range),
            ("min_t", self.min_t == o.min_t),
            ("max_reach", self.max_reach == o.max_reach),
            (
                "commit times",
                (self.first_commit_time, self.last_commit_time)
                    == (o.first_commit_time, o.last_commit_time),
            ),
            ("fact_count", self.fact_count == o.fact_count),
            ("entry_count", self.entry_count == o.entry_count),
            (
                "flags",
                (self.has_entries, self.has_set_aside) == (o.has_entries, o.has_set_aside),
            ),
            ("name range", self.name_range == o.name_range),
        ]
        .into_iter()
        .find(|(_, same)| !same)
        .map(|(f, _)| f)
    }
}

/// A fact as read, with its batch's receive and commit times.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimedFact {
    /// The fact.
    pub fact: Fact,
    /// Its batch's `received_at`.
    pub received_at: i64,
    /// Its batch's commit time.
    pub commit_time: i64,
}

/// A non-fact entry as read: its position, its batch's times and the entry. An [`Entry::Ignored`] stands for that
/// many positions from `position` on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryRow {
    /// Its position.
    pub position: Position,
    /// Its batch's `received_at`.
    pub received_at: i64,
    /// Its batch's commit time.
    pub commit_time: i64,
    /// The entry.
    pub entry: Entry,
}

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
        .filter(|&e| e <= limit && offset % 8 == 0)
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
        let end = e.chunk_offset + u64::from(e.chunk_len);
        let at = end - u64::from(e.dir_len);
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
        let r = d.blocks[i];
        let at = e.chunk_offset + u64::from(r.offset);
        let limit = e.chunk_offset + u64::from(e.chunk_len - e.dir_len);
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
        if !cache.contains_key(&i) {
            let rows = self.times_block(i)?;
            cache.insert(i, rows);
        }
        let rows = &cache[&i];
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
                    let end = s
                        .checked_add(n - 1)
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

/// A valid block of type `t` at `at` inside an in-memory section that starts at file offset `base`: header, body
/// and the offset of the next block.
fn section_block(
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
        && e.chunk_offset % 8 == 0
        && e.dir_len % 8 == 0
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
fn check_times(times: &[TimesRecord], t: &Trailer) -> Result<(), &'static str> {
    let first = &times[0];
    let last = &times[times.len() - 1];
    let ok = times.len() == t.times_count as usize
        && first.offset == t.times_offset
        && times
            .iter()
            .all(|r| r.offset % 8 == 0 && r.offset < t.times_dir_offset)
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
