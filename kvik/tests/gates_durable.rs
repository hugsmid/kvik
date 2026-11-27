//! Durable gates across stops and crashes (spec §6 Gates, Resume; cond2 design §4.4, §6): replay from the cursor
//! file's checkpoint, the reseed where replay is not possible, the start floor, and the failure table's rows.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::block_on;
use common::world::{World, gauge, ready, resync};
use kvik::{
    ConditionsConfig, Consumer, ConsumerError, DeliveryKind, Gate, Health, Input, Level, Reading,
    ReplayMax, ResyncCause, Select, SeriesId, Span, Start, Store, TransitionId, Value,
};
use kvik_log::fs::FaultOp;
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

/// The cursor file's blocks as `(type, offset)`, in order, up to the first that is not whole.
fn blocks(bytes: &[u8]) -> Vec<(u8, usize)> {
    use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
    let mut at = kvik_log::header::HEADER_LEN;
    let mut out = Vec::new();
    while let Some(h) = bytes
        .get(at..at + BLOCK_HEADER_LEN)
        .and_then(|b| <&[u8; BLOCK_HEADER_LEN]>::try_from(b).ok())
        .and_then(BlockHeader::decode)
    {
        out.push((h.block_type, at));
        at += usize::try_from(h.block_len()).unwrap();
    }
    out
}

fn cursor_path(w: &World) -> PathBuf {
    w.path_with(Path::new("cursors").join("c.cur")).unwrap()
}

/// A store whose consumer has confirmed the start transitions and then the hold's `Level(1)`, crashed (without a
/// power loss) after that confirmation returned; the cursor file ends with the pair that confirmation wrote.
fn confirmed_twice(name: &str) -> (World, Vec<u8>) {
    let mut w = World::quiet(name);
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    emit(&w, &s, id, 50.0);
    let got = ready(&mut c);
    block_on(c.confirm(got[1].id)).unwrap();
    emit(&w, &s, id, 80.0);
    w.clock.advance(Duration::from_secs(121));
    s.tick().unwrap();
    let after = ready(&mut c);
    assert_eq!(seen(&after), ["Some(Level(1)) r=false"]);
    block_on(c.confirm(after[0].id)).unwrap();
    w.crash(s, vec![c], false);
    let bytes = w.fs.read(&cursor_path(&w)).unwrap();
    (w, bytes)
}

/// Flips a byte inside the body of the last block of type `t`, as a write torn there leaves it.
fn tear_last(w: &World, bytes: &[u8], t: u8) {
    let bs = blocks(bytes);
    let n = bs.len();
    assert!(
        n >= 2
            && bs[n - 2].0 == kvik_log::cursor::CHECKPOINT_BLOCK
            && bs[n - 1].0 == kvik_log::cursor::CURSOR_BLOCK,
        "a confirmation appends [0x31][0x30]: {bs:?}"
    );
    let at = bs.iter().rev().find(|b| b.0 == t).unwrap().1 + kvik_log::block::BLOCK_HEADER_LEN;
    w.fs.poke(&cursor_path(w), at, &[bytes[at] ^ 0xFF]);
}

/// Spec §6 Gates failure table, "torn inside the 0x31 block" (G8): the previous checkpoint and cursor block hold, so
/// the transition whose confirmation wrote the torn pair comes again, `replayed`, from an exact replay.
#[test]
fn a_cursor_write_torn_in_its_checkpoint_replays_from_the_previous_pair() {
    let (w, bytes) = confirmed_twice("gates-durable-torn-31");
    tear_last(&w, &bytes, kvik_log::cursor::CHECKPOINT_BLOCK);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(
        seen(&ready(&mut c)),
        ["Some(Level(1)) r=true", "resync Resume [(Level(1), true)]"]
    );
    let d = s.stats().delivery;
    assert_eq!((d.gates_reseeded, d.checkpoint_mismatch), (0, 0));
    block_on(s.shutdown()).unwrap();
}

/// Failure table, "torn inside the 0x30 block, 0x31 valid": the new checkpoint stands under the older cursor block;
/// replay starts at it and delivers only ids after it, so the confirmed `Level(1)` does not come again.
#[test]
fn a_cursor_write_torn_in_its_cursor_block_replays_from_the_new_checkpoint() {
    let (w, bytes) = confirmed_twice("gates-durable-torn-30");
    tear_last(&w, &bytes, kvik_log::cursor::CURSOR_BLOCK);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(seen(&ready(&mut c)), ["resync Resume [(Level(1), true)]"]);
    let d = s.stats().delivery;
    assert_eq!((d.gates_reseeded, d.checkpoint_mismatch), (0, 0));
    block_on(s.shutdown()).unwrap();
}

/// Failure table, "cursor file lost": a `NoCursor` resync; the gates are reseeded (state now, `set_by` none) and
/// counted; nothing is replayed.
#[test]
fn a_lost_cursor_file_reseeds_the_gates() {
    let (w, _) = confirmed_twice("gates-durable-lost");
    w.fs.poke(&cursor_path(&w), 0, &[0; 16]);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(
        seen(&ready(&mut c)),
        ["resync NoCursor [(Level(1), false)]"]
    );
    assert_eq!(s.stats().delivery.gates_reseeded, 1);
    block_on(s.shutdown()).unwrap();
}

