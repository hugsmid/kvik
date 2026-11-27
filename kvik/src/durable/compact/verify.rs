//! Verification of a run's outputs (§3.2), a step at a time: each output read back from the device through the
//! stepped canonical check, then a data block a step for counts, per-series checksums and placement, then hashed in
//! 128 KiB pieces for its description. It holds 24 bytes a series, one file's index (twice while the canonical
//! check rebuilds it) and a block of each of two files.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kvik_chunk::file::{
    CanonicalCheck, CanonicalError, Class, CompactedFile, Directory, Fact, FileError, Identity,
    IndexEntry, Input, Layout, Position, ReadAt, SeriesInfo, TimedFact,
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

    /// The word and bit of `p`, if it is not held yet.
    fn free(&self, p: Position) -> Result<(usize, u64), VerifyError> {
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
        Ok((w, b))
    }

    fn hold(&mut self, p: Position) -> Result<(), VerifyError> {
        let (w, b) = self.free(p)?;
        self.bits[w] |= b;
        Ok(())
    }
}

/// Bytes of a file from an offset.
type Window = (u64, Vec<u8>);

/// Windows a source keeps between steps.
const KEPT: usize = 2;

/// The device call this step declared (`None` for CPU only, taken once it is made), and the read that last waited
/// for the next step to declare it; shared by a check's sources (compaction design §4, Q-1990).
#[derive(Debug, Default)]
struct Allowance {
    left: Mutex<Option<u64>>,
    wanted: Mutex<Option<u64>>,
}

type Budget = Arc<Allowance>;

fn would_block() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}

impl Allowance {
    /// Takes the step's call for a read of `n` bytes, or, when none is left or it is declared smaller, records `n`
    /// for the next step to declare.
    fn take(&self, n: u64) -> io::Result<()> {
        let mut left = self.left.lock().expect("not poisoned");
        if left.is_some_and(|l| n <= l) {
            *left = None;
            return Ok(());
        }
        *self.wanted.lock().expect("not poisoned") = Some(n);
        Err(would_block())
    }
}

/// An output read back through windows of at least 128 KiB, shared by its clones (the reader and the canonical
/// check's comparing sink). A read the windows do not cover takes the step's device call, or fails with
/// `WouldBlock` once it is spent; a step that fails so is taken again, finding what it read in the windows, which
/// are trimmed to the last [`KEPT`] only after a step that completes.
#[derive(Clone, Debug)]
pub(super) struct Src {
    file: Arc<dyn FsFile>,
    len: u64,
    budget: Budget,
    /// Most recently used first.
    windows: Arc<Mutex<Vec<Window>>>,
}

impl Src {
    fn open(input: &RunInput<'_>, w: &Written, budget: &Budget) -> io::Result<Self> {
        budget.take(0)?;
        let file: Arc<dyn FsFile> = input.fs.open(&w.path, OpenMode::Read)?.into();
        let len = file.len()?;
        Ok(Self {
            file,
            len,
            budget: budget.clone(),
            windows: Arc::default(),
        })
    }

    /// Bytes its windows hold.
    fn held(&self) -> u64 {
        let w = self.windows.lock().expect("not poisoned");
        w.iter().map(|(_, b)| b.capacity() as u64).sum()
    }

    fn settle(&self) {
        self.windows.lock().expect("not poisoned").truncate(KEPT);
    }
}

impl ReadAt for Src {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .filter(|&e| e <= self.len)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        let mut w = self.windows.lock().expect("not poisoned");
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        while at < end {
            if let Some(i) = w
                .iter()
                .position(|(s, b)| *s <= at && at < s + b.len() as u64)
            {
                let (s, b) = &w[i];
                let to = end.min(s + b.len() as u64);
                out.extend_from_slice(&b[(at - s) as usize..(to - s) as usize]);
                at = to;
                let used = w.remove(i);
                w.insert(0, used);
                continue;
            }
            // A read near the end takes the file's last window, which holds its tail, index and commit times.
            let n = (end - at).max(PIECE as u64).min(self.len);
            self.budget.take(n)?;
            let from = at.min(self.len - n);
            let buf = read_whole(&*self.file, from, n as usize)?;
            out.extend_from_slice(&buf[(at - from) as usize..(end - from) as usize]);
            at = end;
            w.insert(0, (from, buf));
        }
        Ok(out)
    }
}

