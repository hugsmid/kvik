//! The histories, starts and comparison the seeded start's tests share.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Wake};
use std::time::Duration;

use kvik_log::directory::series::SeriesDirectory;
use kvik_log::directory::{Family, Frame};
use kvik_log::fs::MemFs;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig};
use proptest::prelude::*;

use super::super::{PROBE, Probe, WHOLE};
use crate::consume::{Condition, Funnel, Select, Start};
use crate::durable::LOG_SKEW;
use crate::durable::derived::SeriesFold;
use crate::durable::scan::{SCAN_ORDERS, ScanOrder};
use crate::durable::start::GiveWay;
use crate::env::{Env, FixedBootId, ManualClock};
use crate::health::{Health, LossKind, Reason};
use crate::provisional::ProvisionalOptions;
use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
use crate::time::{TimeRange, Timestamp};
use crate::types::{Intent, Quality, Reading, SeriesId, Value};
use crate::{LogConfig, Store, StoreConfig};

pub(super) const T0: i64 = 1_767_225_600_000_000_000;
pub(super) const S: i64 = 1_000_000_000;
pub(super) const SERIES: u32 = 3;
pub(super) const CONSUMERS: [&str; 2] = ["c0", "c1"];

pub(super) fn block_on<F: Future>(f: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut f = std::pin::pin!(f);
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

pub(super) fn next_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!("seeded/{}", N.fetch_add(1, Ordering::SeqCst)))
}

/// Never reached on the wall clock while a history runs: segments seal only at [`Step::Seal`].
pub(super) const SEAL_INTERVAL: Duration = Duration::from_secs(3_600);

pub(super) fn log_config() -> LogConfig {
    LogConfig {
        segment_size: 16 << 20,
        seal_interval: SEAL_INTERVAL,
        ..LogConfig::default()
    }
}

pub(super) fn specs() -> Vec<SeriesSpec> {
    let gauge = |n: &str| {
        SeriesSpec::new(
            n,
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        )
    };
    vec![
        gauge("a/one/v"),
        gauge("a/two/v")
            .versioned(ChangedAtKind::Version)
            .staleness(Duration::from_secs(10)),
        gauge("a/three/v").staleness(Duration::from_secs(10)),
    ]
}

#[derive(Clone, Debug)]
pub(super) enum Step {
    /// Readings: series, seconds from now (negative late, positive ahead), value, `changed_at`, bad.
    Emit(Vec<(u32, i64, u8, u8, bool)>, bool),
    Advance(u64),
    Route(usize, u8),
    /// Takes the consumer's ready deliveries and confirms the first ones.
    Drain(usize, usize),
    Restart,
    /// The process stops; the files stay as the page cache holds them.
    Kill,
    PowerLoss,
    /// A series directory as of the batch chosen, written while the store is shut.
    Directory(usize),
    /// A batch chosen is damaged after a power loss.
    Damage(usize),
    /// The segment in use is due to be sealed, at the writer's next turn, if it holds a batch.
    Seal,
    /// A consumer's cursor file is lost with a power loss: a start that resets names it in its recovery entry.
    LoseCursor(usize),
}

pub(super) fn readings() -> impl Strategy<Value = Vec<(u32, i64, u8, u8, bool)>> {
    let one = (
        1..=SERIES,
        prop_oneof![6 => 0i64..3, 2 => -1_200i64..0, 1 => 60i64..600],
        0u8..6,
        0u8..4,
        prop::bool::weighted(0.1),
    );
    prop::collection::vec(one, 1..5)
}

pub(super) fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => (readings(), prop::bool::weighted(0.1)).prop_map(|(r, v)| Step::Emit(r, v)),
        3 => prop_oneof![1u64..5, 290u64..400, 900u64..1_200].prop_map(Step::Advance),
        2 => (0usize..2, 0u8..6).prop_map(|(c, k)| Step::Route(c, k)),
        3 => (0usize..2, 0usize..6).prop_map(|(c, n)| Step::Drain(c, n)),
        1 => Just(Step::Restart),
        1 => Just(Step::Kill),
        1 => Just(Step::PowerLoss),
        3 => any::<usize>().prop_map(Step::Directory),
        1 => any::<usize>().prop_map(Step::Damage),
        3 => Just(Step::Seal),
        1 => (0usize..2).prop_map(Step::LoseCursor),
    ]
}

