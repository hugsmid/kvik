//! `Each` instances and reset points in the core, against `judge` (spec §6 Gates, One stream; Q-1143, Q-2170).

use kvik_model::gate::{ConditionId, Gate, GateState, Kind, Level, MAX_INSTANCES, Selector, judge};
use kvik_model::types::{Quality, SECOND, SeriesId, Value};

use super::Core;
use super::differential::{Drive, Log, PATTERN, gate, run};

fn each(levels: &[Level]) -> Gate {
    Gate {
        selector: Selector::Each(PATTERN.into()),
        ..gate(Kind::Levels(levels.to_vec()), None)
    }
}

fn one_batch_each() -> Drive {
    Drive {
        cuts: vec![true],
        floors: vec![0],
        restores: vec![false],
    }
}

fn room(i: u32) -> String {
    format!("room/{i}/temp")
}

#[test]
fn a_full_each_refuses_and_counts_a_series_and_takes_it_when_another_leaves() {
    let mut log = Log::default();
    let n = u32::try_from(MAX_INSTANCES).unwrap() + 1;
    for i in 1..=n {
        log.define(0, SeriesId(i), &[&room(i)]);
    }
    log.watch(SECOND, ConditionId(1), each(&[Level::at(10.0)]));
    log.fact(
        SeriesId(n),
        2 * SECOND,
        2 * SECOND,
        Value::Num(12.0),
        Quality::Good,
    );
    log.define(3 * SECOND, SeriesId(1), &["hall/1/temp"]);
    log.define(4 * SECOND, SeriesId(n + 1), &[&room(n + 1)]);
    let until = 10 * SECOND;
    let model = judge(&log.entries, until).unwrap();
    let mut core = Core::new();
    let mut got = Vec::new();
    for (commit, item) in &log.items {
        got.extend(
            core.apply_batch(*commit, std::slice::from_ref(item), &log.replay)
                .unwrap(),
        );
    }
    got.extend(core.advance(until));
    assert_eq!(got, model.transitions);
    assert_eq!(core.states(), model.states);
    // The 257th at the watch, then the newcomer while full again; the 257th joined when series 1 left.
    assert_eq!(model.instances_refused, 2);
    assert_eq!(core.take_refused(), 2);
    let joined = got.iter().find(|t| t.id.series == SeriesId(n)).unwrap();
    assert_eq!(
        (joined.from.clone(), joined.to.clone()),
        (None, Some(GateState::Level(1)))
    );
    assert_eq!(core.instances.len(), MAX_INSTANCES);
}

#[test]
fn a_series_leaving_ends_its_instance_with_a_transition_to_none_and_frees_it() {
    let mut log = Log::default();
    log.define(0, SeriesId(1), &[&room(1)]);
    log.watch(SECOND, ConditionId(1), each(&[Level::at(10.0)]));
    log.fact(
        SeriesId(1),
        2 * SECOND,
        2 * SECOND,
        Value::Num(12.0),
        Quality::Good,
    );
    log.define(3 * SECOND, SeriesId(1), &[]);
    let (got, core) = run(&log, &one_batch_each(), 10 * SECOND);
    let moves: Vec<_> = got.iter().map(|t| (t.from.clone(), t.to.clone())).collect();
    assert_eq!(
        moves,
        [
            (None, Some(GateState::Unknown)),
            (Some(GateState::Unknown), Some(GateState::Level(1))),
            (Some(GateState::Level(1)), None),
        ]
    );
    assert_eq!(got, judge(&log.entries, 10 * SECOND).unwrap().transitions);
    assert!(core.instances.is_empty() && core.series.is_empty() && core.timers.is_empty());
}

#[test]
fn a_reset_reseeds_every_instance_without_a_transition() {
    let mut log = Log::default();
    log.define(0, SeriesId(1), &[&room(1)]);
    log.watch(0, ConditionId(1), each(&[Level::at(10.0).clear(5.0)]));
    log.fact(SeriesId(1), SECOND, SECOND, Value::Num(12.0), Quality::Good);
    log.fact(
        SeriesId(1),
        2 * SECOND,
        2 * SECOND,
        Value::Num(7.0),
        Quality::Good,
    );
    log.reset(3 * SECOND);
    let (got, core) = run(&log, &one_batch_each(), 10 * SECOND);
    let model = judge(&log.entries, 10 * SECOND).unwrap();
    assert_eq!(got, model.transitions);
    assert_eq!(core.states(), model.states);
    // Hysteresis is forgotten: seeded uncrossed at 7, below the level.
    assert_eq!(
        model.states[&(ConditionId(1), SeriesId(1))].0,
        GateState::Level(0)
    );
    assert_eq!(got.last().unwrap().to, Some(GateState::Level(1)));
}