fn read_whole(file: &dyn FsFile, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let got = file.read_at(&mut buf[done..], offset + done as u64)?;
        if got == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += got;
    }
    Ok(buf)
}

/// The registry is lent to each step (`CanonicalCheck::step_with`), so the check holds no name of a series.
type Canon = CanonicalCheck<Src, fn(u32) -> SeriesInfo>;

const PLACED: VerifyError = VerifyError::Placement("a revision row not outranked in the run");

/// Whether a step stopped at a read that must wait for the next step's device call.
fn blocked(e: &VerifyError) -> bool {
    let io = match e {
        VerifyError::Io(e)
        | VerifyError::Read(FileError::Io(e))
        | VerifyError::Canonical(CanonicalError::File(FileError::Io(e))) => e,
        _ => return false,
    };
    io.kind() == io::ErrorKind::WouldBlock
}

/// The current chunks of one series in a file, read a block at a time.
struct Cursor {
    series: u32,
    file: CompactedFile<Src>,
    chunks: VecDeque<IndexEntry>,
    dir: Option<Directory>,
    block: usize,
    buf: VecDeque<Fact>,
}

impl Cursor {
    fn new(file: CompactedFile<Src>) -> Self {
        Self {
            series: u32::MAX,
            file,
            chunks: VecDeque::new(),
            dir: None,
            block: 0,
            buf: VecDeque::new(),
        }
    }

    /// Turns to `series`, reading its index entries.
    fn turn(&mut self, series: u32) -> Result<(), VerifyError> {
        let mut chunks: Vec<IndexEntry> = self
            .file
            .series(series)?
            .into_iter()
            .filter(|e| !e.set_aside())
            .collect();
        chunks.sort_by_key(|e| e.chunk_offset);
        self.series = series;
        self.chunks = chunks.into();
        self.dir = None;
        self.block = 0;
        self.buf.clear();
        Ok(())
    }

    /// Whether the row of `f`'s key here outranks `f`, or `None` until its next block is read; calls come in
    /// `(t, position)` order.
    fn outranks(&mut self, f: &Fact) -> Option<bool> {
        while self.buf.front().is_some_and(|c| c.t < f.t) {
            self.buf.pop_front();
        }
        match self.buf.front() {
            Some(c) => Some(c.t == f.t && rank(c) > rank(f)),
            None if self.chunks.is_empty() => Some(false),
            None => None,
        }
    }

    /// Reads its next directory or block.
    fn read(&mut self) -> Result<(), VerifyError> {
        let e = *self.chunks.front().expect("a block to read");
        let Some(d) = &self.dir else {
            self.dir = Some(self.file.directory(&e)?);
            return Ok(());
        };
        let rows = self.file.read_block(&e, d, self.block)?;
        self.buf.extend(rows.into_iter().map(|r| r.fact));
        self.block += 1;
        if self.block == d.blocks.len() {
            self.chunks.pop_front();
            self.dir = None;
            self.block = 0;
        }
        Ok(())
    }
}

