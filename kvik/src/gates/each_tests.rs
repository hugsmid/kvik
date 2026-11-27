//! `Each` instances and reset points in the core, against `judge` (spec §6 Gates, One stream; Q-1143, Q-2170).

use kvik_model::gate::{
    ConditionId, Gate, GateState, Kind, Level, MAX_EACH_INSTANCES, MAX_INSTANCES, Selector, judge,
};
use kvik_model::types::{Quality, SECOND, SeriesId, Value};

use super::differential::S;
use super::differential::{Drive, Log, PATTERN, gate, run};
use super::{Body, Core, FactItem, Item};

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
    // Still matching while refused: not counted again.
    log.define(5 * SECOND, SeriesId(n + 1), &[&room(n + 1), "room/x/temp"]);
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
fn a_consumer_full_of_each_instances_refuses_and_takes_a_series_when_another_gate_frees_room() {
    let mut log = Log::default();
    let per = u32::try_from(MAX_INSTANCES).unwrap();
    let full = u32::try_from(MAX_EACH_INSTANCES / MAX_INSTANCES).unwrap();
    let name = |g: u32, i: u32| format!("b{g}/{i}/t");
    let series = |g: u32, i: u32| SeriesId(g * per + i + 1);
    for g in 0..full {
        for i in 0..per {
            log.define(0, series(g, i), &[&name(g, i)]);
        }
    }
    log.define(0, series(full, 0), &[&name(full, 0)]);
    log.define(0, series(full, 1), &[&name(full, 1)]);
    let pattern = |g: u32| Gate {
        selector: Selector::Each(format!("b{g}/*/t")),
        ..each(&[Level::at(10.0)])
    };
    for g in 0..=full {
        log.watch(SECOND, ConditionId(g + 1), pattern(g));
    }
    // Single-series gates hold instances that the bound does not count.
    for k in 0..2 {
        log.watch(
            SECOND,
            ConditionId(full + 2 + k),
            Gate {
                selector: Selector::Series(series(0, k + 1)),
                ..each(&[Level::at(10.0)])
            },
        );
    }
    // Gate 1 loses a series and the last gate, below its own cap, takes one of its two in the same definition.
    log.define(2 * SECOND, series(0, 0), &["elsewhere"]);
    let until = 3 * SECOND;
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
    assert_eq!(model.instances_refused, 2);
    assert_eq!(core.take_refused(), 2);
    assert_eq!(core.instances.len(), MAX_EACH_INSTANCES + 2);
    let last = ConditionId(full + 1);
    assert_eq!(
        model.states.keys().filter(|k| k.0 == last).count(),
        1,
        "one of the last gate's series joined"
    );
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

#[test]
fn a_reset_reads_the_series_again_rather_than_keep_a_fact_the_log_no_longer_holds() {
    let mut log = Log::default();
    log.watch(
        0,
        ConditionId(1),
        gate(Kind::Levels(vec![Level::at(10.0)]), None),
    );
    log.fact(S, SECOND, SECOND, Value::Num(12.0), Quality::Good);
    // A fact the core judged that the log has since lost: in the items, not in the history.
    let lost = Item {
        position: log.next(),
        body: Body::Fact {
            fact: FactItem {
                series: S,
                t: 2 * SECOND,
                value: Value::Num(5.0),
                quality: Quality::Good,
                changed_at: None,
            },
            outranked: false,
        },
    };
    log.items.push((2 * SECOND, lost));
    log.reset(3 * SECOND);
    let (_, core) = run(&log, &one_batch_each(), 10 * SECOND);
    assert_eq!(core.states()[&(ConditionId(1), S)].0, GateState::Level(1));
}

#[test]
fn the_largest_checkpoint_a_consumer_can_hold_fits_one_block() {
    use kvik_log::block::MAX_BODY_LEN;
    use kvik_log::cursor::{
        Checkpoint, CheckpointFact, CheckpointInstance, CheckpointSeries, CheckpointState,
        CheckpointValue, GateId, encode_checkpoint,
    };
    use kvik_log::writer::Position;

    // Every field at its widest, though no gate kind holds both pending holds and a reference: about 1,009 bytes an
    // instance with its series (Q-2163).
    let n = MAX_EACH_INSTANCES + crate::MAX_GATES_PER_CONSUMER;
    let pos = Position {
        generation: u32::MAX,
        sequence: u64::MAX,
    };
    let text = CheckpointValue::Text("x".repeat(255));
    let id = |k: u32| GateId {
        at: pos,
        due: Some(i64::MAX),
        gate: k,
        series: k,
    };
    let c = Checkpoint {
        through: pos,
        now: i64::MAX,
        instances: (0..u32::try_from(n).unwrap())
            .map(|k| CheckpointInstance {
                gate: k,
                series: k,
                state: CheckpointState::Value(text.clone()),
                set_by: id(k),
                reseeded: true,
                crossed: u16::MAX,
                pending: (0..16).map(|b| (b, i64::MAX)).collect(),
                reference: Some(text.clone()),
                last_eval: i64::MAX,
            })
            .collect(),
        series: (0..u32::try_from(n).unwrap())
            .map(|k| CheckpointSeries {
                series: k,
                live: Some(CheckpointFact {
                    t: i64::MAX,
                    position: pos,
                    value: text.clone(),
                    usable: true,
                    changed_at: Some(u64::MAX),
                }),
                last_arrival: Some(i64::MAX),
            })
            .collect(),
    };
    let body = encode_checkpoint(&c).expect("representable");
    assert!(
        body.len() <= MAX_BODY_LEN as usize,
        "{} bytes over {MAX_BODY_LEN}",
        body.len()
    );
}
