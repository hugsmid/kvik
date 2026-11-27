//! Ingest latency through `Store::emit` (Q-2184): each call from the emit to its acknowledgement, at a home rate
//! (one reading a call, `KVIK_BENCH_RATE` calls a second, default 45) and at saturation (batches of `BATCH`
//! readings over `SERIES` series, back to back). Prints the best, median, p99 and worst per call, and the
//! saturated rate. The store is on the real file system under `KVIK_BENCH_DIR` (default the temporary directory),
//! made afresh. Run alone, in a release build: `cargo bench -p kvik --bench emit`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::{Kind, Reading, SeriesId, SeriesSpec, Store, StoreConfig, Timestamp, Value, ValueType};

const SERIES: usize = 400;
const BATCH: usize = 100;

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

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn report(name: &str, mut v: Vec<Duration>) {
    v.sort_unstable();
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    println!(
        "{name}: {} calls, best {:?}, median {:?}, p99 {:?}, worst {:?}",
        v.len(),
        v[0],
        at(0.5),
        at(0.99),
        v[v.len() - 1]
    );
}

fn main() {
    let base = std::env::var_os("KVIK_BENCH_DIR").map_or_else(
        || std::env::temp_dir().join("kvik-bench-emit"),
        PathBuf::from,
    );
    let _ = std::fs::remove_dir_all(&base);
    let config = StoreConfig::new(&base, "Etc/UTC").create_if_missing(true);
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

    let rate = env_or("KVIK_BENCH_RATE", 45).max(1);
    let calls = env_or("KVIK_BENCH_CALLS", 900) as usize;
    let gap = Duration::from_secs(1) / u32::try_from(rate).unwrap_or(u32::MAX);
    let mut home = Vec::with_capacity(calls);
    let mut next = Instant::now();
    for k in 0..calls {
        thread::sleep(next.saturating_duration_since(Instant::now()));
        next += gap;
        let r = Reading::new(ids[k % SERIES], wall(), Value::F64(k as f64));
        let t = Instant::now();
        block_on(store.emit(&[r])).unwrap();
        home.push(t.elapsed());
    }
    report("home", home);

    let secs = env_or("KVIK_BENCH_SATURATE_SECS", 10);
    let mut sat = Vec::new();
    let start = Instant::now();
    let mut k = 0usize;
    while start.elapsed() < Duration::from_secs(secs) {
        let t0 = wall();
        let rs: Vec<Reading> = (0..BATCH)
            .map(|j| {
                k += 1;
                Reading::new(
                    ids[k % SERIES],
                    Timestamp(t0.0 + j as i64),
                    Value::F64(k as f64),
                )
            })
            .collect();
        let t = Instant::now();
        block_on(store.emit(&rs)).unwrap();
        sat.push(t.elapsed());
    }
    let n = sat.len();
    let took = start.elapsed().as_secs_f64();
    report("saturation", sat);
    println!(
        "saturation: {:.0} calls/s, {:.0} readings/s",
        n as f64 / took,
        (n * BATCH) as f64 / took
    );
    block_on(store.shutdown()).unwrap();
    let _ = std::fs::remove_dir_all(&base);
}
