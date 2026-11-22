//! CPU, memory and reaction latency of provisional streams (spec §4 Provisional streams) as subscribers are added, with a
//! long gathering window.
//!
//! ```sh
//! cargo run --release -p kvik --example fast_path -- --dir <scratch dir> \
//!     [--subscribers 0,1,5,20,50,200] [--seconds 10] [--series 37] [--window-ms 1000]
//! ```
//!
//! For each count, a fresh durable store under `--dir` with `log.min_sync_interval` and `log.sync_interval_floor`
//! at `--window-ms`; one writer thread emits one reading of each of `--series` series a second, each its own call
//! at its own instant; each subscriber is a thread reading a provisional stream of every series. Prints per count:
//! cores used (process CPU over wall time, from `/proc/self/stat`), resident memory at the end (`/proc/self/status`),
//! provisional delivery latency from the call to the subscriber (p50, p99, max, over every subscriber), and
//! the time from the call until a watermark covered it. Each store directory is removed afterwards.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kvik::{
    Kind, LogConfig, ProvisionalEvent, ProvisionalOptions, Reading, SeriesSpec, Store, StoreConfig,
    Value, ValueType,
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
    subscribers: Vec<usize>,
    seconds: u64,
    series: usize,
    window: Duration,
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::temp_dir(),
        subscribers: vec![0, 1, 5, 20, 50, 200],
        seconds: 10,
        series: 37,
        window: Duration::from_secs(1),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().expect("a value");
        match k.as_str() {
            "--dir" => a.dir = v.into(),
            "--subscribers" => a.subscribers = v.split(',').map(|n| n.parse().unwrap()).collect(),
            "--seconds" => a.seconds = v.parse().unwrap(),
            "--series" => a.series = v.parse().unwrap(),
            "--window-ms" => a.window = Duration::from_millis(v.parse().unwrap()),
            _ => panic!("unknown option {k}"),
        }
    }
    a
}

/// Process CPU time, from `/proc/self/stat` (Linux).
fn cpu() -> Option<Duration> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ticks * 10))
}

/// Resident memory, from `/proc/self/status` (Linux).
fn rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn quantile(v: &mut [Duration], q: f64) -> Duration {
    if v.is_empty() {
        return Duration::ZERO;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn run(a: &Args, subscribers: usize) {
    let dir = a.dir.join(format!(
        "kvik-fast-path-{}-{subscribers}",
        std::process::id()
    ));
    let mut log = LogConfig::default();
    log.min_sync_interval = a.window;
    log.sync_interval_floor = a.window;
    let config = StoreConfig::new(&dir, "Etc/UTC")
        .create_if_missing(true)
        .log(log);
    let store = block_on(Store::open(config)).expect("open");
    let ids: Vec<_> = (0..a.series)
        .map(|i| {
            let spec = SeriesSpec::new(
                format!("bench/s{i}/value"),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "1",
            )
            .expected_interval(Duration::from_secs(1));
            block_on(store.define_series(spec)).expect("define")
        })
        .collect();
    let origin = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..subscribers)
        .map(|_| {
            let mut p = store
                .provisional(ProvisionalOptions::default())
                .expect("stream");
            let stop = stop.clone();
            thread::spawn(move || {
                let mut late = Vec::new();
                let mut durable_after = Vec::new();
                let mut waiting: Vec<(u64, Duration)> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match p.next(Duration::from_millis(200)) {
                        Some(ProvisionalEvent::Accepted(acc)) => {
                            let now = origin.elapsed();
                            for f in acc.facts() {
                                if let Value::F64(at) = f.value {
                                    late.push(now.saturating_sub(Duration::from_secs_f64(at)));
                                }
                            }
                            if !acc.durable() {
                                let at = acc.facts().next().map(|f| match f.value {
                                    Value::F64(v) => Duration::from_secs_f64(v),
                                    _ => now,
                                });
                                waiting.push((acc.seq(), at.unwrap_or(now)));
                            }
                        }
                        Some(ProvisionalEvent::Durable { through, .. }) => {
                            let now = origin.elapsed();
                            waiting.retain(|(s, at)| {
                                let covered = *s <= through;
                                if covered {
                                    durable_after.push(now.saturating_sub(*at));
                                }
                                !covered
                            });
                        }
                        Some(_) | None => {}
                    }
                }
                (late, durable_after)
            })
        })
        .collect();
    let cpu0 = cpu();
    let wall0 = Instant::now();
    let total = a.seconds as usize * a.series;
    let gap = Duration::from_secs(1) / a.series as u32;
    let mut pending = Vec::with_capacity(total);
    for k in 0..total {
        let due = wall0 + gap * k as u32;
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            thread::sleep(w);
        }
        let at = origin.elapsed().as_secs_f64();
        let r = Reading::new(ids[k % ids.len()], store.now(), Value::F64(at));
        pending.push(store.try_emit(&[r]).expect("emit"));
    }
    for p in pending {
        block_on(p).expect("acknowledged");
    }
    thread::sleep(a.window + Duration::from_millis(300));
    let wall = wall0.elapsed();
    let used = cpu().zip(cpu0).map(|(b, a)| b.saturating_sub(a));
    let rss = rss_kib();
    stop.store(true, Ordering::Relaxed);
    let mut late = Vec::new();
    let mut durable = Vec::new();
    for r in readers {
        let (l, d) = r.join().expect("reader");
        late.extend(l);
        durable.extend(d);
    }
    let syncs = store.stats().log.sync_interval;
    block_on(store.shutdown()).expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "{subscribers:>4} subscribers: cores {} | RSS {} MiB | provisional p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms \
         ({} deliveries) | durable after p50 {:.0} ms, max {:.0} ms | sync interval {:?}",
        used.map_or("-".into(), |u| format!(
            "{:.4}",
            u.as_secs_f64() / wall.as_secs_f64()
        )),
        rss.map_or("-".into(), |k| format!("{:.1}", k as f64 / 1024.0)),
        ms(quantile(&mut late, 0.5)),
        ms(quantile(&mut late, 0.99)),
        ms(quantile(&mut late, 1.0)),
        late.len(),
        ms(quantile(&mut durable, 0.5)),
        ms(quantile(&mut durable, 1.0)),
        syncs,
    );
}

fn main() {
    let a = args();
    for &n in &a.subscribers {
        run(&a, n);
    }
}
