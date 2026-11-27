//! Chart reads served by the summary tree (design 7.1b section 6, budget S7): the time of a chart after a reader
//! opens and warm, on a store whose tree was checkpointed at a clean shutdown.
//!
//! ```sh
//! cargo run --release -p kvik --example tree_reads -- --dir <scratch dir> [--shape years --years 1 [--merge]] \
//!     [--shape day37 --days 1] [--runs 3]
//! ```
//!
//! Shape `years`: one gauge, a reading every 60 s (so it stores the 15-minute level) for `--years`, event time ending
//! at the build's wall clock; charts of ten years (when built), two years, a year, a week, a day and ten minutes at
//! `Auto { max_points: 2_000 }`. `--merge` merges the tree's day parts up the ladder (day, week, 13 and 52 weeks)
//! with roll-ups before reading, as the writer's merges will. Shape `day37`: 37 series (gauges, counters and states)
//! at a reading a second for `--days`; a year and a week chart of every series in one `buckets_of` call. The store is
//! built once under `--dir` and kept; the writer is shut down with its reader open, so that it checkpoints the tree.
//! Each line: the chart, the first read on a fresh reader (its open and refresh separately), then the best of
//! `--runs` more, with the bytes the process read (`rchar`) and its peak resident memory (`VmHWM`).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::env::{Env, ManualClock, StdFs};
use kvik::{
    Kind, Reader, ReaderConfig, Reading, Resolution, SeriesId, SeriesSpec, Span, StateValues,
    Store, StoreConfig, Timestamp, Value, ValueType,
};

const NS: i64 = 1_000_000_000;
const DAY: i64 = 86_400;
const POINTS: usize = 2_000;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Years,
    Day37,
}

struct Args {
    dir: PathBuf,
    shape: Shape,
    years: i64,
    days: i64,
    merge: bool,
    runs: usize,
}

fn args() -> Args {
    let mut a = Args {
        dir: PathBuf::new(),
        shape: Shape::Years,
        years: 1,
        days: 1,
        merge: false,
        runs: 3,
    };
    let mut it = std::env::args().skip(1);
    let num = |v: Option<String>| v.and_then(|v| v.parse().ok()).expect("a number");
    while let Some(k) = it.next() {
        match k.as_str() {
            "--dir" => a.dir = PathBuf::from(it.next().expect("a directory")),
            "--shape" => {
                a.shape = match it.next().as_deref() {
                    Some("years") => Shape::Years,
                    Some("day37") => Shape::Day37,
                    other => panic!("unknown shape {other:?}"),
                }
            }
            "--years" => a.years = num(it.next()),
            "--days" => a.days = num(it.next()),
            "--runs" => a.runs = num(it.next()) as usize,
            "--merge" => a.merge = true,
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!a.dir.as_os_str().is_empty(), "--dir is required");
    a
}

fn proc_field(file: &str, key: &str) -> u64 {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn spec(shape: Shape, k: usize) -> SeriesSpec {
    let kind = match (shape, k % 3) {
        (Shape::Years, _) | (_, 0) => Kind::Gauge {
            value: ValueType::F64,
        },
        (_, 1) => Kind::Counter {
            value: ValueType::F64,
        },
        _ => Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into(), "auto".into()]),
        },
    };
    let every = if shape == Shape::Years { 60 } else { 1 };
    SeriesSpec::new(format!("bench/s{k}/v"), kind, "").expected_interval(Duration::from_secs(every))
}

fn value(k: usize, sec: i64) -> Value {
    match k % 3 {
        0 => Value::F64((sec % 3_600) as f64 + k as f64),
        1 => Value::F64((sec / 10) as f64),
        _ => Value::State(((sec / 600) % 3) as u16),
    }
}

