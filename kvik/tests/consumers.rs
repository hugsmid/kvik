//! Consumers on the durable backend (roadmap 5.2): routing changes and cursor files survive restarts, a power loss
//! ends in a reset point carrying `forget_above`, `emits` and routing cycles, and two property tests: deliveries match
//! the model backend's over clean restarts, and after crashes and power losses they follow the specification's
//! rules (at least once, never ahead of a confirmation, reset points).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{at, block_on, start};
use kvik::env::{Env, FixedBootId, ManualClock, MemFs};
use kvik::{
    Backend, Condition, ConditionState, Consumer, ConsumerError, DefineError, Delivery,
    DeliveryKind, Funnel, LogConfig, Outcome, Position, Reading, Refusal, ResyncCause, ResyncPart,
    Select, SeriesId, SeriesSpec, Start, Store, StoreConfig, TriggerCause, Value, ValueType,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

/// A durable store's world: its in-memory file system, boot identity and clock.
struct World {
    dir: String,
    fs: Arc<MemFs>,
    boot: Arc<FixedBootId>,
    clock: ManualClock,
    boot_count: u8,
}

impl World {
    fn new(name: &str) -> Self {
        World {
            dir: format!("consumers/{name}"),
            fs: Arc::new(MemFs::new()),
            boot: Arc::new(FixedBootId::new(Some([1; 16]))),
            clock: ManualClock::new(start()),
            boot_count: 1,
        }
    }

    fn config(&self) -> StoreConfig {
        let mut env = Env::test(self.clock.clone());
        env.fs = self.fs.clone();
        env.boot = self.boot.clone();
        let mut log = LogConfig::default();
        log.segment_size = 16 << 20;
        log.min_sync_interval = Duration::ZERO;
        StoreConfig::new(&self.dir, "Europe/London")
            .create_if_missing(true)
            .env(env)
            .log(log)
    }

    fn open(&self) -> Store {
        block_on(Store::open(self.config())).unwrap()
    }

    /// The process stops (every later file operation fails) and its handles are dropped; with `power`, the power
    /// goes too: every file returns to what was synced and the machine boots with a new boot identity.
    fn crash(&mut self, store: Store, consumers: Vec<Consumer>, power: bool) {
        self.fs.crash_after(0);
        drop(consumers);
        drop(store);
        if power {
            self.fs.power_loss();
            self.boot_count += 1;
            self.boot.set(Some([self.boot_count; 16]));
        } else {
            self.fs.restart();
        }
    }
}

fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        kvik::Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(1))
}

fn num(id: SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

/// Every ready delivery, each confirmed.
fn drain(c: &mut Consumer) -> Vec<Delivery> {
    let mut out = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        block_on(c.confirm(d.id)).unwrap();
        out.push(d);
    }
    out
}

fn resync(d: &Delivery) -> Option<&ResyncPart> {
    match &d.kind {
        DeliveryKind::Resync(r) => Some(r),
        _ => None,
    }
}

/// A consumer taken for the first time: its resync for a consumer with no cursor, confirmed.
fn new_consumer(s: &Store, name: &str, emits: Select) -> Consumer {
    let mut c = block_on(s.consumer(name, Start::Now, emits)).unwrap();
    let d = c.try_next().unwrap().unwrap();
    let r = resync(&d).unwrap();
    assert_eq!(r.cause, ResyncCause::NoCursor);
    assert_eq!(r.forget_above, Some(d.position));
    block_on(c.confirm(d.id)).unwrap();
    c
}

