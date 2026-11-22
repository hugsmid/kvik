//! The stream against the reference model (`kvik_model::provisional`) over scripted outcomes, durable positions,
//! failures and stops; the ring's bound and a slow stream's lag.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use kvik_model::provisional as km;
use kvik_model::types::{Position as ModelPos, SeriesId as ModelSeries};
use proptest::prelude::*;
use proptest::strategy::ValueTree;

use super::*;
use crate::provisional::PROVISIONAL_MAX_BEHIND;
use crate::time::Timestamp;
use crate::types::{FactFlags, Quality, SeriesId, Value};

#[derive(Debug, Default)]
struct Scripted {
    ended: bool,
    stopped: bool,
    mark: u64,
}

#[derive(Clone, Debug, Default)]
struct Script(Arc<Mutex<Scripted>>);

impl Signals for Script {
    fn ended(&self) -> bool {
        lock(&self.0).ended
    }
    fn stopped(&self) -> bool {
        lock(&self.0).stopped
    }
    fn durable(&self) -> LogPos {
        at(lock(&self.0).mark)
    }
}

fn at(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

fn fact(series: u32, t: i64) -> ProvisionalFact {
    ProvisionalFact {
        series: SeriesId(series),
        t: Timestamp(t),
        value: Value::F64(0.0),
        quality: Quality::Good,
        supplied_quality: Quality::Good,
        flags: FactFlags::default(),
        changed_at: None,
    }
}

fn accept(fast: &Fast, facts: &[(u32, i64)], floor: u64) -> Arc<Entry> {
    fast.push(
        facts.iter().map(|&(s, t)| fact(s, t)).collect(),
        at(floor),
        Outcome::Set(Mutex::new(None)),
    )
}

fn set(e: &Entry, r: Result<LogAck, CommitError>) {
    let Outcome::Set(s) = &e.outcome else {
        unreachable!()
    };
    *lock(s) = Some(r);
}

fn behind(max_behind: usize) -> ProvisionalOptions {
    ProvisionalOptions {
        max_behind,
        ..ProvisionalOptions::default()
    }
}

fn drain(s: &mut Stream) -> Vec<ProvisionalEvent> {
    std::iter::from_fn(|| s.next(Duration::ZERO)).collect()
}

/// An event as both sides can state it.
#[derive(Clone, Debug, PartialEq)]
enum Said {
    Accepted(u64, bool, Vec<(u32, i64)>),
    Durable(u64, u64),
    Withdrawn(u64, u64, &'static str),
    Lagged(u64, u64),
    Closed,
}

impl Said {
    fn kind(&self) -> &'static str {
        match self {
            Said::Accepted(_, true, _) => "accepted durable",
            Said::Accepted(_, false, _) => "accepted provisional",
            Said::Durable(..) => "durable",
            Said::Withdrawn(_, _, c) => c,
            Said::Lagged(..) => "lagged",
            Said::Closed => "closed",
        }
    }
}

fn said(e: &ProvisionalEvent) -> Said {
    match e {
        ProvisionalEvent::Accepted(a) => Said::Accepted(
            a.seq(),
            a.durable(),
            a.facts().map(|f| (f.series.0, f.t.0)).collect(),
        ),
        ProvisionalEvent::Durable { through, mark } => Said::Durable(*through, mark.sequence),
        ProvisionalEvent::Withdrawn { first, last, cause } => Said::Withdrawn(
            *first,
            *last,
            match cause {
                Withdrawal::OutcomeUnknown => "withdrawn unknown",
                Withdrawal::NotStored => "withdrawn not stored",
            },
        ),
        ProvisionalEvent::Lagged { first, last, .. } => Said::Lagged(*first, *last),
        ProvisionalEvent::Closed => Said::Closed,
    }
}

fn model_said(e: &km::Event<(u32, i64)>) -> Said {
    match e {
        km::Event::Accepted {
            seq,
            durable,
            facts,
        } => Said::Accepted(*seq, *durable, facts.clone()),
        km::Event::Durable { through, mark } => Said::Durable(*through, mark.sequence),
        km::Event::Withdrawn { first, last, cause } => Said::Withdrawn(
            *first,
            *last,
            match cause {
                km::Withdrawal::OutcomeUnknown => "withdrawn unknown",
                km::Withdrawal::NotStored => "withdrawn not stored",
            },
        ),
        km::Event::Closed => Said::Closed,
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// A call with one fact per listed series.
    Accept(Vec<u32>),
    /// The `n`th call without an outcome is acknowledged with `len` positions.
    Ack(usize, u64),
    /// The `n`th call without an outcome fails, with an unknown outcome when set.
    Fail(usize, bool),
    /// The durable position reaches the `n`th acknowledgement given.
    Sync(usize),
    /// The writer fails: the durable position moves no more.
    End,
    /// The store is shut down and its writer has stopped.
    Close,
    /// A stream opens, selecting series 1 only when set.
    Open(bool),
    /// Stream `n` is read until nothing is due.
    Poll(usize),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => prop::collection::vec(0..3u32, 1..4).prop_map(Op::Accept),
        4 => (0..4usize, 1..4u64).prop_map(|(n, l)| Op::Ack(n, l)),
        1 => (0..4usize, any::<bool>()).prop_map(|(n, u)| Op::Fail(n, u)),
        3 => (0..6usize).prop_map(Op::Sync),
        1 => Just(Op::End),
        1 => Just(Op::Close),
        2 => any::<bool>().prop_map(Op::Open),
        6 => (0..3usize).prop_map(Op::Poll),
    ]
}

struct Pair {
    fast: Arc<Fast>,
    script: Script,
    model: km::Provisional<(u32, i64)>,
    entries: Vec<Arc<Entry>>,
    open: Vec<usize>,
    acked: Vec<u64>,
    next_pos: u64,
    streams: Vec<(Stream, u64)>,
    t: i64,
    kinds: BTreeSet<&'static str>,
}

impl Pair {
    fn new() -> Self {
        Pair {
            fast: Arc::new(Fast::default()),
            script: Script::default(),
            model: km::Provisional::default(),
            entries: Vec::new(),
            open: Vec::new(),
            acked: Vec::new(),
            next_pos: 0,
            streams: Vec::new(),
            t: 0,
            kinds: BTreeSet::new(),
        }
    }

    fn ended(&self) -> bool {
        lock(&self.script.0).ended
    }

    fn signal(&mut self) {
        let s = lock(&self.script.0);
        let (mark, ended, closed) = (s.mark, s.ended, s.stopped);
        drop(s);
        self.model.signal(ModelPos::new(0, mark), ended, closed);
    }

    fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::Accept(series) => {
                if self.ended() {
                    return Ok(());
                }
                let facts: Vec<(u32, i64)> = series
                    .iter()
                    .map(|&s| {
                        self.t += 1;
                        (s, self.t)
                    })
                    .collect();
                let e = accept(&self.fast, &facts, lock(&self.script.0).mark);
                let seq = self
                    .model
                    .accept(facts.iter().map(|&f| (ModelSeries(f.0), f)).collect());
                prop_assert_eq!(seq, e.seq);
                self.open.push(self.entries.len());
                self.entries.push(e);
            }
            Op::Ack(n, len) => {
                if self.open.is_empty() || self.ended() {
                    return Ok(());
                }
                let i = self.open.remove(n % self.open.len());
                let first = self.next_pos + 1;
                self.next_pos += len;
                set(
                    &self.entries[i],
                    Ok(LogAck {
                        first: at(first),
                        last: at(self.next_pos),
                    }),
                );
                self.model.settle(
                    self.entries[i].seq,
                    km::Outcome::Acknowledged(ModelPos::new(0, self.next_pos)),
                );
                self.acked.push(self.next_pos);
            }
            Op::Fail(n, unknown) => {
                if self.open.is_empty() {
                    return Ok(());
                }
                let i = self.open.remove(n % self.open.len());
                let (e, m) = if *unknown {
                    (CommitError::OutcomeUnknown, km::Withdrawal::OutcomeUnknown)
                } else {
                    (CommitError::ReadOnly, km::Withdrawal::NotStored)
                };
                set(&self.entries[i], Err(e));
                self.model
                    .settle(self.entries[i].seq, km::Outcome::Failed(m));
            }
            Op::Sync(n) => {
                if self.acked.is_empty() || self.ended() {
                    return Ok(());
                }
                let p = self.acked[n % self.acked.len()];
                let mut s = lock(&self.script.0);
                s.mark = s.mark.max(p);
                drop(s);
                self.signal();
            }
            Op::End => {
                lock(&self.script.0).ended = true;
                self.signal();
            }
            Op::Close => {
                let mut s = lock(&self.script.0);
                s.ended = true;
                s.stopped = true;
                drop(s);
                self.fast.close();
                self.signal();
            }
            Op::Open(select) => {
                let mut o = behind(PROVISIONAL_MAX_BEHIND);
                if *select {
                    o.series = vec![SeriesId(1)];
                }
                let stream = self.fast.open(&o, Box::new(self.script.clone()));
                let id = self
                    .model
                    .open(select.then(|| BTreeSet::from([ModelSeries(1)])));
                self.streams.push((stream, id));
            }
            Op::Poll(n) => {
                if self.streams.is_empty() {
                    return Ok(());
                }
                let k = n % self.streams.len();
                let (stream, id) = &mut self.streams[k];
                let engine: Vec<Said> = drain(stream).iter().map(said).collect();
                let mut model = Vec::new();
                loop {
                    let ev = self.model.poll(*id);
                    if ev.is_empty() {
                        break;
                    }
                    model.extend(ev.iter().map(model_said));
                }
                self.kinds.extend(engine.iter().map(Said::kind));
                prop_assert_eq!(engine, model);
            }
        }
        Ok(())
    }
}