/// A history's store, run on its own file system.
pub(super) struct World {
    pub(super) fs: MemFs,
    pub(super) clock: ManualClock,
    pub(super) env: Env,
    pub(super) dir: PathBuf,
    pub(super) store: Option<Store>,
    pub(super) boot: u8,
    /// Added to the log's monotonic clock: a [`Step::Seal`] moves it on by the seal interval.
    pub(super) skew: Arc<AtomicU64>,
    /// The series with facts in a batch [`Step::Damage`] damaged.
    pub(super) damaged: BTreeSet<u32>,
    /// The log's stats as a [`Step::Seal`] moved its clock on, until the next emit.
    pub(super) sealing: Option<kvik_log::writer::LogStats>,
}

pub(super) fn env_on(fs: &MemFs, clock: &ManualClock, boot: u8) -> Env {
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    env.boot = Arc::new(FixedBootId::new(Some([boot; 16])));
    env
}

pub(super) fn open(dir: &Path, env: &Env, create: bool) -> Store {
    block_on(Store::open(
        StoreConfig::new(dir, "Etc/UTC")
            .create_if_missing(create)
            .log(log_config())
            .env(env.clone()),
    ))
    .unwrap()
}

pub(super) fn open_skewed(dir: &Path, env: &Env, create: bool, skew: &Arc<AtomicU64>) -> Store {
    LOG_SKEW.set(Some(skew.clone()));
    let store = open(dir, env, create);
    LOG_SKEW.set(None);
    store
}

/// The log's stats of `store`'s open writer.
fn log_stats(store: &Store) -> Option<kvik_log::writer::LogStats> {
    let w = store.writer()?;
    let log = w
        .log
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    log.as_ref().map(kvik_log::writer::Log::stats)
}

/// Moves the log's clock on by the seal interval: the writer's next turn, at the next call, seals the segment in use
/// if it holds a batch, as a timed seal does. Returns the log's stats before.
pub(super) fn seal_due(store: &Store, skew: &AtomicU64) -> Option<kvik_log::writer::LogStats> {
    let before = log_stats(store);
    skew.fetch_add(
        u64::try_from(SEAL_INTERVAL.as_nanos()).unwrap_or(u64::MAX),
        Ordering::SeqCst,
    );
    before
}