fn config(dir: &Path, clock: &ManualClock) -> StoreConfig {
    let mut env = Env::system();
    env.fs = Arc::new(StdFs);
    env.clock = Arc::new(clock.clone());
    let mut c = StoreConfig::new(dir.to_string_lossy().into_owned(), "Etc/UTC")
        .create_if_missing(true)
        .env(env);
    c.log.min_sync_interval = Duration::ZERO;
    c
}

/// Builds the store unless it exists; the second its event time ends at.
fn build(dir: &Path, a: &Args, series: usize, span: i64, every: i64) -> i64 {
    let end_file = dir.join("end-second");
    if let Ok(s) = std::fs::read_to_string(&end_file) {
        return s.trim().parse().expect("end second");
    }
    let end = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let clock = ManualClock::new(Timestamp((end + 1) * NS));
    let store = block_on(Store::open(config(&dir.join("store"), &clock))).expect("open");
    let ids: Vec<SeriesId> = (0..series)
        .map(|k| block_on(store.define_series(spec(a.shape, k))).expect("define"))
        .collect();
    let t0 = Instant::now();
    let mut batch = Vec::with_capacity(10_000);
    let mut sec = end - span;
    while sec < end {
        for (k, id) in ids.iter().enumerate() {
            batch.push(Reading::new(*id, Timestamp(sec * NS), value(k, sec)));
        }
        sec += every;
        if batch.len() >= 10_000 || sec >= end {
            block_on(store.emit(&batch)).expect("emit");
            batch.clear();
        }
    }
    let written = t0.elapsed();
    // The reader open, so that the clean shutdown checkpoints the tree (Q-1950).
    let t1 = Instant::now();
    let _ = store.reader().buckets(
        ids[0],
        Timestamp((end - 60) * NS)..Timestamp((end + 1) * NS),
        Resolution::Auto { max_points: 10 },
    );
    let opened = t1.elapsed();
    let t2 = Instant::now();
    block_on(store.shutdown()).expect("shutdown");
    println!(
        "build: written {:.1} s, reader opened {:.1} s, shutdown with checkpoint {:.1} s",
        written.as_secs_f64(),
        opened.as_secs_f64(),
        t2.elapsed().as_secs_f64()
    );
    std::fs::write(&end_file, end.to_string()).expect("end file");
    end
}

