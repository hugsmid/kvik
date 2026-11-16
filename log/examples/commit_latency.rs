//! Commit latency and throughput of the log writer on a real file system (roadmap 4.8; results and method in
//! `docs/measurements/4.8-commit-latency.md`).
//!
//! ```text
//! cargo run --release --locked -p kvik-log --example commit_latency -- <dir> [options]
//!
//!   --short        about 2 minutes instead of about 8 (the CI run)
//!   --only <load>  run only the loads whose name contains <load>
//!   --repeat <n>   run the whole matrix n times
//!   --any-fs       also run on a file system whose sync does nothing (tmpfs, ramfs)
//!   --keep         keep the stores
//!   --disk <text>  describe the disk where it cannot be found out (a class, never a model or serial)
//!   --setup <text> run only the writer settings whose label contains <text>
//!   --sync-delay <ms>  sleep this long in every `sync_data`, standing in for a slow device
//! ```
//!
//! The run works in a `kvik-commit-latency` subdirectory of `<dir>`, removed at the end, and prints Markdown: the
//! environment (platform, file system, disk class, sync primitive), the sync calls on their own, the latency of
//! every acknowledged call, throughput and syncs per load, and the time to open the store again. Latency runs from
//! the submit call to the acknowledgement, stamped on the writer thread as it completes the call.
//!
//! Nothing here changes the writer: the stores run [`WriterConfig::default`] except for `min_sync_interval`,
//! `sync_interval_floor` and the mark sync, on [`StdFs`] wrapped to time each sync.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, OpenMode, StdFs};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Ack, CommitError, Log, LogWriter, MarkSync, MarkSyncMode, Pending, SubmitError, SystemClock,
    WriterConfig,
};

#[path = "commit_latency/environment.rs"]
mod environment;

use environment::{load_average, os_version, storage, sync_primitive};

const STORE_ID: [u8; 16] = [0x4b; 16];
const BOOT: [u8; 16] = [0x01; 16];
const OTHER_BOOT: [u8; 16] = [0x02; 16];
const MS: f64 = 1e6;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----- the timing file system -----

/// Which sync a sample is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncKind {
    /// `sync_data` on a segment: a batch, or a segment taken into use.
    SegmentData,
    /// `sync_all` on a segment: creation and seal.
    SegmentAll,
    /// `sync_data` on the mark file.
    Mark,
    /// A directory sync.
    Dir,
    /// Any other file.
    Other,
}

#[derive(Debug, Default)]
struct SyncLog {
    samples: Mutex<Vec<(SyncKind, u64)>>,
}

impl SyncLog {
    fn record(&self, kind: SyncKind, since: Instant) {
        lock(&self.samples).push((kind, nanos(since.elapsed())));
    }
    fn take(&self) -> Vec<(SyncKind, u64)> {
        std::mem::take(&mut *lock(&self.samples))
    }
}

/// [`StdFs`], timing every sync. Every other call passes straight through.
#[derive(Debug)]
struct TimingFs {
    log: Arc<SyncLog>,
    delay: Duration,
}

#[derive(Debug)]
struct TimingFile {
    inner: Box<dyn FsFile>,
    data: SyncKind,
    all: SyncKind,
    log: Arc<SyncLog>,
    delay: Duration,
}

impl Fs for TimingFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let name = name.as_deref().unwrap_or("");
        let (data, all) = if name.ends_with(".open") || name.ends_with(".kvl") {
            (SyncKind::SegmentData, SyncKind::SegmentAll)
        } else if name == "MARK" {
            (SyncKind::Mark, SyncKind::Mark)
        } else {
            (SyncKind::Other, SyncKind::Other)
        };
        Ok(Box::new(TimingFile {
            inner: StdFs.open(path, mode)?,
            data,
            all,
            log: self.log.clone(),
            delay: self.delay,
        }))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        StdFs.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        StdFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let t = Instant::now();
        let r = StdFs.sync_dir(path);
        self.log.record(SyncKind::Dir, t);
        r
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        StdFs.free_space(path)
    }
}

impl FsFile for TimingFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        let t = Instant::now();
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let r = self.inner.sync_data();
        self.log.record(self.data, t);
        r
    }
    fn sync_all(&self) -> io::Result<()> {
        let t = Instant::now();
        let r = self.inner.sync_all();
        self.log.record(self.all, t);
        r
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

// ----- statistics -----

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Nearest-rank percentile of `sorted` (ascending), `p` in `0.0..=100.0`.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // The small offset keeps a product such as 99.9 % of 1000 (999.000…01 in binary) at rank 999.
    let rank = (p / 100.0 * sorted.len() as f64 - 1e-9).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn ms(ns: u64) -> String {
    let v = ns as f64 / MS;
    if v < 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.2}")
    }
}

