//! Executable demonstrations of `OPEN-QUESTIONS.md`: each test pins what the
//! model does today where the specification is open, so that a change in the
//! specification shows up here.

mod common;

use common::*;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik_model::engine::{Config, ConsumerDecl, ConsumerId, Error, FileClass, Model, Selection};
use kvik_model::oracle;
use kvik_model::summary::{AggValue, Aggregate, Body, Params, direct};
use kvik_model::tree::{SeriesTree, TreeError, quarters_in_day};
use kvik_model::types::{
    Exact, HOUR, Kind, MINUTE, Nanos, Quality, Reading, SECOND, SeriesId, Value, ValueType,
};

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

fn n(v: f64) -> Value {
    Value::Num(v)
}

/// Q-01: a sealed file corrupted before any other tier holds it is lost.
#[test]
fn q01_corrupt_before_backup_loses_facts() {
    let (mut m, s) = model();
    let now = m.wall;
    m.ingest(vec![Reading::num(s, now, 1.0)]).unwrap();
    m.seal_active();
    let f = *m
        .disk
        .hot
        .iter()
        .find(|f| m.disk.files[f].class == FileClass::Segment)
        .unwrap();
    m.corrupt_hot(f);
    m.scrub();
    assert!(!m.restore_file(f));
    assert!(m.disk.all_facts().is_empty());
}

/// Q-02: on overflow the model catches up from the first position not queued,
/// not from the confirmed cursor, so there is no repeat within a session.
#[test]
fn q02_overflow_catch_up_position() {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 2,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            SECOND,
        )
        .unwrap();
    for i in 0..5 {
        m.ingest(vec![Reading::num(s, T0 + i * SECOND, 1.0 + i as f64)])
            .unwrap();
    }
    assert_eq!(m.disk.cursors[&C], 0, "nothing confirmed yet");
    m.drain().unwrap();
    let ps: Vec<u64> = m.observed[&C].iter().map(|o| o.position).collect();
    assert_eq!(ps, vec![2, 3, 4, 5, 6], "no repeats, in order");
}

/// Q-03: below the low mark, tiering pauses although it would free space.
#[test]
fn q03_low_mark_pauses_tiering() {
    let cfg = Config {
        disk_bytes: 100_000,
        low_mark: 20_000,
        critical_mark: 5_000,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    m.disk.other_bytes = 85_000;
    assert_eq!(m.housekeeping(), Err(Error::Paused));
}

/// Q-04: the stored day node omits the stretches before the day's first and
/// after its last reading, so it differs from the window over the same day.
#[test]
fn q04_day_node_differs_from_day_window() {
    let tz = TimeZone::get("Asia/Tokyo").unwrap();
    let date = Date::constant(2026, 6, 1);
    let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
    let p = Params::new(Kind::Gauge, None);
    let rs = vec![
        (ds - HOUR, n(900.0)),
        (ds + HOUR, n(100.0)),
        (de - HOUR, n(500.0)),
    ];
    let tree = SeriesTree::build(p, &rs);
    let node = tree.day(&tz, date).unwrap();
    let window = tree.window(ds, de, de);
    assert_eq!(node.coverage, i128::from(22 * HOUR));
    assert_eq!(window.coverage, i128::from(24 * HOUR));
}

/// Q-05: rate is increase ÷ covered seconds, so a step across a gap gives a
/// rate far above the true average.
#[test]
fn q05_rate_across_gap() {
    let p = Params::new(Kind::Counter, 3 * MINUTE);
    let rs = vec![(T0, n(100.0)), (T0 + HOUR, n(250.0))];
    let w = oracle::window(&p, &rs, T0, T0 + HOUR + SECOND, T0 + HOUR + SECOND);
    // 150 over 181 covered seconds (180 s held, 1 s trailing), not over 3601 s.
    assert_eq!(
        w.aggregate(&Aggregate::Rate),
        Some(AggValue::Exact(Exact::new(150.into(), 181.into())))
    );
}

/// Q-06: days outside 92–100 quarter-hours, or not on 15-minute boundaries.
#[test]
fn q06_days_outside_the_rule() {
    let apia = TimeZone::get("Pacific/Apia").unwrap();
    assert_eq!(
        quarters_in_day(&apia, Date::constant(2011, 12, 30)).unwrap(),
        0
    );
    let monrovia = TimeZone::get("Africa/Monrovia").unwrap();
    assert!(matches!(
        quarters_in_day(&monrovia, Date::constant(1970, 6, 1)),
        Err(TreeError::UnalignedDay { .. })
    ));
}

/// Q-07: a forward jump of the system clock moves the window clock forward at
/// once, so a healthy series looks stale.
#[test]
fn q07_forward_clock_jump_makes_series_stale() {
    let (mut m, s) = model(); // S = 10 s
    let t = m.wall;
    m.ingest(vec![Reading::num(s, t, 1.0)]).unwrap();
    m.jump_wall(HOUR);
    let w = m.window(s, t, t + 2 * HOUR).unwrap();
    assert_eq!(w.coverage, i128::from(10 * SECOND));
    assert_eq!(m.now, t + HOUR);
}

/// Q-09: quality is compared after future stamping, so resending a future
/// reading as good once it is due records a new fact.
#[test]
fn q09_quality_compared_after_stamping() {
    let (mut m, s) = model();
    let t = m.wall + 2 * MINUTE;
    m.ingest(vec![Reading::num(s, t, 5.0)]).unwrap();
    m.advance(3 * MINUTE);
    let r = m.ingest(vec![Reading::num(s, t, 5.0)]).unwrap();
    assert_eq!(r.positions.len(), 1);
    assert_eq!(m.current().unwrap()[&(s, t)].quality, Quality::Good);
}

/// Q-10: text nodes carry coverage in the model.
#[test]
fn q10_text_coverage() {
    let p = Params::new(Kind::Text, None);
    let rs = vec![
        (T0, Value::Str("ok".into())),
        (T0 + SECOND, Value::Str("ok".into())),
    ];
    assert_eq!(
        direct(&p, T0, T0 + MINUTE, &rs).coverage,
        i128::from(SECOND)
    );
}

/// Q-11: the model keeps counters across a restart.
#[test]
fn q11_counters_across_restart() {
    let (mut m, s) = model();
    m.ingest(vec![Reading::num(s, T0, 1.0), Reading::num(s, T0, 1.0)])
        .unwrap();
    m.restart();
    assert_eq!(m.counters.duplicates_dropped, 1);
}

/// Q-12: a restore loses facts after the last uploaded sealed file.
#[test]
fn q12_restore_loses_unsealed_facts() {
    let (mut m, s) = model();
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.upload();
    m.ingest(vec![Reading::num(s, T0 + SECOND, 2.0)]).unwrap();
    let r = Model::restore_from(m.config.clone(), vec![], &m.disk.bucket, m.wall, false);
    assert_eq!(r.facts().unwrap().len(), 1);
    assert_eq!(m.facts().unwrap().len(), 2);
}

/// Q-13: with negative counter values, an ordinary decrease is a reset.
#[test]
fn q13_negative_counter_values() {
    let p = Params::new(Kind::Counter, HOUR);
    let rs = vec![(T0, n(-10.0)), (T0 + SECOND, n(-20.0))];
    let Body::Counter { resets, .. } = direct(&p, T0, T0 + MINUTE, &rs).body else {
        unreachable!()
    };
    assert_eq!(resets, 1);
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
