//! The derived files on the real threads: the start's work runs in bounded steps between the maintenance thread's
//! other duties, so ingest never waits for it; a log dropped on its own maintenance thread does not wait for itself;
//! a hook that panics is counted and leaves the store writable.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{DirectoryOptions, Log, LogWriter, SystemClock, WriterConfig};

const T0: i64 = 1_767_225_600_000_000_000;

#[derive(Debug, Default)]
struct PaceState {
    armed: bool,
    permits: u64,
    waiting: bool,
    reads: u64,
}

/// Holds every read of a sealed segment until the test grants it a permit, so a build is at a known point.
#[derive(Debug, Default)]
struct Pace {
    state: Mutex<PaceState>,
    cv: Condvar,
}

impl Pace {
    fn arm(&self) {
        self.state.lock().unwrap().armed = true;
    }

    fn disarm(&self) {
        self.state.lock().unwrap().armed = false;
        self.cv.notify_all();
    }

    fn pass(&self) {
        let mut s = self.state.lock().unwrap();
        while s.armed && s.permits == 0 {
            s.waiting = true;
            self.cv.notify_all();
            s = self.cv.wait(s).unwrap();
        }
        s.waiting = false;
        if s.armed {
            s.permits -= 1;
        }
        s.reads += 1;
    }

    fn wait_held(&self) {
        let mut s = self.state.lock().unwrap();
        while !s.waiting {
            s = self.cv.wait(s).unwrap();
        }
    }

    fn reads(&self) -> u64 {
        self.state.lock().unwrap().reads
    }

    /// Lets one read through, then waits until the next is held or `done`.
    fn step(&self, done: &AtomicBool) {
        let mut s = self.state.lock().unwrap();
        s.permits += 1;
        s.waiting = false;
        self.cv.notify_all();
        while !s.waiting && !done.load(Ordering::SeqCst) {
            s = self.cv.wait_timeout(s, Duration::from_millis(1)).unwrap().0;
        }
    }
}

#[derive(Debug)]
struct SlowFs {
    inner: MemFs,
    pace: Pace,
}

impl Fs for SlowFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let sealed = path.extension().is_some_and(|e| e == "kvl");
        if sealed && matches!(mode, OpenMode::Read) {
            self.pace.pass();
        }
        self.inner.open(path, mode)
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

fn fact(i: i64) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t: T0 + i,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(i as u64),
        changed_at: None,
    })
}

fn start(fs: Arc<SlowFs>, dir: &Path, dirs: bool) -> Log {
    let new = NewStore {
        store_id: [9; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    fs.pace.disarm();
    let store = StoreDir::open(fs.clone(), dir, Some(&new)).unwrap();
    let w = LogWriter::open(store, cfg(), Some([5; 16]), Duration::ZERO, T0).unwrap();
    fs.pace.arm();
    Log::start_with(
        w,
        Arc::new(SystemClock::default()),
        dirs.then(DirectoryOptions::default),
    )
    .unwrap()
}

/// Submits `calls` calls one by one, each acknowledged before the next.
fn ingest(log: &Log, calls: i64, base: i64) {
    for i in 0..calls {
        let items: Vec<Item> = (0..40).map(|k| fact(base + i * 40 + k)).collect();
        log.submit(items).unwrap().wait().unwrap();
    }
}

fn sealed(fs: &MemFs) -> usize {
    fs.paths()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
        .count()
}

#[test]
fn ingest_does_not_wait_while_the_directories_are_first_built() {
    let fs = Arc::new(SlowFs {
        inner: MemFs::new(),
        pace: Pace::default(),
    });
    let dir = PathBuf::from("store");
    // A log written before the derived files existed (or whose `derived/` was lost, or a restored store).
    let log = start(fs.clone(), &dir, false);
    ingest(&log, 1_500, 0);
    log.shutdown().unwrap();
    let n = sealed(&fs.inner) as u64;
    assert!(n > 50, "{n} sealed segments");
    let log = start(fs.clone(), &dir, true);
    fs.pace.wait_held();
    let held_at = fs.pace.reads();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            ingest(&log, 5, 10_000_000);
            tx.send(()).unwrap();
        });
        // Within one segment, so no rollover needs the held maintenance thread. Generous: only a failure waits.
        if rx.recv_timeout(Duration::from_secs(60)).is_err() {
            fs.pace.disarm();
            panic!("ingest waited for the directories' first build");
        }
    });
    assert_eq!(
        fs.pace.reads(),
        held_at,
        "the build was not held while ingest ran"
    );
    // A shutdown never waits for the start's work: it ends within the step under way.
    let watch = log.watch();
    let done = Arc::new(AtomicBool::new(false));
    let stopper = {
        let done = done.clone();
        std::thread::spawn(move || {
            let r = log.shutdown();
            done.store(true, Ordering::SeqCst);
            r
        })
    };
    while !watch.stopped() {
        std::thread::yield_now();
    }
    while !done.load(Ordering::SeqCst) {
        fs.pace.step(&done);
    }
    stopper.join().unwrap().unwrap();
    let built = fs.pace.reads() - held_at;
    assert!(
        built < n / 2,
        "{built} reads of {n} sealed segments before the shutdown ended"
    );
}

