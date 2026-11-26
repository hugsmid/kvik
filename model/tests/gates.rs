//! Gates on one stream (spec §6 Gates): levels, sets, change, staleness, windows, `Each`, identities and
//! resume, each test pinning one rule of the model's judgement.

mod gate_common;

use gate_common::*;
use kvik_model::gate::{
    Cause, ConditionId, Entry, GateError, GateState, Input, Kind, Latches, Level, ReplayBound,
    Selector, TimerKind, TransitionId, Window, judge, resume,
};
use kvik_model::summary::Aggregate;
use kvik_model::types::{Position, Quality, SECOND, SeriesId, Value};

const END: i64 = 10_000;

fn run(h: &H) -> kvik_model::gate::Judgement {
    judge(&h.entries, END * SECOND).expect("judge")
}

#[test]
fn levels_one_boundary_rises_above_at_and_falls_below_clear() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).clear(8.0)]))
        .num(1, 5.0)
        .num(2, 11.0)
        .num(3, 9.0)
        .num(4, 7.0);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions),
        vec![
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(0)),
            (lv(0), lv(1)),
            (lv(1), lv(0)),
        ]
    );
    assert_eq!(instants(&j.transitions), vec![0, 1, 2, 4]);
}

#[test]
fn levels_no_flip_at_the_level_or_the_clear_value_itself() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).clear(8.0)]))
        .num(1, 10.0)
        .num(2, 10.5)
        .num(3, 8.0)
        .num(4, 7.9);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 2, 4]);
}

#[test]
fn levels_jump_across_several_boundaries_is_one_transition_each_way() {
    let ls = [Level::at(1.0), Level::at(2.0), Level::at(3.0)];
    let h = H::new()
        .watch(0, C, levels(&ls))
        .num(1, 0.0)
        .num(2, 3.5)
        .num(3, 0.5);
    let j = run(&h);
    assert_eq!(moves(&j.transitions)[2..], [(lv(0), lv(3)), (lv(3), lv(0))]);
}

#[test]
fn levels_hold_up_rises_only_after_the_value_stayed_above_for_the_hold() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 0.0)
        .num(2, 11.0)
        .num(4, 12.0);
    let j = run(&h);
    let up = j.transitions.last().unwrap();
    assert_eq!((up.from.clone(), up.to.clone()), (lv(0), lv(1)));
    assert_eq!(up.now, 7 * SECOND);
    assert_eq!(
        up.cause,
        Cause::Timer {
            due: 7 * SECOND,
            kind: TimerKind::Hold
        }
    );
}

#[test]
fn levels_hold_up_broken_by_a_dip_starts_again_at_the_next_rise() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 0.0)
        .num(2, 11.0)
        .num(5, 9.0)
        .num(6, 11.0);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 11]);
}

#[test]
fn levels_hold_down_falls_only_after_the_value_stayed_below_clear() {
    let h = H::new()
        .watch(
            0,
            C,
            levels(&[Level::at(10.0).clear(8.0).hold(0, 4 * SECOND)]),
        )
        .num(1, 11.0)
        .num(2, 7.0)
        .num(3, 9.0)
        .num(4, 7.0);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions)[1..],
        [(Some(GateState::Unknown), lv(1)), (lv(1), lv(0))]
    );
    assert_eq!(j.transitions.last().unwrap().now, 8 * SECOND);
}

#[test]
fn levels_a_lower_boundary_still_held_keeps_the_level_then_both_cross_as_one() {
    let ls = [
        Level::at(1.0).hold(10 * SECOND, 0),
        Level::at(2.0).hold(2 * SECOND, 0),
    ];
    let h = H::new().watch(0, C, levels(&ls)).num(1, 0.0).num(2, 5.0);
    let j = run(&h);
    assert_eq!(moves(&j.transitions)[2..], [(lv(0), lv(2))]);
    assert_eq!(j.transitions.last().unwrap().now, 12 * SECOND);
}

#[test]
fn a_timer_transition_is_identified_by_the_last_position_before_it_and_its_due_instant() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 11.0)
        .num(3, 12.0);
    let last = h.last();
    let j = run(&h);
    assert_eq!(
        j.transitions.last().unwrap().id,
        TransitionId {
            at: last,
            due: Some(6 * SECOND),
            condition: C,
            series: S
        }
    );
}

