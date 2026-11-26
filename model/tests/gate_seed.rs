//! The seed at a gate's start (Q-1145 (b), Q-1390): an instance started mid-history has the
//! state, and then the transitions, of the same gate watched at the look-back's start over what the seed
//! reads, and emits nothing before its start.

mod gate_common;

use gate_common::*;
use kvik_model::gate::{Entry, Gate, GateState, Kind, Level, Transition, judge};
use kvik_model::types::{DAY, Fact, Nanos, Position, Quality, SECOND, SeriesId, Value};
use proptest::prelude::*;

const OTHER: SeriesId = SeriesId(2);

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
/// the watch at the look-back's start; the live fact at the start when no fact committed inside the
/// look-back has a value; those facts; the live fact at the start committed at it (the start's own
/// evaluation); facts committed before the look-back that take effect after the start; then every later
/// fact of `S`. Returns the entries and how many precede the later facts.
fn early_watch(c: &Case, h: &[Entry], watch: Position, start: Nanos) -> (Vec<Entry>, usize) {
    let from = start - (2 * longest_hold(&c.gate)).min(DAY);
    let inside: Vec<(Nanos, Fact)> = s_facts(h)
        .filter(|(ct, f)| f.position < watch && *ct >= from)
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
    let now_live = live(h, watch, start);
    if !inside.iter().any(|(_, f)| usable(f))
        && let Some(f) = now_live.clone()
    {
        push(&mut out, from, f);
    }
    for (ct, f) in inside {
        push(&mut out, ct, f);
    }
    if let Some(f) = now_live {
        push(&mut out, start, f);
    }
    let late: Vec<Fact> = s_facts(h)
        .filter(|(ct, f)| f.position < watch && *ct < from && f.t > start)
        .map(|(_, f)| f.clone())
        .collect();
    for f in late {
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