#[test]
fn routes_cursors_and_latches_survive_a_restart() {
    let w = World::new("restart");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarms", Select::None);
    let f1 = block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let cond = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }))
    .unwrap();
    block_on(c.unroute(f1)).unwrap();
    // Funnel ids continue above every id named, removed ones included.
    let f3 = block_on(c.route(Funnel::facts(Select::Names("plant/*/power".into())))).unwrap();
    assert_eq!((f1.0, cond.0, f3.0), (1, 2, 3));
    for (i, v) in [5.0, 20.0].into_iter().enumerate() {
        block_on(s.emit(&[num(g, i as i64, v)])).unwrap();
        w.clock.advance(Duration::from_secs(1));
    }
    let ds = drain(&mut c);
    let states: Vec<_> = ds
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Trigger(t) => Some((t.funnel, t.state.clone(), t.cause)),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![(cond, ConditionState::True, TriggerCause::Fact)]
    );
    let confirmed = s.consumer_stats("alarms").unwrap().cursor;
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    // The routing table is rebuilt from the log; the cursor and the latch come from the cursor file.
    let s = w.open();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("alarms").unwrap().cursor, confirmed);
    let routes: Vec<_> = c.routes().unwrap().into_iter().map(|(id, _)| id).collect();
    assert_eq!(routes, vec![cond, f3]);
    let ds = drain(&mut c);
    assert_eq!(ds.len(), 1, "nothing to replay: {ds:?}");
    let r = resync(&ds[0]).unwrap();
    assert_eq!((r.cause, r.forget_above), (ResyncCause::Resume, None));
    let fs = r.funnels.iter().find(|f| f.funnel == cond).unwrap();
    assert_eq!(fs.condition, Some(ConditionState::True));
    assert!(!fs.transition, "the latch holds the confirmed state");
    // The next funnel id stays above the highest one named.
    assert_eq!(block_on(c.route(Funnel::facts(Select::All))).unwrap().0, 4);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_power_loss_ends_the_resume_with_a_reset_point() {
    let mut w = World::new("power-loss");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let mut positions = Vec::new();
    for i in 0..4 {
        w.clock.advance(Duration::from_secs(1));
        let ack = block_on(s.emit(&[num(g, i, i as f64)])).unwrap();
        positions.push(ack.last().unwrap());
        // The first two are confirmed, a second apart, so the cursor file names the second.
        if i < 2 {
            drain(&mut c);
        }
    }
    let k = s.stats().log.durable;
    assert_eq!(s.consumer_stats("log").unwrap().cursor, positions[1]);
    w.crash(s, vec![c], true);

    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("log").unwrap().cursor, positions[1]);
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        got.push(d);
    }
    // The facts after the cursor are replayed, then the resync that ends the resume is also the reset point of
    // the recovery entry: `forget_above` is the last kept position and its id the entry's position.
    let replayed: Vec<Position> = got
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Fact(f) => {
                assert!(d.flags.replayed);
                Some(f.position)
            }
            _ => None,
        })
        .collect();
    assert_eq!(replayed, positions[2..].to_vec());
    let last = got.last().unwrap();
    let r = resync(last).unwrap();
    assert_eq!(r.cause, ResyncCause::Resume);
    assert_eq!(r.forget_above, Some(k));
    assert!(last.position > k);
    // Nothing further until it is confirmed; its confirmation writes the cursor file naming its id.
    block_on(s.emit(&[num(g, 9, 9.0)])).unwrap();
    assert!(c.try_next().unwrap().is_none());
    for d in &got {
        block_on(c.confirm(d.id)).unwrap();
    }
    assert_eq!(s.consumer_stats("log").unwrap().cursor, last.position);
    let next = c.try_next().unwrap().unwrap();
    assert!(matches!(&next.kind, DeliveryKind::Fact(f) if f.position > last.position));
    assert!(!next.flags.replayed);
    // A process crash on the same boot keeps the cursor written at the confirmation.
    let id = last.position;
    w.crash(s, vec![c], false);
    let s = w.open();
    let _c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("log").unwrap().cursor, id);
}