/// Waits for the seal due since `before` to finish: an emit leaves the segment in use holding a batch, so the writer
/// seals it once the segment created ahead exists.
pub(super) fn sealed_since(store: &Store, before: &kvik_log::writer::LogStats) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while let Some(now) = log_stats(store) {
        if now.sealed_segments > before.sealed_segments
            || now.batches == before.batches
            || std::time::Instant::now() > deadline
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The sealed segments of the store in `dir`.
pub(super) fn sealed(fs: &MemFs, dir: &Path) -> usize {
    fs.paths()
        .iter()
        .filter(|p| p.starts_with(dir.join("log")) && p.extension().is_some_and(|e| e == "kvl"))
        .count()
}

impl World {
    pub(super) fn new() -> World {
        let fs = MemFs::new();
        let clock = ManualClock::new(Timestamp(T0));
        let env = env_on(&fs, &clock, 1);
        let dir = next_dir();
        let skew = Arc::new(AtomicU64::new(0));
        let store = open_skewed(&dir, &env, true, &skew);
        for spec in specs() {
            block_on(store.define_series(spec)).unwrap();
        }
        World {
            fs,
            clock,
            env,
            dir,
            store: Some(store),
            boot: 1,
            skew,
            damaged: BTreeSet::new(),
            sealing: None,
        }
    }

    pub(super) fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }

    pub(super) fn shut(&mut self) {
        if let Some(s) = self.store.take() {
            let _ = block_on(s.shutdown());
        }
    }

    pub(super) fn reopen(&mut self) {
        self.sealing = None;
        self.env = env_on(&self.fs, &self.clock, self.boot);
        self.store = Some(open_skewed(&self.dir, &self.env, false, &self.skew));
    }

    pub(super) fn kill(&mut self) {
        self.fs.crash_after(0);
        drop(self.store.take());
        self.fs.restart();
    }

    pub(super) fn power_loss(&mut self) {
        self.fs.crash_after(0);
        drop(self.store.take());
        self.fs.power_loss();
        self.fs.restart();
        self.boot = self.boot.wrapping_add(1).max(1);
    }

    pub(super) fn step(&mut self, s: &Step) {
        match s {
            Step::Emit(rs, revise) => {
                let now = self.clock.wall().0;
                let readings = to_readings(now, rs);
                let intent = if *revise {
                    Intent::Revise
                } else {
                    Intent::Normal
                };
                let _ = block_on(self.store().emit_with(&readings, intent));
                if let Some(before) = self.sealing.take() {
                    sealed_since(self.store(), &before);
                }
            }
            Step::Advance(secs) => {
                self.clock.advance(Duration::from_secs(*secs));
                let _ = self.store().tick();
            }
            Step::Route(c, k) => {
                if let Ok(h) = block_on(self.store().consumer(
                    CONSUMERS[*c],
                    Start::Now,
                    Select::None,
                )) {
                    let _ = block_on(h.route(funnel(*k)));
                }
            }
            Step::Drain(c, n) => {
                if let Ok(mut h) = block_on(self.store().consumer(
                    CONSUMERS[*c],
                    Start::Now,
                    Select::None,
                )) {
                    let mut got = Vec::new();
                    while let Ok(Some(d)) = h.try_next() {
                        got.push(d);
                    }
                    for d in got.iter().take(*n) {
                        let _ = block_on(h.confirm(d.id));
                    }
                }
            }
            Step::Restart => {
                self.shut();
                self.reopen();
            }
            Step::Kill => {
                self.kill();
                self.reopen();
            }
            Step::PowerLoss => {
                self.power_loss();
                self.reopen();
            }
            Step::Directory(k) => {
                self.shut();
                write_directory(&self.env, &self.dir, *k);
                self.reopen();
            }
            Step::Damage(k) => {
                self.power_loss();
                self.damaged
                    .extend(damage(&self.fs, &self.env, &self.dir, *k));
                self.reopen();
            }
            Step::LoseCursor(c) => {
                self.power_loss();
                let path = self
                    .dir
                    .join("cursors")
                    .join(format!("{}.cur", CONSUMERS[*c]));
                let _ = self.env.fs.remove_file(&path);
                self.reopen();
            }
            Step::Seal => {
                if self.sealing.is_none() {
                    self.sealing = seal_due(self.store(), &self.skew);
                }
            }
        }
    }
}

pub(super) fn to_readings(now: i64, rs: &[(u32, i64, u8, u8, bool)]) -> Vec<Reading> {
    rs.iter()
        .map(|&(id, secs, v, c, bad)| {
            let mut r = Reading::new(
                SeriesId(id),
                Timestamp(now.saturating_add(secs * S)),
                Value::F64(f64::from(v) * 20.0),
            );
            if bad {
                r = r.with_quality(Quality::Bad);
            }
            if id == 2 {
                r.changed_at = Some(u64::from(c));
            }
            r
        })
        .collect()
}

pub(super) fn funnel(k: u8) -> Funnel {
    let cond = |id, c| Funnel {
        series: Select::Ids(vec![SeriesId(id)]),
        operator: None,
        condition: Some(c),
    };
    match k {
        0 => Funnel::facts(Select::All),
        1 => Funnel::facts(Select::Ids(vec![SeriesId(2)])),
        2 => cond(1, Condition::above(50.0).clear_below(30.0)),
        3 => cond(3, Condition::stale_for(Duration::from_secs(30))),
        4 => cond(2, Condition::held_for(40.0, Duration::from_secs(5))),
        _ => cond(2, Condition::stale_for(Duration::from_secs(20))),
    }
}

pub(super) fn reader(env: &Env, dir: &Path) -> LogReader {
    let clock = Arc::new(kvik_log::writer::SystemClock::default());
    LogReader::open(LogReaderConfig::new(
        dir,
        env.fs.clone(),
        clock,
        env.boot.current(),
        [0xad; 16],
    ))
    .unwrap()
}

