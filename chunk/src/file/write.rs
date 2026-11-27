//! The compacted segment writer (`docs/format-compacted.md` B.1): one series at a time, then the entries, then the
//! commit-time table, the chunk index and the trailer. For given rows, registry state, class, level, period and
//! inputs it writes one byte sequence (B.1 Canonical writing).

use std::io::{self, Write};

use kvik_log::block::{BlockHeader, MAX_BODY_LEN, encode_block};
use kvik_log::body::encode_end_marker;
use kvik_log::crc::{Crc32c, crc32c};
use kvik_log::entries::{EntriesBlock, Entry};
use kvik_log::header::FileHeader;

use super::desc::{Class, Description, push_field};
use super::layout::{Identity, Layout};
use super::parts::{
    BlockRecord, Directory, IndexEntry, MARKER_LEN, RootRecord, SET_ASIDE, TimesRecord, Trailer,
    encode_index_block, encode_marker, encode_root, encode_times_directory, sort_key,
};
use super::stats::{BoundsFold, Reach};
use super::times::{Batch, encode_times};
pub use super::tweaks::Tweaks;
use super::{
    BLOCK_ROWS, CHUNK_ROWS, INDEX_BLOCK_ENTRIES, KIND_COMPACTED, Position, TIMES_ROWS, block_type,
};
use crate::block::directory;
use crate::bytes::put_leb128;
use crate::{Block, flag, value_type};

/// One fact of a series, as the log holds it; `received_at` and the commit time come from its batch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fact {
    /// The series.
    pub series_id: u32,
    /// Its position.
    pub position: Position,
    /// Event time.
    pub t: i64,
    /// Adapter-supplied quality, 0–3.
    pub quality: u8,
    /// Fact flags (`format.md` A.5).
    pub flags: u16,
    /// Value type, 1–5.
    pub value_type: u8,
    /// Value word; for text, the text's length; 0 without a value.
    pub value: u64,
    /// The text's bytes, for a text row with a value.
    pub text: Vec<u8>,
    /// `changed_at`, with flag bit 6; else 0.
    pub changed_at: u64,
    /// Stored decimal scale, for value type 2; else 0.
    pub scale: u8,
}

impl Fact {
    /// A good fact with a value word and no other flag.
    pub fn value(series_id: u32, position: Position, t: i64, value_type: u8, value: u64) -> Self {
        Self {
            series_id,
            position,
            t,
            quality: 0,
            flags: 0,
            value_type,
            value,
            text: Vec::new(),
            changed_at: 0,
            scale: 0,
        }
    }
}

/// The facts of one series in one file, with what the registry says at the file's greatest position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Series {
    /// The series.
    pub series_id: u32,
    /// Its primary name in force at the file's greatest position; `None` for a series with no definition.
    pub name: Option<String>,
    /// The reach of its readings.
    pub reach: Reach,
    /// Its facts, in any order.
    pub facts: Vec<Fact>,
    /// Its facts set aside by a restore (`spec.md` §8 Restore), revision files only.
    pub set_aside: Vec<Fact>,
}