fn arb_script() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), 1..80)
}

fn play(ops: &[Op]) -> Result<Pair, TestCaseError> {
    let mut pair = Pair::new();
    for op in ops {
        pair.apply(op)?;
    }
    for n in 0..pair.streams.len() {
        pair.apply(&Op::Poll(n))?;
    }
    Ok(pair)
}

proptest! {
    #[test]
    fn the_stream_says_what_the_reference_model_says(ops in arb_script()) {
        play(&ops)?;
    }
}

#[test]
fn the_reference_model_comparison_reaches_every_event_but_lagged() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut kinds = BTreeSet::new();
    for _ in 0..300 {
        let ops = arb_script().new_tree(&mut runner).unwrap().current();
        kinds.extend(play(&ops).unwrap().kinds);
    }
    assert_eq!(
        kinds,
        BTreeSet::from([
            "accepted durable",
            "accepted provisional",
            "closed",
            "durable",
            "withdrawn not stored",
            "withdrawn unknown",
        ])
    );
}

#[test]
fn a_ring_with_no_stream_keeps_nothing_and_hands_out_no_facts() {
    let fast = Arc::new(Fast::default());
    let items = vec![Item::Fact(kvik_log::facts::FactRow {
        series_id: 1,
        t: 1,
        quality: kvik_log::facts::Quality::Good,
        flags: Default::default(),
        value: kvik_log::facts::Value::F64(0),
        changed_at: None,
    })];
    assert!(facts_of(&fast, &items, 0).is_none());
    let s = fast.open(&ProvisionalOptions::default(), Box::new(Script::default()));
    assert_eq!(facts_of(&fast, &items, 0).map(|f| f.len()), Some(1));
    drop(s);
    accept(&fast, &[(1, 1)], 0);
    assert!(lock(&fast.ring).entries.is_empty());
    assert!(facts_of(&fast, &items, 0).is_none());
}