/// Writes a series directory as of the batch `k` picks, folded from the log as the writer folds it.
pub(super) fn write_directory(env: &Env, dir: &Path, k: usize) {
    let store_id = kvik_log::store::read_descriptor(&*env.fs, dir)
        .unwrap()
        .store_id;
    let mut log = reader(env, dir);
    let Ok(u) = log.refresh() else { return };
    let _ = log.close();
    let batches: Vec<_> = u
        .events
        .iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some(b),
            _ => None,
        })
        .collect();
    let Some(b) = batches.get(k % batches.len().max(1)) else {
        return;
    };
    let through = b.last;
    let fold = SeriesFold::new(through);
    for b in batches.iter().filter(|b| b.last <= through) {
        fold.batch(b);
    }
    let wall = env.clock.wall().0;
    let Some(e) = fold.entries(through, wall) else {
        return;
    };
    let st = u.state;
    let d = SeriesDirectory {
        through: e.through,
        lost: 0,
        stamp: (st.stamp != Default::default()).then_some(st.stamp),
        judged_at: e.judged_at,
        entries: e.entries,
    };
    let id = kvik_log::directory::generations(&*env.fs, dir, Family::Series)
        .first()
        .map_or(1, |(id, _)| id + 1);
    let frame = Frame {
        generation: st.generation,
        file_id: id,
        written_at: wall,
    };
    let bytes = d.encode(frame, store_id).unwrap();
    kvik_log::directory::write_file(&*env.fs, dir, Family::Series, id, &bytes).unwrap();
    // Unlike the writer's own (A.18), these outlive a power loss, so that seeded starts follow one.
    let path = dir.join("derived").join(Family::Series.file_name(id));
    if let Ok(f) = env.fs.open(&path, kvik_log::fs::OpenMode::ReadWrite) {
        let _ = f.sync_all();
    }
    let _ = env.fs.sync_dir(&dir.join("derived"));
}

/// Damages the batch holding a fact that `k` picks; returns the series of its facts.
pub(super) fn damage(fs: &MemFs, env: &Env, dir: &Path, k: usize) -> Vec<u32> {
    let mut log = reader(env, dir);
    let Ok(u) = log.refresh() else {
        return Vec::new();
    };
    let _ = log.close();
    let held: Vec<_> = u
        .events
        .iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some(b),
            _ => None,
        })
        .filter(|b| {
            b.content
                .items
                .iter()
                .any(|i| matches!(i, kvik_log::content::Item::Fact(_)))
        })
        .collect();
    let Some(b) = held.get(k % held.len().max(1)) else {
        return Vec::new();
    };
    let Some(segment) = fs.paths().into_iter().find(|p| {
        p.starts_with(dir.join("log"))
            && p.to_string_lossy().contains(&format!("{:016x}", b.file_id))
    }) else {
        return Vec::new();
    };
    fs.poke(&segment, b.offset as usize + 40, b"XXXXXXXX");
    b.content
        .items
        .iter()
        .filter_map(|i| match i {
            kvik_log::content::Item::Fact(f) => Some(f.series_id),
            _ => None,
        })
        .collect()
}

/// `s` without the addresses a waker's debug form shows.
pub(super) fn no_pointers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let n = tail
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(tail.len());
        rest = &tail[n..];
    }
    out.push_str(rest);
    out
}

/// What one start gives, in a comparable form, and whether it was seeded.
pub(super) struct Seen {
    pub(super) out: Vec<String>,
    pub(super) seeded: bool,
    /// A loss the start's pass did not find was reported after it.
    pub(super) unrecorded: bool,
    pub(super) probe: Probe,
}

