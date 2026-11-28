//! The gate for a fourth thread (compaction design §4, Q-801): the delay a background task on `kvik-maint` adds to
//! creation ahead and to seals, and to commit latency, on a real file system with a sleep standing in for a
//! device's sync time. Run it on one core:
//!
//! ```text
//! taskset -c 0 cargo run --release --locked -p kvik-log --example maint_gate -- <dir> [options]
//!   --sync-delay <ms>  sleep this long in every sync (default 2)
//!   --io-rate <MiB/s>  the task's device rate (default 8)
//!   --rate <n>         readings a second, one per call (default 1000)
//!   --secs <n>         seconds per run (default 20)
//! ```
//!
//! Each run is made twice, without the task and with it; the task's every step encodes a block of 4,096 rows and
//! writes one 128 KiB piece, syncing its file every 1 MiB, as a compaction does.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, OpenMode, StdFs};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::share::{Background, Next, PIECE, ShareConfig, Task};
use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};

/// [`StdFs`] with a sleep in every sync.
#[derive(Debug)]
struct SlowFs(Duration);

#[derive(Debug)]
struct SlowFile(Box<dyn FsFile>, Duration);

impl Fs for SlowFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        Ok(Box::new(SlowFile(StdFs.open(path, mode)?, self.0)))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        StdFs.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        StdFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        std::thread::sleep(self.0);
        StdFs.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        StdFs.free_space(path)
    }
}

impl FsFile for SlowFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.0.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        std::thread::sleep(self.1);
        self.0.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        std::thread::sleep(self.1);
        self.0.sync_all()
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

/// A compaction-shaped task: a block encoded, a piece written, a sync every 1 MiB, forever.
struct Job {
    file: Box<dyn FsFile>,
    at: u64,
    block: Vec<u64>,
    out: Vec<u8>,
    phase: u8,
}

impl Background for Job {
    fn next(&mut self) -> Next {
        match self.phase {
            0 => Next::Cpu,
            1 => Next::Device(PIECE),
            _ => Next::Device(0),
        }
    }

    fn step(&mut self) {
        match self.phase {
            0 => {
                self.out.clear();
                let mut prev = 0u64;
                for (i, v) in self.block.iter_mut().enumerate() {
                    *v = v.rotate_left(7) ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    let x = *v ^ prev;
                    prev = *v;
                    self.out
                        .extend_from_slice(&x.to_le_bytes()[..(x.leading_zeros() / 8) as usize]);
                    self.out.extend_from_slice(&x.to_le_bytes());
                }
                self.phase = 1;
            }
            1 => {
                self.out.resize(PIECE as usize, 0);
                self.file
                    .write_at(&self.out, self.at % (64 << 20))
                    .expect("write a piece");
                self.at += PIECE;
                self.phase = if self.at.is_multiple_of(1 << 20) {
                    2
                } else {
                    0
                };
            }
            _ => {
                self.file.sync_data().expect("sync the output");
                self.phase = 0;
            }
        }
    }
}

fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i].as_secs_f64() * 1e3
}

struct Measured {
    creation: Vec<Duration>,
    seal: Vec<Duration>,
    commit: Vec<Duration>,
    steps: u64,
}

