//! The seed at a gate's start (Q-1145 (b), Q-1390): an instance started mid-history has the
//! state, and then the transitions, of the same gate watched at the look-back's start over what the seed
//! reads, and emits nothing before its start.

mod gate_common;

use gate_common::*;
use kvik_model::gate::{Entry, Gate, GateState, Kind, Level, Selector, Transition, judge};
use kvik_model::gate::{Input, Window};
use kvik_model::summary::{Aggregate, Params};
use kvik_model::types::{
    DAY, Fact, Kind as SeriesKind, Nanos, Position, Quality, SECOND, SeriesId, Value,
};
use proptest::prelude::*;

const OTHER: SeriesId = SeriesId(2);
const END: i64 = 10_000;

fn run(h: &H) -> kvik_model::gate::Judgement {
    judge(&h.entries, END * SECOND).expect("judge")
}

fn fact_at(h: H, commit: i64, t: i64, v: f64) -> H {
    h.fact_on(S, commit * SECOND, t * SECOND, Value::Num(v), Quality::Good)
}

fn hold_5_both_ways() -> Gate {
    levels(&[Level::at(10.0).hold(5 * SECOND, 5 * SECOND)])
}

#[test]
fn a_gate_watched_after_its_series_has_data_starts_from_none_at_its_state_now_judged_from_history()
{
    let h = H::new().num(1, 25.0).watch(
        20,
        C,
        levels(&[
            Level::at(10.0).hold(5 * SECOND, 0),
            Level::at(20.0).hold(5 * SECOND, 0),
        ]),
    );
    let watch = h.last();
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(2))]);
    assert_eq!(j.transitions[0].id.at, watch);
    assert_eq!(j.transitions[0].value, Some(Value::Num(25.0)));
    assert_eq!(j.transitions[0].event_time, Some(SECOND));
    assert_eq!(
        j.states[&(C, S)],
        (GateState::Level(2), j.transitions[0].id)
    );
}

#[test]
fn a_hold_is_seeded_from_before_the_watch_so_a_value_long_above_starts_crossed() {
    let h = H::new()
        .num(1, 11.0)
        .watch(100, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]));
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(1))]);
    assert_eq!(instants(&j.transitions), vec![100]);
}

#[test]
fn a_seed_reads_twice_the_longest_hold_back_and_counts_from_the_first_reading_inside() {
    let gate = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    let h = H::new()
        .num(1, 0.0)
        .num(91, 11.0)
        .num(99, 11.0)
        .watch(100, C, gate);
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(1))]);
}

#[test]
fn a_value_that_rose_shortly_before_the_watch_completes_its_hold_late_never_early() {
    let h = H::new().num(1, 0.0).num(98, 11.0).watch(
        100,
        C,
        levels(&[Level::at(10.0).hold(5 * SECOND, 0)]),
    );
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0)), (lv(0), lv(1))]);
    assert_eq!(instants(&j.transitions), vec![100, 103]);
}

#[test]
fn the_look_back_is_at_most_a_day() {
    let gate = levels(&[
        Level::at(10.0).clear(5.0),
        Level::at(100.0).hold(84_600 * SECOND, 0),
    ]);
    let h = H::new()
        .num(0, 11.0)
        .num(3_600, 7.0)
        .watch(26 * 3_600, C, gate);
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(0))]);
}

#[test]
fn a_gate_without_holds_reads_no_history_so_its_boundaries_start_uncrossed() {
    let h = H::new()
        .num(1, 30.0)
        .num(2, 15.0)
        .watch(10, C, levels(&[Level::at(20.0).clear(10.0)]));
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(0))]);
}

#[test]
fn a_set_hold_is_seeded_too() {
    let set = gate(Kind::Set {
        members: vec![Value::Num(3.0)],
        hold_on: 5 * SECOND,
        hold_off: 0,
    });
    let h = H::new().num(1, 3.0).watch(100, C, set);
    assert_eq!(
        moves(&run(&h).transitions),
        vec![(None, Some(GateState::Match(true)))]
    );
}

