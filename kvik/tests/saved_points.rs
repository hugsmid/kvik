//! Saved points over histories with power losses and damage (`spec.md` §7 Readers, Reset stamp and saved points):
//! the durable backend's `Reader::check` and `Start::Position` give the model backend's answers, which are the
//! model's `check_point`; an answer never rises; a consumer started at a point is sent the reset points above it
//! before any fact.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::world::World;
use common::{at, block_on, config_on, scanned};
use kvik::env::Env;
use kvik::{
    Backend, ConsumerError, Delivery, DeliveryKind, Funnel, Kind, MarkSync, ModelFault, PointCheck,
    Position, Reader, ReaderConfig, Reading, ResetStamp, ResyncCause, SavedPoint, Select, SeriesId,
    SeriesSpec, Start, Store, StoreConfig, Value,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

const DAY: Duration = Duration::from_secs(86_400);

#[derive(Clone, Debug)]
enum Step {
    Emit,
    /// A definition: a batch that is not facts, after which both backends sync the mark (Q-630).
    Sync,
    /// A power loss losing up to this many of the batches of facts written since the mark was last synced.
    PowerLoss(usize),
    /// A crash on the same boot with a batch written since the last start damaged: the start's recovery entry if
    /// asked and there is one, else the fact chosen by the index.
    Damage(usize, bool),
    /// Saves the point this many positions below the last position, on each backend under its own stamp.
    Save(u8),
    Check,
    /// Starts a new consumer at the saved point chosen by the index.
    Start(usize),
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => Just(Step::Emit),
        2 => Just(Step::Sync),
        2 => (0usize..4).prop_map(Step::PowerLoss),
        2 => (any::<usize>(), any::<bool>()).prop_map(|(i, e)| Step::Damage(i, e)),
        3 => (0u8..6).prop_map(Step::Save),
        2 => Just(Step::Check),
        1 => any::<usize>().prop_map(Step::Start),
    ]
}

/// What a history drew, for the coverage guard.
#[derive(Default, Debug)]
struct Seen {
    kept: u64,
    void_by_entry: u64,
    void_by_loss: u64,
    unknown: u64,
    started: u64,
}

struct Saved {
    durable: SavedPoint,
    model: SavedPoint,
    last: Option<PointCheck>,
}

struct Rig {
    w: World,
    model: StoreConfig,
    d: Option<Store>,
    m: Option<Store>,
    ids: (SeriesId, SeriesId),
    texts: BTreeMap<Position, String>,
    since_sync: Vec<Position>,
    since_start: Vec<Position>,
    /// The recovery entry the last start wrote, with its segment and the bytes of its batch.
    entry: Option<(Position, PathBuf, usize, usize)>,
    /// A recovery entry was damaged: later restarts and the delivery comparison are left out (Q-1631).
    entry_lost: bool,
    damaged: Vec<Position>,
    saved: Vec<Saved>,
    n: u64,
}

/// Facts by position, with their values printed.
type Facts = Vec<(Position, String)>;

static RUN: AtomicU64 = AtomicU64::new(0);

fn note(id: SeriesId, secs: i64, text: &str) -> Reading {
    Reading::new(id, at(secs), Value::Text(text.into()))
}

fn spec(name: &str) -> SeriesSpec {
    SeriesSpec::new(name, Kind::Text, "")
}

/// Ranks an answer for a point at `p`: `Kept` is void above `p` itself, `Unknown` the lowest (S3).
fn rank(c: PointCheck, p: Position) -> Option<Position> {
    match c {
        PointCheck::Kept => Some(p),
        PointCheck::VoidAbove(x) => Some(x),
        PointCheck::Unknown => None,
    }
}