/// A percentile in ms, or a dash where the sample is too small to give it: p90 needs 10 samples, p99 100 and
/// p99.9 1,000.
fn pct(sorted: &[u64], p: f64) -> String {
    let need = (100.0 / (100.0 - p)).round() as usize;
    if sorted.len() < need.max(1) {
        "–".into()
    } else {
        ms(percentile(sorted, p))
    }
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

// ----- acknowledgement stamps -----

/// How long an acknowledged call may wait for its waker before the run fails.
const STAMP_TIMEOUT: Duration = Duration::from_secs(10);

/// Records when the writer thread completes a call: the waker of its [`Pending`].
#[derive(Debug, Default)]
struct Stamp {
    at: Mutex<Option<Instant>>,
    set: Condvar,
}

impl Stamp {
    fn set_now(&self) {
        let mut at = lock(&self.at);
        if at.is_none() {
            *at = Some(Instant::now());
            self.set.notify_all();
        }
    }

    /// Blocks until the stamp is set. The writer publishes a call's result before it wakes the waker, so a caller
    /// can see the result first; once `poll` has returned `Pending` the wake is certain.
    fn wait(&self) -> Instant {
        let at = lock(&self.at);
        let (at, _) = self
            .set
            .wait_timeout_while(at, STAMP_TIMEOUT, |at| at.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        at.expect("the writer acknowledged a call but did not wake its waker")
    }
}

impl Wake for Stamp {
    fn wake(self: Arc<Self>) {
        self.set_now();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.set_now();
    }
}

/// A submitted call, for the collector.
struct Sent {
    t0: Instant,
    stamp: Arc<Stamp>,
    outcome: Outcome,
}

enum Outcome {
    Ready(Result<Ack, CommitError>),
    Pending(Pending),
}

/// Registers a stamp on `pending`; the writer thread sets it when it completes the call.
fn track(t0: Instant, mut pending: Pending) -> Sent {
    let stamp = Arc::new(Stamp::default());
    let waker = Waker::from(stamp.clone());
    let mut cx = Context::from_waker(&waker);
    let outcome = match Pin::new(&mut pending).poll(&mut cx) {
        Poll::Ready(r) => {
            stamp.set_now();
            Outcome::Ready(r)
        }
        Poll::Pending => Outcome::Pending(pending),
    };
    Sent { t0, stamp, outcome }
}

/// Waits for every call sent and returns their latencies in ns and the number that failed.
fn collector(rx: mpsc::Receiver<Sent>) -> JoinHandle<(Vec<u64>, u64, Option<Instant>)> {
    let run = move || {
        let mut lat = Vec::new();
        let mut failed = 0;
        let mut last = None;
        for s in rx {
            let r = match s.outcome {
                Outcome::Ready(r) => r,
                Outcome::Pending(p) => p.wait(),
            };
            if r.is_err() {
                failed += 1;
                continue;
            }
            let at = s.stamp.wait();
            lat.push(nanos(at.saturating_duration_since(s.t0)));
            last = Some(last.map_or(at, |l: Instant| l.max(at)));
        }
        (lat, failed, last)
    };
    std::thread::Builder::new()
        .name("collector".into())
        .spawn(run)
        .expect("start the collector")
}

// ----- loads -----

#[derive(Clone, Copy, Debug)]
enum Load {
    /// One writer, one reading per call, a pause of `min..max` ms between calls (pseudo-random): every call is the
    /// first after a quiet period.
    Quiet { min_ms: u64, max_ms: u64 },
    /// `series` series at 1 Hz, staggered evenly, one reading per call (open loop).
    Steady { series: u32 },
    /// Every `every_ms`, `calls` calls of one reading submitted back to back.
    Burst { calls: u32, every_ms: u64 },
    /// `writers` writers at `per_sec` calls a second in all, each periodic at its own pseudo-random phase, with
    /// `readings` readings per call (open loop).
    Paced {
        writers: u32,
        per_sec: u32,
        readings: u32,
    },
    /// One writer, `readings` readings per call, each call submitted when the previous one is acknowledged.
    BackToBack { readings: u32 },
    /// `threads` writers submitting calls of `readings` readings as fast as back-pressure allows.
    Saturation { threads: u32, readings: u32 },
}

impl Load {
    fn name(&self) -> String {
        match *self {
            Load::Quiet { min_ms, max_ms } => format!("quiet ({min_ms}–{max_ms} ms apart)"),
            Load::Steady { series } => format!("steady {series} × 1 Hz"),
            Load::Burst { calls, every_ms } => format!("burst {calls} every {every_ms} ms"),
            Load::Paced {
                writers,
                per_sec,
                readings,
            } => format!("{writers} writers, {per_sec} calls/s × {readings}"),
            Load::BackToBack { readings: 1 } => "back to back, 1 writer".into(),
            Load::BackToBack { readings } => format!("back to back, 1 writer × {readings}/call"),
            Load::Saturation { threads, readings } => {
                format!("saturation {threads} × {readings}/call")
            }
        }
    }
}

/// The loads of a full or short run, with their durations in seconds.
fn loads(short: bool) -> Vec<(Load, u64)> {
    let d = |full: u64, short_s: u64| if short { short_s } else { full };
    vec![
        (
            Load::Quiet {
                min_ms: 20,
                max_ms: 200,
            },
            d(40, 6),
        ),
        (Load::Steady { series: 10 }, d(40, 10)),
        (Load::Steady { series: 100 }, d(20, 5)),
        (Load::Steady { series: 1000 }, d(10, 3)),
        (
            Load::Burst {
                calls: 500,
                every_ms: 1000,
            },
            d(20, 4),
        ),
        (
            Load::Paced {
                writers: 16,
                per_sec: 200,
                readings: 10,
            },
            d(20, 5),
        ),
        (Load::BackToBack { readings: 1 }, d(10, 3)),
        (Load::BackToBack { readings: 2000 }, d(10, 3)),
        (
            Load::Saturation {
                threads: 4,
                readings: 100,
            },
            d(5, 2),
        ),
        (
            Load::Saturation {
                threads: 16,
                readings: 10,
            },
            d(5, 2),
        ),
    ]
}

/// The writer settings each load runs under.
#[derive(Clone, Copy, Debug)]
struct Setup {
    min_sync: Duration,
    floor: Duration,
    every_commit: bool,
}

impl Setup {
    const ALL: [Setup; 4] = [
        Setup {
            min_sync: Duration::ZERO,
            floor: Duration::ZERO,
            every_commit: false,
        },
        Setup {
            min_sync: Duration::from_millis(10),
            floor: Duration::from_millis(10),
            every_commit: false,
        },
        Setup {
            min_sync: Duration::from_millis(10),
            floor: Duration::from_millis(2),
            every_commit: false,
        },
        Setup {
            min_sync: Duration::from_millis(10),
            floor: Duration::from_millis(10),
            every_commit: true,
        },
    ];
    /// `min_sync_interval`, with the floor where the interval adapts.
    fn interval(&self) -> String {
        if self.floor >= self.min_sync {
            format!("{} ms", self.min_sync.as_millis())
        } else {
            format!(
                "adaptive {}–{} ms",
                self.floor.as_millis(),
                self.min_sync.as_millis()
            )
        }
    }
    fn label(&self) -> String {
        format!("{}, {}", self.interval(), self.mark())
    }
    fn mark(&self) -> &'static str {
        if self.every_commit {
            "every commit"
        } else {
            "timed 1 s"
        }
    }
    fn config(&self) -> WriterConfig {
        WriterConfig {
            min_sync_interval: self.min_sync,
            sync_interval_floor: self.floor,
            mark_sync: if self.every_commit {
                MarkSync::EveryCommit
            } else {
                MarkSync::Interval(Duration::from_secs(1))
            },
            ..WriterConfig::default()
        }
    }
}

fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn reading(series: u32, n: u64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: wall_ns(),
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64((n as f64 * 0.25).to_bits()),
        changed_at: None,
    })
}

