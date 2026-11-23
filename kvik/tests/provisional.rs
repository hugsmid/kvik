//! The fast path (spec §4 Provisional streams, `api.md` Provisional streams, Q-1109): provisional delivery before the sync, the
//! watermark, withdrawal across failed syncs, crashes and power losses, isolation of a slow stream, and durable
//! consumers left as they were.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use common::world::{World, gauge, num};
use common::{block_on, start};
use kvik::env::{Env, MemFs, SystemClock};
use kvik::{
    Consumer, DeliveryKind, Funnel, LogConfig, Provisional, ProvisionalEvent, ProvisionalFact,
    ProvisionalOptions, Select, SeriesId, Start, Store, StoreConfig, Timestamp, View, Withdrawal,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};

const WAIT: Duration = Duration::from_secs(5);

/// Holds every log segment sync while closed.
#[derive(Default)]
struct Gate {
    closed: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn install(fs: &MemFs) -> Arc<Gate> {
        let g = Arc::new(Gate::default());
        let h = g.clone();
        fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
            if p.extension().is_some_and(|e| e == "open") {
                let s = h.closed.lock().unwrap();
                drop(h.cv.wait_while(s, |c| *c).unwrap());
            }
        })));
        g
    }
    fn set(&self, closed: bool) {
        *self.closed.lock().unwrap() = closed;
        self.cv.notify_all();
    }
}

fn define(s: &Store, name: &str) -> SeriesId {
    block_on(s.define_series(gauge(name))).unwrap()
}

/// Events until `stop` holds of one, or none comes within [`WAIT`].
fn until(
    p: &mut Provisional,
    mut stop: impl FnMut(&ProvisionalEvent) -> bool,
) -> Vec<ProvisionalEvent> {
    let mut out = Vec::new();
    while let Some(e) = p.next(WAIT) {
        let done = stop(&e);
        out.push(e);
        if done {
            return out;
        }
    }
    panic!("no event within {WAIT:?} after {out:?}");
}

/// What a stream said about each call.
#[derive(Debug, Default)]
struct Told {
    accepted: BTreeMap<u64, (bool, Vec<(u32, i64)>)>,
    through: u64,
    durable: BTreeSet<u64>,
    withdrawn: BTreeMap<u64, Withdrawal>,
    lagged: Vec<(u64, u64)>,
    closed: bool,
    full: BTreeMap<u64, Vec<ProvisionalFact>>,
    mark: Option<kvik::Position>,
}

impl Told {
    /// Takes one event, checking that no call is both durable and withdrawn, and nothing follows `Closed`.
    fn take(&mut self, e: &ProvisionalEvent) -> Result<(), String> {
        if self.closed {
            return Err(format!("{e:?} after Closed"));
        }
        match e {
            ProvisionalEvent::Accepted(a) => {
                let facts = a.facts().map(|f| (f.series.0, f.t.0)).collect();
                if self
                    .accepted
                    .insert(a.seq(), (a.durable(), facts))
                    .is_some()
                {
                    return Err(format!("call {} delivered twice", a.seq()));
                }
                self.full.insert(a.seq(), a.facts().cloned().collect());
                if a.durable() {
                    self.durable.insert(a.seq());
                }
            }
            ProvisionalEvent::Durable { through, mark } => {
                if *through <= self.through {
                    return Err(format!("watermark {through} after {}", self.through));
                }
                if self.mark.is_some_and(|m| *mark < m) {
                    return Err(format!("mark {mark:?} after {:?}", self.mark));
                }
                self.mark = Some(*mark);
                self.through = *through;
                for (&s, _) in self.accepted.range(..=through) {
                    if !self.withdrawn.contains_key(&s) {
                        self.durable.insert(s);
                    }
                }
            }
            ProvisionalEvent::Withdrawn { first, last, cause } => {
                for (&s, _) in self.accepted.range(first..=last) {
                    if self.durable.contains(&s) {
                        return Err(format!("call {s} withdrawn after it was durable"));
                    }
                    self.withdrawn.insert(s, *cause);
                }
            }
            ProvisionalEvent::Lagged { first, last, .. } => self.lagged.push((*first, *last)),
            ProvisionalEvent::Closed => self.closed = true,
            _ => return Err(format!("unexpected {e:?}")),
        }
        Ok(())
    }