impl Rig {
    fn new() -> Rig {
        let run = RUN.fetch_add(1, Ordering::Relaxed);
        let mut w = World::quiet(&format!("saved-{run}"));
        w.mark_sync = MarkSync::Interval(DAY);
        let mut model = config_on(Backend::Model, &format!("saved-{run}"), &w.clock);
        model.log.seal_interval = DAY;
        model.log.mark_sync = MarkSync::Interval(DAY);
        let mut rig = Rig {
            w,
            model,
            d: None,
            m: None,
            ids: (SeriesId(0), SeriesId(0)),
            texts: BTreeMap::new(),
            since_sync: Vec::new(),
            since_start: Vec::new(),
            entry: None,
            entry_lost: false,
            damaged: Vec::new(),
            saved: Vec::new(),
            n: 0,
        };
        rig.open();
        assert!(kvik::model_fault(&rig.model.dir, ModelFault::TimedMark));
        assert!(kvik::model_fault(&rig.model.dir, ModelFault::LongSegments));
        rig.restart_model();
        let d = block_on(rig.d().define_series(spec("a/b/note"))).unwrap();
        let m = block_on(rig.m().define_series(spec("a/b/note"))).unwrap();
        rig.ids = (d, m);
        rig
    }

    fn d(&self) -> &Store {
        self.d.as_ref().unwrap()
    }

    fn m(&self) -> &Store {
        self.m.as_ref().unwrap()
    }

    fn durable_config(&self) -> StoreConfig {
        let mut c = self.w.config();
        c.log.seal_interval = DAY;
        c
    }

    fn open(&mut self) {
        self.d = Some(block_on(Store::open(self.durable_config())).unwrap());
        scanned(self.d());
        if self.m.is_none() {
            self.m = Some(block_on(Store::open(self.model.clone())).unwrap());
        }
    }

    fn restart_model(&mut self) {
        drop(self.m.take());
        self.m = Some(block_on(Store::open(self.model.clone())).unwrap());
    }

    fn readers(&self) -> (Reader, Reader) {
        let mut env = Env::test(self.w.clock.clone());
        env.fs = self.w.fs.clone();
        env.boot = self.w.boot.clone();
        let d = Reader::open(ReaderConfig::new(&self.w.dir).env(env)).unwrap();
        let m = Reader::open(ReaderConfig::new(&self.model.dir).backend(Backend::Model)).unwrap();
        (d, m)
    }

    fn emit(&mut self) -> Position {
        self.n += 1;
        self.w.clock.advance(Duration::from_secs(1));
        let text = format!("F{:06}X", self.n);
        let secs = self.n as i64;
        let d = block_on(self.d().emit(&[note(self.ids.0, secs, &text)])).unwrap();
        let m = block_on(self.m().emit(&[note(self.ids.1, secs, &text)])).unwrap();
        let (d, m) = (d.first().unwrap(), m.first().unwrap());
        assert_eq!(d, m, "both backends assign one position");
        self.texts.insert(d, text);
        self.since_sync.push(d);
        self.since_start.push(d);
        d
    }

    /// The end of the non-zero bytes of each segment file.
    fn ends(&self) -> BTreeMap<PathBuf, usize> {
        self.w
            .fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(&self.w.dir)
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .filter_map(|p| Some((p.clone(), nonzero_end(&self.w.fs.read(&p)?))))
            .collect()
    }

    fn segments_holding(&self, text: &[u8]) -> Vec<(PathBuf, usize)> {
        self.w
            .fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(&self.w.dir)
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .filter_map(|p| {
                let b = self.w.fs.read(&p)?;
                let i = b.windows(text.len()).position(|x| x == text)?;
                Some((p, i))
            })
            .collect()
    }

    /// The facts each backend's reader returns, by position.
    fn facts(&self) -> (Facts, Facts) {
        let (rd, rm) = self.readers();
        let read = |r: &Reader, id| {
            r.facts(id, at(-1)..at(1_000_000), kvik::View::History)
                .unwrap()
                .map(|f| {
                    let f = f.unwrap();
                    (f.position, format!("{:?}", f.value))
                })
                .collect::<Vec<_>>()
        };
        (read(&rd, self.ids.0), read(&rm, self.ids.1))
    }

    fn after_start(&mut self) {
        self.since_sync.clear();
        self.since_start.clear();
        self.entry = None;
        let (d, m) = self.facts();
        assert_eq!(d, m, "both backends keep the same facts");
        self.texts.retain(|p, _| d.iter().any(|(q, _)| q == p));
        let (rd, rm) = self.readers();
        assert_eq!(rd.refresh().unwrap().durable, rm.refresh().unwrap().durable);
    }