/// Sleeps, then spins, until `t`. A sleeping thread can wake late, by up to half the time asked for where the
/// operating system adds timer slack ([`timer_slack`]), so it sleeps half the time left at a time.
fn wait_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep((left - Duration::from_millis(1)) / 2);
        } else {
            std::hint::spin_loop();
        }
    }
}

/// How late a 5 ms sleep wakes, p50 and max of 50, in ms. The writer waits on timers too (`min_sync_interval`, the
/// timed mark sync), so slack added by the operating system shows in the results.
fn timer_slack() -> String {
    let mut v: Vec<u64> = (0..50)
        .map(|_| {
            let t = Instant::now();
            std::thread::sleep(Duration::from_millis(5));
            nanos(t.elapsed().saturating_sub(Duration::from_millis(5)))
        })
        .collect();
    v.sort_unstable();
    format!(
        "a 5 ms sleep wakes {} ms late at p50, {} ms at most",
        ms(percentile(&v, 50.0)),
        ms(v[v.len() - 1])
    )
}

/// A small deterministic generator for the quiet load's pauses.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// The open-loop schedule of a paced load: offsets from the start, with the first series and the number of
/// readings of each call.
fn schedule(load: Load, secs: u64) -> Vec<(Duration, u32, u32)> {
    let end = Duration::from_secs(secs);
    let mut out = Vec::new();
    match load {
        Load::Quiet { min_ms, max_ms } => {
            let mut rng = Lcg(0x9e37_79b9_7f4a_7c15);
            let mut t = Duration::ZERO;
            while t < end {
                out.push((t, 1, 1));
                t += Duration::from_millis(min_ms + rng.next() % (max_ms - min_ms + 1));
            }
        }
        Load::Steady { series } => {
            let step = Duration::from_secs(1) / series;
            let mut t = Duration::ZERO;
            let mut i = 0;
            while t < end {
                out.push((t, 1 + i % series, 1));
                i += 1;
                t = step * i;
            }
        }
        Load::Burst { calls, every_ms } => {
            let mut t = Duration::ZERO;
            while t < end {
                out.extend((0..calls).map(|c| (t, 1 + c, 1)));
                t += Duration::from_millis(every_ms);
            }
        }
        Load::Paced {
            writers,
            per_sec,
            readings,
        } => {
            let mut rng = Lcg(0x2545_f491_4f6c_dd1d);
            let period = Duration::from_secs(1) * writers / per_sec;
            for w in 0..writers {
                let mut t = period.mul_f64((rng.next() % 1000) as f64 / 1000.0);
                while t < end {
                    out.push((t, 1 + w * readings, readings));
                    t += period;
                }
            }
            out.sort_unstable();
        }
        Load::BackToBack { .. } | Load::Saturation { .. } => {}
    }
    out
}