/// Why a file could not be written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The output failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A block's rows break a rule of `docs/format-chunk.md`.
    #[error("data block: {0}")]
    Block(#[from] crate::EncodeError),
    /// An entry breaks a rule of `format.md` A.6, or a block would pass 16 MiB.
    #[error("entries: {0}")]
    Log(#[from] kvik_log::EncodeError),
    /// The content breaks a rule of `docs/format-compacted.md` B.1.
    #[error("content: {0}")]
    Content(&'static str),
}

/// What a finished file holds: its length, trailer and layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// Bytes written.
    pub len: u64,
    /// Its trailer.
    pub trailer: Trailer,
    /// Its layout, as a manifest records it.
    pub layout: Layout,
}

/// Writes one compacted segment.
pub struct FileWriter<W: Write> {
    out: W,
    offset: u64,
    crc: Crc32c,
    id: Identity,
    pub(super) description: Description,
    description_crc: u32,
    batches: Vec<Batch>,
    held: Vec<u64>,
    index: Vec<IndexEntry>,
    last_key: Option<(bool, Vec<u8>, u32)>,
    fact_count: u64,
    entry_positions: u64,
    range: Option<(Position, Position)>,
    min_t: i64,
    max_reach: i64,
    set_aside: bool,
    entries_written: bool,
    entries_offset: u64,
    entries_crc: u32,
    tweaks: Tweaks,
    data_blocks: usize,
    /// Alternative bodies by data block number (the canonical check's [`Tweaks::bodies`]).
    alt_bodies: std::collections::BTreeMap<usize, Vec<u8>>,
    pub(super) block_rows: usize,
    pub(super) chunk_rows: usize,
    pub(super) stream: Option<super::stream::SeriesStream>,
}

/// A chunk being written: where it starts, its block records so far and its index entry's folds.
pub(super) struct ChunkAcc {
    series_id: u32,
    flags: u16,
    start: u64,
    records: Vec<BlockRecord>,
    rows: u32,
    fold: BoundsFold,
    min_t: i64,
    max_t: i64,
    reach: i64,
    first: Option<Position>,
    last: Position,
}

fn content(what: &'static str) -> WriteError {
    WriteError::Content(what)
}

/// Inserts a plain column into a C.2-shaped body that starts `prefix` bytes in.
fn insert_column(body: &[u8], prefix: usize, id: u8, payload: &[u8]) -> Vec<u8> {
    let c2 = &body[prefix..];
    let mut cols: Vec<(u8, u8, &[u8])> = directory(c2).unwrap_or_default();
    cols.push((id, crate::encoding::PLAIN, payload));
    cols.sort_by_key(|c| c.0);
    let mut out = body[..prefix + 2].to_vec();
    out.push(cols.len() as u8);
    for (id, enc, p) in &cols {
        out.push(*id);
        out.push(*enc);
        put_leb128(&mut out, p.len() as u64);
    }
    for (_, _, p) in &cols {
        out.extend_from_slice(p);
    }
    out
}

fn columns(facts: &[Fact]) -> Block {
    let mut b = Block::default();
    for f in facts {
        b.t.push(f.t);
        b.generation.push(f.position.0);
        b.sequence.push(f.position.1);
        b.quality.push(f.quality);
        b.flags.push(f.flags);
        b.value_type.push(f.value_type);
        b.value.push(f.value);
        if f.value_type == value_type::TEXT && f.flags & flag::NO_VALUE == 0 {
            b.heap.extend_from_slice(&f.text);
        }
        b.changed_at.push(f.changed_at);
        b.scale.push(f.scale);
    }
    b
}

impl<W: Write> FileWriter<W> {
    /// Starts a file: checks the description and the batches (ascending, not overlapping, a commit time at or
    /// above its receive time and not decreasing within a generation), then writes the header and description.
    pub fn new(
        out: W,
        id: Identity,
        created_at: i64,
        description: &Description,
        batches: Vec<Batch>,
    ) -> Result<Self, WriteError> {
        description.check().map_err(content)?;
        if description.inputs.contains(&super::desc::Input {
            kind: KIND_COMPACTED,
            file_id: id.file_id,
        }) {
            return Err(content("a file named as its own input"));
        }
        if batches
            .iter()
            .any(|b| b.first_sequence > b.last_sequence || b.commit_time < b.received_at)
            || !batches.windows(2).all(|w| {
                w[0].generation < w[1].generation
                    || (w[0].generation == w[1].generation
                        && w[0].last_sequence < w[1].first_sequence
                        && w[0].commit_time <= w[1].commit_time)
            })
        {
            return Err(content(
                "batches out of order, overlapping or with a negative lag",
            ));
        }
        let held = vec![0; batches.len()];
        let mut w = Self {
            out,
            offset: 0,
            crc: Crc32c::new(),
            id,
            description: description.clone(),
            description_crc: 0,
            batches,
            held,
            index: Vec::new(),
            last_key: None,
            fact_count: 0,
            entry_positions: 0,
            range: None,
            min_t: i64::MAX,
            max_reach: i64::MIN,
            set_aside: false,
            entries_written: false,
            entries_offset: 0,
            entries_crc: 0,
            tweaks: Tweaks::default(),
            data_blocks: 0,
            alt_bodies: Default::default(),
            block_rows: BLOCK_ROWS,
            chunk_rows: CHUNK_ROWS,
            stream: None,
        };
        let header = FileHeader::new(
            KIND_COMPACTED,
            id.generation,
            id.file_id,
            created_at,
            id.store_id,
        );
        w.put(&header.encode())?;
        Ok(w)
    }

    /// Departures from canonical writing; call before the first series.
    #[doc(hidden)]
    pub fn with_tweaks(mut self, mut tweaks: Tweaks) -> Self {
        self.alt_bodies = std::mem::take(&mut tweaks.bodies)
            .into_iter()
            .enumerate()
            .collect();
        self.tweaks = tweaks;
        self
    }

    /// Offers the file's own body for the data block numbered `n` in write order (the canonical check, C.10).
    pub(super) fn offer_body(&mut self, n: usize, body: Vec<u8>) {
        self.alt_bodies.insert(n, body);
    }

    /// Smaller blocks and chunks, so tests reach chunk boundaries with few rows.
    #[cfg(test)]
    pub(crate) fn with_rows(mut self, block_rows: usize, chunk_rows: usize) -> Self {
        self.block_rows = block_rows;
        self.chunk_rows = chunk_rows;
        self
    }

    /// The sink.
    pub fn sink(&self) -> &W {
        &self.out
    }

    /// The sink, for a caller that drains what is written as it goes.
    pub fn sink_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Bytes written so far.
    pub fn offset(&mut self) -> u64 {
        self.offset
    }

    /// Data blocks written so far.
    pub fn data_blocks(&self) -> usize {
        self.data_blocks
    }

    /// Reserves room for `n` more index entries, so that the index is allocated once.
    pub fn reserve_index(&mut self, n: usize) {
        self.index.reserve_exact(n);
    }

    /// Bytes the writer holds besides its sink: index entries, commit-time rows and the open series' rows.
    pub fn held_bytes(&self) -> u64 {
        let fact = std::mem::size_of::<Fact>() as u64;
        (self.index.capacity() * std::mem::size_of::<IndexEntry>()) as u64
            + (self.batches.capacity() * (std::mem::size_of::<Batch>() + 8)) as u64
            + self.held_rows() as u64 * (fact + 255)
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.crc.update(bytes);
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn block(&mut self, t: u8, body: &[u8]) -> Result<(u64, BlockHeader), WriteError> {
        let mut b = Vec::with_capacity(body.len() + 24);
        let h = encode_block(t, 0, body, &mut b)?;
        let at = self.offset;
        self.put(&b)?;
        Ok((at, h))
    }

    fn description_written(&mut self) -> Result<(), WriteError> {
        if self.offset > kvik_log::header::HEADER_LEN as u64 {
            return Ok(());
        }
        let mut body = self.description.encode();
        if let Some((id, v)) = self.tweaks.description_field() {
            push_field(&mut body, id, &v);
        }
        let (_, h) = self.block(block_type::DESCRIPTION, &body)?;
        self.description_crc = h.body_crc32c;
        Ok(())
    }

    /// The index of the batch holding `p`, counted as held once more.
    fn hold(&mut self, p: Position) -> Result<(), WriteError> {
        let i = self
            .batches
            .partition_point(|b| (b.generation, b.last_sequence) < p);
        let b = self
            .batches
            .get(i)
            .filter(|b| b.holds(p.0, p.1))
            .ok_or(content("a position no batch holds"))?;
        let size = b.last_sequence - b.first_sequence + 1;
        self.held[i] += 1;
        if self.held[i] > size {
            return Err(content("more positions than a batch holds"));
        }
        self.range = Some(match self.range {
            None => (p, p),
            Some((a, z)) => (a.min(p), z.max(p)),
        });
        Ok(())
    }

    /// Writes the chunks of one series, which it takes so that its facts are sorted in place. Series come in the
    /// order of B.1 (by name as UTF-8 bytes, then those with no definition by id), each once; facts of a current
    /// file have strictly ascending `t`.
    pub fn series(&mut self, mut s: Series) -> Result<(), WriteError> {
        self.series_head(
            s.series_id,
            s.name.as_deref(),
            s.reach,
            !s.set_aside.is_empty(),
        )?;
        let mut chunks: Vec<(u16, bool, std::ops::Range<usize>)> = Vec::new();
        for (flags, aside) in [(0, false), (SET_ASIDE, true)] {
            let facts = if aside {
                &mut s.set_aside
            } else {
                &mut s.facts
            };
            if facts.iter().any(|f| f.series_id != s.series_id) {
                return Err(content("a fact of another series"));
            }
            facts.sort_unstable_by_key(|f| (f.t, f.position));
            if self.description.class == Class::Current
                && !facts.windows(2).all(|w| w[0].t < w[1].t)
            {
                return Err(content("two facts of one key in a current file"));
            }
            for start in (0..facts.len()).step_by(self.chunk_rows) {
                chunks.push((
                    flags,
                    aside,
                    start..facts.len().min(start + self.chunk_rows),
                ));
            }
        }
        let rows = |aside: bool, r: &std::ops::Range<usize>| {
            if aside {
                &s.set_aside[r.clone()]
            } else {
                &s.facts[r.clone()]
            }
        };
        chunks.sort_by_key(|(_, aside, r)| {
            let c = rows(*aside, r);
            (c[0].t, c.iter().map(|f| f.position).min())
        });
        for (flags, aside, r) in &chunks {
            self.chunk(s.series_id, s.reach, *flags, rows(*aside, r))?;
        }
        Ok(())
    }

    /// The checks every series passes before its chunks: after the description, before the entries, in the order
    /// of B.1, set-aside facts only in a revision file, a positive interval length.
    pub(super) fn series_head(
        &mut self,
        series_id: u32,
        name: Option<&str>,
        reach: Reach,
        has_set_aside: bool,
    ) -> Result<(), WriteError> {
        if self.entries_written {
            return Err(content("a series after the entries"));
        }
        if self.stream.is_some() {
            return Err(content("a series while another is open"));
        }
        self.description_written()?;
        let key = (
            name.is_none(),
            name.unwrap_or("").as_bytes().to_vec(),
            series_id,
        );
        if self
            .last_key
            .as_ref()
            .is_some_and(|k| *k >= key || k.2 == series_id)
        {
            return Err(content("series out of the order of B.1"));
        }
        self.last_key = Some(key);
        if has_set_aside && self.description.class != Class::Revision {
            return Err(content("set-aside facts outside a revision file"));
        }
        if matches!(reach, Reach::Fixed(d) if d <= 0) {
            return Err(content("an interval length of 0 or less"));
        }
        Ok(())
    }

    pub(super) fn chunk(
        &mut self,
        series_id: u32,
        reach: Reach,
        flags: u16,
        facts: &[Fact],
    ) -> Result<(), WriteError> {
        let mut c = self.chunk_begin(series_id, flags);
        for rows in facts.chunks(self.block_rows) {
            self.chunk_block(&mut c, reach, rows)?;
        }
        self.chunk_end(c)
    }

    /// Starts a chunk at the current offset; its blocks follow with [`Self::chunk_block`].
    pub(super) fn chunk_begin(&mut self, series_id: u32, flags: u16) -> ChunkAcc {
        ChunkAcc {
            series_id,
            flags,
            start: self.offset,
            records: Vec::new(),
            rows: 0,
            fold: BoundsFold::default(),
            min_t: i64::MAX,
            max_t: i64::MIN,
            reach: i64::MIN,
            first: None,
            last: Position::default(),
        }
    }

    /// Writes one data block of a chunk.
    pub(super) fn chunk_block(
        &mut self,
        c: &mut ChunkAcc,
        reach: Reach,
        rows: &[Fact],
    ) -> Result<(), WriteError> {
        for f in rows {
            self.hold(f.position)?;
            c.fold.push(f.flags, f.value_type, f.value, f.scale);
            c.min_t = c.min_t.min(f.t);
            c.max_t = c.max_t.max(f.t);
            c.reach = c.reach.max(reach.of(f.t));
            c.first = Some(c.first.map_or(f.position, |p| p.min(f.position)));
            c.last = c.last.max(f.position);
        }
        let block = columns(rows);
        let mut body = block.encode()?;
        if let Some(alt) = self.alt_bodies.remove(&self.data_blocks)
            && alt != body
            && Block::decode_canonical(&alt).as_ref() == Ok(&block)
        {
            body = alt;
        }
        if self.data_blocks == 0
            && let Some((id, payload)) = self.tweaks.data_column()
        {
            body = insert_column(&body, 0, id, &payload);
        }
        self.data_blocks += 1;
        let (at, h) = self.block(block_type::DATA, &body)?;
        c.records.push(BlockRecord {
            first_t: rows[0].t,
            last_t: rows[rows.len() - 1].t,
            offset: u32::try_from(at - c.start).map_err(|_| content("chunk above 4 GiB"))?,
            body_len: h.body_len,
            row_count: rows.len() as u16,
            body_crc32c: h.body_crc32c,
        });
        c.rows += rows.len() as u32;
        Ok(())
    }

    /// Writes a chunk's directory and records its index entry.
    pub(super) fn chunk_end(&mut self, c: ChunkAcc) -> Result<(), WriteError> {
        let ChunkAcc {
            series_id,
            flags,
            start,
            mut records,
            rows,
            fold,
            min_t,
            max_t,
            reach,
            first,
            last,
        } = c;
        let flags = flags | self.tweaks.chunk_flags();
        let mut recorded = rows;
        if self.tweaks.unrecorded_last_block() && records.len() > 1 {
            recorded -= u32::from(records.pop().map_or(0, |r| r.row_count));
        }
        let dir = Directory {
            series_id,
            flags,
            row_count: recorded,
            blocks: records,
            summary: Vec::new(),
        };
        let (dir_at, dh) = self.block(block_type::DIRECTORY, &dir.encode())?;
        let (b, has_nan) = fold.finish();
        let raw = b.map(|b| b.raw()).unwrap_or_default();
        let empty = rows == 0;
        let entry = IndexEntry {
            series_id,
            flags: flags
                | if b.is_some() {
                    super::parts::HAS_BOUNDS
                } else {
                    0
                }
                | if has_nan { super::parts::HAS_NAN } else { 0 },
            value_type: raw.value_type,
            scale: raw.scale,
            min_t: if empty { 0 } else { min_t },
            max_t: if empty { 0 } else { max_t },
            reach: if empty { 0 } else { reach },
            first: first.unwrap_or_default(),
            last,
            chunk_offset: start,
            chunk_len: u32::try_from(self.offset - start)
                .map_err(|_| content("chunk above 4 GiB"))?,
            dir_len: (self.offset - dir_at) as u32,
            row_count: recorded,
            dir_crc32c: dh.body_crc32c,
            min_value: raw.min,
            max_value: raw.max,
        };
        self.index.push(entry);
        self.fact_count += u64::from(rows);
        if !empty {
            self.min_t = self.min_t.min(min_t);
            self.max_reach = self.max_reach.max(reach);
        }
        self.set_aside |= flags & SET_ASIDE != 0;
        Ok(())
    }

    /// Writes the non-fact entries, once, after every series: strictly ascending positions, in a current file.
    /// Per generation a marker, then entries blocks grouped as `format.md` A.16 groups them (one batch,
    /// consecutive sequences, a recovery entry alone, at most 16 MiB a body).
    pub fn entries(&mut self, rows: &[(Position, Entry)]) -> Result<(), WriteError> {
        if self.stream.is_some() {
            return Err(content("entries while a series is open"));
        }
        if self.entries_written {
            return Err(content("entries written twice"));
        }
        if rows.is_empty() {
            return Ok(());
        }
        if self.description.class != Class::Current {
            return Err(content("entries outside a current file"));
        }
        if !rows.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(content("entries not in position order"));
        }
        self.description_written()?;
        self.entries_written = true;
        let mut generations: Vec<(u32, Vec<EntriesBlock>)> = Vec::new();
        let mut body_len = 0usize;
        for (p, e) in rows {
            if matches!(e, Entry::Ignored { .. }) {
                return Err(content("an ignored entry is never written"));
            }
            self.hold(*p)?;
            let batch = self.batches[self
                .batches
                .partition_point(|b| (b.generation, b.last_sequence) < *p)];
            let len = EntriesBlock {
                first_sequence: p.1,
                received_at: batch.received_at,
                entries: vec![e.clone()],
            }
            .encode()?
            .len()
                - kvik_log::body::DATA_PREFIX_LEN;
            if generations.last().is_none_or(|g| g.0 != p.0) {
                generations.push((p.0, Vec::new()));
            }
            let blocks = &mut generations.last_mut().expect("pushed above").1;
            let continues = !e.is_recovery()
                && blocks.last().is_some_and(|b| {
                    !b.entries.iter().any(Entry::is_recovery)
                        && batch.holds(p.0, b.first_sequence)
                        && b.first_sequence + b.entries.len() as u64 == p.1
                        && body_len + len <= MAX_BODY_LEN as usize
                });
            if !continues {
                blocks.push(EntriesBlock {
                    first_sequence: p.1,
                    received_at: batch.received_at,
                    entries: Vec::new(),
                });
                body_len = kvik_log::body::DATA_PREFIX_LEN;
            }
            body_len += len;
            blocks
                .last_mut()
                .expect("pushed above")
                .entries
                .push(e.clone());
        }
        let mut section = Vec::new();
        for (g, blocks) in &generations {
            let marker: [u8; MARKER_LEN] = encode_marker(*g, blocks.len() as u32);
            encode_block(block_type::MARKER, 0, &marker, &mut section)?;
            for b in blocks {
                encode_block(block_type::ENTRIES, 0, &b.encode()?, &mut section)?;
            }
        }
        self.entries_offset = self.offset;
        self.entries_crc = crc32c(&section);
        self.entry_positions = rows.len() as u64;
        self.put(&section)?;
        Ok(())
    }

    /// Writes the commit-time table, the chunk index, the trailer and the end marker; returns the output.
    pub fn finish(mut self) -> Result<(W, Written), WriteError> {
        if self.stream.is_some() {
            return Err(content("finished while a series is open"));
        }
        self.description_written()?;
        if self.description.has_entries != self.entries_written
            || self.description.has_set_aside != self.set_aside
        {
            return Err(content("description flags disagree with the content"));
        }
        let Some((first, last)) = self.range else {
            return Err(content("a file holds at least one position"));
        };
        let mut positions: Vec<(u32, u64)> = Vec::new();
        for (b, &n) in self.batches.iter().zip(&self.held).filter(|(_, n)| **n > 0) {
            match positions.last_mut() {
                Some((g, count)) if *g == b.generation => *count += n,
                _ => positions.push((b.generation, n)),
            }
        }
        if positions != self.description.positions {
            return Err(content(
                "description positions per generation disagree with the content",
            ));
        }
        let used: Vec<Batch> = match self.tweaks.take_times_rows() {
            Some(rows) => rows,
            None => self
                .batches
                .iter()
                .zip(&self.held)
                .filter(|(_, n)| **n > 0)
                .map(|(b, _)| *b)
                .collect(),
        };
        let times_offset = self.offset;
        let mut times = Vec::new();
        let mut at = 0;
        while at < used.len() {
            let g = used[at].generation;
            let end = at + used[at..].iter().take_while(|b| b.generation == g).count();
            for rows in used[at..end].chunks(TIMES_ROWS) {
                let extra = self.tweaks.take_times_column();
                let body = encode_times(g, rows, extra.as_ref().map(|(i, p)| (*i, p.as_slice())));
                let (offset, h) = self.block(block_type::TIMES, &body)?;
                times.push(TimesRecord {
                    generation: g,
                    body_crc32c: h.body_crc32c,
                    first_sequence: rows[0].first_sequence,
                    last_sequence: rows[rows.len() - 1].last_sequence,
                    first_commit_time: rows[0].commit_time,
                    last_commit_time: rows[rows.len() - 1].commit_time,
                    offset,
                });
            }
            at = end;
        }
        let (times_dir_offset, tdh) =
            self.block(block_type::TIMES_DIRECTORY, &encode_times_directory(&times))?;
        let mut index = std::mem::take(&mut self.index);
        index.sort_by_key(sort_key);
        let index_offset = if index.is_empty() { 0 } else { self.offset };
        let mut roots = Vec::with_capacity(index.len().div_ceil(INDEX_BLOCK_ENTRIES));
        for entries in index.chunks(INDEX_BLOCK_ENTRIES) {
            let (offset, h) = self.block(block_type::INDEX, &encode_index_block(entries))?;
            roots.push(RootRecord {
                first_series_id: entries[0].series_id,
                last_series_id: entries[entries.len() - 1].series_id,
                first_min_t: entries[0].min_t,
                offset: offset - index_offset,
                entry_count: entries.len() as u32,
                body_crc32c: h.body_crc32c,
            });
        }
        let (index_root_offset, rh) = self.block(block_type::ROOT, &encode_root(&roots))?;
        let no_fact = self.fact_count == 0;
        let trailer = Trailer {
            entry_count: self.fact_count + self.entry_positions,
            fact_count: self.fact_count,
            first,
            last,
            first_commit_time: used.iter().map(|b| b.commit_time).min().unwrap_or(0),
            last_commit_time: used.iter().map(|b| b.commit_time).max().unwrap_or(0),
            min_t: if no_fact { 0 } else { self.min_t },
            max_reach: if no_fact { 0 } else { self.max_reach },
            chunk_count: u32::try_from(index.len()).map_err(|_| content("too many chunks"))?,
            times_count: times.len() as u32,
            entries_offset: self.entries_offset,
            times_offset,
            times_dir_offset,
            index_offset,
            index_root_offset,
            file_crc32c: self.crc.clone().finish(),
            root_crc32c: rh.body_crc32c,
            times_dir_crc32c: tdh.body_crc32c,
            description_crc32c: self.description_crc,
            entries_crc32c: self.entries_crc,
        };
        let mut body = trailer.encode();
        body.extend_from_slice(self.tweaks.trailer_tail());
        let (trailer_offset, _) = self.block(block_type::TRAILER, &body)?;
        self.put(&encode_end_marker(trailer_offset))?;
        let layout = Layout::of(&self.id, &self.description, &trailer);
        Ok((
            self.out,
            Written {
                len: self.offset,
                trailer,
                layout,
            },
        ))
    }
}
