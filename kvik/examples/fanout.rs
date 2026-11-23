//! CPU and reaction of push delivery as subscribers are added, on the W0 home load: 37 series at 1 Hz, unaligned,
//! from 8 calling threads.
//!
//! ```sh
//! cargo run --release -p kvik --example fanout -- --dir <scratch dir> \
//!     [--live 5,20,50,200] [--consumers 20,50] [--seconds 30] [--warmup 5]
//! ```
//!
//! Each cell is a fresh durable store under `--dir` with the default configuration. Every subscriber watches 2 to 6
//! of the series: a live session (`min_batch_interval` 20 ms) with a `Tail` of them, waiting in `next`, or a
//! consumer routed to them that confirms each delivery. After the warm-up it prints, over the window: the whole
//! process's CPU and the log writer thread's (% of one core, from `/proc`), context switches a second over every
//! thread (wakes), and reaction from a reading's timestamp to its delivery (p50, p99). Each store directory is
//! removed afterwards.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik::{
    Appended, DeliveryKind, Funnel, Kind, LiveEvent, LiveOptions, Query, Reading, Select, SeriesId,
    SeriesSpec, SessionOptions, Start, Store, StoreConfig, Timestamp, Value, ValueType,
};

const SERIES: usize = 37;
const CALLERS: usize = 8;

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
    live: Vec<usize>,
    consumers: Vec<usize>,
    seconds: u64,
    warmup: u64,
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::temp_dir(),
        live: vec![5, 20, 50, 200],
        consumers: vec![20, 50],
        seconds: 30,
        warmup: 5,
    };
    let list = |v: &str| -> Vec<usize> {
        v.split(',')
            .filter(|s| !s.is_empty())
            .map(|n| n.parse().unwrap())
            .collect()
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().expect("a value");
        match k.as_str() {
            "--dir" => a.dir = v.into(),
            "--live" => a.live = list(&v),
            "--consumers" => a.consumers = list(&v),
            "--seconds" => a.seconds = v.parse().unwrap(),
            "--warmup" => a.warmup = v.parse().unwrap(),
            _ => panic!("unknown option {k}"),
        }
    }
    a
}

/// User and system CPU time from a `/proc` stat file (Linux).
fn cpu_of(path: &str) -> Option<Duration> {
    let s = std::fs::read_to_string(path).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    Some(Duration::from_millis(ticks * 10))
}

/// The stat file of this process's thread with the given name.
fn thread_stat(name: &str) -> Option<String> {
    std::fs::read_dir("/proc/self/task").ok()?.find_map(|t| {
        let t = t.ok()?.path();
        let comm = std::fs::read_to_string(t.join("comm")).ok()?;
        (comm.trim() == name).then(|| t.join("stat").to_string_lossy().into_owned())
    })
}

