//! The plaintext SHA-256 of a sealed log segment and its file description (`format.md` A.9, A.14; Q-220, Q-221):
//! computed at seal on `kvik-maint`, once, and after a start for every sealed segment no record names.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, FaultOp, Fs, FsFile, MemFs, OpenMode};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::segment::SegmentDescription;
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, IngestQueue, Log, LogWriter, SEAL_RETRY_INTERVAL, SystemClock,
    WriterConfig, create_segment, seal_segment,
};
use sha2::{Digest, Sha256};

const STORE_ID: [u8; 16] = [6; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);
const WAIT: Duration = Duration::from_secs(10);

fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    }
}

fn fact(series: u32, t: i64) -> Item {
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
struct Watch {
    inner: MemFs,
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
    fn new() -> Self {
        Self {
            inner: MemFs::new(),
            state: Arc::default(),
        }
    }
    fn reset_reads(&self) {
        self.state.lock().unwrap().read.clear();
    }
    fn read_of(&self, path: &Path) -> u64 {
        self.state.lock().unwrap().read.get(path).copied().unwrap_or(0)
    }
    fn fail_reads(&self, path_contains: &str) {
        self.state.lock().unwrap().fail.push(path_contains.into());
    }
    fn clear_failures(&self) {
        self.state.lock().unwrap().fail.clear();
    }
    /// Holds the next read on `kvik-maint`: `entered` is signalled, then the read waits for `release`.
    fn hold_next_maint_read(&self) -> (Receiver<()>, Sender<()>) {
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

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn seg_path(file_id: u64, sealed: bool) -> PathBuf {
    let name = SegmentName {
        generation: 0,
        file_id,
        sealed,
    };
    dir().join("log").join(name.to_string())
}

/// The writer driven by hand: maintenance work runs only when asked.
struct Rig {
    fs: Watch,
    w: LogWriter,
    q: IngestQueue,
    now: Duration,
    wall: i64,
    t: i64,
    described: Vec<SegmentDescription>,
}

impl Rig {
    fn open(fs: Watch) -> Self {
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

    fn create(&mut self) {
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    /// Runs every job until none is handed out, collecting descriptions.
    fn maint(&mut self) {
        loop {
            self.create();
            let jobs = self.w.take_seal_jobs(self.now);
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                let r = seal_segment(&self.fs, &job, &self.w.halt());
                self.w.seal_done(job, r, self.now);
            }
        }
        self.create();
        self.described.extend(self.w.take_sealed());
    }

    /// Commits facts until `n` segments have been handed to be sealed, without sealing them.
    fn fill(&mut self, n: u64) {
        let mut open = self.w.stats().open_file_id;
        let mut rolls = 0;
        while rolls < n {
            let items: Vec<Item> = (0..40).map(|i| fact(1 + i % 3, self.t + i64::from(i))).collect();
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

fn sealed_files(fs: &MemFs) -> BTreeMap<u64, Vec<u8>> {
    fs.paths()
        .into_iter()
        .filter_map(|p| {
            let n = SegmentName::parse(p.file_name()?.to_str()?)?;
            n.sealed.then(|| (n.file_id, fs.read(&p).unwrap()))
        })
        .collect()
}

/// The description a sealed file's own bytes give.
fn expected(file_id: u64, bytes: &[u8]) -> SegmentDescription {
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    };
    let seal = scan(bytes, &id, ScanLimit::End).unwrap().seal.unwrap();
    SegmentDescription {
        generation: 0,
        file_id,
        first_sequence: seal.trailer.first_sequence,
        last_sequence: seal.trailer.last_sequence,
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    }
}

fn by_id(ds: &[SegmentDescription]) -> BTreeMap<u64, SegmentDescription> {
    let m: BTreeMap<_, _> = ds.iter().map(|d| (d.file_id, *d)).collect();
    assert_eq!(m.len(), ds.len(), "a segment described twice");
    m
}

#[test]
fn the_hash_is_sha256_of_the_sealed_files_bytes() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    let files = sealed_files(&r.fs.inner);
    assert_eq!(files.len(), 3);
    let got = by_id(&r.described);
    assert_eq!(r.w.stats().sealed_segments, 3);
    for (id, bytes) in &files {
        assert_eq!(got[id], expected(*id, bytes));
    }
    assert_eq!(got.len(), files.len());
    assert!(r.w.take_sealed().is_empty(), "each description is handed over once");
}

#[test]
fn a_blocked_seal_reads_and_hashes_its_segment_once() {
    let mut r = Rig::open(Watch::new());
    r.fill(1);
    r.fs.reset_reads();
    r.fs.inner
        .fail_nth(FaultOp::Rename, ".kvl", 0, io::ErrorKind::StorageFull);
    r.maint();
    assert!(r.described.is_empty());
    assert!(sealed_files(&r.fs.inner).is_empty());
    r.now += SEAL_RETRY_INTERVAL;
    r.maint();
    let files = sealed_files(&r.fs.inner);
    let (id, bytes) = files.iter().next().unwrap();
    let id = *id;
    let seal = scan(
        bytes,
        &SegmentIdentity {
            store_id: STORE_ID,
            file_id: id,
            generation: 0,
        },
        ScanLimit::End,
    )
    .unwrap()
    .seal
    .unwrap();
    assert_eq!(r.fs.read_of(&seg_path(id, false)), seal.start);
    assert_eq!(r.described, vec![expected(id, bytes)]);
}

#[test]
fn a_read_error_while_sealing_is_a_write_failure_on_the_log() {
    let mut r = Rig::open(Watch::new());
    r.fill(1);
    r.fs.fail_reads(".open");
    r.maint();
    let f = r.w.fault().expect("read-only").clone();
    assert_eq!(f.op, kvik_log::writer::FaultOp::Seal);
    assert!(r.described.is_empty());
    assert!(sealed_files(&r.fs.inner).is_empty());
}

#[test]
fn a_start_describes_every_sealed_segment_no_record_names() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    let at_seal = by_id(&r.described);
    r.w.shutdown(r.now).unwrap();
    let fs = r.fs.clone();
    drop(r);
    let mut r = Rig::open(fs);
    let files = sealed_files(&r.fs.inner);
    assert_eq!(files.len(), 3);
    // One description at a time, outside the seals a shutdown waits for and the seal count.
    let jobs = r.w.take_seal_jobs(r.now);
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].describes_only());
    assert!(!r.w.seals_running());
    assert!(r.w.take_seal_jobs(r.now).is_empty());
    for job in jobs {
        let res = seal_segment(&r.fs, &job, &r.w.halt());
        r.w.seal_done(job, res, r.now);
    }
    r.maint();
    assert_eq!(r.w.stats().sealed_segments, 0);
    let got = by_id(&r.described);
    assert_eq!(got, at_seal);
    for (id, bytes) in &files {
        assert_eq!(got[id], expected(*id, bytes));
    }
}

#[test]
fn a_start_leaves_a_segment_it_cannot_read_or_verify_unnamed() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    r.w.shutdown(r.now).unwrap();
    let fs = r.fs.clone();
    drop(r);
    let files = sealed_files(&fs.inner);
    let ids: Vec<u64> = files.keys().copied().collect();
    // The first cannot be read; the second has a byte flipped inside a batch, which `file_crc32c` covers.
    fs.fail_reads(&seg_path(ids[0], true).to_string_lossy());
    let at = 200;
    fs.inner
        .poke(&seg_path(ids[1], true), at, &[files[&ids[1]][at] ^ 0x40]);
    let mut r = Rig::open(fs.clone());
    r.maint();
    assert!(r.w.fault().is_none());
    let got = by_id(&r.described);
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![ids[2]]);
    r.fill(1);
    r.maint();
    assert!(r.w.fault().is_none());
    r.w.shutdown(r.now).unwrap();
    drop(r);
    // The next start tries again: the readable one is described, the damaged one never.
    fs.clear_failures();
    let mut r = Rig::open(fs);
    r.maint();
    let got = by_id(&r.described);
    assert!(got.contains_key(&ids[0]));
    assert!(!got.contains_key(&ids[1]));
    assert_eq!(got[&ids[0]], expected(ids[0], &files[&ids[0]]));
}