    fn drain_to_close(&mut self, p: &mut Provisional) -> Result<(), String> {
        while !self.closed {
            let e = p.next(WAIT).ok_or("no Closed")?;
            self.take(&e)?;
        }
        Ok(())
    }

    fn unsettled(&self) -> Vec<u64> {
        self.accepted
            .keys()
            .filter(|s| !self.durable.contains(s) && !self.withdrawn.contains_key(s))
            .copied()
            .collect()
    }
}

fn all(s: &Store, ids: &[SeriesId]) -> Vec<kvik::Fact> {
    let r = s.reader();
    ids.iter()
        .flat_map(|&id| {
            r.facts(id, Timestamp(0)..Timestamp(i64::MAX), View::Current)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn stored(w: &World, ids: &[SeriesId]) -> BTreeSet<(u32, i64)> {
    stored_facts(w, ids).into_keys().collect()
}

fn stored_facts(w: &World, ids: &[SeriesId]) -> BTreeMap<(u32, i64), kvik::Fact> {
    let s = w.open();
    let facts = all(&s, ids)
        .into_iter()
        .map(|f| ((f.series.0, f.t.0), f))
        .collect();
    block_on(s.shutdown()).unwrap();
    facts
}

/// Whether a provisional fact is the fact recorded, but for its position.
fn same_fact(p: &ProvisionalFact, f: &kvik::Fact) -> bool {
    p.series == f.series
        && p.t == f.t
        && p.value == f.value
        && p.quality == f.quality
        && p.supplied_quality == f.supplied_quality
        && p.flags == f.flags
        && p.changed_at == f.changed_at
}

/// A queued call's result if it has one, without waiting.
fn now_or_never(p: &mut Pin<Box<kvik::Pending>>) -> Option<Result<kvik::Ack, kvik::EmitError>> {
    match p.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(r) => Some(r),
        Poll::Pending => None,
    }
}

fn consumer(s: &Store, ids: &[SeriesId]) -> Consumer {
    let c = block_on(s.consumer("automation", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(ids.to_vec())))).unwrap();
    c
}

#[test]
fn a_reading_reaches_the_stream_before_its_sync_and_the_watermark_follows_it_at_once() {
    let w = World::new("fast-before-sync");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    let gate = Gate::install(&w.fs);
    gate.set(true);
    let pending = s.try_emit(&[num(id, 1, 1.0)]).unwrap();
    let ev = until(&mut p, |e| matches!(e, ProvisionalEvent::Accepted(_)));
    let ProvisionalEvent::Accepted(a) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert!(!a.durable());
    assert_eq!(
        a.facts().map(|f| f.t.0).collect::<Vec<_>>(),
        [start().0 + 1_000_000_000]
    );
    assert!(
        p.next(Duration::from_millis(50)).is_none(),
        "no watermark before the sync"
    );
    gate.set(false);
    let synced = Instant::now();
    let ev = until(&mut p, |e| matches!(e, ProvisionalEvent::Durable { .. }));
    assert!(
        synced.elapsed() < WAIT / 2,
        "the call's settling woke the stream only at its timeout"
    );
    let ProvisionalEvent::Durable { through, mark } = ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(through, a.seq());
    let ack = block_on(pending).unwrap();
    assert!(ack.first().unwrap() <= mark);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_stream_that_reads_after_the_sync_gets_the_calls_flagged_durable_and_no_watermark() {
    let w = World::new("fast-late-reader");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    block_on(s.emit(&[num(id, 1, 1.0)])).unwrap();
    block_on(s.emit(&[num(id, 2, 2.0)])).unwrap();
    let mut told = Told::default();
    while told.accepted.len() < 2 {
        told.take(&p.next(WAIT).expect("accepted")).unwrap();
    }
    assert_eq!(
        told.accepted.values().map(|a| a.0).collect::<Vec<_>>(),
        [true, true]
    );
    let more = p.next(Duration::from_millis(50));
    assert!(more.is_none(), "{more:?}");
    block_on(s.shutdown()).unwrap();
    told.drain_to_close(&mut p).unwrap();
}

#[test]
fn a_store_with_no_stream_open_hands_nothing_to_one_opened_later() {
    let w = World::new("fast-opt-in");
    let s = w.open();
    let id = define(&s, "site/a/power");
    block_on(s.emit(&[num(id, 1, 1.0)])).unwrap();
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    assert!(p.next(Duration::from_millis(50)).is_none());
    block_on(s.emit(&[num(id, 2, 2.0)])).unwrap();
    let ev = until(&mut p, |e| matches!(e, ProvisionalEvent::Accepted(_)));
    let ProvisionalEvent::Accepted(a) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(a.seq(), 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_stream_selecting_series_gets_only_their_facts() {
    let w = World::new("fast-select");
    let s = w.open();
    let a = define(&s, "site/a/power");
    let b = define(&s, "site/b/power");
    let mut opts = ProvisionalOptions::default();
    opts.series = vec![b];
    let mut p = s.provisional(opts).unwrap();
    block_on(s.emit(&[num(a, 1, 1.0)])).unwrap();
    block_on(s.emit(&[num(a, 2, 1.0), num(b, 2, 2.0)])).unwrap();
    let ev = until(&mut p, |e| matches!(e, ProvisionalEvent::Accepted(_)));
    let ProvisionalEvent::Accepted(acc) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(acc.seq(), 2);
    assert_eq!(acc.facts().map(|f| f.series).collect::<Vec<_>>(), [b]);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_failed_sync_withdraws_every_call_it_held_with_the_writers_outcome() {
    let w = World::new("fast-sync-fails");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    let gate = Gate::install(&w.fs);
    gate.set(true);
    let calls: Vec<_> = (1..=3)
        .map(|i| s.try_emit(&[num(id, i, i as f64)]).unwrap())
        .collect();
    let mut told = Told::default();
    while told.accepted.len() < 3 {
        told.take(&p.next(WAIT).expect("accepted")).unwrap();
    }
    assert!(told.accepted.values().all(|a| !a.0));
    w.fs.fail_syncs(true);
    gate.set(false);
    while !told.unsettled().is_empty() {
        told.take(&p.next(WAIT).expect("withdrawn")).unwrap();
    }
    let results: Vec<_> = calls.into_iter().map(block_on).collect();
    for (seq, r) in (1..=3).zip(&results) {
        let want = match r {
            Err(kvik::EmitError::OutcomeUnknown(_)) => Withdrawal::OutcomeUnknown,
            Err(kvik::EmitError::ReadOnly(_) | kvik::EmitError::Closed) => Withdrawal::NotStored,
            other => panic!("call {seq}: {other:?}"),
        };
        assert_eq!(told.withdrawn.get(&seq), Some(&want), "call {seq}");
    }
    assert!(told.durable.is_empty());
    drop(s);
    told.drain_to_close(&mut p).unwrap();
}

#[test]
fn a_crash_withdraws_what_was_not_durable_and_the_rest_survives_the_restart() {
    let mut w = World::new("fast-crash");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    block_on(s.emit(&[num(id, 1, 1.0)])).unwrap();
    block_on(s.emit(&[num(id, 2, 2.0)])).unwrap();
    let gate = Gate::install(&w.fs);
    gate.set(true);
    let held: Vec<_> = (3..=4)
        .map(|i| s.try_emit(&[num(id, i, 0.0)]).unwrap())
        .collect();
    let mut told = Told::default();
    while told.accepted.len() < 4 {
        told.take(&p.next(WAIT).expect("accepted")).unwrap();
    }
    w.fs.crash_after(0);
    gate.set(false);
    drop(held);
    w.crash(s, Vec::new(), false);
    told.drain_to_close(&mut p).unwrap();
    assert_eq!(told.durable, BTreeSet::from([1, 2]));
    // Call 4 is withdrawn as not stored when the failed sync came before the writer took it.
    assert_eq!(told.withdrawn.keys().copied().collect::<Vec<_>>(), [3, 4]);
    assert_eq!(told.withdrawn[&3], Withdrawal::OutcomeUnknown);
    let t = |i: i64| start().0 + i * 1_000_000_000;
    let facts = stored(&w, &[id]);
    assert!(facts.contains(&(id.0, t(1))) && facts.contains(&(id.0, t(2))));
}

#[test]
fn a_stream_that_falls_behind_lags_and_resumes_while_the_writer_and_others_go_on() {
    let w = World::new("fast-isolation");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut slow_opts = ProvisionalOptions::default();
    slow_opts.max_behind = 4;
    let mut slow = s.provisional(slow_opts).unwrap();
    let mut fast = s.provisional(ProvisionalOptions::default()).unwrap();
    let mut seen = Told::default();
    for i in 1..=20 {
        block_on(s.emit(&[num(id, i, i as f64)])).unwrap();
        while let Some(e) = fast.try_next() {
            seen.take(&e).unwrap();
        }
    }
    while seen.accepted.len() < 20 {
        seen.take(&fast.next(WAIT).expect("fast stream")).unwrap();
    }
    let mut lagged = Told::default();
    for e in until(
        &mut slow,
        |e| matches!(e, ProvisionalEvent::Accepted(a) if a.seq() == 20),
    ) {
        lagged.take(&e).unwrap();
    }
    assert_eq!(lagged.lagged, [(1, 16)]);
    assert_eq!(
        lagged.accepted.keys().copied().collect::<Vec<_>>(),
        [17, 18, 19, 20]
    );
    block_on(s.shutdown()).unwrap();
}

fn slow_opts_with(max_behind: usize) -> ProvisionalOptions {
    let mut o = ProvisionalOptions::default();
    o.max_behind = max_behind;
    o
}

#[test]
fn lagged_names_a_position_below_every_fact_it_skipped() {
    let w = World::new("fast-lag-after");
    let s = w.open();
    let id = define(&s, "site/a/power");
    let mut slow = s.provisional(slow_opts_with(1)).unwrap();
    for i in 1..=3 {
        block_on(s.emit(&[num(id, i, i as f64)])).unwrap();
    }
    let ev = until(&mut slow, |e| matches!(e, ProvisionalEvent::Lagged { .. }));
    let ProvisionalEvent::Lagged { first, last, after } = ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!((first, last), (1, 2));
    assert!(all(&s, &[id]).iter().all(|f| f.position > after));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn max_behind_outside_its_bounds_is_refused() {
    let w = World::new("fast-bounds");
    let s = w.open();
    for m in [0, kvik::PROVISIONAL_MAX_BEHIND + 1] {
        assert!(matches!(
            s.provisional(slow_opts_with(m)),
            Err(kvik::QueryError::Invalid { .. })
        ));
    }
    block_on(s.shutdown()).unwrap();
}

/// Consumer deliveries over a fixed history, with or without a provisional stream reading alongside.
fn consumer_deliveries(name: &str, with_stream: bool) -> Vec<String> {
    let w = World::quiet(name);
    let s = w.open();
    let a = define(&s, "site/a/power");
    let b = define(&s, "site/b/power");
    let mut c = consumer(&s, &[a, b]);
    let mut p = with_stream.then(|| s.provisional(ProvisionalOptions::default()).unwrap());
    let mut out = Vec::new();
    for i in 1..=6 {
        block_on(s.emit(&[num(a, i, i as f64), num(b, i, -(i as f64))])).unwrap();
        if let Some(p) = &mut p {
            while p.try_next().is_some() {}
        }
        for d in common::world::drain(&mut c) {
            out.push(format!("{:?} {:?}", d.kind, d.flags));
        }
    }
    drop(c);
    block_on(s.shutdown()).unwrap();
    out
}

#[test]
fn a_provisional_stream_changes_nothing_a_durable_consumer_receives() {
    assert_eq!(
        consumer_deliveries("fast-consumer-with", true),
        consumer_deliveries("fast-consumer-without", false)
    );
}

#[test]
fn with_a_one_second_gathering_window_readings_arrive_within_milliseconds() {
    let mut env = Env::test(kvik::env::ManualClock::new(start()));
    env.clock = Arc::new(SystemClock::default());
    let fs = Arc::new(MemFs::new());
    env.fs = fs.clone();
    let mut log = LogConfig::default();
    log.segment_size = 16 << 20;
    log.min_sync_interval = Duration::from_secs(1);
    log.sync_interval_floor = Duration::from_secs(1);
    let config = StoreConfig::new("fast-window", "Etc/UTC")
        .create_if_missing(true)
        .env(env)
        .log(log);
    let s = block_on(Store::open(config)).unwrap();
    let id = define(&s, "site/a/power");
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    let syncs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = syncs.clone();
    fs.set_sync_hook(Some(Arc::new(move |path: &Path| {
        if path.extension().is_some_and(|e| e == "open") {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    })));
    let began = Instant::now();
    let now = || s.now().0;
    let mut late = Vec::new();
    let mut acks = Vec::new();
    // About 37 independent writes a second for 1.5 s, each its own call.
    for i in 0..56 {
        let sent = Instant::now();
        acks.push((
            sent,
            s.try_emit(&[kvik::Reading::new(
                id,
                Timestamp(now()),
                kvik::Value::F64(i as f64),
            )])
            .unwrap(),
        ));
        loop {
            match p.next(WAIT) {
                Some(ProvisionalEvent::Accepted(a)) => {
                    assert!(!a.durable() || i < 6, "call {i} already durable");
                    late.push(sent.elapsed());
                    break;
                }
                Some(ProvisionalEvent::Durable { .. }) => {}
                other => panic!("call {i}: {other:?}"),
            }
        }
        while p.try_next().is_some() {}
        std::thread::sleep(Duration::from_millis(27).saturating_sub(sent.elapsed()));
    }
    let worst = late.iter().max().copied().unwrap();
    assert!(
        worst < Duration::from_millis(50),
        "slowest provisional delivery {worst:?}"
    );
    let waited: Vec<Duration> = acks
        .into_iter()
        .map(|(sent, a)| {
            block_on(a).unwrap();
            sent.elapsed()
        })
        .collect();
    assert!(
        waited
            .iter()
            .filter(|d| **d > Duration::from_millis(200))
            .count()
            > waited.len() / 2,
        "acknowledgements should wait for the gathering window"
    );
    // Spec §3 Group commit: at most the window's length over the interval, plus four, batch syncs.
    let most = began.elapsed().as_secs() as usize + 1 + 4;
    let n = syncs.load(std::sync::atomic::Ordering::SeqCst);
    assert!(n <= most, "{n} log syncs for 56 calls, at most {most}");
    block_on(s.shutdown()).unwrap();
}

#[derive(Clone, Debug)]
enum Op {
    Emit { series: usize, n: usize, wait: bool },
    Hold(bool),
    Read,
}

#[derive(Clone, Copy, Debug)]
enum End {
    Shutdown,
    Crash,
    PowerLoss,
    PartialPowerLoss(u64),
    SyncFails,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..2usize, 1..3usize, any::<bool>()).prop_map(|(series, n, wait)| Op::Emit { series, n, wait }),
        1 => any::<bool>().prop_map(Op::Hold),
        2 => Just(Op::Read),
    ]
}

fn arb_end() -> impl Strategy<Value = End> {
    prop_oneof![
        Just(End::Shutdown),
        Just(End::Crash),
        Just(End::PowerLoss),
        any::<u64>().prop_map(End::PartialPowerLoss),
        Just(End::SyncFails),
    ]
}

fn history(ops: &[Op], end: End) -> Result<(), TestCaseError> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut w = World::new(&format!("fast-history-{n}"));
    let s = w.open();
    let ids = [define(&s, "site/a/power"), define(&s, "site/b/power")];
    let mut c = consumer(&s, &ids);
    let mut p = s.provisional(ProvisionalOptions::default()).unwrap();
    let gate = Gate::install(&w.fs);
    let mut told = Told::default();
    let mut confirmed = BTreeSet::new();
    let mut t = 0;
    // Each call's first reading time, and its result or the call still pending.
    let mut calls: Vec<(i64, Call)> = Vec::new();
    for op in ops {
        match op {
            Op::Emit { series, n, wait } => {
                let rs: Vec<_> = (0..*n)
                    .map(|_| {
                        t += 1;
                        num(ids[*series], t, t as f64)
                    })
                    .collect();
                let first = rs[0].t.0;
                match s.try_emit(&rs) {
                    Ok(pending) if *wait && !*gate.closed.lock().unwrap() => {
                        calls.push((first, Call::Done(block_on(pending))));
                    }
                    Ok(pending) => calls.push((first, Call::Held(Box::pin(pending)))),
                    Err(e) => return Err(TestCaseError::fail(format!("emit: {e:?}"))),
                }
            }
            Op::Hold(on) => gate.set(*on),
            Op::Read => {
                while let Some(e) = p.try_next() {
                    told.take(&e).map_err(TestCaseError::fail)?;
                }
                for d in common::world::drain(&mut c) {
                    if let DeliveryKind::Fact(f) = &d.kind {
                        confirmed.insert((f.series.0, f.t.0));
                    }
                }
            }
        }
    }
    let results;
    match end {
        End::Shutdown => {
            gate.set(false);
            results = resolve(calls);
            drop(c);
            block_on(s.shutdown()).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        }
        End::SyncFails => {
            w.fs.fail_syncs(true);
            let last = num(ids[0], t + 1, 0.0);
            if let Ok(p) = s.try_emit(std::slice::from_ref(&last)) {
                calls.push((last.t.0, Call::Held(Box::pin(p))));
            }
            gate.set(false);
            results = resolve(calls);
            while !told.unsettled().is_empty() || told.accepted.is_empty() {
                match p.next(WAIT) {
                    Some(e) => told.take(&e).map_err(TestCaseError::fail)?,
                    None => break,
                }
            }
            w.fs.fail_syncs(false);
            w.crash(s, vec![c], false);
        }
        End::Crash | End::PowerLoss | End::PartialPowerLoss(_) => {
            w.fs.crash_after(0);
            gate.set(false);
            results = resolve(calls);
            let power = match end {
                End::Crash => None,
                End::PowerLoss => Some(None),
                End::PartialPowerLoss(seed) => Some(Some(seed)),
                _ => unreachable!(),
            };
            w.stop(s, vec![c], power);
        }
    }
    told.drain_to_close(&mut p).map_err(TestCaseError::fail)?;
    prop_assert!(
        told.unsettled().is_empty(),
        "silently gone: {:?}",
        told.unsettled()
    );
    for (seq, fs) in &told.full {
        let first = fs.first().map(|f| f.t.0).unwrap_or_default();
        let result = results.get(&first);
        if let Some(cause) = told.withdrawn.get(seq) {
            let given = match result {
                Some(Err(kvik::EmitError::OutcomeUnknown(_))) => Withdrawal::OutcomeUnknown,
                Some(Err(kvik::EmitError::ReadOnly(_) | kvik::EmitError::Closed)) => {
                    Withdrawal::NotStored
                }
                other => {
                    return Err(TestCaseError::fail(format!(
                        "call {seq} withdrawn {cause:?}, its writer was given {other:?}"
                    )));
                }
            };
            prop_assert_eq!(*cause, given, "call {}", seq);
        }
        if told.durable.contains(seq) {
            prop_assert!(
                matches!(result, Some(Ok(_))),
                "call {} said durable, its writer was given {:?}",
                seq,
                result
            );
        }
    }
    let facts = stored_facts(&w, &ids);
    for (seq, fs) in &told.full {
        if told.durable.contains(seq) {
            for f in fs {
                let recorded = facts.get(&(f.series.0, f.t.0));
                prop_assert!(
                    recorded.is_some_and(|r| same_fact(f, r)),
                    "call {} said durable as {:?}, recorded {:?} after {:?}",
                    seq,
                    f,
                    recorded,
                    end
                );
            }
        }
    }
    for f in &confirmed {
        prop_assert!(
            facts.contains_key(f),
            "a consumer confirmed {f:?}, gone after {end:?}"
        );
    }
    Ok(())
}

/// Each call's result by its first reading time, waiting up to [`WAIT`] for those still pending; dropping a pending
/// call lets the store's writer go once the store is dropped.
fn resolve(calls: Vec<(i64, Call)>) -> BTreeMap<i64, Result<kvik::Ack, kvik::EmitError>> {
    let until = Instant::now() + WAIT;
    calls
        .into_iter()
        .filter_map(|(first, call)| match call {
            Call::Done(r) => Some((first, r)),
            Call::Held(mut p) => loop {
                if let Some(r) = now_or_never(&mut p) {
                    break Some((first, r));
                }
                if Instant::now() >= until {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(1));
            },
        })
        .collect()
}

enum Call {
    Done(Result<kvik::Ack, kvik::EmitError>),
    Held(Pin<Box<kvik::Pending>>),
}

#[test]
fn every_provisional_reading_ends_durable_or_withdrawn_across_crashes_and_failed_syncs() {
    let cases = (ProptestConfig::default().cases / 8).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &(prop::collection::vec(arb_op(), 1..16), arb_end()),
            |(ops, end)| history(&ops, end),
        )
        .unwrap();
}