#[allow(clippy::too_many_arguments)]
/// Opens a copy of `fs`, from the whole log or seeded, and records what the store gives, then what it decides and
/// delivers for `later` readings emitted after `advance` seconds.
pub(super) fn observe(
    fs: &MemFs,
    dir: &Path,
    boot: u8,
    at: i64,
    whole: bool,
    order: ScanOrder,
    later: &[(u32, i64, u8, u8, bool)],
    start_at: i64,
) -> Seen {
    let fs = fs.snapshot();
    let clock = ManualClock::new(Timestamp(at));
    let env = env_on(&fs, &clock, boot);
    SCAN_ORDERS.lock().unwrap().insert(dir.to_path_buf(), order);
    WHOLE.set(whole);
    PROBE.set(Probe::default());
    let store = open(dir, &env, false);
    WHOLE.set(false);
    let probe = PROBE.get();
    let w = store.writer().unwrap().clone();
    w.wait_scan();
    let stats = store.stats();
    let seeded = stats.ingest.start_seeded;
    let mut out = Vec::new();
    let mut losses = store.losses();
    losses.sort_by_key(|l| (l.identity, format!("{:?}", l.kind)));
    let unrecorded = losses
        .iter()
        .any(|l| l.kind == LossKind::DataLost && !l.recorded);
    out.push(format!("losses {losses:?}"));
    // The log's description of an unlisted sealed segment runs in the background and may not have named it yet.
    let health = match store.health() {
        Health::Degraded { reasons } => {
            let r: Vec<_> = reasons
                .into_iter()
                .filter(|r| !matches!(r, Reason::SegmentDamaged { .. }))
                .collect();
            if r.is_empty() {
                "Ok".to_string()
            } else {
                format!("Degraded {r:?}")
            }
        }
        h => format!("{h:?}"),
    };
    out.push(format!("health {health}"));
    for id in 1..=SERIES {
        out.push(format!(
            "series {id} {:?} {:?}",
            store.series(SeriesId(id)),
            store.series_stats(SeriesId(id))
        ));
        let r = store.reader();
        out.push(format!("live {id} {:?}", r.live(SeriesId(id))));
        let all = TimeRange {
            from: Timestamp(i64::MIN),
            to: Timestamp(i64::MAX),
        };
        out.push(format!(
            "facts {id} {:?}",
            r.facts(SeriesId(id), all, crate::read::View::History)
                .map(|f| f
                    .into_iter()
                    .map(|f| f.map(|f| (f.position, f.t)))
                    .collect::<Vec<_>>())
        ));
    }
    let mut stream = store.provisional(ProvisionalOptions::default()).ok();
    let mut consumers: Vec<_> = CONSUMERS
        .iter()
        .map(|c| block_on(store.consumer(c, Start::Now, Select::None)))
        .collect();
    let drain = |consumers: &mut Vec<Result<crate::Consumer, _>>, out: &mut Vec<String>| {
        for (i, c) in consumers.iter_mut().enumerate() {
            let Ok(c) = c else {
                out.push(format!("consumer {i} {c:?}"));
                continue;
            };
            let mut got = Vec::new();
            while let Ok(Some(d)) = c.try_next() {
                got.push(d);
            }
            for d in &got {
                let _ = block_on(c.confirm(d.id));
            }
            out.push(format!("consumer {i} {got:?}"));
        }
    };
    drain(&mut consumers, &mut out);
    let ack = block_on(store.emit_with(&to_readings(at, later), Intent::Normal));
    out.push(format!("ack {:?}", ack.map(|a| a.outcomes)));
    drain(&mut consumers, &mut out);
    if let Some(s) = stream.as_mut() {
        let mut events = Vec::new();
        while let Some(e) = s.try_next() {
            events.push(no_pointers(&format!("{e:?}")));
        }
        out.push(format!("stream {events:?}"));
    }
    clock.advance(Duration::from_secs(40));
    let _ = store.tick();
    drain(&mut consumers, &mut out);
    let mut by_time =
        block_on(store.consumer("late", Start::Time(Timestamp(start_at)), Select::None));
    if let Ok(c) = &by_time {
        let _ = block_on(c.route(Funnel::facts(Select::All)));
    }
    let mut late = Vec::new();
    if let Ok(c) = by_time.as_mut() {
        while let Ok(Some(d)) = c.try_next() {
            late.push(d);
        }
    }
    // Every delivery carries the time of the call that hands it out, the clock having moved on since the open.
    let called = clock.wall();
    out.push(format!(
        "by time at the call {}",
        late.iter().all(|d| d.delivered_at >= called)
    ));
    out.push(format!("by time {late:?}"));
    drop(consumers);
    drop(by_time);
    drop(stream);
    let _ = block_on(store.shutdown());
    SCAN_ORDERS.lock().unwrap().remove(dir);
    Seen {
        out,
        seeded,
        unrecorded,
        probe,
    }
}

