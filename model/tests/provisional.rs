//! The fast path (spec §4 Provisional streams, Q-1109): calls handed to provisional streams before they are durable, the
//! watermark, and withdrawal across crashes, power losses, remounts and failed syncs; consumers unaffected.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::T0;
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, Fault, MarkSync, Model, Selection, WriteOutcome,
};
use kvik_model::provisional::{Event, Withdrawal};
use kvik_model::registry::{Durability, SeriesOptions};
use kvik_model::types::{Fact, Kind, Position, Reading, SECOND, SeriesId, ValueType};
use proptest::prelude::*;
use proptest::strategy::ValueTree;

const C: ConsumerId = ConsumerId(1);

fn model() -> (Model, [SeriesId; 2]) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 1_000,
    };
    let cfg = Config {
        mark_sync: MarkSync::Timed,
        seal_after_batches: 64,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![consumer], T0);
    let synced = m
        .define("site/a/power", Kind::Gauge, ValueType::Number, "W", SECOND)
        .unwrap();
    let written = m
        .define_with(
            "site/b/power",
            Kind::Gauge,
            ValueType::Number,
            "W",
            SECOND,
            SeriesOptions::durability(Durability::Written),
        )
        .unwrap();
    m.drain().unwrap();
    (m, [synced, written])
}

type Key = (SeriesId, i64);

fn key(f: &Fact) -> (Key, Position) {
    ((f.series, f.t), f.position)
}

/// What one stream was told about each call.
#[derive(Debug, Default)]
struct Told {
    accepted: BTreeMap<u64, Vec<(Key, Position)>>,
    through: u64,
    durable: BTreeSet<u64>,
    withdrawn: BTreeMap<u64, Withdrawal>,
    /// Calls delivered provisional and later covered by a watermark.
    promoted: usize,
    closed: bool,
}

