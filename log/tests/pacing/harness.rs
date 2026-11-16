//! The harness of `pacing.rs`: the writer core driven with explicit times and a manual clock, the generated
//! histories and the reference schedule.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode};
use kvik_log::store::{MARK, NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource, Clock, IngestQueue, LogWriter, MarkSync, WriterConfig, create_segment,
};
use proptest::prelude::*;

pub const T0: i64 = 1_767_225_600_000_000_000;
pub const US: Duration = Duration::from_micros(1);
pub const MS: Duration = Duration::from_millis(1);
pub const CEILING: Duration = Duration::from_millis(10);
pub const FLOOR: Duration = Duration::from_millis(2);
pub const FAST: Duration = Duration::from_micros(160);
pub const START: Duration = Duration::from_secs(1);
pub const NO_SEAL: Duration = Duration::from_secs(300);
/// A timed mark sync no test reaches, so that only batch syncs set the writer's deadlines.
pub const NO_MARK: MarkSync = MarkSync::Interval(Duration::from_secs(3600));
/// How long after its arrival a call wakes the writer, unless a test says otherwise.
pub const WAKE: Duration = Duration::from_micros(20);

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
pub struct ManualClock(pub Mutex<Duration>);

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *lock(&self.0)
    }
    fn wall(&self) -> i64 {
        T0
    }
}

/// How long the next segment and mark syncs, and every write, take while armed.
#[derive(Debug, Default)]
pub struct SyncTimes {
    pub armed: bool,
    pub segment: Duration,
    pub mark: Duration,
    pub write: Duration,
}

/// [`MemFs`] whose writes advance the manual clock while armed.
#[derive(Debug, Clone)]
pub struct TimedWrites {
    inner: MemFs,
    clock: Arc<ManualClock>,
    times: Arc<Mutex<SyncTimes>>,
}

#[derive(Debug)]
struct TimedFile {
    inner: Box<dyn FsFile>,
    fs: TimedWrites,
}

