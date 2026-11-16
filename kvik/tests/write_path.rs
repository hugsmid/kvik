//! The durable store's path above the log writer: how often the delivery reader reads acknowledged batches back,
//! and that its lease is kept alive while the store is idle.

mod common;

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::world::{gauge, num};
use common::{block_on, start};
use kvik::env::{DirEntry, Env, Fs, FsFile, ManualClock, MemFs, OpenMode};
use kvik::{LogConfig, SeriesId, Store, StoreConfig};
use kvik_log::name::SegmentName;

#[derive(Debug, Default)]
struct Counts {
    lease_writes: u64,
    segment_syncs: u64,
}

/// The in-memory file system counting lease writes and log segment syncs.
#[derive(Debug)]
struct CountFs {
    inner: MemFs,
    counts: Arc<Mutex<Counts>>,
}

#[derive(Debug)]
struct CountFile {
    inner: Box<dyn FsFile>,
    lease: bool,
    segment: bool,
    counts: Arc<Mutex<Counts>>,
}

impl FsFile for CountFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.lease {
            self.counts.lock().unwrap().lease_writes += 1;
        }
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        if self.segment {
            self.counts.lock().unwrap().segment_syncs += 1;
        }
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        if self.segment {
            self.counts.lock().unwrap().segment_syncs += 1;
        }
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

impl Fs for CountFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        Ok(Box::new(CountFile {
            inner: self.inner.open(path, mode)?,
            lease: path.parent().is_some_and(|p| p.ends_with("leases")),
            segment: SegmentName::parse(name).is_some(),
            counts: self.counts.clone(),
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

/// A durable store on a counting file system, with `n` gauges defined.
fn counted(name: &str, n: usize) -> (Store, ManualClock, Arc<Mutex<Counts>>, Vec<SeriesId>) {
    let clock = ManualClock::new(start());
    let counts = Arc::new(Mutex::new(Counts::default()));
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(CountFs {
        inner: MemFs::new(),
        counts: counts.clone(),
    });
    let mut log = LogConfig::default();
    log.min_sync_interval = Duration::ZERO;
    let dir = format!("write-path/{name}");
    let store = block_on(Store::open(
        StoreConfig::new(&dir, "Etc/UTC")
            .create_if_missing(true)
            .env(env)
            .log(log),
    ))
    .unwrap();
    let ids = (0..n)
        .map(|i| block_on(store.define_series(gauge(&format!("p/s{i}/power")))).unwrap())
        .collect();
    (store, clock, counts, ids)
}

fn snapshot(c: &Mutex<Counts>) -> (u64, u64) {
    let c = c.lock().unwrap();
    (c.lease_writes, c.segment_syncs)
}

#[test]
fn the_delivery_reader_refreshes_at_most_once_per_batch_sync() {
    let (store, _clock, counts, ids) = counted("once-per-sync", 32);
    let (l0, s0) = snapshot(&counts);
    for i in 0..50 {
        block_on(store.emit(&[num(ids[0], i, 1.0)])).unwrap();
    }
    let (l1, s1) = snapshot(&counts);
    assert!(
        l1 - l0 <= s1 - s0,
        "{} refreshes for {} syncs",
        l1 - l0,
        s1 - s0
    );
    let writers: Vec<_> = (0..16)
        .map(|w| {
            let store = store.clone();
            let mine = [ids[2 * w], ids[2 * w + 1]];
            std::thread::spawn(move || {
                for i in 100..140 {
                    block_on(store.emit(&[num(mine[0], i, 1.0), num(mine[1], i, 2.0)])).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    let (l2, s2) = snapshot(&counts);
    assert!(
        l2 - l1 <= s2 - s1,
        "{} refreshes for {} syncs",
        l2 - l1,
        s2 - s1
    );
    block_on(store.shutdown()).unwrap();
}

#[test]
fn an_idle_store_writes_no_delivery_lease_before_its_renewal_is_due() {
    let (store, _clock, counts, ids) = counted("idle-lease", 1);
    block_on(store.emit(&[num(ids[0], 0, 1.0)])).unwrap();
    let (l0, _) = snapshot(&counts);
    for _ in 0..5 {
        store.tick().unwrap();
    }
    assert_eq!(snapshot(&counts).0, l0);
    block_on(store.shutdown()).unwrap();
}
