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
use kvik::{LogConfig, Select, SeriesId, Start, Store, StoreConfig};
use kvik_log::name::SegmentName;

#[derive(Debug, Default)]
struct Counts {
    lease_writes: u64,
    segment_syncs: u64,
    /// Reads made on the `kvik-deliver` thread.
    deliver_reads: u64,
}

/// The in-memory file system counting lease writes, log segment syncs and the `kvik-deliver` thread's reads.
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
        if std::thread::current().name() == Some("kvik-deliver") {
            self.counts.lock().unwrap().deliver_reads += 1;
        }
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
    counted_sealing(name, n, LogConfig::default().seal_interval)
}

/// [`counted`] with segments sealed every `seal`.
fn counted_sealing(
    name: &str,
    n: usize,
    seal: Duration,
) -> (Store, ManualClock, Arc<Mutex<Counts>>, Vec<SeriesId>) {
    let clock = ManualClock::new(start());
    let counts = Arc::new(Mutex::new(Counts::default()));
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(CountFs {
        inner: MemFs::new(),
        counts: counts.clone(),
    });
    let mut log = LogConfig::default();
    log.min_sync_interval = Duration::ZERO;
    log.seal_interval = seal;
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
    // With a consumer every emit reads its batch back itself (Q-706), as each caller did before.
    let consumer = block_on(store.consumer("c", Start::Now, Select::None)).unwrap();
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
    drop(consumer);
    block_on(store.shutdown()).unwrap();
}

fn deliver_reads(c: &Mutex<Counts>) -> u64 {
    c.lock().unwrap().deliver_reads
}

/// The `kvik-deliver` thread's reads over a burst of 50 emits with no consumer, once `before` has passed.
fn reads_over_a_burst(name: &str, seal: Duration, before: Duration) -> u64 {
    let (store, _clock, counts, ids) = counted_sealing(name, 1, seal);
    block_on(store.emit(&[num(ids[0], -1, 1.0)])).unwrap();
    std::thread::sleep(before);
    let r0 = deliver_reads(&counts);
    for i in 0..50 {
        block_on(store.emit(&[num(ids[0], i, 1.0)])).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    let reads = deliver_reads(&counts) - r0;
    block_on(store.shutdown()).unwrap();
    reads
}

#[test]
fn with_no_consumer_the_delivery_thread_reads_no_batch_back_on_each_emit() {
    // Past the first interval, so the burst is counted in the thread's steady state.
    let reads = reads_over_a_burst(
        "no-consumer-reads",
        LogConfig::default().seal_interval,
        Duration::from_millis(1_200),
    );
    // One refresh's reads at most: the burst is shorter than the interval between reads with no consumer (Q-1490).
    assert!(reads <= 16, "{reads} reads for 50 emits");
}

#[test]
fn with_no_consumer_the_delivery_thread_reads_once_at_a_seal_and_not_on_each_emit_after_it() {
    // A seal at about a second, then a burst before the next.
    let reads = reads_over_a_burst(
        "no-consumer-seal",
        Duration::from_secs(1),
        Duration::from_millis(1_300),
    );
    assert!(reads <= 32, "{reads} reads for 50 emits");
}

#[test]
fn an_idle_store_writes_no_delivery_lease_before_its_renewal_is_due() {
    let (store, _clock, counts, ids) = counted("idle-lease", 1);
    block_on(store.emit(&[num(ids[0], 0, 1.0)])).unwrap();
    // With no consumer the batch is read back after the emit returns (Q-706); a tick takes it.
    store.tick().unwrap();
    let (l0, _) = snapshot(&counts);
    for _ in 0..5 {
        store.tick().unwrap();
    }
    assert_eq!(snapshot(&counts).0, l0);
    block_on(store.shutdown()).unwrap();
}