/// Failure table, "failed write or sync at a cursor write": `confirm` returns the error, the store goes read-only and
/// nothing further is delivered. At the next start (no power loss) a failed write leaves the last synced checkpoint,
/// so the start transitions come again; a failed sync leaves the written pair in the page cache, confirmed.
#[test]
fn a_failed_cursor_write_at_a_confirmation_leaves_the_store_read_only() {
    for (op, again) in [
        (
            FaultOp::Write,
            &[
                "Some(Unknown) r=true",
                "Some(Level(0)) r=true",
                "resync Resume [(Level(0), true)]",
            ][..],
        ),
        (FaultOp::Sync, &["resync Resume [(Level(0), true)]"][..]),
    ] {
        let mut w = World::quiet(&format!("gates-durable-write-{op:?}"));
        let s = w.open();
        let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
        let mut c = take(&s);
        block_on(c.watch("humidity", humidity(id))).unwrap();
        emit(&w, &s, id, 50.0);
        let got = ready(&mut c);
        w.fs.fail_nth(op, "cursors", 0, std::io::ErrorKind::Other);
        assert!(
            matches!(block_on(c.confirm(got[1].id)), Err(ConsumerError::Write(_))),
            "{op:?}"
        );
        assert!(matches!(s.health(), Health::ReadOnly { .. }), "{op:?}");
        emit_ignoring(&w, &s, id, 80.0);
        w.clock.advance(Duration::from_secs(121));
        let _ = s.tick();
        assert!(c.try_next().is_err(), "{op:?}");
        w.crash(s, vec![c], false);
        let s = w.open();
        let mut c = take(&s);
        assert_eq!(seen(&ready(&mut c)), again, "{op:?}");
        block_on(s.shutdown()).unwrap();
    }
}

fn emit_ignoring(w: &World, s: &Store, id: SeriesId, v: f64) {
    let _ = block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))]));
}

/// `conditions.replay_max` (spec §6 Gates, Resume): more facts after the checkpoint than the bound, or more time,
/// and nothing is replayed; the gates are rebuilt from history without `set_by`, counted `transitions_not_replayed`.
#[test]
fn a_resume_beyond_the_replay_bound_reseeds_without_replaying() {
    for (name, max) in [
        ("facts", ReplayMax::new(Duration::from_secs(24 * 3_600), 2)),
        ("span", ReplayMax::new(Duration::from_secs(600), 1_000_000)),
    ] {
        let mut w = World::quiet(&format!("gates-durable-bound-{name}"));
        w.conditions = Some(ConditionsConfig::default().replay_max(max));
        let s = w.open();
        let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
        let mut c = take(&s);
        block_on(c.watch("humidity", humidity(id))).unwrap();
        emit(&w, &s, id, 50.0);
        for v in [55.0, 60.0, 80.0] {
            emit(&w, &s, id, v);
            w.clock.advance(Duration::from_secs(300));
        }
        s.tick().unwrap();
        // Nothing confirmed: the checkpoint stays before the start transition.
        assert_eq!(
            seen(&ready(&mut c)),
            [
                "Some(Unknown) r=false",
                "Some(Level(0)) r=false",
                "Some(Level(1)) r=false"
            ],
            "{name}"
        );
        w.crash(s, vec![c], false);
        let s = w.open();
        let mut c = take(&s);
        assert_eq!(
            seen(&ready(&mut c)),
            ["resync Resume [(Level(1), false)]"],
            "{name}"
        );
        let d = s.stats().delivery;
        assert_eq!(
            (d.transitions_not_replayed, d.gates_reseeded),
            (1, 1),
            "{name}"
        );
        block_on(s.shutdown()).unwrap();
    }
}

/// G10: with the host clock stepped back across a restart, no batch commits below the checkpoint's `now`, so a hold
/// that starts after the start runs from that instant, not from the stepped-back wall clock.
#[test]
fn a_clock_stepped_back_across_a_restart_does_not_shorten_a_hold() {
    let w = World::quiet("gates-durable-clock-back");
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    emit(&w, &s, id, 50.0);
    let got = ready(&mut c);
    block_on(c.confirm(got[1].id)).unwrap();
    w.clock.advance(Duration::from_secs(100));
    s.tick().unwrap();
    drop(c);
    block_on(s.shutdown()).unwrap();
    drop(s);
    w.clock.jump_wall(Span::from_secs(-90));
    let s = w.open();
    let mut c = take(&s);
    let r = ready(&mut c);
    block_on(c.confirm(r.last().unwrap().id)).unwrap();
    emit(&w, &s, id, 80.0);
    // Committed at the checkpoint's instant, 90 s after the wall clock, the hold ends 210 s after the emit.
    w.clock.advance(Duration::from_secs(200));
    s.tick().unwrap();
    assert_eq!(seen(&ready(&mut c)), Vec::<String>::new());
    w.clock.advance(Duration::from_secs(11));
    s.tick().unwrap();
    assert_eq!(seen(&ready(&mut c)), ["Some(Level(1)) r=false"]);
    block_on(s.shutdown()).unwrap();
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
        REPLAYS_CHECKED.fetch_add(before.len() as u64, Ordering::SeqCst);
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
    assert!(
        REPLAYS_CHECKED.load(Ordering::SeqCst) > 0,
        "no history replayed a transition handed out and never confirmed"
    );
}

/// Transitions the crash property saw replayed after a crash, so a run that never reaches that check fails.
static REPLAYS_CHECKED: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}