#[test]
fn the_ring_keeps_the_largest_max_behind_of_its_streams() {
    let fast = Arc::new(Fast::default());
    let mut o = behind(3);
    let small = fast.open(&o, Box::new(Script::default()));
    o = behind(5);
    let large = fast.open(&o, Box::new(Script::default()));
    for t in 0..10 {
        accept(&fast, &[(1, t)], 0);
    }
    assert_eq!(lock(&fast.ring).entries.len(), 5);
    drop(large);
    accept(&fast, &[(1, 10)], 0);
    assert_eq!(lock(&fast.ring).entries.len(), 3);
    drop(small);
}

#[test]
fn a_slow_stream_lags_to_within_max_behind_and_others_miss_nothing() {
    let fast = Arc::new(Fast::default());
    let o = behind(2);
    let mut slow = fast.open(&o, Box::new(Script::default()));
    let mut quick = fast.open(&ProvisionalOptions::default(), Box::new(Script::default()));
    let mut got = 0;
    for t in 0..8 {
        accept(&fast, &[(1, t)], t as u64);
        got += drain(&mut quick)
            .iter()
            .filter(|e| matches!(e, ProvisionalEvent::Accepted(_)))
            .count();
    }
    assert_eq!(got, 8);
    let ev: Vec<Said> = drain(&mut slow).iter().map(said).collect();
    assert_eq!(
        ev,
        [
            Said::Lagged(1, 6),
            Said::Accepted(7, false, vec![(1, 6)]),
            Said::Accepted(8, false, vec![(1, 7)]),
            Said::Durable(6, 0),
        ]
    );
}

#[test]
fn lagged_names_the_floor_of_the_last_call_taken() {
    let fast = Arc::new(Fast::default());
    let o = behind(1);
    let mut s = fast.open(&o, Box::new(Script::default()));
    accept(&fast, &[(1, 1)], 7);
    drain(&mut s);
    accept(&fast, &[(1, 2)], 9);
    accept(&fast, &[(1, 3)], 11);
    let ev = drain(&mut s);
    let ProvisionalEvent::Lagged { first, last, after } = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!((*first, *last, after.sequence), (2, 2, 7));
}
