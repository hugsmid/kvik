//! The canonical checks (`docs/format-compacted.md` B.1): the writer's read-back, golden files and the scrub. The
//! file is read through the reader's checks, written again from what it holds with the same registry state, and
//! must come out byte for byte; a data block in the XOR class keeps the file's own compressed payload where
//! that is a canonical body of the same rows (`docs/format-chunk.md` C.10).

use super::Position;
use super::access::facts_of;
use super::error::FileError;
use super::layout::{Identity, Ignored};
use super::parts::{Directory, IndexEntry, SET_ASIDE};
use super::read::CompactedFile;
use super::source::ReadAt;
use super::stats::Reach;
use super::times::Batch;
use super::write::{FileWriter, WriteError};

/// What the registry says about a series at a file's greatest position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesInfo {
    /// Its primary name; `None` for a series with no definition.
    pub name: Option<String>,
    /// The reach of its readings.
    pub reach: Reach,
}

/// Why a file fails the canonical checks.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    /// It fails a reader's check.
    #[error(transparent)]
    File(#[from] FileError),
    /// What it holds breaks a writer's rule.
    #[error("the content cannot be written: {0}")]
    Content(#[from] WriteError),
    /// A position held twice.
    #[error("a position held twice")]
    Positions,
    /// It holds elements this version does not know and skipped: it is read, but not judged.
    #[error("holds elements this version skipped: {0:?}")]
    Unknown(Ignored),
    /// Its bytes differ from the canonical ones from this offset.
    #[error("not canonical from offset {0}")]
    Bytes(u64),
}

/// Applies the canonical checks to a whole compacted segment, with `registry` giving each series' name and reach.
pub fn check_canonical(
    bytes: &[u8],
    id: &Identity,
    registry: &dyn Fn(u32) -> SeriesInfo,
) -> Result<(), CanonicalError> {
    let mut c = CanonicalCheck::new(bytes, id, registry)?;
    while !c.step()? {}
    Ok(())
}

/// The canonical checks a data block at a time (Q-811): the file is read through a [`ReadAt`] series by series and
/// written again into a sink that compares as it goes, holding a series' set-aside facts, one block and a bit per
/// position. Each step makes its reads before it changes anything, so a step whose read fails with
/// [`std::io::ErrorKind::WouldBlock`] can be taken again; the sink keeps what it could not yet compare until the next
/// step.
pub struct CanonicalCheck<S: ReadAt + Clone, R: Fn(u32) -> SeriesInfo> {
    file: CompactedFile<S>,
    writer: Option<FileWriter<Compare<S>>>,
    registry: Option<R>,
    /// Chunks in file order once read; directories are read as their chunk is reached.
    order: Vec<IndexEntry>,
    batches: Vec<Batch>,
    held: Vec<Vec<u64>>,
    /// The next index and commit-times block to read, until the writer starts.
    opening: Option<(usize, usize)>,
    /// The next chunk and data block.
    at: (usize, usize),
    /// The number of the next chunk's first data block, in write order.
    next_block: usize,
    dir: Option<Directory>,
    /// The chunk that ends the open series.
    series_end: Option<usize>,
    /// Blocks of the open series' set-aside chunks, read when it opened, by chunk.
    aside_blocks: Vec<(usize, usize)>,
    end: End,
}

/// What is left once every chunk is checked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Entries,
    Tail,
    Verdict,
}

impl<S: ReadAt + Clone, R: Fn(u32) -> SeriesInfo> CanonicalCheck<S, R> {
    /// Opens the file through the reader's checks; the index and commit times are read a block a step.
    pub fn new(src: S, id: &Identity, registry: R) -> Result<Self, CanonicalError> {
        Ok(Self::from_file(
            CompactedFile::open(src, id, None)?,
            registry,
        ))
    }

    /// [`Self::new`] for a file the caller has opened.
    pub fn from_file(file: CompactedFile<S>, registry: R) -> Self {
        let order = Vec::with_capacity(file.listed_entries());
        Self {
            file,
            writer: None,
            registry: Some(registry),
            order,
            batches: Vec::new(),
            held: Vec::new(),
            opening: Some((0, 0)),
            at: (0, 0),
            next_block: 0,
            dir: None,
            series_end: None,
            aside_blocks: Vec::new(),
            end: End::Entries,
        }
    }