enum Phase {
    Open,
    Opening(Src),
    Canon(Box<Canon>),
    Content {
        file: Box<CompactedFile<Src>>,
        order: Vec<IndexEntry>,
        at: (usize, usize),
        dir: Option<Box<Directory>>,
        /// The rows of the block read last, not yet checked, and its index entry.
        rows: VecDeque<TimedFact>,
        entry: Option<Box<IndexEntry>>,
        /// The current file of the period, for a revision file's rows: its source, then a cursor over it.
        top: Option<Src>,
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
    budget: Budget,
    /// The open files' sources, for their windows' bytes.
    reading: Vec<Src>,
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
            budget: Budget::default(),
            reading: Vec::new(),
            positions: 0,
            entries: 0,
            out: Vec::new(),
        }
    }

    /// The outputs it checks.
    pub(super) fn files(&self) -> &[Written] {
        &self.files
    }

    /// Bytes it holds: the open file's index, the position bits, the sources' windows and a block of each of two
    /// files; the sums and counts it takes from are the census's.
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
        let windows: u64 = self.reading.iter().map(Src::held).sum();
        index + bits as u64 + windows + 2 * PIECE as u64
    }

    /// The next step's device call, `None` for CPU only: a step reads from its windows until a read waits, and the
    /// next step declares that read.
    pub(super) fn next_bytes(&self) -> Option<u64> {
        match &self.phase {
            Phase::Hash { src, .. } => Some(src.len.min(PIECE as u64)),
            _ => *self.budget.wanted.lock().expect("not poisoned"),
        }
    }

    /// One step, at most one device call; the verified outputs once every check has passed. Each row is taken off
    /// `expect`'s sums and counts, which end at zero when the outputs hold what the inputs promised.
    pub(super) fn step(
        &mut self,
        input: &RunInput<'_>,
        expect: &mut Expect,
        infos: &dyn Fn(u32, Position) -> SeriesInfo,
    ) -> Result<Option<Vec<Output>>, VerifyError> {
        *self.budget.left.lock().expect("not poisoned") = self.next_bytes();
        *self.budget.wanted.lock().expect("not poisoned") = None;
        match self.step_once(input, expect, infos) {
            Err(e) if blocked(&e) => Ok(None),
            r => {
                for s in &self.reading {
                    s.settle();
                }
                r
            }
        }
    }

    /// A step that reads before it changes anything, so that one stopped by `WouldBlock` is taken again.
    fn step_once(
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
                let src = Src::open(input, &w, &self.budget)?;
                self.reading = vec![src.clone()];
                self.phase = Phase::Opening(src);
            }
            Phase::Opening(src) => {
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
                let lent: fn(u32) -> SeriesInfo = |_| unreachable!("lent to each step");
                self.phase = Phase::Canon(Box::new(CanonicalCheck::from_file(file, lent)));
            }
            Phase::Canon(c) => {
                if c.step_with(&|s| infos(s, w.greatest))? {
                    let Phase::Canon(c) = std::mem::replace(&mut self.phase, Phase::Open) else {
                        unreachable!("in the canonical phase")
                    };
                    let (file, order) = c.into_parts();
                    self.phase = Phase::Content {
                        file: Box::new(file),
                        order,
                        at: (0, 0),
                        dir: None,
                        rows: VecDeque::new(),
                        entry: None,
                        top: None,
                        current: None,
                    };
                }
            }
            Phase::Content {
                file,
                order,
                at,
                dir,
                rows,
                entry,
                top,
                current,
            } => {
                if rows.is_empty() {
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
                        *dir = Some(Box::new(file.directory(&e)?));
                    }
                    let d = dir.as_ref().expect("read above");
                    let n = d.blocks.len();
                    *rows = file.read_block(&e, d, at.1)?.into();
                    *entry = Some(Box::new(e));
                    at.1 += 1;
                    if at.1 == n {
                        *at = (at.0 + 1, 0);
                        *dir = None;
                    }
                }
                let e = **entry.as_ref().expect("set with its rows");
                while let Some(r) = rows.front() {
                    let f = &r.fact;
                    let p = f.position;
                    if !within(f.t) {
                        return Err(VerifyError::Placement("a row outside its file's period"));
                    }
                    if let Some(h) = &self.held {
                        h.free(p)?;
                    }
                    if e.set_aside() != input.set_aside.holds(p) {
                        return Err(VerifyError::Placement("a set-aside row in the wrong chunk"));
                    }
                    if !e.set_aside() && w.class == Class::Revision {
                        let Some(c) = current.as_mut().filter(|c| c.series == f.series_id) else {
                            if let Some(c) = current.as_mut() {
                                c.turn(f.series_id)?;
                                continue;
                            }
                            let t = self
                                .files
                                .iter()
                                .find(|o| o.period == w.period && o.class == Class::Current)
                                .ok_or(PLACED)?;
                            let Some(src) = top.as_ref() else {
                                let src = Src::open(input, t, &self.budget)?;
                                self.reading.push(src.clone());
                                *top = Some(src);
                                continue;
                            };
                            let cf = CompactedFile::open(src.clone(), &t.id, Some(&t.layout))?;
                            *current = Some(Box::new(Cursor::new(cf)));
                            continue;
                        };
                        match c.outranks(f) {
                            Some(true) => {}
                            Some(false) => return Err(PLACED),
                            None => {
                                c.read()?;
                                continue;
                            }
                        }
                    }
                    if let Some(h) = &mut self.held {
                        h.hold(p)?;
                    }
                    self.positions += 1;
                    let n = expect.anchors.facts.entry(f.series_id).or_default();
                    *n = n.wrapping_sub(1);
                    let s = expect.sums.entry(f.series_id).or_default();
                    *s = s.wrapping_sub(fact_digest(f, r.received_at, r.commit_time));
                    rows.pop_front();
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