#[test]
fn a_series_joining_an_each_is_seeded_from_its_history_at_the_join() {
    let mut each = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    each.selector = Selector::Each("room/*/temp".into());
    let s2 = SeriesId(2);
    let h = H::new()
        .watch(0, C, each)
        .num_on(s2, 1, 11.0)
        .define(100, s2, &["room/b/temp"]);
    let join = h.last();
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(1))]);
    assert_eq!(j.transitions[0].id.at, join);
}

#[test]
fn a_backfill_committed_inside_the_look_back_is_not_the_live_value_in_the_seed() {
    let h = fact_at(fact_at(H::new(), 50, 50, 5.0), 92, 40, 50.0).watch(100, C, hold_5_both_ways());
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0))]);
}

#[test]
fn an_outranked_fact_committed_inside_the_look_back_is_not_the_live_value_in_the_seed() {
    let mut live = fact(
        S,
        Position::ORIGIN,
        50 * SECOND,
        50 * SECOND,
        Value::Num(5.0),
        Quality::Good,
    );
    live.changed_at = Some(9);
    let mut outranked = fact(
        S,
        Position::ORIGIN,
        92 * SECOND,
        50 * SECOND,
        Value::Num(50.0),
        Quality::Good,
    );
    outranked.changed_at = Some(1);
    let h = H::new()
        .push_fact(50 * SECOND, live)
        .push_fact(92 * SECOND, outranked)
        .watch(100, C, hold_5_both_ways());
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(0))]);
}

#[test]
fn a_backfill_before_a_late_rise_does_not_start_the_hold_early() {
    let h = fact_at(
        fact_at(fact_at(H::new(), 60, 60, 5.0), 91, 40, 50.0),
        99,
        99,
        50.0,
    )
    .watch(100, C, hold_5_both_ways());
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0)), (lv(0), lv(1))]);
    assert_eq!(instants(&j.transitions), vec![100, 104]);
}

#[test]
fn a_future_fact_replaced_before_the_look_back_takes_effect_as_its_replacement() {
    let h = fact_at(
        fact_at(fact_at(fact_at(H::new(), 0, 0, 0.0), 0, 0, 0.0), 0, 27, 0.0),
        5,
        27,
        5.0,
    )
    .watch(15, C, levels(&[Level::at(4.0).hold(7 * SECOND, 0)]));
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0)), (lv(0), lv(1))]);
    assert_eq!(instants(&j.transitions), vec![15, 34]);
}

#[test]
fn a_gate_without_holds_starts_from_the_last_fact_committed_at_its_watchs_instant() {
    let h = H::new()
        .num(0, 3.0)
        .num(0, 2.0)
        .watch(0, C, levels(&[Level::at(2.0)]));
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(0))]);
}

#[test]
fn a_window_in_the_seed_aggregates_by_the_series_definition() {
    let mut g = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    g.input = Input::Window(Window {
        width: 20 * SECOND,
        aggregate: Aggregate::Increase,
        min_coverage: 0,
        cadence: SECOND,
    });
    let mut h = H::new().define(0, S, &["meter"]);
    if let Some(Entry::Define { params, .. }) = h.entries.last_mut() {
        *params = Params::new(SeriesKind::Counter, None);
    }
    let h = fact_at(fact_at(fact_at(h, 80, 80, 0.0), 85, 85, 20.0), 90, 90, 40.0).watch(100, C, g);
    assert_eq!(moves(&run(&h).transitions)[0], (None, lv(1)));
}

#[test]
fn a_crossing_between_one_and_two_longest_holds_back_is_kept_by_hysteresis() {
    let h = fact_at(
        fact_at(fact_at(H::new(), 1, 1, 0.0), 91, 91, 11.0),
        93,
        93,
        7.0,
    )
    .watch(
        100,
        C,
        levels(&[Level::at(10.0).clear(5.0).hold(SECOND, 5 * SECOND)]),
    );
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(1))]);
}

#[test]
fn a_fact_committed_at_the_look_backs_start_is_read() {
    let h = fact_at(fact_at(H::new(), 1, 1, 0.0), 90, 90, 11.0).watch(
        100,
        C,
        levels(&[Level::at(10.0).hold(5 * SECOND, 0)]),
    );
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(1))]);
}

#[test]
fn the_longest_hold_counts_hold_down() {
    let h = fact_at(fact_at(H::new(), 1, 1, 11.0), 98, 98, 0.0).watch(
        100,
        C,
        levels(&[Level::at(10.0).hold(0, 5 * SECOND)]),
    );
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(1)), (lv(1), lv(0))]);
    assert_eq!(instants(&j.transitions), vec![100, 103]);
}

