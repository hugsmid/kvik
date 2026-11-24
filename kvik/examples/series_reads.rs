//! One-series chart reads against one read of every series: a dashboard that charts each series with its own
//! `buckets` call, one `buckets_of` call for all of them, and three such dashboards at once.
//!
//! ```sh
//! cargo run --release -p kvik --example series_reads -- --dir <scratch dir> --days 1 [--hours h] [--shape w0|slice] \
//!     [--runs 3] [--no-many] [--quick] [--profile <calls>]
//! ```
//!
//! The store is `series-reads-<shape>-<days>d` under `--dir`, built once and kept: 37 series (gauges, counters and
//! states in turn) with a reading a second each, event time ending at the build's wall clock, written by one writer
//! with syncs left out (default configuration otherwise, no sync-rate budget) and shut down before the reads. Shape
//! `w0` (the default) is the home load's arrivals, each series on its own phase, the arrivals of each 10 ms in one
//! call; `slice` is 60-second slices of every series, so that every batch holds every series. A fresh reader then refreshes and charts the
//! last 24 hours (`Resolution::Auto { max_points: 1_500 }`): one series alone, every series one call at a time, every
//! series in one `buckets_of` call (left out with `--no-many`, for a tree without it), and three dashboards at once,
//! each on its own thread charting every series one call at a time through the same reader. Each figure is the time,
//! the bytes the process read (`rchar`) and the growth of its peak resident memory (`VmHWM`), the first run on the
//! fresh reader, then the best of `--runs` more. `--profile n` makes n one-series reads and nothing else; `--quick` stops after
//! the one-series and `buckets_of` reads.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::io;

use kvik::env::{DirEntry, Env, Fs, FsFile, ManualClock, OpenMode, StdFs};
use kvik::{
    Kind, Reader, ReaderConfig, Reading, Resolution, SeriesId, SeriesSpec, Span, StateValues,
    Store, StoreConfig, Timestamp, Value, ValueType,
};

const SERIES: usize = 37;
const SLICE: i64 = 60;
const DAY: i64 = 86_400;
const MAX_POINTS: usize = 1_500;

fn block_on<F: Future>(f: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park_timeout(Duration::from_millis(100));
    }
}

struct Args {
    dir: PathBuf,
    days: i64,
    runs: usize,
    many: bool,
    shape: Shape,
    profile: usize,
    quick: bool,
    hours: i64,
}

/// How readings reach the writer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// 60-second slices of every series, one call each: every batch holds every series.
    Slice,
    /// W0 arrivals: each series on its own phase with ±100 ms jitter, 0.5% late by 1–5 s, 0.1% missing; the
    /// arrivals of each 10 ms (the batch-sync ceiling) in one call.
    W0,
}

fn args() -> Args {
    let mut a = Args {
        dir: PathBuf::new(),
        days: 1,
        runs: 3,
        many: true,
        shape: Shape::W0,
        profile: 0,
        quick: false,
        hours: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--dir" => a.dir = PathBuf::from(it.next().expect("a directory")),
            "--days" => a.days = it.next().and_then(|v| v.parse().ok()).expect("days"),
            "--hours" => {
                a.hours = it.next().and_then(|v| v.parse().ok()).expect("hours");
            }
            "--runs" => a.runs = it.next().and_then(|v| v.parse().ok()).expect("runs"),
            "--no-many" => a.many = false,
            "--quick" => a.quick = true,
            "--shape" => {
                a.shape = match it.next().as_deref() {
                    Some("slice") => Shape::Slice,
                    Some("w0") => Shape::W0,
                    other => panic!("unknown shape {other:?}"),
                }
            }
            "--profile" => a.profile = it.next().and_then(|v| v.parse().ok()).expect("calls"),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!a.dir.as_os_str().is_empty(), "--dir is required");
    a
}

fn proc_field(file: &str, key: &str) -> u64 {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn read_bytes() -> u64 {
    proc_field("/proc/self/io", "rchar:")
}

fn peak_kib() -> u64 {
    proc_field("/proc/self/status", "VmHWM:")
}

fn spec(k: usize) -> SeriesSpec {
    let kind = match k % 3 {
        0 => Kind::Gauge {
            value: ValueType::F64,
        },
        1 => Kind::Counter {
            value: ValueType::F64,
        },
        _ => Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into(), "auto".into()]),
        },
    };
    SeriesSpec::new(format!("bench/s{k}/v"), kind, "").expected_interval(Duration::from_secs(1))
}

fn value(k: usize, sec: i64) -> Value {
    match k % 3 {
        0 => Value::F64((sec % 3_600) as f64 + k as f64),
        1 => Value::F64((sec * (k as i64 + 1)) as f64),
        _ => Value::State(((sec / 600 + k as i64) % 3) as u16),
    }
}