fn readings(first: u32, count: u32, n: u64) -> Vec<Item> {
    (first..first + count).map(|s| reading(s, n)).collect()
}

/// What one load under one setup measured.
struct Run {
    load: Load,
    setup: Setup,
    lat: Vec<u64>,
    lag: Vec<u64>,
    busy: u64,
    failed: u64,
    calls: u64,
    readings: u64,
    secs: f64,
    syncs: u64,
    mark_syncs: u64,
    batches: u64,
    sealed: u64,
    sync_interval: Duration,
    sync_time: Option<Duration>,
    sync_samples: Vec<(SyncKind, u64)>,
}

fn run_load(log: &Arc<Log>, sync_log: &SyncLog, load: Load, secs: u64, setup: Setup) -> Run {
    let before = log.stats();
    let _ = sync_log.take();
    let (tx, rx) = mpsc::channel();
    let coll = collector(rx);
    let start = Instant::now();
    let mut lag = Vec::new();
    let mut busy = 0;
    let mut calls = 0u64;
    let mut total = 0u64;
    match load {
        Load::Quiet { .. } | Load::Steady { .. } | Load::Burst { .. } | Load::Paced { .. } => {
            let mut prev = None;
            for (off, series, count) in schedule(load, secs) {
                let due = start + off;
                wait_until(due);
                if prev != Some(off) {
                    lag.push(nanos(Instant::now().saturating_duration_since(due)));
                }
                prev = Some(off);
                let t0 = Instant::now();
                let p = match log.try_submit(readings(series, count, calls)) {
                    Ok(p) => p,
                    Err(SubmitError::Busy(_)) => {
                        busy += 1;
                        log.submit(readings(series, count, calls))
                            .expect("submit after back-pressure")
                    }
                    Err(e) => panic!("submit failed: {e}"),
                };
                // A send fails only once the collector has panicked; its join below reports why.
                if tx.send(track(t0, p)).is_err() {
                    break;
                }
                calls += 1;
                total += u64::from(count);
            }
        }
        Load::BackToBack { readings: count } => {
            let end = start + Duration::from_secs(secs);
            while Instant::now() < end {
                let t0 = Instant::now();
                let p = log
                    .submit(readings(1, count, calls))
                    .expect("submit back to back");
                let mut s = track(t0, p);
                if let Outcome::Pending(p) = s.outcome {
                    s.outcome = Outcome::Ready(p.wait());
                }
                if tx.send(s).is_err() {
                    break;
                }
                calls += 1;
                total += u64::from(count);
            }
        }
        Load::Saturation {
            threads,
            readings: per_call,
        } => {
            let end = start + Duration::from_secs(secs);
            let handles: Vec<_> = (0..threads)
                .map(|w| {
                    let log = log.clone();
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let mut n = 0u64;
                        while Instant::now() < end {
                            let items = (0..per_call)
                                .map(|i| reading(1 + w * per_call + i, n))
                                .collect();
                            let t0 = Instant::now();
                            let p = log.submit(items).expect("submit under saturation");
                            if tx.send(track(t0, p)).is_err() {
                                break;
                            }
                            n += 1;
                        }
                        n
                    })
                })
                .collect();
            for h in handles {
                let n = h.join().expect("a saturation writer");
                calls += n;
                total += n * u64::from(per_call);
            }
        }
    }
    drop(tx);
    // The collector's panic hook has printed the reason; carry its panic on without a second message.
    let (lat, failed, last) = coll.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
    let secs = last.map_or(0.0, |l| l.saturating_duration_since(start).as_secs_f64());
    let after = log.stats();
    Run {
        load,
        setup,
        lat: sorted(lat),
        lag: sorted(lag),
        busy,
        failed,
        calls,
        readings: total,
        secs,
        syncs: after.syncs - before.syncs,
        mark_syncs: after.mark_syncs - before.mark_syncs,
        batches: after.batches - before.batches,
        sealed: after.sealed_segments - before.sealed_segments,
        sync_interval: after.sync_interval,
        sync_time: after.sync_time,
        sync_samples: sync_log.take(),
    }
}