fn run(dir: &Path, delay: Duration, share: Option<ShareConfig>, rate: u64, secs: u64) -> Measured {
    let _ = std::fs::remove_dir_all(dir);
    let fs: Arc<dyn Fs> = Arc::new(SlowFs(delay));
    let new = NewStore {
        store_id: [7; 16],
        created_at: wall_ns(),
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik maint gate".into(),
    };
    let store = StoreDir::open(fs.clone(), dir, Some(&new)).expect("create a store");
    let cfg = WriterConfig {
        segment_size: 64 << 10,
        relaxed_limits: true,
        disk_critical: 0,
        disk_low_bytes: 0,
        ..WriterConfig::default()
    };
    let w = LogWriter::open(store, cfg, Some([1; 16]), Duration::ZERO, wall_ns()).expect("open");
    let task = share.map(|share| Task {
        task: Box::new(Job {
            file: fs
                .open(&dir.join("job.tmp"), OpenMode::CreateNew)
                .expect("the task's file"),
            at: 0,
            block: (0..4096).collect(),
            out: Vec::new(),
            phase: 0,
        }),
        share,
    });
    let log = Log::start_with_task(w, Arc::new(SystemClock::default()), None, task).expect("start");
    let (tx, rx) = mpsc::channel::<(Instant, kvik_log::writer::Pending)>();
    let commit = Arc::new(Mutex::new(Vec::new()));
    let c = commit.clone();
    let waiter = std::thread::spawn(move || {
        for (t0, p) in rx {
            p.wait().expect("a call lands");
            c.lock().unwrap().push(t0.elapsed());
        }
    });
    let start = Instant::now();
    let every = Duration::from_nanos(1_000_000_000 / rate.max(1));
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let due = start + every * n as u32;
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        let item = Item::Fact(FactRow {
            series_id: (n % 100) as u32,
            t: wall_ns(),
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: Value::F64((n as f64).to_bits()),
            changed_at: None,
        });
        let t0 = Instant::now();
        tx.send((t0, log.submit(vec![item]).expect("submit")))
            .expect("waiter");
        n += 1;
    }
    drop(tx);
    waiter.join().expect("waiter");
    let waits = log.maint_waits();
    log.shutdown().expect("shut down");
    let commit = std::mem::take(&mut *commit.lock().unwrap());
    Measured {
        creation: waits.creation,
        seal: waits.seal,
        commit,
        steps: waits.steps,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: maint_gate <dir> [options]"));
    let (mut delay_ms, mut io_mib, mut rate, mut secs) = (2.0f64, 8u64, 1000u64, 20u64);
    while let Some(a) = args.next() {
        let v = args.next().expect("a value");
        match a.as_str() {
            "--sync-delay" => delay_ms = v.parse().expect("milliseconds"),
            "--io-rate" => io_mib = v.parse().expect("MiB/s"),
            "--rate" => rate = v.parse().expect("readings a second"),
            "--secs" => secs = v.parse().expect("seconds"),
            _ => panic!("unknown option {a}"),
        }
    }
    let delay = Duration::from_secs_f64(delay_ms / 1e3);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let share = ShareConfig {
        io_rate: io_mib << 20,
        ..ShareConfig::for_cores(cores)
    };
    println!(
        "sync delay {delay_ms} ms, io rate {io_mib} MiB/s, cpu share {}, {rate} readings/s, {secs} s, {cores} core(s)\n",
        share.cpu_share
    );
    println!(
        "| Run | creations | p50 ms | p99 ms | max ms | seals | p50 ms | p99 ms | max ms | commit p50 ms | commit p99 ms | task steps |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut p99 = [[0.0; 2]; 2];
    for (k, (name, s)) in [("no task", None), ("task", Some(share))]
        .into_iter()
        .enumerate()
    {
        let mut m = run(&dir.join("store"), delay, s, rate, secs);
        p99[k] = [pct(&mut m.creation, 0.99), pct(&mut m.seal, 0.99)];
        println!(
            "| {name} | {} | {:.2} | {:.2} | {:.2} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {} |",
            m.creation.len(),
            pct(&mut m.creation, 0.5),
            pct(&mut m.creation, 0.99),
            pct(&mut m.creation, 1.0),
            m.seal.len(),
            pct(&mut m.seal, 0.5),
            pct(&mut m.seal, 0.99),
            pct(&mut m.seal, 1.0),
            pct(&mut m.commit, 0.5),
            pct(&mut m.commit, 0.99),
            m.steps,
        );
    }
    let _ = std::fs::remove_dir_all(dir.join("store"));
    println!(
        "\nadded p99 delay: creation {:.2} ms, seal {:.2} ms",
        p99[1][0] - p99[0][0],
        p99[1][1] - p99[0][1]
    );
}
