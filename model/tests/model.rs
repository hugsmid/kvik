//! Behaviour of the model where it implements a simpler rule than the
//! specification (see "Where the model differs from the specification" in
//! `OPEN-QUESTIONS.md`). These tests keep that behaviour stable until the
//! model follows the specification; they do not claim a specification row.

mod common;

use common::*;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik_model::condition::{Condition, Event, Runner};
use kvik_model::engine::{Config, ConsumerDecl, ConsumerId, Model, Selection};
use kvik_model::oracle;
use kvik_model::registry::SeriesOptions;
use kvik_model::summary::{Aggregate, Params};
use kvik_model::tree::{SeriesTree, TreeError, quarters_in_day};
use kvik_model::types::{
    DAY, HOUR, Kind, MINUTE, Nanos, Reading, SECOND, SeriesId, Value, ValueType, floor_to,
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
        .define_with(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    (m, s)
}

fn num(s: SeriesId, t: Nanos, v: f64) -> Reading {
    Reading::num(s, t, v)
}

#[test]
fn consumer_down_for_a_day_resumes_from_cursor_replayed() {
    let (mut m, s) = model();
    m.ingest(vec![num(s, T0, 0.0)]).unwrap();
    m.drain().unwrap();
    let cursor = m.disk.cursors[&C];
    let mut times = Vec::new();
    for i in 1..=200 {
        m.advance(DAY / 200);
        let now = m.wall;
        m.ingest(vec![num(s, now, i as f64)]).unwrap();
        times.push(now);
    }
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let obs = &m.observed[&C];
    let resumed: Vec<_> = obs.iter().filter(|o| o.session == 2).collect();
    assert_eq!(resumed[0].position, cursor.next());
    assert_eq!(resumed.len(), 200);
    assert!(resumed.iter().all(|o| o.replayed));
    // Original times are kept.
    let facts = m.facts().unwrap();
    assert!(
        facts
            .iter()
            .skip(1)
            .zip(&times)
            .all(|(f, t)| f.received_at == *t)
    );
}

#[test]
fn restart_rebuilds_threshold_without_firing_again() {
    let (mut m, s) = model();
    let cond = Condition::Threshold {
        on: 50.0,
        clear: 40.0,
    };
    let avg = |m: &Model| -> Option<f64> {
        let end = floor_to(m.wall, SECOND);
        m.window(s, end - MINUTE, end)
            .unwrap()
            .aggregate(&Aggregate::Avg)
            .and_then(|v| v.to_f64())
    };
    let mut runner = Runner::new(cond.clone(), m.mono);
    let mut fired = 0;
    for v in [10.0, 90.0, 90.0] {
        let now = m.wall;
        m.ingest(vec![num(s, now, v)]).unwrap();
        m.advance(5 * SECOND);
        fired += runner
            .on_value(m.mono, avg(&m))
            .iter()
            .filter(|e| **e == Event::Fired)
            .count();
    }
    assert_eq!(fired, 1);
    m.crash();
    m.restart();
    // Rebuild from the tree: history of the window value, then its value now.
    let history: Vec<f64> = (1..=3)
        .filter_map(|k| {
            let end = floor_to(m.wall, SECOND) - k * 5 * SECOND;
            m.window(s, end - MINUTE, end)
                .unwrap()
                .aggregate(&Aggregate::Avg)
                .and_then(|v| v.to_f64())
        })
        .rev()
        .collect();
    let mut runner = Runner::rebuild(cond, m.mono, &history, avg(&m));
    assert!(runner.active());
    assert!(runner.on_value(m.mono, avg(&m)).is_empty());
    // It clears (only the 0 reading is left in the window), then fires again.
    let now = m.wall;
    m.ingest(vec![num(s, now, 0.0)]).unwrap();
    m.advance(MINUTE + 5 * SECOND);
    assert_eq!(runner.on_value(m.mono, avg(&m)), vec![Event::Cleared]);
    let now = m.wall;
    m.ingest(vec![num(s, now, 100.0)]).unwrap();
    m.advance(5 * SECOND);
    assert_eq!(runner.on_value(m.mono, avg(&m)), vec![Event::Fired]);
}

#[test]
fn calendar_depends_on_zone_utc_levels_do_not() {
    let (mut m, s) = model();
    let date = Date::constant(2026, 3, 29);
    for h in 0..30 {
        m.ingest(vec![num(s, T0 - 6 * HOUR + h * HOUR, h as f64)])
            .unwrap();
    }
    let facts = m.facts().unwrap();
    let trees = m.trees().unwrap().clone();
    let rs = oracle::readings(m.current().unwrap(), s);
    let p = m.tree(s).unwrap().params;
    let mut days = Vec::new();
    for zone in ["Europe/Paris", "Pacific/Auckland"] {
        let tz = TimeZone::get(zone).unwrap();
        let d = m.tree(s).unwrap().day(&tz, date).unwrap();
        assert_eq!(d, oracle::day(&p, &rs, &tz, date).unwrap());
        days.push(d);
    }
    assert_ne!(days[0], days[1], "the calendar level depends on the zone");
    assert_eq!(m.facts().unwrap(), facts, "facts untouched");
    for (id, t) in &trees {
        assert!(
            t.same_summaries(&m.trees().unwrap()[id]),
            "UTC levels untouched"
        );
    }
}

/// §5.7: a calendar result is a window evaluation over the day, edge stretches
/// included. The model's stored day node omits the stretches before the day's
/// first and after its last reading, so it differs from the window.
#[test]
fn day_node_omits_edge_stretches() {
    let tz = TimeZone::get("Asia/Tokyo").unwrap();
    let date = Date::constant(2026, 6, 1);
    let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
    // A dense series (expected interval 1 s), so a day node is stored.
    let p = Params {
        expected_interval: Some(SECOND),
        ..Params::new(Kind::Gauge, None)
    };
    let rs = vec![
        (ds - HOUR, Value::Num(900.0)),
        (ds + HOUR, Value::Num(100.0)),
        (de - HOUR, Value::Num(500.0)),
    ];
    let tree = SeriesTree::build(p, &rs);
    let node = tree.day(&tz, date).unwrap();
    let window = tree.window(ds, de, de);
    assert_eq!(node.coverage, i128::from(22 * HOUR));
    assert_eq!(window.coverage, i128::from(24 * HOUR));
}

/// §5.7: days whose edges are not on 15-minute boundaries are evaluated as
/// windows in the specification; the model refuses them.
#[test]
fn unaligned_days_are_refused() {
    let monrovia = TimeZone::get("Africa/Monrovia").unwrap();
    assert!(matches!(
        quarters_in_day(&monrovia, Date::constant(1970, 6, 1)),
        Err(TreeError::UnalignedDay { .. })
    ));
    let apia = TimeZone::get("Pacific/Apia").unwrap();
    assert_eq!(
        quarters_in_day(&apia, Date::constant(2011, 12, 30)).unwrap(),
        0,
        "a skipped date has an empty span"
    );
}