impl Fs for TimedWrites {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>> {
        Ok(Box::new(TimedFile {
            inner: self.inner.open(path, mode)?,
            fs: self.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        self.inner.free_space(path)
    }
}

impl FsFile for TimedFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let t = lock(&self.fs.times);
        if t.armed {
            *lock(&self.fs.clock.0) += t.write;
        }
        drop(t);
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
}

pub fn fact(t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub floor: Duration,
    pub ceiling: Duration,
    pub seal: Duration,
    pub clock: bool,
    pub mark: MarkSync,
}

impl Setup {
    pub fn adaptive() -> Self {
        Self {
            floor: FLOOR,
            ceiling: CEILING,
            seal: NO_SEAL,
            clock: true,
            mark: NO_MARK,
        }
    }
    pub fn fixed() -> Self {
        Self {
            clock: false,
            ..Self::adaptive()
        }
    }
    pub fn config(&self) -> WriterConfig {
        WriterConfig {
            segment_size: 1 << 20,
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            seal_interval: self.seal,
            min_sync_interval: self.ceiling,
            sync_interval_floor: self.floor,
            mark_sync: self.mark,
            ..WriterConfig::default()
        }
    }
}

pub struct Sim {
    pub fs: MemFs,
    pub setup: Setup,
    pub w: LogWriter,
    pub q: IngestQueue,
    pub clock: Arc<ManualClock>,
    pub times: Arc<Mutex<SyncTimes>>,
    /// Start of every batch sync.
    pub starts: Vec<Duration>,
    /// Start of every log sync, a timed seal's included.
    pub log_syncs: Vec<Duration>,
    /// Start of the sync that covered each call, by call id.
    pub covered: BTreeMap<u64, Duration>,
    /// The interval in force after each batch sync.
    pub intervals: Vec<Duration>,
    /// Turns after which the next batch sync may wait more than a ceiling after the last batch sync and more than
    /// half one after the last log sync: the turn's time and the writer's ready time.
    pub ceiling_breaks: Vec<(Duration, Duration)>,
    /// How long each write takes during a turn.
    pub write: Duration,
    pub pushed: i64,
}

impl Sim {
    pub fn new(setup: Setup) -> Self {
        let fs = MemFs::new();
        let new = NewStore {
            store_id: [7; 16],
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), Some(&new)).unwrap();
        let clock = Arc::new(ManualClock::default());
        let times = Arc::new(Mutex::new(SyncTimes::default()));
        let (c, t) = (clock.clone(), times.clone());
        fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
            let t = lock(&t);
            if t.armed {
                let d = if p.ends_with(MARK) { t.mark } else { t.segment };
                *lock(&c.0) += d;
            }
        })));
        Self::with(fs, setup, clock, times)
    }

    pub fn with(
        fs: MemFs,
        setup: Setup,
        clock: Arc<ManualClock>,
        times: Arc<Mutex<SyncTimes>>,
    ) -> Self {
        let w = Self::open_writer(&Self::timed(&fs, &clock, &times), setup, &clock);
        let mut s = Self {
            fs,
            setup,
            w,
            q: IngestQueue::new(1 << 16),
            clock,
            times,
            starts: Vec::new(),
            log_syncs: Vec::new(),
            covered: BTreeMap::new(),
            intervals: Vec::new(),
            ceiling_breaks: Vec::new(),
            write: Duration::ZERO,
            pushed: 0,
        };
        s.create();
        s
    }

    fn timed(fs: &MemFs, clock: &Arc<ManualClock>, times: &Arc<Mutex<SyncTimes>>) -> TimedWrites {
        TimedWrites {
            inner: fs.clone(),
            clock: clock.clone(),
            times: times.clone(),
        }
    }

    pub fn open_writer(fs: &TimedWrites, setup: Setup, clock: &Arc<ManualClock>) -> LogWriter {
        let store = StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), None).unwrap();
        let mut w = LogWriter::open(store, setup.config(), Some([9; 16]), START, T0).unwrap();
        if setup.clock {
            w.set_sync_clock(clock.clone());
        }
        w
    }

    /// Shuts the writer down and opens it again on the same files.
    pub fn reopen(mut self, now: Duration) -> Self {
        self.w.shutdown(now).unwrap();
        let Sim {
            fs,
            setup,
            w,
            clock,
            times,
            ..
        } = self;
        drop(w);
        Self::with(fs, setup, clock, times)
    }

    pub fn create(&mut self) {
        if let Some(req) = self.w.segment_request(T0) {
            let fs = Self::timed(&self.fs, &self.clock, &self.times);
            let r = create_segment(&fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    pub fn push(&mut self) -> u64 {
        self.push_items(vec![fact(T0 + self.pushed + 1)])
    }

    pub fn push_items(&mut self, items: Vec<Item>) -> u64 {
        self.pushed += 1;
        let call = Call::new(items, self.w.config().call_limit()).unwrap();
        self.q.push(call).unwrap()
    }

    /// One turn of the writer at `now`, a segment sync taking `segment` and a mark sync `mark`: its next deadline
    /// and how long its syncs kept it busy.
    pub fn turn_with(
        &mut self,
        now: Duration,
        segment: Duration,
        mark: Duration,
    ) -> (Option<Duration>, Duration) {
        let (batches, syncs) = (self.w.stats().batches, self.w.stats().syncs);
        *lock(&self.times) = SyncTimes {
            armed: true,
            segment,
            mark,
            write: self.write,
        };
        let before = self.clock.monotonic();
        let step = self.w.step(now, T0 + now.as_nanos() as i64, &mut self.q);
        lock(&self.times).armed = false;
        let busy = self.clock.monotonic() - before;
        for (id, r) in step.done {
            match r {
                Ok(_) => drop(self.covered.insert(id, now)),
                Err(e) => assert!(self.w.fault().is_some(), "{e:?} with no fault"),
            }
        }
        self.create();
        if self.w.stats().syncs > syncs {
            self.log_syncs.push(now);
        }
        if self.w.stats().batches > batches {
            self.starts.push(now);
            self.intervals.push(self.w.stats().sync_interval);
        }
        if let (Some(&batch), Some(&log)) = (self.starts.last(), self.log_syncs.last()) {
            let c = self.setup.ceiling;
            if self.w.sync_ready_at() > batch.saturating_add(c).max(log.saturating_add(c / 2)) {
                self.ceiling_breaks.push((now, self.w.sync_ready_at()));
            }
        }
        (step.next, busy)
    }

    pub fn turn(&mut self, now: Duration, segment: Duration) -> (Option<Duration>, Duration) {
        self.turn_with(now, segment, Duration::ZERO)
    }
}

/// When the writer thread next turns: at once if busy past its deadline, else `late` after the deadline.
pub fn timed(next: Option<Duration>, free: Duration, late: Duration) -> Option<Duration> {
    next.map(|t| if t <= free { free } else { t + late })
}

/// A generated history: calls at `(arrival, wake)` (the writer is woken `wake` after the arrival, in order of
/// arrival), timed wakes `late` after each deadline, and the `k`-th batch sync taking `syncs[k % len]`.
#[derive(Clone, Debug)]
pub struct History {
    pub calls: Vec<(Duration, Duration)>,
    pub late: Duration,
    pub syncs: Vec<Duration>,
}

impl History {
    pub fn sync(&self, k: usize) -> Duration {
        self.syncs[k % self.syncs.len()]
    }
}

/// The start of each batch sync and the start of the sync covering each call, in order of arrival.
#[derive(Debug, PartialEq, Eq)]
pub struct Schedule {
    pub starts: Vec<Duration>,
    pub covered: Vec<Duration>,
}

/// Runs `h` on the writer.
pub fn simulate(s: &mut Sim, h: &History) -> Schedule {
    let n = h.calls.len();
    let mut ids = Vec::new();
    let (mut k, mut free, mut next) = (0, Duration::ZERO, None);
    for _ in 0..100 * n + 100 {
        if k == n && s.q.is_empty() {
            let covered = ids.iter().map(|id| s.covered[id]).collect();
            let starts = s.starts.clone();
            return Schedule { starts, covered };
        }
        let by_call = (k < n).then(|| h.calls[k].0 + h.calls[k].1);
        let at = [by_call, timed(next, free, h.late)]
            .into_iter()
            .flatten()
            .min()
            .unwrap()
            .max(free);
        while k < n && h.calls[k].0 <= at {
            ids.push(s.push());
            k += 1;
        }
        let (n2, busy) = s.turn(at, h.sync(s.starts.len()));
        free = at + busy;
        next = n2;
    }
    panic!("the calls do not drain");
}

/// The reference schedule: spec §3 Group commit with the interval of `pacing.rs`, driven as [`simulate`] drives the
/// writer. With `clocked` false no sync is timed, so the interval stays at the ceiling.
pub fn reference(h: &History, floor: Duration, ceiling: Duration, clocked: bool) -> Schedule {
    let ns = |d: Duration| d.as_nanos() as u64;
    let interval = |d: Option<u64>| {
        d.map_or(ceiling, |d| {
            let c = ns(ceiling);
            Duration::from_nanos(d.saturating_mul(8).clamp(ns(floor).min(c), c))
        })
    };
    let (mut d, mut due, mut last) = (None::<u64>, None::<Duration>, None::<Duration>);
    let ready = |d, due: Option<Duration>, last: Option<Duration>| {
        let i = interval(d);
        let grid = due.map_or(Duration::ZERO, |x| x + i);
        (grid, grid.max(last.map_or(Duration::ZERO, |x| x + i / 2)))
    };
    let n = h.calls.len();
    let (mut starts, mut covered) = (Vec::new(), vec![Duration::ZERO; n]);
    let (mut k, mut taken, mut free, mut next) = (0, 0, Duration::ZERO, None);
    while k < n || taken < k {
        let by_call = (k < n).then(|| h.calls[k].0 + h.calls[k].1);
        let at = [by_call, timed(next, free, h.late)]
            .into_iter()
            .flatten()
            .min()
            .unwrap()
            .max(free);
        while k < n && h.calls[k].0 <= at {
            k += 1;
        }
        let (grid, at_ready) = ready(d, due, last);
        free = at;
        if taken < k && at >= at_ready {
            due = Some(grid.max(at.saturating_sub(interval(d))));
            last = Some(at);
            covered[taken..k].fill(at);
            taken = k;
            let sync = h.sync(starts.len());
            starts.push(at);
            free = at + sync;
            if clocked {
                let s = ns(sync);
                d = Some(d.filter(|&x| s < x).map_or(s, |x| x - (x - s) / 8));
            }
        }
        next = (taken < k).then(|| ready(d, due, last).1.max(at));
    }
    Schedule { starts, covered }
}

pub fn arb_gap() -> impl Strategy<Value = Duration> {
    prop_oneof![
        1 => Just(Duration::ZERO),
        3 => (0u64..3_000).prop_map(|u| US * u as u32),
        3 => (0u64..15_000).prop_map(|u| US * u as u32),
        1 => (20u64..60).prop_map(|m| MS * m as u32),
    ]
}

pub fn arb_wake() -> impl Strategy<Value = Duration> {
    prop_oneof![6 => Just(WAKE), 1 => (0u64..3_000).prop_map(|u| US * u as u32)]
}

pub fn arb_sync() -> impl Strategy<Value = Duration> {
    prop_oneof![
        4 => (0u64..300).prop_map(|u| US * u as u32),
        2 => (300u64..2_000).prop_map(|u| US * u as u32),
        1 => (2_000u64..30_000).prop_map(|u| US * u as u32),
    ]
}

pub fn arb_history() -> impl Strategy<Value = History> {
    (
        prop::collection::vec((arb_gap(), arb_wake()), 1..150),
        (0u64..400).prop_map(|u| US * u as u32),
        prop::collection::vec(arb_sync(), 1..20),
    )
        .prop_map(|(gaps, late, syncs)| {
            let mut t = START;
            let calls = gaps
                .into_iter()
                .map(|(gap, wake)| {
                    t += gap;
                    (t, wake)
                })
                .collect();
            History { calls, late, syncs }
        })
}

/// A floor and a ceiling with the floor below the ceiling; a zero ceiling now and then.
pub fn arb_bounds() -> impl Strategy<Value = (Duration, Duration)> {
    prop_oneof![
        8 => (1u64..=20, 0u64..20).prop_map(|(c, f)| (US * (500 * f.min(2 * c - 1)) as u32, MS * c as u32)),
        1 => (0u64..=10).prop_map(|f| (MS * f as u32, Duration::ZERO)),
    ]
}

pub fn run(setup: Setup, h: &History) -> (Sim, Schedule) {
    let mut s = Sim::new(setup);
    let out = simulate(&mut s, h);
    (s, out)
}

/// One call every `period` from `START`, with each batch sync taking `sync`: each call's wait.
pub fn periodic(setup: Setup, n: usize, period: Duration, sync: Duration) -> (Sim, Vec<Duration>) {
    let calls = (0..n)
        .map(|k| (START + period * k as u32, WAKE))
        .collect::<Vec<_>>();
    let h = History {
        calls: calls.clone(),
        late: 100 * US,
        syncs: vec![sync],
    };
    let (s, out) = run(setup, &h);
    let waits = out
        .covered
        .iter()
        .zip(&calls)
        .map(|(c, a)| *c - a.0)
        .collect();
    (s, waits)
}

/// Saturation: a call always queued, batch syncs taking `sync(k)`.
pub fn saturated(setup: Setup, n: usize, sync: impl Fn(usize) -> Duration) -> Sim {
    let mut s = Sim::new(setup);
    let (mut at, mut free) = (START, START);
    s.push();
    while s.starts.len() < n {
        let before = s.clock.monotonic();
        s.turn(at, sync(s.starts.len()));
        let busy = s.clock.monotonic() - before;
        free = free.max(at + busy);
        // A call arrives during each sync, so the writer turns again at its ready time.
        s.push();
        at = timed(Some(s.w.sync_ready_at().max(at)), free, 50 * US).unwrap();
    }
    s
}
