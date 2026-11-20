//! The latest-value and tail budgets of the query design's section 1 on a store with a year of seals (roadmap 9.0e):
//! S1, 400 latest values from one snapshot in the writer's process; S2, the same from another reader handle that
//! stays open; S3, the same as a one-shot command (a reader opened, one snapshot, closed; and the `kvik` binary when
//! `KVIK_BIN` names it); S13, a reading acknowledged until it is in a batch of a reader handle's session, and an
//! empty poll. Another handle reads the files as another process does. Every figure is the best, the median and the
//! worst of its runs; the design's figures are p99 targets per hardware class.
//!
//! The store is on the real file system, at `KVIK_BENCH_DIR` (default under the temporary directory), and is built
//! once: `KVIK_BENCH_SEALS` sealed segments (default 105,120, a year at the default five-minute seal interval) of one
//! reading each, the series in turn, then an open segment holding five minutes at 45 readings a second, one a batch. Run
//! alone, in a release build: `cargo bench -p kvik --bench read`; with `KVIK_BENCH_BUILD_ONLY` set it builds the store
//! and stops.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::{
    Appended, Kind, Live, LiveEvent, LiveOptions, Query, Reader, ReaderConfig, Reading, SeriesId,
    SeriesSpec, SessionOptions, Store, StoreConfig, Timestamp, Value, ValueType,
};

const SERIES: usize = 400;
const RUNS: usize = 50;
const ONE_SHOT_RUNS: usize = 20;
const POLLS: usize = 50;
const READINGS: usize = 60;

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park_timeout(Duration::from_secs(1));
    }
}

fn wall() -> Timestamp {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Timestamp(i64::try_from(ns).unwrap())
}

fn round(store: &Store, ids: &[SeriesId], k: usize) {
    let t = wall();
    let readings: Vec<Reading> = ids
        .iter()
        .map(|id| Reading::new(*id, t, Value::F64(k as f64)))
        .collect();
    block_on(store.emit(&readings)).unwrap();
}

fn count(dir: &Path, ext: &str) -> (usize, u64) {
    let mut n = 0;
    let mut bytes = 0;
    for e in std::fs::read_dir(dir.join("log")).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == ext) {
            n += 1;
            bytes += e.metadata().map_or(0, |m| m.len());
        }
    }
    (n, bytes)
}

fn config(dir: &Path, seal: Duration) -> StoreConfig {
    let mut c = StoreConfig::new(dir, "Etc/UTC").create_if_missing(true);
    c.log.segment_size = 16 << 20;
    c.log.seal_interval = seal;
    c
}

fn ids_of(store: &Store) -> Vec<SeriesId> {
    (0..SERIES)
        .map(|k| {
            let spec = SeriesSpec::new(
                format!("site/d{}/m{}", k / 20, k % 20),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            );
            block_on(store.define_series(spec)).unwrap()
        })
        .collect()
}