/// Waits for an acknowledgement on another thread, so a writer that never answers fails the test instead of
/// hanging it.
fn ack_within(p: kvik_log::writer::Pending, d: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.wait().is_ok());
    });
    rx.recv_timeout(d).unwrap_or(false)
}

#[test]
fn the_commit_thread_acknowledges_while_a_seal_is_hashing() {
    let fs = Watch::new();
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    let w = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let (entered, release) = fs.hold_next_maint_read();
    let mut t = 0;
    let mut submit = |log: &Log| {
        let items: Vec<Item> = (0..40).map(|i| fact(1, t + i)).collect();
        t += 40;
        log.submit(items).unwrap()
    };
    // Fill the first segment until its seal starts reading it, on `kvik-maint`.
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(ack_within(submit(&log), WAIT));
        if entered.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "no seal started");
    }
    // The seal is held mid-read: acknowledgements continue and nothing is described yet.
    for _ in 0..20 {
        assert!(ack_within(submit(&log), WAIT));
    }
    assert!(log.take_sealed().is_empty());
    release.send(()).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.is_empty() {
        assert!(Instant::now() < deadline, "the seal did not finish");
        got.extend(log.take_sealed());
        std::thread::sleep(MS);
    }
    log.shutdown().unwrap();
    let files = sealed_files(&fs.inner);
    for d in &got {
        assert_eq!(*d, expected(d.file_id, &files[&d.file_id]));
    }
}
