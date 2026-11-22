//! A seeded start against the start from the whole log (`spec.md` §3 Order of a start): from one copy of the files
//! each, after randomised histories with restarts, crashes, power losses, damage, late and future readings,
//! revisions and consumers at every cursor, both give the same state, reads, deliveries and loss reports, and decide
//! later readings alike.

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

use super::WHOLE;
use crate::consume::{Condition, Funnel, Select, Start};
use crate::durable::derived::SeriesFold;
use crate::durable::scan::{SCAN_ORDERS, ScanOrder};
use crate::env::{Env, FixedBootId, ManualClock};
use crate::health::LossKind;
use crate::provisional::ProvisionalOptions;
use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
use crate::time::{TimeRange, Timestamp};
use crate::types::{Intent, Quality, Reading, SeriesId, Value};
use crate::{LogConfig, Store, StoreConfig};

const T0: i64 = 1_767_225_600_000_000_000;
const S: i64 = 1_000_000_000;
const SERIES: u32 = 3;
const CONSUMERS: [&str; 2] = ["c0", "c1"];

fn block_on<F: Future>(f: F) -> F::Output {
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

fn next_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!("seeded/{}", N.fetch_add(1, Ordering::SeqCst)))
}

fn log_config() -> LogConfig {
    LogConfig {
        segment_size: 16 << 20,
        ..LogConfig::default()
    }
}

fn specs() -> Vec<SeriesSpec> {
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
        gauge("a/two/v").versioned(ChangedAtKind::Version),
        gauge("a/three/v"),
    ]
}

#[derive(Clone, Debug)]
enum Step {
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
}

fn readings() -> impl Strategy<Value = Vec<(u32, i64, u8, u8, bool)>> {
    let one = (
        1..=SERIES,
        prop_oneof![6 => 0i64..3, 2 => -1_200i64..0, 1 => 60i64..600],
        0u8..6,
        0u8..4,
        prop::bool::weighted(0.1),
    );
    prop::collection::vec(one, 1..5)
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => (readings(), prop::bool::weighted(0.1)).prop_map(|(r, v)| Step::Emit(r, v)),
        3 => prop_oneof![1u64..5, 290u64..400, 900u64..1_200].prop_map(Step::Advance),
        2 => (0usize..2, 0u8..5).prop_map(|(c, k)| Step::Route(c, k)),
        3 => (0usize..2, 0usize..6).prop_map(|(c, n)| Step::Drain(c, n)),
        1 => Just(Step::Restart),
        1 => Just(Step::Kill),
        1 => Just(Step::PowerLoss),
        3 => any::<usize>().prop_map(Step::Directory),
        1 => any::<usize>().prop_map(Step::Damage),
    ]
}

/// A history's store, run on its own file system.
struct World {
    fs: MemFs,
    clock: ManualClock,
    env: Env,
    dir: PathBuf,
    store: Option<Store>,
    boot: u8,
}

fn env_on(fs: &MemFs, clock: &ManualClock, boot: u8) -> Env {
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(fs.clone());
    env.boot = Arc::new(FixedBootId::new(Some([boot; 16])));
    env
}

fn open(dir: &Path, env: &Env, create: bool) -> Store {
    block_on(Store::open(
        StoreConfig::new(dir, "Etc/UTC")
            .create_if_missing(create)
            .log(log_config())
            .env(env.clone()),
    ))
    .unwrap()
}

impl World {
    fn new() -> World {
        let fs = MemFs::new();
        let clock = ManualClock::new(Timestamp(T0));
        let env = env_on(&fs, &clock, 1);
        let dir = next_dir();
        let store = open(&dir, &env, true);
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
        }
    }

    fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }

    fn shut(&mut self) {
        if let Some(s) = self.store.take() {
            let _ = block_on(s.shutdown());
        }
    }

    fn reopen(&mut self) {
        self.env = env_on(&self.fs, &self.clock, self.boot);
        self.store = Some(open(&self.dir, &self.env, false));
    }

    fn kill(&mut self) {
        self.fs.crash_after(0);
        drop(self.store.take());
        self.fs.restart();
    }

    fn power_loss(&mut self) {
        self.fs.crash_after(0);
        drop(self.store.take());
        self.fs.power_loss();
        self.fs.restart();
        self.boot = self.boot.wrapping_add(1).max(1);
    }

    fn step(&mut self, s: &Step) {
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
                damage(&self.fs, &self.env, &self.dir, *k);
                self.reopen();
            }
        }
    }
}

fn to_readings(now: i64, rs: &[(u32, i64, u8, u8, bool)]) -> Vec<Reading> {
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

fn funnel(k: u8) -> Funnel {
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
        _ => cond(2, Condition::held_for(40.0, Duration::from_secs(5))),
    }
}