    /// The file and its index in file order, for a caller that reads the file again after the check.
    pub fn into_parts(self) -> (CompactedFile<S>, Vec<IndexEntry>) {
        (self.file, self.order)
    }

    /// The file's index in file order, for a caller that reads the file again after the check.
    pub fn into_order(self) -> Vec<IndexEntry> {
        self.order
    }

    /// Bytes it holds besides the file's blocks: its index, the writer's, the position bits and what the sink has
    /// yet to compare.
    pub fn held_bytes(&self) -> u64 {
        let entry = std::mem::size_of::<IndexEntry>() as u64;
        self.order.capacity() as u64 * entry
            + self
                .writer
                .as_ref()
                .map_or(0, |w| w.held_bytes() + w.sink().pending.capacity() as u64)
            + (self.batches.capacity() * std::mem::size_of::<Batch>()) as u64
            + self
                .held
                .iter()
                .map(|b| 24 + 8 * b.capacity() as u64)
                .sum::<u64>()
    }

    fn writer(&mut self) -> &mut FileWriter<Compare<S>> {
        self.writer.as_mut().expect("until the last step")
    }

    /// Counts `p` as held once, in a batch of the file.
    fn hold(&mut self, p: Position) -> Result<(), CanonicalError> {
        let i = self
            .batches
            .partition_point(|b| (b.generation, b.last_sequence) < p);
        let Some(b) = self.batches.get(i).filter(|b| b.holds(p.0, p.1)) else {
            if let Err(e) = self.file.batch(p)
                && !blocked(&e)
            {
                return Err(e.into());
            }
            return Err(CanonicalError::Positions);
        };
        let k = (p.1 - b.first_sequence) as usize;
        let bits = &mut self.held[i];
        if bits.is_empty() {
            *bits = vec![0; ((b.last_sequence - b.first_sequence) as usize / 64) + 1];
        }
        if bits[k / 64] & (1 << (k % 64)) != 0 {
            return Err(CanonicalError::Positions);
        }
        bits[k / 64] |= 1 << (k % 64);
        Ok(())
    }

    /// One block of the index or commit times, or the writer's start once both are read.
    fn open_step(&mut self, (i, t): (usize, usize)) -> Result<(), CanonicalError> {
        if i < self.file.index_blocks() {
            let b = self.file.index_block(i)?;
            self.order.extend(b);
            self.opening = Some((i + 1, t));
            return Ok(());
        }
        if t < self.file.times_directory().len() {
            let rows = self.file.times_block(t)?;
            self.batches.extend_from_slice(rows);
            self.opening = Some((i, t + 1));
            return Ok(());
        }
        self.order.sort_unstable_by_key(|e| e.chunk_offset);
        let compare = Compare {
            source: self.file.source().clone(),
            at: 0,
            first_difference: None,
            pending: Vec::new(),
        };
        let h = self.file.header();
        let id = Identity {
            store_id: h.store_id,
            generation: h.generation,
            file_id: h.file_id,
        };
        let mut writer = FileWriter::new(
            compare,
            id,
            h.created_at,
            self.file.description(),
            self.batches.clone(),
        )?;
        writer.reserve_index(self.order.len());
        self.held = vec![Vec::new(); self.batches.len()];
        self.writer = Some(writer);
        self.opening = None;
        Ok(())
    }

