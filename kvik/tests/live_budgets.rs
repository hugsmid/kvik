//! The tail-while-writing budget S13 of the query design (roadmap 9.0d), timed: an empty poll of a live session (target
//! 50 µs, 200 µs on the minimal class) and a reading acknowledged by the writer until it is in a batch of a reader
//! process's session (target: commit latency plus the poll interval, 10 ms at least and 100 ms by default). The store
//! is on the real file system under the temporary directory, with a seal after every few rounds of readings. Run in a
//! release build, alone: `cargo test --release -p kvik --test live_budgets -- --ignored --nocapture`.

mod common;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::block_on;
use kvik::{
    Appended, Kind, LiveEvent, LiveOptions, Query, Reader, ReaderConfig, Reading, SeriesId,
    SeriesSpec, SessionOptions, Store, StoreConfig, Timestamp, Value, ValueType,
};

const SERIES: usize = 400;
const ROUNDS: usize = 300;
const RUNS: usize = 5;
const POLLS: usize = 50;
const READINGS: usize = 60;

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

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn best_and_spread(what: &str, unit: &str, runs: &[f64]) {
    let best = runs.iter().copied().fold(f64::INFINITY, f64::min);
    let worst = runs.iter().copied().fold(0.0, f64::max);
    eprintln!(
        "{what}: best {best:.3} {unit}, spread {:.3} {unit}, runs {runs:.3?}",
        worst - best
    );
}

/// Times empty polls of a session holding `queries`: each after the intervals have passed, so that it reads the
/// mark (or, in the writer's process, finds the writer's progress unchanged) and finds nothing. The mean of `POLLS`
/// per run, best and spread of `RUNS`, in microseconds.
fn empty_polls(what: &str, reader: &Reader, queries: &[Query]) {
    let opts = SessionOptions::new(Duration::from_millis(20), Duration::from_millis(10));
    let mut session = reader.session_with(opts).unwrap();
    for q in queries {
        session.open(q.clone(), LiveOptions::default()).unwrap();
    }
    assert!(session.next(Duration::ZERO).unwrap().is_some());
    let mut runs = Vec::new();
    let mut worst = Duration::ZERO;
    for _ in 0..RUNS {
        let mut total = Duration::ZERO;
        for _ in 0..POLLS {
            std::thread::sleep(Duration::from_millis(21));
            let began = Instant::now();
            let b = session.next(Duration::ZERO).unwrap();
            let took = began.elapsed();
            assert!(b.is_none(), "{b:?}");
            total += took;
            worst = worst.max(took);
        }
        runs.push(total.as_secs_f64() * 1e6 / POLLS as f64);
    }
    best_and_spread(&format!("S13 empty poll, {what}"), "µs", &runs);
    eprintln!(
        "S13 empty poll, {what}: slowest single poll {:.1} µs",
        worst.as_secs_f64() * 1e6
    );
}

/// Times readings from acknowledgement to their batch in a session of a reader on `dir` polling every `poll`:
/// `READINGS` readings, each after a quiet spell longer than the batch interval. Returns the commit latencies and
/// the latencies to visible, sorted.
fn visibility(
    store: &Store,
    id: SeriesId,
    dir: &PathBuf,
    poll: Duration,
    quiet: Duration,
) -> (Vec<Duration>, Vec<Duration>) {
    let (tx, rx) = mpsc::channel::<(f64, Instant)>();
    let reader = Reader::open(ReaderConfig::new(dir)).unwrap();
    let least = poll.max(Duration::from_millis(20));
    let watcher = std::thread::spawn(move || {
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
    std::thread::sleep(Duration::from_millis(300));
    let mut acked = Vec::new();
    let mut commit = Vec::new();
    for k in 0..READINGS {
        std::thread::sleep(quiet + Duration::from_millis((k as u64 * 7) % 13));
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
    commit.sort();
    visible.sort();
    (commit, visible)
}

fn pct(v: &[Duration], p: usize) -> Duration {
    v[(v.len() * p / 100).min(v.len() - 1)]
}

#[test]
#[ignore = "a measurement"]
fn tail_while_writing_timed() {
    let dir: PathBuf = std::env::temp_dir().join(format!("kvik-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = StoreConfig::new(&dir, "Etc/UTC").create_if_missing(true);
    config.log.segment_size = 16 << 20;
    config.log.seal_interval = Duration::from_millis(50);
    let store = block_on(Store::open(config)).unwrap();
    let ids: Vec<SeriesId> = (0..SERIES)
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
        .collect();
    for k in 0..ROUNDS {
        round(&store, &ids, k);
        std::thread::sleep(Duration::from_millis(5));
    }
    let all = || kvik::Selection::all();
    let tail = Query::Tail { sel: all() };
    let latest = Query::Latest(all());
    let other = Reader::open(ReaderConfig::new(&dir)).unwrap();
    empty_polls(
        "a reader process, Tail",
        &other,
        std::slice::from_ref(&tail),
    );
    empty_polls(
        "a reader process, Tail and Latest of 400 series",
        &other,
        &[tail.clone(), latest.clone()],
    );
    empty_polls(
        "the writer's process, Tail",
        &store.reader(),
        &[tail.clone()],
    );
    empty_polls(
        "the writer's process, Tail and Latest of 400 series",
        &store.reader(),
        &[tail, latest],
    );

    for (poll, quiet) in [
        (Duration::from_millis(10), Duration::from_millis(30)),
        (Duration::from_millis(100), Duration::from_millis(220)),
    ] {
        let mut p50 = Vec::new();
        let mut p99 = Vec::new();
        let mut commits = Vec::new();
        let mut worst = Duration::ZERO;
        let runs = if poll < Duration::from_millis(50) {
            RUNS
        } else {
            1
        };
        for _ in 0..runs {
            let (commit, visible) = visibility(&store, ids[0], &dir, poll, quiet);
            p50.push(ms(pct(&visible, 50)));
            p99.push(ms(pct(&visible, 99)));
            commits.push(ms(pct(&commit, 99)));
            worst = worst.max(*visible.last().unwrap());
        }
        let what = format!("S13 acknowledged to visible, poll {} ms", poll.as_millis());
        best_and_spread(&format!("{what}, p50"), "ms", &p50);
        best_and_spread(&format!("{what}, p99"), "ms", &p99);
        best_and_spread(&format!("{what}, commit latency p99"), "ms", &commits);
        eprintln!("{what}: slowest {:.3} ms", ms(worst));
    }

    drop(other);
    block_on(store.shutdown()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
