//! Properties of the gate judgement (spec §6 Gates) over generated histories of one stream, several
//! gates on it and an `Each` over three series.

mod gate_common;

use std::collections::BTreeMap;

use gate_common::*;
use kvik_model::gate::{
    ConditionId, Entry, Frontier, Gate, GatePoint, GateState, Input, Judgement, Kind, Level,
    ReplayBound, Selector, Transition, TransitionId, Window, judge, pattern_matches, resume,
};
use kvik_model::summary::Aggregate;
use kvik_model::types::{Nanos, Position, Quality, SECOND, SeriesId, Value};
use proptest::prelude::*;

const NAMES: [&str; 3] = ["room/a/temp", "room/b/temp", "hall/a/temp"];
const PATTERN: &str = "room/*/temp";
const EACH: ConditionId = ConditionId(9);

#[derive(Clone, Debug)]
enum Op {
    Fact {
        dt: i64,
        series: u32,
        v: u8,
        ahead: i64,
        bad: bool,
    },
    Define {
        dt: i64,
        series: u32,
        name: usize,
    },
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (0..12i64, prop_oneof![6 => Just(1u32), 1 => 2..4u32], 0..=10u8,
              prop_oneof![9 => Just(0i64), 1 => 1..8i64], prop::bool::weighted(0.1))
            .prop_map(|(dt, series, v, ahead, bad)| Op::Fact { dt, series, v, ahead, bad }),
        1 => (0..5i64, 1..4u32, 0..3usize).prop_map(|(dt, series, name)| Op::Define { dt, series, name }),
    ]
}

fn arb_levels() -> impl Strategy<Value = Vec<Level>> {
    let holds = prop_oneof![2 => Just(0i64), 1 => Just(3), 1 => Just(7)];
    proptest::sample::subsequence((1..=9).collect::<Vec<i32>>(), 1..=3)
        .prop_flat_map(move |ats| {
            let n = ats.len();
            (
                Just(ats),
                prop::collection::vec(prop_oneof![Just(0.0), Just(0.5), Just(1.5)], n),
                prop::collection::vec((holds.clone(), holds.clone()), n),
            )
        })
        .prop_map(|(ats, gaps, holds)| {
            let mut out: Vec<Level> = Vec::new();
            for ((a, g), (u, d)) in ats.into_iter().zip(gaps).zip(holds) {
                let at = f64::from(a);
                let floor = out.last().map_or(f64::MIN, |l| l.clear);
                out.push(
                    Level::at(at)
                        .clear((at - g).max(floor))
                        .hold(u * SECOND, d * SECOND),
                );
            }
            out
        })
}

#[derive(Clone, Debug)]
struct Case {
    gates: Vec<(ConditionId, Gate)>,
    ops: Vec<Op>,
}

fn arb_case() -> impl Strategy<Value = Case> {
    let stale = prop_oneof![Just(None), Just(Some(15 * SECOND))];
    (
        arb_levels(),
        arb_levels(),
        stale.clone(),
        stale,
        prop::collection::vec(0..=10u8, 1..4),
        (0..4i64, 0..4i64),
        0..=500u16,
        prop::collection::vec(arb_op(), 0..40),
    )
        .prop_map(|(l1, l2, st1, st2, members, (on, off), cov, ops)| {
            let mut value = levels(&l1);
            value.stale_after = st1;
            let mut window = levels(&l2);
            window.stale_after = st2;
            window.input = Input::Window(Window {
                width: 10 * SECOND,
                aggregate: Aggregate::Max,
                min_coverage: cov,
                cadence: 2 * SECOND,
            });
            let set = gate(Kind::Set {
                members: members
                    .into_iter()
                    .map(|m| Value::Num(f64::from(m)))
                    .collect(),
                hold_on: on * SECOND,
                hold_off: off * SECOND,
            });
            let mut change = gate(Kind::Change { deadband: 1.5 });
            change.stale_after = st1;
            let mut each = levels(&l1);
            each.selector = Selector::Each(PATTERN.into());
            Case {
                gates: vec![
                    (ConditionId(1), value),
                    (ConditionId(2), window),
                    (ConditionId(3), set),
                    (ConditionId(4), change),
                    (EACH, each),
                ],
                ops,
            }
        })
}

