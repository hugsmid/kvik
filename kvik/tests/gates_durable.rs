//! Durable gates across stops and crashes (spec §6 Gates, Resume; cond2 design §4.4, §6): replay from the cursor
//! file's checkpoint, the reseed where replay is not possible, the start floor, and the failure table's rows.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::block_on;
use common::world::{World, gauge, ready, resync};
use kvik::{
    Consumer, DeliveryKind, Gate, Input, Level, Reading, ResyncCause, Select, SeriesId, Start,
    Store, TransitionId, Value,
};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

fn humidity(series: SeriesId) -> Gate {
    Gate::levels(
        Input::value(series),
        [Level::above(70.0)
            .clear(65.0)
            .hold_up(Duration::from_secs(120))],
    )
}

fn take(s: &Store) -> Consumer {
    block_on(s.consumer("c", Start::Now, Select::None)).unwrap()
}

fn emit(w: &World, s: &Store, id: SeriesId, v: f64) {
    block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))])).unwrap();
}

/// Transitions as `(gate state after, replayed)`, and whether a resync came, in order.
fn seen(ds: &[kvik::Delivery]) -> Vec<String> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Transition(t) => Some(format!("{:?} r={}", t.to, d.flags.replayed)),
            DeliveryKind::Resync(r) => Some(format!(
                "resync {:?} {:?}",
                r.cause,
                r.gates
                    .iter()
                    .map(|g| (&g.state, g.set_by.is_some()))
                    .collect::<Vec<_>>()
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn an_unconfirmed_start_transition_is_replayed_after_a_clean_stop() {
    let w = World::quiet("gates-durable-unconfirmed");
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    drop(c);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    let mut c = take(&s);
    let got = ready(&mut c);
    assert_eq!(
        seen(&got),
        ["Some(Unknown) r=true", "resync Resume [(Unknown, true)]"],
    );
    assert_eq!(s.stats().delivery.gates_reseeded, 0);
}

#[test]
fn a_confirmed_transition_is_not_delivered_again_after_a_crash() {
    let mut w = World::quiet("gates-durable-confirmed");
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    emit(&w, &s, id, 50.0);
    let got = ready(&mut c);
    assert_eq!(
        seen(&got),
        ["Some(Unknown) r=false", "Some(Level(0)) r=false"]
    );
    block_on(c.confirm(got[1].id)).unwrap();
    emit(&w, &s, id, 80.0);
    w.clock.advance(Duration::from_secs(121));
    s.tick().unwrap();
    let after = ready(&mut c);
    assert_eq!(seen(&after), ["Some(Level(1)) r=false"]);
    // The process stops without confirming the hold's transition: it comes again, the confirmed ones do not.
    w.crash(s, vec![c], false);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(
        seen(&ready(&mut c)),
        ["Some(Level(1)) r=true", "resync Resume [(Level(1), true)]"]
    );
    assert_eq!(s.stats().delivery.gates_reseeded, 0);
}

#[derive(Clone, Debug)]
enum Op {
    Emit(u8),
    Advance(u64),
    Watch(u8),
    Drain(usize),
    Restart,
    /// The process stops after this many more file operations, in an emit and a drain; with `power`, the power goes
    /// too (a partial loss chosen by `seed`).
    Crash {
        ops: u8,
        power: bool,
        seed: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => any::<u8>().prop_map(Op::Emit),
        3 => (0u64..200).prop_map(Op::Advance),
        2 => (0u8..3).prop_map(Op::Watch),
        5 => (0usize..6).prop_map(Op::Drain),
        1 => Just(Op::Restart),
        2 => (0u8..24, any::<bool>(), any::<u64>()).prop_map(|(ops, power, seed)| Op::Crash { ops, power, seed }),
    ]
}

fn gate_of(k: u8, id: SeriesId) -> (String, Gate) {
    match k {
        0 => ("levels".into(), humidity(id)),
        1 => (
            "set".into(),
            Gate::set(Input::value(id), [Value::F64(0.0), Value::F64(15.0)])
                .hold_on(Duration::from_secs(1))
                .hold_off(Duration::from_secs(2)),
        ),
        _ => (
            "change".into(),
            Gate::change(Input::value(id)).deadband(20.0),
        ),
    }
}

/// What the test knows of the consumer's transitions across runs.
#[derive(Default)]
struct Track {
    /// Every transition id delivered and not forgotten.
    delivered: BTreeSet<TransitionId>,
    /// Ids a confirmation that returned covers: never delivered again (spec §6 Gates, Durability of a confirmation).
    confirmed: BTreeSet<TransitionId>,
    /// Ids handed out in this run, in order, with whether a confirmation was attempted through them.
    run: Vec<(TransitionId, bool)>,
}

impl Track {
    /// Checks and records what a drain gave (G6): an id seen before comes `replayed`; none a returned confirmation
    /// covers comes again. A resync's `forget_above` makes the consumer forget ids above it (Q-1140).
    fn take(&mut self, got: &[kvik::Delivery]) -> Result<(), TestCaseError> {
        for d in got {
            match &d.kind {
                DeliveryKind::Transition(t) => {
                    prop_assert!(
                        !self.confirmed.contains(&t.id),
                        "{:?} came again after its confirmation returned",
                        t.id
                    );
                    prop_assert!(
                        d.flags.replayed || !self.delivered.contains(&t.id),
                        "{:?} came twice without `replayed`",
                        t.id
                    );
                    self.delivered.insert(t.id);
                    self.run.push((t.id, false));
                }
                DeliveryKind::Resync(r) => {
                    if let Some(x) = r.forget_above {
                        let keep = |id: &TransitionId| id.at <= x;
                        self.delivered.retain(keep);
                        self.confirmed.retain(keep);
                        self.run.retain(|(id, _)| keep(id));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A confirmation through the delivery that was handed out after the transitions at `upto` in this run.
    fn confirming(&mut self, upto: usize, returned: bool) {
        for (id, attempted) in &mut self.run[..upto] {
            *attempted = true;
            if returned {
                self.confirmed.insert(*id);
            }
        }
    }

    /// The ids handed out in this run that no confirmation attempt covers.
    fn unattempted(&self) -> Vec<TransitionId> {
        self.run
            .iter()
            .filter(|(_, a)| !a)
            .map(|(id, _)| *id)
            .collect()
    }
}

/// Drains and confirms through the `n`th delivery (or the last), recording the confirmation's outcome.
fn drain(c: &mut Consumer, n: usize, track: &mut Track) -> Result<(), TestCaseError> {
    let got = ready(c);
    track.take(&got)?;
    if let Some(i) = (n < got.len()).then_some(n).or(got.len().checked_sub(1)) {
        let upto = track.run.len()
            - got[i + 1..]
                .iter()
                .filter(|d| matches!(d.kind, DeliveryKind::Transition(_)))
                .count();
        let returned = block_on(c.confirm(got[i].id)).is_ok();
        track.confirming(upto, returned);
    }
    Ok(())
}

/// After a start: takes the consumer and drains through the resync that ends its resume, confirming reset points on
/// the way and the resync last. With no `forget_above` and a resume, every transition handed out before the stop and
/// covered by no confirmation attempt comes again, `replayed` (G6; within `conditions.replay_max`).
fn resume(s: &Store, track: &mut Track) -> Result<Consumer, TestCaseError> {
    let before = track.unattempted();
    track.run.clear();
    let mut c = take(s);
    let mut got = Vec::new();
    let r = loop {
        let more = ready(&mut c);
        track.take(&more)?;
        got.extend(more);
        let Some(r) = got.last().and_then(resync).cloned() else {
            prop_assert!(false, "a resume ends with a resync: {:?}", got);
            unreachable!()
        };
        if r.cause != ResyncCause::ResetPoint {
            break r;
        }
        block_on(c.confirm(got.last().unwrap().id)).unwrap();
    };
    if r.cause == ResyncCause::Resume && r.forget_above.is_none() {
        for id in before {
            prop_assert!(
                got.iter().any(
                    |d| matches!(&d.kind, DeliveryKind::Transition(t) if t.id == id)
                        && d.flags.replayed
                ),
                "{:?} handed out and never confirmed was not replayed: {:?}",
                id,
                got
            );
        }
    }
    let n = track.run.len();
    block_on(c.confirm(got.last().unwrap().id)).unwrap();
    track.confirming(n, true);
    Ok(c)
}

/// The crash rules for gates (G5, G6): after any sequence of clean stops, process crashes in the middle of an emit and
/// a drain, and partial power losses, no transition comes twice without `replayed`, none a returned confirmation
/// covers comes again, and after a crash without a reset point every transition handed out and never confirmed is
/// replayed.
#[test]
fn gate_deliveries_after_crashes_follow_the_rules() {
    let cases = (ProptestConfig::default().cases / 4).max(16);
    TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    })
    .run(&prop::collection::vec(op(), 1..30), |history| {
        let mut w = World::quiet(&format!("gates-crashes-{}", next_id()));
        let mut s = w.open();
        let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
        let mut c = take(&s);
        let mut track = Track::default();
        for op in &history {
            match op {
                Op::Emit(v) => {
                    let _ = block_on(s.emit(&[Reading::new(
                        id,
                        w.clock.wall(),
                        Value::F64(f64::from(v % 9) * 15.0),
                    )]));
                }
                Op::Advance(secs) => {
                    w.clock.advance(Duration::from_secs(*secs));
                    let _ = s.tick();
                }
                Op::Watch(k) => {
                    let (name, g) = gate_of(*k, id);
                    let _ = block_on(c.watch(&name, g));
                }
                Op::Drain(n) => drain(&mut c, *n, &mut track)?,
                Op::Restart => {
                    drop(c);
                    block_on(s.shutdown()).unwrap();
                    drop(s);
                    s = w.open();
                    c = resume(&s, &mut track)?;
                }
                Op::Crash { ops, power, seed } => {
                    w.fs.crash_after(usize::from(*ops));
                    let _ = block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(90.0))]));
                    let _ = drain(&mut c, usize::MAX, &mut track);
                    w.stop(s, vec![c], power.then_some(Some(*seed)));
                    s = w.open();
                    c = resume(&s, &mut track)?;
                }
            }
        }
        drop(c);
        block_on(s.shutdown()).unwrap();
        Ok(())
    })
    .unwrap();
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}
