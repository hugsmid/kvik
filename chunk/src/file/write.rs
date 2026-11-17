//! The compacted segment writer (`docs/format-compacted.md` B.1): one series at a time, then the entries, then the
//! commit-time table, the chunk index and the trailer. For given rows, registry state, class, level, period and
//! inputs it writes one byte sequence (B.1 Canonical writing).

use std::io::{self, Write};

use crc::{CRC_32_ISCSI, Crc, Digest, Table};
use kvik_log::block::{BlockHeader, MAX_BODY_LEN, encode_block};
use kvik_log::body::encode_end_marker;
use kvik_log::crc::crc32c;
use kvik_log::entries::{EntriesBlock, Entry};
use kvik_log::header::FileHeader;

use super::desc::{Class, Description, push_field};
use super::parts::{
    BlockRecord, Directory, IndexEntry, MARKER_LEN, RootRecord, SET_ASIDE, TimesRecord, Trailer,
    encode_index_block, encode_marker, encode_root, encode_times_directory, sort_key,
};
use super::read::{Identity, Layout};
use super::stats::{Reach, bounds};
use super::times::{Batch, encode_times};
use super::{
    BLOCK_ROWS, CHUNK_ROWS, INDEX_BLOCK_ENTRIES, KIND_COMPACTED, Position, TIMES_ROWS, block_type,
};
use crate::block::directory;
use crate::bytes::put_leb128;
use crate::{Block, flag, value_type};

static CASTAGNOLI: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);

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

/// Departures from canonical writing, for golden files of unknown elements and for the canonical check, which
/// hands the file's own data block bodies back (a compressed XOR payload is a writer's choice, C.10).
#[derive(Clone, Debug, Default)]
pub struct Tweaks {
    /// Bits or'ed into every chunk's directory and index flags.
    pub chunk_flags: u16,
    /// A plain column added to the first data block.
    pub data_column: Option<(u8, Vec<u8>)>,
    /// A plain column added to the first commit-times block.
    pub times_column: Option<(u8, Vec<u8>)>,
    /// A field appended to the description.
    pub description_field: Option<(u16, Vec<u8>)>,
    /// Bytes appended to the trailer body.
    pub trailer_tail: Vec<u8>,
    /// Data block bodies in file order, each used where it is a canonical body of the same rows.
    pub bodies: Vec<Vec<u8>>,
}

