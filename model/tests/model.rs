//! Behaviour of the model where it implements a simpler rule than the
//! specification (see "Where the model differs from the specification" in
//! `OPEN-QUESTIONS.md`). These tests keep that behaviour stable until the
//! model follows the specification; they do not claim a specification row.

mod common;

use common::*;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik_model::condition::{Condition, Event, Runner};
use kvik_model::engine::{
    CompactionCrash, Config, ConsumerDecl, ConsumerId, Error, FileClass, Locality, Model, Selection,
};
use kvik_model::oracle;
use kvik_model::summary::{AggValue, Aggregate, Body, Params, direct};
use kvik_model::tree::{SeriesTree, TreeError, quarters_in_day};
use kvik_model::types::{
    DAY, Exact, HOUR, Kind, MINUTE, Nanos, Quality, Reading, SECOND, SeriesId, Value, ValueType,
    floor_to,
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

fn num(s: SeriesId, t: Nanos, v: f64) -> Reading {
    Reading::num(s, t, v)
}

fn sealed_model() -> (Model, SeriesId) {
    let (mut m, s) = model();
    let base = m.wall - 10 * SECOND;
    for i in 0..5 {
        m.ingest(vec![num(s, base + i * SECOND, i as f64)]).unwrap();
    }
    m.ingest(vec![num(s, base, 100.0)]).unwrap(); // a revision
    m.seal_active();
    (m, s)
}

#[test]
fn compaction_crash_after_publish_resumes_retirement() {
    let (mut m, _) = sealed_model();
    let before = durable(&m);
    assert_eq!(
        m.compact(Some(CompactionCrash::AfterPublish)),
        Err(Error::Crashed)
    );
    // Both inputs and outputs are listed and readable.
    let r = m.open_reader();
    let classes: Vec<FileClass> = r.files.iter().map(|f| m.disk.files[f].class).collect();
    assert!(classes.contains(&FileClass::Segment) && classes.contains(&FileClass::Current));
    let (_, facts) = m.read(&r).unwrap();
    assert_eq!(facts.len(), before.len());
    m.restart();
    let listed = m.disk.listed_at(m.open_reader().epoch);
    assert!(
        listed
            .iter()
            .all(|f| m.disk.files[f].class != FileClass::Segment)
    );
    assert_eq!(durable(&m), before);
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

#[test]
fn reader_epoch_survives_grace_then_reads_remotely() {
    let (mut m, _) = sealed_model();
    let reader = m.open_reader();
    let (_, facts) = m.read(&reader).unwrap();
    m.compact(None).unwrap();
    m.upload();
    m.advance(HOUR - 1);
    m.housekeeping().unwrap();
    let (loc, again) = m.read(&reader).unwrap();
    assert!(
        loc.iter().all(|(_, l)| *l == Locality::Hot),
        "survive the grace period"
    );
    assert_eq!(again, facts);
    m.advance(1);
    let removed = m.housekeeping().unwrap();
    assert!(!removed.is_empty());
    let (loc, again) = m.read(&reader).unwrap();
    assert!(loc.iter().all(|(_, l)| *l != Locality::Missing));
    assert!(loc.iter().any(|(_, l)| *l == Locality::Remote));
    assert_eq!(again, facts);
}

#[test]
fn restore_onto_empty_disk_current_first() {
    let (mut m, s) = sealed_model();
    m.compact(None).unwrap();
    m.upload();
    let cur = m.current().unwrap().clone();
    let tree = m.tree(s).unwrap().clone();
    let mut r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, true).unwrap();
    assert_eq!(r.current().unwrap(), &cur, "usable after current facts");
    assert!(r.tree(s).unwrap().same_summaries(&tree));
    assert!(
        r.facts().unwrap().len() < durable(&m).len(),
        "detail not yet back"
    );
    r.restore_rest(false);
    assert_eq!(durable(&r), durable(&m));
}

/// §3 Disk space: below the low mark the specification pauses compaction only;
/// the model also pauses tiering, which would free space.
#[test]
fn low_mark_pauses_tiering_too() {
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

/// §5.7: a calendar result is a window evaluation over the day, edge stretches
/// included. The model's stored day node omits the stretches before the day's
/// first and after its last reading, so it differs from the window.
#[test]
fn day_node_omits_edge_stretches() {
    let tz = TimeZone::get("Asia/Tokyo").unwrap();
    let date = Date::constant(2026, 6, 1);
    let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
    let p = Params::new(Kind::Gauge, None);
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

/// §6: rate is the increase over the elapsed time between the readings that
/// bound it; the model still divides by covered seconds.
#[test]
fn rate_divides_by_covered_seconds() {
    let p = Params::new(Kind::Counter, 3 * MINUTE);
    let rs = vec![(T0, Value::Num(100.0)), (T0 + HOUR, Value::Num(250.0))];
    let w = oracle::window(&p, &rs, T0, T0 + HOUR + SECOND, T0 + HOUR + SECOND);
    // 150 over 181 covered seconds (180 s held, 1 s trailing); the
    // specification gives 150 over 3600 s, flagged.
    assert_eq!(
        w.aggregate(&Aggregate::Rate),
        Some(AggValue::Exact(Exact::new(150.into(), 181.into())))
    );
}

/// §2.4: identity uses the adapter-supplied quality; the model compares the
/// quality after future stamping, so a resend as good once due is recorded.
#[test]
fn duplicate_check_compares_engine_assigned_quality() {
    let (mut m, s) = model();
    let t = m.wall + 2 * MINUTE;
    m.ingest(vec![num(s, t, 5.0)]).unwrap();
    m.advance(3 * MINUTE);
    let r = m.ingest(vec![num(s, t, 5.0)]).unwrap();
    assert_eq!(r.positions.len(), 1);
    assert_eq!(m.current().unwrap()[&(s, t)].quality, Quality::Good);
}

/// §5.5: counter values are non-negative and a negative value is recorded as bad
/// quality; the model applies the reset rule to negative values.
#[test]
fn negative_counter_decrease_counts_as_reset() {
    let p = Params::new(Kind::Counter, HOUR);
    let rs = vec![(T0, Value::Num(-10.0)), (T0 + SECOND, Value::Num(-20.0))];
    let Body::Counter { resets, .. } = direct(&p, T0, T0 + MINUTE, &rs).body else {
        unreachable!()
    };
    assert_eq!(resets, 1);
}