#[test]
fn a_sets_longest_hold_counts_hold_off() {
    let set = gate(Kind::Set {
        members: vec![Value::Num(3.0)],
        hold_on: 0,
        hold_off: 5 * SECOND,
    });
    let h = fact_at(fact_at(H::new(), 1, 1, 3.0), 98, 98, 0.0).watch(100, C, set);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions),
        vec![
            (None, Some(GateState::Match(true))),
            (Some(GateState::Match(true)), Some(GateState::Match(false)))
        ]
    );
    assert_eq!(instants(&j.transitions), vec![100, 103]);
}

#[test]
fn a_bad_fact_inside_the_look_back_leaves_the_earlier_live_value_in_the_seed() {
    let h = fact_at(H::new(), 1, 1, 11.0)
        .fact_on(S, 95 * SECOND, 95 * SECOND, Value::Num(0.0), Quality::Bad)
        .watch(100, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]));
    assert_eq!(moves(&run(&h).transitions), vec![(None, lv(1))]);
}

#[test]
fn a_future_fact_taking_effect_inside_the_look_back_holds_from_its_t() {
    let h = fact_at(fact_at(H::new(), 1, 1, 0.0), 1, 97, 11.0).watch(
        100,
        C,
        levels(&[Level::at(10.0).hold(5 * SECOND, 0)]),
    );
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0)), (lv(0), lv(1))]);
    assert_eq!(instants(&j.transitions), vec![100, 102]);
}

#[test]
fn a_series_stale_throughout_the_look_back_still_completes_its_hold_in_the_seed() {
    let mut g = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    g.stale_after = Some(8 * SECOND);
    let h = fact_at(
        fact_at(H::new(), 1, 1, 50.0).watch(100, C, g),
        110,
        110,
        50.0,
    );
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions[..2]),
        vec![
            (None, Some(GateState::Stale)),
            (Some(GateState::Stale), lv(1))
        ]
    );
    assert_eq!(instants(&j.transitions[..2]), vec![100, 110]);
}

#[derive(Clone, Debug)]
enum Op {
    Fact {
        dt: i64,
        other: bool,
        v: u8,
        ahead: i64,
        bad: bool,
    },
    Quiet(i64),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (0..12i64, prop::bool::weighted(0.2), 0..=10u8,
              prop_oneof![9 => Just(0i64), 1 => 1..30i64], prop::bool::weighted(0.1))
            .prop_map(|(dt, other, v, ahead, bad)| Op::Fact { dt, other, v, ahead, bad }),
        1 => (10..40i64).prop_map(Op::Quiet),
    ]
}

fn arb_gate() -> impl Strategy<Value = Gate> {
    let hold = || prop_oneof![1 => Just(0i64), 2 => Just(3), 2 => Just(7)];
    let level = (1..=9i32, prop_oneof![Just(0.0), Just(1.5)], hold(), hold()).prop_map(
        |(at, gap, up, down)| {
            let at = f64::from(at);
            Level::at(at)
                .clear(at - gap)
                .hold(up * SECOND, down * SECOND)
        },
    );
    prop_oneof![
        prop::collection::vec(level, 1..=3).prop_map(|mut ls| {
            ls.sort_by(|a, b| a.at.total_cmp(&b.at));
            ls.dedup_by(|a, b| a.at == b.at);
            for k in 1..ls.len() {
                ls[k].clear = ls[k].clear.max(ls[k - 1].clear);
            }
            levels(&ls)
        }),
        (prop::collection::vec(0..=10u8, 1..4), hold(), hold()).prop_map(|(ms, on, off)| {
            gate(Kind::Set {
                members: ms.into_iter().map(|m| Value::Num(f64::from(m))).collect(),
                hold_on: on * SECOND,
                hold_off: off * SECOND,
            })
        }),
    ]
}

#[derive(Clone, Debug)]
struct Case {
    gate: Gate,
    ops: Vec<Op>,
    watch_after: usize,
    watch_dt: i64,
}