/// What a compared seeded start met, for the measure of the test's reach.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Reach {
    pub(super) seeded: bool,
    /// Seeded over a log with a sealed segment.
    pub(super) sealed: bool,
    /// Seeded, and a loss below the seed was found after the open.
    pub(super) found_after: bool,
    /// The scan before the open met damage below the seed.
    pub(super) found_before: bool,
    pub(super) void: bool,
    /// The pass above the seed met a region at or below it.
    pub(super) below: bool,
    /// Recovery entries at or below the seed name a consumer.
    pub(super) recoveries: bool,
}

/// Runs `steps` and compares the starts on copies of the files it leaves: the seeded one with `order` against the one
/// from the whole log.
pub(super) fn compare(
    steps: &[Step],
    end: End,
    order: ScanOrder,
    later: &[(u32, i64, u8, u8, bool)],
    start_back: i64,
) -> Result<Reach, TestCaseError> {
    let mut w = World::new();
    for s in steps {
        w.step(s);
    }
    match end {
        End::Shutdown => w.shut(),
        End::Kill => w.kill(),
        End::PowerLoss => w.power_loss(),
        End::KillAt(k) => {
            w.step(&Step::Directory(k));
            w.kill();
        }
    }
    let at = w.clock.wall().0;
    let start_at = at.saturating_sub(start_back * S);
    let whole = observe(&w.fs, &w.dir, w.boot, at, true, order, later, start_at);
    let seeded = observe(&w.fs, &w.dir, w.boot, at, false, order, later, start_at);
    prop_assert!(!whole.seeded);
    let found_after = order == ScanOrder::Background && seeded.seeded && seeded.unrecorded;
    prop_assert_eq!(whole.out.len(), seeded.out.len());
    for (a, b) in whole.out.iter().zip(&seeded.out) {
        // A loss below the seed found after the open (Q-1110): the reports and every series without a fact in the
        // damage are alike.
        if found_after && !untouched(a, &w.damaged) {
            continue;
        }
        prop_assert_eq!(a, b);
    }
    let gave = seeded.probe.gave_way;
    Ok(Reach {
        seeded: seeded.seeded,
        sealed: seeded.seeded && sealed(&w.fs, &w.dir) > 0,
        found_after,
        found_before: order == ScanOrder::BeforeOpen && gave == Some(GiveWay::Below),
        void: gave == Some(GiveWay::Void),
        below: order == ScanOrder::Background && gave == Some(GiveWay::Below),
        recoveries: seeded.probe.recoveries_naming > 0,
    })
}

/// Whether a line of [`observe`]'s output is promised alike while a loss below the seed is found after the open: the
/// loss reports, and the state and reads of a series with no fact in the damage.
fn untouched(line: &str, damaged: &BTreeSet<u32>) -> bool {
    if line.starts_with("losses ") {
        return true;
    }
    ["series ", "live ", "facts "].iter().any(|p| {
        line.strip_prefix(p)
            .and_then(|r| r.split(' ').next())
            .and_then(|id| id.parse::<u32>().ok())
            .is_some_and(|id| !damaged.contains(&id))
    })
}

/// How a history ends before the starts are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    Shutdown,
    Kill,
    PowerLoss,
    /// A series directory as of the batch chosen, then the process stops: a seed with the log after it.
    KillAt(usize),
}

pub(super) fn end() -> impl Strategy<Value = End> {
    prop_oneof![
        Just(End::Shutdown),
        Just(End::Kill),
        Just(End::PowerLoss),
        any::<usize>().prop_map(End::KillAt),
    ]
}

/// The index of the newest batch, for [`write_directory`].
pub(super) fn newest_batch(env: &Env, dir: &Path) -> usize {
    let mut log = reader(env, dir);
    let n = log.refresh().map_or(0, |u| {
        u.events
            .iter()
            .filter(|e| matches!(e, ReadEvent::Batch(_)))
            .count()
    });
    let _ = log.close();
    n.saturating_sub(1)
}