// ----- restart -----

struct Restart {
    setup: Setup,
    segments: usize,
    bytes: u64,
    same_boot: Duration,
    same_rule: String,
    other_boot: Duration,
    other_rule: String,
}

fn restart(fs: &Arc<dyn Fs>, dir: &Path, setup: Setup) -> Restart {
    let mut segments = 0;
    let mut bytes = 0;
    if let Ok(entries) = std::fs::read_dir(dir.join("log")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".open") || name.ends_with(".kvl") {
                segments += 1;
                bytes += e.metadata().map_or(0, |m| m.len());
            }
        }
    }
    let open = |boot: [u8; 16]| {
        let t = Instant::now();
        let store = StoreDir::open(fs.clone(), dir, None).expect("open the store again");
        let w = LogWriter::open(store, setup.config(), Some(boot), Duration::ZERO, wall_ns())
            .expect("open the writer again");
        let took = t.elapsed();
        assert!(w.fault().is_none(), "the start succeeded");
        (took, format!("{:?}", w.start_report().rule))
    };
    let (same_boot, same_rule) = open(BOOT);
    let (other_boot, other_rule) = open(OTHER_BOOT);
    Restart {
        setup,
        segments,
        bytes,
        same_boot,
        same_rule,
        other_boot,
        other_rule,
    }
}

// ----- sync calls on their own -----

struct CallRow {
    name: &'static str,
    lat: Vec<u64>,
    secs: f64,
}