#[test]
fn a_reset_point_in_the_replay_waits_for_its_confirmation() {
    let mut w = World::new("reset-middle");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    // Facts, a power loss, more facts, another power loss; the consumer is not taken in between.
    let s = w.open();
    block_on(s.emit(&[num(g, 1, 1.0)])).unwrap();
    let k1 = s.stats().log.durable;
    w.crash(s, Vec::new(), true);
    let s = w.open();
    block_on(s.emit(&[num(g, 2, 2.0)])).unwrap();
    let k2 = s.stats().log.durable;
    w.crash(s, Vec::new(), true);

    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let mut first = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        first.push(d);
    }
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(matches!(&first[0].kind, DeliveryKind::Fact(f) if f.number() == Some(1.0)));
    let r = resync(&first[1]).unwrap();
    assert_eq!(
        (r.cause, r.forget_above),
        (ResyncCause::ResetPoint, Some(k1))
    );
    assert!(first[1].position > k1 && first[1].position < k2);
    block_on(c.confirm(first[1].id)).unwrap();
    let rest = drain(&mut c);
    assert!(matches!(&rest[0].kind, DeliveryKind::Fact(f) if f.number() == Some(2.0)));
    let r = resync(rest.last().unwrap()).unwrap();
    assert_eq!((r.cause, r.forget_above), (ResyncCause::Resume, Some(k2)));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_failed_cursor_sync_turns_the_store_read_only_and_is_never_retried() {
    use kvik_log::fs::FaultOp;
    let mut w = World::new("cursor-fault");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    w.crash(s, vec![c], true);
    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        got.push(d);
    }
    let last = got.last().unwrap();
    assert!(resync(last).unwrap().forget_above.is_some());
    // The sync of the cursor file that the reset point's confirmation writes fails.
    w.fs.fail_nth(FaultOp::Sync, "cursors", 0, std::io::ErrorKind::Other);
    let syncs = || {
        w.fs.ops()
            .iter()
            .filter(|o| o.op == FaultOp::Sync && o.path.to_string_lossy().contains("cursors"))
            .count()
    };
    let before = syncs();
    assert!(matches!(
        block_on(c.confirm(last.id)),
        Err(ConsumerError::Write(_))
    ));
    assert_eq!(syncs(), before + 1);
    assert!(matches!(s.health(), kvik::Health::ReadOnly { .. }));
    // Nothing further is delivered, and no cursor file is written again, by a call, the timer or the shutdown.
    block_on(s.emit(&[num(g, 1, 2.0)])).ok();
    assert!(c.try_next().is_err());
    w.clock.advance(Duration::from_secs(2));
    s.tick().unwrap();
    assert!(block_on(s.shutdown()).is_err());
    assert_eq!(syncs(), before + 1, "a failed sync is never retried");
}

#[test]
fn emits_are_declared_and_routing_cycles_are_refused() {
    let w = World::new("emits");
    let s = w.open();
    let x = block_on(s.define_series(gauge("site/a/in"))).unwrap();
    let y = block_on(s.define_series(gauge("site/a/out"))).unwrap();
    let v = block_on(s.define_series(gauge("other/b/value"))).unwrap();
    assert!(matches!(
        block_on(s.consumer("Bad Name", Start::Now, Select::None)),
        Err(ConsumerError::Route(_))
    ));
    let a = new_consumer(&s, "a", Select::Ids(vec![x]));
    let b = new_consumer(&s, "b", Select::Ids(vec![y]));
    block_on(b.route(Funnel::facts(Select::Ids(vec![x])))).unwrap();
    // a emits x, which b selects; b emits y: a funnel of a selecting y closes the cycle.
    assert!(matches!(
        block_on(a.route(Funnel::facts(Select::Names("site/*/out".into())))),
        Err(ConsumerError::RouteCycle(c)) if c.contains("a") && c.contains("b")
    ));
    // A consumer's own cycle, through two patterns that one name could match.
    let p = new_consumer(&s, "p", Select::Names("loop/*/a".into()));
    assert!(matches!(
        block_on(p.route(Funnel::facts(Select::Names("loop/x*/*".into())))),
        Err(ConsumerError::RouteCycle(_))
    ));
    // An alias that would make an id selected by p match what p emits is refused.
    block_on(p.route(Funnel::facts(Select::Ids(vec![v])))).unwrap();
    assert!(matches!(
        block_on(s.alias(v, "loop/x/a")),
        Err(DefineError::RouteCycle(_))
    ));
    block_on(s.alias(v, "other/b/renamed")).unwrap();
    // A consumer emits through its own handle, only on series it declared.
    let ack = block_on(a.try_emit(&[num(x, 0, 1.0), num(y, 0, 2.0)]).unwrap()).unwrap();
    assert!(matches!(ack.outcomes[0], Outcome::Recorded { .. }));
    assert_eq!(ack.outcomes[1], Outcome::Refused(Refusal::NotDeclared));
    assert_eq!(s.series_stats(y).unwrap().refused[&Refusal::NotDeclared], 1);
    // `emits` is recorded with the routing changes: after a restart the cycle is still found.
    block_on(s.shutdown()).unwrap();
    drop((a, b, p, s));
    let s = w.open();
    let a = block_on(s.consumer("a", Start::Now, Select::Ids(vec![x]))).unwrap();
    assert!(matches!(
        block_on(a.route(Funnel::facts(Select::Ids(vec![y])))),
        Err(ConsumerError::RouteCycle(_))
    ));
    // A change of `emits` that would close it is refused too.
    drop(a);
    assert!(matches!(
        block_on(s.consumer("b", Start::Now, Select::All)),
        Err(ConsumerError::RouteCycle(_))
    ));
    block_on(s.shutdown()).unwrap();
}