    /// Opens the series of the chunk at `i`: its run of chunks, and its set-aside facts read whole, all read before
    /// any is taken.
    fn open_series(
        &mut self,
        i: usize,
        registry: &dyn Fn(u32) -> SeriesInfo,
    ) -> Result<(), CanonicalError> {
        let id = self.order[i].series_id;
        let end = i + self.order[i..]
            .iter()
            .take_while(|e| e.series_id == id)
            .count();
        let last_aside = (i..end)
            .rev()
            .find(|&k| self.order[k].flags & SET_ASIDE != 0);
        let mut read = Vec::new();
        for k in i..last_aside.map_or(i, |k| k + 1) {
            let e = self.order[k];
            let d = self.file.directory(&e)?;
            let mut blocks = Vec::new();
            if e.flags & SET_ASIDE != 0 {
                for b in 0..d.blocks.len() {
                    blocks.push(self.file.data_block_raw(&e, &d, b)?);
                }
            }
            read.push((k, d.blocks.len(), e.flags & SET_ASIDE != 0, blocks));
        }
        let mut aside = Vec::new();
        self.aside_blocks.clear();
        let mut first = self.next_block;
        for (k, n, set_aside, blocks) in read {
            if set_aside {
                for (b, (block, body)) in blocks.into_iter().enumerate() {
                    for f in facts_of(id, &block) {
                        self.hold(f.position)?;
                        aside.push(f);
                    }
                    self.writer().offer_body(first + b, body);
                }
                self.aside_blocks.push((k, n));
            }
            first += n;
        }
        let info = registry(id);
        self.writer()
            .begin_series(id, info.name.as_deref(), info.reach, aside)?;
        self.series_end = Some(end);
        Ok(())
    }

    /// Checks one data block, or ends a series, or at the end the entries and the tail; `true` once the file passed.
    pub fn step(&mut self) -> Result<bool, CanonicalError> {
        let registry = self.registry.take().expect("held between steps");
        let r = self.step_with(&registry);
        self.registry = Some(registry);
        r
    }

    /// [`Self::step`] with the registry given for this step, for a caller that cannot lend it for the whole check.
    pub fn step_with(
        &mut self,
        registry: &dyn Fn(u32) -> SeriesInfo,
    ) -> Result<bool, CanonicalError> {
        if let Some(w) = self.writer.as_mut() {
            w.sink_mut().settle().map_err(FileError::from)?;
        }
        if let Some(at) = self.opening {
            self.open_step(at)?;
            return Ok(false);
        }
        let (i, b) = self.at;
        if i == self.order.len() {
            return self.finish_step();
        }
        if self.series_end.is_none() {
            self.open_series(i, registry)?;
        }
        let e = self.order[i];
        let first = self.next_block;
        if e.flags & SET_ASIDE == 0 {
            if self.dir.is_none() {
                self.dir = Some(self.file.directory(&e)?);
            }
            let d = self.dir.as_ref().expect("read above");
            let n = d.blocks.len();
            if b < n {
                let (block, body) = self.file.data_block_raw(&e, d, b)?;
                self.writer().offer_body(first + b, body);
                for f in facts_of(e.series_id, &block) {
                    self.hold(f.position)?;
                    self.writer().push(f)?;
                }
                self.at.1 += 1;
                return Ok(false);
            }
            self.next_block += n;
        } else {
            self.next_block += self
                .aside_blocks
                .iter()
                .find(|a| a.0 == i)
                .map_or(0, |a| a.1);
        }
        self.dir = None;
        self.at = (i + 1, 0);
        if self.series_end == Some(i + 1) {
            self.writer().end_series()?;
            self.series_end = None;
        }
        Ok(false)
    }

    /// The entries, then the tail an index block a step, then the verdict once the sink has compared every byte.
    fn finish_step(&mut self) -> Result<bool, CanonicalError> {
        match self.end {
            End::Entries => {
                let rows = self.file.entries()?;
                let mut entries = Vec::with_capacity(rows.len());
                for r in rows {
                    if matches!(r.entry, kvik_log::entries::Entry::Ignored { .. }) {
                        return Err(CanonicalError::Unknown(self.file.ignored()));
                    }
                    self.hold(r.position)?;
                    entries.push((r.position, r.entry));
                }
                if !self.file.ignored().is_empty() {
                    return Err(CanonicalError::Unknown(self.file.ignored()));
                }
                self.writer().entries(&entries)?;
                self.end = End::Tail;
            }
            End::Tail => {
                if self.writer().finish_step()? {
                    self.end = End::Verdict;
                }
            }
            End::Verdict => {
                let len = self.file.source().len();
                let c = self.writer().sink();
                return match c.first_difference {
                    None if c.at == len => Ok(true),
                    None => Err(CanonicalError::Bytes(c.at.min(len))),
                    Some(at) => Err(CanonicalError::Bytes(at)),
                };
            }
        }
        Ok(false)
    }
}

