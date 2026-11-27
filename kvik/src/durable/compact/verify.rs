//! Verification of a run's outputs (§3.2), a step at a time: each output read back from the device through the
//! stepped canonical check, then a data block a step for counts, per-series checksums and placement, then hashed in
//! 128 KiB pieces for its description. It holds 24 bytes a series, one file's index (twice while the canonical
//! check rebuilds it) and a block of each of two files.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use kvik_chunk::file::{
    CanonicalCheck, CanonicalError, Class, CompactedFile, Directory, Fact, FileError, Identity,
    IndexEntry, Input, Layout, Position, ReadAt, SeriesInfo,
};
use kvik_log::fs::{FsFile, OpenMode};
use sha2::{Digest, Sha256};

use super::run::{RunInput, rank};
use super::segment::PIECE;
use super::{Output, entry_digest, fact_digest};

/// The input side of the counts, from what the sealer wrote: trailers' `entry_count` and the segment indexes'
/// per-series `fact_count`, less positions met twice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Anchors {
    pub(crate) positions: u64,
    pub(crate) facts: BTreeMap<u32, u64>,
}

/// What the inputs promise the outputs hold: the sealer's counts and the digests taken as the inputs were read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Expect {
    pub(super) anchors: Anchors,
    pub(super) sums: BTreeMap<u32, u128>,
    pub(super) entries: u128,
    /// The inputs every description names.
    pub(super) names: Vec<Input>,
}

/// An output as written, before it is read back.
#[derive(Clone, Debug)]
pub(super) struct Written {
    pub(super) id: Identity,
    pub(super) layout: Layout,
    pub(super) len: u64,
    pub(super) path: PathBuf,
    pub(super) period: (i64, i64),
    pub(super) class: Class,
    /// Its greatest position, which names its series (Q-1412).
    pub(super) greatest: Position,
}