// ----- differential test against the model backend, and crash rules -----

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
    Restart,
    Define,
    Alias,
    /// Crash steps, for the durable-only test: a process crash, or a power loss.
    Crash(bool),
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
    // A clean restart in the differential test; a crash or a power loss in the test of the crash rules.
    let stop = any::<bool>().prop_map(move |power| {
        if crashes {
            Step::Crash(power)
        } else {
            Step::Restart
        }
    });
    let step = prop_oneof![
        10 => (0usize..4, any::<u8>()).prop_map(|(i, v)| Step::Emit(i, v)),
        4 => (0u64..3).prop_map(Step::Advance),
        1 => Just(Step::Tick),
        4 => (0usize..2, funnel_kind()).prop_map(|(c, k)| Step::Route(c, k)),
        1 => (0usize..2, 0usize..3).prop_map(|(c, n)| Step::Unroute(c, n)),
        5 => (0usize..2, 0usize..8).prop_map(|(c, n)| Step::Drain(c, n)),
        1 => (0usize..2).prop_map(Step::Retake),
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

/// A delivery in a form both backends give alike: positions differ between them (the model backend writes routing
/// entries when a pattern is evaluated again, the durable backend tzdata and recovery entries), and condition
/// sequences start again at every start of the durable engine only.
fn normal(d: &Delivery) -> String {
    match &d.kind {
        DeliveryKind::Fact(f) => format!(
            "fact {} {} {:?} r={} o={} f={}",
            f.series.0, f.t.0, f.value, d.flags.replayed, d.flags.outranked, d.flags.future
        ),
        DeliveryKind::Trigger(t) => format!("trigger {} {:?} {:?}", t.funnel.0, t.state, t.cause),
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
                "resync {:?} {} {}/{} [{}]",
                r.cause,
                r.forget_above.is_some(),
                r.part,
                r.last,
                fs.join("; ")
            )
        }
        other => format!("{other:?}"),
    }
}

/// Takes a consumer; the durable backend's resync for a consumer with no cursor is checked, confirmed and left out
/// of the record, the model backend sends none.
fn take(s: &Store, name: &str, out: &mut Vec<String>) -> Option<Consumer> {
    match block_on(s.consumer(name, Start::Now, Select::None)) {
        Ok(mut c) => {
            // The first delivery is taken at once on both backends.
            if let Some(d) = c.try_next().unwrap() {
                match resync(&d) {
                    Some(r) if r.cause == ResyncCause::NoCursor => {
                        block_on(c.confirm(d.id)).unwrap();
                    }
                    _ => {
                        // Not a new consumer: the first delivery is part of what is compared.
                        out.push(format!("{name}: {}", normal(&d)));
                        pending(&mut c, d);
                    }
                }
            }
            Some(c)
        }
        Err(e) => {
            out.push(format!("{name}: take {e:?}"));
            None
        }
    }
}

