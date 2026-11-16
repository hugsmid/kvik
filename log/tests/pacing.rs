//! The adaptive batch-sync interval (spec §3 Group commit, Q-310, Q-712): the synchronous writer core driven with
//! explicit times, as in `cadence.rs`, and a manual clock that each sync advances by a scripted duration from the
//! file system's sync hook, against a reference schedule written out on its own.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{FaultOp, MemFs};
use kvik_log::store::{MARK, NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource, Clock, IngestQueue, Log, LogWriter, MarkSync, WriterConfig, create_segment,
};
use proptest::prelude::*;

const T0: i64 = 1_767_225_600_000_000_000;
const US: Duration = Duration::from_micros(1);
const MS: Duration = Duration::from_millis(1);
const CEILING: Duration = Duration::from_millis(10);
const FLOOR: Duration = Duration::from_millis(2);
const FAST: Duration = Duration::from_micros(160);
const START: Duration = Duration::from_secs(1);
const NO_SEAL: Duration = Duration::from_secs(300);
/// A timed mark sync no test reaches, so that only batch syncs set the writer's deadlines.
const NO_MARK: MarkSync = MarkSync::Interval(Duration::from_secs(3600));
/// How long after its arrival a call wakes the writer, unless a test says otherwise.
const WAKE: Duration = Duration::from_micros(20);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct ManualClock(Mutex<Duration>);

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *lock(&self.0)
    }
    fn wall(&self) -> i64 {
        T0
    }
}

/// How long the next segment and mark syncs take, while armed.
#[derive(Debug, Default)]
struct SyncTimes {
    armed: bool,
    segment: Duration,
    mark: Duration,
}