/// Voluntary and involuntary context switches of every thread of this process.
fn switches() -> u64 {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("status")).ok())
        .flat_map(|s| {
            s.lines()
                .filter(|l| l.contains("ctxt_switches"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .collect::<Vec<_>>()
        })
        .sum()
}

fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn quantile(v: &mut [Duration], q: f64) -> Duration {
    if v.is_empty() {
        return Duration::ZERO;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// A small deterministic generator, so that every run places the same phases and selections.
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

/// The series each subscriber watches: 2 to 6 of them.
fn watched(ids: &[SeriesId], n: usize) -> Vec<Vec<SeriesId>> {
    let mut g = Lcg(7);
    (0..n)
        .map(|_| {
            let k = 2 + (g.next() % 5) as usize;
            let mut w: Vec<SeriesId> = Vec::new();
            while w.len() < k {
                let id = ids[(g.next() as usize) % ids.len()];
                if !w.contains(&id) {
                    w.push(id);
                }
            }
            w
        })
        .collect()
}

#[derive(Clone)]
struct Shared {
    stop: Arc<AtomicBool>,
    /// Reactions are kept from this wall-clock instant (nanoseconds).
    from: i64,
    reactions: Arc<Mutex<Vec<Duration>>>,
}

impl Shared {
    fn react(&self, t: Timestamp, mine: &mut Vec<Duration>) {
        if t.0 >= self.from {
            mine.push(Duration::from_nanos(
                u64::try_from(wall_ns() - t.0).unwrap_or(0),
            ));
        }
    }
}

fn feed(store: &Store, ids: &[SeriesId], sh: &Shared) -> Vec<thread::JoinHandle<()>> {
    let mut g = Lcg(11);
    let phases: Vec<Duration> = (0..ids.len())
        .map(|_| Duration::from_micros(g.next() % 1_000_000))
        .collect();
    let origin = Instant::now();
    (0..CALLERS)
        .map(|c| {
            let store = store.clone();
            let sh = sh.clone();
            let mine: Vec<(SeriesId, Duration)> = (c..ids.len())
                .step_by(CALLERS)
                .map(|i| (ids[i], phases[i]))
                .collect();
            thread::spawn(move || {
                let mut due: Vec<(Duration, SeriesId)> =
                    mine.iter().map(|&(id, p)| (p, id)).collect();
                let mut v = 0.0;
                while !sh.stop.load(Ordering::Relaxed) {
                    due.sort_unstable();
                    let (at, id) = due[0];
                    thread::sleep(at.saturating_sub(origin.elapsed()));
                    v += 1.0;
                    let r = [Reading::new(id, Timestamp(wall_ns()), Value::F64(v))];
                    if block_on(store.emit(&r)).is_err() {
                        return;
                    }
                    due[0].0 = at + Duration::from_secs(1);
                }
            })
        })
        .collect()
}

fn live(store: &Store, w: Vec<SeriesId>, sh: Shared) -> thread::JoinHandle<()> {
    let reader = store.reader();
    thread::spawn(move || {
        let opts = SessionOptions::new(Duration::from_millis(20), Duration::from_millis(100));
        let mut s = reader.session_with(opts).unwrap();
        s.open(
            Query::Tail {
                sel: Select::Ids(w).into(),
            },
            LiveOptions::default(),
        )
        .unwrap();
        let mut mine = Vec::new();
        while !sh.stop.load(Ordering::Relaxed) {
            let Ok(Some(b)) = s.next(Duration::from_secs(1)) else {
                continue;
            };
            for (_, e) in b.events {
                if let LiveEvent::Appended(Appended::Facts(fs)) = e {
                    for f in fs {
                        sh.react(f.t, &mut mine);
                    }
                }
            }
        }
        sh.reactions.lock().unwrap().extend(mine);
    })
}

fn consumer(store: &Store, k: usize, w: Vec<SeriesId>, sh: Shared) -> thread::JoinHandle<()> {
    let mut c = block_on(store.consumer(&format!("a{k}"), Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(w)))).unwrap();
    thread::spawn(move || {
        let mut mine = Vec::new();
        while let Some(d) = block_on(c.next()) {
            if let DeliveryKind::Fact(f) = &d.kind {
                sh.react(f.t, &mut mine);
            }
            if block_on(c.confirm(d.id)).is_err() {
                break;
            }
        }
        sh.reactions.lock().unwrap().extend(mine);
    })
}

fn cell(a: &Args, what: &str, n: usize) {
    let dir = a
        .dir
        .join(format!("fanout-{what}-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = block_on(Store::open(
        StoreConfig::new(&dir, "Etc/UTC").create_if_missing(true),
    ))
    .unwrap();
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|i| {
            let spec = SeriesSpec::new(
                format!("w0/s{i}/power"),
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            );
            block_on(store.define_series(spec)).unwrap()
        })
        .collect();
    let sh = Shared {
        stop: Arc::new(AtomicBool::new(false)),
        from: wall_ns() + i64::try_from(a.warmup).unwrap() * 1_000_000_000,
        reactions: Arc::new(Mutex::new(Vec::new())),
    };
    let subs: Vec<_> = watched(&ids, n)
        .into_iter()
        .enumerate()
        .map(|(k, w)| match what {
            "live" => live(&store, w, sh.clone()),
            _ => consumer(&store, k, w, sh.clone()),
        })
        .collect();
    let callers = feed(&store, &ids, &sh);
    thread::sleep(Duration::from_secs(a.warmup));
    let writer = thread_stat("kvik-writer").unwrap_or_default();
    let (p0, w0, s0, t0) = (
        cpu_of("/proc/self/stat"),
        cpu_of(&writer),
        switches(),
        Instant::now(),
    );
    thread::sleep(Duration::from_secs(a.seconds));
    let (p1, w1, s1, wall) = (
        cpu_of("/proc/self/stat"),
        cpu_of(&writer),
        switches(),
        t0.elapsed(),
    );
    sh.stop.store(true, Ordering::SeqCst);
    for c in callers {
        c.join().unwrap();
    }
    block_on(store.shutdown()).unwrap();
    for s in subs {
        s.join().unwrap();
    }
    let pct = |b: Option<Duration>, e: Option<Duration>| {
        e.zip(b).map_or(f64::NAN, |(e, b)| {
            100.0 * e.saturating_sub(b).as_secs_f64() / wall.as_secs_f64()
        })
    };
    let mut r = std::mem::take(&mut *sh.reactions.lock().unwrap());
    println!(
        "| {what} | {n} | {:.2} | {:.2} | {:.0} | {:.2} | {:.2} | {} |",
        pct(p0, p1),
        pct(w0, w1),
        (s1 - s0) as f64 / wall.as_secs_f64(),
        quantile(&mut r, 0.5).as_secs_f64() * 1e3,
        quantile(&mut r, 0.99).as_secs_f64() * 1e3,
        r.len(),
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let a = args();
    println!(
        "| mode | subscribers | process CPU % | writer CPU % | wakes/s | reaction p50 ms | p99 ms | deliveries |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for &n in &a.live {
        cell(&a, "live", n);
    }
    for &n in &a.consumers {
        cell(&a, "consumer", n);
    }
}