    fn step(&mut self, step: &Step, seen: &mut Seen) {
        match *step {
            Step::Emit => {
                self.emit();
            }
            Step::Sync => {
                self.n += 1;
                let name = format!("a/s{}/note", self.n);
                block_on(self.d().define_series(spec(&name))).unwrap();
                block_on(self.m().define_series(spec(&name))).unwrap();
                self.since_sync.clear();
            }
            Step::PowerLoss(_) | Step::Damage(..) if self.entry_lost => {}
            Step::PowerLoss(k) => {
                let n = k.min(self.since_sync.len());
                let cut = (n > 0).then(|| {
                    let first = self.since_sync[self.since_sync.len() - n];
                    self.segments_holding(self.texts[&first].as_bytes())
                });
                let d = self.d.take().unwrap();
                self.w.crash(d, Vec::new(), true);
                for (path, i) in cut.into_iter().flatten() {
                    let len = self.w.fs.read(&path).map_or(0, |b| b.len());
                    if len > i {
                        self.w.fs.poke(&path, i, &vec![0u8; len - i]);
                    }
                }
                drop(self.m.take());
                assert!(kvik::model_fault(
                    &self.model.dir,
                    ModelFault::PowerLossLosing(n)
                ));
                let ends = self.ends();
                self.open();
                self.after_start();
                let h = self.d().stats().log.durable;
                self.entry = self.ends().into_iter().find_map(|(path, end)| {
                    let was = ends.get(&path).copied().unwrap_or(0);
                    (end > was).then_some((h, path, was, end))
                });
            }
            Step::Damage(i, entry) => {
                let (f, held) = match self.entry.clone() {
                    Some((at, path, from, to)) if entry => {
                        self.entry_lost = true;
                        (at, vec![(path, (from + to) / 2)])
                    }
                    _ if self.since_start.is_empty() => return,
                    _ => {
                        let f = self.since_start[i % self.since_start.len()];
                        (f, self.segments_holding(self.texts[&f].as_bytes()))
                    }
                };
                let d = self.d.take().unwrap();
                self.w.crash(d, Vec::new(), false);
                for (path, i) in held {
                    self.w.fs.poke(&path, i, b"XXXX");
                }
                drop(self.m.take());
                assert!(kvik::model_fault(&self.model.dir, ModelFault::Damage(f)));
                self.open();
                self.damaged.push(f);
                self.after_start();
            }
            Step::Save(j) => {
                let (rd, rm) = self.readers();
                let (sd, sm) = (rd.refresh().unwrap(), rm.refresh().unwrap());
                assert_eq!(sd.durable, sm.durable);
                assert_eq!(
                    sd.stamp == ResetStamp::default(),
                    sm.stamp == ResetStamp::default()
                );
                let position = Position {
                    generation: sd.durable.generation,
                    sequence: sd.durable.sequence.saturating_sub(u64::from(j)),
                };
                self.saved.push(Saved {
                    durable: SavedPoint {
                        position,
                        stamp: sd.stamp,
                    },
                    model: SavedPoint {
                        position,
                        stamp: sm.stamp,
                    },
                    last: None,
                });
            }
            Step::Check => {
                let (rd, rm) = self.readers();
                for s in &mut self.saved {
                    let got = rd.check(s.durable).unwrap();
                    assert_eq!(got, rm.check(s.model).unwrap(), "S2: {:?}", s.durable);
                    let p = s.durable.position;
                    // S3 bounds what follows a void; after `Unknown` the holder has started again.
                    if let Some(last) = s.last.filter(|l| *l != PointCheck::Unknown) {
                        assert!(
                            rank(got, p) <= rank(last, p),
                            "S3: {last:?} then {got:?} for {p:?}"
                        );
                    }
                    s.last = Some(got);
                    match got {
                        PointCheck::Kept => seen.kept += 1,
                        PointCheck::VoidAbove(x) if x == p => seen.kept += 1,
                        PointCheck::VoidAbove(x) => {
                            let next = Position {
                                generation: x.generation,
                                sequence: x.sequence + 1,
                            };
                            if self.damaged.contains(&next) {
                                seen.void_by_loss += 1;
                            } else {
                                seen.void_by_entry += 1;
                            }
                        }
                        PointCheck::Unknown => seen.unknown += 1,
                    }
                }
            }
            Step::Start(i) => {
                if self.saved.is_empty() {
                    return;
                }
                let s = &self.saved[i % self.saved.len()];
                let (dp, mp) = (s.durable, s.model);
                let (rd, _) = self.readers();
                let answer = rd.check(dp).unwrap();
                self.n += 1;
                let name = format!("c{}", self.n);
                // The last position a start is lowered to (Q-1401): where a consumer started now begins.
                let probe = format!("n{}", self.n);
                let now = block_on(self.d().consumer(&probe, Start::Now, Select::None)).unwrap();
                let head = self.d().consumer_stats(&probe).unwrap().cursor;
                drop(now);
                let d = block_on(self.d().consumer(&name, Start::Position(dp), Select::None));
                let m = block_on(self.m().consumer(&name, Start::Position(mp), Select::None));
                let want = match answer {
                    PointCheck::Unknown => {
                        assert!(matches!(d, Err(ConsumerError::UnknownPoint)), "{d:?}");
                        assert!(matches!(m, Err(ConsumerError::UnknownPoint)), "{m:?}");
                        return;
                    }
                    PointCheck::Kept => dp.position.min(head),
                    PointCheck::VoidAbove(x) => x.min(head),
                };
                let (mut d, mut m) = (d.unwrap(), m.unwrap());
                seen.started += 1;
                let cursor = |s: &Store| s.consumer_stats(&name).unwrap().cursor;
                assert_eq!(
                    cursor(self.d()),
                    want,
                    "S2: the start takes the check's answer"
                );
                assert_eq!(cursor(self.m()), want);
                block_on(d.route(Funnel::facts(Select::All))).unwrap();
                block_on(m.route(Funnel::facts(Select::All))).unwrap();
                let mut got = (drain_all(&mut d), drain_all(&mut m));
                self.emit();
                got.0.extend(drain_all(&mut d));
                got.1.extend(drain_all(&mut m));
                // Q-1630: a reset point with nothing to forget at the consumer's start may be on one side only.
                let vacuous = |d: &Delivery| {
                    matches!(&d.kind, DeliveryKind::Resync(r)
                        if r.cause == ResyncCause::ResetPoint && r.forget_above.is_some_and(|fa| fa >= want))
                };
                got.0.retain(|d| !vacuous(d));
                got.1.retain(|d| !vacuous(d));
                // Q-1631: once a recovery entry is lost the two differ on its lost range's reset point.
                if !self.entry_lost {
                    assert_eq!(summary(&got.0), summary(&got.1), "S5 against the model");
                }
                let first_fact = got
                    .0
                    .iter()
                    .position(|d| matches!(d.kind, DeliveryKind::Fact(_)));
                for (i, d) in got.0.iter().enumerate() {
                    if let DeliveryKind::Resync(r) = &d.kind
                        && r.cause == ResyncCause::ResetPoint
                    {
                        assert!(
                            first_fact.is_none_or(|f| i < f),
                            "S5: a reset point after a fact"
                        );
                        assert!(r.forget_above.is_some_and(|fa| fa <= d.position));
                    }
                }
                drop((d, m));
                self.since_sync.clear();
            }
        }
    }
}

