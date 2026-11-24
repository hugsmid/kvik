//! Gate edges: arrivals and staleness, unknown values, `Each` room, the resume bound and duration limits.

mod gate_common;

use gate_common::*;
use kvik_model::gate::{
    ConditionId, Entry, GateState, Input, Kind, Latches, Level, MAX_DURATION, ReplayBound,
    Selector, Window, judge, resume,
};
use kvik_model::summary::{Aggregate, Params};
use kvik_model::types::{DAY, Position, Quality, SECOND, SeriesId, Value};

fn stale_gate() -> kvik_model::gate::Gate {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(10 * SECOND);
    g
}

#[test]
fn a_reading_without_a_value_does_not_take_a_silent_series_out_of_stale() {
    let h = H::new().watch(0, C, stale_gate()).num(1, 11.0).fact_on(
        S,
        60 * SECOND,
        60 * SECOND,
        Value::Absent,
        Quality::Bad,
    );
    let j = judge(&h.entries, 65 * SECOND).unwrap();
    assert_eq!(j.states[&(C, S)].0, GateState::Stale);
    assert_eq!(j.transitions.last().unwrap().to, Some(GateState::Stale));
}

#[test]
fn a_bad_quality_reading_does_not_take_a_silent_series_out_of_stale() {
    let h = H::new().watch(0, C, stale_gate()).num(1, 11.0).fact_on(
        S,
        60 * SECOND,
        60 * SECOND,
        Value::Num(0.0),
        Quality::Bad,
    );
    let j = judge(&h.entries, 65 * SECOND).unwrap();
    assert_eq!(j.states[&(C, S)].0, GateState::Stale);
}

#[test]
fn a_bad_reading_inside_the_limit_does_not_postpone_staleness() {
    let h = H::new().watch(0, C, stale_gate()).num(1, 11.0).fact_on(
        S,
        8 * SECOND,
        8 * SECOND,
        Value::Num(0.0),
        Quality::Bad,
    );
    let j = judge(&h.entries, 30 * SECOND).unwrap();
    let last = j.transitions.last().unwrap();
    assert_eq!(
        (last.now, last.to.clone()),
        (11 * SECOND, Some(GateState::Stale))
    );
}

#[test]
fn a_good_reading_after_bad_ones_leaves_stale() {
    let h = H::new()
        .watch(0, C, stale_gate())
        .num(1, 11.0)
        .fact_on(S, 60 * SECOND, 60 * SECOND, Value::Absent, Quality::Bad)
        .num(70, 0.0);
    let j = judge(&h.entries, 71 * SECOND).unwrap();
    assert_eq!(j.states[&(C, S)].0, GateState::Level(0));
}

#[test]
fn a_series_with_no_arrival_is_unknown_never_stale() {
    let h = H::new().watch(0, C, stale_gate());
    let j = judge(&h.entries, DAY).unwrap();
    assert_eq!(j.states[&(C, S)].0, GateState::Unknown);
    assert_eq!(j.transitions.len(), 1);
}

#[test]
fn a_value_a_levels_gate_cannot_compare_breaks_a_pending_hold_which_restarts_after_it() {
    let g = levels(&[Level::at(10.0).hold(5 * SECOND, 0)]);
    let h = H::new()
        .watch(0, C, g)
        .num(1, 11.0)
        .fact_on(
            S,
            3 * SECOND,
            3 * SECOND,
            Value::Str("fault".into()),
            Quality::Good,
        )
        .num(4, 11.0);
    let j = judge(&h.entries, 20 * SECOND).unwrap();
    let rise: Vec<i64> = j
        .transitions
        .iter()
        .filter(|t| t.to == lv(1))
        .map(|t| t.now / SECOND)
        .collect();
    assert_eq!(rise, vec![9]);
}

fn each(pattern: &str) -> kvik_model::gate::Gate {
    let mut g = levels(&[Level::at(10.0)]);
    g.selector = Selector::Each(pattern.into());
    g
}

#[test]
fn a_window_falling_below_min_coverage_breaks_a_pending_hold_which_restarts_when_the_value_is_back()
{
    let mut g = levels(&[Level::at(10.0).hold(8 * SECOND, 0)]);
    g.input = Input::Window(Window {
        width: 10 * SECOND,
        aggregate: Aggregate::Sum,
        min_coverage: 500,
        cadence: SECOND,
    });
    let mut h = H::new().define(0, S, &["s"]);
    if let Some(Entry::Define { params, .. }) = h.entries.last_mut() {
        *params = Params::new(kvik_model::types::Kind::Gauge, 2 * SECOND);
    }
    h = h.watch(0, C, g);
    for t in [1, 3, 5, 20, 22, 24, 26, 28, 30, 32, 34, 36] {
        h = h.num(t, 6.0);
    }
    let j = judge(&h.entries, 60 * SECOND).unwrap();
    let at = |to: Option<GateState>| -> Vec<i64> {
        j.transitions
            .iter()
            .filter(|t| t.to == to)
            .map(|t| t.now / SECOND)
            .collect()
    };
    assert_eq!(at(Some(GateState::Unknown)), vec![0, 13, 44]);
    assert_eq!(at(lv(1)), vec![33]);
}