impl Told {
    fn take(&mut self, e: Event<Fact>) -> Result<(), String> {
        if self.closed {
            return Err(format!("{e:?} after Closed"));
        }
        match e {
            Event::Accepted {
                seq,
                durable,
                facts,
            } => {
                let keys = facts.iter().map(key).collect();
                if self.accepted.insert(seq, keys).is_some() {
                    return Err(format!("call {seq} delivered twice"));
                }
                if durable {
                    self.durable.insert(seq);
                }
            }
            Event::Durable { through, .. } => {
                if through <= self.through {
                    return Err(format!("watermark {through} after {}", self.through));
                }
                self.through = through;
                for &s in self.accepted.range(..=through).map(|(s, _)| s) {
                    if !self.withdrawn.contains_key(&s) && self.durable.insert(s) {
                        self.promoted += 1;
                    }
                }
            }
            Event::Withdrawn { first, last, cause } => {
                for &s in self.accepted.range(first..=last).map(|(s, _)| s) {
                    if self.durable.contains(&s) {
                        return Err(format!("call {s} withdrawn after it was durable"));
                    }
                    self.withdrawn.insert(s, cause);
                }
            }
            Event::Closed => self.closed = true,
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

fn poll(m: &mut Model, s: (usize, u64), told: &mut Told) -> Result<(), String> {
    for e in m.provisional_poll(s) {
        told.take(e)?;
    }
    Ok(())
}

#[test]
fn s4_a_call_is_provisional_until_the_watermark_covers_it() {
    let (mut m, [synced, _]) = model();
    let s = m.provisional_open(None).unwrap();
    let mut told = Told::default();
    let _ = m.emit(Reading::num(synced, T0 + SECOND, 1.0));
    m.commit(None).unwrap();
    poll(&mut m, s, &mut told).unwrap();
    assert_eq!(
        told.durable,
        BTreeSet::from([1]),
        "synced before it is accepted in the model"
    );
    assert!(told.withdrawn.is_empty());
}

#[test]
fn s4_a_flagged_call_stays_provisional_until_its_sync() {
    let (mut m, [_, written]) = model();
    let s = m.provisional_open(None).unwrap();
    let _ = m.emit(Reading::num(written, T0 + SECOND, 1.0));
    m.commit(None).unwrap();
    assert_eq!(
        m.provisional_poll(s),
        vec![Event::Accepted {
            seq: 1,
            durable: false,
            facts: m
                .facts()
                .unwrap()
                .into_iter()
                .filter(|f| f.series == written)
                .collect(),
        }]
    );
    assert!(m.provisional_poll(s).is_empty());
    m.sync_log();
    assert!(matches!(
        m.provisional_poll(s)[..],
        [Event::Durable { through: 1, .. }]
    ));
}

#[test]
fn s4_a_failed_sync_withdraws_its_call_and_every_flagged_call_not_yet_synced() {
    let (mut m, [synced, written]) = model();
    let s = m.provisional_open(None).unwrap();
    let mut told = Told::default();
    let _ = m.emit(Reading::num(written, T0 + SECOND, 1.0));
    m.commit(None).unwrap();
    poll(&mut m, s, &mut told).unwrap();
    let _ = m.emit(Reading::num(synced, T0 + 2 * SECOND, 1.0));
    assert!(
        m.commit(Some(Fault::SyncError(WriteOutcome::Survived)))
            .is_err()
    );
    poll(&mut m, s, &mut told).unwrap();
    // Call 2 failed before the stream looked at it, so it was never delivered.
    assert_eq!(told.accepted.keys().copied().collect::<Vec<_>>(), [1]);
    assert_eq!(
        told.withdrawn,
        BTreeMap::from([(1, Withdrawal::OutcomeUnknown)])
    );
}

#[test]
fn s4_a_crash_closes_the_stream_once_every_call_is_settled() {
    let (mut m, [_, written]) = model();
    let s = m.provisional_open(None).unwrap();
    let mut told = Told::default();
    let _ = m.emit(Reading::num(written, T0 + SECOND, 1.0));
    m.commit(None).unwrap();
    poll(&mut m, s, &mut told).unwrap();
    m.crash();
    poll(&mut m, s, &mut told).unwrap();
    assert!(!told.closed, "Closed comes alone, after the withdrawal");
    poll(&mut m, s, &mut told).unwrap();
    assert!(told.closed);
    assert_eq!(
        told.withdrawn,
        BTreeMap::from([(1, Withdrawal::OutcomeUnknown)])
    );
    m.restart();
    assert!(
        m.facts().unwrap().iter().any(|f| f.series == written),
        "a same-boot start keeps the flagged batch withdrawn from the stream (§9 row 7a's kin)"
    );
}

#[test]
fn s4_a_stream_selecting_a_series_gets_only_its_facts() {
    let (mut m, [synced, written]) = model();
    let s = m.provisional_open(Some(BTreeSet::from([written]))).unwrap();
    let _ = m.emit(Reading::num(synced, T0 + SECOND, 1.0));
    m.commit(None).unwrap();
    let _ = m.emit(Reading::num(synced, T0 + 2 * SECOND, 1.0));
    let _ = m.emit(Reading::num(written, T0 + 2 * SECOND, 1.0));
    m.commit(None).unwrap();
    let ev = m.provisional_poll(s);
    let [Event::Accepted { seq: 2, facts, .. }] = &ev[..] else {
        panic!("{ev:?}")
    };
    assert!(facts.iter().all(|f| f.series == written) && facts.len() == 1);
}

#[derive(Clone, Debug)]
enum Op {
    Emit(usize, f64),
    Commit(Option<Fault>),
    SyncLog,
    Consume,
    Poll,
    Crash,
    PowerLoss(usize),
    Remount(usize),
    Restart,
}

fn arb_outcome() -> impl Strategy<Value = WriteOutcome> {
    prop_oneof![
        Just(WriteOutcome::Lost),
        Just(WriteOutcome::Torn),
        Just(WriteOutcome::Survived)
    ]
}

fn arb_fault() -> impl Strategy<Value = Option<Fault>> {
    prop_oneof![
        8 => Just(None),
        1 => Just(Some(Fault::Crash(CrashPoint::BeforeWrite))),
        1 => arb_outcome().prop_map(|o| Some(Fault::Crash(CrashPoint::AfterWrite(o)))),
        1 => Just(Some(Fault::Crash(CrashPoint::AfterSyncBeforeMark))),
        1 => Just(Some(Fault::Crash(CrashPoint::AfterSync))),
        1 => arb_outcome().prop_map(|o| Some(Fault::PowerLoss(o))),
        1 => arb_outcome().prop_map(|o| Some(Fault::SyncError(o))),
    ]
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => (-5.0..5.0f64).prop_map(|v| Op::Emit(0, v)),
        4 => (-5.0..5.0f64).prop_map(|v| Op::Emit(1, v)),
        4 => arb_fault().prop_map(Op::Commit),
        2 => Just(Op::SyncLog),
        2 => Just(Op::Consume),
        5 => Just(Op::Poll),
        1 => Just(Op::Crash),
        1 => (0..3usize).prop_map(Op::PowerLoss),
        1 => (0..3usize).prop_map(Op::Remount),
        2 => Just(Op::Restart),
    ]
}

/// Runs `ops`; with `streams`, a provisional stream is open on every start and polled at each `Poll`. Returns
/// each stream's account, the store after a final crash and restart, and everything the consumer received.
fn run(ops: &[Op], streams: bool) -> Result<(Vec<Told>, Model, String), String> {
    let (mut m, ids) = model();
    let mut open: Vec<((usize, u64), Told)> = Vec::new();
    let mut t = 0;
    for op in ops {
        if streams
            && m.is_up()
            && open
                .last()
                .is_none_or(|((run, _), _)| *run + 1 != m.provisional.len())
        {
            open.push((
                m.provisional_open(None).map_err(|e| e.to_string())?,
                Told::default(),
            ));
        }
        match op {
            Op::Emit(s, v) => {
                t += 1;
                let _ = m.emit(Reading::num(ids[*s], T0 + t * SECOND, *v));
            }
            Op::Commit(f) => {
                let _ = m.commit(*f);
            }
            Op::SyncLog => m.sync_log(),
            Op::Consume => {
                if let Ok(ds) = m.poll(C, 10)
                    && let Some(d) = ds.last()
                {
                    let _ = m.confirm(C, d.position);
                }
            }
            Op::Poll => {
                for (s, told) in &mut open {
                    poll(&mut m, *s, told)?;
                }
            }
            Op::Crash => m.crash(),
            Op::PowerLoss(k) => m.power_loss_keeping(*k),
            Op::Remount(k) => m.remount_keeping(*k),
            Op::Restart => m.restart(),
        }
    }
    m.crash();
    for (s, told) in &mut open {
        while !told.closed {
            let ev = m.provisional_poll(*s);
            if ev.is_empty() {
                break;
            }
            for e in ev {
                told.take(e)?;
            }
        }
    }
    m.restart();
    let received = format!("{:?}", m.received);
    Ok((open.into_iter().map(|(_, t)| t).collect(), m, received))
}

proptest! {
    #[test]
    fn s4_every_provisional_call_ends_durable_or_withdrawn_and_durable_ones_survive(
        ops in prop::collection::vec(arb_op(), 1..60)
    ) {
        let (streams, m, _) = run(&ops, true).map_err(TestCaseError::fail)?;
        let kept: BTreeSet<Key> = m.facts().unwrap_or_default().iter().map(|f| key(f).0).collect();
        let resets = m.reset_points();
        for told in &streams {
            prop_assert!(told.closed, "every start's stream ends with Closed");
            prop_assert!(told.unsettled().is_empty(), "silently gone: {:?}", told.unsettled());
            for (seq, keys) in &told.accepted {
                if told.durable.contains(seq) {
                    // As for an acknowledged reading: kept, or lost within §3's limits (a remount under the timed
                    // mark sync) and then reported by a reset point.
                    for (k, p) in keys {
                        prop_assert!(
                            kept.contains(k) || resets.iter().any(|r| r.forget_above < *p),
                            "call {seq} said durable, {k:?} at {p:?} gone unreported"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn s4_provisional_streams_change_nothing_a_consumer_receives(
        ops in prop::collection::vec(arb_op(), 1..60)
    ) {
        let with = run(&ops, true).map_err(TestCaseError::fail)?;
        let without = run(&ops, false).map_err(TestCaseError::fail)?;
        prop_assert_eq!(with.2, without.2);
        prop_assert_eq!(with.1.facts().ok(), without.1.facts().ok());
    }

}

#[test]
fn s4_provisional_generator_coverage() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let (mut withdrawn, mut promoted, mut durable_at_once) = (0, 0, 0);
    for _ in 0..1_000 {
        let ops = prop::collection::vec(arb_op(), 1..60)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        let (streams, _, _) = run(&ops, true).unwrap();
        for told in streams {
            withdrawn += told.withdrawn.len();
            promoted += told.promoted;
            durable_at_once += told.durable.len() - told.promoted;
        }
    }
    assert!(
        withdrawn > 25 && promoted > 25 && durable_at_once > 25,
        "withdrawn {withdrawn}, promoted {promoted}, durable at once {durable_at_once}"
    );
}