thread_local! {
    static HELD: std::cell::RefCell<BTreeMap<String, Vec<Delivery>>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// Keeps a delivery taken outside a drain, to be confirmed by the next drain.
fn pending(c: &mut Consumer, d: Delivery) {
    HELD.with(|h| {
        h.borrow_mut()
            .entry(c.name().to_string())
            .or_default()
            .push(d)
    });
}

fn held(name: &str) -> Vec<Delivery> {
    HELD.with(|h| h.borrow_mut().remove(name).unwrap_or_default())
}

/// What a history gave: per step, what each consumer received, in a comparable form.
fn run(
    store: Store,
    reopen: &dyn Fn() -> Store,
    clock: &ManualClock,
    history: &[Step],
) -> Vec<String> {
    HELD.with(|h| h.borrow_mut().clear());
    let mut s = store;
    let mut out = Vec::new();
    let mut ids: Vec<SeriesId> = NAMES[..3]
        .iter()
        .map(|n| block_on(s.define_series(gauge(n))).unwrap())
        .collect();
    let mut cs: Vec<Option<Consumer>> = CONSUMERS.iter().map(|n| take(&s, n, &mut out)).collect();
    let mut t = 0i64;
    for step in history {
        match step {
            Step::Emit(i, v) => {
                let id = ids.get(*i).copied().unwrap_or(SeriesId(999));
                t += 1;
                let value = f64::from(*v % 8) * 15.0;
                let r = block_on(s.emit(&[Reading::new(id, clock.wall(), Value::F64(value))]));
                match r {
                    Ok(a) => out.push(format!(
                        "emit {:?}",
                        a.outcomes
                            .first()
                            .map(|o| matches!(o, Outcome::Recorded { .. }))
                    )),
                    Err(e) => out.push(format!("emit {e:?}")),
                }
            }
            Step::Advance(secs) => clock.advance(Duration::from_secs(*secs)),
            Step::Tick => s.tick().unwrap(),
            Step::Route(c, k) => {
                if let Some(c) = &cs[*c] {
                    let r = block_on(c.route(funnel(k, &ids)));
                    out.push(format!(
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
                let name = CONSUMERS[*c];
                if let Some(cons) = cs[*c].as_mut() {
                    let mut got = held(name);
                    while let Some(d) = cons.try_next().unwrap() {
                        out.push(format!("{name}: {}", normal(&d)));
                        got.push(d);
                    }
                    if let Some(d) = got.get(*n).or(got.last().filter(|_| *n >= 4)) {
                        block_on(cons.confirm(d.id)).unwrap();
                    }
                }
            }
            Step::Retake(c) => {
                cs[*c] = None;
                held(CONSUMERS[*c]);
                cs[*c] = take(&s, CONSUMERS[*c], &mut out);
            }
            Step::Restart => {
                block_on(s.shutdown()).unwrap();
                cs.clear();
                drop(s);
                for n in CONSUMERS {
                    held(n);
                }
                s = reopen();
                cs = CONSUMERS.iter().map(|n| take(&s, n, &mut out)).collect();
            }
            Step::Define => {
                if ids.len() == 3 {
                    ids.push(block_on(s.define_series(gauge(NAMES[3]))).unwrap());
                }
            }
            Step::Alias => {
                let r = block_on(s.alias(ids[2], "site/x/power"));
                out.push(format!("alias {}", r.is_ok()));
            }
            Step::Crash(_) => {}
        }
    }
    let _ = t;
    // Everything still ready.
    for (i, c) in cs.iter_mut().enumerate() {
        if let Some(c) = c {
            while let Some(d) = c.try_next().unwrap() {
                out.push(format!("{}: {}", CONSUMERS[i], normal(&d)));
                block_on(c.confirm(d.id)).unwrap();
            }
        }
    }
    drop(cs);
    block_on(s.shutdown()).unwrap();
    out
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

#[test]
fn deliveries_follow_the_model_over_restarts() {
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&steps(false), |history| {
            let m_clock = ManualClock::new(start());
            let dir = format!("kvik-test/consumers-differential/{}", next_id());
            let m_config = StoreConfig::new(&dir, "Europe/London")
                .create_if_missing(true)
                .backend(Backend::Model)
                .env(Env::test(m_clock.clone()));
            let model = block_on(Store::open(m_config.clone())).unwrap();
            let m = run(
                model,
                &|| block_on(Store::open(m_config.clone())).unwrap(),
                &m_clock,
                &history,
            );
            let w = World::new(&format!("differential-{}", next_id()));
            let d = run(w.open(), &|| w.open(), &w.clock, &history);
            prop_assert_eq!(m, d);
            Ok(())
        })
        .unwrap();
}

/// The crash rules, checked against what the test itself saw: after a crash or a power loss a consumer resumes at a
/// cursor no later than its last confirmation, every fact handed to it above that cursor before the stop is
/// replayed in position order, a power loss ends the resume with a resync whose `forget_above` is the last
/// position acknowledged before it and whose id is above it, and nothing follows before that resync is confirmed.
#[test]
fn deliveries_after_crashes_follow_the_rules() {
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&steps(true), |history| {
            let mut w = World::new(&format!("crashes-{}", next_id()));
            let s = w.open();
            let mut s = Some(s);
            let ids: Vec<SeriesId> = NAMES[..3]
                .iter()
                .map(|n| block_on(s.as_ref().unwrap().define_series(gauge(n))).unwrap())
                .collect();
            let mut ignore = Vec::new();
            let mut cs: Vec<Option<Consumer>> = CONSUMERS
                .iter()
                .map(|n| take(s.as_ref().unwrap(), n, &mut ignore))
                .collect();
            // Per consumer: facts handed out in this run (position), and the last confirmed position.
            let mut handed: Vec<Vec<Position>> = vec![Vec::new(), Vec::new()];
            let mut confirmed: Vec<Position> = CONSUMERS
                .iter()
                .map(|n| s.as_ref().unwrap().consumer_stats(n).unwrap().cursor)
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
                            let mut got = Vec::new();
                            while let Some(d) = c.try_next().unwrap() {
                                if let DeliveryKind::Fact(f) = &d.kind {
                                    handed[*i].push(f.position);
                                }
                                got.push(d);
                            }
                            if let Some(d) = got.get(*n).or(got.last()) {
                                block_on(c.confirm(d.id)).unwrap();
                                confirmed[*i] = st.consumer_stats(CONSUMERS[*i]).unwrap().cursor;
                            }
                        }
                    }
                    Step::Crash(power) => {
                        let durable = st.stats().log.durable;
                        let store = s.take().unwrap();
                        w.crash(store, cs.drain(..).flatten().collect(), *power);
                        let st = w.open();
                        for (i, name) in CONSUMERS.iter().enumerate() {
                            let mut c =
                                block_on(st.consumer(name, Start::Now, Select::None)).unwrap();
                            let cursor = st.consumer_stats(name).unwrap().cursor;
                            prop_assert!(
                                cursor <= confirmed[i],
                                "cursor {:?} ahead of the confirmation {:?}",
                                cursor,
                                confirmed[i]
                            );
                            let mut got = Vec::new();
                            while let Some(d) = c.try_next().unwrap() {
                                got.push(d);
                            }
                            // Every fact handed out above the cursor comes again, replayed, in position order.
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
                            for p in handed[i].iter().filter(|p| **p > cursor) {
                                prop_assert!(
                                    replayed.contains(p),
                                    "fact at {p:?} not replayed after {cursor:?}"
                                );
                            }
                            let last = got.last().map(resync);
                            if let Some(Some(r)) = last {
                                prop_assert_eq!(r.cause, ResyncCause::Resume);
                                if *power {
                                    prop_assert_eq!(r.forget_above, Some(durable));
                                    prop_assert!(got.last().unwrap().position > durable);
                                }
                                if !*power {
                                    prop_assert_eq!(r.forget_above, None);
                                }
                                // Nothing further before its confirmation.
                                prop_assert!(c.try_next().unwrap().is_none());
                                let id = got.last().unwrap().id;
                                block_on(c.confirm(id)).unwrap();
                                confirmed[i] = st.consumer_stats(name).unwrap().cursor;
                            } else {
                                prop_assert!(false, "a resume ends with a resync: {:?}", got);
                            }
                            handed[i].clear();
                            cs.push(Some(c));
                        }
                        s = Some(st);
                    }
                    Step::Retake(_) | Step::Restart | Step::Define | Step::Alias => {}
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
