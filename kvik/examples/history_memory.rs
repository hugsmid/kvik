//! Resident memory of a durable store as readings accumulate, and its start time and memory once reopened.
//!
//! ```sh
//! cargo run --release -p kvik --example history_memory -- --dir <scratch dir> \
//!     [--series 2000] [--calls 1500] [--late-every 0] [--sample 100] [--reopen]
//! ```
//!
//! One writer emits `--calls` calls of one reading per series, each call at a new timestamp one second after the
//! last, with `log.min_sync_interval = 0` and no consumer. With `--late-every n`, every n-th call also repeats ten
//! readings of an hour earlier (outside the recent window, so duplicates found by the slow path) and records ten new
//! readings two hours earlier. Prints a line every `--sample` calls: calls, readings, resident memory (`VmRSS`),
//! readings per second since the last line and the slow-path count. With `--reopen` the store is then shut down,
//! opened again and the open's time and resident memory printed; `--open-only` opens an existing store and prints
//! the same. The store is `history-memory-store` under `--dir`; it is removed at the end unless `--keep`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::{
    Kind, Outcome, Reading, SeriesId, SeriesSpec, Store, StoreConfig, Timestamp, Value, ValueType,
};

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
        thread::park();
    }
}

/// Resident memory in MiB, from `/proc/self/status`; 0 elsewhere.
fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map_or(0.0, |kib| kib / 1024.0)
}

struct Args {
    dir: PathBuf,
    series: usize,
    calls: usize,
    late_every: usize,
    sample: usize,
    reopen: bool,
    open_only: bool,
    keep: bool,
}

fn args() -> Args {
    let mut a = Args {
        dir: PathBuf::new(),
        series: 2_000,
        calls: 1_500,
        late_every: 0,
        sample: 100,
        reopen: false,
        open_only: false,
        keep: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--reopen" => a.reopen = true,
            "--open-only" => a.open_only = true,
            "--keep" => a.keep = true,
            _ => {
                let v = it.next().unwrap_or_else(|| panic!("{k} needs a value"));
                let n = || {
                    v.parse::<usize>()
                        .unwrap_or_else(|_| panic!("{k}: a number"))
                };
                match k.as_str() {
                    "--dir" => a.dir = PathBuf::from(&v),
                    "--series" => a.series = n(),
                    "--calls" => a.calls = n(),
                    "--late-every" => a.late_every = n(),
                    "--sample" => a.sample = n().max(1),
                    _ => panic!("unknown argument {k}"),
                }
            }
        }
    }
    assert!(!a.dir.as_os_str().is_empty(), "--dir is required");
    a
}

fn config(dir: &std::path::Path) -> StoreConfig {
    let mut config =
        StoreConfig::new(dir.to_string_lossy().into_owned(), "Etc/UTC").create_if_missing(true);
    config.log.min_sync_interval = Duration::ZERO;
    config
}

fn slow_path(store: &Store) -> u64 {
    store.stats().ingest.slow_path
}

fn open_and_report(dir: &std::path::Path) {
    let before = rss_mib();
    let t = Instant::now();
    let store = block_on(Store::open(config(dir))).expect("open");
    let took = t.elapsed();
    println!(
        "open: {:.2} s, resident {:.0} MiB (before open {:.0} MiB)",
        took.as_secs_f64(),
        rss_mib(),
        before
    );
    block_on(store.shutdown()).expect("shutdown");
}

fn main() {
    let a = args();
    let store_dir = a.dir.join("history-memory-store");
    if a.open_only {
        open_and_report(&store_dir);
        return;
    }
    assert!(!store_dir.exists(), "{} exists", store_dir.display());
    let store = block_on(Store::open(config(&store_dir))).expect("open");
    let ids: Vec<SeriesId> = (0..a.series)
        .map(|i| {
            let spec = SeriesSpec::new(
                format!("bench/s{i}/v"),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "",
            )
            .expected_interval(Duration::from_secs(1));
            block_on(store.define_series(spec)).expect("define")
        })
        .collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let base = (now - 7 * 86_400) * 1_000_000_000;
    let s = 1_000_000_000i64;
    let mut batch = Vec::with_capacity(a.series + 20);
    let (mut recorded, mut duplicates) = (0u64, 0u64);
    let mut last = Instant::now();
    let mut since = 0u64;
    println!("calls readings rss_mib readings_per_s slow_path");
    for c in 0..a.calls {
        let t = base + c as i64 * s;
        batch.clear();
        for (k, id) in ids.iter().enumerate() {
            batch.push(Reading::new(*id, Timestamp(t), Value::F64((c + k) as f64)));
        }
        if a.late_every > 0 && c % a.late_every == a.late_every - 1 && c >= 3_600 {
            for k in 0..10 {
                let back = c - 3_600;
                batch.push(Reading::new(
                    ids[(k * 7) % ids.len()],
                    Timestamp(base + back as i64 * s),
                    Value::F64((back + (k * 7) % ids.len()) as f64),
                ));
                batch.push(Reading::new(
                    ids[(k * 11 + 1) % ids.len()],
                    Timestamp(t - 7_200 * s + 1),
                    Value::F64(k as f64),
                ));
            }
        }
        let ack = block_on(store.emit(&batch)).expect("emit");
        for o in &ack.outcomes {
            match o {
                Outcome::Recorded { .. } => recorded += 1,
                Outcome::Duplicate => duplicates += 1,
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        since += batch.len() as u64;
        if (c + 1) % a.sample == 0 || c + 1 == a.calls {
            let el = last.elapsed().as_secs_f64();
            println!(
                "{} {} {:.0} {:.0} {}",
                c + 1,
                recorded + duplicates,
                rss_mib(),
                since as f64 / el,
                slow_path(&store)
            );
            last = Instant::now();
            since = 0;
        }
    }
    println!("recorded {recorded}, duplicates {duplicates}");
    block_on(store.shutdown()).expect("shutdown");
    drop(store);
    if a.reopen {
        open_and_report(&store_dir);
    }
    if !a.keep {
        std::fs::remove_dir_all(&store_dir).expect("remove the store");
    }
}