/// Merges the tree's parts as far as the planner goes, roll-ups included.
fn merge(store: &Path) -> usize {
    use kvik_tree::part::Part;
    use kvik_tree::read::newest_root;
    use kvik_tree::set::{Policy, merge, next_root, plan};
    use kvik_tree::write::{Identity, Opening, TreeFiles};
    let fs: Arc<dyn kvik_log::fs::Fs> = Arc::new(kvik_log::fs::StdFs);
    let root = newest_root(&*fs, store, |_| true).expect("a root");
    let id = Identity {
        generation: root.header.generation,
        reset: root.reset,
        store_id: root.header.store_id,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i64
        + 30 * DAY * NS;
    let at = Opening {
        mark: root.checkpoint,
        fresh: false,
        wall: now,
        now: Duration::ZERO,
    };
    let (mut files, _) = TreeFiles::start(fs, store, id, at, |_| false).expect("start");
    let policy = Policy {
        late_window: 0,
        minute_keep: i64::MAX / 4,
    };
    let params = kvik_tree::Params::new(kvik_tree::Kind::Gauge, None);
    // Decoded once; each merge is given the live parts that overlap its periods, as the root it checks against.
    let mut decoded: std::collections::BTreeMap<u64, Part> = files
        .read_parts()
        .into_iter()
        .map(|(r, p)| (r.file_id, p.expect("a part")))
        .collect();
    let mut n = 0;
    while let Some(p) = plan(files.root(), now, &policy).into_iter().next() {
        let root = files.root().clone();
        let mut near = root.clone();
        near.parts.retain(|r| {
            p.inputs.contains(&r.file_id)
                || p.periods.iter().any(|&(s, l)| {
                    r.period_start < s + l as i64 && s < r.period_start + r.period_len as i64
                })
        });
        let parts: Vec<Part> = near
            .parts
            .iter()
            .map(|r| decoded[&r.file_id].clone())
            .collect();
        let mut out = merge(&near, &parts, &p, &root.header, |_| Some(params)).expect("merge");
        for o in &mut out {
            o.header.file_id = files.take_file_id();
        }
        for i in &p.inputs {
            decoded.remove(i);
        }
        for o in &out {
            decoded.insert(o.header.file_id, o.clone());
        }
        files
            .publish(next_root(&root, &p, &[], now), out, now, Duration::ZERO)
            .expect("publish");
        n += 1;
    }
    let root = files.root();
    println!(
        "merged: {n} merges, {} live parts, {} covered ranges",
        root.parts.len(),
        root.covered.len()
    );
    n
}

fn main() {
    let a = args();
    let (series, span, every, name) = match a.shape {
        Shape::Years => (1, a.years * 365 * DAY, 60, format!("years-{}", a.years)),
        Shape::Day37 => (37, a.days * DAY, 1, format!("day37-{}", a.days)),
    };
    let dir = a.dir.join(format!("tree-reads-{name}"));
    std::fs::create_dir_all(&dir).expect("dir");
    let end = build(&dir, &a, series, span, every);
    let store = dir.join("store");
    if a.merge {
        merge(&store);
    }
    let to = Timestamp((end + 1) * NS);
    let mut charts: Vec<(&str, i64)> = Vec::new();
    if a.shape == Shape::Years {
        if a.years >= 10 {
            charts.push(("ten years", 10 * 365 * DAY));
        }
        if a.years >= 2 {
            charts.push(("two years", 2 * 365 * DAY));
        }
    }
    charts.extend([("year", 365 * DAY), ("week", 7 * DAY)]);
    if a.shape == Shape::Years {
        charts.extend([("day", DAY), ("ten minutes", 600)]);
    }
    let auto = Resolution::Auto { max_points: POINTS };
    for (label, len) in charts {
        let range = to + Span::from_secs(-len)..to;
        let t0 = Instant::now();
        let reader =
            Reader::open(ReaderConfig::new(store.to_string_lossy().into_owned())).expect("reader");
        let ids: Vec<SeriesId> = (0..series)
            .map(|k| reader.series(format!("bench/s{k}/v")).expect("a series").id)
            .collect();
        let opened = t0.elapsed();
        let read0 = proc_field("/proc/self/io", "rchar:");
        let t1 = Instant::now();
        let charts = reader.buckets_of(&ids, range.clone(), auto).expect("chart");
        let first = t1.elapsed();
        let first_read = proc_field("/proc/self/io", "rchar:") - read0;
        let mut best = Duration::MAX;
        let mut best_read = 0;
        for _ in 0..a.runs {
            let r0 = proc_field("/proc/self/io", "rchar:");
            let t = Instant::now();
            reader.buckets_of(&ids, range.clone(), auto).expect("chart");
            let e = t.elapsed();
            if e < best {
                best = e;
                best_read = proc_field("/proc/self/io", "rchar:") - r0;
            }
        }
        let points = match &charts[0] {
            kvik::ChartData::Buckets { buckets, .. } => buckets.len(),
            kvik::ChartData::Facts(f) => f.len(),
            _ => 0,
        };
        println!(
            "{name} {label}: open+refresh {:.1} ms; first {:.1} ms ({:.1} MB read); warm best of {} {:.2} ms ({:.1} MB read); {points} points a series; peak {} MiB",
            opened.as_secs_f64() * 1e3,
            first.as_secs_f64() * 1e3,
            first_read as f64 / 1e6,
            a.runs,
            best.as_secs_f64() * 1e3,
            best_read as f64 / 1e6,
            proc_field("/proc/self/status", "VmHWM:") / 1024,
        );
    }
}
