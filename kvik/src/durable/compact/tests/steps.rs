use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kvik_log::facts::Value;
use kvik_log::fs::{DirEntry, Fs, FsFile, OpenMode, StdFs};
use kvik_log::setaside::SetAside;
use kvik_log::writer::share::{Background, Next};

use super::super::run::{DATA_DIR, MINIMAL_MEMORY, RunInput};
use super::super::segment::PIECE;
use super::super::task::RunTask;
use super::{DAY, STORE, TestBatch, batch, put_segment, reading, segment_bytes, temp_dir};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Open(PathBuf),
    CreateDir(PathBuf),
    ReadDir(PathBuf),
    Rename(PathBuf),
    Remove(PathBuf),
    SyncDir(PathBuf),
    Read(PathBuf, usize),
    Write(PathBuf, usize),
    Sync(PathBuf),
    SetLen(PathBuf),
}

impl Call {
    fn bytes(&self) -> u64 {
        match self {
            Call::Read(_, n) | Call::Write(_, n) => *n as u64,
            _ => 0,
        }
    }
}

type Log = Arc<Mutex<Vec<Call>>>;

/// A file system that records every blocking call; a file's `len` is its metadata, taken with its open.
#[derive(Debug, Default)]
struct Counting {
    log: Log,
}

#[derive(Debug)]
struct CountingFile {
    inner: Box<dyn FsFile>,
    path: PathBuf,
    log: Log,
}

