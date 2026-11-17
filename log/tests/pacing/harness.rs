//! The harness of `pacing.rs`: the writer core driven with explicit times and a manual clock, calls that arrive by
//! that clock (also while a sync is in flight), the generated histories and the reference schedule.

use std::collections::{BTreeMap, VecDeque};
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
    pub fn base() -> Self {
        Self {
            floor: FLOOR,
            ceiling: CEILING,
            seal: NO_SEAL,
            clock: true,
            mark: NO_MARK,
        }
    }
    pub fn unclocked() -> Self {
        Self {
            clock: false,
            ..Self::base()
        }
    }
    /// The spacing the writer keeps under load (Q-981).
    pub fn spacing(&self) -> Duration {
        self.floor.min(self.ceiling)
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

/// The writer's call source: the queue, fed with each scheduled call once the manual clock reaches its arrival.
struct Arriving<'a> {
    q: &'a mut IngestQueue,
    pending: &'a mut VecDeque<(Duration, Vec<Item>)>,
    ids: &'a mut Vec<u64>,
    clock: &'a ManualClock,
    limit: usize,
}

impl Arriving<'_> {
    fn arrived(&self) -> bool {
        let now = self.clock.monotonic();
        self.pending.front().is_some_and(|(t, _)| *t <= now)
    }
}

impl CallSource for Arriving<'_> {
    fn is_empty(&self) -> bool {
        self.q.is_empty() && !self.arrived()
    }
    fn take(&mut self, max_bytes: u64) -> Vec<Call> {
        while self.arrived() {
            let (_, items) = self.pending.pop_front().unwrap();
            let call = Call::new(items, self.limit).unwrap();
            self.ids.push(self.q.push(call).unwrap());
        }
        self.q.take(max_bytes)
    }
    fn put_back(&mut self, calls: Vec<Call>) {
        self.q.put_back(calls);
    }
}

pub struct Sim {
    pub fs: MemFs,
    pub setup: Setup,
    pub w: LogWriter,
    pub q: IngestQueue,
    pub clock: Arc<ManualClock>,
    pub times: Arc<Mutex<SyncTimes>>,
    /// Calls not yet in the queue, by arrival time.
    pub pending: VecDeque<(Duration, Vec<Item>)>,
    /// Call ids in the order the calls entered the queue.
    pub ids: Vec<u64>,
    /// Start of every turn.
    pub turns: Vec<Duration>,
    /// Start of every batch sync.
    pub starts: Vec<Duration>,
    /// End of the turn of every batch sync, when its calls are acknowledged.
    pub ends: Vec<Duration>,
    /// Whether calls were waiting as each batch sync's turn ended.
    pub waited: Vec<bool>,
    /// Start of every log sync, a timed seal's included.
    pub log_syncs: Vec<Duration>,
    /// Whether each log sync was a batch's.
    pub log_sync_is_batch: Vec<bool>,
    /// Start of the sync that covered each call, by call id.
    pub covered: BTreeMap<u64, Duration>,
    /// When each call was acknowledged, by call id.
    pub acked: BTreeMap<u64, Duration>,
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
            pending: VecDeque::new(),
            ids: Vec::new(),
            turns: Vec::new(),
            starts: Vec::new(),
            ends: Vec::new(),
            waited: Vec::new(),
            log_syncs: Vec::new(),
            log_sync_is_batch: Vec::new(),
            covered: BTreeMap::new(),
            acked: BTreeMap::new(),
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

    fn next_fact(&mut self) -> Vec<Item> {
        self.pushed += 1;
        vec![fact(T0 + self.pushed)]
    }

    /// Queues a call now.
    pub fn push(&mut self) -> u64 {
        let items = self.next_fact();
        self.push_items(items)
    }