fn history(c: &Case) -> Vec<Entry> {
    let mut h = H::new();
    for (id, g) in &c.gates {
        h = h.watch(0, *id, g.clone());
    }
    let mut now = 0;
    for op in &c.ops {
        h = match *op {
            Op::Fact {
                dt,
                series,
                v,
                ahead,
                bad,
            } => {
                now += dt * SECOND;
                let q = if bad { Quality::Bad } else { Quality::Good };
                h.fact_on(
                    SeriesId(series),
                    now,
                    now + ahead * SECOND,
                    Value::Num(f64::from(v)),
                    q,
                )
            }
            Op::Define { dt, series, name } => {
                now += dt * SECOND;
                let mut e = h.define(0, SeriesId(series), &[NAMES[name]]);
                if let Some(Entry::Define { commit_time, .. }) = e.entries.last_mut() {
                    *commit_time = now;
                }
                e
            }
        };
    }
    h.entries
}

fn last_commit(h: &[Entry]) -> Nanos {
    h.last().map_or(0, Entry::commit_time)
}

fn until(h: &[Entry]) -> Nanos {
    last_commit(h) + 40 * SECOND
}

/// Live-state value of `S` at instant `x`, by scanning the history (spec §2.3).
fn live_at(h: &[Entry], x: Nanos) -> Option<f64> {
    let mut current: BTreeMap<Nanos, (u64, Position, f64, bool)> = BTreeMap::new();
    for e in h {
        if let Entry::Fact { commit_time, fact } = e
            && fact.series == S
            && *commit_time <= x
        {
            let rank = fact.rank();
            let keep = current.get(&fact.t).is_none_or(|c| (c.0, c.1) < rank);
            if keep {
                let v = fact.value.as_f64().unwrap_or(f64::NAN);
                current.insert(fact.t, (rank.0, rank.1, v, fact.quality == Quality::Bad));
            }
        }
    }
    current
        .range(..=x)
        .rev()
        .find(|(_, c)| !c.3)
        .map(|(_, c)| c.2)
}

/// Instants at which `S`'s live value can change in `[from, to]`, with `from` itself.
fn change_points(h: &[Entry], from: Nanos, to: Nanos) -> Vec<Nanos> {
    let mut xs = vec![from, to];
    for e in h {
        if let Entry::Fact { commit_time, fact } = e
            && fact.series == S
        {
            xs.extend(
                [*commit_time, fact.t]
                    .into_iter()
                    .filter(|x| (from..=to).contains(x)),
            );
        }
    }
    xs
}

/// Last arrival of `series` at instant `x` over the entries up to `upto` (a timer at `x` precedes the
/// entries committed at `x`, Q-1144).
fn last_arrival(h: &[Entry], series: SeriesId, x: Nanos, upto: Position) -> Option<Nanos> {
    let mut best: BTreeMap<Nanos, (u64, Position)> = BTreeMap::new();
    let mut arrivals = Vec::new();
    for e in h {
        if let Entry::Fact { commit_time, fact } = e
            && fact.series == series
            && *commit_time <= x
            && fact.position <= upto
        {
            if best.get(&fact.t).is_some_and(|r| *r > fact.rank()) {
                continue;
            }
            best.insert(fact.t, fact.rank());
            if fact.quality != Quality::Bad && !matches!(fact.value, Value::Absent) {
                arrivals.push((*commit_time).max(fact.t));
            }
        }
    }
    arrivals.into_iter().filter(|a| *a <= x).max()
}

fn levels_of(c: &Case, id: ConditionId) -> Option<Vec<Level>> {
    c.gates
        .iter()
        .find(|(i, _)| *i == id)
        .and_then(|(_, g)| match &g.kind {
            Kind::Levels(ls) => Some(ls.clone()),
            _ => None,
        })
}

fn instance_chains(ts: &[Transition]) -> BTreeMap<(ConditionId, SeriesId), Vec<&Transition>> {
    let mut m: BTreeMap<_, Vec<&Transition>> = BTreeMap::new();
    for t in ts {
        m.entry((t.id.condition, t.id.series)).or_default().push(t);
    }
    m
}

fn judged(c: &Case) -> (Vec<Entry>, Judgement) {
    let h = history(c);
    let j = judge(&h, until(&h)).expect("judge");
    (h, j)
}

