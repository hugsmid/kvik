//! CPU, memory and latency of provisional streams (spec §4 Provisional streams) as subscribers are added, with a long
//! gathering window.
//!
//! ```sh
//! cargo run --release -p kvik --example fast_path -- --dir <scratch dir> \
//!     [--subscribers 0,1,5,20,50,200] [--seconds 30] [--series 37] [--rate 370] [--window-ms 1000] \
//!     [--select all|one] [--extra none|slow-failing] [--stall <seconds>]
//! ```
//!
//! For each count, a fresh durable store under `--dir` with `log.min_sync_interval` and `log.sync_interval_floor`
//! at `--window-ms`; one thread emits `--rate` calls a second, each one reading of the next of `--series` series;
//! each subscriber is a thread reading a provisional stream of every series (`--select all`) or of one
//! (`--select one`, round robin). `--extra slow-failing` adds a subscriber that sleeps 10 ms per event and one that
//! panics after 100 events. `--stall s` adds a subscriber with the largest `max_behind` that reads nothing for `s`
//! seconds, then catches up. Prints per count: the emitting thread's, the log writer thread's and the whole
//! process's CPU (cores, from `/proc`), resident memory at the end, the `try_emit` call's latency, provisional
//! delivery latency from the call to the subscriber (over every subscriber but the extras), and the time from the
//! call until a watermark covered it. With `--stall`, also the emit latency in the 2 s after the stalled stream
//! wakes. Each store directory is removed afterwards.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kvik::{
    Kind, LogConfig, PROVISIONAL_MAX_BEHIND, Pending, Provisional, ProvisionalEvent,
    ProvisionalOptions, Reading, SeriesId, SeriesSpec, Store, StoreConfig, Value, ValueType,
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
    rate: u32,
    window: Duration,
    one_series: bool,
    extras: bool,
    stall: Option<Duration>,
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::temp_dir(),
        subscribers: vec![0, 1, 5, 20, 50, 200],
        seconds: 30,
        series: 37,
        rate: 370,
        window: Duration::from_secs(1),
        one_series: false,
        extras: false,
        stall: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().expect("a value");
        match k.as_str() {
            "--dir" => a.dir = v.into(),
            "--subscribers" => a.subscribers = v.split(',').map(|n| n.parse().unwrap()).collect(),
            "--seconds" => a.seconds = v.parse().unwrap(),
            "--series" => a.series = v.parse().unwrap(),
            "--rate" => a.rate = v.parse().unwrap(),
            "--window-ms" => a.window = Duration::from_millis(v.parse().unwrap()),
            "--select" => a.one_series = v == "one",
            "--extra" => a.extras = v == "slow-failing",
            "--stall" => a.stall = Some(Duration::from_secs(v.parse().unwrap())),
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

/// The stat file of the calling thread.
fn own_stat() -> String {
    let tid = std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    format!("/proc/self/task/{tid}/stat")
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

fn spread(v: &mut [Duration]) -> String {
    format!(
        "p50 {:.3} p99 {:.3} p99.9 {:.3} max {:.3} ms (n {})",
        ms(quantile(v, 0.5)),
        ms(quantile(v, 0.99)),
        ms(quantile(v, 0.999)),
        ms(quantile(v, 1.0)),
        v.len()
    )
}

fn cores(before: Option<Duration>, after: Option<Duration>, wall: Duration) -> String {
    after.zip(before).map_or("-".into(), |(b, a)| {
        format!(
            "{:.4}",
            b.saturating_sub(a).as_secs_f64() / wall.as_secs_f64()
        )
    })
}

#[derive(Default)]
struct Seen {
    late: Vec<Duration>,
    durable_after: Vec<Duration>,
}

/// How a subscriber reads its stream.
#[derive(Clone, Copy)]
enum Reader {
    Normal,
    Slow,
    Failing,
    Stalled(Duration),
}

/// What every subscriber thread shares.
#[derive(Clone)]
struct Shared {
    origin: Instant,
    stop: Arc<AtomicBool>,
    woke: Arc<Mutex<Option<Duration>>>,
}

fn subscribe(mut p: Provisional, reader: Reader, sh: Shared) -> thread::JoinHandle<Seen> {
    thread::spawn(move || {
        let mut seen = Seen::default();
        let mut waiting: Vec<(u64, Duration)> = Vec::new();
        let mut events = 0u64;
        if let Reader::Stalled(d) = reader {
            thread::sleep(d);
            *sh.woke.lock().unwrap() = Some(sh.origin.elapsed());
        }
        while !sh.stop.load(Ordering::Relaxed) {
            let e = p.next(Duration::from_millis(200));
            events += 1;
            match reader {
                Reader::Slow => thread::sleep(Duration::from_millis(10)),
                Reader::Failing if events == 100 => panic!("a failing subscriber"),
                _ => {}
            }
            match e {
                Some(ProvisionalEvent::Accepted(acc)) => {
                    let now = sh.origin.elapsed();
                    let at = acc.facts().next().map_or(now, |f| match f.value {
                        Value::F64(v) => Duration::from_secs_f64(v),
                        _ => now,
                    });
                    seen.late.push(now.saturating_sub(at));
                    if !acc.durable() {
                        waiting.push((acc.seq(), at));
                    }
                }
                Some(ProvisionalEvent::Durable { through, .. }) => {
                    let now = sh.origin.elapsed();
                    waiting.retain(|(s, at)| {
                        let covered = *s <= through;
                        if covered {
                            seen.durable_after.push(now.saturating_sub(*at));
                        }
                        !covered
                    });
                }
                Some(_) | None => {}
            }
        }
        seen
    })
}

fn define(store: &Store, n: usize) -> Vec<SeriesId> {
    (0..n)
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
        .collect()
}

/// Drops the oldest calls once settled, so that a long run holds only the calls in flight.
fn reap(pending: &mut VecDeque<Pin<Box<Pending>>>) {
    let mut cx = Context::from_waker(Waker::noop());
    while let Some(p) = pending.front_mut() {
        match p.as_mut().poll(&mut cx) {
            Poll::Ready(r) => {
                r.expect("acknowledged");
                pending.pop_front();
            }
            Poll::Pending => break,
        }
    }
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
    let ids = define(&store, a.series);
    let sh = Shared {
        origin: Instant::now(),
        stop: Arc::new(AtomicBool::new(false)),
        woke: Arc::new(Mutex::new(None)),
    };
    let open = |series: Vec<SeriesId>, max_behind: usize, reader: Reader| {
        let mut o = ProvisionalOptions::default();
        o.series = series;
        o.max_behind = max_behind;
        subscribe(store.provisional(o).expect("stream"), reader, sh.clone())
    };
    let readers: Vec<_> = (0..subscribers)
        .map(|i| {
            let series = if a.one_series {
                vec![ids[i % ids.len()]]
            } else {
                Vec::new()
            };
            open(series, 65_536, Reader::Normal)
        })
        .collect();
    let mut extras = Vec::new();
    if a.extras {
        extras.push(open(Vec::new(), 65_536, Reader::Slow));
        extras.push(open(Vec::new(), 65_536, Reader::Failing));
    }
    if let Some(d) = a.stall {
        extras.push(open(Vec::new(), PROVISIONAL_MAX_BEHIND, Reader::Stalled(d)));
    }
    let (emitter, writer) = (own_stat(), thread_stat("kvik-writer").unwrap_or_default());
    let (cpu0, emit_cpu0, writer_cpu0) =
        (cpu_of("/proc/self/stat"), cpu_of(&emitter), cpu_of(&writer));
    let wall0 = Instant::now();
    let total = a.seconds as usize * a.rate as usize;
    let gap = Duration::from_secs(1) / a.rate;
    let mut pending = VecDeque::new();
    let mut emits: Vec<(Duration, Duration)> = Vec::with_capacity(total);
    for k in 0..total {
        let due = wall0 + gap * k as u32;
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            thread::sleep(w);
        }
        let at = sh.origin.elapsed();
        let value = Value::F64(at.as_secs_f64());
        let r = Reading::new(ids[k % ids.len()], store.now(), value);
        let started = Instant::now();
        let p = store.try_emit(&[r]).expect("emit");
        emits.push((at, started.elapsed()));
        pending.push_back(Box::pin(p));
        if k % 64 == 0 {
            reap(&mut pending);
        }
    }
    for p in pending {
        block_on(p).expect("acknowledged");
    }
    let (emit_cpu, writer_cpu) = (cpu_of(&emitter), cpu_of(&writer));
    let emit_wall = wall0.elapsed();
    thread::sleep(a.window + Duration::from_millis(300));
    let wall = wall0.elapsed();
    let used = cpu_of("/proc/self/stat");
    let rss = rss_kib();
    sh.stop.store(true, Ordering::Relaxed);
    let mut late = Vec::new();
    let mut durable = Vec::new();
    for r in readers {
        let s = r.join().expect("reader");
        late.extend(s.late);
        durable.extend(s.durable_after);
    }
    for e in extras {
        let _ = e.join();
    }
    let syncs = store.stats().log.sync_interval;
    block_on(store.shutdown()).expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
    let after_wake = sh.woke.lock().unwrap().map(|w| {
        let mut v: Vec<Duration> = emits
            .iter()
            .filter(|(at, _)| *at >= w && *at < w + Duration::from_secs(2))
            .map(|(_, d)| *d)
            .collect();
        format!("\n     emit in the 2 s after the wake {}", spread(&mut v))
    });
    let mut emit_lat: Vec<Duration> = emits.iter().map(|(_, d)| *d).collect();
    let extra = match (a.extras, a.stall) {
        (true, _) => ", plus a slow and a failing one".to_string(),
        (_, Some(d)) => format!(", plus one stalled {d:?}"),
        _ => String::new(),
    };
    println!(
        "{subscribers:>4} subscribers ({} series each{extra}): cores process {} | emitting thread {} | log writer {} \
         | RSS {} MiB\n     emit {}\n     provisional {}\n     durable after p50 {:.0} ms, max {:.0} ms | sync \
         interval {syncs:?}{}",
        if a.one_series { "one" } else { "all" },
        cores(cpu0, used, wall),
        cores(emit_cpu0, emit_cpu, emit_wall),
        cores(writer_cpu0, writer_cpu, emit_wall),
        rss.map_or("-".into(), |k| format!("{:.1}", k as f64 / 1024.0)),
        spread(&mut emit_lat),
        spread(&mut late),
        ms(quantile(&mut durable, 0.5)),
        ms(quantile(&mut durable, 1.0)),
        after_wake.unwrap_or_default(),
    );
}

fn main() {
    let a = args();
    for &n in &a.subscribers {
        run(&a, n);
    }
}
