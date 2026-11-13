//! Executable demonstrations of `OPEN-QUESTIONS.md`: each test pins what the
//! model does today where the specification is open, so that a change in the
//! specification shows up here.

mod common;

use common::*;
use kvik_model::engine::{Config, ConsumerDecl, ConsumerId, Model, Selection};
use kvik_model::oracle;
use kvik_model::summary::{Body, Params};
use kvik_model::types::{HOUR, Kind, Nanos, Quality, Reading, SECOND, SeriesId, Value, ValueType};

const C: ConsumerId = ConsumerId(1);

fn model() -> (Model, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let s = m
        .define(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
        )
        .unwrap();
    (m, s)
}

/// Q-14: the model credits the state transition from the reading before a
/// window to its first reading inside, as it does the counter step.
#[test]
fn q14_leading_edge_state_transition() {
    let p = Params::new(Kind::State, None);
    let rs: Vec<(Nanos, Value)> = vec![
        (T0, Value::Str("idle".into())),
        (T0 + 10 * SECOND, Value::Str("running".into())),
    ];
    let w = oracle::window(&p, &rs, T0 + 5 * SECOND, T0 + 20 * SECOND, T0 + HOUR);
    let Body::State { transitions, .. } = w.body else {
        unreachable!()
    };
    assert_eq!(transitions, 1);
}

/// Q-15: after a forward jump of the system clock, the window clock moves
/// ahead at once and a rolling window's coverage stops at the last reading's
/// event time plus S.
#[test]
fn q15_forward_clock_jump_reduces_window_coverage() {
    let (mut m, s) = model(); // S = 10 s
    let t = m.wall;
    m.ingest(vec![Reading::num(s, t, 1.0)]).unwrap();
    m.jump_wall(HOUR);
    assert_eq!(m.now, t + HOUR);
    let w = m.window(s, m.now - 5 * SECOND, m.now).unwrap();
    assert_eq!(w.coverage, 0);
}

/// Q-17: the model records a non-finite reading as "no value" and compares
/// that, so a repeat of the same NaN reading is dropped as a duplicate.
#[test]
fn q17_repeated_non_finite_reading() {
    let (mut m, s) = model();
    let t = m.wall;
    m.ingest(vec![Reading::num(s, t, f64::NAN)]).unwrap();
    let r = m.ingest(vec![Reading::num(s, t, f64::NAN)]).unwrap();
    assert!(r.positions.is_empty());
    assert_eq!(m.counters.duplicates_dropped, 1);
    let f = &m.current().unwrap()[&(s, t)];
    assert_eq!((f.value.clone(), f.quality), (Value::Absent, Quality::Bad));
}