/// Writes one compacted segment.
pub struct FileWriter<W: Write> {
    out: W,
    offset: u64,
    crc: Digest<'static, u32, Table<16>>,
    id: Identity,
    description: Description,
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
            crc: CASTAGNOLI.digest(),
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
    pub fn with_tweaks(mut self, tweaks: Tweaks) -> Self {
        self.tweaks = tweaks;
        self
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
        if let Some((id, v)) = self.tweaks.description_field.clone() {
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

    /// Writes the chunks of one series. Series come in the order of B.1 (by name as UTF-8 bytes, then those with
    /// no definition by id), each once; facts of a current file have strictly ascending `t`.
    pub fn series(&mut self, s: &Series) -> Result<(), WriteError> {
        if self.entries_written {
            return Err(content("a series after the entries"));
        }
        self.description_written()?;
        let key = (
            s.name.is_none(),
            s.name.as_deref().unwrap_or("").as_bytes().to_vec(),
            s.series_id,
        );
        if self
            .last_key
            .as_ref()
            .is_some_and(|k| *k >= key || k.2 == s.series_id)
        {
            return Err(content("series out of the order of B.1"));
        }
        self.last_key = Some(key);
        if !s.set_aside.is_empty() && self.description.class != Class::Revision {
            return Err(content("set-aside facts outside a revision file"));
        }
        let mut chunks: Vec<(u16, Vec<Fact>)> = Vec::new();
        for (flags, facts) in [(0, &s.facts), (SET_ASIDE, &s.set_aside)] {
            if facts.iter().any(|f| f.series_id != s.series_id) {
                return Err(content("a fact of another series"));
            }
            let mut sorted = facts.clone();
            sorted.sort_by_key(|f| (f.t, f.position));
            if self.description.class == Class::Current
                && !sorted.windows(2).all(|w| w[0].t < w[1].t)
            {
                return Err(content("two facts of one key in a current file"));
            }
            for c in sorted.chunks(CHUNK_ROWS) {
                chunks.push((flags, c.to_vec()));
            }
        }
        let first_of = |c: &[Fact]| c.iter().map(|f| f.position).min();
        chunks.sort_by_key(|(_, c)| (c[0].t, first_of(c)));
        for (flags, facts) in chunks {
            self.chunk(s, flags, &facts)?;
        }
        Ok(())
    }

    fn chunk(&mut self, s: &Series, flags: u16, facts: &[Fact]) -> Result<(), WriteError> {
        for f in facts {
            self.hold(f.position)?;
        }
        let start = self.offset;
        let mut records = Vec::with_capacity(facts.len().div_ceil(BLOCK_ROWS));
        for rows in facts.chunks(BLOCK_ROWS) {
            let block = columns(rows);
            let mut body = block.encode()?;
            if let Some(alt) = self.tweaks.bodies.get(self.data_blocks)
                && Block::decode_canonical(alt).as_ref() == Ok(&block)
            {
                body = alt.clone();
            }
            if self.data_blocks == 0
                && let Some((id, payload)) = &self.tweaks.data_column
            {
                body = insert_column(&body, 0, *id, payload);
            }
            self.data_blocks += 1;
            let (at, h) = self.block(block_type::DATA, &body)?;
            records.push(BlockRecord {
                first_t: rows[0].t,
                last_t: rows[rows.len() - 1].t,
                offset: u32::try_from(at - start).map_err(|_| content("chunk above 4 GiB"))?,
                body_len: h.body_len,
                row_count: rows.len() as u16,
                body_crc32c: h.body_crc32c,
            });
        }
        let flags = flags | self.tweaks.chunk_flags;
        let dir = Directory {
            series_id: s.series_id,
            flags,
            row_count: facts.len() as u32,
            blocks: records,
            summary: Vec::new(),
        };
        let (dir_at, dh) = self.block(block_type::DIRECTORY, &dir.encode())?;
        let (b, has_nan) = bounds(
            facts
                .iter()
                .map(|f| (f.flags, f.value_type, f.value, f.scale)),
        );
        let raw = b.map(|b| b.raw()).unwrap_or_default();
        let min_t = facts.iter().map(|f| f.t).min().unwrap_or(0);
        let reach = facts.iter().map(|f| s.reach.of(f.t)).max().unwrap_or(0);
        let entry = IndexEntry {
            series_id: s.series_id,
            flags: flags
                | if b.is_some() {
                    super::parts::HAS_BOUNDS
                } else {
                    0
                }
                | if has_nan { super::parts::HAS_NAN } else { 0 },
            value_type: raw.value_type,
            scale: raw.scale,
            min_t,
            max_t: facts.iter().map(|f| f.t).max().unwrap_or(0),
            reach,
            first: facts.iter().map(|f| f.position).min().unwrap_or_default(),
            last: facts.iter().map(|f| f.position).max().unwrap_or_default(),
            chunk_offset: start,
            chunk_len: u32::try_from(self.offset - start)
                .map_err(|_| content("chunk above 4 GiB"))?,
            dir_len: (self.offset - dir_at) as u32,
            row_count: facts.len() as u32,
            dir_crc32c: dh.body_crc32c,
            min_value: raw.min,
            max_value: raw.max,
        };
        self.index.push(entry);
        self.fact_count += facts.len() as u64;
        self.min_t = self.min_t.min(min_t);
        self.max_reach = self.max_reach.max(reach);
        self.set_aside |= flags & SET_ASIDE != 0;
        Ok(())
    }

    /// Writes the non-fact entries, once, after every series: strictly ascending positions, in a current file.
    /// Per generation a marker, then entries blocks grouped as `format.md` A.16 groups them (one batch,
    /// consecutive sequences, a recovery entry alone, at most 16 MiB a body).
    pub fn entries(&mut self, rows: &[(Position, Entry)]) -> Result<(), WriteError> {
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
        self.description_written()?;
        if self.description.has_entries != self.entries_written
            || self.description.has_set_aside != self.set_aside
        {
            return Err(content("description flags disagree with the content"));
        }
        let Some((first, last)) = self.range else {
            return Err(content("a file holds at least one position"));
        };
        let used: Vec<Batch> = self
            .batches
            .iter()
            .zip(&self.held)
            .filter(|(_, n)| **n > 0)
            .map(|(b, _)| *b)
            .collect();
        let times_offset = self.offset;
        let mut times = Vec::new();
        let mut at = 0;
        while at < used.len() {
            let g = used[at].generation;
            let end = at + used[at..].iter().take_while(|b| b.generation == g).count();
            for rows in used[at..end].chunks(TIMES_ROWS) {
                let extra = self.tweaks.times_column.take();
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
            file_crc32c: self.crc.clone().finalize(),
            root_crc32c: rh.body_crc32c,
            times_dir_crc32c: tdh.body_crc32c,
            description_crc32c: self.description_crc,
            entries_crc32c: self.entries_crc,
        };
        let mut body = trailer.encode();
        body.extend_from_slice(&self.tweaks.trailer_tail);
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