#[test]
fn set_matches_after_hold_on_and_leaves_after_hold_off() {
    let g = gate(Kind::Set {
        members: vec![Value::Num(3.0), Value::Num(4.0)],
        hold_on: 2 * SECOND,
        hold_off: 3 * SECOND,
    });
    let h = H::new()
        .watch(0, C, g)
        .num(1, 0.0)
        .num(2, 3.0)
        .num(3, 4.0)
        .num(10, 0.0)
        .num(12, 0.0);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions)[1..],
        [
            (Some(GateState::Unknown), Some(GateState::Match(false))),
            (Some(GateState::Match(false)), Some(GateState::Match(true))),
            (Some(GateState::Match(true)), Some(GateState::Match(false))),
        ]
    );
    assert_eq!(instants(&j.transitions), vec![0, 1, 4, 13]);
}

#[test]
fn change_moves_beyond_the_deadband_from_the_last_state_and_on_any_code_change() {
    let h = H::new()
        .watch(0, C, gate(Kind::Change { deadband: 1.0 }))
        .num(1, 5.0)
        .num(2, 5.9)
        .num(3, 6.0)
        .num(4, 6.1);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 4]);
    let codes = H::new()
        .watch(0, C, gate(Kind::Change { deadband: 0.0 }))
        .fact_on(S, SECOND, SECOND, Value::Str("idle".into()), Quality::Good)
        .fact_on(
            S,
            2 * SECOND,
            2 * SECOND,
            Value::Str("idle".into()),
            Quality::Good,
        )
        .fact_on(
            S,
            3 * SECOND,
            3 * SECOND,
            Value::Str("run".into()),
            Quality::Good,
        );
    assert_eq!(instants(&run(&codes).transitions), vec![0, 1, 3]);
}

#[test]
fn stale_after_silence_on_a_timer_and_left_at_the_next_arrival() {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(30 * SECOND);
    let h = H::new().watch(0, C, g).num(1, 11.0).num(100, 11.0);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions)[2..4],
        [
            (lv(1), Some(GateState::Stale)),
            (Some(GateState::Stale), lv(1))
        ]
    );
    assert_eq!(instants(&j.transitions), vec![0, 1, 31, 100, 130]);
    assert!(matches!(
        j.transitions[2].cause,
        Cause::Timer {
            kind: TimerKind::Stale,
            ..
        }
    ));
}

#[test]
fn stale_breaks_a_pending_hold_which_starts_again_after_the_arrival() {
    let mut g = levels(&[Level::at(10.0).hold(50 * SECOND, 0)]);
    g.stale_after = Some(30 * SECOND);
    let h = H::new()
        .watch(0, C, g)
        .num(1, 11.0)
        .num(40, 11.0)
        .num(60, 11.0)
        .num(80, 11.0)
        .num(100, 11.0);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 31, 40, 90, 130]);
    assert_eq!(j.transitions[4].to, lv(1));
}

#[test]
fn a_gate_watched_after_its_series_has_data_starts_from_none_at_its_state_now() {
    let h = H::new()
        .num(1, 25.0)
        .watch(2, C, levels(&[Level::at(10.0), Level::at(20.0)]));
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
fn a_hold_counts_from_the_watch_not_from_before_it() {
    let h = H::new()
        .num(1, 11.0)
        .watch(100, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]));
    let j = run(&h);
    assert_eq!(moves(&j.transitions), vec![(None, lv(0)), (lv(0), lv(1))]);
    assert_eq!(instants(&j.transitions), vec![100, 105]);
}

#[test]
fn state_now_is_the_last_transitions_state_with_its_id() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0)]))
        .num(1, 11.0)
        .num(2, 12.0);
    let j = run(&h);
    let last = j.transitions.last().unwrap();
    assert_eq!(j.states[&(C, S)], (GateState::Level(1), last.id));
}

#[test]
fn a_future_fact_takes_effect_at_its_event_time_on_a_timer() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0)]))
        .num(1, 0.0)
        .fact_on(S, 2 * SECOND, 9 * SECOND, Value::Num(11.0), Quality::Good);
    let j = run(&h);
    let up = j.transitions.last().unwrap();
    assert_eq!(up.now, 9 * SECOND);
    assert!(matches!(
        up.cause,
        Cause::Timer {
            kind: TimerKind::Mature,
            ..
        }
    ));
}