/// Builds the store unless a marker beside it says it was built; returns the open store and its series.
fn year(dir: &Path, seals: usize) -> (Store, Vec<SeriesId>) {
    let marker = dir.with_extension("built");
    if !marker.exists() {
        // A build that was stopped continues where it stopped.
        let store = block_on(Store::open(config(dir, Duration::from_millis(1)))).unwrap();
        let ids = ids_of(&store);
        let began = Instant::now();
        let mut k = 0;
        loop {
            for _ in 0..2_000 {
                let r = Reading::new(ids[k % SERIES], wall(), Value::F64(k as f64));
                block_on(store.emit(&[r])).unwrap();
                k += 1;
                thread::sleep(Duration::from_millis(2));
            }
            let (n, _) = count(dir, "kvl");
            eprintln!(
                "building: {n} sealed segments after {k} rounds, {:.0} s",
                began.elapsed().as_secs_f64()
            );
            if n >= seals {
                break;
            }
        }
        block_on(store.shutdown()).unwrap();
        // The open segment: five minutes at 45 readings a second, one reading a batch.
        let store = block_on(Store::open(config(dir, Duration::from_secs(300)))).unwrap();
        let ids = ids_of(&store);
        for j in 0..13_500 {
            let r = Reading::new(ids[j % SERIES], wall(), Value::F64(j as f64));
            block_on(store.emit(&[r])).unwrap();
        }
        block_on(store.shutdown()).unwrap();
        std::fs::write(&marker, b"").unwrap();
    }
    let store = block_on(Store::open(config(dir, Duration::from_secs(300)))).unwrap();
    let ids = ids_of(&store);
    (store, ids)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn report(what: &str, unit: f64, suffix: &str, mut runs: Vec<Duration>) {
    runs.sort();
    let at = |q: usize| runs[(runs.len() * q / 100).min(runs.len() - 1)].as_secs_f64() * unit;
    eprintln!(
        "{what}: best {:.3} {suffix}, median {:.3} {suffix}, worst {:.3} {suffix} ({} runs)",
        at(0),
        at(50),
        runs.last().unwrap().as_secs_f64() * unit,
        runs.len()
    );
}

/// A snapshot and the live state of every series from it.
fn latest_400(reader: &Reader, ids: &[SeriesId]) -> Duration {
    let began = Instant::now();
    let snap = reader.snapshot().unwrap();
    let live = ids
        .iter()
        .filter(|id| matches!(snap.live(**id), Ok(Live::Value(_))))
        .count();
    let took = began.elapsed();
    assert_eq!(live, ids.len());
    took
}

fn timed_latest(what: &str, store: &Store, ids: &[SeriesId], reader: &Reader, new: bool) {
    let runs = (0..RUNS)
        .map(|k| {
            if new {
                round(store, ids, 1_000_000 + k);
            }
            thread::sleep(Duration::from_millis(20));
            latest_400(reader, ids)
        })
        .collect();
    report(what, 1e3, "ms", runs);
}

fn one_shot(dir: &Path, ids: &[SeriesId]) {
    let runs = (0..ONE_SHOT_RUNS)
        .map(|_| {
            let began = Instant::now();
            let r = Reader::open(ReaderConfig::new(dir)).unwrap();
            latest_400(&r, ids);
            drop(r);
            began.elapsed()
        })
        .collect();
    report(
        "S3, a reader opened, one snapshot of 400, closed",
        1e3,
        "ms",
        runs,
    );
    let Some(bin) = std::env::var_os("KVIK_BIN") else {
        eprintln!("S3, the kvik binary: not run (set KVIK_BIN)");
        return;
    };
    let runs = (0..ONE_SHOT_RUNS)
        .map(|_| {
            let began = Instant::now();
            let out = std::process::Command::new(&bin)
                .args(["latest", "--format", "csv", "--store"])
                .arg(dir)
                .output()
                .unwrap();
            let took = began.elapsed();
            assert!(out.status.success(), "{out:?}");
            assert_eq!(
                out.stdout.iter().filter(|b| **b == b'\n').count(),
                SERIES + 1
            );
            took
        })
        .collect();
    report("S3, `kvik latest` as a process", 1e3, "ms", runs);
}

fn empty_polls(what: &str, reader: &Reader, queries: &[Query]) {
    let opts = SessionOptions::new(Duration::from_millis(20), Duration::from_millis(10));
    let mut session = reader.session_with(opts).unwrap();
    for q in queries {
        session.open(q.clone(), LiveOptions::default()).unwrap();
    }
    assert!(session.next(Duration::ZERO).unwrap().is_some());
    let mut means = Vec::new();
    let mut singles = Vec::new();
    for _ in 0..5 {
        let mut total = Duration::ZERO;
        for _ in 0..POLLS {
            thread::sleep(Duration::from_millis(21));
            let began = Instant::now();
            let b = session.next(Duration::ZERO).unwrap();
            let took = began.elapsed();
            assert!(b.is_none(), "{b:?}");
            total += took;
            singles.push(took);
        }
        means.push(total / POLLS as u32);
    }
    report(
        &format!("S13 empty poll, {what}, mean of {POLLS}"),
        1e6,
        "µs",
        means,
    );
    report(
        &format!("S13 empty poll, {what}, single polls"),
        1e6,
        "µs",
        singles,
    );
}

fn visibility(store: &Store, id: SeriesId, dir: &Path, poll: Duration) {
    let (tx, rx) = mpsc::channel::<(f64, Instant)>();
    let reader = Reader::open(ReaderConfig::new(dir)).unwrap();
    let least = poll.max(Duration::from_millis(20));
    let watcher = thread::spawn(move || {
        let mut session = reader
            .session_with(SessionOptions::new(least, poll))
            .unwrap();
        let sel = kvik::Select::Ids(vec![id]).into();
        session
            .open(Query::Tail { sel }, LiveOptions::default())
            .unwrap();
        let mut seen = 0;
        while seen < READINGS {
            let Some(b) = session.next(Duration::from_secs(5)).unwrap() else {
                continue;
            };
            let at = Instant::now();
            for (_, e) in &b.events {
                if let LiveEvent::Appended(Appended::Facts(f)) = e {
                    for fact in f {
                        seen += 1;
                        tx.send((fact.number().unwrap(), at)).unwrap();
                    }
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(300));
    let quiet = (poll * 2).max(Duration::from_millis(30)) + Duration::from_millis(20);
    let mut acked = Vec::new();
    let mut commit = Vec::new();
    for k in 0..READINGS {
        thread::sleep(quiet + Duration::from_millis((k as u64 * 7) % 13));
        let began = Instant::now();
        block_on(store.emit(&[Reading::new(id, wall(), Value::F64(k as f64))])).unwrap();
        let at = Instant::now();
        commit.push(at - began);
        acked.push(at);
    }
    let mut visible = Vec::new();
    for _ in 0..READINGS {
        let (v, at) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        visible.push(at.saturating_duration_since(acked[v as usize]));
    }
    watcher.join().unwrap();
    let what = format!("S13 acknowledged to visible, poll {} ms", poll.as_millis());
    report(&format!("{what}, commit latency"), 1e3, "ms", commit);
    report(&what, 1e3, "ms", visible);
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        return;
    }
    let dir: PathBuf = std::env::var_os("KVIK_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("kvik-read-year"));
    let dir = dir.as_path();
    let seals: usize = std::env::var("KVIK_BENCH_SEALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(105_120);
    let (store, ids) = year(dir, seals);
    if std::env::var_os("KVIK_BENCH_BUILD_ONLY").is_some() {
        block_on(store.shutdown()).unwrap();
        return;
    }
    let (sealed, sealed_bytes) = count(dir, "kvl");
    let (open, open_bytes) = count(dir, "open");
    eprintln!(
        "store: {sealed} sealed segments ({:.1} MB), {open} open ({:.2} MB)",
        sealed_bytes as f64 / 1e6,
        open_bytes as f64 / 1e6
    );

    let own = store.reader();
    let began = Instant::now();
    own.snapshot().unwrap();
    eprintln!(
        "S1, first snapshot of the writer's reader: {:.3} ms",
        ms(began.elapsed())
    );
    timed_latest(
        "S1, the writer's process, nothing since",
        &store,
        &ids,
        &own,
        false,
    );
    timed_latest(
        "S1, the writer's process, a round since",
        &store,
        &ids,
        &own,
        true,
    );

    let began = Instant::now();
    let other = Reader::open(ReaderConfig::new(dir)).unwrap();
    latest_400(&other, &ids);
    eprintln!(
        "S2, first snapshot of a reader just opened: {:.3} ms",
        ms(began.elapsed())
    );
    timed_latest(
        "S2, another handle, nothing since",
        &store,
        &ids,
        &other,
        false,
    );
    timed_latest(
        "S2, another handle, a round since",
        &store,
        &ids,
        &other,
        true,
    );

    one_shot(dir, &ids);

    let all = kvik::Selection::all;
    let tail = Query::Tail { sel: all() };
    empty_polls("another handle, Tail", &other, std::slice::from_ref(&tail));
    empty_polls(
        "another handle, Tail and Latest of 400",
        &other,
        &[tail.clone(), Query::Latest(all())],
    );
    empty_polls("the writer's process, Tail", &store.reader(), &[tail]);
    for poll in [Duration::from_millis(10), Duration::from_millis(100)] {
        visibility(&store, ids[0], dir, poll);
    }
    drop(other);
    drop(own);
    block_on(store.shutdown()).unwrap();
}