/// A hook that holds the log and drops it on its first call, as the last owner of a store would on the maintenance
/// thread.
struct Dropper(std::sync::Mutex<Option<Log>>, AtomicBool);

impl kvik_log::writer::SealHook for Dropper {
    fn series(
        &self,
        _: kvik_log::writer::Position,
        _: i64,
    ) -> Option<kvik_log::writer::SeriesEntries> {
        None
    }
    fn health(&self) -> Option<(u8, bool)> {
        drop(self.0.lock().unwrap().take());
        self.1.store(true, Ordering::SeqCst);
        Some((0, false))
    }
}

#[test]
fn a_log_dropped_on_its_own_maintenance_thread_does_not_wait_for_itself() {
    let fs = Arc::new(SlowFs {
        inner: MemFs::new(),
        pace: Pace::default(),
    });
    let new = NewStore {
        store_id: [9; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let store = StoreDir::open(fs.clone(), &PathBuf::from("store"), Some(&new)).unwrap();
    let w = LogWriter::open(store, cfg(), Some([5; 16]), Duration::ZERO, T0).unwrap();
    let hook = Arc::new(Dropper(std::sync::Mutex::new(None), AtomicBool::new(false)));
    let opts = DirectoryOptions {
        hook: Some(hook.clone()),
        status_interval: None,
    };
    let log = Log::start_with(w, Arc::new(SystemClock::default()), Some(opts)).unwrap();
    let watch = log.watch();
    *hook.0.lock().unwrap() = Some(log);
    let at = Instant::now();
    while !(hook.1.load(Ordering::SeqCst) && watch.stopped()) {
        assert!(
            at.elapsed() < Duration::from_secs(10),
            "the log did not stop"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(watch.fault(), None, "no panic on the maintenance thread");
    assert!(!hook.0.is_poisoned(), "the drop panicked inside the hook");
}

/// A hook that panics in every call.
struct Panics;

impl kvik_log::writer::SealHook for Panics {
    fn series(
        &self,
        _: kvik_log::writer::Position,
        _: i64,
    ) -> Option<kvik_log::writer::SeriesEntries> {
        panic!("a hook that fails")
    }
    fn health(&self) -> Option<(u8, bool)> {
        panic!("a hook that fails")
    }
}

#[test]
fn a_hook_that_panics_leaves_the_store_writable() {
    let fs = Arc::new(SlowFs {
        inner: MemFs::new(),
        pace: Pace::default(),
    });
    let new = NewStore {
        store_id: [9; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let store = StoreDir::open(fs.clone(), &PathBuf::from("store"), Some(&new)).unwrap();
    let w = LogWriter::open(store, cfg(), Some([5; 16]), Duration::ZERO, T0).unwrap();
    let opts = DirectoryOptions {
        hook: Some(Arc::new(Panics)),
        status_interval: None,
    };
    let log = Log::start_with(w, Arc::new(SystemClock::default()), Some(opts)).unwrap();
    ingest(&log, 300, 0);
    assert!(sealed(&fs.inner) > 2);
    assert_eq!(log.fault(), None);
    log.shutdown().unwrap();
}

#[test]
fn a_seal_is_listed_soon_although_the_timed_mark_sync_is_an_hour_away() {
    let fs = Arc::new(SlowFs {
        inner: MemFs::new(),
        pace: Pace::default(),
    });
    let new = NewStore {
        store_id: [9; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let dir = PathBuf::from("store");
    let store = StoreDir::open(fs.clone(), &dir, Some(&new)).unwrap();
    let c = WriterConfig {
        mark_sync: kvik_log::writer::MarkSync::Interval(Duration::from_secs(3_600)),
        ..cfg()
    };
    let w = LogWriter::open(store, c, Some([5; 16]), Duration::ZERO, T0).unwrap();
    let log = Log::start_with(
        w,
        Arc::new(SystemClock::default()),
        Some(DirectoryOptions::default()),
    )
    .unwrap();
    ingest(&log, 100, 0);
    assert!(sealed(&fs.inner) > 0);
    // The maintenance thread asks for the mark sync a listing waits for.
    let at = Instant::now();
    loop {
        let root = kvik_log::directory::read_latest(
            &fs.inner,
            &dir,
            kvik_log::directory::Family::Root,
            |b| kvik_log::directory::logdir::Root::decode(b, &[9; 16]),
        )
        .unwrap();
        if root.is_some_and(|(_, r)| r.segment_count > 0) {
            break;
        }
        assert!(
            at.elapsed() < Duration::from_secs(20),
            "no sealed segment listed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    log.shutdown().unwrap();
}