/// An output that fails a check of §3.2.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifyError {
    /// It could not be read back.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The positional reader refused it.
    #[error(transparent)]
    Read(#[from] FileError),
    /// Its bytes are not the canonical ones for its content (step 4).
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
    /// Rows or entries differ in number from the sealer's counts (step 1).
    #[error("counts differ from the sealer's")]
    Counts,
    /// A per-series checksum, or the entries' checksum when `None`, differs (step 2).
    #[error("checksum of series {0:?} differs")]
    Checksum(Option<u32>),
    /// A row is out of place (step 3).
    #[error("misplaced: {0}")]
    Placement(&'static str),
    /// Its description names other inputs, run or run size than the run's (§3.5 rebuild rule).
    #[error("description disagrees with the run: {0}")]
    Description(&'static str),
}

/// A bit for each position of the inputs' ranges, so a position held by two outputs is found (step 3).
struct Held {
    spans: Vec<(Position, Position, usize)>,
    bits: Vec<u64>,
}

/// The most positions the bits cover; inputs whose ranges are unknown or wider are checked by counts and digests.
const MAX_HELD: u64 = 1 << 28;

impl Held {
    fn new(ranges: &[(Position, Position)]) -> Option<Self> {
        let mut r = ranges.to_vec();
        r.sort();
        let mut spans: Vec<(Position, Position, usize)> = Vec::new();
        let mut total = 0u64;
        for (a, z) in r {
            match spans.last_mut() {
                Some(s) if s.0.0 == a.0 && a.1 <= s.1.1.saturating_add(1) => s.1 = s.1.max(z),
                _ => spans.push((a, z, 0)),
            }
        }
        for s in &mut spans {
            s.2 = usize::try_from(total).ok()?;
            total = total.checked_add(s.1.1.checked_sub(s.0.1)?.checked_add(1)?)?;
            if total > MAX_HELD {
                return None;
            }
        }
        Some(Self {
            spans,
            bits: vec![0; usize::try_from(total.div_ceil(64)).ok()?],
        })
    }

    fn hold(&mut self, p: Position) -> Result<(), VerifyError> {
        let s = self
            .spans
            .iter()
            .find(|s| s.0 <= p && p <= s.1)
            .ok_or(VerifyError::Placement("a position outside the inputs"))?;
        let i = s.2 + (p.1 - s.0.1) as usize;
        let (w, b) = (i / 64, 1u64 << (i % 64));
        if self.bits[w] & b != 0 {
            return Err(VerifyError::Placement("a position held twice"));
        }
        self.bits[w] |= b;
        Ok(())
    }
}

/// An output read back by offset.
#[derive(Clone, Debug)]
pub(super) struct Src {
    file: Arc<dyn FsFile>,
    len: u64,
}

impl Src {
    fn open(input: &RunInput<'_>, w: &Written) -> io::Result<Self> {
        let file: Arc<dyn FsFile> = input.fs.open(&w.path, OpenMode::Read)?.into();
        let len = file.len()?;
        Ok(Self { file, len })
    }
}

impl ReadAt for Src {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = self.file.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(buf)
    }
}

/// The registry is lent to each step (`CanonicalCheck::step_with`), so the check holds no name of a series.
type Canon = CanonicalCheck<Src, fn(u32) -> SeriesInfo>;

const PLACED: VerifyError = VerifyError::Placement("a revision row not outranked in the run");

/// The current chunks of one series in a file, read a block at a time.
struct Cursor {
    series: u32,
    file: CompactedFile<Src>,
    chunks: VecDeque<IndexEntry>,
    dir: Option<(Directory, usize)>,
    buf: VecDeque<Fact>,
}

impl Cursor {
    fn new(file: CompactedFile<Src>, series: u32) -> Result<Self, VerifyError> {
        let mut chunks: Vec<IndexEntry> = file
            .series(series)?
            .into_iter()
            .filter(|e| !e.set_aside())
            .collect();
        chunks.sort_by_key(|e| e.chunk_offset);
        Ok(Self {
            series,
            file,
            chunks: chunks.into(),
            dir: None,
            buf: VecDeque::new(),
        })
    }

    /// Whether the row of `f`'s key here outranks `f`; calls come in `(t, position)` order. A revision block's step
    /// reads the current blocks it spans.
    fn outranks(&mut self, f: &Fact) -> Result<bool, VerifyError> {
        loop {
            while self.buf.front().is_some_and(|c| c.t < f.t) {
                self.buf.pop_front();
            }
            if let Some(c) = self.buf.front() {
                return Ok(c.t == f.t && rank(c) > rank(f));
            }
            let Some(e) = self.chunks.front().copied() else {
                return Ok(false);
            };
            if self.dir.is_none() {
                self.dir = Some((self.file.directory(&e)?, 0));
            }
            let (d, i) = self.dir.as_mut().expect("set above");
            let rows = self.file.read_block(&e, d, *i)?;
            self.buf.extend(rows.into_iter().map(|r| r.fact));
            *i += 1;
            if *i == d.blocks.len() {
                self.chunks.pop_front();
                self.dir = None;
            }
        }
    }
}

enum Phase {
    Open,
    Canon(Box<Canon>),
    Content {
        file: Box<CompactedFile<Src>>,
        order: Vec<IndexEntry>,
        at: (usize, usize),
        dir: Option<Directory>,
        current: Option<Box<Cursor>>,
    },
    Hash {
        src: Src,
        at: u64,
        hash: Box<Sha256>,
    },
}

/// The verification of a run's outputs, stepped.
pub(super) struct Check {
    files: Vec<Written>,
    held: Option<Held>,
    j: usize,
    phase: Phase,
    positions: u64,
    entries: u128,
    out: Vec<Output>,
}

impl Check {
    /// Checks `files`, the outputs of a run over inputs with these position ranges.
    pub(super) fn new(files: Vec<Written>, ranges: &[(Position, Position)]) -> Self {
        Self {
            files,
            held: Held::new(ranges),
            j: 0,
            phase: Phase::Open,
            positions: 0,
            entries: 0,
            out: Vec::new(),
        }
    }

    /// The outputs it checks.
    pub(super) fn files(&self) -> &[Written] {
        &self.files
    }

    /// Bytes it holds: the open file's index, the position bits and a block of each of two files; the sums and counts
    /// it takes from are the census's.
    pub(super) fn held(&self) -> u64 {
        let index = match &self.phase {
            Phase::Content { order, .. } => {
                (order.capacity() * std::mem::size_of::<IndexEntry>()) as u64
            }
            Phase::Canon(c) => c.held_bytes(),
            _ => 0,
        };
        let bits = self
            .held
            .as_ref()
            .map_or(0, |h| 8 * h.bits.len() + 32 * h.spans.len());
        index + bits as u64 + 2 * PIECE as u64
    }

    /// The size of the next step's device call.
    pub(super) fn next_bytes(&self) -> u64 {
        match &self.phase {
            Phase::Hash { src, at, .. } => (src.len - at).min(PIECE as u64),
            _ => PIECE as u64,
        }
    }

    /// One step; the verified outputs once every check has passed. Each row is taken off `expect`'s sums and counts,
    /// which end at zero when the outputs hold what the inputs promised.
    pub(super) fn step(
        &mut self,
        input: &RunInput<'_>,
        expect: &mut Expect,
        infos: &dyn Fn(u32, Position) -> SeriesInfo,
    ) -> Result<Option<Vec<Output>>, VerifyError> {
        let Some(w) = self.files.get(self.j).cloned() else {
            return self.totals(expect).map(Some);
        };
        let within = |t: i64| {
            let (s, l) = w.period;
            (i128::from(s)..i128::from(s) + i128::from(l)).contains(&i128::from(t))
        };
        match &mut self.phase {
            Phase::Open => {
                let src = Src::open(input, &w)?;
                let file = CompactedFile::open(src.clone(), &w.id, Some(&w.layout))?;
                let d = file.description();
                if d.inputs != expect.names {
                    return Err(VerifyError::Description("inputs"));
                }
                if d.run_id != input.first_file_id {
                    return Err(VerifyError::Description("run id"));
                }
                if d.run_count as usize != self.files.len() {
                    return Err(VerifyError::Description("run count"));
                }
                drop(file);
                let lent: fn(u32) -> SeriesInfo = |_| unreachable!("lent to each step");
                self.phase = Phase::Canon(Box::new(CanonicalCheck::new(src, &w.id, lent)?));
            }
            Phase::Canon(c) => {
                if c.step_with(&|s| infos(s, w.greatest))? {
                    let Phase::Canon(c) = std::mem::replace(&mut self.phase, Phase::Open) else {
                        unreachable!("in the canonical phase")
                    };
                    let order = c.into_order();
                    let file = CompactedFile::open(Src::open(input, &w)?, &w.id, Some(&w.layout))?;
                    self.phase = Phase::Content {
                        file: Box::new(file),
                        order,
                        at: (0, 0),
                        dir: None,
                        current: None,
                    };
                }
            }
            Phase::Content {
                file,
                order,
                at,
                dir,
                current,
            } => {
                let Some(e) = order.get(at.0).copied() else {
                    for row in file.entries()? {
                        if !within(row.commit_time) {
                            return Err(VerifyError::Placement(
                                "an entry outside its file's period",
                            ));
                        }
                        let d = entry_digest(
                            row.position,
                            &row.entry,
                            row.received_at,
                            row.commit_time,
                        )
                        .map_err(|_| VerifyError::Checksum(None))?;
                        self.entries = self.entries.wrapping_add(d);
                        if let Some(h) = &mut self.held {
                            h.hold(row.position)?;
                        }
                        self.positions += 1;
                    }
                    if !file.ignored().is_empty() {
                        return Err(VerifyError::Placement(
                            "an element the writer does not write",
                        ));
                    }
                    self.phase = Phase::Hash {
                        src: file.source().clone(),
                        at: 0,
                        hash: Box::default(),
                    };
                    return Ok(None);
                };
                if dir.is_none() {
                    *dir = Some(file.directory(&e)?);
                }
                let d = dir.as_ref().expect("read above");
                let rows = file.read_block(&e, d, at.1)?;
                at.1 += 1;
                if at.1 == d.blocks.len() {
                    *at = (at.0 + 1, 0);
                    *dir = None;
                }
                for r in rows {
                    let f = &r.fact;
                    let p = f.position;
                    if !within(f.t) {
                        return Err(VerifyError::Placement("a row outside its file's period"));
                    }
                    if let Some(h) = &mut self.held {
                        h.hold(p)?;
                    }
                    if e.set_aside() != input.set_aside.holds(p) {
                        return Err(VerifyError::Placement("a set-aside row in the wrong chunk"));
                    }
                    if !e.set_aside() && w.class == Class::Revision {
                        if current.as_ref().is_none_or(|c| c.series != f.series_id) {
                            let top = self
                                .files
                                .iter()
                                .find(|o| o.period == w.period && o.class == Class::Current)
                                .ok_or(PLACED)?;
                            let cf = CompactedFile::open(
                                Src::open(input, top)?,
                                &top.id,
                                Some(&top.layout),
                            )?;
                            *current = Some(Box::new(Cursor::new(cf, f.series_id)?));
                        }
                        if !current.as_mut().expect("opened above").outranks(f)? {
                            return Err(PLACED);
                        }
                    }
                    self.positions += 1;
                    let n = expect.anchors.facts.entry(f.series_id).or_default();
                    *n = n.wrapping_sub(1);
                    let s = expect.sums.entry(f.series_id).or_default();
                    *s = s.wrapping_sub(fact_digest(f, r.received_at, r.commit_time));
                }
            }
            Phase::Hash { src, at, hash } => {
                let n = (src.len - *at).min(PIECE as u64) as usize;
                if n > 0 {
                    hash.update(src.read_at(*at, n)?);
                    *at += n as u64;
                    return Ok(None);
                }
                let sha256 = std::mem::take(&mut **hash).finalize().into();
                self.out.push(Output {
                    id: w.id,
                    layout: w.layout.clone(),
                    len: w.len,
                    sha256,
                    path: w.path.clone(),
                });
                self.j += 1;
                self.phase = Phase::Open;
            }
        }
        Ok(None)
    }

    fn totals(&mut self, expect: &Expect) -> Result<Vec<Output>, VerifyError> {
        if self.positions != expect.anchors.positions
            || expect.anchors.facts.values().any(|&n| n != 0)
        {
            return Err(VerifyError::Counts);
        }
        if let Some((s, _)) = expect.sums.iter().find(|(_, sum)| **sum != 0) {
            return Err(VerifyError::Checksum(Some(*s)));
        }
        if self.entries != expect.entries {
            return Err(VerifyError::Checksum(None));
        }
        Ok(std::mem::take(&mut self.out))
    }
}
