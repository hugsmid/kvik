//! Property tests of consumers on the durable backend (roadmap 5.2): deliveries match the model backend's over
//! clean restarts, and after crashes in the middle of a step and partial power losses they follow the
//! specification's rules (at least once, never ahead of a confirmation, reset points, transitions against latches).

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::world::{World, gauge, ready, resync};
use common::{block_on, start};
use kvik::env::{Env, ManualClock};
use kvik::{
    Backend, Condition, ConditionState, Consumer, Delivery, DeliveryId, DeliveryKind, Funnel,
    FunnelId, Outcome, Position, Reading, ResetStamp, ResyncCause, SavedPoint, Select, SeriesId,
    Start, Store, StoreConfig, Value,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

const NAMES: [&str; 4] = [
    "site/a/power",
    "site/b/power",
    "site/a/energy",
    "site/c/power",
];

#[derive(Clone, Debug)]
enum FunnelKind {
    Ids(usize),
    All,
    Pattern,
    Threshold(usize),
    OnChange(usize),
    HeldFor(usize),
    StaleFor(usize),
}

#[derive(Clone, Debug)]
enum Step {
    Emit(usize, u8),
    Advance(u64),
    Tick,
    Route(usize, FunnelKind),
    Unroute(usize, usize),
    /// Takes every ready delivery and confirms up to the one at this index, if any.
    Drain(usize, usize),
    Retake(usize),
    /// Binds the slot to a new consumer name started now, at a time or at an emitted fact, by the kind modulo 3.
    Register(usize, u8),
    /// A clean restart with the slot's cursor file unreadable at the start (Q-131).
    LoseCursor(usize),
    Restart,
    Define,
    Alias,
    /// For the crash test: the process stops after this many more file operations, in the middle of an emit and a
    /// drain, then the power goes too (a partial loss chosen by the seed) or not.
    Crash {
        ops: u8,
        power: bool,
        seed: u64,
    },
}

fn funnel_kind() -> impl Strategy<Value = FunnelKind> {
    prop_oneof![
        (0usize..4).prop_map(FunnelKind::Ids),
        Just(FunnelKind::All),
        Just(FunnelKind::Pattern),
        (0usize..3).prop_map(FunnelKind::Threshold),
        (0usize..3).prop_map(FunnelKind::OnChange),
        (0usize..3).prop_map(FunnelKind::HeldFor),
        (0usize..3).prop_map(FunnelKind::StaleFor),
    ]
}

fn steps(crashes: bool) -> impl Strategy<Value = Vec<Step>> {
    // A clean restart in the differential test; a crash in the test of the crash rules.
    let stop = (0u8..16, any::<bool>(), any::<u64>()).prop_map(move |(ops, power, seed)| {
        if crashes {
            Step::Crash { ops, power, seed }
        } else {
            Step::Restart
        }
    });
    let step = prop_oneof![
        10 => (0usize..4, any::<u8>()).prop_map(|(i, v)| Step::Emit(i, v)),
        4 => (0u64..3).prop_map(Step::Advance),
        // Beyond the recent window (15 minutes), so live state and held-for history are read from the log.
        1 => (1_000u64..3_000).prop_map(Step::Advance),
        1 => Just(Step::Tick),
        4 => (0usize..2, funnel_kind()).prop_map(|(c, k)| Step::Route(c, k)),
        1 => (0usize..2, 0usize..3).prop_map(|(c, n)| Step::Unroute(c, n)),
        5 => (0usize..2, 0usize..8).prop_map(|(c, n)| Step::Drain(c, n)),
        1 => (0usize..2).prop_map(Step::Retake),
        2 => (0usize..2, any::<u8>()).prop_map(|(c, k)| Step::Register(c, k)),
        1 => (0usize..2).prop_map(Step::LoseCursor),
        2 => stop,
        1 => Just(Step::Define),
        1 => Just(Step::Alias),
    ];
    prop::collection::vec(step, 1..40)
}

const CONSUMERS: [&str; 2] = ["c0", "c1"];

fn funnel(k: &FunnelKind, ids: &[SeriesId]) -> Funnel {
    let id = |i: usize| ids.get(i).copied().unwrap_or(SeriesId(999));
    let cond = |i: usize, c: Condition| Funnel {
        series: Select::Ids(vec![id(i)]),
        operator: None,
        condition: Some(c),
    };
    match k {
        FunnelKind::Ids(i) => Funnel::facts(Select::Ids(vec![id(*i)])),
        FunnelKind::All => Funnel::facts(Select::All),
        FunnelKind::Pattern => Funnel::facts(Select::Names("site/*/power".into())),
        FunnelKind::Threshold(i) => cond(*i, Condition::above(50.0).clear_below(30.0)),
        FunnelKind::OnChange(i) => cond(*i, Condition::on_change(40.0)),
        FunnelKind::HeldFor(i) => cond(*i, Condition::held_for(50.0, Duration::from_secs(2))),
        FunnelKind::StaleFor(i) => cond(*i, Condition::stale_for(Duration::from_secs(1))),
    }
}

/// One history's record on one backend: what it gave, in a comparable form, and the positions of the facts it
/// emitted, in order.
#[derive(Default)]
struct Record {
    out: Vec<String>,
    emitted: Vec<Position>,
    held: BTreeMap<String, Vec<Delivery>>,
}

impl Record {
    /// A position as the number of facts emitted at or before it: positions differ between the backends (the model
    /// backend writes routing entries when a pattern is evaluated again, the durable backend tzdata entries), the
    /// order of facts does not.
    fn ordinal(&self, p: Position) -> usize {
        self.emitted.iter().filter(|e| **e <= p).count()
    }

    fn normal(&self, d: &Delivery) -> String {
        let at = self.ordinal(d.position);
        match &d.kind {
            DeliveryKind::Fact(f) => format!(
                "fact @{} {} {} {:?} r={} o={} f={}",
                self.ordinal(f.position),
                f.series.0,
                f.t.0,
                f.value,
                d.flags.replayed,
                d.flags.outranked,
                d.flags.future
            ),
            DeliveryKind::Trigger(t) => {
                let sequence = match d.id {
                    DeliveryId::Condition { sequence, .. } => sequence,
                    _ => 0,
                };
                format!(
                    "trigger @{at} {}#{sequence} {:?} {:?}",
                    t.funnel.0, t.state, t.cause
                )
            }
            DeliveryKind::Resync(r) => {
                let fs: Vec<String> = r
                    .funnels
                    .iter()
                    .map(|f| {
                        // Live staleness is left out: the model backend judges it when it hands the resync out, the
                        // durable backend when it evaluates the resync, as the specification has it.
                        let live = match &f.live {
                            kvik::Live::Value(v) => format!("{} {:?}", v.fact.t.0, v.fact.value),
                            other => format!("{other:?}"),
                        };
                        format!(
                            "{} {} {:?} {} {live}",
                            f.funnel.0, f.series.0, f.condition, f.transition
                        )
                    })
                    .collect();
                format!(
                    "resync @{at} {:?} {:?} {}/{} [{}]",
                    r.cause,
                    r.forget_above.map(|p| self.ordinal(p)),
                    r.part,
                    r.last,
                    fs.join("; ")
                )
            }
            other => format!("{other:?}"),
        }
    }

    fn push(&mut self, name: &str, d: &Delivery) {
        let line = format!("{name}: {}", self.normal(d));
        if resync(d).is_some_and(|r| r.cause == ResyncCause::NoCursor) {
            NO_CURSOR.fetch_add(1, Ordering::Relaxed);
        }
        self.out.push(line);
    }

    /// Takes a consumer. A new one is sent no resync on either backend (Q-131); the first delivery of one taken
    /// again is taken at once on both and kept for the next drain to confirm.
    fn take(&mut self, s: &Store, name: &str) -> Option<Consumer> {
        self.take_at(s, name, Start::Now)
    }

    fn take_at(&mut self, s: &Store, name: &str, start: Start) -> Option<Consumer> {
        match block_on(s.consumer(name, start, Select::None)) {
            Ok(mut c) => {
                if let Some(d) = c.try_next().unwrap() {
                    self.push(name, &d);
                    self.held.entry(name.to_string()).or_default().push(d);
                }
                Some(c)
            }
            Err(e) => {
                self.out.push(format!("{name}: take {e:?}"));
                None
            }
        }
    }
}

/// What a history gave on one backend.
fn run(
    store: Store,
    reopen: &dyn Fn() -> Store,
    lose_cursor: &dyn Fn(&str),
    clock: &ManualClock,
    history: &[Step],
) -> Vec<String> {
    let mut rec = Record::default();
    let mut s = store;
    let mut ids: Vec<SeriesId> = NAMES[..3]
        .iter()
        .map(|n| block_on(s.define_series(gauge(n))).unwrap())
        .collect();
    let mut names: Vec<String> = CONSUMERS.iter().map(|n| n.to_string()).collect();
    let mut registered = 0;
    let mut cs: Vec<Option<Consumer>> = names.iter().map(|n| rec.take(&s, n)).collect();
    for step in history {
        match step {
            Step::Emit(i, v) => {
                let id = ids.get(*i).copied().unwrap_or(SeriesId(999));
                // A bad reading now and then, so that the live-state fact may lie before the recent window.
                let value = match *v % 9 {
                    8 => f64::NAN,
                    k => f64::from(k) * 15.0,
                };
                let r = block_on(s.emit(&[Reading::new(id, clock.wall(), Value::F64(value))]));
                match r {
                    Ok(a) => {
                        if let Some(Outcome::Recorded { position, .. }) = a.outcomes.first() {
                            rec.emitted.push(*position);
                        }
                        rec.out.push(format!(
                            "emit {:?}",
                            a.outcomes
                                .first()
                                .map(|o| matches!(o, Outcome::Recorded { .. }))
                        ));
                    }
                    Err(e) => rec.out.push(format!("emit {e:?}")),
                }
            }
            Step::Advance(secs) => clock.advance(Duration::from_secs(*secs)),
            Step::Tick => s.tick().unwrap(),
            Step::Route(c, k) => {
                if let Some(c) = &cs[*c] {
                    let r = block_on(c.route(funnel(k, &ids)));
                    rec.out.push(format!(
                        "route {:?}",
                        r.map_err(|e| format!("{e:?}").split('(').next().unwrap().to_string())
                    ));
                }
            }
            Step::Unroute(c, n) => {
                if let Some(c) = &cs[*c] {
                    let routes = c.routes().unwrap();
                    if let Some((id, _)) = routes.get(*n) {
                        block_on(c.unroute(*id)).unwrap();
                    }
                }
            }
            Step::Drain(c, n) => {
                let name = names[*c].as_str();
                if let Some(cons) = cs[*c].as_mut() {
                    let mut got = rec.held.remove(name).unwrap_or_default();
                    while let Some(d) = cons.try_next().unwrap() {
                        rec.push(name, &d);
                        got.push(d);
                    }
                    if let Some(d) = got.get(*n).or(got.last().filter(|_| *n >= 4)) {
                        block_on(cons.confirm(d.id)).unwrap();
                    }
                }
            }
            Step::Retake(c) => {
                cs[*c] = None;
                rec.held.remove(&names[*c]);
                cs[*c] = rec.take(&s, &names[*c]);
            }
            Step::Register(c, k) => {
                cs[*c] = None;
                rec.held.remove(&names[*c]);
                registered += 1;
                names[*c] = format!("r{registered}");
                let start = match k % 3 {
                    0 => Start::Now,
                    1 => Start::Time(kvik::Timestamp(
                        clock.wall().0 - i64::from(*k % 5) * 1_000_000_000,
                    )),
                    _ => Start::Position(SavedPoint {
                        position: rec
                            .emitted
                            .get(usize::from(*k) % (rec.emitted.len() + 1))
                            .copied()
                            .unwrap_or_default(),
                        stamp: ResetStamp::default(),
                    }),
                };
                rec.out.push(format!("register {} {}", names[*c], k % 3));
                cs[*c] = rec.take_at(&s, &names[*c], start);
            }
            Step::LoseCursor(_) | Step::Restart => {
                block_on(s.shutdown()).unwrap();
                cs.clear();
                drop(s);
                rec.held.clear();
                if let Step::LoseCursor(c) = step {
                    lose_cursor(&names[*c]);
                }
                s = reopen();
                cs = names.iter().map(|n| rec.take(&s, n)).collect();
            }
            Step::Define => {
                if ids.len() == 3 {
                    ids.push(block_on(s.define_series(gauge(NAMES[3]))).unwrap());
                }
            }
            Step::Alias => {
                let r = block_on(s.alias(ids[2], "site/x/power"));
                rec.out.push(format!("alias {}", r.is_ok()));
            }
            Step::Crash { .. } => {}
        }
    }
    // Everything still ready.
    for (i, c) in cs.iter_mut().enumerate() {
        if let Some(c) = c {
            while let Some(d) = c.try_next().unwrap() {
                rec.push(&names[i], &d);
                block_on(c.confirm(d.id)).unwrap();
            }
        }
    }
    drop(cs);
    SLOW.fetch_add(s.stats().delivery.slow_path, Ordering::Relaxed);
    block_on(s.shutdown()).unwrap();
    rec.out
}

/// The deliverer's reads of the log for facts outside its recent window, over the differential histories.
static SLOW: AtomicU64 = AtomicU64::new(0);

/// `NoCursor` resyncs handed out, over both backends' runs.
static NO_CURSOR: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn runner() -> TestRunner {
    // Each case opens durable stores; a quarter of the configured cases keeps the deep run in bounds.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    })
}

/// Timers run only when the store is called, on both backends, so that both see the same evaluations; condition
/// sequences start at 1 at every start on both (spec §4 Order).
/// Half the histories, chosen with each history, run with windows of a few dozen facts (Q-751).
#[test]
fn deliveries_follow_the_model_over_restarts() {
    let smalls = AtomicU64::new(0);
    runner()
        .run(
            &(steps(false), prop::bool::weighted(0.5)),
            |(history, small)| {
                let m_clock = ManualClock::new(start());
                let dir = format!("kvik-test/consumers-differential/{}", next_id());
                let mut env = Env::test(m_clock.clone());
                env.background_timers = false;
                let m_config = StoreConfig::new(&dir, "Europe/London")
                    .create_if_missing(true)
                    .backend(Backend::Model)
                    .env(env);
                let model = block_on(Store::open(m_config.clone())).unwrap();
                let m = run(
                    model,
                    &|| block_on(Store::open(m_config.clone())).unwrap(),
                    &|name| {
                        assert!(kvik::model_fault(
                            &m_config.dir,
                            kvik::ModelFault::LoseCursorFile(name.into())
                        ));
                    },
                    &m_clock,
                    &history,
                );
                let mut w = World::quiet(&format!("differential-{}", next_id()));
                if small {
                    w.window_memory = Some(8 << 10);
                    smalls.fetch_add(1, Ordering::Relaxed);
                }
                let lose = |name: &str| {
                    let path = w
                        .path_with(std::path::Path::new("cursors").join(format!("{name}.cur")))
                        .expect("a consumer is given its cursor file at the take");
                    w.fs.poke(&path, 0, &[0; 16]);
                };
                let d = run(w.open(), &|| w.open(), &lose, &w.clock, &history);
                prop_assert_eq!(m, d);
                Ok(())
            },
        )
        .unwrap();
    assert!(
        smalls.into_inner() > 0,
        "some history runs with small windows"
    );
    assert!(
        SLOW.load(Ordering::Relaxed) > 0,
        "some history reads facts outside the deliverer's recent window"
    );
    assert!(
        NO_CURSOR.load(Ordering::Relaxed) > 0,
        "some history loses the cursor file of a routed consumer"
    );
}

/// What the crash test knows of one consumer.
#[derive(Default)]
struct Seen {
    /// Facts handed out since the last start, by position.
    handed: Vec<Position>,
    /// The highest position a confirmation was attempted for (its cursor can be no higher).
    bound: Position,
    /// Condition states handed out, in order, by funnel.
    states: Vec<(FunnelId, ConditionState)>,
    /// How many of `states` were handed out before the last confirmation attempt: only those can be latched.
    latchable: usize,
}

impl Seen {
    fn hand(&mut self, d: &Delivery) {
        match &d.kind {
            DeliveryKind::Fact(f) => self.handed.push(f.position),
            DeliveryKind::Trigger(t) => self.states.push((t.funnel, t.state.clone())),
            DeliveryKind::Resync(r) => self.states.extend(
                r.funnels
                    .iter()
                    .filter_map(|f| f.condition.clone().map(|c| (f.funnel, c))),
            ),
            _ => {}
        }
    }

    fn confirming(&mut self, d: &Delivery) {
        self.bound = self.bound.max(d.position);
        self.latchable = self.states.len();
    }

    fn may_be_latched(&self, funnel: FunnelId, state: &ConditionState) -> bool {
        self.states[..self.latchable]
            .iter()
            .any(|(f, s)| *f == funnel && s == state)
    }
}

/// The crash rules, checked against what the test itself saw. After a crash, possibly in the middle of a write and
/// with part of the unsynced work surviving a power loss (torn cursor appends included), a consumer resumes at a
/// cursor no later than its last attempted confirmation; every fact handed to it above that cursor is replayed in
/// position order; reset points in the replay wait for their confirmation; the resume ends with a resync whose
/// `forget_above`, if any, is at or above the position durable before the crash (always present after a power loss)
/// and below its id; nothing follows before it is confirmed; and a condition it reports as no transition is in a
/// state handed to the consumer before a confirmation, so that a transition is never lost.
#[test]
fn deliveries_after_crashes_follow_the_rules() {
    runner()
        .run(&steps(true), |history| {
            let mut w = World::quiet(&format!("crashes-{}", next_id()));
            let mut s = Some(w.open());
            let ids: Vec<SeriesId> = NAMES[..3]
                .iter()
                .map(|n| block_on(s.as_ref().unwrap().define_series(gauge(n))).unwrap())
                .collect();
            let mut cs: Vec<Option<Consumer>> = CONSUMERS
                .iter()
                .map(|n| Some(block_on(s.as_ref().unwrap().consumer(n, Start::Now, Select::None)).unwrap()))
                .collect();
            let mut seen: Vec<Seen> = CONSUMERS
                .iter()
                .map(|n| Seen {
                    bound: s.as_ref().unwrap().consumer_stats(n).unwrap().cursor,
                    ..Seen::default()
                })
                .collect();
            for step in &history {
                let st = s.as_ref().unwrap();
                match step {
                    Step::Emit(i, v) => {
                        let _ = block_on(st.emit(&[Reading::new(
                            ids[*i % 3],
                            w.clock.wall(),
                            Value::F64(f64::from(*v)),
                        )]));
                    }
                    Step::Advance(secs) => w.clock.advance(Duration::from_secs(*secs)),
                    Step::Tick => st.tick().unwrap(),
                    Step::Route(c, k) => {
                        if let Some(c) = &cs[*c] {
                            let _ = block_on(c.route(funnel(k, &ids)));
                        }
                    }
                    Step::Unroute(c, n) => {
                        if let Some(c) = &cs[*c]
                            && let Some((id, _)) = c.routes().unwrap().get(*n)
                        {
                            block_on(c.unroute(*id)).unwrap();
                        }
                    }
                    Step::Drain(i, n) => {
                        if let Some(c) = cs[*i].as_mut() {
                            let got = ready(c);
                            got.iter().for_each(|d| seen[*i].hand(d));
                            if let Some(d) = got.get(*n).or(got.last()) {
                                seen[*i].confirming(d);
                                block_on(c.confirm(d.id)).unwrap();
                            }
                        }
                    }
                    Step::Crash { ops, power, seed } => {
                        let before = st.stats().log.durable;
                        // The process stops after `ops` more file operations, somewhere in an emit, the deliveries
                        // it causes and their confirmation (a cursor file append among them).
                        w.fs.crash_after(usize::from(*ops));
                        let _ = block_on(st.emit(&[Reading::new(ids[0], w.clock.wall(), Value::F64(60.0))]));
                        if let Some(c) = cs[0].as_mut() {
                            while let Ok(Some(d)) = c.try_next() {
                                seen[0].hand(&d);
                                seen[0].confirming(&d);
                                let _ = block_on(c.confirm(d.id));
                            }
                        }
                        let durable = st.stats().log.durable.max(before);
                        let store = s.take().unwrap();
                        w.stop(store, cs.drain(..).flatten().collect(), power.then_some(Some(*seed)));
                        let st = w.open();
                        for (i, name) in CONSUMERS.iter().enumerate() {
                            let mut c = block_on(st.consumer(name, Start::Now, Select::None)).unwrap();
                            let cursor = st.consumer_stats(name).unwrap().cursor;
                            let mut got = Vec::new();
                            loop {
                                let more = ready(&mut c);
                                let pause = more.last().and_then(resync).is_some_and(|r| r.cause == ResyncCause::ResetPoint);
                                got.extend(more);
                                if !pause {
                                    break;
                                }
                                // A reset point in the replay: nothing more until it is confirmed.
                                prop_assert!(c.try_next().unwrap().is_none());
                                block_on(c.confirm(got.last().unwrap().id)).unwrap();
                            }
                            let last = got.last().cloned();
                            let Some(r) = last.as_ref().and_then(resync).cloned() else {
                                prop_assert!(false, "a resume ends with a resync: {:?}", got);
                                unreachable!()
                            };
                            let last = last.unwrap();
                            if r.cause == ResyncCause::NoCursor {
                                // Its cursor file was lost: no latch to compare, every condition is a transition.
                                prop_assert!(r.funnels.iter().all(|f| f.condition.is_none() || f.transition));
                            } else {
                                prop_assert_eq!(r.cause, ResyncCause::Resume);
                                prop_assert!(
                                    cursor <= seen[i].bound,
                                    "cursor {:?} ahead of every confirmation {:?}",
                                    cursor,
                                    seen[i].bound
                                );
                                let replayed: Vec<Position> = got
                                    .iter()
                                    .filter_map(|d| match &d.kind {
                                        DeliveryKind::Fact(f) => Some((f.position, d.flags.replayed)),
                                        _ => None,
                                    })
                                    .map(|(p, r)| {
                                        assert!(r, "a replayed fact");
                                        p
                                    })
                                    .collect();
                                prop_assert!(replayed.windows(2).all(|w| w[0] < w[1]));
                                for p in seen[i].handed.iter().filter(|p| **p > cursor) {
                                    prop_assert!(replayed.contains(p), "fact at {:?} not replayed after {:?}", p, cursor);
                                }
                                for f in &r.funnels {
                                    if let Some(state @ (ConditionState::True | ConditionState::False)) = &f.condition
                                        && !f.transition
                                    {
                                        prop_assert!(
                                            seen[i].may_be_latched(f.funnel, state),
                                            "{:?} of funnel {:?} reported as latched but never handed out and confirmed",
                                            state,
                                            f.funnel
                                        );
                                    }
                                }
                            }
                            if let Some(x) = r.forget_above {
                                prop_assert!(x >= durable, "forget_above {:?} below the durable {:?}", x, durable);
                                prop_assert!(last.position > x);
                            } else {
                                prop_assert!(!*power, "a power loss ends the resume with forget_above");
                            }
                            prop_assert!(c.try_next().unwrap().is_none());
                            let mut next = Seen {
                                states: std::mem::take(&mut seen[i].states),
                                latchable: seen[i].latchable,
                                ..Seen::default()
                            };
                            got.iter().for_each(|d| next.hand(d));
                            next.confirming(&last);
                            block_on(c.confirm(last.id)).unwrap();
                            seen[i] = next;
                            cs.push(Some(c));
                        }
                        s = Some(st);
                    }
                    Step::Retake(_)
                    | Step::Register(..)
                    | Step::LoseCursor(_)
                    | Step::Restart
                    | Step::Define
                    | Step::Alias => {}
                }
            }
            drop(cs);
            if let Some(st) = s {
                block_on(st.shutdown()).unwrap();
            }
            Ok(())
        })
        .unwrap();
}