#[test]
fn a_series_refused_by_a_full_each_joins_with_a_start_transition_when_room_frees() {
    let mut h = H::new().watch(0, C, each("room/*/temp"));
    for i in 0..257u32 {
        h = h.define(1, SeriesId(i + 1), &[&format!("room/r{i}/temp")]);
    }
    h = h.define(2, SeriesId(1), &["store/r0/temp"]);
    let join_at = h.last();
    let j = judge(&h.entries, 3 * SECOND).unwrap();
    assert_eq!(j.instances_refused, 1);
    assert_eq!(j.states.len(), 256);
    assert!(!j.states.contains_key(&(C, SeriesId(1))));
    let late = SeriesId(257);
    let first = j
        .transitions
        .iter()
        .find(|t| t.id.series == late)
        .expect("joined");
    assert_eq!(
        (first.from.clone(), first.to.clone()),
        (None, Some(GateState::Unknown))
    );
    assert_eq!(first.id.at, join_at);
}

fn quiet_and_busy() -> (H, Position, Latches) {
    let (quiet, busy) = (ConditionId(1), ConditionId(2));
    let mut h = H::new()
        .watch(0, quiet, levels(&[Level::at(1_000.0)]))
        .watch(0, busy, levels(&[Level::at(10.0)]))
        .num(1, 0.0);
    let base = 2 * DAY / SECOND;
    for k in 0..30 {
        h = h.num(base + 60 * (k + 1), if k % 2 == 0 { 11.0 } else { 0.0 });
    }
    let all = judge(&h.entries, 3 * DAY).unwrap().transitions;
    let cut = Position::new(0, 23);
    let mut latches = Latches::new();
    for t in all.iter().filter(|t| t.id.at <= cut) {
        latches.insert((t.id.condition, t.id.series), t.id);
    }
    (h, cut, latches)
}

#[test]
fn the_replay_bound_runs_from_the_cursor_not_from_a_quiet_instances_old_latch() {
    let (h, cut, latches) = quiet_and_busy();
    let r = resume(&h.entries, 3 * DAY, cut, &latches, ReplayBound::default()).unwrap();
    assert!(!r.not_replayed);
    assert!(!r.replayed.is_empty());
}

#[test]
fn the_replay_bound_still_refuses_a_cursor_older_than_the_span() {
    let (h, cut, latches) = quiet_and_busy();
    let bound = ReplayBound {
        span: 10 * 60 * SECOND,
        facts: 1_000_000,
    };
    let r = resume(&h.entries, 3 * DAY, cut, &latches, bound).unwrap();
    assert!(r.not_replayed && r.replayed.is_empty());
}

#[test]
fn the_facts_bound_admits_exactly_that_many_facts_after_the_cursor() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 11.0)
        .num(2, 12.0)
        .num(20, 0.0)
        .num(30, 11.0);
    let start = judge(&h.entries, 100 * SECOND).unwrap().transitions[0].id;
    let latches: Latches = [((C, S), start)].into();
    let at = |facts| ReplayBound {
        span: 1_000 * SECOND,
        facts,
    };
    let ok = resume(&h.entries, 100 * SECOND, Position::ORIGIN, &latches, at(4)).unwrap();
    assert!(!ok.not_replayed);
    let over = resume(&h.entries, 100 * SECOND, Position::ORIGIN, &latches, at(3)).unwrap();
    assert!(over.not_replayed);
}

#[test]
fn durations_beyond_the_maximum_are_refused_and_the_maximum_is_accepted() {
    let window = |width, cadence| {
        let mut g = levels(&[Level::at(1.0)]);
        g.input = Input::Window(Window {
            width,
            aggregate: Aggregate::Max,
            min_coverage: 500,
            cadence,
        });
        g
    };
    let mut stale = levels(&[Level::at(1.0)]);
    stale.stale_after = Some(MAX_DURATION + 1);
    let bad = [
        stale,
        levels(&[Level::at(1.0).hold(MAX_DURATION + 1, 0)]),
        levels(&[Level::at(1.0).hold(0, i64::MAX)]),
        gate(Kind::Set {
            members: vec![Value::Num(0.0)],
            hold_on: MAX_DURATION + 1,
            hold_off: 0,
        }),
        gate(Kind::Set {
            members: vec![Value::Num(0.0)],
            hold_on: 0,
            hold_off: i64::MAX,
        }),
        window(MAX_DURATION + 1, SECOND),
        window(SECOND, MAX_DURATION + 1),
    ];
    for g in bad {
        assert_eq!(
            g.validate(),
            Err(kvik_model::gate::GateError::Duration),
            "{g:?}"
        );
    }
    let mut edge = levels(&[Level::at(1.0).hold(MAX_DURATION, MAX_DURATION)]);
    edge.stale_after = Some(MAX_DURATION);
    assert!(edge.validate().is_ok());
    assert!(window(MAX_DURATION, MAX_DURATION).validate().is_ok());
}

#[test]
fn the_longest_durations_late_on_the_commit_clock_judge_without_overflow() {
    let late = i64::MAX - SECOND;
    let mut g = levels(&[Level::at(10.0).hold(MAX_DURATION, MAX_DURATION)]);
    g.stale_after = Some(MAX_DURATION);
    let h = H::new()
        .watch(0, C, g)
        .fact_on(S, late, late, Value::Num(11.0), Quality::Good)
        .fact_on(S, late + 1, late + 1, Value::Num(0.0), Quality::Good);
    let j = judge(&h.entries, i64::MAX).unwrap();
    assert_eq!(j.states[&(C, S)].0, GateState::Level(0));
}