    pub fn push_items(&mut self, items: Vec<Item>) -> u64 {
        let call = Call::new(items, self.w.config().call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.ids.push(id);
        id
    }

    /// Schedules a call that enters the queue once the manual clock reaches `at`, also during a turn.
    pub fn arrive(&mut self, at: Duration) {
        let items = self.next_fact();
        let i = self.pending.partition_point(|(t, _)| *t <= at);
        self.pending.insert(i, (at, items));
    }

    /// Whether a call is queued or has arrived by the manual clock.
    pub fn calls_waiting(&self) -> bool {
        let now = self.clock.monotonic();
        !self.q.is_empty() || self.pending.front().is_some_and(|(t, _)| *t <= now)
    }

    /// One turn of the writer at `now`, a segment sync taking `segment` and a mark sync `mark`: its next deadline
    /// and how long the turn kept it busy.
    pub fn turn_with(
        &mut self,
        now: Duration,
        segment: Duration,
        mark: Duration,
    ) -> (Option<Duration>, Duration) {
        assert!(
            now >= self.clock.monotonic(),
            "a turn before the last ended"
        );
        *lock(&self.clock.0) = now;
        let (batches, syncs) = (self.w.stats().batches, self.w.stats().syncs);
        *lock(&self.times) = SyncTimes {
            armed: true,
            segment,
            mark,
            write: self.write,
        };
        let limit = self.w.config().call_limit();
        let mut source = Arriving {
            q: &mut self.q,
            pending: &mut self.pending,
            ids: &mut self.ids,
            clock: &self.clock,
            limit,
        };
        let step = self.w.step(now, T0 + now.as_nanos() as i64, &mut source);
        lock(&self.times).armed = false;
        let end = self.clock.monotonic();
        self.turns.push(now);
        for (id, r) in step.done {
            match r {
                Ok(_) => {
                    self.covered.insert(id, now);
                    self.acked.insert(id, end);
                }
                Err(e) => assert!(self.w.fault().is_some(), "{e:?} with no fault"),
            }
        }
        let waited = self.calls_waiting();
        self.create();
        if self.w.stats().syncs > syncs {
            self.log_syncs.push(now);
            self.log_sync_is_batch
                .push(self.w.stats().batches > batches);
        }
        if self.w.stats().batches > batches {
            self.starts.push(now);
            self.ends.push(end);
            self.waited.push(waited);
        }
        (step.next, end - now)
    }

    pub fn turn(&mut self, now: Duration, segment: Duration) -> (Option<Duration>, Duration) {
        self.turn_with(now, segment, Duration::ZERO)
    }
}

/// When the writer thread next turns: at once if busy past its deadline, else `late` after the deadline.
pub fn timed(next: Option<Duration>, free: Duration, late: Duration) -> Option<Duration> {
    next.map(|t| if t <= free { free } else { t + late })
}

/// A generated history: calls at `(arrival, wake)` in order of arrival (a call enters the queue at its arrival, a
/// sync in flight or not, and the next call to arrive wakes an idle writer `wake` after it), timed wakes `late`
/// after each deadline, and the `k`-th batch sync taking `syncs[k % len]`.
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
    /// How many calls have arrived by `t`.
    pub fn arrived(&self, t: Duration) -> usize {
        self.calls.partition_point(|c| c.0 <= t)
    }
}

/// The start of each batch sync, and the start of the sync covering each call and its acknowledgement, in order of
/// arrival.
#[derive(Debug, PartialEq, Eq)]
pub struct Schedule {
    pub starts: Vec<Duration>,
    pub covered: Vec<Duration>,
    pub acked: Vec<Duration>,
}

/// When the writer thread next turns after being free at `free`: when the next call to arrive wakes it, or at its
/// deadline.
fn next_turn(h: &History, free: Duration, next: Option<Duration>) -> Duration {
    let k = h.arrived(free);
    let by_call = (k < h.calls.len()).then(|| h.calls[k].0 + h.calls[k].1);
    [by_call, timed(next, free, h.late)]
        .into_iter()
        .flatten()
        .min()
        .expect("a call to come or a deadline")
        .max(free)
}