#[test]
fn a_future_fact_refreshes_staleness_only_once_it_takes_effect() {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(10 * SECOND);
    let h = H::new().watch(0, C, g).num(1, 0.0).fact_on(
        S,
        2 * SECOND,
        30 * SECOND,
        Value::Num(0.0),
        Quality::Good,
    );
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 11, 30, 40]);
}

/// Q-1200: a future fact outranked before its `t` by a fact that is not an arrival is not an arrival at `t`.
#[test]
fn a_future_fact_outranked_before_its_t_does_not_take_a_silent_series_out_of_stale() {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(10 * SECOND);
    let mut future = fact(
        S,
        Position::ORIGIN,
        2 * SECOND,
        30 * SECOND,
        Value::Num(0.0),
        Quality::Good,
    );
    future.changed_at = Some(1);
    let mut retract = future.clone();
    retract.changed_at = Some(2);
    retract.value = Value::Absent;
    retract.quality = Quality::Bad;
    let h = H::new()
        .watch(0, C, g)
        .num(1, 0.0)
        .push_fact(2 * SECOND, future)
        .push_fact(3 * SECOND, retract);
    let j = run(&h);
    assert!(!j.transitions.iter().any(|t| matches!(
        t.cause,
        Cause::Timer {
            kind: TimerKind::Mature,
            ..
        }
    )));
    assert_eq!(instants(&j.transitions), vec![0, 1, 11]);
    assert_eq!(j.transitions.last().unwrap().to, Some(GateState::Stale));
}

#[test]
fn a_bad_quality_fact_neither_moves_the_value_nor_refreshes_staleness() {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(10 * SECOND);
    let h = H::new().watch(0, C, g).num(1, 11.0).fact_on(
        S,
        8 * SECOND,
        8 * SECOND,
        Value::Num(0.0),
        Quality::Bad,
    );
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 11]);
    assert_eq!(j.transitions.last().unwrap().to, Some(GateState::Stale));
}

#[test]
fn an_outranked_fact_neither_moves_the_value_nor_refreshes_staleness() {
    let mut g = levels(&[Level::at(10.0)]);
    g.stale_after = Some(10 * SECOND);
    let mut newer = fact(
        S,
        Position::ORIGIN,
        SECOND,
        SECOND,
        Value::Num(11.0),
        Quality::Good,
    );
    newer.changed_at = Some(5);
    let mut older = newer.clone();
    older.changed_at = Some(1);
    older.value = Value::Num(0.0);
    let h = H::new()
        .watch(0, C, g)
        .push_fact(SECOND, newer)
        .push_fact(8 * SECOND, older);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1, 11]);
}

#[test]
fn unknown_until_the_first_reading() {
    let h = H::new().watch(0, C, levels(&[Level::at(10.0)])).num(5, 1.0);
    let j = run(&h);
    assert_eq!(
        moves(&j.transitions),
        vec![
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(0))
        ]
    );
}

fn window_gate(width: i64, min_coverage: u16) -> kvik_model::gate::Gate {
    let mut g = levels(&[Level::at(10.0).clear(5.0)]);
    g.input = Input::Window(Window {
        width: width * SECOND,
        aggregate: Aggregate::Sum,
        min_coverage,
        cadence: SECOND,
    });
    g
}

#[test]
fn a_window_sum_rises_on_the_tick_its_readings_enter_and_falls_on_the_tick_they_leave() {
    let h = H::new()
        .watch(0, C, window_gate(10, 0))
        .num(1, 6.0)
        .num(2, 6.0)
        .num(5, 1.0);
    let j = run(&h);
    let up = j.transitions.iter().position(|t| t.to == lv(1)).unwrap();
    // A reading at `now` is outside `[now − width, now)` until the next tick (spec §6 Windows).
    assert_eq!(j.transitions[up].now, 3 * SECOND);
    let down = &j.transitions[up + 1];
    assert_eq!(down.to, lv(0));
    assert_eq!(down.now, 13 * SECOND);
    assert!(matches!(
        down.cause,
        Cause::Timer {
            kind: TimerKind::Window,
            ..
        }
    ));
}

#[test]
fn a_window_below_its_min_coverage_is_unknown_and_known_once_covered() {
    let h = H::new().watch(0, C, window_gate(10, 500)).num(1, 1.0);
    let j = run(&h);
    let known = j.transitions.iter().find(|t| t.to == lv(0)).expect("known");
    assert_eq!(known.now, 6 * SECOND);
    assert_eq!(known.from, Some(GateState::Unknown));
}