fn sync_calls(dir: &Path, short: bool) -> io::Result<Vec<CallRow>> {
    let fs = StdFs;
    let (cap_n, cap_t) = if short {
        (500, Duration::from_millis(500))
    } else {
        (2000, Duration::from_secs(2))
    };
    std::fs::create_dir_all(dir)?;
    let mut rows = Vec::new();
    let timed = |name, f: &mut dyn FnMut(u64) -> io::Result<()>| -> io::Result<CallRow> {
        let start = Instant::now();
        let mut lat = Vec::new();
        let mut i = 0u64;
        while i < cap_n && start.elapsed() < cap_t {
            let t = Instant::now();
            f(i)?;
            lat.push(nanos(t.elapsed()));
            i += 1;
        }
        Ok(CallRow {
            name,
            lat: sorted(lat),
            secs: start.elapsed().as_secs_f64(),
        })
    };

    // A zero-filled, preallocated file, as a segment is (`format.md` A.4).
    let seg_path = dir.join("probe.open");
    let seg = fs.open(&seg_path, OpenMode::CreateNew)?;
    let zeros = vec![0u8; 1 << 20];
    for i in 0..16u64 {
        seg.write_at(&zeros, i << 20)?;
    }
    seg.sync_all()?;
    fs.sync_dir(dir)?;
    let rec = [0x5au8; 256];
    rows.push(timed(
        "256 B write + `sync_data`, preallocated (a batch)",
        &mut |i| {
            seg.write_at(&rec, (i * 256) % (15 << 20))?;
            seg.sync_data()
        },
    )?);
    rows.push(timed("256 B write + `sync_all`, preallocated", &mut |i| {
        seg.write_at(&rec, (i * 256) % (15 << 20))?;
        seg.sync_all()
    })?);
    drop(seg);

    // A mark-sized file updated in place.
    let mark = fs.open(&dir.join("probe-mark"), OpenMode::CreateNew)?;
    mark.write_at(&[0u8; 4096], 0)?;
    mark.sync_all()?;
    rows.push(timed(
        "64 B write in place + `sync_data` (the mark)",
        &mut |i| {
            mark.write_at(&[i as u8; 64], (i % 2) * 2048)?;
            mark.sync_data()
        },
    )?);
    drop(mark);
    rows.push(timed("directory sync (`sync_dir`)", &mut |_| {
        fs.sync_dir(dir)
    })?);

    // Creating a segment ahead of use: 64 MiB of zeros, `sync_all`, directory sync.
    let reps = if short { 1 } else { 3 };
    let mut lat = Vec::new();
    let start = Instant::now();
    for r in 0..reps {
        let p = dir.join(format!("probe-create-{r}.open"));
        let t = Instant::now();
        let f = fs.open(&p, OpenMode::CreateNew)?;
        for i in 0..64u64 {
            f.write_at(&zeros, i << 20)?;
        }
        f.sync_all()?;
        fs.sync_dir(dir)?;
        lat.push(nanos(t.elapsed()));
        drop(f);
        fs.remove_file(&p)?;
    }
    rows.push(CallRow {
        name: "create 64 MiB zero-filled + `sync_all` + directory sync",
        lat: sorted(lat),
        secs: start.elapsed().as_secs_f64(),
    });
    fs.remove_file(&seg_path)?;
    fs.remove_file(&dir.join("probe-mark"))?;
    Ok(rows)
}