proptest! {
    #[test]
    fn ids_are_unique_in_delivery_order_and_each_instance_chains_from_none(c in arb_case()) {
        let (_, j) = judged(&c);
        prop_assert!(j.transitions.windows(2).all(|w| w[0].id < w[1].id));
        for chain in instance_chains(&j.transitions).values() {
            let mut prev: Option<GateState> = None;
            for t in chain {
                prop_assert_eq!(&t.from, &prev, "{:?}", t.id);
                prop_assert_ne!(&t.from, &t.to);
                prev = t.to.clone();
            }
        }
        for ((id, s), (state, set_by)) in &j.states {
            let chain = &instance_chains(&j.transitions)[&(*id, *s)];
            let last = chain.last().expect("an instance starts with a transition");
            prop_assert_eq!((last.to.as_ref(), last.id), (Some(state), *set_by));
        }
    }

    #[test]
    fn levels_cross_upwards_only_above_a_level_and_downwards_only_below_its_clear(c in arb_case()) {
        let (_, j) = judged(&c);
        for t in &j.transitions {
            let (Some(ls), Some(GateState::Level(a)), Some(GateState::Level(b))) =
                (levels_of(&c, t.id.condition), t.from.as_ref(), t.to.as_ref()) else { continue };
            let v = t.value.as_ref().and_then(Value::as_f64).expect("a level has a value");
            let (a, b) = (usize::from(*a), usize::from(*b));
            // Only the lowest boundary crossed is decided at this evaluation; others may have crossed
            // earlier and wait on it (spec §6 Gates, Levels).
            if b > a {
                prop_assert!(v > ls[a].at, "{t:?}");
            } else {
                prop_assert!(v < ls[b].clear, "{t:?}");
            }
        }
    }

    #[test]
    fn a_crossing_with_a_hold_follows_a_value_beyond_its_boundary_throughout_the_hold(c in arb_case()) {
        let (h, j) = judged(&c);
        let ls = levels_of(&c, ConditionId(1)).expect("levels");
        for t in j.transitions.iter().filter(|t| t.id.condition == ConditionId(1)) {
            let (Some(GateState::Level(a)), Some(GateState::Level(b))) = (t.from.as_ref(), t.to.as_ref())
                else { continue };
            let (a, b) = (usize::from(*a), usize::from(*b));
            let (k, hold) = if b > a { (a, ls[a].hold_up) } else { (b, ls[b].hold_down) };
            let at_now = t.value.as_ref().and_then(Value::as_f64).expect("a level has a value");
            for x in change_points(&h, t.now - hold, t.now) {
                let v = if x == t.now { at_now } else { live_at(&h, x).expect("a value throughout the hold") };
                if b > a {
                    prop_assert!(v > ls[k].at, "{t:?} at {x}: {v}");
                } else {
                    prop_assert!(v < ls[k].clear, "{t:?} at {x}: {v}");
                }
            }
        }
    }

    #[test]
    fn stale_is_reported_exactly_while_the_series_is_silent_past_its_limit(c in arb_case()) {
        let (h, j) = judged(&c);
        for t in &j.transitions {
            let Some(lim) = c.gates.iter().find(|(i, _)| *i == t.id.condition).and_then(|(_, g)| g.stale_after)
                else { continue };
            let silent = last_arrival(&h, t.id.series, t.now, t.id.at).is_some_and(|a| t.now - a >= lim);
            match &t.to {
                Some(GateState::Stale) => {
                    prop_assert!(silent, "{t:?}");
                    if let Some(due) = t.id.due {
                        let a = last_arrival(&h, t.id.series, due, t.id.at).expect("arrival");
                        prop_assert!(due == a + lim || t.from.is_none(), "{t:?}");
                    }
                }
                Some(_) => prop_assert!(!silent, "{t:?}"),
                None => {}
            }
        }
    }

    #[test]
    fn each_holds_an_instance_for_every_series_whose_name_matches(c in arb_case()) {
        let (h, j) = judged(&c);
        let mut names: BTreeMap<SeriesId, String> = BTreeMap::new();
        for e in &h {
            if let Entry::Define { series, names: n, .. } = e {
                names.insert(*series, n[0].clone());
            }
        }
        let want: Vec<SeriesId> = names.iter().filter(|(_, n)| pattern_matches(PATTERN, n)).map(|(s, _)| *s).collect();
        let got: Vec<SeriesId> = j.states.keys().filter(|(id, _)| *id == EACH).map(|(_, s)| *s).collect();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn judging_a_prefix_gives_the_same_transitions_as_the_whole_history_up_to_its_horizon(
        c in arb_case(), cut in 0..40usize, wait in 0..30i64,
    ) {
        let (h, all) = judged(&c);
        let k = (c.gates.len() + cut).min(h.len());
        let prefix = &h[..k];
        let horizon = (last_commit(prefix) + wait * SECOND).min(h.get(k).map_or(i64::MAX, Entry::commit_time));
        let part = judge(prefix, horizon).expect("judge");
        let last = prefix.last().map_or(Position::ORIGIN, Entry::position);
        let expected: Vec<&Transition> = all.transitions.iter()
            .filter(|t| t.id.at < last || (t.id.at == last && t.id.due.is_none_or(|d| d <= horizon)))
            .collect();
        prop_assert_eq!(part.transitions.iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn a_restart_resumes_every_transition_once_after_dedup_by_id(
        c in arb_case(), cut in 0..40usize, delivered in 0..60usize, confirmed in 0..60usize,
        pick in 0..64usize,
    ) {
        let (h, all) = judged(&c);
        let k = (c.gates.len() + cut).min(h.len());
        let prefix = &h[..k];
        let horizon = h.get(k).map_or(until(&h), Entry::commit_time);
        let before = judge(prefix, horizon).expect("judge").transitions;
        let d = delivered.min(before.len());
        let n = confirmed.min(d);
        let confirmed_ids: Vec<TransitionId> = before[..n].iter().map(|t| t.id).collect();
        let cursor = before[..n].last().map(|t| GatePoint::Transition(t.id));
        let start = checkpoint(&h, &all.transitions, &confirmed_ids, pick);
        let unbounded = ReplayBound { span: Nanos::MAX, facts: u64::MAX };
        let r = resume(&h, until(&h), start, cursor, unbounded).expect("resume");
        prop_assert!(!r.not_replayed);
        prop_assert!(r.replayed.iter().all(|t| !confirmed_ids.contains(&t.id)));
        prop_assert_eq!(&r.replayed, &by_latches(&all.transitions, &before[..n]));
        let mut seen: Vec<TransitionId> = before[..d].iter().chain(&r.replayed).map(|t| t.id).collect();
        seen.sort();
        seen.dedup();
        prop_assert_eq!(seen, all.transitions.iter().map(|t| t.id).collect::<Vec<_>>());
        prop_assert_eq!(r.states, all.states);
    }
}

/// A checkpoint frontier that covers only confirmed transitions (design G5), drawn by `pick`.
fn checkpoint(
    h: &[Entry],
    all: &[Transition],
    confirmed: &[TransitionId],
    pick: usize,
) -> Frontier {
    let mut cands = vec![Frontier {
        at: Position::ORIGIN,
        now: 0,
    }];
    cands.extend(h.iter().map(|e| Frontier {
        at: e.position(),
        now: e.commit_time(),
    }));
    cands.extend(
        confirmed
            .iter()
            .filter_map(|id| id.due.map(|now| Frontier { at: id.at, now })),
    );
    cands.retain(|f| {
        all.iter()
            .all(|t| !f.covers(&t.id) || confirmed.contains(&t.id))
    });
    cands[pick % cands.len()]
}

/// The replay under per-instance latches (Q-1391): each instance's transitions after the last of its
/// own that was confirmed, all of them for an instance with none confirmed.
fn by_latches(all: &[Transition], confirmed: &[Transition]) -> Vec<Transition> {
    let mut latches: BTreeMap<(ConditionId, SeriesId), TransitionId> = BTreeMap::new();
    for t in confirmed {
        latches.insert((t.id.condition, t.id.series), t.id);
    }
    all.iter()
        .filter(|t| {
            latches
                .get(&(t.id.condition, t.id.series))
                .is_none_or(|l| t.id > *l)
        })
        .cloned()
        .collect()
}

#[test]
fn gate_generator_coverage() {
    use kvik_model::gate::{Cause, TimerKind};
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let mut seen = BTreeMap::new();
    for _ in 0..300 {
        let c = arb_case().new_tree(&mut runner).expect("case").current();
        let (_, j) = judged(&c);
        for t in &j.transitions {
            let key = match (&t.cause, &t.to) {
                (_, None) => "each leaves",
                (
                    Cause::Timer {
                        kind: TimerKind::Hold,
                        ..
                    },
                    _,
                ) => "hold",
                (
                    Cause::Timer {
                        kind: TimerKind::Mature,
                        ..
                    },
                    _,
                ) => "mature",
                (
                    Cause::Timer {
                        kind: TimerKind::Window,
                        ..
                    },
                    _,
                ) => "window tick",
                (_, Some(GateState::Stale)) => "stale",
                (Cause::Fact { .. }, Some(GateState::Level(n))) if *n >= 2 => "level two",
                _ => continue,
            };
            *seen.entry(key).or_insert(0u32) += 1;
        }
    }
    for k in [
        "each leaves",
        "hold",
        "mature",
        "window tick",
        "stale",
        "level two",
    ] {
        assert!(seen.get(k).is_some_and(|n| *n >= 10), "{k}: {seen:?}");
    }
}