impl Counting {
    fn push(&self, c: Call) {
        self.log.lock().unwrap().push(c);
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

impl Fs for Counting {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        self.push(Call::Open(path.into()));
        Ok(Box::new(CountingFile {
            inner: StdFs.open(path, mode)?,
            path: path.into(),
            log: self.log.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.push(Call::CreateDir(path.into()));
        StdFs.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.push(Call::ReadDir(path.into()));
        StdFs.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.push(Call::Rename(from.into()));
        StdFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.push(Call::Remove(path.into()));
        StdFs.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.push(Call::SyncDir(path.into()));
        StdFs.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        StdFs.free_space(path)
    }
}

impl CountingFile {
    fn push(&self, c: Call) {
        self.log.lock().unwrap().push(c);
    }
}

impl FsFile for CountingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.push(Call::Read(self.path.clone(), buf.len()));
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.push(Call::Write(self.path.clone(), buf.len()));
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.push(Call::Sync(self.path.clone()));
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.push(Call::Sync(self.path.clone()));
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.push(Call::SetLen(self.path.clone()));
        self.inner.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
}

/// Two days of 200-byte texts, the second day's keys written again so that it has a revision file, over more than
/// one 1 MiB sync interval a file.
fn shape() -> Vec<(u32, Vec<TestBatch>)> {
    let text = |k: u64| Value::Text(format!("{k:03}").repeat(67));
    let mut batches: Vec<TestBatch> = (0..240)
        .map(|i| {
            let day = 28 + (i % 2) as i64;
            let items = (0..50)
                .map(|k| reading(k as u32, day * DAY + i as i64 * 1_000, text(k)))
                .collect();
            batch(1 + 50 * i, 30 * DAY, items)
        })
        .collect();
    let at = 1 + 50 * batches.len() as u64;
    batches.push(batch(
        at,
        30 * DAY,
        (0..50)
            .map(|k| reading(k as u32, 29 * DAY + 1_000, Value::F64(k)))
            .collect(),
    ));
    batches.chunks(100).map(|b| (0, b.to_vec())).collect()
}

struct Trace {
    /// `(stage, declared, calls)` a step.
    steps: Vec<(&'static str, Next, Vec<Call>)>,
    data: PathBuf,
}

fn trace() -> Trace {
    let dir: &'static Path = Box::leak(temp_dir("counted").into_boxed_path());
    let set_aside: &'static SetAside = Box::leak(Box::new(SetAside::new(vec![])));
    let weeks: &'static BTreeSet<i64> = Box::leak(Box::new(BTreeSet::new()));
    let fs: &'static Counting = Box::leak(Box::default());
    let cands = shape()
        .iter()
        .enumerate()
        .map(|(i, (g, b))| {
            let f = i as u64 + 1;
            put_segment(dir, *g, f, &segment_bytes(*g, f, b))
        })
        .collect();
    let input = RunInput {
        fs,
        dir,
        store_id: STORE,
        generation: 1,
        first_file_id: 100,
        created_at: 5,
        set_aside,
        weeks_merged: weeks,
        registry: &|_| None,
    };
    let mut task = RunTask::new(input, cands, MINIMAL_MEMORY);
    let mut steps = Vec::new();
    loop {
        let declared = task.next();
        if matches!(declared, Next::Idle) {
            break;
        }
        let stage = task.stage();
        fs.take();
        task.step();
        steps.push((stage, declared, fs.take()));
    }
    assert!(task.outcome().unwrap().is_ok());
    let data = dir.join(DATA_DIR);
    std::fs::remove_dir_all(dir).unwrap();
    Trace { steps, data }
}

/// The most calls a step makes in each stage, and the steps that break the bar outside verification.
fn tally(t: &Trace) -> (BTreeMap<&'static str, usize>, Vec<String>) {
    let mut most: BTreeMap<&str, usize> = BTreeMap::new();
    let mut bad = Vec::new();
    for (i, (stage, declared, calls)) in t.steps.iter().enumerate() {
        let m = most.entry(stage).or_default();
        *m = (*m).max(calls.len());
        let bytes: u64 = calls.iter().map(Call::bytes).sum();
        let ok = match declared {
            Next::Device(n) => calls.len() <= 1 && bytes <= *n,
            _ => calls.is_empty(),
        };
        if !ok && *stage != "verify" && bad.len() < 12 {
            bad.push(format!("step {i} {stage} {declared:?}: {calls:?}"));
        }
    }
    (most, bad)
}

#[test]
fn each_step_of_the_census_and_passes_makes_at_most_one_device_call_of_at_most_its_declared_bytes()
{
    let (most, bad) = tally(&trace());
    assert!(
        bad.is_empty(),
        "most calls a step: {most:?}\n{}",
        bad.join("\n")
    );
}

/// Q-1990: verification still makes up to three calls in a step (an output's open, header and tail; a window
/// crossed by the reader and the comparing sink together; a revision block and the current file's).
#[test]
fn each_verification_step_makes_at_most_three_device_calls() {
    let (most, _) = tally(&trace());
    assert!(most["verify"] <= 3, "most calls a step: {most:?}");
}

#[test]
fn outputs_are_synced_every_mebibyte_at_their_end_and_the_directory_before_verification() {
    let t = trace();
    let calls: Vec<&Call> = t.steps.iter().flat_map(|s| &s.2).collect();
    let is_output = |p: &Path| p.starts_with(&t.data);
    let mut unsynced: BTreeMap<&Path, u64> = BTreeMap::new();
    let mut last_write: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut synced_at: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut dir_sync = None;
    let mut first_read = None;
    for (i, c) in calls.iter().enumerate() {
        match c {
            Call::Write(p, n) if is_output(p) => {
                assert!(*n <= PIECE, "a write of {n} bytes");
                let u = unsynced.entry(p).or_default();
                assert!(*u < 1 << 20, "{} bytes written to {p:?} since its sync", *u);
                *u += *n as u64;
                last_write.insert(p, i);
            }
            Call::Sync(p) if is_output(p) => {
                unsynced.insert(p, 0);
                synced_at.insert(p, i);
            }
            Call::SyncDir(p) if *p == t.data => dir_sync = Some(i),
            Call::Read(p, _) if is_output(p) => {
                first_read.get_or_insert(i);
            }
            _ => {}
        }
    }
    assert!(last_write.len() >= 3, "{} outputs", last_write.len());
    assert!(
        last_write.keys().all(|p| synced_at[p] > last_write[p]),
        "an output not synced after its last write"
    );
    let dir_sync = dir_sync.expect("the data directory is synced");
    assert!(
        last_write.keys().all(|p| synced_at[p] < dir_sync),
        "an output synced after the directory"
    );
    assert!(
        dir_sync < first_read.expect("verified"),
        "verification before the directory sync"
    );
    let syncs = calls
        .iter()
        .filter(|c| matches!(c, Call::Sync(p) if is_output(p)))
        .count();
    assert!(syncs > last_write.len(), "no output synced before its end");
}