// ----- the report -----

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut short = false;
    let mut keep = false;
    let mut any_fs = false;
    let mut only: Option<String> = None;
    let mut repeat = 1u32;
    let mut disk_note: Option<String> = None;
    let mut setup_filter: Option<String> = None;
    let mut sync_delay = Duration::ZERO;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--short" => short = true,
            "--keep" => keep = true,
            "--any-fs" => any_fs = true,
            "--only" => only = args.next(),
            "--disk" => disk_note = args.next(),
            "--setup" => setup_filter = args.next(),
            "--sync-delay" => {
                let ms: f64 = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .expect("--sync-delay takes milliseconds");
                sync_delay = Duration::from_secs_f64(ms / 1e3);
            }
            "--repeat" => {
                repeat = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .expect("--repeat takes a number");
            }
            // `cargo bench` and test harnesses pass this; it means nothing here.
            "--bench" => {}
            _ if a.starts_with("--") => panic!("unknown option {a}"),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let Some(base) = dir else {
        eprintln!(
            "usage: commit_latency <dir> [--short] [--only <load>] [--repeat <n>] [--any-fs] [--keep] [--disk <text>] \
             [--setup <text>] [--sync-delay <ms>]"
        );
        std::process::exit(2);
    };
    let root = base.join("kvik-commit-latency");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the working directory");

    let (fstype, found) = storage(&root);
    let disk = match disk_note {
        Some(note) => format!("{note} (stated); found: {found}"),
        None => found,
    };
    if !any_fs && matches!(fstype.as_str(), "tmpfs" | "ramfs") {
        eprintln!("{fstype} does not sync to a device; choose another directory or pass --any-fs");
        std::process::exit(2);
    }
    let started = Instant::now();
    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(
        w,
        "## Environment\n\n| Item | Value |\n|---|---|\n\
         | Run | {} |\n| Platform | {} {}, {} |\n| CPUs | {} |\n| File system | {fstype} |\n| Disk | {disk} |\n\
         | Sync primitive | {} |\n| Sync delay added | {} |\n| Timer slack | {} |\n| Build | {} |\n\
         | Load average at start | {} |",
        if short { "short" } else { "full" },
        std::env::consts::OS,
        std::env::consts::ARCH,
        os_version(),
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        sync_primitive(),
        if sync_delay.is_zero() {
            "none".to_owned()
        } else {
            format!("{} ms in every `sync_data`", ms(nanos(sync_delay)))
        },
        timer_slack(),
        if cfg!(debug_assertions) {
            "debug (not a valid measurement)"
        } else {
            "release"
        },
        load_average(),
    );
    print!("{out}");
    out.clear();

    // The sync calls on their own.
    let calls = sync_calls(&root.join("probe"), short).expect("time the sync calls");
    let _ = writeln!(
        out,
        "\n## Sync calls on their own\n\n| Call | n | p50 ms | p99 ms | max ms | per s |\n|---|---:|---:|---:|---:|---:|"
    );
    for c in &calls {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {:.0} |",
            c.name,
            c.lat.len(),
            ms(percentile(&c.lat, 50.0)),
            pct(&c.lat, 99.0),
            ms(c.lat.last().copied().unwrap_or(0)),
            c.lat.len() as f64 / c.secs.max(1e-9),
        );
    }
    print!("{out}");
    out.clear();

    let mut runs = Vec::new();
    let mut restarts = Vec::new();
    for rep in 0..repeat {
        for (si, setup) in Setup::ALL.iter().copied().enumerate() {
            if setup_filter
                .as_ref()
                .is_some_and(|f| !setup.label().contains(f.as_str()))
            {
                continue;
            }
            let dir = root.join(format!("store-{rep}-{si}"));
            let sync_log = Arc::new(SyncLog::default());
            let fs: Arc<dyn Fs> = Arc::new(TimingFs {
                log: sync_log.clone(),
                delay: sync_delay,
            });
            let new = NewStore {
                store_id: STORE_ID,
                created_at: wall_ns(),
                site_zone: "Etc/UTC".into(),
                tzdata: "2026a".into(),
                software: "kvik-log commit_latency".into(),
            };
            let store = StoreDir::open(fs.clone(), &dir, Some(&new)).expect("create a store");
            let writer =
                LogWriter::open(store, setup.config(), Some(BOOT), Duration::ZERO, wall_ns())
                    .expect("open the writer");
            let want = if setup.every_commit {
                matches!(writer.stats().mark_sync, MarkSyncMode::EveryCommit { .. })
            } else {
                matches!(writer.stats().mark_sync, MarkSyncMode::Interval(_))
            };
            assert!(
                want,
                "the mark sync mode is the one the row is labelled with"
            );
            let log = Arc::new(
                Log::start(writer, Arc::new(SystemClock::default())).expect("start the writer"),
            );
            // Warm-up: the first call waits for the first segment; then let the next one be created ahead.
            log.submit(vec![reading(1, 0)])
                .expect("warm-up")
                .wait()
                .expect("warm-up acknowledged");
            std::thread::sleep(Duration::from_millis(500));
            for (load, secs) in loads(short) {
                if only
                    .as_ref()
                    .is_some_and(|o| !load.name().contains(o.as_str()))
                {
                    continue;
                }
                let r = run_load(&log, &sync_log, load, secs, setup);
                eprintln!(
                    "{} | {} | {}: {} calls, p50 {} ms",
                    load.name(),
                    setup.interval(),
                    setup.mark(),
                    r.calls,
                    ms(percentile(&r.lat, 50.0))
                );
                runs.push((rep, r));
                std::thread::sleep(Duration::from_millis(300));
            }
            let log = Arc::into_inner(log).expect("every writer thread has finished");
            log.shutdown().expect("clean shutdown");
            restarts.push((rep, restart(&fs, &dir, setup)));
            if !keep {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    let rep_col = repeat > 1;
    let rc = |rep: u32| {
        if rep_col {
            format!(" {} |", rep + 1)
        } else {
            String::new()
        }
    };
    let (rh, rs) = if rep_col {
        (" Run |", "---:|")
    } else {
        ("", "")
    };
    let _ = writeln!(
        out,
        "\n## Commit latency per acknowledged call, ms\n\n\
         | Load | Interval | Mark sync |{rh} Calls | p50 | p90 | p99 | p99.9 | max | Lag p99 | Lag max | Busy |\n\
         |---|---|---|{rs}---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    for (rep, r) in &runs {
        // How late the generator sent, for the open-loop loads: latency counts from the submit call, so lateness
        // would not show in it (coordinated omission) but would bunch arrivals.
        let (lag, lag_max) = if matches!(r.load, Load::BackToBack { .. } | Load::Saturation { .. })
        {
            ("–".into(), "–".into())
        } else {
            (pct(&r.lag, 99.0), ms(r.lag.last().copied().unwrap_or(0)))
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} |{} {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.load.name(),
            r.setup.interval(),
            r.setup.mark(),
            rc(*rep),
            r.lat.len(),
            ms(percentile(&r.lat, 50.0)),
            pct(&r.lat, 90.0),
            pct(&r.lat, 99.0),
            pct(&r.lat, 99.9),
            ms(r.lat.last().copied().unwrap_or(0)),
            lag,
            lag_max,
            r.busy,
        );
    }
    let _ = writeln!(
        out,
        "\n## Throughput and syncs\n\n\
         | Load | Interval | Mark sync |{rh} Seconds | Calls/s | Readings/s | Log syncs/s | Readings per batch | \
         Calls per batch | Interval at end | Sync time at end | Mark syncs | Sealed | Batch sync p50 | p99 | max | \
         Mark sync p50 | max |\n\
         |---|---|---|{rs}---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    for (rep, r) in &runs {
        let kind = |k: SyncKind| {
            sorted(
                r.sync_samples
                    .iter()
                    .filter(|(x, _)| *x == k)
                    .map(|(_, v)| *v)
                    .collect(),
            )
        };
        let seg = kind(SyncKind::SegmentData);
        let mark = kind(SyncKind::Mark);
        let secs = r.secs.max(1e-9);
        let m = |v: &[u64], p: f64| {
            if v.is_empty() {
                "–".into()
            } else {
                ms(percentile(v, p))
            }
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} |{} {:.1} | {:.0} | {:.0} | {:.0} | {:.1} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.load.name(),
            r.setup.interval(),
            r.setup.mark(),
            rc(*rep),
            r.secs,
            r.calls as f64 / secs,
            r.readings as f64 / secs,
            r.syncs as f64 / secs,
            r.readings as f64 / r.batches.max(1) as f64,
            r.calls as f64 / r.batches.max(1) as f64,
            ms(nanos(r.sync_interval)),
            r.sync_time.map_or("–".into(), |t| ms(nanos(t))),
            r.mark_syncs,
            r.sealed,
            m(&seg, 50.0),
            pct(&seg, 99.0),
            m(&seg, 100.0),
            m(&mark, 50.0),
            m(&mark, 100.0),
        );
    }
    let _ = writeln!(
        out,
        "\n## Opening the store again after the run (clean shutdown, warm page cache)\n\n\
         | Interval | Mark sync |{rh} Segments | MiB | Same boot ms | Rule | Another boot ms | Rule |\n\
         |---|---|{rs}---:|---:|---:|---|---:|---|"
    );
    for (rep, r) in &restarts {
        let _ = writeln!(
            out,
            "| {} | {} |{} {} | {} | {} | {} | {} | {} |",
            r.setup.interval(),
            r.setup.mark(),
            rc(*rep),
            r.segments,
            r.bytes >> 20,
            ms(nanos(r.same_boot)),
            r.same_rule,
            ms(nanos(r.other_boot)),
            r.other_rule,
        );
    }
    let failed: u64 = runs.iter().map(|(_, r)| r.failed).sum();
    let _ = writeln!(
        out,
        "\nLoad average at the end: {}. Total time: {:.0} s. Failed calls: {failed}.",
        load_average(),
        started.elapsed().as_secs_f64()
    );
    print!("{out}");
    if !keep {
        let _ = std::fs::remove_dir_all(&root);
    }
    assert_eq!(failed, 0, "every call was acknowledged");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_wait_blocks_until_a_late_wake() {
        let stamp = Arc::new(Stamp::default());
        let waker = Waker::from(stamp.clone());
        let before = Instant::now();
        let waking = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.wake();
        });
        let at = stamp.wait();
        waking.join().expect("the waking thread");
        assert!(at >= before + Duration::from_millis(50));
        assert!(
            before.elapsed() < STAMP_TIMEOUT / 2,
            "the wake notifies the waiter"
        );
    }

    #[test]
    fn stamp_keeps_the_first_wake() {
        let stamp = Arc::new(Stamp::default());
        stamp.set_now();
        let first = stamp.wait();
        std::thread::sleep(Duration::from_millis(5));
        stamp.set_now();
        assert_eq!(stamp.wait(), first);
    }
}