impl Drop for Rig {
    /// A history that panics still shuts its durable store, whose threads otherwise keep the world's files alive.
    fn drop(&mut self) {
        if let Some(d) = self.d.take() {
            let _ =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on(d.shutdown())));
        }
    }
}

fn drain_all(c: &mut kvik::Consumer) -> Vec<Delivery> {
    let mut out = Vec::new();
    for _ in 0..2 {
        while let Some(d) = c.try_next().unwrap() {
            block_on(c.confirm(d.id)).unwrap();
            out.push(d);
            assert!(
                out.len() < 1_000,
                "deliveries without end: {:?}",
                summary(&out[..8])
            );
        }
    }
    out
}

fn summary(ds: &[Delivery]) -> Vec<String> {
    ds.iter()
        .map(|d| match &d.kind {
            DeliveryKind::Fact(f) => format!("fact {:?}", f.position),
            DeliveryKind::Resync(r) => {
                format!("resync {:?} {:?} {:?}", d.position, r.cause, r.forget_above)
            }
            other => format!("{other:?}"),
        })
        .collect()
}

fn run(steps: &[Step], seen: &mut Seen) {
    let mut rig = Rig::new();
    for s in steps {
        rig.step(s, seen);
    }
    rig.step(&Step::Check, seen);
    if let Some(d) = rig.d.take() {
        block_on(d.shutdown()).unwrap();
    }
}

