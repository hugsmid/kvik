//! Tests of specification rules not covered by an edge-case row, named after
//! the section they check.

mod common;

use common::*;
use kvik_model::engine::{Config, Model};
use kvik_model::oracle;
use kvik_model::summary::{AggValue, Aggregate, Body, Params, direct};
use kvik_model::tree::SeriesTree;
use kvik_model::types::{
    HOUR, Kind, MINUTE, Nanos, Quality, Reading, SECOND, SeriesId, Value, ValueType, exact,
};

fn model(staleness: Nanos) -> (Model, SeriesId) {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            staleness,
        )
        .unwrap();
    (m, s)
}

fn n(v: f64) -> Value {
    Value::Num(v)
}

#[test]
fn s2_2_late_flag() {
    let (mut m, s) = model(10 * SECOND); // 2 × S = 20 s
    let now = m.wall;
    m.ingest(vec![Reading::num(s, now, 1.0)]).unwrap();
    m.ingest(vec![Reading::num(s, now - 15 * SECOND, 1.0)])
        .unwrap();
    m.ingest(vec![Reading::num(s, now - 25 * SECOND, 1.0)])
        .unwrap();
    let late: Vec<bool> = m.facts().unwrap().iter().map(|f| f.flags.late).collect();
    assert_eq!(
        late,
        vec![false, false, true],
        "out of order by more than 2 × S"
    );
    // Older than the lateness bound (5 minutes) is late even in order.
    let (mut m, s) = model(10 * SECOND);
    let now = m.wall;
    m.ingest(vec![Reading::num(s, now - 6 * MINUTE, 1.0)])
        .unwrap();
    assert!(m.facts().unwrap()[0].flags.late);
}

#[test]
fn s2_4_duplicates_within_one_batch() {
    let (mut m, s) = model(10 * SECOND);
    let r = m
        .ingest(vec![Reading::num(s, T0, 1.0), Reading::num(s, T0, 1.0)])
        .unwrap();
    assert_eq!((r.positions.len(), r.duplicates), (1, 1));
}

#[test]
fn s2_4_values_compare_bitwise() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 0.0)]).unwrap();
    let r = m.ingest(vec![Reading::num(s, T0, -0.0)]).unwrap();
    assert_eq!(r.positions.len(), 1, "-0.0 is a different value");
}

#[test]
fn s4_future_fact_in_tree_not_in_windows_or_live_state() {
    let (mut m, s) = model(10 * SECOND);
    let t = m.wall + 2 * MINUTE;
    m.ingest(vec![Reading::num(s, t, 5.0)]).unwrap();
    let minute = t - t.rem_euclid(MINUTE);
    assert_eq!(m.tree(s).unwrap().node(0, minute).count, 1);
    assert_eq!(m.window(s, t - MINUTE, t + MINUTE).unwrap().count, 0);
    assert!(m.latest(s).unwrap().is_none());
    m.advance(3 * MINUTE);
    assert_eq!(m.window(s, t - MINUTE, t + MINUTE).unwrap().count, 1);
    assert!(m.latest(s).unwrap().is_some());
}

#[test]
fn s2_5_bad_reading_does_not_end_hold() {
    // Open decision D6, default: the previous value's hold continues up to S.
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![
        Reading::num(s, T0, 10.0),
        Reading {
            quality: Quality::Bad,
            ..Reading::num(s, T0 + 2 * SECOND, 99.0)
        },
    ])
    .unwrap();
    let w = m.window(s, T0, T0 + MINUTE).unwrap();
    assert_eq!(w.coverage, i128::from(10 * SECOND));
    assert_eq!(w.count, 1);
}

#[test]
fn s5_2_state_transition_across_stale_gap() {
    let p = Params::new(Kind::State, SECOND);
    let rs: Vec<(Nanos, Value)> = vec![
        (T0, Value::Str("running".into())),
        (T0 + HOUR, Value::Str("stopped".into())),
    ];
    let node = direct(&p, T0, T0 + 2 * HOUR, &rs);
    let Body::State { transitions, .. } = node.body else {
        unreachable!()
    };
    assert_eq!(transitions, 1);
}

#[test]
fn s5_5_counter_steps_never_capped() {
    let rs = vec![(T0, n(100.0)), (T0 + HOUR, n(250.0))];
    let p = Params::new(Kind::Counter, MINUTE);
    let node = direct(&p, T0, T0 + 2 * HOUR, &rs);
    assert_eq!(
        node.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(150.0)))
    );
    assert_eq!(node.coverage, i128::from(MINUTE), "coverage still stops");
}

#[test]
fn s6_edge_stretches() {
    let p = Params::new(Kind::Gauge, HOUR);
    let rs = vec![(T0, n(-50.0)), (T0 + 10 * SECOND, n(1.0))];
    // Leading stretch credited with the reading before the start; min and max
    // over readings inside only.
    let w = oracle::window(&p, &rs, T0 + 5 * SECOND, T0 + 20 * SECOND, T0 + HOUR);
    assert_eq!(w.count, 1);
    assert_eq!(
        w.aggregate(&Aggregate::Min),
        Some(AggValue::Exact(exact(1.0)))
    );
    assert_eq!(w.coverage, i128::from(15 * SECOND));
    // No reading inside, but a valid held value: last uses it; min is empty.
    let held = oracle::window(&p, &rs, T0 + 2 * SECOND, T0 + 4 * SECOND, T0 + HOUR);
    assert_eq!(held.count, 0);
    assert_eq!(
        held.aggregate(&Aggregate::Last),
        Some(AggValue::Value(n(-50.0)))
    );
    assert_eq!(held.aggregate(&Aggregate::Min), None);
    // The counter step to the first reading inside is credited to the window.
    let pc = Params::new(Kind::Counter, HOUR);
    let rc = vec![(T0, n(0.0)), (T0 + 10 * SECOND, n(10.0))];
    let w = oracle::window(&pc, &rc, T0 + 9 * SECOND, T0 + 20 * SECOND, T0 + HOUR);
    assert_eq!(
        w.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(10.0)))
    );
}

#[test]
fn s6_windows_exact_to_the_nanosecond() {
    let p = Params::new(Kind::Gauge, SECOND);
    let rs = vec![(T0, n(1.0))];
    let tree = SeriesTree::build(p, &rs);
    let w = tree.window(T0 + 1, T0 + 7, T0 + HOUR);
    assert_eq!(w.coverage, 6);
    assert_eq!(w, oracle::window(&p, &rs, T0 + 1, T0 + 7, T0 + HOUR));
}