fn arb_case() -> impl Strategy<Value = Case> {
    (
        arb_gate(),
        prop::collection::vec(arb_op(), 0..30),
        0..30usize,
        0..12i64,
    )
        .prop_map(|(gate, ops, w, watch_dt)| {
            let watch_after = w.min(ops.len());
            Case {
                gate,
                ops,
                watch_after,
                watch_dt,
            }
        })
}

/// The history in seconds, the watch's position and its commit time.
fn history(c: &Case) -> (Vec<Entry>, Position, Nanos) {
    let mut h = H::new();
    let mut now = 0;
    let mut watch = (Position::ORIGIN, 0);
    for (k, op) in c.ops.iter().enumerate() {
        if k == c.watch_after {
            now += c.watch_dt;
            h = h.watch(now, C, c.gate.clone());
            watch = (h.last(), now * SECOND);
        }
        h = match *op {
            Op::Fact {
                dt,
                other,
                v,
                ahead,
                bad,
            } => {
                now += dt;
                let s = if other { OTHER } else { S };
                let q = if bad { Quality::Bad } else { Quality::Good };
                h.fact_on(
                    s,
                    now * SECOND,
                    (now + ahead) * SECOND,
                    Value::Num(f64::from(v)),
                    q,
                )
            }
            Op::Quiet(dt) => {
                now += dt;
                h
            }
        };
    }
    if c.watch_after == c.ops.len() {
        now += c.watch_dt;
        h = h.watch(now, C, c.gate.clone());
        watch = (h.last(), now * SECOND);
    }
    (h.entries, watch.0, watch.1)
}

fn longest_hold(g: &Gate) -> Nanos {
    match &g.kind {
        Kind::Levels(ls) => ls
            .iter()
            .map(|l| l.hold_up.max(l.hold_down))
            .max()
            .unwrap_or(0),
        Kind::Set {
            hold_on, hold_off, ..
        } => (*hold_on).max(*hold_off),
        Kind::Change { .. } => 0,
    }
}

fn s_facts(h: &[Entry]) -> impl Iterator<Item = (Nanos, &Fact)> {
    h.iter().filter_map(|e| match e {
        Entry::Fact { commit_time, fact } if fact.series == S => Some((*commit_time, fact)),
        _ => None,
    })
}

fn usable(f: &Fact) -> bool {
    f.quality != Quality::Bad && !matches!(f.value, Value::Absent)
}

/// The live-state fact of `S` at `x` over the facts before `upto` (spec §2.3): per event time the last
/// fact (no versions here, so the last outranks), then the latest event time whose fact has a value.
fn live(h: &[Entry], upto: Position, x: Nanos) -> Option<Fact> {
    let mut current: std::collections::BTreeMap<Nanos, &Fact> = Default::default();
    for (_, f) in s_facts(h).filter(|(_, f)| f.position < upto && f.t <= x) {
        current.insert(f.t, f);
    }
    current
        .values()
        .rev()
        .find(|f| usable(f))
        .map(|f| (*f).clone())
}

/// The early watch the seed stands for, built from the history alone, as entries renumbered in order:
/// the watch at the look-back's start; the series' state then, the current fact per event time among
/// those committed before the look-back, committed at its start in descending event time so the watch
/// first sees a value when it sees the live one; the facts committed inside the look-back; the live fact
/// at the start committed at it (the start's own evaluation); then every later fact of `S`. Returns the
/// entries and how many precede the later facts.
fn early_watch(c: &Case, h: &[Entry], watch: Position, start: Nanos) -> (Vec<Entry>, usize) {
    let from = start - (2 * longest_hold(&c.gate)).min(DAY);
    // With no hold the seed reads nothing: every fact before the watch is the state at the start.
    let cut = if from == start { start + 1 } else { from };
    let mut earlier: std::collections::BTreeMap<Nanos, Fact> = Default::default();
    for (_, f) in s_facts(h).filter(|(ct, f)| f.position < watch && *ct < cut) {
        earlier.insert(f.t, f.clone());
    }
    let inside: Vec<(Nanos, Fact)> = s_facts(h)
        .filter(|(ct, f)| f.position < watch && *ct >= cut)
        .map(|(ct, f)| (ct, f.clone()))
        .collect();
    let mut seq = 0u64;
    let mut out = Vec::new();
    let mut push = |out: &mut Vec<Entry>, commit_time: Nanos, mut fact: Fact| {
        seq += 1;
        fact.position = Position::new(0, seq);
        out.push(Entry::Fact { commit_time, fact });
    };
    out.push(Entry::Watch {
        position: Position::ORIGIN,
        commit_time: from,
        id: C,
        gate: c.gate.clone(),
    });
    for f in earlier.into_values().rev() {
        push(&mut out, from, f);
    }
    for (ct, f) in inside {
        push(&mut out, ct, f);
    }
    if let Some(f) = live(h, watch, start) {
        push(&mut out, start, f);
    }
    let before = out.len();
    for (ct, f) in s_facts(h).filter(|(_, f)| f.position > watch) {
        push(&mut out, ct, f.clone());
    }
    (out, before)
}