#[test]
fn a_window_gate_empty_and_past_stale_after_is_stale_not_unknown() {
    let mut g = window_gate(5, 0);
    g.stale_after = Some(10 * SECOND);
    let h = H::new().watch(0, C, g).num(1, 1.0);
    let j = run(&h);
    let last = j.transitions.last().unwrap();
    assert_eq!(
        (last.now, last.to.clone()),
        (11 * SECOND, Some(GateState::Stale))
    );
    assert!(
        j.transitions
            .iter()
            .any(|t| t.to == Some(GateState::Unknown) && t.now == 7 * SECOND)
    );
}

fn each(pattern: &str) -> kvik_model::gate::Gate {
    let mut g = levels(&[Level::at(10.0)]);
    g.selector = Selector::Each(pattern.into());
    g
}

#[test]
fn each_joins_a_series_whose_name_matches_and_leaves_when_it_no_longer_does() {
    let (a, b) = (SeriesId(1), SeriesId(2));
    let h = H::new()
        .define(0, a, &["room/a/temp"])
        .watch(1, C, each("room/*/temp"))
        .num_on(a, 2, 11.0)
        .define(3, b, &["room/b/temp"])
        .num_on(b, 4, 12.0)
        .define(5, a, &["store/a/temp"]);
    let j = run(&h);
    let of = |s| {
        j.transitions
            .iter()
            .filter(|t| t.id.series == s)
            .map(|t| (t.from.clone(), t.to.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        of(a),
        vec![
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(1)),
            (lv(1), None)
        ]
    );
    assert_eq!(
        of(b),
        vec![
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), lv(1))
        ]
    );
    assert_eq!(j.states.keys().copied().collect::<Vec<_>>(), vec![(C, b)]);
}

#[test]
fn each_takes_at_most_its_instance_limit_and_counts_the_series_refused() {
    let mut h = H::new().watch(0, C, each("room/*/temp"));
    for i in 0..258u32 {
        h = h.define(1, SeriesId(i + 1), &[&format!("room/r{i}/temp")]);
    }
    let j = run(&h);
    assert_eq!(j.states.len(), 256);
    assert_eq!(j.instances_refused, 2);
}

#[test]
fn unwatch_ends_a_gate_with_no_transition_and_no_state() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0)]))
        .num(1, 1.0)
        .unwatch(2, C)
        .num(3, 11.0);
    let j = run(&h);
    assert_eq!(instants(&j.transitions), vec![0, 1]);
    assert!(j.states.is_empty());
}

#[test]
fn a_timer_due_at_an_entrys_commit_time_fires_before_that_entry() {
    let h = H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 11.0)
        .num(6, 0.0);
    let j = run(&h);
    assert_eq!(moves(&j.transitions)[2..], [(lv(0), lv(1)), (lv(1), lv(0))]);
    assert_eq!(j.transitions[2].id.due, Some(6 * SECOND));
    assert_eq!(j.transitions[3].id.due, None);
}

#[test]
fn a_history_out_of_order_or_reusing_an_id_is_refused() {
    let mut h = H::new().watch(5, C, levels(&[Level::at(1.0)])).num(6, 1.0);
    h.entries.swap(0, 1);
    assert!(matches!(judge(&h.entries, 0), Err(GateError::Order(_))));
    let twice =
        H::new()
            .watch(0, C, levels(&[Level::at(1.0)]))
            .watch(1, C, levels(&[Level::at(1.0)]));
    assert_eq!(judge(&twice.entries, 0), Err(GateError::Id(C)));
    let back = H::new().num(5, 1.0).num(4, 1.0);
    assert!(matches!(judge(&back.entries, 0), Err(GateError::Order(_))));
}

#[test]
fn gates_with_invalid_parameters_are_refused() {
    let bad = [
        levels(&[]),
        levels(&[Level::at(2.0), Level::at(1.0)]),
        levels(&[Level::at(1.0).clear(2.0)]),
        levels(&[Level::at(1.0).clear(0.5), Level::at(2.0).clear(0.4)]),
        levels(&[Level::at(f64::NAN)]),
        levels(&[Level::at(1.0).hold(-1, 0)]),
        gate(Kind::Set {
            members: vec![],
            hold_on: 0,
            hold_off: 0,
        }),
        gate(Kind::Change { deadband: -1.0 }),
        each("room/r*/temp"),
        window_gate(0, 0),
        window_gate(1, 1_001),
    ];
    for g in bad {
        assert!(g.validate().is_err(), "{g:?}");
    }
    assert!(
        levels(&[Level::at(1.0).clear(0.5), Level::at(2.0).clear(0.5)])
            .validate()
            .is_ok()
    );
}