/// Runs `h` on the writer.
pub fn simulate(s: &mut Sim, h: &History) -> Schedule {
    for &(t, _) in &h.calls {
        s.arrive(t);
    }
    let (mut free, mut next) = (s.clock.monotonic(), None);
    for _ in 0..100 * h.calls.len() + 100 {
        if s.pending.is_empty() && s.q.is_empty() {
            let at = |m: &BTreeMap<u64, Duration>| s.ids.iter().map(|id| m[id]).collect();
            return Schedule {
                starts: s.starts.clone(),
                covered: at(&s.covered),
                acked: at(&s.acked),
            };
        }
        let at = next_turn(h, free, next);
        let (n, busy) = s.turn(at, h.sync(s.starts.len()));
        free = at + busy;
        next = n;
    }
    panic!("the calls do not drain");
}

/// The reference schedule of spec §3 Group commit (Q-980): a batch sync starts as soon as the writer turns with a
/// call queued, unless calls were waiting as the previous batch sync returned; then not before `spacing` after
/// that sync started. It takes every call queued, and its calls are acknowledged as it returns. Driven as
/// [`simulate`] drives the writer.
pub fn reference(h: &History, spacing: Duration) -> Schedule {
    let n = h.calls.len();
    let ready = |last: Option<Duration>, waited: bool| match last {
        Some(s) if waited => s.saturating_add(spacing),
        _ => Duration::ZERO,
    };
    let (mut starts, mut covered, mut acked) =
        (Vec::new(), vec![Duration::ZERO; n], vec![Duration::ZERO; n]);
    let (mut taken, mut free, mut next) = (0, Duration::ZERO, None);
    let (mut last, mut waited) = (None, false);
    while taken < n {
        let at = next_turn(h, free, next);
        free = at;
        let queued = h.arrived(at);
        if queued > taken && at >= ready(last, waited) {
            let end = at + h.sync(starts.len());
            covered[taken..queued].fill(at);
            acked[taken..queued].fill(end);
            taken = queued;
            starts.push(at);
            last = Some(at);
            free = end;
            waited = h.arrived(end) > taken;
        }
        next = (h.arrived(free) > taken).then(|| ready(last, waited).max(at));
    }
    Schedule {
        starts,
        covered,
        acked,
    }
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

/// A floor and a ceiling, either above the other; zero now and then for each.
pub fn arb_bounds() -> impl Strategy<Value = (Duration, Duration)> {
    prop_oneof![
        6 => (0u64..=20, 1u64..=20).prop_map(|(f, c)| (US * (500 * f) as u32, MS * c as u32)),
        1 => (0u64..=10).prop_map(|f| (MS * f as u32, Duration::ZERO)),
        1 => (0u64..=20).prop_map(|c| (Duration::ZERO, MS * c as u32)),
    ]
}

pub fn run(setup: Setup, h: &History) -> (Sim, Schedule) {
    let mut s = Sim::new(setup);
    let out = simulate(&mut s, h);
    (s, out)
}

/// One call every `period` from `START`, with each batch sync taking `sync`: each call's wait from its arrival to
/// the start of the sync covering it.
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

/// Saturation: a call always queued and another arriving just after each turn starts, so one waits as every batch
/// sync returns; batch syncs take `sync(k)` (at least a nanosecond for the arrival to fall inside), and timed wakes
/// are 50 µs late.
pub fn saturated(setup: Setup, n: usize, sync: impl Fn(usize) -> Duration) -> Sim {
    let mut s = Sim::new(setup);
    let mut at = START;
    s.push();
    while s.starts.len() < n {
        assert!(s.turns.len() < 10 * n + 100, "the writer stopped syncing");
        s.arrive(at + Duration::from_nanos(1));
        let (next, busy) = s.turn(at, sync(s.starts.len()));
        at = timed(next, at + busy, 50 * US).unwrap();
    }
    s
}

/// A lone writer in a closed loop: each call arrives `turnaround` after the previous one is acknowledged and wakes
/// the writer at once; batch syncs take `sync`.
pub fn lone_writer(setup: Setup, n: usize, sync: Duration, turnaround: Duration) -> Sim {
    let mut s = Sim::new(setup);
    let mut at = START;
    for _ in 0..n {
        s.arrive(at);
        let (_, busy) = s.turn(at, sync);
        at += busy + turnaround;
    }
    s
}