/// S2, S3, S5 over histories of emits, mark syncs, power losses losing acknowledged batches, damage a start records
/// as a lost range, saves, checks and starts at saved points.
#[test]
fn saved_points_follow_the_model_over_power_losses_and_damage() {
    // Each shrink step replays a whole history; bounded so a failure ends in minutes.
    let mut runner = TestRunner::new(ProptestConfig {
        max_shrink_iters: 64,
        ..ProptestConfig::default()
    });
    let seen = std::cell::RefCell::new(Seen::default());
    runner
        .run(&proptest::collection::vec(arb_step(), 1..40), |steps| {
            run(&steps, &mut seen.borrow_mut());
            Ok(())
        })
        .unwrap();
    let seen = seen.into_inner();
    let cases = u64::from(runner.config().cases);
    if cases >= 256 {
        assert!(
            seen.kept > 0
                && seen.void_by_entry > 0
                && seen.void_by_loss > 0
                && seen.unknown > 0
                && seen.started > 0,
            "coverage: {seen:?}"
        );
    }
}

fn next(p: Position) -> Position {
    Position {
        generation: p.generation,
        sequence: p.sequence + 1,
    }
}

fn nonzero_end(b: &[u8]) -> usize {
    b.iter().rposition(|x| *x != 0).map_or(0, |i| i + 1)
}

impl Rig {
    /// The answer for saved point `i` on each backend, asserted equal.
    fn check(&self, i: usize) -> PointCheck {
        let (rd, rm) = self.readers();
        let got = rd.check(self.saved[i].durable).unwrap();
        assert_eq!(got, rm.check(self.saved[i].model).unwrap());
        got
    }
}

/// Row 7b: a reader in an export's role keeps (P, stamp); a power loss losing acknowledged batches voids it above K,
/// and it continues from K and reads the reused positions; a later reset voids the new point above itself; once the
/// recovery entry the mark names is lost the answer is unknown. The model's `row_07b_…` step for step.
#[test]
fn row_07b_a_kept_point_is_void_above_k_after_a_power_loss_then_unknown_once_its_entry_is_lost() {
    let mut rig = Rig::new();
    let mut seen = Seen::default();
    rig.emit();
    rig.step(&Step::Sync, &mut seen);
    let k = rig.d().stats().log.durable;
    rig.emit();
    let p = rig.emit();
    rig.step(&Step::Save(0), &mut seen);
    assert_eq!(rig.saved[0].durable.position, p);
    assert_eq!(rig.check(0), PointCheck::Kept, "nothing reset yet");
    rig.step(&Step::PowerLoss(2), &mut seen);
    assert_eq!(rig.check(0), PointCheck::VoidAbove(k));
    let entry = rig.d().stats().log.durable;
    assert_eq!(entry, next(k), "the recovery entry follows K");
    let again = rig.emit();
    assert_eq!(again, next(entry));
    assert!(
        again <= p,
        "a position the export held names a different fact"
    );
    rig.step(&Step::Save(0), &mut seen);
    assert_eq!(rig.check(1), PointCheck::Kept);
    rig.step(&Step::Sync, &mut seen);
    let text = rig.texts[&again].clone();
    let (path, _) = rig.segments_holding(text.as_bytes()).pop().unwrap();
    let end0 = nonzero_end(&rig.w.fs.read(&path).unwrap());
    rig.step(&Step::PowerLoss(0), &mut seen);
    assert_eq!(
        rig.check(1),
        PointCheck::VoidAbove(again),
        "a later reset below nothing it holds"
    );
    let entry = rig.d().stats().log.durable;
    let end1 = nonzero_end(&rig.w.fs.read(&path).unwrap());
    assert!(end1 > end0, "the second entry is in the same segment");
    rig.emit();
    let d = rig.d.take().unwrap();
    rig.w.crash(d, Vec::new(), false);
    rig.w.fs.poke(&path, (end0 + end1) / 2, b"XXXX");
    drop(rig.m.take());
    assert!(kvik::model_fault(&rig.model.dir, ModelFault::Damage(entry)));
    rig.open();
    rig.after_start();
    assert_eq!(
        rig.check(1),
        PointCheck::Unknown,
        "the reset the mark names is lost"
    );
}