fn blocked(e: &FileError) -> bool {
    matches!(e, FileError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// The most bytes the comparing sink reads from the file at a time.
const WINDOW: u64 = 128 * 1024;

/// A sink that compares what the writer writes with the file as it is produced, at most a window a read; bytes it
/// cannot read yet ([`std::io::ErrorKind::WouldBlock`]) wait in `pending` for [`Compare::settle`].
struct Compare<S: ReadAt> {
    source: S,
    /// The offset after the last byte written.
    at: u64,
    first_difference: Option<u64>,
    pending: Vec<u8>,
}

impl<S: ReadAt> Compare<S> {
    /// Compares `buf` as the bytes at `at`; the number of bytes compared before a read would block.
    fn compare(&mut self, mut at: u64, buf: &[u8]) -> std::io::Result<usize> {
        let len = self.source.len();
        let mut done = 0;
        while done < buf.len() && self.first_difference.is_none() {
            if at >= len {
                self.first_difference = Some(at);
                break;
            }
            let n = (buf.len() - done).min(WINDOW.min(len - at) as usize);
            let have = match self.source.read_at(at, n) {
                Ok(h) => h,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(done),
                Err(e) => return Err(e),
            };
            if let Some(k) = buf[done..done + n]
                .iter()
                .zip(&have)
                .position(|(a, b)| a != b)
            {
                self.first_difference = Some(at + k as u64);
            }
            done += n;
            at += n as u64;
        }
        Ok(buf.len())
    }

    /// Compares what waits; [`std::io::ErrorKind::WouldBlock`] while some still cannot be read.
    fn settle(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let p = std::mem::take(&mut self.pending);
        let at = self.at - p.len() as u64;
        let done = self.compare(at, &p)?;
        if done < p.len() {
            self.pending = p[done..].to_vec();
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
}

impl<S: ReadAt> std::io::Write for Compare<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            let done = self.compare(self.at, buf)?;
            self.pending.extend_from_slice(&buf[done..]);
        } else {
            self.pending.extend_from_slice(buf);
        }
        self.at += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::super::desc::{Class, Description, Input, Level};
    use super::super::write::{Fact, Series};
    use super::*;

    struct Counting<'a>(&'a [u8], &'a Cell<usize>);

    impl ReadAt for Counting<'_> {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
            self.1.set(self.1.get().max(len));
            self.0.read_at(offset, len)
        }
    }

    impl Clone for Counting<'_> {
        fn clone(&self) -> Self {
            Counting(self.0, self.1)
        }
    }

    #[test]
    fn the_check_of_a_file_of_two_chunks_reads_at_most_a_window_at_a_time() {
        let n = super::super::CHUNK_ROWS as u64 + 10_000;
        let id = Identity {
            store_id: [5; 16],
            generation: 1,
            file_id: 7,
        };
        let d = Description {
            class: Class::Current,
            level: Level::L0,
            period_start: 0,
            period_len: i64::MAX / 2,
            run_id: 7,
            run_count: 1,
            inputs: vec![Input {
                kind: 1,
                file_id: 1,
            }],
            name_range: None,
            positions: vec![(1, n)],
            has_entries: false,
            has_set_aside: false,
            complete: true,
        };
        let batches = vec![Batch {
            generation: 1,
            first_sequence: 0,
            last_sequence: n - 1,
            received_at: 0,
            commit_time: 0,
        }];
        let mut w = FileWriter::new(Vec::new(), id, 0, &d, batches).unwrap();
        w.series(Series {
            series_id: 1,
            name: Some("s".into()),
            reach: Reach::Point,
            facts: (0..n)
                .map(|i| {
                    Fact::value(1, (1, i), i as i64 * 1_000, 1, {
                        let x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                        x ^ (x >> 29) ^ (x << 17)
                    })
                })
                .collect(),
            set_aside: Vec::new(),
        })
        .unwrap();
        let (bytes, _) = w.finish().unwrap();
        let largest = Cell::new(0);
        let info = |_| SeriesInfo {
            name: Some("s".into()),
            reach: Reach::Point,
        };
        let mut c = CanonicalCheck::new(Counting(&bytes, &largest), &id, info).unwrap();
        let mut steps = 0;
        while !c.step().unwrap() {
            steps += 1;
        }
        assert!(steps > (n as usize).div_ceil(super::super::BLOCK_ROWS));
        assert!(
            largest.get() as u64 <= WINDOW && (bytes.len() as u64) > 4 * WINDOW,
            "largest read {} of a file of {}",
            largest.get(),
            bytes.len()
        );
    }

    /// A revision file of one series of `f64` ninths, whose value columns are compressed XOR (C.9): a full chunk, a
    /// set-aside chunk that sorts after its first time, and a chunk of the rest.
    fn aside_then_ninths(bodies: Vec<Vec<u8>>) -> (Vec<u8>, Identity) {
        aside_then_ninths_reaching(bodies, Reach::Point)
    }

    fn aside_then_ninths_reaching(bodies: Vec<Vec<u8>>, reach: Reach) -> (Vec<u8>, Identity) {
        let (aside, n) = (4_000u64, super::super::CHUNK_ROWS as u64 + 6_000);
        let id = Identity {
            store_id: [5; 16],
            generation: 1,
            file_id: 8,
        };
        let d = Description {
            class: Class::Revision,
            level: Level::L0,
            period_start: 0,
            period_len: i64::MAX / 2,
            run_id: 8,
            run_count: 1,
            inputs: vec![Input {
                kind: 1,
                file_id: 1,
            }],
            name_range: None,
            positions: vec![(1, aside + n)],
            has_entries: false,
            has_set_aside: true,
            complete: true,
        };
        let batches = vec![Batch {
            generation: 1,
            first_sequence: 0,
            last_sequence: aside + n - 1,
            received_at: 0,
            commit_time: 0,
        }];
        let ninth = |i: u64| (f64::from(((i / 2) % 5) as u32) / 9.0 + 1.0).to_bits();
        let f64_ = crate::value_type::F64;
        let mut w = FileWriter::new(Vec::new(), id, 0, &d, batches)
            .unwrap()
            .with_tweaks(super::super::Tweaks {
                bodies,
                ..Default::default()
            });
        w.series(Series {
            series_id: 1,
            name: Some("s".into()),
            reach,
            facts: (aside..aside + n)
                .map(|i| Fact::value(1, (1, i), 1_000_000 + (i - aside) as i64, f64_, ninth(i)))
                .collect(),
            set_aside: (0..aside)
                .map(|i| Fact::value(1, (1, i), 1_000_010 + i as i64, f64_, ninth(i)))
                .collect(),
        })
        .unwrap();
        (w.finish().unwrap().0, id)
    }

    /// The file's data block bodies in file order, each value column in compressed XOR replaced by its plain XOR
    /// stream: a canonical body this writer does not choose (C.10).
    fn plain_xor_bodies(bytes: &[u8], id: &Identity) -> (Vec<Vec<u8>>, usize) {
        let mut chunks = Vec::new();
        let file = CompactedFile::open(bytes, id, None).unwrap();
        let mut order = file.index().unwrap();
        order.sort_unstable_by_key(|e| e.chunk_offset);
        let (mut out, mut changed) = (Vec::new(), 0);
        for e in &order {
            let d = file.directory(e).unwrap();
            chunks.push((e.flags & SET_ASIDE != 0, d.blocks.len()));
            for b in 0..d.blocks.len() {
                let (block, body) = file.data_block_raw(e, &d, b).unwrap();
                let cols = crate::block::directory(&body).unwrap();
                let words = block.len();
                let swapped: Vec<(u8, u8, Vec<u8>)> = cols
                    .iter()
                    .map(|&(c, enc, p)| {
                        if c == crate::column::VALUE && enc == crate::encoding::XOR_ZSTD {
                            let stream = crate::float::decompress_xor(p, words).unwrap();
                            (c, crate::encoding::XOR, stream)
                        } else {
                            (c, enc, p.to_vec())
                        }
                    })
                    .collect();
                let refs: Vec<(u8, u8, &[u8])> = swapped
                    .iter()
                    .map(|(c, e, p)| (*c, *e, p.as_slice()))
                    .collect();
                let alt = crate::block::tests::rebuild(block.len() as u16, &refs);
                if alt != body {
                    changed += 1;
                }
                out.push(alt);
            }
        }
        assert_eq!(chunks, [(false, 64), (true, 1), (false, 2)]);
        (out, changed)
    }

    #[test]
    fn a_file_keeps_its_own_xor_bodies_in_chunks_before_within_and_after_a_set_aside_chunk() {
        let (canonical, id) = aside_then_ninths(Vec::new());
        let (bodies, changed) = plain_xor_bodies(&canonical, &id);
        assert!(changed >= 66, "{changed} bodies changed");
        let (own, _) = aside_then_ninths(bodies);
        assert_ne!(own, canonical);
        let info = |_| SeriesInfo {
            name: Some("s".into()),
            reach: Reach::Point,
        };
        check_canonical(&own, &id, &info).unwrap();
        check_canonical(&canonical, &id, &info).unwrap();
    }

    /// A source each of whose reads fails once with `WouldBlock` before it succeeds.
    #[derive(Clone)]
    struct Shy<'a>(
        &'a [u8],
        &'a std::cell::RefCell<std::collections::HashSet<(u64, usize)>>,
    );

    impl ReadAt for Shy<'_> {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
            if self.1.borrow_mut().insert((offset, len)) {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            self.0.read_at(offset, len)
        }
    }

    /// The check over `Shy`, each step that waits taken again; the verdict and how many steps waited.
    fn shy_check(bytes: &[u8], id: &Identity) -> (Result<(), String>, usize) {
        let seen = std::cell::RefCell::default();
        let info = |_| SeriesInfo {
            name: Some("s".into()),
            reach: Reach::Point,
        };
        let mut c = loop {
            match CanonicalCheck::new(Shy(bytes, &seen), id, info) {
                Err(CanonicalError::File(FileError::Io(e)))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                r => break r.map_err(|e| e.to_string()),
            }
        };
        let mut waited = 0;
        let verdict = loop {
            let Ok(check) = c.as_mut() else {
                break c.map(|_| ()).map_err(|e| e.clone());
            };
            match check.step() {
                Ok(true) => break Ok(()),
                Ok(false) => {}
                Err(CanonicalError::File(FileError::Io(e)))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    waited += 1
                }
                Err(e) => break Err(e.to_string()),
            }
        };
        (verdict, waited)
    }

    #[test]
    fn a_check_whose_reads_each_wait_once_compares_every_byte_and_reaches_the_plain_verdict() {
        let (canonical, id) = aside_then_ninths(Vec::new());
        let (bodies, _) = plain_xor_bodies(&canonical, &id);
        let (own, _) = aside_then_ninths(bodies);
        let mut damaged = own.clone();
        let mid = damaged.len() / 2;
        damaged[mid] ^= 1;
        let info = |_| SeriesInfo {
            name: Some("s".into()),
            reach: Reach::Point,
        };
        let (reaching, _) = aside_then_ninths_reaching(Vec::new(), Reach::Fixed(1));
        for (bytes, most_wait) in [
            (&canonical, true),
            (&own, true),
            (&damaged, true),
            (&reaching, false),
        ] {
            let plain = check_canonical(bytes, &id, &info).map_err(|e| e.to_string());
            let (shy, waited) = shy_check(bytes, &id);
            assert_eq!(shy, plain);
            assert!(
                waited > if most_wait { 60 } else { 0 },
                "{waited} steps waited"
            );
        }
        assert!(check_canonical(&damaged, &id, &info).is_err());
        CompactedFile::open(reaching.as_slice(), &id, None).unwrap();
        match check_canonical(&reaching, &id, &info) {
            Err(CanonicalError::Bytes(at)) => assert!(at < reaching.len() as u64, "{at}"),
            r => panic!("{r:?}"),
        }
    }
}