type Seen = (Option<GateState>, Option<GateState>, Nanos, Option<Value>);

fn summary(ts: &[Transition]) -> Vec<Seen> {
    ts.iter()
        .map(|t| (t.from.clone(), t.to.clone(), t.now, t.value.clone()))
        .collect()
}

proptest! {
    #[test]
    fn a_seeded_start_judges_as_the_same_gate_watched_at_the_look_backs_start(c in arb_case()) {
        let (h, watch, start) = history(&c);
        let until = h.last().map_or(0, Entry::commit_time) + 40 * SECOND;
        let j = judge(&h, until).expect("judge");
        let ours: Vec<Transition> = j.transitions.iter().filter(|t| t.id.condition == C).cloned().collect();
        prop_assert!(ours.iter().all(|t| t.id.at >= watch), "{ours:?}");
        let first = ours.first().expect("a start transition");
        prop_assert_eq!((first.id.at, &first.from), (watch, &None));

        let (early, before) = early_watch(&c, &h, watch, start);
        let at_start = judge(&early[..before], start).expect("judge");
        let state = at_start.states.get(&(C, S)).map(|(s, _)| s.clone()).unwrap_or(GateState::Unknown);
        prop_assert_eq!(first.to.clone(), Some(state));

        let whole = judge(&early, until).expect("judge");
        let later = &whole.transitions[at_start.transitions.len()..];
        prop_assert_eq!(summary(&ours[1..]), summary(later));
    }
}

#[test]
fn seed_generator_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut crossed, mut carried, mut late) = (0, 0, 0);
    for _ in 0..300 {
        let c = arb_case().new_tree(&mut runner).expect("case").current();
        let (h, watch, start) = history(&c);
        let j = judge(&h, start + 40 * SECOND).expect("judge");
        let first = j
            .transitions
            .iter()
            .find(|t| t.id.condition == C)
            .expect("start");
        if longest_hold(&c.gate) > 0 && matches!(&first.to, Some(GateState::Level(n)) if *n > 0) {
            crossed += 1;
        }
        let from = start - 2 * longest_hold(&c.gate);
        let inside = s_facts(&h).any(|(ct, f)| f.position < watch && ct >= from && usable(f));
        if longest_hold(&c.gate) > 0 && !inside && live(&h, watch, start).is_some() {
            carried += 1;
        }
        if j.transitions
            .iter()
            .any(|t| t.id.condition == C && t.id.due.is_some())
        {
            late += 1;
        }
    }
    assert!(crossed >= 10, "starts crossed by a seed: {crossed}");
    assert!(carried >= 10, "live fact carried: {carried}");
    assert!(late >= 10, "holds completing after the start: {late}");
}

#[test]
fn a_fact_at_the_origin_position_is_refused_as_out_of_order_not_a_panic_in_the_seed() {
    // Q-1800: positions start at 0:1, and the seed's inner run starts after the origin.
    let f = fact(
        S,
        Position::ORIGIN,
        98 * SECOND,
        98 * SECOND,
        Value::Num(25.0),
        Quality::Good,
    );
    let mut h = H::new().watch(100, C, hold_5_both_ways());
    h.entries.insert(
        0,
        Entry::Fact {
            commit_time: 98 * SECOND,
            fact: f,
        },
    );
    assert_eq!(
        judge(&h.entries, END * SECOND).map(|_| ()),
        Err(kvik_model::gate::GateError::Order(Position::ORIGIN))
    );
}
