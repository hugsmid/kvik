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
use kvik_model::summary::Aggregate;
use kvik_model::types::{
    DAY, HOUR, Kind, MINUTE, Nanos, Reading, SECOND, SeriesId, ValueType, floor_to,
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
    assert_eq!(resumed[0].position, cursor + 1);
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
    let mut r = Model::restore_from(m.config.clone(), vec![], &m.disk.bucket, m.wall, true);
    assert_eq!(r.current().unwrap(), &cur, "usable after current facts");
    assert!(r.tree(s).unwrap().same_summaries(&tree));
    assert!(
        r.facts().unwrap().len() < durable(&m).len(),
        "detail not yet back"
    );
    r.restore_rest(false);
    assert_eq!(durable(&r), durable(&m));
}
