//! The gate for a fourth thread (compaction design §4, Q-1533, Q-1842) with the real run as the background task:
//! ingest at 1,000 readings a second while `kvik-maint` runs compaction of a heavy shape over and over, on a file
//! system that sleeps in every sync to stand in for the device class. Run alone, pinned to one core:
//!
//! `KVIK_GATE_DIR=<dir> KVIK_GATE_SYNC_MS=2 KVIK_GATE_IO_MIB=8 taskset -c 0 cargo test --release -p kvik --lib
//! real_run_gate -- --ignored --nocapture`

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, OpenMode, StdFs};
use kvik_log::setaside::SetAside;
use kvik_log::store::{LOG_DIR, NewStore, StoreDir};
use kvik_log::writer::share::{Background, Next, ShareConfig, Task};
use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};

use super::super::plan::Candidate;
use super::super::run::{MINIMAL_MEMORY, RunInput};
use super::super::task::RunTask;
use super::STORE;
use super::task::{heavy_shapes, put_all};

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

/// Each step's stage, whether it was declared device work, and its wall time.
type Steps = Arc<Mutex<Vec<(&'static str, bool, Duration)>>>;

/// The real run over the same inputs, started again whenever it ends; each step's wall time by its declaration.
struct Again {
    input: RunInput<'static>,
    cands: Vec<Candidate>,
    task: RunTask<'static>,
    runs: Arc<Mutex<u64>>,
    steps: Steps,
    ended: Arc<Mutex<Option<String>>>,
}

impl Background for Again {
    fn next(&mut self) -> Next {
        match self.task.next() {
            Next::Idle => {
                if let Some(Err(e)) = self.task.outcome() {
                    *self.ended.lock().unwrap() = Some(format!("{e:?}"));
                    return Next::Idle;
                }
                *self.runs.lock().unwrap() += 1;
                self.task = RunTask::new(self.input, self.cands.clone(), MINIMAL_MEMORY);
                self.task.next()
            }
            n => n,
        }
    }

    fn step(&mut self) {
        let device = matches!(self.task.next(), Next::Device(_));
        let stage = self.task.stage();
        let t0 = Instant::now();
        self.task.step();
        self.steps
            .lock()
            .unwrap()
            .push((stage, device, t0.elapsed()));
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
    v[((v.len() as f64 - 1.0) * p).round() as usize].as_secs_f64() * 1e3
}

fn env(name: &str, default: f64) -> f64 {
    std::env::var(name).map_or(default, |v| v.parse().expect("a number"))
}

#[test]
#[ignore = "a measurement: run alone, pinned to one core (see the module's documentation)"]
fn real_run_gate() {
    let root = PathBuf::from(std::env::var("KVIK_GATE_DIR").expect("KVIK_GATE_DIR"));
    let delay = Duration::from_secs_f64(env("KVIK_GATE_SYNC_MS", 2.0) / 1e3);
    let io_rate = (env("KVIK_GATE_IO_MIB", 8.0) * f64::from(1 << 20)) as u64;
    let secs = env("KVIK_GATE_SECS", 60.0) as u64;
    let shape = std::env::var("KVIK_GATE_SHAPE").unwrap_or("many-series".into());
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let share = ShareConfig {
        io_rate,
        ..ShareConfig::for_cores(cores)
    };
    println!(
        "{shape}; sync {delay:?}, io rate {io_rate} B/s, cpu share {}, {secs} s, {cores} core(s)\n",
        share.cpu_share
    );
    println!(
        "| Run | creations | p99 ms | max ms | seals | p99 ms | max ms | commit p99 ms | runs | steps | steps ≥ 10 ms | longest device step ms | longest CPU step ms |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let segs = heavy_shapes()
        .into_iter()
        .find(|s| s.0 == shape)
        .expect("a heavy shape")
        .1;
    let fs: &'static SlowFs = Box::leak(Box::new(SlowFs(delay)));
    let input_dir: &'static Path = Box::leak(root.join("inputs").into_boxed_path());
    let _ = std::fs::remove_dir_all(input_dir);
    std::fs::create_dir_all(input_dir.join(LOG_DIR)).unwrap();
    let cands = put_all(input_dir, &segs);
    let set_aside: &'static SetAside = Box::leak(Box::new(SetAside::new(vec![])));
    let weeks: &'static BTreeSet<i64> = Box::leak(Box::default());
    let input = RunInput {
        fs,
        dir: input_dir,
        store_id: STORE,
        generation: 1,
        first_file_id: 100,
        created_at: 5,
        set_aside,
        weeks_merged: weeks,
        registry: &|_| None,
    };
    let mut added = [0.0; 2];
    for with in [false, true] {
        let store_dir = root.join("store");
        let _ = std::fs::remove_dir_all(&store_dir);
        let sfs: Arc<dyn Fs> = Arc::new(SlowFs(delay));
        let new = NewStore {
            store_id: [7; 16],
            created_at: wall_ns(),
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik real run gate".into(),
        };
        let store = StoreDir::open(sfs, &store_dir, Some(&new)).unwrap();
        let cfg = WriterConfig {
            segment_size: 64 << 10,
            relaxed_limits: true,
            disk_critical: 0,
            disk_low_bytes: 0,
            ..WriterConfig::default()
        };
        let w = LogWriter::open(store, cfg, Some([1; 16]), Duration::ZERO, wall_ns()).unwrap();
        let runs = Arc::new(Mutex::new(0));
        let steps = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(Mutex::new(None));
        let task = with.then(|| Task {
            task: Box::new(Again {
                input,
                cands: cands.clone(),
                task: RunTask::new(input, cands.clone(), MINIMAL_MEMORY),
                runs: runs.clone(),
                steps: steps.clone(),
                ended: ended.clone(),
            }),
            share,
        });
        let log = Log::start_with_task(w, Arc::new(SystemClock::default()), None, task).unwrap();
        let (tx, rx) = mpsc::channel::<(Instant, kvik_log::writer::Pending)>();
        let commit = Arc::new(Mutex::new(Vec::new()));
        let c = commit.clone();
        let waiter = std::thread::spawn(move || {
            for (t0, p) in rx {
                p.wait().unwrap();
                c.lock().unwrap().push(t0.elapsed());
            }
        });
        let start = Instant::now();
        let every = Duration::from_millis(1);
        let mut n = 0u64;
        while start.elapsed() < Duration::from_secs(secs) {
            if let Some(d) = (start + every * n as u32).checked_duration_since(Instant::now()) {
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
            tx.send((Instant::now(), log.submit(vec![item]).unwrap()))
                .unwrap();
            n += 1;
        }
        drop(tx);
        waiter.join().unwrap();
        let mut waits = log.maint_waits();
        log.shutdown().unwrap();
        let mut commit = std::mem::take(&mut *commit.lock().unwrap());
        let steps = std::mem::take(&mut *steps.lock().unwrap());
        let longest = |device: bool| {
            steps
                .iter()
                .filter(|s| s.1 == device)
                .map(|s| s.2.as_secs_f64() * 1e3)
                .fold(0.0, f64::max)
        };
        let p99 = [pct(&mut waits.creation, 0.99), pct(&mut waits.seal, 0.99)];
        if with {
            added = [p99[0] - added[0], p99[1] - added[1]];
        } else {
            added = p99;
        }
        println!(
            "| {} | {} | {:.2} | {:.2} | {} | {:.2} | {:.2} | {:.2} | {} | {} | {} | {:.2} | {:.2} |",
            if with { "real run" } else { "no task" },
            waits.creation.len(),
            p99[0],
            pct(&mut waits.creation, 1.0),
            waits.seal.len(),
            p99[1],
            pct(&mut waits.seal, 1.0),
            pct(&mut commit, 0.99),
            runs.lock().unwrap(),
            steps.len(),
            steps
                .iter()
                .filter(|s| s.2 >= Duration::from_millis(10))
                .count(),
            longest(true),
            longest(false),
        );
        if let Some(e) = ended.lock().unwrap().take() {
            println!("the run failed: {e}");
        }
        let mut stages: Vec<(&str, usize, usize, f64)> = Vec::new();
        for (stage, _, d) in &steps {
            let i = stages
                .iter()
                .position(|s| s.0 == *stage)
                .unwrap_or_else(|| {
                    stages.push((stage, 0, 0, 0.0));
                    stages.len() - 1
                });
            let s = &mut stages[i];
            s.1 += 1;
            s.2 += usize::from(*d >= Duration::from_millis(10));
            s.3 = s.3.max(d.as_secs_f64() * 1e3);
        }
        for (stage, n, slow, ms) in stages {
            println!("stage {stage}: {n} steps, {slow} of 10 ms or more, longest {ms:.2} ms");
        }
    }
    println!(
        "\nadded p99 delay: creation {:.2} ms, seal {:.2} ms",
        added[0], added[1]
    );
    let _ = std::fs::remove_dir_all(&root);
}