fn fact(t: i64) -> Item {
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
struct Setup {
    floor: Duration,
    ceiling: Duration,
    seal: Duration,
    clock: bool,
    mark: MarkSync,
}

impl Setup {
    fn adaptive() -> Self {
        Self {
            floor: FLOOR,
            ceiling: CEILING,
            seal: NO_SEAL,
            clock: true,
            mark: NO_MARK,
        }
    }
    fn fixed() -> Self {
        Self {
            clock: false,
            ..Self::adaptive()
        }
    }
    fn config(&self) -> WriterConfig {
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

struct Sim {
    fs: MemFs,
    setup: Setup,
    w: LogWriter,
    q: IngestQueue,
    clock: Arc<ManualClock>,
    times: Arc<Mutex<SyncTimes>>,
    /// Start of every batch sync.
    starts: Vec<Duration>,
    /// Start of every log sync, a timed seal's included.
    log_syncs: Vec<Duration>,
    /// Start of the sync that covered each call, by call id.
    covered: BTreeMap<u64, Duration>,
    /// The interval in force after each batch sync.
    intervals: Vec<Duration>,
    /// Turns after which the next batch sync may wait more than the ceiling after the previous one: the turn's time
    /// and the writer's ready time.
    ceiling_breaks: Vec<(Duration, Duration)>,
    pushed: i64,
}

impl Sim {
    fn new(setup: Setup) -> Self {
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

    fn with(
        fs: MemFs,
        setup: Setup,
        clock: Arc<ManualClock>,
        times: Arc<Mutex<SyncTimes>>,
    ) -> Self {
        let w = Self::open_writer(&fs, setup, &clock);
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
            pushed: 0,
        };
        s.create();
        s
    }

    fn open_writer(fs: &MemFs, setup: Setup, clock: &Arc<ManualClock>) -> LogWriter {
        let store = StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), None).unwrap();
        let mut w = LogWriter::open(store, setup.config(), Some([9; 16]), START, T0).unwrap();
        if setup.clock {
            w.set_sync_clock(clock.clone());
        }
        w
    }

    /// Shuts the writer down and opens it again on the same files.
    fn reopen(mut self, now: Duration) -> Self {
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

    fn create(&mut self) {
        if let Some(req) = self.w.segment_request(T0) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    fn push(&mut self) -> u64 {
        self.push_items(vec![fact(T0 + self.pushed + 1)])
    }

    fn push_items(&mut self, items: Vec<Item>) -> u64 {
        self.pushed += 1;
        let call = Call::new(items, self.w.config().call_limit()).unwrap();
        self.q.push(call).unwrap()
    }

    /// One turn of the writer at `now`, a segment sync taking `segment` and a mark sync `mark`: its next deadline
    /// and how long its syncs kept it busy.
    fn turn_with(
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
            if self.w.sync_ready_at() > (batch + c).max(log + c / 2) {
                self.ceiling_breaks.push((now, self.w.sync_ready_at()));
            }
        }
        (step.next, busy)
    }

    fn turn(&mut self, now: Duration, segment: Duration) -> (Option<Duration>, Duration) {
        self.turn_with(now, segment, Duration::ZERO)
    }
}

/// When the writer thread next turns: at once if busy past its deadline, else `late` after the deadline.
fn timed(next: Option<Duration>, free: Duration, late: Duration) -> Option<Duration> {
    next.map(|t| if t <= free { free } else { t + late })
}

/// A generated history: calls at `(arrival, wake)` (the writer is woken `wake` after the arrival, in order of
/// arrival), timed wakes `late` after each deadline, and the `k`-th batch sync taking `syncs[k % len]`.
#[derive(Clone, Debug)]
struct History {
    calls: Vec<(Duration, Duration)>,
    late: Duration,
    syncs: Vec<Duration>,
}

impl History {
    fn sync(&self, k: usize) -> Duration {
        self.syncs[k % self.syncs.len()]
    }
}

/// The start of each batch sync and the start of the sync covering each call, in order of arrival.
#[derive(Debug, PartialEq, Eq)]
struct Schedule {
    starts: Vec<Duration>,
    covered: Vec<Duration>,
}

/// Runs `h` on the writer.
fn simulate(s: &mut Sim, h: &History) -> Schedule {
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
fn reference(h: &History, floor: Duration, ceiling: Duration, clocked: bool) -> Schedule {
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

fn arb_gap() -> impl Strategy<Value = Duration> {
    prop_oneof![
        1 => Just(Duration::ZERO),
        3 => (0u64..3_000).prop_map(|u| US * u as u32),
        3 => (0u64..15_000).prop_map(|u| US * u as u32),
        1 => (20u64..60).prop_map(|m| MS * m as u32),
    ]
}

fn arb_wake() -> impl Strategy<Value = Duration> {
    prop_oneof![6 => Just(WAKE), 1 => (0u64..3_000).prop_map(|u| US * u as u32)]
}

fn arb_sync() -> impl Strategy<Value = Duration> {
    prop_oneof![
        4 => (0u64..300).prop_map(|u| US * u as u32),
        2 => (300u64..2_000).prop_map(|u| US * u as u32),
        1 => (2_000u64..30_000).prop_map(|u| US * u as u32),
    ]
}

fn arb_history() -> impl Strategy<Value = History> {
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
fn arb_bounds() -> impl Strategy<Value = (Duration, Duration)> {
    prop_oneof![
        8 => (1u64..=20, 0u64..20).prop_map(|(c, f)| (US * (500 * f.min(2 * c - 1)) as u32, MS * c as u32)),
        1 => (0u64..=10).prop_map(|f| (MS * f as u32, Duration::ZERO)),
    ]
}

fn run(setup: Setup, h: &History) -> (Sim, Schedule) {
    let mut s = Sim::new(setup);
    let out = simulate(&mut s, h);
    (s, out)
}

proptest! {
    #[test]
    fn the_writer_agrees_with_the_reference_schedule((floor, ceiling) in arb_bounds(), h in arb_history()) {
        let setup = Setup { floor, ceiling, ..Setup::adaptive() };
        let (_, got) = run(setup, &h);
        prop_assert_eq!(got, reference(&h, floor, ceiling, true));
    }

    #[test]
    fn any_n_plus_1_batch_syncs_span_at_least_n_floors_less_one_ceiling(
        (floor, ceiling) in arb_bounds(),
        h in arb_history(),
    ) {
        let (_, got) = run(Setup { floor, ceiling, ..Setup::adaptive() }, &h);
        let st = &got.starts;
        for i in 0..st.len() {
            for j in i + 1..st.len() {
                let bound = (floor.min(ceiling) * (j - i) as u32).saturating_sub(ceiling);
                prop_assert!(st[j] - st[i] >= bound, "syncs {} to {} span {:?}", i, j, st[j] - st[i]);
            }
        }
    }

    #[test]
    fn no_batch_sync_starts_within_half_the_floor_of_a_log_sync_a_seal_included(
        (floor, ceiling) in arb_bounds(),
        seal_ms in 30u64..400,
        h in arb_history(),
    ) {
        let seal = MS * seal_ms as u32;
        let (s, _) = run(Setup { floor, ceiling, seal, ..Setup::adaptive() }, &h);
        prop_assert!(s.w.stats().sealed_segments == 0, "seal jobs are not run here");
        for pair in s.log_syncs.windows(2) {
            if s.starts.contains(&pair[1]) {
                prop_assert!(pair[1] - pair[0] >= floor.min(ceiling) / 2, "log syncs at {:?}", pair);
            }
        }
    }

    #[test]
    fn a_queued_batch_never_waits_more_than_the_ceiling_after_the_previous_batch_sync(
        (floor, ceiling) in arb_bounds(),
        seal_ms in 30u64..400,
        h in arb_history(),
    ) {
        let seal = MS * seal_ms as u32;
        let (s, _) = run(Setup { floor, ceiling, seal, ..Setup::adaptive() }, &h);
        prop_assert!(s.ceiling_breaks.is_empty(), "{:?}", s.ceiling_breaks);
    }

    #[test]
    fn with_the_floor_at_the_ceiling_or_no_clock_the_schedule_is_the_fixed_one(
        ceiling_ms in 0u64..=20,
        above in 0u64..20,
        below in 0u64..20,
        h in arb_history(),
    ) {
        let ceiling = MS * ceiling_ms as u32;
        let fixed = reference(&h, ceiling, ceiling, false);
        let at_or_above = Setup { floor: ceiling + MS * above as u32, ceiling, ..Setup::adaptive() };
        prop_assert_eq!(&run(at_or_above, &h).1, &fixed);
        let no_clock = Setup { floor: MS * below as u32, ceiling, ..Setup::fixed() };
        prop_assert_eq!(&run(no_clock, &h).1, &fixed);
    }

    #[test]
    fn the_smoothed_sync_time_stays_between_the_last_sample_and_the_largest(
        syncs in prop::collection::vec(arb_sync(), 1..60),
    ) {
        let mut s = Sim::new(Setup::adaptive());
        let mut at = START;
        let mut largest = Duration::ZERO;
        for (k, &d) in syncs.iter().enumerate() {
            s.push();
            s.turn(at, d);
            largest = largest.max(d);
            let est = s.w.stats().sync_time.unwrap();
            prop_assert!(d <= est && est <= largest, "after sync {}: {:?} of {:?}", k, est, d);
            let i = s.w.stats().sync_interval;
            prop_assert_eq!(i, s.w.sync_interval());
            prop_assert!(FLOOR <= i && i <= CEILING, "interval {:?}", i);
            at += Duration::from_secs(1);
        }
    }

    #[test]
    fn after_a_peak_the_estimate_falls_at_least_an_eighth_of_the_way_per_sync(
        peak_us in 0u64..50_000,
        level_us in 0u64..50_000,
        m in 0usize..60,
    ) {
        let (peak, level) = (peak_us.max(level_us), peak_us.min(level_us));
        let mut s = Sim::new(Setup::adaptive());
        let mut at = START;
        for d in std::iter::once(peak).chain(std::iter::repeat_n(level, m)) {
            s.push();
            s.turn(at, US * d as u32);
            at += Duration::from_secs(1);
        }
        let est = s.w.stats().sync_time.unwrap().as_nanos() as f64;
        let bound = 1e3 * (level as f64 + (peak - level) as f64 * (7.0f64 / 8.0).powi(m as i32)) + m as f64;
        prop_assert!(est <= bound, "{} ns above {} ns", est, bound);
    }
}

/// One call every `period` from `START`, with each batch sync taking `sync`: each call's wait.
fn periodic(setup: Setup, n: usize, period: Duration, sync: Duration) -> (Sim, Vec<Duration>) {
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
fn saturated(setup: Setup, n: usize, sync: impl Fn(usize) -> Duration) -> Sim {
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

#[test]
fn a_fast_device_at_one_call_per_5_ms_syncs_every_call_at_once() {
    let (s, waits) = periodic(Setup::adaptive(), 200, 5 * MS, FAST);
    assert!(waits.iter().all(|w| *w <= WAKE), "{:?}", waits.iter().max());
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    let (_, waits) = periodic(Setup::fixed(), 200, 5 * MS, FAST);
    assert!(
        waits.iter().filter(|w| **w > WAKE).count() >= 90,
        "the fixed interval delays every other call"
    );
}

#[test]
fn a_20_ms_device_runs_the_fixed_schedule() {
    let slow = 20 * MS;
    for period in [3 * MS, 10 * MS, 25 * MS] {
        let (a, wa) = periodic(Setup::adaptive(), 150, period, slow);
        let (f, wf) = periodic(Setup::fixed(), 150, period, slow);
        assert_eq!(
            (&a.starts, &wa),
            (&f.starts, &wf),
            "one call every {period:?}"
        );
        assert!(a.intervals.iter().all(|i| *i == CEILING));
    }
}

#[test]
fn a_device_of_half_a_millisecond_runs_at_eight_times_its_sync_time() {
    let s = saturated(Setup::adaptive(), 50, |_| 500 * US);
    assert_eq!(s.w.stats().sync_interval, 4 * MS);
    let gaps: Vec<_> = s.starts[10..].windows(2).map(|p| p[1] - p[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= 4 * MS && *g < 4 * MS + 100 * US),
        "{gaps:?}"
    );
}

#[test]
fn a_single_2_ms_sync_holds_the_ceiling_for_four_syncs() {
    let s = saturated(
        Setup::adaptive(),
        80,
        |k| if k == 40 { 2 * MS } else { FAST },
    );
    assert_eq!(s.intervals[39], FLOOR);
    assert_eq!(&s.intervals[40..44], &[CEILING; 4]);
    assert!(s.intervals[44] < CEILING && s.intervals[44] > FLOOR);
    let back = s.intervals[40..].iter().position(|i| *i == FLOOR).unwrap();
    assert_eq!(back, 23);
}

#[test]
fn a_single_25_ms_stall_holds_the_ceiling_for_24_syncs_and_the_floor_returns_after_43() {
    let s = saturated(
        Setup::adaptive(),
        120,
        |k| if k == 40 { 25 * MS } else { FAST },
    );
    assert_eq!(s.intervals[39], FLOOR);
    let at_ceiling = s.intervals[40..]
        .iter()
        .take_while(|i| **i == CEILING)
        .count();
    assert_eq!(at_ceiling, 24);
    let back = s.intervals[40..].iter().position(|i| *i == FLOOR).unwrap();
    assert_eq!(back, 43);
    assert_eq!(
        s.starts[41] - s.starts[40],
        25 * MS,
        "the next batch starts as the stall returns"
    );
    for (k, p) in s.starts[41..40 + at_ceiling].windows(2).enumerate() {
        assert!(
            p[1] - p[0] >= CEILING / 2,
            "syncs {} and {} after the stall",
            k + 1,
            k + 2
        );
    }
    let held = &s.starts[41..=40 + at_ceiling];
    for i in 0..held.len() {
        for j in i + 1..held.len() {
            assert!(
                held[j] - held[i] >= CEILING * (j - i - 1) as u32,
                "the fixed schedule's rate bound"
            );
        }
    }
}

#[test]
fn a_failed_sync_leaves_the_estimate_alone() {
    let mut s = saturated(Setup::adaptive(), 30, |_| FAST);
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    s.fs.fail_nth(FaultOp::Sync, ".open", 0, io::ErrorKind::Other);
    s.push();
    let at = *s.starts.last().unwrap() + CEILING;
    s.turn(at, 25 * MS);
    assert!(s.w.fault().is_some(), "the sync failed");
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    assert_eq!(s.w.sync_interval(), FLOOR);
}

#[test]
fn a_mark_sync_leaves_the_estimate_alone() {
    for mark in [NO_MARK, MarkSync::EveryCommit] {
        let mut s = Sim::new(Setup {
            mark,
            ..Setup::adaptive()
        });
        let mut at = START;
        for k in 0..20 {
            if k % 2 == 1 || mark == MarkSync::EveryCommit {
                s.push();
            } else {
                // A definition is not a fact: its batch forces a mark sync.
                s.push_items(vec![Item::Entry(Entry::Alias {
                    id: k,
                    name: format!("a{k}"),
                })]);
            }
            s.turn_with(at, FAST, 25 * MS);
            at += Duration::from_secs(1);
        }
        assert!(s.w.stats().mark_syncs >= 10, "{mark:?}");
        assert_eq!(s.w.stats().sync_time, Some(FAST), "{mark:?}");
        assert_eq!(s.w.stats().sync_interval, FLOOR, "{mark:?}");
    }
}

#[test]
fn a_seal_sync_spaces_the_next_batch_by_half_the_interval_and_leaves_the_estimate_alone() {
    let mut s = Sim::new(Setup {
        seal: Duration::from_secs(2),
        ..Setup::adaptive()
    });
    let mut at = START;
    while s.starts.len() < 30 {
        s.push();
        s.turn(at, FAST);
        at += 5 * MS;
    }
    let (seal_at, _) = s.turn(at, FAST);
    let seal_at = seal_at.unwrap();
    assert_eq!(seal_at, START + Duration::from_secs(2));
    let syncs = s.w.stats().syncs;
    s.turn(seal_at, 25 * MS);
    assert_eq!(
        s.w.stats().syncs,
        syncs + 1,
        "the seal synced the next segment"
    );
    assert_eq!(s.w.stats().sync_time, Some(FAST));
    s.push();
    let (next, _) = s.turn(seal_at + 100 * US, FAST);
    assert_eq!(next, Some(seal_at + FLOOR / 2));
}

#[test]
fn the_interval_is_the_ceiling_after_an_open_until_the_first_batch_sync() {
    let s = saturated(Setup::adaptive(), 20, |_| FAST);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
    let at = *s.starts.last().unwrap() + Duration::from_secs(1);
    let mut s = s.reopen(at);
    assert_eq!(s.w.sync_interval(), CEILING);
    assert_eq!(s.w.stats().sync_interval, CEILING);
    assert_eq!(s.w.stats().sync_time, None);
    s.push();
    s.turn(at, FAST);
    assert_eq!(s.w.stats().sync_interval, FLOOR);
}

#[test]
fn a_floor_above_the_ceiling_fixes_the_interval_at_the_ceiling() {
    let s = saturated(
        Setup {
            floor: 20 * MS,
            ..Setup::adaptive()
        },
        20,
        |_| FAST,
    );
    assert!(s.intervals.iter().all(|i| *i == CEILING));
    assert_eq!(s.w.stats().sync_time, Some(FAST));
}

#[test]
fn a_zero_ceiling_disables_the_bound_whatever_the_floor() {
    let s = saturated(
        Setup {
            ceiling: Duration::ZERO,
            ..Setup::adaptive()
        },
        20,
        |_| FAST,
    );
    assert!(s.intervals.iter().all(|i| i.is_zero()));
    assert!(
        s.starts.windows(2).all(|p| p[1] - p[0] == FAST),
        "each sync starts as the last returns"
    );
}

#[test]
fn log_start_times_batch_syncs_on_its_clock() {
    let fs = MemFs::new();
    let new = NewStore {
        store_id: [7; 16],
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    let store = StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), Some(&new)).unwrap();
    let clock = Arc::new(ManualClock::default());
    // Creation syncs each segment once; a later sync of the same segment is a batch's.
    let seen = Arc::new(Mutex::new(BTreeMap::<PathBuf, u32>::new()));
    let (c, seen2) = (clock.clone(), seen.clone());
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        let mut seen = lock(&seen2);
        let n = seen.entry(p.to_path_buf()).or_insert(0);
        *n += 1;
        if *n >= 2 && p.extension().is_some_and(|e| e == "open") {
            *lock(&c.0) += 500 * US;
        }
    })));
    let w = LogWriter::open(
        store,
        Setup::adaptive().config(),
        Some([9; 16]),
        Duration::ZERO,
        T0,
    )
    .unwrap();
    let log = Log::start(w, clock).unwrap();
    log.submit(vec![fact(T0)]).unwrap().wait().unwrap();
    let stats = log.stats();
    assert_eq!(stats.sync_time, Some(500 * US));
    assert_eq!(stats.sync_interval, 4 * MS);
    log.shutdown().unwrap();
}
