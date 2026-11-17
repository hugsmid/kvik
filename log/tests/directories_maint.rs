//! The derived files on the real threads: the start's work runs in bounded steps between the maintenance thread's
//! other duties, so ingest never waits for it; a log dropped on its own maintenance thread does not wait for itself;
//! a hook that panics is counted and leaves the store writable.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{DirectoryOptions, Log, LogWriter, SystemClock, WriterConfig};

const T0: i64 = 1_767_225_600_000_000_000;

#[derive(Debug)]
struct SlowFs {
    inner: MemFs,
    slow: AtomicBool,
}

impl Fs for SlowFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let sealed = path.extension().is_some_and(|e| e == "kvl");
        if sealed && matches!(mode, OpenMode::Read) && self.slow.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
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
    fs.slow.store(false, Ordering::SeqCst);
    let store = StoreDir::open(fs.clone(), dir, Some(&new)).unwrap();
    let w = LogWriter::open(store, cfg(), Some([5; 16]), Duration::ZERO, T0).unwrap();
    fs.slow.store(true, Ordering::SeqCst);
    Log::start_with(
        w,
        Arc::new(SystemClock::default()),
        dirs.then(DirectoryOptions::default),
    )
    .unwrap()
}

/// Submits calls one by one for `calls` calls and returns the longest wait for an acknowledgement.
fn ingest(log: &Log, calls: i64, base: i64) -> Duration {
    let mut worst = Duration::ZERO;
    for i in 0..calls {
        let at = Instant::now();
        let items: Vec<Item> = (0..40).map(|k| fact(base + i * 40 + k)).collect();
        log.submit(items).unwrap().wait().unwrap();
        worst = worst.max(at.elapsed());
    }
    worst
}

fn sealed(fs: &MemFs) -> usize {
    fs.paths()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
        .count()
}

#[test]
fn ingest_does_not_wait_while_the_directories_are_first_built() {
    let mut worst = Vec::new();
    for dirs in [false, true] {
        let fs = Arc::new(SlowFs {
            inner: MemFs::new(),
            slow: AtomicBool::new(false),
        });
        let dir = PathBuf::from("store");
        // A log written before the derived files existed (or whose `derived/` was lost, or a restored store).
        let log = start(fs.clone(), &dir, false);
        ingest(&log, 1_500, 0);
        log.shutdown().unwrap();
        let n = sealed(&fs.inner);
        let log = start(fs.clone(), &dir, dirs);
        let w = ingest(&log, 200, 10_000_000);
        assert!(n > 50, "{n} sealed segments");
        let at = Instant::now();
        log.shutdown().unwrap();
        // A shutdown never waits for the start's work.
        assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
        worst.push(w);
    }
    assert!(
        worst[1] < worst[0] * 20 + Duration::from_millis(500),
        "ingest waited {:?} with the directories, {:?} without",
        worst[1],
        worst[0]
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
        slow: AtomicBool::new(false),
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
        slow: AtomicBool::new(false),
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
        slow: AtomicBool::new(false),
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