/// Q-1630: a lost range at the tail, above the last kept record, is a reset point the model sends to a consumer
/// started at the position before it, a resync with nothing to forget; the durable backend's last position is below
/// the range, so it sends none. Pinned for both start kinds until the two agree.
#[test]
fn a_lost_range_at_the_tail_gives_a_new_consumer_a_resync_with_nothing_to_forget_on_the_model_only()
{
    let mut rig = Rig::new();
    let mut seen = Seen::default();
    rig.emit();
    rig.step(&Step::Damage(0, false), &mut seen);
    rig.step(&Step::Save(0), &mut seen);
    let s = &rig.saved[0];
    let starts = [
        ("now", Start::Now, Start::Now),
        ("kept", Start::Position(s.durable), Start::Position(s.model)),
    ];
    for (name, sd, sm) in starts {
        let mut d = block_on(rig.d().consumer(name, sd, Select::None)).unwrap();
        let mut m = block_on(rig.m().consumer(name, sm, Select::None)).unwrap();
        let x = rig.d().consumer_stats(name).unwrap().cursor;
        assert_eq!(x, rig.m().consumer_stats(name).unwrap().cursor);
        let (dd, dm) = (drain_all(&mut d), drain_all(&mut m));
        assert!(dd.is_empty(), "{:?}", summary(&dd));
        assert!(
            matches!(&dm[..], [r] if matches!(&r.kind, DeliveryKind::Resync(r)
                if r.cause == ResyncCause::ResetPoint && r.forget_above == Some(x))),
            "{:?}",
            summary(&dm)
        );
    }
}

/// Q-1631: after a start's recovery entry is lost to damage and the store restarts, a later power loss leaves the
/// two backends at different last positions. Recovery, not the saved-point check; the history property stops
/// restarting once an entry is lost.
#[test]
#[ignore = "Q-1631: open"]
fn a_power_loss_after_a_lost_recovery_entry_keeps_the_model_s_last_position() {
    use Step::*;
    let steps = [Save(0), PowerLoss(0), Damage(0, true), Emit, Start(0)];
    let mut seen = Seen::default();
    let mut rig = Rig::new();
    for s in &steps {
        rig.step(s, &mut seen);
    }
    rig.entry_lost = false;
    rig.step(&PowerLoss(1), &mut seen);
}

/// Spec §3 Recovery rule 2: a batch damaged next to one recorded lost before lies in the same invalid region and
/// grows that loss, so its reset point forgets from below both; a consumer started between them is sent it.
#[test]
fn a_batch_damaged_next_to_a_recorded_loss_grows_it_and_a_start_between_them_meets_its_reset_point()
{
    use Step::*;
    let steps = [
        Emit,
        Damage(0, false),
        Emit,
        Save(1),
        Sync,
        Damage(0, false),
        Emit,
        Start(0),
    ];
    let mut rig = Rig::new();
    let mut seen = Seen::default();
    for s in &steps {
        rig.step(s, &mut seen);
    }
    // The model backend lists lost-range records unfolded; the model's own fold is pinned in `model/tests`.
    let d: Vec<_> = rig.d().losses().iter().map(|l| (l.first, l.last)).collect();
    let at = |sequence| {
        Some(Position {
            generation: 0,
            sequence,
        })
    };
    assert_eq!(d, [(at(2), at(3))], "one grown loss");
    assert_eq!(seen.started, 1);
}
