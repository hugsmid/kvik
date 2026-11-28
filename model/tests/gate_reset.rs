//! Reset points in the judgement (cond2 design §4.4 step 5; Q-2170): every instance seeded again at the reset, with
//! no transition.

mod gate_common;

use gate_common::*;
use kvik_model::gate::{Cause, Entry, Gate, GateState, Kind, Level, Selector, TransitionId, judge};
use kvik_model::types::{Nanos, Position, Quality, SECOND, SeriesId, Value};
use proptest::prelude::*;

fn at_reset(h: &H) -> Position {
    h.entries
        .iter()
        .find(|e| matches!(e, Entry::Reset { .. }))
        .map(Entry::position)
        .expect("a reset")
}

#[test]
fn a_reset_forgets_hysteresis_and_emits_nothing() {
    let g = levels(&[Level::at(10.0).clear(5.0)]);
    let h = H::new().watch(0, C, g).num(1, 12.0).num(2, 7.0).reset(3);
    let p = at_reset(&h);
    let h = h.num(4, 8.0);
    let j = judge(&h.entries, 10 * SECOND).unwrap();
    assert_eq!(
        moves(&j.transitions),
        [
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(1))
        ]
    );
    let set_by = TransitionId {
        at: p,
        due: None,
        condition: C,
        series: S,
    };
    assert_eq!(j.states[&(C, S)], (GateState::Level(0), set_by));
}

#[test]
fn a_reset_seeds_a_hold_from_before_it() {
    let g = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    let h = H::new()
        .watch(0, C, g)
        .num(1, 12.0)
        .num(2, 4.0)
        .num(3, 12.0)
        .reset(5)
        .num(6, 12.0);
    let j = judge(&h.entries, 20 * SECOND).unwrap();
    // The hold begun at 3 s completes at 8 s, after the reset, as seeded; unseeded it would wait for 11 s.
    assert_eq!(instants(&j.transitions), [0, 1, 8]);
    assert_eq!(j.transitions.last().unwrap().to, lv(1));
}

#[test]
fn a_reset_keeps_an_each_gates_series_and_its_refusals() {
    let mut g = levels(&[Level::at(10.0)]);
    g.selector = Selector::Each("room/*/temp".into());
    let h = H::new()
        .define(0, SeriesId(1), &["room/a/temp"])
        .watch(0, C, g)
        .num_on(SeriesId(1), 1, 12.0)
        .reset(2)
        .num_on(SeriesId(1), 3, 4.0);
    let j = judge(&h.entries, 10 * SECOND).unwrap();
    assert_eq!(
        moves(&j.transitions),
        [
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(1)),
            (lv(1), lv(0))
        ]
    );
    assert_eq!(j.instances_refused, 0);
}

#[test]
fn a_reset_with_no_gate_in_force_changes_nothing() {
    let h = H::new()
        .num(1, 12.0)
        .reset(2)
        .watch(3, C, levels(&[Level::at(10.0)]));
    let j = judge(&h.entries, 10 * SECOND).unwrap();
    assert_eq!(moves(&j.transitions), [(None, lv(1))]);
}

#[derive(Clone, Debug)]
enum Op {
    Fact {
        dt: i64,
        series: u32,
        v: u8,
        ahead: i64,
        bad: bool,
    },
}

fn arb_op() -> impl Strategy<Value = Op> {
    (
        0..12i64,
        prop_oneof![5 => Just(1u32), 1 => Just(2u32)],
        0..=10u8,
        prop_oneof![9 => Just(0i64), 1 => 1..8i64],
        prop::bool::weighted(0.1),
    )
        .prop_map(|(dt, series, v, ahead, bad)| Op::Fact {
            dt,
            series,
            v,
            ahead,
            bad,
        })
}

fn arb_gate() -> impl Strategy<Value = Gate> {
    let hold = prop_oneof![2 => Just(0i64), 1 => Just(3), 1 => Just(7)].prop_map(|h| h * SECOND);
    let stale = prop_oneof![Just(None), Just(Some(4 * SECOND)), Just(Some(15 * SECOND))];
    let kind = prop_oneof![
        (
            hold.clone(),
            hold.clone(),
            prop_oneof![Just(0.0), Just(2.5)]
        )
            .prop_map(|(u, d, g)| Kind::Levels(vec![
                Level::at(5.0).clear(5.0 - g).hold(u, d),
                Level::at(8.0).clear(8.0 - g).hold(d, u),
            ])),
        (hold.clone(), hold).prop_map(|(on, off)| Kind::Set {
            members: vec![Value::Num(3.0), Value::Num(9.0)],
            hold_on: on,
            hold_off: off,
        }),
        Just(Kind::Change { deadband: 1.5 }),
    ];
    (kind, stale).prop_map(|(kind, stale_after)| {
        let mut g = gate(kind);
        g.stale_after = stale_after;
        g
    })
}

/// The history with the reset after `k` ops, and the same history with the gate watched there instead of at the
/// start.
fn histories(g: &Gate, ops: &[Op], k: usize) -> (Vec<Entry>, Vec<Entry>, Nanos) {
    let (mut a, mut b) = (H::new().watch(0, C, g.clone()), H::new());
    // `b` keeps `a`'s positions: an entry at the watch's place that judges nothing.
    b = b.reset(0);
    let mut now = 0;
    for (i, op) in ops.iter().enumerate() {
        if i == k {
            a = a.reset(0);
            b = b.watch(0, C, g.clone());
            for h in [&mut a, &mut b] {
                set_commit(h, now);
            }
        }
        let Op::Fact {
            dt,
            series,
            v,
            ahead,
            bad,
        } = *op;
        now += dt * SECOND;
        let q = if bad { Quality::Bad } else { Quality::Good };
        let value = Value::Num(f64::from(v));
        a = a.fact_on(
            SeriesId(series),
            now,
            now + ahead * SECOND,
            value.clone(),
            q,
        );
        b = b.fact_on(SeriesId(series), now, now + ahead * SECOND, value, q);
    }
    (a.entries, b.entries, now + 40 * SECOND)
}

fn set_commit(h: &mut H, now: Nanos) {
    match h.entries.last_mut() {
        Some(Entry::Reset { commit_time, .. } | Entry::Watch { commit_time, .. }) => {
            *commit_time = now
        }
        _ => unreachable!(),
    }
}

proptest! {
    /// A reset is a start at its position without the start transition: what follows it, and the states at the end,
    /// are what a gate watched there gives (G7 for a reseed).
    #[test]
    fn after_a_reset_the_judgement_is_that_of_a_gate_started_there(
        g in arb_gate(),
        ops in prop::collection::vec(arb_op(), 1..40),
        k in 0usize..40,
    ) {
        let k = k % ops.len();
        let (a, b, until) = histories(&g, &ops, k);
        let reset = a.iter().find(|e| matches!(e, Entry::Reset { position, .. } if position.sequence > 1))
            .map(Entry::position).expect("a reset");
        let ja = judge(&a, until).unwrap();
        let jb = judge(&b, until).unwrap();
        let after: Vec<_> = ja.transitions.iter().filter(|t| t.id.at >= reset).cloned().collect();
        prop_assert!(after.iter().all(|t| t.id.at > reset || t.id.due.is_some()), "a reset emits nothing");
        let started: Vec<_> = jb.transitions.iter()
            .filter(|t| !(t.id.at == reset && matches!(t.cause, Cause::Entry { .. })))
            .cloned().collect();
        prop_assert_eq!(after, started);
        prop_assert_eq!(ja.states, jb.states);
    }
}