fn latches(entries: &[(SeriesId, TransitionId)]) -> Latches {
    entries.iter().map(|(s, id)| ((C, *s), *id)).collect()
}

fn hold_history() -> H {
    H::new()
        .watch(0, C, levels(&[Level::at(10.0).hold(5 * SECOND, 0)]))
        .num(1, 11.0)
        .num(2, 12.0)
        .num(20, 0.0)
        .num(30, 11.0)
}

#[test]
fn resume_replays_each_transition_after_the_latch_under_the_same_id() {
    let h = hold_history();
    let all = run(&h).transitions;
    let latch = latches(&[(S, all[1].id)]);
    let r = resume(
        &h.entries,
        END * SECOND,
        Position::ORIGIN,
        &latch,
        ReplayBound::default(),
    )
    .unwrap();
    assert_eq!(r.replayed, all[2..].to_vec());
    assert!(!r.not_replayed);
    let last = all.last().unwrap();
    assert_eq!(r.states[&(C, S)], (last.to.clone().unwrap(), last.id));
}

#[test]
fn resume_with_no_latch_replays_from_the_cursor() {
    let h = hold_history();
    let all = run(&h).transitions;
    let r = resume(
        &h.entries,
        END * SECOND,
        Position::new(0, 3),
        &Latches::new(),
        ReplayBound::default(),
    )
    .unwrap();
    assert_eq!(
        r.replayed.first().map(|t| t.id.at),
        Some(Position::new(0, 3))
    );
    assert_eq!(
        r.replayed,
        all.into_iter()
            .filter(|t| t.id.at >= Position::new(0, 3))
            .collect::<Vec<_>>()
    );
}

#[test]
fn resume_beyond_the_replay_bound_replays_nothing_and_carries_state_now() {
    let h = hold_history();
    let latch = latches(&[(S, run(&h).transitions[0].id)]);
    let by_facts = ReplayBound {
        span: END * SECOND,
        facts: 3,
    };
    let r = resume(&h.entries, END * SECOND, Position::ORIGIN, &latch, by_facts).unwrap();
    assert!(r.not_replayed && r.replayed.is_empty());
    assert_eq!(r.states[&(C, S)].0, GateState::Level(1));
    let by_span = ReplayBound {
        span: 60 * SECOND,
        facts: 100,
    };
    let r = resume(&h.entries, 61 * SECOND, Position::ORIGIN, &latch, by_span).unwrap();
    assert!(r.not_replayed);
    let r = resume(&h.entries, 60 * SECOND, Position::ORIGIN, &latch, by_span).unwrap();
    assert!(!r.not_replayed && !r.replayed.is_empty());
}

#[test]
fn a_restart_delivers_every_transition_once_after_dedup_with_unconfirmed_ones_again_under_their_ids()
 {
    let h = hold_history();
    let split = 3;
    let prefix = &h.entries[..split];
    let before = judge(prefix, 15 * SECOND).unwrap().transitions;
    let delivered = &before[..3];
    let latch = latches(&[(S, before[1].id)]);
    let r = resume(
        &h.entries,
        END * SECOND,
        Position::ORIGIN,
        &latch,
        ReplayBound::default(),
    )
    .unwrap();
    assert_eq!(r.replayed[0].id, delivered[2].id);
    let mut seen: Vec<TransitionId> = delivered.iter().chain(&r.replayed).map(|t| t.id).collect();
    seen.sort();
    seen.dedup();
    let all: Vec<TransitionId> = run(&h).transitions.iter().map(|t| t.id).collect();
    assert_eq!(seen, all);
}

#[test]
fn a_gate_on_a_series_with_no_definition_reads_its_facts_by_id() {
    let h = H::new()
        .watch(0, ConditionId(1), levels(&[Level::at(1.0)]))
        .num(1, 2.0);
    assert_eq!(run(&h).transitions.last().unwrap().to, lv(1));
    assert!(matches!(h.entries[1], Entry::Fact { .. }));
}