/// The operating system's file system with syncs left out, for building the store quickly; reads go through the
/// plain one.
#[derive(Debug)]
struct NoSync(StdFs);

#[derive(Debug)]
struct NoSyncFile(Box<dyn FsFile>);

impl Fs for NoSync {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        Ok(Box::new(NoSyncFile(self.0.open(path, mode)?)))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.0.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.0.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.0.remove_file(path)
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        self.0.free_space(path)
    }
}

impl FsFile for NoSyncFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.0.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.0.len()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.0.try_lock_exclusive()
    }
}

fn config(dir: &Path, clock: Option<&ManualClock>) -> StoreConfig {
    let mut env = Env::system();
    env.fs = Arc::new(NoSync(StdFs));
    if let Some(c) = clock {
        env.clock = Arc::new(c.clone());
    }
    let paced = clock.is_some();
    let mut c = StoreConfig::new(dir.to_string_lossy().into_owned(), "Etc/UTC")
        .create_if_missing(true)
        .env(env);
    c.log.min_sync_interval = Duration::ZERO;
    if paced {
        c.log.segment_size = 16 << 20;
        c.log.seal_interval = SEAL_REAL;
    }
    c
}

/// The writer seals by its monotonic clock, which is real time: a W0 build gives each five minutes of event time
/// [`BLOCK_REAL`] of real time and seals after [`SEAL_REAL`], so that a segment holds about five minutes, as in the household load (W0).
const SEAL_REAL: Duration = Duration::from_millis(560);
const BLOCK_REAL: Duration = Duration::from_millis(600);

/// A small deterministic generator (xorshift), so that every build of a shape is the same store.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// W0 readings of seconds `[from, to)` as (arrival ns, series index, event second), in arrival order.
fn w0_arrivals(rng: &mut Rng, from: i64, to: i64) -> Vec<(i64, usize, i64)> {
    let mut out = Vec::with_capacity(((to - from) as usize) * SERIES);
    for sec in from..to {
        for k in 0..SERIES {
            if rng.below(1_000) == 0 {
                continue;
            }
            let phase = (k as i64 * 1_000_000_000) / SERIES as i64;
            let jitter = rng.below(200_000_000) as i64 - 100_000_000;
            let late = if rng.below(200) == 0 {
                1_000_000_000 + rng.below(4_000_000_000) as i64
            } else {
                0
            };
            out.push((sec * 1_000_000_000 + phase + jitter + late, k, sec));
        }
    }
    out.sort_unstable();
    out
}

/// Builds the store unless it exists; the wall-clock second its event time ends at, stored beside it.
fn build(dir: &Path, span: i64, shape: Shape) -> i64 {
    let end_file = dir.join("end-second");
    if let Ok(s) = std::fs::read_to_string(&end_file) {
        return s.trim().parse().expect("end second");
    }
    let end = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let start = end - span;
    // W0 arrives in real time, so that segments are sealed every five minutes of it, as in the household load (W0).
    let clock = (shape == Shape::W0).then(|| ManualClock::new(Timestamp(start * 1_000_000_000)));
    let store = block_on(Store::open(config(&dir.join("store"), clock.as_ref()))).expect("open");
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|k| block_on(store.define_series(spec(k))).expect("define"))
        .collect();
    let advance = |at: i64| {
        if let Some(c) = &clock {
            let now = c.wall().0;
            if at > now {
                c.advance(Duration::from_nanos((at - now) as u64));
            }
        }
    };
    let t0 = Instant::now();
    let mut batch = Vec::with_capacity(SERIES * SLICE as usize);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut calls = 0u64;
    let mut sec = start;
    let mut block_began = Instant::now();
    while sec < end {
        if clock.is_some() && (sec - start) % 300 == 0 && sec > start {
            let spent = block_began.elapsed();
            if spent < BLOCK_REAL {
                thread::sleep(BLOCK_REAL - spent);
            }
            block_began = Instant::now();
        }
        let to = (sec + SLICE).min(end);
        match shape {
            Shape::Slice => {
                batch.clear();
                for s in sec..to {
                    for (k, id) in ids.iter().enumerate() {
                        batch.push(Reading::new(*id, Timestamp(s * 1_000_000_000), value(k, s)));
                    }
                }
                block_on(store.emit(&batch)).expect("emit");
                calls += 1;
            }
            Shape::W0 => {
                // Arrivals past the slice wait for the next one, as the late ones would.
                let arrivals = w0_arrivals(&mut rng, sec, to);
                let mut window = i64::MIN;
                batch.clear();
                for (at, k, s) in arrivals {
                    let w = at.div_euclid(10_000_000);
                    if w != window && !batch.is_empty() {
                        advance(window * 10_000_000 + 9_999_999);
                        block_on(store.emit(&batch)).expect("emit");
                        calls += 1;
                        batch.clear();
                    }
                    window = w;
                    batch.push(Reading::new(
                        ids[k],
                        Timestamp(s * 1_000_000_000),
                        value(k, s),
                    ));
                }
                if !batch.is_empty() {
                    advance(window * 10_000_000 + 9_999_999);
                    block_on(store.emit(&batch)).expect("emit");
                    calls += 1;
                }
            }
        }
        sec = to;
    }
    block_on(store.shutdown()).expect("shutdown");
    eprintln!(
        "built {span} s in {:.1} s, {calls} calls",
        t0.elapsed().as_secs_f64()
    );
    std::fs::write(&end_file, end.to_string()).expect("end file");
    end
}