fn reader(env: &Env, dir: &Path) -> LogReader {
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
fn write_directory(env: &Env, dir: &Path, k: usize) {
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

/// Damages the batch holding a fact that `k` picks.
fn damage(fs: &MemFs, env: &Env, dir: &Path, k: usize) {
    let mut log = reader(env, dir);
    let Ok(u) = log.refresh() else { return };
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
        return;
    };
    let Some(segment) = fs.paths().into_iter().find(|p| {
        p.starts_with(dir.join("log"))
            && p.to_string_lossy().contains(&format!("{:016x}", b.file_id))
    }) else {
        return;
    };
    fs.poke(&segment, b.offset as usize + 40, b"XXXXXXXX");
}

/// `s` without the addresses a waker's debug form shows.
fn no_pointers(s: &str) -> String {
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
struct Seen {
    out: Vec<String>,
    seeded: bool,
    /// A loss the start's pass did not find was reported after it.
    unrecorded: bool,
}

#[allow(clippy::too_many_arguments)]
/// Opens a copy of `fs`, from the whole log or seeded, and records what the store gives, then what it decides and
/// delivers for `later` readings emitted after `advance` seconds.
fn observe(
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
    SCAN_ORDERS.lock().unwrap().push((dir.to_path_buf(), order));
    WHOLE.set(whole);
    let store = open(dir, &env, false);
    WHOLE.set(false);
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
    out.push(format!("health {:?}", store.health()));
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
    out.push(format!("by time {late:?}"));
    drop(consumers);
    drop(by_time);
    drop(stream);
    let _ = block_on(store.shutdown());
    Seen {
        out,
        seeded,
        unrecorded,
    }
}

/// Runs `steps` and compares the starts on copies of the files it leaves: the seeded one with `order` against the one
/// from the whole log. Returns whether the seeded start was seeded.
fn compare(
    steps: &[Step],
    end: End,
    order: ScanOrder,
    later: &[(u32, i64, u8, u8, bool)],
    start_back: i64,
) -> Result<bool, TestCaseError> {
    let mut w = World::new();
    for s in steps {
        w.step(s);
    }
    match end {
        End::Shutdown => w.shut(),
        End::Kill => w.kill(),
        End::PowerLoss => w.power_loss(),
    }
    let at = w.clock.wall().0;
    let start_at = at.saturating_sub(start_back * S);
    let whole = observe(&w.fs, &w.dir, w.boot, at, true, order, later, start_at);
    let seeded = observe(&w.fs, &w.dir, w.boot, at, false, order, later, start_at);
    prop_assert!(!whole.seeded);
    if order == ScanOrder::Background && seeded.unrecorded {
        // A loss below the seed found after the open: only the reports are promised alike (Q-1110).
        prop_assert_eq!(&whole.out[0], &seeded.out[0]);
    } else {
        for (a, b) in whole.out.iter().zip(&seeded.out) {
            prop_assert_eq!(a, b);
        }
        prop_assert_eq!(whole.out.len(), seeded.out.len());
    }
    Ok(seeded.seeded)
}

/// How a history ends before the starts are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Shutdown,
    Kill,
    PowerLoss,
}

fn end() -> impl Strategy<Value = End> {
    prop_oneof![Just(End::Shutdown), Just(End::Kill), Just(End::PowerLoss)]
}

static SEEDED: AtomicU64 = AtomicU64::new(0);

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).map_or(32, |n: u32| (n / 16).max(8)),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_seeded_start_gives_what_the_start_from_the_whole_log_gives(
        steps in prop::collection::vec(step(), 1..30),
        end in end(),
        before in any::<bool>(),
        later in readings(),
        start_back in 0i64..3_000,
    ) {
        let order = if before { ScanOrder::BeforeOpen } else { ScanOrder::Background };
        if compare(&steps, end, order, &later, start_back)? {
            SEEDED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[test]
fn a_store_with_a_directory_starts_seeded_and_gives_what_the_whole_log_gives() {
    let mut steps = vec![Step::Route(0, 0), Step::Route(1, 3)];
    for k in 0..20 {
        steps.push(Step::Emit(
            vec![(1, 0, k % 6, 0, false), (2, -5, 3, k % 4, false)],
            false,
        ));
        steps.push(Step::Advance(2));
        if k == 8 {
            steps.push(Step::Drain(0, 3));
        }
    }
    steps.push(Step::Directory(usize::MAX - 3));
    steps.push(Step::Emit(vec![(3, 0, 1, 0, false)], false));
    let later = [
        (1, 0, 4, 0, false),
        (2, -5, 3, 1, false),
        (2, -5, 3, 3, false),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        for end in [End::Shutdown, End::Kill, End::PowerLoss] {
            // Derived files are never synced (A.18): a power loss may take the directory, and the start reads the
            // whole log.
            let seeded = compare(&steps, end, order, &later, 30).unwrap();
            assert!(seeded || end == End::PowerLoss, "{order:?} {end:?}");
        }
    }
}

#[test]
fn a_late_revision_behind_a_fact_that_moves_the_window_is_checked_against_its_whole_key() {
    let e = |id, dt, v| Step::Emit(vec![(id, dt, v, 0, false)], false);
    let steps = vec![
        e(1, 0, 0),
        e(1, 0, 0),
        e(2, 0, 0),
        e(1, 0, 1),
        e(3, 0, 0),
        e(1, 1, 0),
        e(1, 0, 2),
        Step::Route(0, 0),
        Step::Directory(738048058492601894),
        Step::Advance(290),
        Step::Emit(vec![(2, 0, 0, 0, false), (2, -901, 0, 0, false)], false),
    ];
    compare(
        &steps,
        End::Shutdown,
        ScanOrder::Background,
        &[(1, 0, 0, 0, false)],
        0,
    )
    .unwrap();
}

#[test]
fn a_consumer_behind_the_seed_with_no_batch_after_it_is_replayed_from_the_log() {
    let e = |id, v, revise| Step::Emit(vec![(id, 0, v, 0, false)], revise);
    let steps = vec![
        Step::Route(0, 0),
        e(3, 0, false),
        e(1, 0, false),
        e(1, 1, false),
        e(1, 0, true),
        Step::Directory(usize::MAX),
    ];
    for order in [ScanOrder::Background, ScanOrder::BeforeOpen] {
        assert!(compare(&steps, End::Shutdown, order, &[(1, 0, 0, 0, false)], 0).unwrap());
    }
}
