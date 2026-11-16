//! Readings per second and CPU of `Store::emit` on the durable backend, with no consumer registered.
//!
//! ```sh
//! cargo run --release -p kvik --example emit_throughput -- --dir <scratch dir> \
//!     [--writers 1] [--per-call 2000] [--series 2000] [--calls 1500]
//! ```
//!
//! Each writer thread holds its own `Store` clone and emits `--calls` calls of `--per-call` readings, each call at
//! one new timestamp, series taken in turn from the writer's share (a gauge, every twentieth a boolean state;
//! expected interval 1 s), with `log.min_sync_interval = 0`. Prints one line: calls/s, readings/s, cores used
//! (process CPU over wall time, from `/proc/self/stat`; `-` elsewhere) and CPU per reading. The store is created in
//! `emit-throughput-store` under `--dir`, which must not exist yet, and only that is removed afterwards.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::{
    Kind, Outcome, Reading, SeriesSpec, StateValues, Store, StoreConfig, Timestamp, Value,
    ValueType,
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

struct Args {
    dir: PathBuf,
    writers: usize,
    per_call: usize,
    series: usize,
    calls: usize,
}

fn args() -> Args {
    let mut a = Args {
        dir: PathBuf::new(),
        writers: 1,
        per_call: 2_000,
        series: 2_000,
        calls: 1_500,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        let n = || {
            v.parse::<usize>()
                .unwrap_or_else(|_| panic!("{k}: a number"))
        };
        match k.as_str() {
            "--dir" => a.dir = PathBuf::from(&v),
            "--writers" => a.writers = n(),
            "--per-call" => a.per_call = n(),
            "--series" => a.series = n(),
            "--calls" => a.calls = n(),
            _ => panic!("unknown argument {k}"),
        }
    }
    assert!(!a.dir.as_os_str().is_empty(), "--dir is required");
    assert!(a.writers > 0 && a.per_call > 0 && a.series >= a.writers);
    assert!(
        a.per_call <= a.series / a.writers,
        "--per-call is at most a writer's share of the series ({})",
        a.series / a.writers
    );
    a
}

/// User plus system CPU of the process, in seconds; `None` without `/proc`.
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the command name, which is in parentheses and may hold spaces.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    // USER_HZ is 100 on every Linux target in use.
    Some(ticks as f64 / 100.0)
}

fn main() {
    let a = args();
    let store_dir = a.dir.join("emit-throughput-store");
    assert!(
        !store_dir.exists(),
        "{} exists: remove it or choose another --dir",
        store_dir.display()
    );
    let mut config = StoreConfig::new(store_dir.to_string_lossy().into_owned(), "Etc/UTC")
        .create_if_missing(true);
    config.log.min_sync_interval = Duration::ZERO;
    let store = block_on(Store::open(config)).expect("open");
    let mut ids = Vec::with_capacity(a.series);
    for i in 0..a.series {
        let kind = if i % 20 == 19 {
            Kind::State {
                values: StateValues::Bool,
            }
        } else {
            Kind::Gauge {
                value: ValueType::F64,
            }
        };
        let spec = SeriesSpec::new(format!("bench/s{i}/v"), kind, "")
            .expected_interval(Duration::from_secs(1));
        ids.push((
            block_on(store.define_series(spec)).expect("define"),
            i % 20 == 19,
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let base = (now - 7 * 86_400) * 1_000_000_000;
    let share = a.series / a.writers;
    let barrier = Arc::new(Barrier::new(a.writers + 1));
    let handles: Vec<_> = (0..a.writers)
        .map(|w| {
            let store = store.clone();
            let mine: Vec<_> = ids[w * share..(w + 1) * share].to_vec();
            let barrier = barrier.clone();
            let (calls, per_call) = (a.calls, a.per_call);
            thread::spawn(move || {
                let mut batch = Vec::with_capacity(per_call);
                let mut next = 0usize;
                barrier.wait();
                for c in 0..calls {
                    let t = Timestamp(base + c as i64 * 1_000_000_000);
                    batch.clear();
                    for _ in 0..per_call {
                        let (id, state) = mine[next % mine.len()];
                        next += 1;
                        let v = if state {
                            Value::Bool(c % 2 == 0)
                        } else {
                            Value::F64(c as f64 * 0.5)
                        };
                        batch.push(Reading::new(id, t, v));
                    }
                    let ack = block_on(store.emit(&batch)).expect("emit");
                    assert!(
                        ack.outcomes
                            .iter()
                            .all(|o| matches!(o, Outcome::Recorded { .. })),
                        "every reading recorded"
                    );
                }
            })
        })
        .collect();
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    barrier.wait();
    for h in handles {
        h.join().expect("writer");
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_seconds().zip(cpu0).map(|(b, a)| b - a);
    let calls = (a.calls * a.writers) as f64;
    let readings = calls * a.per_call as f64;
    let cores = cpu.map_or("-".into(), |c| format!("{:.2}", c / wall));
    let per_reading = cpu.map_or("-".into(), |c| format!("{:.2}", c / readings * 1e6));
    println!(
        "writers {} per_call {} series {} calls/s {:.0} readings/s {:.0} cores {cores} cpu_us_per_reading {per_reading}",
        a.writers,
        a.per_call,
        a.series,
        calls / wall,
        readings / wall
    );
    block_on(store.shutdown()).expect("shutdown");
    let _ = std::fs::remove_dir_all(&store_dir);
}