/// Resets the process's peak resident memory, so that `VmHWM` counts from here (Linux).
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Time, bytes read and peak growth.
type Sample = (Duration, u64, u64);

/// A query measured: the first run and the runs after it.
type Runs<'a> = &'a dyn Fn(&dyn Fn()) -> (Sample, Vec<Sample>);

/// Time, bytes read and peak growth of `f`.
fn measure(f: impl FnOnce()) -> Sample {
    reset_peak();
    let (b, p) = (read_bytes(), peak_kib());
    let t = Instant::now();
    f();
    let took = t.elapsed();
    (took, read_bytes() - b, peak_kib().saturating_sub(p))
}

fn line(what: &str, first: Sample, again: Vec<Sample>) {
    let best = again.iter().min_by_key(|r| r.0).copied();
    let fmt = |r: Sample| {
        format!(
            "{:8.1} ms {:9.1} MB {:6.1} MiB",
            r.0.as_secs_f64() * 1e3,
            r.1 as f64 / 1e6,
            r.2 as f64 / 1024.0
        )
    };
    println!(
        "{what:<34} first {} | best {}",
        fmt(first),
        best.map_or("-".into(), fmt)
    );
}

fn main() {
    let a = args();
    let name = match a.shape {
        Shape::Slice => "slice",
        Shape::W0 => "w0",
    };
    let (span, label) = if a.hours > 0 {
        (a.hours * 3_600, format!("{}h", a.hours))
    } else {
        (a.days * DAY, format!("{}d", a.days))
    };
    let dir = a.dir.join(format!("series-reads-{name}-{label}"));
    std::fs::create_dir_all(&dir).expect("dir");
    let end = build(&dir, span, a.shape);
    let reader = Arc::new(
        Reader::open(ReaderConfig::new(
            dir.join("store").to_string_lossy().into_owned(),
        ))
        .expect("reader"),
    );
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|k| reader.series(format!("bench/s{k}/v")).expect("a series").id)
        .collect();
    let to = Timestamp((end + 1) * 1_000_000_000);
    let day = to + Span::from_secs(-DAY)..to;
    let auto = Resolution::Auto {
        max_points: MAX_POINTS,
    };
    let one = |id: SeriesId| {
        reader.buckets(id, day.clone(), auto).expect("buckets");
    };
    let each = || ids.iter().for_each(|id| one(*id));
    let runs = |f: &dyn Fn()| -> (Sample, Vec<Sample>) {
        let first = measure(f);
        (first, (0..a.runs).map(|_| measure(f)).collect())
    };
    if a.profile > 0 {
        for _ in 0..a.profile {
            one(ids[SERIES / 2]);
        }
        return;
    }
    println!("store {name}, {label}, {SERIES} series at 1 Hz, last 24 h, auto {MAX_POINTS} points");
    let (f, r) = runs(&|| one(ids[SERIES / 2]));
    line("one series", f, r);
    if a.many {
        many(&reader, &ids, &day, &runs);
    }
    if a.quick {
        return;
    }
    let (f, r) = runs(&each);
    line("37 series one call each", f, r);
    let concurrent = || {
        thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(each);
            }
        });
    };
    let (f, r) = runs(&concurrent);
    line("3 dashboards at once, one call each", f, r);
}

fn many(reader: &Reader, ids: &[SeriesId], day: &std::ops::Range<Timestamp>, runs: Runs<'_>) {
    let auto = Resolution::Auto {
        max_points: MAX_POINTS,
    };
    let (f, r) = runs(&|| {
        reader
            .buckets_of(ids, day.clone(), auto)
            .expect("buckets_of");
    });
    line("37 series in one buckets_of", f, r);
    let (f, r) = runs(&|| {
        thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    reader
                        .buckets_of(ids, day.clone(), auto)
                        .expect("buckets_of")
                });
            }
        });
    });
    line("3 dashboards at once, buckets_of", f, r);
}
