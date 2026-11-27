//! Verification of a run's outputs (§3.2), a step at a time: each output read back from the device through the
//! stepped canonical check, then a data block a step for counts, per-series checksums and placement, then hashed in
//! 128 KiB pieces for its description. It holds 24 bytes a series, one file's index and a block of each of two
//! files.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use kvik_chunk::file::{
    CanonicalCheck, CanonicalError, Class, CompactedFile, Directory, Fact, FileError, Identity,
    IndexEntry, Layout, Position, Reach, ReadAt, SeriesInfo,
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

type Infos = Box<dyn Fn(u32) -> SeriesInfo + Send>;

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
    Canon(Box<CanonicalCheck<Src, Infos>>),
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
    j: usize,
    phase: Phase,
    sums: BTreeMap<u32, u128>,
    facts: BTreeMap<u32, u64>,
    positions: u64,
    entries: u128,
    out: Vec<Output>,
}

impl Check {
    pub(super) fn new(files: Vec<Written>) -> Self {
        Self {
            files,
            j: 0,
            phase: Phase::Open,
            sums: BTreeMap::new(),
            facts: BTreeMap::new(),
            positions: 0,
            entries: 0,
            out: Vec::new(),
        }
    }

    /// The outputs it checks.
    pub(super) fn files(&self) -> &[Written] {
        &self.files
    }

    /// Bytes it holds: its sums and counts by series, the open file's index and a block of each of two files.
    pub(super) fn held(&self) -> u64 {
        let index = match &self.phase {
            Phase::Content { order, .. } => order.len() as u64 * 96,
            _ => 0,
        };
        64 * self.sums.len() as u64 + index + 2 * PIECE as u64
    }

    /// The size of the next step's device call.
    pub(super) fn next_bytes(&self) -> u64 {
        match &self.phase {
            Phase::Hash { src, at, .. } => (src.len - at).min(PIECE as u64),
            _ => PIECE as u64,
        }
    }

    /// One step; the verified outputs once every check has passed.
    pub(super) fn step(
        &mut self,
        input: &RunInput<'_>,
        expect: &Expect,
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
                let mut named: BTreeMap<u32, SeriesInfo> = BTreeMap::new();
                for e in file.index()? {
                    named
                        .entry(e.series_id)
                        .or_insert_with(|| infos(e.series_id, w.greatest));
                }
                let registry: Infos = Box::new(move |s| {
                    named.get(&s).cloned().unwrap_or(SeriesInfo {
                        name: None,
                        reach: Reach::Point,
                    })
                });
                self.phase = Phase::Canon(Box::new(CanonicalCheck::new(src, &w.id, registry)?));
            }
            Phase::Canon(c) => {
                if c.step()? {
                    let file = CompactedFile::open(Src::open(input, &w)?, &w.id, Some(&w.layout))?;
                    let mut order = file.index()?;
                    order.sort_by_key(|e| e.chunk_offset);
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
                    if e.set_aside() != input.set_aside.holds(p)
                        || (e.set_aside() && w.class != Class::Revision)
                    {
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
                    *self.facts.entry(f.series_id).or_default() += 1;
                    let s = self.sums.entry(f.series_id).or_default();
                    *s = s.wrapping_add(fact_digest(f, r.received_at, r.commit_time));
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
        if self.positions != expect.anchors.positions || self.facts != expect.anchors.facts {
            return Err(VerifyError::Counts);
        }
        for (s, sum) in &expect.sums {
            if self.sums.get(s) != Some(sum) {
                return Err(VerifyError::Checksum(Some(*s)));
            }
        }
        if self.entries != expect.entries {
            return Err(VerifyError::Checksum(None));
        }
        Ok(std::mem::take(&mut self.out))
    }
}
