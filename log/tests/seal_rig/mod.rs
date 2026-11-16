//! A writer driven by hand over a file system that watches reads, for the tests of the plaintext hash at seal and of
//! the description of unlisted segments (Q-220, Q-221).

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::segment::SealedFile;
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, IngestQueue, LogWriter, WriterConfig, create_segment, describe_sealed,
    seal_segment_described,
};
use sha2::{Digest, Sha256};

pub const STORE_ID: [u8; 16] = [6; 16];
pub const T0: i64 = 1_767_225_600_000_000_000;
pub const MS: Duration = Duration::from_millis(1);
pub const WAIT: Duration = Duration::from_secs(10);

pub fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

pub fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    }
}

pub fn fact(series: u32, t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

/// A file system over [`MemFs`] that counts the bytes read per path opened, fails reads of chosen paths, and can hold
/// the first read made on `kvik-maint`.
#[derive(Clone, Debug)]
pub struct Watch {
    pub inner: MemFs,
    state: Arc<Mutex<WatchState>>,
}

#[derive(Debug, Default)]
struct WatchState {
    read: HashMap<PathBuf, u64>,
    fail: Vec<String>,
    hold: Option<(Sender<()>, Receiver<()>)>,
}

#[derive(Debug)]
struct WatchFile {
    inner: Box<dyn FsFile>,
    path: PathBuf,
    state: Arc<Mutex<WatchState>>,
}

impl Watch {
    pub fn new() -> Self {
        Self {
            inner: MemFs::new(),
            state: Arc::default(),
        }
    }
    pub fn reset_reads(&self) {
        self.state.lock().unwrap().read.clear();
    }
    pub fn read_of(&self, path: &Path) -> u64 {
        self.state
            .lock()
            .unwrap()
            .read
            .get(path)
            .copied()
            .unwrap_or(0)
    }
    pub fn fail_reads(&self, path_contains: &str) {
        self.state.lock().unwrap().fail.push(path_contains.into());
    }
    pub fn clear_failures(&self) {
        self.state.lock().unwrap().fail.clear();
    }
    /// Holds the next read on `kvik-maint`: `entered` is signalled, then the read waits for `release`.
    pub fn hold_next_maint_read(&self) -> (Receiver<()>, Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        self.state.lock().unwrap().hold = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
}

impl Fs for Watch {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        Ok(Box::new(WatchFile {
            inner: self.inner.open(path, mode)?,
            path: path.to_path_buf(),
            state: self.state.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        self.inner.free_space(path)
    }
}

impl FsFile for WatchFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let hold = {
            let mut s = self.state.lock().unwrap();
            let name = self.path.to_string_lossy();
            if s.fail.iter().any(|f| name.contains(f.as_str())) {
                return Err(io::Error::other("injected read failure"));
            }
            if std::thread::current().name() == Some("kvik-maint") {
                s.hold.take()
            } else {
                None
            }
        };
        if let Some((entered, release)) = hold {
            entered.send(()).unwrap();
            let _ = release.recv();
        }
        let n = self.inner.read_at(buf, offset)?;
        *self
            .state
            .lock()
            .unwrap()
            .read
            .entry(self.path.clone())
            .or_default() += n as u64;
        Ok(n)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
}

pub fn dir() -> PathBuf {
    PathBuf::from("store")
}

pub fn seg_path(file_id: u64, sealed: bool) -> PathBuf {
    let name = SegmentName {
        generation: 0,
        file_id,
        sealed,
    };
    dir().join("log").join(name.to_string())
}

/// The writer driven by hand: maintenance work runs only when asked.
pub struct Rig {
    pub fs: Watch,
    pub w: LogWriter,
    pub q: IngestQueue,
    pub now: Duration,
    pub wall: i64,
    pub t: i64,
    pub described: Vec<SealedFile>,
}

impl Rig {
    pub fn open(fs: Watch) -> Self {
        let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
        let w = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap();
        let mut r = Self {
            fs,
            w,
            q: IngestQueue::new(cfg().ingest_queue),
            now: Duration::ZERO,
            wall: T0,
            t: 0,
            described: Vec::new(),
        };
        r.create();
        r
    }

    pub fn create(&mut self) {
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    /// Runs every seal job and description until none is handed out, collecting descriptions.
    pub fn maint(&mut self) {
        loop {
            self.create();
            let jobs = self.w.take_seal_jobs(self.now);
            let unlisted = if jobs.is_empty() {
                self.w.take_unlisted()
            } else {
                None
            };
            if jobs.is_empty() && unlisted.is_none() {
                break;
            }
            for job in jobs {
                let r = seal_segment_described(&self.fs, &job, &self.w.halt());
                self.w.seal_done(job, r, self.now);
            }
            if let Some(id) = unlisted {
                let r = describe_sealed(&self.fs, self.w.log_dir(), &id);
                self.w.unlisted_done(r);
            }
        }
        self.create();
        self.described.extend(self.w.take_sealed());
    }

    /// Commits `n` calls of two facts each, one batch a call, without a roll.
    pub fn fill_batches(&mut self, n: usize) {
        let open = self.w.stats().open_file_id;
        for _ in 0..n {
            let items = vec![fact(1, self.t), fact(2, self.t)];
            self.t += 1;
            self.q
                .push(Call::new(items, cfg().call_limit()).unwrap())
                .unwrap();
            while !self.q.is_empty() {
                let s = self.w.step(self.now, self.wall, &mut self.q);
                assert!(s.done.iter().all(|(_, r)| r.is_ok()));
                self.now += 20 * MS;
                self.wall += 20_000_000;
            }
        }
        assert_eq!(self.w.stats().open_file_id, open);
    }

    /// Commits facts until `n` segments have been handed to be sealed, without sealing them.
    pub fn fill(&mut self, n: u64) {
        let mut open = self.w.stats().open_file_id;
        let mut rolls = 0;
        while rolls < n {
            let items: Vec<Item> = (0..40)
                .map(|i| fact(1 + i % 3, self.t + i64::from(i)))
                .collect();
            self.t += 40;
            self.q
                .push(Call::new(items, cfg().call_limit()).unwrap())
                .unwrap();
            while !self.q.is_empty() {
                let s = self.w.step(self.now, self.wall, &mut self.q);
                assert!(s.done.iter().all(|(_, r)| r.is_ok()));
                self.create();
                self.now += 20 * MS;
                self.wall += 20_000_000;
                let now_open = self.w.stats().open_file_id;
                if now_open != open {
                    rolls += u64::from(open.is_some());
                    open = now_open;
                }
            }
        }
    }
}

pub fn sealed_files(fs: &MemFs) -> BTreeMap<u64, Vec<u8>> {
    fs.paths()
        .into_iter()
        .filter_map(|p| {
            let n = SegmentName::parse(p.file_name()?.to_str()?)?;
            n.sealed.then(|| (n.file_id, fs.read(&p).unwrap()))
        })
        .collect()
}

/// The description a sealed file's own bytes give.
pub fn expected(file_id: u64, bytes: &[u8]) -> SealedFile {
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    };
    let t = scan(bytes, &id, ScanLimit::End)
        .unwrap()
        .seal
        .unwrap()
        .trailer;
    SealedFile {
        id,
        len: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
        sequences: Some((t.first_sequence, t.last_sequence)),
        commit_times: Some((t.first_commit_time, t.last_commit_time)),
    }
}

pub fn by_id(fs: &[SealedFile]) -> BTreeMap<u64, SealedFile> {
    let m: BTreeMap<_, _> = fs.iter().map(|f| (f.id.file_id, f.clone())).collect();
    assert_eq!(m.len(), fs.len(), "a segment described twice");
    m
}
