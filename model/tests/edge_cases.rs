//! One test per row of the edge-case table in spec §9, named after the row.
//! Rows whose spec test is a property test also have a property here.

mod common;

use std::collections::BTreeMap;

use kvik_model::calendar::{Month, Period};

use common::*;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik_model::condition::{Condition, ConditionState, Event, Runner};
use kvik_model::engine::{
    CompactionCrash, Config, ConsumerDecl, ConsumerId, CrashPoint, Error, FORMAT_VERSION, Fault,
    FileClass, FileId, Funnel, FunnelId, Level, Locality, Model, Operator, Reason, Refusal,
    Selection, TriggerCause, Wait, WriteOutcome,
};
use kvik_model::oracle;
use kvik_model::registry::{ChangedAtKind, SeriesOptions, Timing};
use kvik_model::summary::{AggValue, Aggregate, Params};
use kvik_model::tree::{SeriesTree, TreeError, quarters_in_day};
use kvik_model::types::{
    DAY, HOUR, Kind, MINUTE, Nanos, Position, QUARTER, Quality, Reading, SECOND, SeriesId, Value,
    ValueType, exact, floor_to,
};
use kvik_model::view;
use proptest::prelude::*;

const C: ConsumerId = ConsumerId(1);

fn model_with(cfg: Config) -> (Model, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(cfg, vec![consumer], T0 + HOUR);
    let s = m
        .define_with(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    (m, s)
}

fn model() -> (Model, SeriesId) {
    model_with(Config::default())
}

fn num(s: SeriesId, t: Nanos, v: f64) -> Reading {
    Reading::num(s, t, v)
}

fn leaf_ok(m: &Model, s: SeriesId, t: Nanos) {
    let tree = m.tree(s).unwrap();
    let rs = oracle::readings(m.current().unwrap(), s);
    let minute = floor_to(t, MINUTE);
    assert_eq!(
        tree.node(0, minute),
        oracle::node(&tree.params, &rs, minute, minute + MINUTE)
    );
    let q = floor_to(t, QUARTER);
    assert_eq!(
        tree.node(1, q),
        oracle::node(&tree.params, &rs, q, q + QUARTER)
    );
}

fn delivered(m: &Model) -> Vec<Position> {
    m.observed
        .get(&C)
        .map(|o| o.iter().map(|x| x.position).collect())
        .unwrap_or_default()
}

#[test]
fn row_01_same_t_different_value_becomes_current() {
    let (mut m, s) = model();
    let t = T0 + 5 * SECOND + 3;
    let a = m.ingest(vec![num(s, t, 1.0)]).unwrap().positions[0];
    let b = m.ingest(vec![num(s, t, 2.0)]).unwrap().positions[0];
    let cur = &m.current().unwrap()[&(s, t)];
    assert_eq!(cur.position, b);
    assert_eq!(cur.value, Value::Num(2.0));
    let facts = m.facts().unwrap();
    let hist = view::history(facts.iter(), s, t);
    assert_eq!(
        hist.iter().map(|f| f.position).collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(view::revisions(facts.iter())[0].position, a);
    leaf_ok(&m, s, t);
}

proptest! {
    /// Row 1 property: any sequence of values at the same `(series, t)`
    /// records each value the first time it is seen (a repeat of any earlier
    /// value is a duplicate, row 2a); the last new value is current and the
    /// minute path is recomputed.
    #[test]
    fn row_01_property(vals in proptest::collection::vec(-3i32..3, 1..10), off in arb_offset()) {
        let (mut m, s) = model();
        let t = T0 + off;
        for v in &vals {
            m.ingest(vec![num(s, t, f64::from(*v))]).unwrap();
        }
        let mut seen: Vec<i32> = Vec::new();
        for v in &vals {
            if !seen.contains(v) {
                seen.push(*v);
            }
        }
        prop_assert_eq!(&m.current().unwrap()[&(s, t)].value, &Value::Num(f64::from(*seen.last().unwrap())));
        prop_assert_eq!(m.facts().unwrap().len(), seen.len());
        leaf_ok(&m, s, t);
    }
}

/// Row 2: "Same `(series, t)`, identical to any fact in its history: dropped
/// and counted (§2.4)." Across batches and within one; a different quality or
/// `t` is a new fact.
#[test]
fn row_02_identical_repeat_dropped_and_counted() {
    let (mut m, s) = model();
    let t = T0 + 7 * SECOND;
    m.ingest(vec![num(s, t, 1.0)]).unwrap();
    // Across batches and within one batch.
    let r = m.ingest(vec![num(s, t, 1.0), num(s, t, 1.0)]).unwrap();
    assert_eq!(r.duplicates, 2);
    assert_eq!(m.counters.duplicates_dropped, 2);
    assert_eq!(m.facts().unwrap().len(), 1);
    // A different quality is a different fact.
    m.ingest(vec![Reading {
        quality: Quality::Estimated,
        ..num(s, t, 1.0)
    }])
    .unwrap();
    assert_eq!(m.facts().unwrap().len(), 2);
    // The same value one nanosecond later is always recorded.
    m.ingest(vec![num(s, t + 1, 1.0)]).unwrap();
    assert_eq!(m.facts().unwrap().len(), 3);
}

/// Row 2a: "Series not versioned: A, then B, then a replay of A: replay
/// dropped; B stays current. With `revise`, A is recorded and current."
#[test]
fn row_02a_replay_of_earlier_value_dropped_unless_revise() {
    let (mut m, s) = model();
    let t = T0 + 7 * SECOND;
    m.ingest(vec![num(s, t, 1.0)]).unwrap(); // A
    m.ingest(vec![num(s, t, 2.0)]).unwrap(); // B
    let r = m.ingest(vec![num(s, t, 1.0)]).unwrap(); // replay of A
    assert_eq!(r.duplicates, 1);
    assert_eq!(m.current().unwrap()[&(s, t)].value, Value::Num(2.0));
    // With revise intent, A is recorded and becomes current.
    m.emit_revise(num(s, t, 1.0)).unwrap();
    m.commit(None).unwrap();
    let cur = &m.current().unwrap()[&(s, t)];
    assert_eq!(cur.value, Value::Num(1.0));
    assert!(cur.flags.revise);
    assert_eq!(m.facts().unwrap().len(), 3);
}

/// Row 2b: "Identical repeat whose current fact is off every local tier:
/// recorded, `duplicates_unchecked`."
#[test]
fn row_02b_repeat_whose_current_fact_is_off_every_local_tier() {
    let cfg = Config {
        disk_bytes: 400,
        low_mark: 0,
        critical_mark: 0,
        ..Config::default()
    };
    let (mut m, s) = model_with(cfg);
    for i in 0..6 {
        m.ingest(vec![num(s, T0 + i, 1.0)]).unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    m.upload();
    m.advance(2 * HOUR);
    m.housekeeping().unwrap();
    // An identical repeat is recorded, not dropped: the check never reads
    // off-machine data (§2.4, §12 D5).
    let r = m.ingest(vec![num(s, T0 + 1, 1.0)]).unwrap();
    assert_eq!((r.positions.len(), r.duplicates), (1, 0));
    assert_eq!(m.counters.duplicates_unchecked, 1);
    // A different value is recorded and counted the same way.
    m.ingest(vec![num(s, T0, 2.0)]).unwrap();
    assert_eq!(m.counters.duplicates_unchecked, 2);
    // Once the repeat is recorded locally, the next one is checked again.
    let r = m.ingest(vec![num(s, T0 + 1, 1.0)]).unwrap();
    assert_eq!(r.duplicates, 1);
    assert_eq!(m.counters.duplicates_unchecked, 2);
}

/// A store with a consumer of every series and one versioned gauge, with
/// `changed_at` of the given kind and timing.
fn versioned_model(options: SeriesOptions) -> (Model, SeriesId) {
    let (mut m, _) = model();
    let v = m
        .define_with(
            "site/forecast-1/load",
            Kind::Gauge,
            ValueType::Number,
            "kW",
            10 * SECOND,
            options,
        )
        .unwrap();
    (m, v)
}

/// Moves every file holding `p` off the local tiers (they are in the bucket).
fn move_off_machine(m: &mut Model, p: Position) {
    m.seal_active();
    m.upload();
    for f in m.disk.hot.clone() {
        if m.disk.files[&f].entries().any(|e| e.position == p) {
            assert!(m.disk.in_bucket(f));
            m.disk.hot.remove(&f);
            m.disk.cold.remove(&f);
        }
    }
}

proptest! {
    /// Row 2c: "Versioned series: revision A (`changed_at` 1), correction B
    /// (2), then a replay of A, also with A's detail off every tier: replay
    /// is a duplicate or recorded unchecked; B stays current. A later
    /// revision (3) with A's value is recorded and current."
    #[test]
    fn row_02c_replay_never_reverts_a_versioned_correction(
        a in -3i32..3, step in 1i32..6, off in any::<bool>(), compact in any::<bool>(),
    ) {
        // b differs from a by construction, so no case is rejected.
        let b = (a + 3 + step) % 6 - 3;
        let (mut m, v) = versioned_model(SeriesOptions::versioned(ChangedAtKind::Version));
        let (a, b) = (f64::from(a), f64::from(b));
        let pa = m.ingest(vec![Reading::versioned(v, T0, a, 1)]).unwrap().positions[0];
        if compact {
            m.seal_active();
            m.compact(None).unwrap();
        }
        let pb = m.ingest(vec![Reading::versioned(v, T0, b, 2)]).unwrap().positions[0];
        if off {
            move_off_machine(&mut m, pa);
        }
        let r = m.ingest(vec![Reading::versioned(v, T0, a, 1)]).unwrap();
        if off {
            prop_assert_eq!((r.positions.len(), m.counters.duplicates_unchecked), (1, 1));
            prop_assert_eq!(m.counters.outranked_on_arrival, 1);
        } else {
            prop_assert_eq!((r.positions.len(), r.duplicates), (0, 1));
        }
        prop_assert_eq!(m.current().unwrap()[&(v, T0)].position, pb);
        let p3 = m.ingest(vec![Reading::versioned(v, T0, a, 3)]).unwrap().positions[0];
        let cur = &m.current().unwrap()[&(v, T0)];
        prop_assert_eq!((cur.position, cur.value.clone()), (p3, Value::Num(a)));
    }

    /// Row 2d: "Versioned series: a fact with a lower `changed_at` than the
    /// current one and a new value: recorded, delivered flagged `outranked`,
    /// counted; no node, live-state or condition change."
    #[test]
    fn row_02d_lower_changed_at_is_outranked(
        (cur, lower) in (1u64..5).prop_flat_map(|c| (Just(c), 0..c)),
        a in -3i32..3,
        d in 1i32..4,
    ) {
        let b = a + d;
        let (mut m, v) = versioned_model(SeriesOptions::versioned(ChangedAtKind::Version));
        m.add_route(C, FunnelId(1), Funnel::condition(v, None, Condition::OnChange { deadband: 0.0 })).unwrap();
        let now = m.wall;
        m.ingest(vec![Reading::versioned(v, now, f64::from(a), cur)]).unwrap();
        m.drain().unwrap();
        let tree = m.tree(v).unwrap().clone();
        let live = m.latest(v).unwrap();
        let received = m.received[&C].len();
        let p = m.ingest(vec![Reading::versioned(v, now, f64::from(b), lower)]).unwrap().positions[0];
        prop_assert_eq!(m.counters.outranked_on_arrival, 1);
        prop_assert_eq!(m.tree(v).unwrap(), &tree);
        prop_assert_eq!(m.latest(v).unwrap(), live);
        m.drain().unwrap();
        let new: Vec<_> = m.received[&C][received..].iter().map(|r| &r.delivery).collect();
        prop_assert_eq!(new.len(), 1, "only the fact, no condition delivery");
        prop_assert!(new[0].position == p && new[0].outranked);
    }

    /// Row 37: "Forecast re-issued: the higher `changed_at` is current;
    /// earlier issues stay in history; a replay of an old issue is a
    /// duplicate or outranked." Each issue covers some valid times, with the
    /// issue time as `changed_at`.
    #[test]
    fn row_37_forecast_reissued(
        issues in proptest::collection::vec(proptest::collection::btree_map(0i64..6, -3i32..3, 1..6), 1..5),
        replays in proptest::collection::vec((any::<proptest::sample::Index>(), any::<proptest::sample::Index>()), 0..6),
    ) {
        let options = SeriesOptions { versioned: Some(ChangedAtKind::Time), timing: Timing::Forecast, ..SeriesOptions::default() };
        let (mut m, v) = versioned_model(options);
        let valid = |h: i64| m.wall + h * HOUR;
        let valid: Vec<Nanos> = (0..6).map(valid).collect();
        let mut want: BTreeMap<Nanos, (u64, f64)> = BTreeMap::new();
        let mut issued = 0usize;
        for issue in &issues {
            let at = u64::try_from(m.wall).unwrap();
            let readings: Vec<Reading> = issue.iter()
                .map(|(h, x)| Reading::versioned(v, valid[*h as usize], f64::from(*x), at))
                .collect();
            issued += readings.len();
            m.ingest(readings).unwrap();
            for (h, x) in issue {
                want.insert(valid[*h as usize], (at, f64::from(*x)));
            }
            m.advance(MINUTE);
        }
        let check = |m: &Model| -> Result<(), TestCaseError> {
            for (t, (at, x)) in &want {
                let f = &m.current().unwrap()[&(v, *t)];
                prop_assert_eq!((f.changed_at, f.value.clone()), (Some(*at), Value::Num(*x)));
            }
            Ok(())
        };
        check(&m)?;
        prop_assert_eq!(m.facts().unwrap().len(), issued, "every issue stays in history");
        for (i, j) in &replays {
            let k = i.index(issues.len());
            let issue = &issues[k];
            let (h, x) = issue.iter().nth(j.index(issue.len())).unwrap();
            let at = u64::try_from(T0 + HOUR + k as i64 * MINUTE).unwrap();
            let outranked = m.counters.outranked_on_arrival;
            let r = m.ingest(vec![Reading::versioned(v, valid[*h as usize], f64::from(*x), at)]).unwrap();
            prop_assert!(r.duplicates == 1 || m.counters.outranked_on_arrival == outranked + 1);
            check(&m)?;
        }
    }
}

/// A store with consumer `C` (every series) and a gauge of the given
/// timing with S = 30 minutes; a forecast series is versioned with
/// `changed_at` kind `time`.
fn timed_model(timing: Timing) -> (Model, SeriesId) {
    let (mut m, _) = model();
    let versioned = (timing == Timing::Forecast).then_some(ChangedAtKind::Time);
    let s = m
        .define_with(
            "site/tariff-1/price",
            Kind::Gauge,
            ValueType::Number,
            "c/kWh",
            30 * MINUTE,
            SeriesOptions {
                versioned,
                ..SeriesOptions::timing(timing)
            },
        )
        .unwrap();
    (m, s)
}

fn timed_reading(m: &Model, s: SeriesId, t: Nanos, v: f64) -> Reading {
    let forecast = m.registry().unwrap().get(s).unwrap().options.timing == Timing::Forecast;
    if forecast {
        Reading::versioned(s, t, v, u64::try_from(m.wall).unwrap())
    } else {
        Reading::num(s, t, v)
    }
}

/// Row 36: "Scheduled value for tomorrow: recorded unflagged, quality kept;
/// delivered at commit as notice; in live state from its `t`, with no second
/// delivery; stale S after the last value takes effect."
#[test]
fn row_36_scheduled_value_for_tomorrow() {
    let (mut m, s) = timed_model(Timing::Scheduled);
    let now = m.wall;
    let today = Reading {
        quality: Quality::Estimated,
        ..Reading::num(s, now - MINUTE, 10.0)
    };
    let tomorrow = now + DAY;
    let p = m
        .ingest(vec![today, Reading::num(s, tomorrow, 20.0)])
        .unwrap()
        .positions[1];
    let f = m.current().unwrap()[&(s, tomorrow)].clone();
    assert!(!f.flags.future && !f.flags.late);
    assert_eq!(f.quality, Quality::Good);
    m.drain().unwrap();
    assert_eq!(
        delivered(&m).iter().filter(|q| **q == p).count(),
        1,
        "a notice at commit"
    );
    let notice = m.received[&C]
        .iter()
        .find(|r| r.delivery.position == p)
        .unwrap();
    assert!(!notice.delivery.fact().unwrap().flags.future);
    // Live state keeps today's value until tomorrow's takes effect.
    assert_eq!(m.latest(s).unwrap().unwrap().value, Value::Num(10.0));
    assert_eq!(m.scheduled_stale(s).unwrap(), Some(false));
    // Today's value falls stale S after it took effect.
    m.advance(30 * MINUTE);
    assert_eq!(m.scheduled_stale(s).unwrap(), Some(true));
    m.advance(DAY - 30 * MINUTE);
    assert_eq!(m.latest(s).unwrap().unwrap().value, Value::Num(20.0));
    assert_eq!(
        m.scheduled_stale(s).unwrap(),
        Some(false),
        "commits do not refresh it; t does"
    );
    m.drain().unwrap();
    assert_eq!(
        delivered(&m).iter().filter(|q| **q == p).count(),
        1,
        "no second delivery"
    );
    m.advance(30 * MINUTE + 1);
    assert_eq!(m.scheduled_stale(s).unwrap(), Some(true));
}

/// Row 37b: "Forecast source falls silent: no live staleness; reported by
/// stale-for on the adapter's companion series, and later by shrinking
/// lookahead coverage."
#[test]
fn row_37b_forecast_source_falls_silent() {
    let (mut m, f) = timed_model(Timing::Forecast);
    // The adapter's companion series: one reading per successful fetch.
    let fetch = m
        .define(
            "site/weather-adapter/fetch",
            Kind::Gauge,
            ValueType::Number,
            "1",
            10 * MINUTE,
        )
        .unwrap();
    m.add_route(
        C,
        FunnelId(1),
        Funnel::condition(
            fetch,
            None,
            Condition::StaleFor {
                staleness: 10 * MINUTE,
                n: 0,
            },
        ),
    )
    .unwrap();
    let issue = |m: &mut Model| {
        let now = m.wall;
        // Half-hourly values for the next 24 hours.
        let rs: Vec<Reading> = (0..48)
            .map(|k| timed_reading(m, f, now + k * 30 * MINUTE, 1.0))
            .collect();
        m.ingest(rs).unwrap();
        m.ingest(vec![Reading::num(fetch, now, 1.0)]).unwrap();
    };
    issue(&mut m);
    let lookahead = |m: &Model| m.window(f, m.now, m.now + DAY).unwrap().coverage;
    let full = lookahead(&m);
    assert_eq!(full, i128::from(DAY));
    // Silent for two hours: the forecast series has no live state and no live
    // staleness; the companion's stale-for fires; the lookahead shrinks.
    m.drain().unwrap();
    for _ in 0..12 {
        m.advance(10 * MINUTE);
        m.tick().unwrap();
    }
    assert_eq!(m.latest(f).unwrap(), None);
    assert_eq!(m.scheduled_stale(f).unwrap(), None);
    m.drain().unwrap();
    let fired = m.received[&C].iter().any(|r| {
        r.delivery
            .trigger()
            .is_some_and(|t| t.funnel == FunnelId(1) && t.state == ConditionState::Active(true))
    });
    assert!(fired, "stale-for on the companion series");
    assert_eq!(lookahead(&m), full - i128::from(2 * HOUR));
}

/// Row 38: "Window over tomorrow on a scheduled or forecast series: credited
/// up to the window end; coverage shows where the values run out."
#[test]
fn row_38_window_over_tomorrow_on_scheduled_or_forecast_series() {
    for timing in [Timing::Scheduled, Timing::Forecast] {
        let (mut m, s) = timed_model(timing);
        let start = floor_to(m.wall, DAY) + DAY;
        // Half-hourly values from tomorrow 00:00 to 06:00 (exclusive).
        let rs: Vec<Reading> = (0..12)
            .map(|k| timed_reading(&m, s, start + k * 30 * MINUTE, f64::from(k as i32)))
            .collect();
        m.ingest(rs).unwrap();
        let w = m.window(s, start, start + DAY).unwrap();
        assert_eq!(w.count, 12);
        // Credited to 06:00, the last value's validity (S = 30 minutes).
        assert_eq!(w.coverage, i128::from(6 * HOUR), "{timing:?}");
        let p = m.tree(s).unwrap().params;
        let rs = oracle::readings(m.current().unwrap(), s);
        assert_eq!(w, oracle::window(&p, &rs, start, start + DAY, m.now));
    }
}

/// Row 39: "Window ending after `now` on an observed series: nothing credited
/// past `now`." A window wholly after `now` is empty (§6 Windows), even with
/// a future fact before it.
#[test]
fn row_39_window_ending_after_now_on_an_observed_series() {
    let (mut m, s) = model(); // S = 10 s
    let now = m.wall;
    m.ingest(vec![
        num(s, now - 5 * SECOND, 1.0),
        num(s, now + 2 * MINUTE, 2.0),
    ])
    .unwrap();
    let w = m.window(s, now - 5 * SECOND, now + HOUR).unwrap();
    assert_eq!(w.coverage, i128::from(5 * SECOND));
    assert_eq!(w.count, 1);
    let w = m.window(s, now + 3 * MINUTE, now + HOUR).unwrap();
    assert_eq!((w.count, w.coverage, w.last.clone()), (0, 0, None));
}

/// A store with consumer `C` and an observed interval series of length
/// `d` and phase `phase` (an amount of energy per interval).
fn interval_model(d: Nanos, phase: Nanos) -> (Model, SeriesId) {
    let (mut m, _) = model();
    let s = m
        .define_with(
            "site/meter-1/energy",
            Kind::Interval,
            ValueType::Number,
            "kWh",
            None,
            SeriesOptions::interval(d, phase),
        )
        .unwrap();
    (m, s)
}

/// Row 40: "Interval reading off its phase: refused and counted."
#[test]
fn row_40_interval_reading_off_its_phase() {
    let (mut m, s) = interval_model(30 * MINUTE, 15 * MINUTE);
    let base = floor_to(m.wall, HOUR) - 2 * HOUR;
    assert_eq!(m.emit(num(s, base, 1.0)), Err(Refusal::OffPhase.into()));
    assert_eq!(
        m.emit(num(s, base + 15 * MINUTE + 1, 1.0)),
        Err(Refusal::OffPhase.into())
    );
    assert_eq!(m.counters.refused[&Refusal::OffPhase], 2);
    m.ingest(vec![
        num(s, base + 15 * MINUTE, 1.0),
        num(s, base - 15 * MINUTE, 2.0),
    ])
    .unwrap();
    assert_eq!(
        m.facts().unwrap().len(),
        2,
        "on the phase, also before the epoch's multiple"
    );
}

/// Row 41: "Window edge inside an interval: that interval's amount credited
/// pro rata by time; result flagged `apportioned`." Sum is the amount in the
/// window; rate is sum ÷ covered seconds; count, min, max and last are over
/// readings whose `t` lies in the window; avg, integral, increase and
/// time-in-state are refused (§6).
#[test]
fn row_41_window_edge_inside_an_interval() {
    let (mut m, s) = interval_model(30 * MINUTE, 0);
    let base = floor_to(m.wall, HOUR) - 3 * HOUR;
    m.ingest(vec![
        num(s, base, 6.0),
        num(s, base + 30 * MINUTE, 3.0),
        num(s, base + HOUR, 9.0),
    ])
    .unwrap();
    let ex = |v: f64| Some(AggValue::Exact(exact(v)));
    // Whole intervals: exact, not apportioned.
    let w = m.window(s, base, base + HOUR).unwrap();
    assert!(!w.apportioned());
    assert_eq!(w.aggregate(&Aggregate::Sum), ex(9.0));
    // From 10 minutes into the first interval to 10 minutes into the third.
    let w = m.window(s, base + 10 * MINUTE, base + 70 * MINUTE).unwrap();
    assert!(w.apportioned());
    assert_eq!(
        w.aggregate(&Aggregate::Sum),
        ex(6.0 * 2.0 / 3.0 + 3.0 + 9.0 / 3.0)
    );
    assert_eq!(w.coverage, i128::from(HOUR));
    let per_second = kvik_model::types::Exact::new(1.into(), 360.into());
    assert_eq!(
        w.aggregate(&Aggregate::Rate),
        Some(AggValue::Exact(per_second))
    );
    assert_eq!(
        (
            w.count,
            w.aggregate(&Aggregate::Min),
            w.aggregate(&Aggregate::Max)
        ),
        (2, ex(3.0), ex(9.0))
    );
    assert_eq!(
        w.aggregate(&Aggregate::Last),
        Some(AggValue::Value(Value::Num(9.0)))
    );
    // A window inside one interval holds no reading but an amount.
    let w = m.window(s, base + 40 * MINUTE, base + 50 * MINUTE).unwrap();
    assert_eq!((w.count, w.aggregate(&Aggregate::Sum)), (0, ex(1.0)));
    for agg in [Aggregate::Avg, Aggregate::Integral, Aggregate::Increase] {
        assert_eq!(w.aggregate(&agg), None);
        let route = Funnel::condition(
            s,
            Some(Operator {
                aggregate: agg,
                width: HOUR,
            }),
            Condition::Threshold {
                on: 1.0,
                clear: 0.0,
            },
        );
        assert!(matches!(
            m.add_route(C, FunnelId(1), route),
            Err(Error::BadRoute(_))
        ));
    }
}

/// Row 42: "Interval missing: a gap in coverage; nothing held across it."
#[test]
fn row_42_interval_missing() {
    let (mut m, s) = interval_model(15 * MINUTE, 0);
    let base = floor_to(m.wall, HOUR) - 3 * HOUR;
    m.ingest(vec![num(s, base, 1.0), num(s, base + 30 * MINUTE, 2.0)])
        .unwrap();
    let w = m.window(s, base, base + 45 * MINUTE).unwrap();
    assert_eq!(w.coverage, i128::from(30 * MINUTE));
    assert_eq!(
        w.aggregate(&Aggregate::Sum),
        Some(AggValue::Exact(exact(3.0)))
    );
    // A window over the missing interval alone is empty: no held value.
    let w = m.window(s, base + 15 * MINUTE, base + 30 * MINUTE).unwrap();
    assert_eq!((w.coverage, w.count, w.last.clone()), (0, 0, None));
    assert_eq!(w.aggregate(&Aggregate::Sum), None);
}

/// §2.2 for an observed interval series: `t + d` stands for `t` in `future`,
/// in the second clause of `late`, and in live state and windows (a reading
/// enters them only once `t + d ≤ now`).
#[test]
fn s2_2_observed_interval_reading_ready_at_its_end() {
    let (mut m, s) = interval_model(30 * MINUTE, 0);
    let start = floor_to(m.wall, 30 * MINUTE);
    m.ingest(vec![num(s, start - 30 * MINUTE, 1.0), num(s, start, 2.0)])
        .unwrap();
    let f = m.current().unwrap()[&(s, start)].clone();
    assert!(f.flags.future, "its end is more than 60 s ahead");
    let f = m.current().unwrap()[&(s, start - 30 * MINUTE)].clone();
    assert!(!f.flags.late, "30 minutes old by t, on time by t + d");
    assert_eq!(m.latest(s).unwrap().unwrap().t, start - 30 * MINUTE);
    assert_eq!(m.window(s, start - HOUR, start + HOUR).unwrap().count, 1);
    m.advance(30 * MINUTE);
    assert_eq!(m.latest(s).unwrap().unwrap().t, start);
    assert_eq!(m.window(s, start - HOUR, start + HOUR).unwrap().count, 2);
    // Late by the bound measured from the interval's end.
    let old = start - HOUR;
    let p = m.ingest(vec![num(s, old, 3.0)]).unwrap().positions[0];
    // The first clause of `late` (2 × S, S = 3 × d) applies as well: a
    // series with a lateness bound of a day shows it alone.
    let (mut m2, _) = model();
    let e = m2
        .define_with(
            "site/meter-2/energy",
            Kind::Interval,
            ValueType::Number,
            "kWh",
            None,
            SeriesOptions {
                lateness_bound: Some(DAY),
                ..SeriesOptions::interval(30 * MINUTE, 0)
            },
        )
        .unwrap();
    let now2 = floor_to(m2.wall, 30 * MINUTE);
    m2.ingest(vec![num(e, now2 - 30 * MINUTE, 1.0)]).unwrap();
    let ps = m2
        .ingest(vec![
            num(e, now2 - 3 * HOUR, 1.0),
            num(e, now2 - 4 * HOUR, 1.0),
        ])
        .unwrap()
        .positions;
    let flags: Vec<bool> = ps
        .iter()
        .map(|p| {
            m2.facts()
                .unwrap()
                .into_iter()
                .find(|f| f.position == *p)
                .unwrap()
                .flags
                .late
        })
        .collect();
    assert_eq!(
        flags,
        vec![false, true],
        "earlier than the latest t by more than 3 h"
    );
    let late = |m: &Model, p| {
        m.facts()
            .unwrap()
            .into_iter()
            .find(|f| f.position == p)
            .unwrap()
            .flags
            .late
    };
    assert!(late(&m, p));
}

/// Row 20a for an interval series with d = 30 min: no stored level, so no
/// calendar node; a day's result is a window over its facts.
#[test]
fn row_20a_interval_series_of_30_minutes_has_no_calendar_node() {
    let tz = TimeZone::get("Asia/Kathmandu").unwrap();
    let date = Date::constant(2026, 6, 1);
    let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
    let p = Params::interval(30 * MINUTE, 0);
    let first = floor_to(ds, 30 * MINUTE) - 30 * MINUTE;
    let rs: Vec<(Nanos, Value)> = (0..52)
        .map(|k| (first + k * 30 * MINUTE, Value::Num(1.0)))
        .collect();
    let tree = SeriesTree::build(p, &rs);
    assert!(!tree.stores(0) && !tree.stores(1));
    assert_eq!(tree.day(&tz, date), Err(TreeError::NoCalendarNode));
    // The day starts at 18:15 UTC, inside an interval: apportioned.
    let w = tree.window(ds, de, de + DAY);
    assert_eq!(w, oracle::window(&p, &rs, ds, de, de + DAY));
    assert!(w.apportioned());
    assert_eq!(
        w.aggregate(&Aggregate::Sum),
        Some(AggValue::Exact(exact(48.0)))
    );
}

/// Row 3: "Reading for last week arrives now: flagged `late`; minute path
/// updated; no retroactive trigger."
#[test]
fn row_03_late_reading_flagged_tree_updated_no_retroactive_trigger() {
    let (mut m, s) = model();
    let now = m.wall;
    m.ingest(vec![num(s, now - SECOND, 10.0)]).unwrap();
    let mut runner = Runner::new(
        Condition::Threshold {
            on: 50.0,
            clear: 40.0,
        },
        m.mono,
    );
    let latest = |m: &Model| m.latest(s).unwrap().and_then(|f| f.value.as_f64());
    assert!(runner.on_value(m.mono, latest(&m)).is_empty());
    let last_week = now - 7 * DAY;
    let p = m.ingest(vec![num(s, last_week, 999.0)]).unwrap().positions[0];
    let f = m
        .facts()
        .unwrap()
        .into_iter()
        .find(|f| f.position == p)
        .unwrap();
    assert!(f.flags.late);
    leaf_ok(&m, s, last_week);
    // The latest value is unchanged, so the condition does not fire.
    assert!(runner.on_value(m.mono, latest(&m)).is_empty());
    m.drain().unwrap();
    assert!(delivered(&m).contains(&p), "a late fact is still a fact");
}

/// Row 4: "Reading > 60 s in the future, on an observed series: recorded,
/// `future`, suspect; ignored by live state, staleness, windows until due."
#[test]
fn row_04_future_reading_recorded_suspect_excluded_until_its_time() {
    let (mut m, s) = model();
    let now = m.wall;
    let t = now + 61 * SECOND;
    m.ingest(vec![num(s, now - 2 * SECOND, 1.0), num(s, t, 5.0)])
        .unwrap();
    let f = m.current().unwrap()[&(s, t)].clone();
    assert!(f.flags.future);
    assert_eq!(f.quality, Quality::Suspect);
    // Excluded from windows and live state until its time comes.
    assert_eq!(
        m.window(s, now - MINUTE, now + 2 * MINUTE).unwrap().count,
        1
    );
    assert_eq!(m.latest(s).unwrap().unwrap().value, Value::Num(1.0));
    m.advance(2 * MINUTE);
    assert_eq!(
        m.window(s, now - MINUTE, now + 2 * MINUTE).unwrap().count,
        2
    );
    assert_eq!(m.latest(s).unwrap().unwrap().value, Value::Num(5.0));
    // Exactly 60 s ahead is not future (the rule says "> 60 s").
    let now = m.wall;
    m.ingest(vec![num(s, now + 60 * SECOND, 1.0)]).unwrap();
    assert!(!m.current().unwrap()[&(s, now + 60 * SECOND)].flags.future);
}

/// Row 5: "Device clock stuck or far in the past: late data; adapter health
/// reports skew when `received_at − t` is persistently large."
#[test]
fn row_05_stuck_or_past_clock_is_late_and_reported() {
    let (mut m, s) = model();
    for i in 0..3 {
        let t = m.wall - DAY + i * SECOND;
        m.ingest(vec![num(s, t, 1.0)]).unwrap();
        m.advance(SECOND);
    }
    assert!(m.facts().unwrap().iter().all(|f| f.flags.late));
    let h = m.health();
    assert_eq!(h.level, Level::Degraded);
    assert!(h.reasons.contains(&Reason::ClockSkew(s)));
    // A stuck clock: the same t again with a new value is a correction, not lost.
    let t = m.wall - DAY;
    m.ingest(vec![num(s, t, 2.0)]).unwrap();
    assert_eq!(m.current().unwrap()[&(s, t)].value, Value::Num(2.0));
    // A sane reading clears the report.
    let now = m.wall;
    m.ingest(vec![num(s, now, 1.0)]).unwrap();
    assert_eq!(m.health().level, Level::Ok);
}

/// Row 6: "Two readings in one second: both facts; the minute node folds both."
#[test]
fn row_06_two_readings_in_one_second() {
    let (mut m, s) = model();
    let t = T0 + 30 * SECOND;
    m.ingest(vec![num(s, t, 1.0), num(s, t + SECOND / 2, 3.0)])
        .unwrap();
    assert_eq!(m.facts().unwrap().len(), 2);
    let leaf = m.window(s, t, t + SECOND).unwrap();
    assert_eq!(leaf.count, 2);
    assert_eq!(leaf.coverage, i128::from(SECOND));
    assert_eq!(
        leaf.aggregate(&Aggregate::Avg),
        Some(AggValue::Exact(exact(2.0)))
    );
    leaf_ok(&m, s, t);
}

/// Row 7: "Process crash after write, before sync. Same boot: truncated to
/// the durable mark on restart; never delivered." With the mark lost
/// (row 7, second clause) recovery treats it as a different boot and may keep
/// the batch: `s3_missing_mark_counts_as_different_boot`. The model's mark is
/// exact and `possible_loss` is not reported (see `OPEN-QUESTIONS.md`).
#[test]
fn row_07_process_crash_after_write_before_sync() {
    // Same boot: recovery truncates to the durable mark, whatever reached the
    // page cache, and a batch synced but not yet covered by the mark; the
    // batch is never delivered.
    for point in [
        CrashPoint::AfterWrite(WriteOutcome::Lost),
        CrashPoint::AfterWrite(WriteOutcome::Torn),
        CrashPoint::AfterWrite(WriteOutcome::Survived),
        CrashPoint::AfterSyncBeforeMark,
    ] {
        let (mut m, s) = model();
        m.emit(num(s, T0, 1.0)).unwrap();
        let mark = m.disk.mark;
        let r = m.commit(Some(Fault::Crash(point)));
        assert_eq!(m.disk.mark, mark, "mark not moved");
        // A reader while the writer is down sees nothing past the mark.
        assert!(m.read(&m.open_reader()).unwrap().1.is_empty());
        assert_eq!(r, Err(Error::Crashed));
        m.restart();
        assert!(m.facts().unwrap().is_empty());
        assert_eq!(m.counters.batches_discarded, 1);
        m.drain().unwrap();
        assert!(delivered(&m).is_empty());
    }
}

/// Row 7a: "Power loss after write, before sync. New boot: torn tail truncated
/// by checksum; complete batches synced on recovery before use." The `resync`
/// owed after the recovery is
/// `s4_resync_after_rule_3_recovery_before_later_positions`.
#[test]
fn row_07a_power_loss_after_write_before_sync() {
    // New boot: a torn tail is truncated; a complete batch with valid
    // checksums is kept and, being durable, delivered after recovery.
    for (outcome, kept) in [
        (WriteOutcome::Lost, false),
        (WriteOutcome::Torn, false),
        (WriteOutcome::Survived, true),
    ] {
        let (mut m, s) = model();
        m.emit(num(s, T0, 1.0)).unwrap();
        assert_eq!(
            m.commit(Some(Fault::PowerLoss(outcome))),
            Err(Error::Crashed)
        );
        assert!(
            delivered(&m).is_empty(),
            "nothing delivered before the crash"
        );
        m.restart();
        assert_eq!(!m.facts().unwrap().is_empty(), kept);
        assert_eq!(m.disk.mark.unwrap().boot, m.boot, "recovery moved the mark");
        m.drain().unwrap();
        assert_eq!(delivered(&m).len(), usize::from(kept));
    }
    // A power loss with nothing past the mark loses nothing.
    let (mut m, s) = model();
    m.ingest(vec![num(s, T0, 1.0)]).unwrap();
    m.power_loss();
    m.restart();
    assert_eq!(m.facts().unwrap().len(), 1);
    assert_eq!(m.counters.batches_discarded, 0);
}

/// Row 8: "Crash after sync, before delivery: delivered on restart from each
/// consumer's cursor."
#[test]
fn row_08_crash_after_sync_before_delivery() {
    let (mut m, s) = model();
    m.emit(num(s, T0, 1.0)).unwrap();
    assert_eq!(
        m.commit(Some(Fault::Crash(CrashPoint::AfterSync))),
        Err(Error::Crashed)
    );
    m.restart();
    m.drain().unwrap();
    let obs = &m.observed[&C];
    assert_eq!(obs.len(), 1);
    assert!(obs[0].replayed);
}

/// Row 8: a crash during delivery comes after the durable mark: a consumer never sees
/// a batch that a restart on the same boot could truncate.
#[test]
fn row_08_crash_during_delivery_after_mark() {
    let (mut m, s) = model();
    m.emit(num(s, T0, 1.0)).unwrap();
    assert_eq!(
        m.commit(Some(Fault::Crash(CrashPoint::DuringDelivery(1)))),
        Err(Error::Crashed)
    );
    let last = *m.disk.all_entries().keys().next_back().unwrap();
    let mark = m.disk.mark.unwrap();
    assert_eq!((mark.boot, mark.last), (m.boot, last));
    m.restart();
    m.drain().unwrap();
    let obs = &m.observed[&C];
    assert_eq!(
        (obs.len(), obs[0].position, obs[0].replayed),
        (1, last, true)
    );
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

/// Row 9: a failed recovery sync leaves the engine read-only before it writes
/// anything: unpublished compaction outputs stay until a later recovery.
#[test]
fn row_09_failed_recovery_sync_discards_nothing() {
    let (mut m, _) = sealed_model();
    assert_eq!(
        m.compact(Some(CompactionCrash::AfterWrite)),
        Err(Error::Crashed)
    );
    let hot = m.disk.hot.clone();
    m.power_loss();
    m.restart_with(true);
    assert!(m.is_read_only());
    assert_eq!((m.counters.compaction_discarded, &m.disk.hot), (0, &hot));
    m.restart();
    assert!(!m.is_read_only());
    // A current and a revision chunk for the series, and one for the
    // registry entry.
    assert_eq!(m.counters.compaction_discarded, 3);
    // No segment is open, yet recovery stamped the mark with the new boot.
    assert_eq!(m.disk.mark.unwrap().boot, m.boot);
}

/// Row 9: "Crash during compaction: output unpublished and discarded; inputs
/// intact."
#[test]
fn row_09_crash_during_compaction() {
    let (mut m, _) = sealed_model();
    let before = durable(&m);
    let inputs = m.disk.listed_at(m.open_reader().epoch);
    assert_eq!(
        m.compact(Some(CompactionCrash::AfterWrite)),
        Err(Error::Crashed)
    );
    m.restart();
    assert_eq!(m.counters.compaction_discarded, 3);
    assert_eq!(m.disk.listed_at(m.open_reader().epoch), inputs);
    assert_eq!(durable(&m), before);
}

/// Five compactions of one series in one 15-minute span: their current
/// chunks are merged (§7 Policy). Returns the model and those chunks.
fn crowded_model() -> (Model, Vec<FileId>) {
    let (mut m, s) = model();
    let mut chunks = Vec::new();
    for i in 0..5 {
        m.ingest(vec![num(s, T0 + i * SECOND, i as f64)]).unwrap();
        m.seal_active();
        chunks.extend(m.compact(None).unwrap().into_iter().filter(|f| {
            m.disk.files[f].class == FileClass::Current
                && m.disk.files[f].entries().any(|e| e.fact().is_some())
        }));
    }
    (m, chunks)
}

/// Row 10: "Crash around the manifest record: before: output discarded.
/// After: inputs retired. Never both live." Compaction of compacted files
/// publishes its outputs and retires its inputs in one record (§7).
#[test]
fn row_10_crash_around_the_manifest_record() {
    let live = |m: &Model| m.disk.listed_at(m.disk.latest_epoch());
    let (mut m, inputs) = crowded_model();
    let before = durable(&m);
    let listed = live(&m);
    assert_eq!(
        m.merge_chunks(Some(CompactionCrash::AfterWrite)),
        Err(Error::Crashed)
    );
    m.restart();
    assert_eq!(live(&m), listed, "outputs discarded, inputs live");
    assert_eq!(m.counters.compaction_discarded, 1);
    assert_eq!(durable(&m), before);
    let (mut m, inputs2) = crowded_model();
    assert_eq!(inputs, inputs2);
    assert_eq!(
        m.merge_chunks(Some(CompactionCrash::AfterPublish)),
        Err(Error::Crashed)
    );
    m.restart();
    let now = live(&m);
    assert!(inputs.iter().all(|f| !now.contains(f)), "inputs retired");
    assert!(m.disk.retired().keys().copied().collect::<Vec<_>>() == inputs);
    assert_eq!(durable(&m), before);
    assert_eq!(
        m.merge_chunks(None).unwrap(),
        Vec::new(),
        "not merged twice"
    );
}

/// Row 11: "Torn or corrupt block in a sealed file: quarantine; fetch from
/// cold/deep; verify, swap by manifest; errors meanwhile." Never repaired in
/// place. A log segment with no other copy is
/// `s8_corrupt_log_segment_with_no_other_copy_loses_range`.
#[test]
fn row_11_corrupt_sealed_file_quarantined_and_restored_from_tier() {
    let (mut m, _) = sealed_model();
    let before = durable(&m);
    m.upload();
    let f = *m.disk.hot.iter().next().unwrap();
    let content = m.disk.files[&f].clone();
    m.corrupt_hot(f);
    assert_eq!(m.scrub(), vec![f]);
    let h = m.health();
    assert_eq!(h.level, Level::Fatal);
    assert!(h.reasons.contains(&Reason::Quarantined(f)));
    assert!(m.restore_file(f));
    assert_eq!(m.disk.files[&f], content, "never repaired in place");
    assert_eq!(m.health().level, Level::Ok);
    assert_eq!(durable(&m), before);
}

/// Two consumers of every series and a gauge; four batches of two facts
/// each in the open segment. Returns the model, the series and the positions
/// of each batch.
fn open_segment_model() -> (Model, SeriesId, Vec<Vec<Position>>) {
    let consumers = [ConsumerId(1), ConsumerId(2)].map(|id| ConsumerDecl {
        id,
        selection: Selection::All,
        capacity: 100,
    });
    let mut m = Model::new(Config::default(), consumers.to_vec(), T0 + HOUR);
    let s = m
        .define_with(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    let batches = (0..4)
        .map(|i| {
            let t = T0 + i * SECOND;
            m.ingest(vec![num(s, t, 1.0), num(s, t + SECOND / 2, 2.0)])
                .unwrap()
                .positions
        })
        .collect();
    (m, s, batches)
}

/// Index in the open segment of the batch holding `p`.
fn batch_holding(m: &Model, p: Position) -> usize {
    let f = m.disk.active.unwrap();
    m.disk.files[&f]
        .batches
        .iter()
        .position(|b| b.iter().any(|e| e.position == p))
        .unwrap()
}

/// Takes and confirms deliveries of consumer `c` up to `upto`.
fn confirm_upto(m: &mut Model, c: ConsumerId, upto: Position) {
    for d in m.poll(c, 100).unwrap() {
        if d.position <= upto {
            m.confirm(c, d.position).unwrap();
        }
    }
}

/// Asserts what consumer `c` receives in session `session` after a lost
/// range: the kept facts above `cursor`, none in the range, then `resync`.
fn assert_resumed(m: &Model, c: ConsumerId, session: u32, cursor: Position, kept: &[Position]) {
    let stream = session_stream(m, c, session);
    let (last, facts) = stream.split_last().unwrap();
    let got: Vec<Position> = facts.iter().map(|d| d.fact().unwrap().position).collect();
    let want: Vec<Position> = kept.iter().copied().filter(|p| *p > cursor).collect();
    assert_eq!(got, want, "consumer {}", c.0);
    assert!(last.resync().is_some(), "consumer {} ends with resync", c.0);
}

/// Row 11a: "Corrupt block inside the open segment, or below an old boot's
/// mark after a power loss: later complete batches kept; range recorded as
/// lost, `data_lost`; positions not reused; `resync` for cursors below it."
/// Same boot: the second of four batches of the open segment, below the
/// mark, is damaged while the engine is down. Recovery keeps the later
/// batches, records the range from the position after the first batch to the
/// position before the third, reports `data_lost` without going read-only,
/// and continues above the last batch. A consumer whose cursor is below the
/// range and one whose cursor is inside it receive the kept facts after their
/// cursor, never a lost position, then `resync`. The report stays after
/// another restart, read from the manifest (pins the model's choice for
/// Q-34 (c)).
#[test]
fn row_11a_corrupt_block_inside_the_open_segment() {
    let (mut m, s, b) = open_segment_model();
    let (below, inside) = (ConsumerId(1), ConsumerId(2));
    let below_cursor = b[0][1];
    let inside_cursor = b[1][0];
    confirm_upto(&mut m, below, below_cursor);
    confirm_upto(&mut m, inside, inside_cursor);
    m.crash();
    let damaged = batch_holding(&m, b[1][0]);
    m.corrupt_open_batch(damaged).unwrap();
    m.restart();
    let lost = (b[1][0], b[1][1]);
    assert_eq!(m.disk.lost_ranges(), vec![lost]);
    let kept: Vec<Position> = [&b[0], &b[2], &b[3]]
        .into_iter()
        .flatten()
        .copied()
        .collect();
    let facts: Vec<Position> = m.facts().unwrap().iter().map(|f| f.position).collect();
    assert_eq!(facts, kept, "later complete batches kept");
    let h = m.health();
    assert_eq!(h.level, Level::Degraded);
    assert!(h.reasons.contains(&Reason::DataLost(lost.0, lost.1)));
    assert!(!m.is_read_only());
    m.drain().unwrap();
    assert_resumed(&m, below, 2, below_cursor, &kept);
    assert_resumed(&m, inside, 2, inside_cursor, &kept);
    let next = m.ingest(vec![num(s, T0 + MINUTE, 3.0)]).unwrap().positions[0];
    assert_eq!(next, b[3][1].next());
    m.restart();
    assert!(
        m.health()
            .reasons
            .contains(&Reason::DataLost(lost.0, lost.1))
    );
    assert_eq!(m.disk.lost_ranges(), vec![lost], "recorded once");
}

/// Row 11a after a power loss: the last batch below the old boot's mark is
/// damaged, with no complete batch after it, so the range runs to the mark's
/// last position (§3 Recovery rule 2), and new positions continue above it:
/// treated as a torn tail (rule 3), the range would be truncated and its
/// positions issued again. A consumer whose cursor is below it receives
/// `resync`, and no lost position.
#[test]
fn row_11a_corrupt_block_below_an_old_boots_mark() {
    let (mut m, s, b) = open_segment_model();
    let c = ConsumerId(1);
    confirm_upto(&mut m, c, b[2][1]);
    let mark = m.disk.mark.unwrap();
    assert_eq!(mark.last, b[3][1]);
    m.power_loss();
    let damaged = batch_holding(&m, b[3][0]);
    m.corrupt_open_batch(damaged).unwrap();
    m.restart();
    assert_ne!(m.disk.mark.unwrap().boot, mark.boot, "a new boot");
    assert_eq!(m.disk.lost_ranges(), vec![(b[3][0], mark.last)]);
    assert!(
        m.health()
            .reasons
            .contains(&Reason::DataLost(b[3][0], mark.last))
    );
    m.drain().unwrap();
    let kept: Vec<Position> = b[..3].iter().flatten().copied().collect();
    assert_resumed(&m, c, 2, b[2][1], &kept);
    let next = m.ingest(vec![num(s, T0 + MINUTE, 3.0)]).unwrap().positions[0];
    assert_eq!(next, mark.last.next(), "positions not reused");
    // Across a later restart too.
    m.crash();
    m.restart();
    let after = m
        .ingest(vec![num(s, T0 + 2 * MINUTE, 4.0)])
        .unwrap()
        .positions[0];
    assert_eq!(after, next.next());
}

/// Row 11a with a failed recovery sync (§3 Recovery rule 4): after a power
/// loss, the recovery that finds a damaged batch cannot sync, so the engine
/// stays read-only. Whether the lost range is recorded and `data_lost`
/// reported then is Q-79; the model records nothing until a recovery that can
/// write, even if the read-only engine sealed the segment meanwhile, and then
/// records the range once and continues above it.
#[test]
fn row_11a_failed_recovery_sync_records_the_range_later() {
    let (mut m, s, b) = open_segment_model();
    m.crash();
    let damaged = batch_holding(&m, b[3][0]);
    m.corrupt_open_batch(damaged).unwrap();
    m.power_loss();
    m.restart_with(true);
    assert!(m.is_read_only());
    assert!(
        m.disk.lost_ranges().is_empty(),
        "nothing recorded while read-only"
    );
    assert!(
        !m.health()
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::DataLost(..)))
    );
    m.seal_active();
    assert!(
        m.disk.active.is_none(),
        "the read-only engine sealed the segment"
    );
    m.restart();
    assert!(!m.is_read_only());
    assert_eq!(m.disk.lost_ranges(), vec![(b[3][0], b[3][1])]);
    m.restart();
    assert_eq!(m.disk.lost_ranges().len(), 1, "recorded once");
    let next = m.ingest(vec![num(s, T0 + MINUTE, 3.0)]).unwrap().positions[0];
    assert_eq!(next, b[3][1].next(), "positions not reused");
}

/// Row 11a in the model: damage to a batch holding a definition is refused,
/// since what a lost registry, routing or zone entry means is open (Q-33);
/// so is damage while the engine runs (the model finds corruption of the open
/// segment at recovery only) or past the mark.
#[test]
fn row_11a_fault_injection_limits() {
    let (mut m, _, b) = open_segment_model();
    assert!(
        matches!(m.corrupt_open_batch(1), Err(Error::BadFault(_))),
        "engine up"
    );
    m.crash();
    let definition = batch_holding(&m, Position::new(0, 1));
    assert!(matches!(
        m.corrupt_open_batch(definition),
        Err(Error::BadFault(_))
    ));
    let past_mark = batch_holding(&m, b[3][0]) + 1;
    assert!(matches!(
        m.corrupt_open_batch(past_mark),
        Err(Error::BadFault(_))
    ));
    m.restart();
    assert!(m.disk.lost_ranges().is_empty());
}

/// Row 12: "Sync returns an error: read-only, fatal; a restart on the same boot
/// truncates to the durable mark."
#[test]
fn row_12_sync_error_read_only_fatal_restart_truncates() {
    for outcome in [
        WriteOutcome::Lost,
        WriteOutcome::Torn,
        WriteOutcome::Survived,
    ] {
        let (mut m, s) = model();
        m.emit(num(s, T0, 1.0)).unwrap();
        assert_eq!(
            m.commit(Some(Fault::SyncError(outcome))),
            Err(Error::SyncFailed)
        );
        assert_eq!(m.health().level, Level::Fatal);
        assert_eq!(m.emit(num(s, T0 + 1, 1.0)), Err(Refusal::ReadOnly.into()));
        assert_eq!(m.commit(None), Err(Error::ReadOnly), "no retry");
        assert!(delivered(&m).is_empty());
        assert!(m.read(&m.open_reader()).unwrap().1.is_empty());
        m.restart();
        assert!(
            m.facts().unwrap().is_empty(),
            "truncated to the durable mark"
        );
        assert_eq!(m.health().level, Level::Ok);
        assert!(m.ingest(vec![num(s, T0 + 2, 1.0)]).is_ok());
    }
}

/// Row 13: "Disk fills: degrade, then ingest waits; compaction ENOSPC abandons
/// the job; nothing deleted." Here compaction pauses at the low mark before any
/// ENOSPC.
#[test]
fn row_13_disk_fills_degrade_then_ingest_waits_nothing_deleted() {
    let cfg = Config {
        disk_bytes: 100_000,
        low_mark: 20_000,
        critical_mark: 5_000,
        ..Config::default()
    };
    let (mut m, s) = model_with(cfg);
    m.ingest(vec![num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    let before = durable(&m);
    m.disk.other_bytes = 85_000;
    assert!(m.health().reasons.contains(&Reason::LowDisk));
    assert_eq!(m.compact(None), Err(Error::Paused));
    // Tiering and eviction, which free space, continue (§3 Disk space).
    assert!(m.housekeeping().is_ok());
    m.disk.other_bytes = 96_000;
    assert!(m.health().reasons.contains(&Reason::CriticalDisk));
    // Ingest waits: the reading is held back, not refused, and the wait is
    // counted.
    let waiting = num(s, T0 + 1, 2.0);
    assert_eq!(m.emit(waiting.clone()), Err(Wait::DiskFull.into()));
    assert_eq!(m.counters.waits[&Wait::DiskFull], 1);
    assert!(m.counters.refused.is_empty());
    assert_eq!(m.queued(), 0);
    // Reads continue; nothing deleted.
    assert_eq!(m.window(s, T0, T0 + MINUTE).unwrap().count, 1);
    assert_eq!(durable(&m), before);
    // Once space is free, the same reading goes through and is recorded.
    m.disk.other_bytes = 0;
    m.emit(waiting).unwrap();
    m.commit(None).unwrap();
    assert_eq!(m.window(s, T0, T0 + MINUTE).unwrap().count, 2);
}

/// Row 14: "Consumer slower than the stream: catch-up from the log after the
/// last position enqueued, hand-over to live without gap or repeat."
#[test]
fn row_14_consumer_slower_than_stream_catches_up_from_log() {
    let slow = ConsumerDecl {
        id: ConsumerId(1),
        selection: Selection::All,
        capacity: 2,
    };
    let fast = ConsumerDecl {
        id: ConsumerId(2),
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![slow, fast], T0 + HOUR);
    let s = m
        .define_with(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    for i in 0..10 {
        m.ingest(vec![num(s, T0 + i * SECOND, i as f64)]).unwrap();
    }
    assert!(m.in_catch_up(ConsumerId(1)));
    assert!(
        !m.in_catch_up(ConsumerId(2)),
        "never blocks other consumers"
    );
    assert_eq!(m.poll(ConsumerId(2), 100).unwrap().len(), 10);
    let mut got = Vec::new();
    loop {
        m.catch_up(ConsumerId(1), 1).unwrap();
        let d = m.poll(ConsumerId(1), 1).unwrap();
        if d.is_empty() && !m.in_catch_up(ConsumerId(1)) {
            break;
        }
        got.extend(d);
    }
    let ps = seqs(got.iter().map(|d| d.position));
    assert!(ps.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(ps.len(), 10);
    assert!(got[..2].iter().all(|d| !d.replayed));
    assert!(got[2..].iter().all(|d| d.replayed));
    assert_eq!(m.counters.overflows, 1);
}

/// Row 15: consumer down for a day — "Resumes from cursor; deliveries
/// `replayed`; then `resync`. Cursor older than the oldest local log
/// segment: `resync` only".
#[test]
fn row_15_consumer_down_for_a_day() {
    let (mut m, s) = model();
    m.ingest(vec![num(s, T0, 0.0)]).unwrap();
    m.drain().unwrap();
    let cursor = m.disk.cursors[&C];
    for i in 1..=24 {
        m.advance(HOUR);
        let now = m.wall;
        m.ingest(vec![num(s, now, f64::from(i))]).unwrap();
    }
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    assert_eq!(stream.len(), 25);
    assert_eq!(stream[0].position, cursor.next());
    assert!(
        stream[..24]
            .iter()
            .all(|d| d.replayed && d.fact().is_some())
    );
    assert!(stream[24].resync().is_some_and(|r| r.replaced.is_none()));
    // Down for 91 days, with the log of that time uploaded and gone from
    // the fast disk: `resync` only.
    let cursor = m.disk.cursors[&C];
    for _ in 0..3 {
        m.advance(DAY);
        let now = m.wall;
        m.ingest(vec![num(s, now, 1.0)]).unwrap();
    }
    m.seal_active();
    m.upload();
    m.advance(91 * DAY);
    m.housekeeping().unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 3);
    assert_eq!(stream.len(), 1);
    let head = m.entries().unwrap().last().unwrap().position;
    assert_eq!(stream[0].resync().unwrap().replaced, Some((cursor, head)));
}

/// Row 16: consumer restarts; a condition became true while it was down —
/// "Operator rebuilt as of now, not replayed; `resync` differs from the latch
/// → acted on (equal → no new transition)".
#[test]
fn row_16_condition_true_while_consumer_down() {
    let (mut m, s) = model();
    let f = FunnelId(1);
    let cond = Condition::Threshold {
        on: 50.0,
        clear: 40.0,
    };
    m.add_route(C, f, Funnel::condition(s, None, cond)).unwrap();
    for v in [90.0, 10.0] {
        m.advance(SECOND);
        let now = m.wall;
        m.ingest(vec![num(s, now, v)]).unwrap();
    }
    m.drain().unwrap();
    assert_eq!(m.disk.latches[&(C, f)].state, ConditionState::Active(false));
    // Down: the condition becomes true; the consumer never takes it.
    m.advance(SECOND);
    let now = m.wall;
    m.ingest(vec![num(s, now, 90.0)]).unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    // The fact is replayed, never fed to the condition; then `resync`.
    assert_eq!(stream.len(), 2);
    assert!(stream[0].replayed && stream[0].fact().is_some());
    let cond_now = |d: &kvik_model::engine::Envelope| {
        let c = d
            .resync()
            .unwrap()
            .funnels
            .iter()
            .find(|x| x.funnel == f)
            .unwrap();
        let c = c.condition.clone().unwrap();
        (c.state, c.transition)
    };
    assert_eq!(cond_now(&stream[1]), (ConditionState::Active(true), true));
    // Restarting again with the latch now equal: no new transition.
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 3);
    assert_eq!(cond_now(&stream[0]), (ConditionState::Active(true), false));
    // Live transitions resume after the `resync`.
    m.advance(SECOND);
    let now = m.wall;
    m.ingest(vec![num(s, now, 10.0)]).unwrap();
    m.drain().unwrap();
    let t = session_stream(&m, C, 3)
        .iter()
        .find_map(|d| d.trigger().cloned())
        .unwrap();
    assert_eq!(
        (t.cause, t.state),
        (TriggerCause::Fact, ConditionState::Active(false))
    );
}

/// Row 34: route added while a consumer is catching up — "Applies from its
/// position only".
#[test]
fn row_34_route_added_while_catching_up() {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(Default::default()),
        capacity: 1,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let a = m
        .define_with(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    let b = m
        .define_with(
            "vehicle/7/rpm",
            Kind::Gauge,
            ValueType::Number,
            "1/min",
            SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    m.add_route(C, FunnelId(1), Funnel::facts(Selection::Series([a].into())))
        .unwrap();
    let mut a_facts = Vec::new();
    for i in 0..3 {
        a_facts.extend(
            m.ingest(vec![num(a, T0 + i * SECOND, 1.0)])
                .unwrap()
                .positions,
        );
    }
    assert!(m.in_catch_up(C));
    let before = m.ingest(vec![num(b, T0, 1.0)]).unwrap().positions[0];
    let route = m
        .add_route(C, FunnelId(2), Funnel::facts(Selection::Series([b].into())))
        .unwrap();
    let after = m.ingest(vec![num(b, T0 + SECOND, 2.0)]).unwrap().positions[0];
    assert!(before < route && route < after);
    m.drain().unwrap();
    let got: Vec<Position> = m.observed[&C].iter().map(|o| o.position).collect();
    let mut want = a_facts;
    want.push(after);
    assert_eq!(got, want);
}

/// Row 33: "Consumer emits readings from a delivery: non-blocking emit off the
/// writer thread; cyclic routes rejected." A consumer that takes a delivery
/// emits a derived reading: the emit returns at once, enters the next batch
/// and waits only as back-pressure (`Wait::QueueFull`), never for a commit;
/// nothing is delivered until that batch commits, and then at a later position.
/// The model is synchronous, so "off the writer thread" is shown by the engine
/// (roadmap 5.2). Rejecting a cycle is Q-78: nothing tells the engine which
/// series a consumer emits, and here a route from the emitted series back to
/// the consumer is accepted; each emitted reading reaches it once.
#[test]
fn row_33_consumer_emits_readings_from_a_delivery() {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(Default::default()),
        capacity: 100,
    };
    let cfg = Config {
        ingest_capacity: 2,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![consumer], T0 + HOUR);
    let def = |m: &mut Model, name: &str| {
        m.define_with(
            name,
            Kind::Gauge,
            ValueType::Number,
            "W",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap()
    };
    let raw = def(&mut m, "plant/line-1/power");
    let derived = def(&mut m, "plant/line-1/power-smoothed");
    m.add_route(
        C,
        FunnelId(1),
        Funnel::facts(Selection::Series([raw].into())),
    )
    .unwrap();
    // A route from the series the consumer emits back to it (Q-78).
    m.add_route(
        C,
        FunnelId(2),
        Funnel::facts(Selection::Series([derived].into())),
    )
    .unwrap();
    let now = m.wall;
    let p_raw = m.ingest(vec![num(raw, now, 10.0)]).unwrap().positions[0];
    let mut seen = Vec::new();
    let mut emitted = 0;
    for _ in 0..4 {
        for d in m.poll(C, 10).unwrap() {
            seen.push(d.position);
            m.confirm(C, d.position).unwrap();
            if d.fact.series == raw {
                // Emits from the delivery; the emit does not commit.
                let before = m.disk.all_entries().len();
                m.emit(num(derived, d.fact.t, 5.0)).unwrap();
                emitted += 1;
                assert_eq!(
                    m.disk.all_entries().len(),
                    before,
                    "no commit inside the emit"
                );
                assert!(
                    m.poll(C, 10).unwrap().is_empty(),
                    "nothing delivered before the commit"
                );
            }
        }
        if m.queued() > 0 {
            m.commit(None).unwrap();
        }
    }
    assert_eq!(emitted, 1);
    let p_derived = m.current().unwrap()[&(derived, now)].position;
    assert!(p_raw < p_derived);
    assert_eq!(
        seen,
        vec![p_raw, p_derived],
        "each emitted reading delivered once"
    );
    // A full ingest queue makes the emit return back-pressure, not wait.
    m.emit(num(derived, now + SECOND, 1.0)).unwrap();
    m.emit(num(derived, now + 2 * SECOND, 1.0)).unwrap();
    assert_eq!(
        m.emit(num(derived, now + 3 * SECOND, 1.0)),
        Err(Wait::QueueFull.into())
    );
    assert_eq!(m.counters.waits[&Wait::QueueFull], 1);
}

/// Row 17: "Series goes silent: stale after S; coverage stops; stale-for-N
/// funnels fire."
#[test]
fn row_17_series_goes_silent() {
    let (mut m, s) = model(); // staleness 10 s
    let t = floor_to(m.wall, SECOND) - 30 * SECOND;
    m.ingest(vec![num(s, t, 1.0)]).unwrap();
    let w = m.window(s, t, t + 30 * SECOND).unwrap();
    assert_eq!(w.coverage, i128::from(10 * SECOND), "coverage stops");
    let mut stale = Runner::new(
        Condition::StaleFor {
            staleness: 10 * SECOND,
            n: 5 * SECOND,
        },
        m.mono,
    );
    stale.on_arrival(m.mono);
    m.advance(14 * SECOND);
    assert!(stale.on_tick(m.mono).is_empty());
    m.advance(SECOND);
    assert_eq!(stale.on_tick(m.mono), vec![Event::Fired]);
    assert_eq!(stale.on_arrival(m.mono), vec![Event::Cleared]);
}

/// Row 17a: "Record-on-change series, value unchanged: keep-alive fact each
/// expected interval; never stale while healthy." The series is declared
/// record on change with a keep-alive interval of 20 s as its expected
/// interval (§2.6), so S = 60 s; its source polls every 2 s. Whether the
/// engine or the adapter leaves out the unchanged polls is Q-73; here the
/// adapter emits only changes and keep-alive facts.
#[test]
fn row_17a_record_on_change_keep_alive_never_stale() {
    let (mut m, _) = model();
    let s = m
        .define_with(
            "plant/line-1/valve",
            Kind::Gauge,
            ValueType::Number,
            "%",
            None,
            SeriesOptions {
                record_on_change: true,
                ..SeriesOptions::expected(20 * SECOND)
            },
        )
        .unwrap();
    let d = m.registry().unwrap().get(s).unwrap().clone();
    assert!(d.options.record_on_change);
    assert_eq!(
        (d.expected_interval(), d.staleness),
        (Some(20 * SECOND), Some(MINUTE))
    );
    let mut stale = Runner::new(
        Condition::StaleFor {
            staleness: MINUTE,
            n: 0,
        },
        m.mono,
    );
    let t0 = m.wall;
    let mut last: Option<(f64, Nanos)> = None;
    // Ten minutes of polls: the value changes twice, at 2 and 5 minutes.
    for k in 0..300 {
        let t = t0 + k * 2 * SECOND;
        let v = if k < 60 {
            5.0
        } else if k < 150 {
            7.0
        } else {
            6.0
        };
        let changed = last.is_none_or(|(x, _)| x != v);
        let due = last.is_none_or(|(_, at)| t - at >= 20 * SECOND);
        if changed || due {
            m.ingest(vec![num(s, t, v)]).unwrap();
            stale.on_arrival(m.mono);
            last = Some((v, t));
        }
        assert!(
            stale.on_tick(m.mono).is_empty(),
            "never stale while healthy"
        );
        m.advance(2 * SECOND);
    }
    assert_eq!(
        m.facts().unwrap().iter().filter(|f| f.series == s).count(),
        30
    );
    let w = m.window(s, t0, t0 + 10 * MINUTE).unwrap();
    assert_eq!(
        w.coverage,
        i128::from(10 * MINUTE),
        "never stale while healthy"
    );
    // The source falls silent: stale S after its last fact.
    m.advance(MINUTE);
    assert_eq!(stale.on_tick(m.mono), vec![Event::Fired]);
}

proptest! {
    /// Row 18: a decrease is a reset; increase stays correct across nodes.
    #[test]
    fn row_18_counter_reset_increase_across_nodes(
        steps in proptest::collection::vec((0u32..5, 0u32..100, proptest::bool::weighted(0.2)), 1..40),
        start in arb_offset(),
    ) {
        let p = Params::new(Kind::Counter, HOUR);
        let mut v = 0.0;
        let mut t = T0 + start;
        let mut rs = Vec::new();
        let mut expect = 0.0;
        let mut prev: Option<f64> = None;
        for (dt, inc, reset) in steps {
            t += i64::from(dt) * 7 * SECOND + 1;
            v = if reset { f64::from(inc % 7) } else { v + f64::from(inc) };
            if let Some(pv) = prev {
                // §5.5 without wrap: a decrease below 10 % of the previous
                // value is a reset; any other decrease is a regression (0).
                expect += if v >= pv { v - pv } else if v < 0.1 * pv { v } else { 0.0 };
            }
            prev = Some(v);
            rs.push((t, Value::Num(v)));
        }
        let tree = SeriesTree::build(p, &rs);
        let (s, e) = (floor_to(T0, QUARTER), floor_to(t, QUARTER) + QUARTER);
        let w = tree.window(s, e, e);
        prop_assert_eq!(&w, &oracle::window(&p, &rs, s, e, e));
        prop_assert_eq!(w.aggregate(&Aggregate::Increase), Some(AggValue::Exact(exact(expect))));
    }

    /// Row 19: a correction that changes which reading is the min or max:
    /// the leaf is recomputed from the current view.
    #[test]
    fn row_19_correction_changes_min_max(
        vals in proptest::collection::vec(-50i32..50, 2..10),
        which in any::<proptest::sample::Index>(),
        new in -100i32..100,
    ) {
        let (mut m, s) = model();
        let base = T0 + 3 * SECOND;
        let rs: Vec<Reading> = vals.iter().enumerate()
            .map(|(i, v)| num(s, base + i as i64 * SECOND / 10, f64::from(*v)))
            .collect();
        m.ingest(rs.clone()).unwrap();
        let i = which.index(rs.len());
        m.ingest(vec![num(s, rs[i].t, f64::from(new))]).unwrap();
        leaf_ok(&m, s, base);
        let mut cur: Vec<i32> = vals.clone();
        cur[i] = new;
        let leaf = m.tree(s).unwrap().node(0, floor_to(base, MINUTE));
        prop_assert_eq!(leaf.aggregate(&Aggregate::Min), Some(AggValue::Exact(exact(f64::from(*cur.iter().min().unwrap())))));
        prop_assert_eq!(leaf.aggregate(&Aggregate::Max), Some(AggValue::Exact(exact(f64::from(*cur.iter().max().unwrap())))));
    }

    /// Row 20: DST — the UTC tree is unaffected and local days have 92 to 100
    /// quarter-hours, in zones on several continents (see OPEN-QUESTIONS Q-06
    /// for days outside that range). The stored day node is the merge of its
    /// quarter-hours; the day's result is a window over its span, edge
    /// stretches included, with the stored node as an input (§5.7).
    #[test]
    fn row_20_dst_days(
        zone in proptest::sample::select(vec![
            "UTC", "Europe/Paris", "Europe/Berlin", "Europe/London", "America/New_York", "America/Santiago",
            "Australia/Sydney", "Asia/Kolkata", "Asia/Kathmandu", "Pacific/Chatham", "America/Havana",
            "Australia/Lord_Howe", "Asia/Tokyo", "Africa/Cairo", "America/Sao_Paulo",
        ]),
        day in 0i32..(366 * 6),
        near in proptest::option::weighted(0.7, (0usize..12, -1i32..=1)),
        link in any::<bool>(),
        head in arb_readings(Kind::Gauge, 12),
        tail in arb_readings(Kind::Gauge, 12),
        late in 0i64..(2 * HOUR),
    ) {
        use kvik_model::calendar::{CalendarFile, Tzdb, Zoning};
        let tz = TimeZone::get(zone).unwrap();
        // Mostly a DST change in the zone, or a day either side of one.
        let start: jiff::Timestamp = "2024-01-01T00:00:00Z".parse().unwrap();
        let date = match near.and_then(|(k, off)| tz.following(start).nth(k).map(|t| (t, off))) {
            Some((t, off)) => t.timestamp().to_zoned(tz.clone()).date().checked_add(jiff::Span::new().days(off)).unwrap(),
            None => Date::constant(2024, 1, 1).checked_add(jiff::Span::new().days(day)).unwrap(),
        };
        let q = quarters_in_day(&tz, date).unwrap();
        prop_assert!((92..=100).contains(&q) && q % 2 == 0, "{} {} has {} quarter-hours", zone, date, q);
        // Readings around both edges of this local day, some outside it.
        let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
        let mut rs: BTreeMap<Nanos, Value> = BTreeMap::new();
        rs.extend(head.into_iter().map(|(t, v)| (t - T0 + ds - 20 * MINUTE, v)));
        rs.extend(tail.into_iter().map(|(t, v)| (t - T0 + de - 20 * MINUTE, v)));
        let rs: Vec<(Nanos, Value)> = rs.into_iter().collect();
        let p = Params {
            expected_interval: Some(SECOND),
            ..Params::new(Kind::Gauge, 7 * SECOND)
        };
        let tree = SeriesTree::build(p, &rs);
        prop_assert_eq!(tree.day(&tz, date).unwrap(), oracle::day(&p, &rs, &tz, date).unwrap());
        let db = Tzdb::bundled();
        let zoning = Zoning::Named(zone.into());
        let file = CalendarFile::build(&tree, &zoning, &db).unwrap();
        // The query may name a link to the zone the file was built in.
        let query = if link { link_of(zone) } else { zone };
        let d = Zoning::Named(query.into()).day(&db, date).unwrap();
        prop_assert_eq!((d.start, d.end), (ds, de));
        let now = de - HOUR + late;
        let inputs = kvik_model::calendar::inputs(Some(&file), &db, &[d], None, ds, now.min(de));
        let stored = file.days.get(&date).map(|n| n.node.clone());
        prop_assert_eq!(stored.clone(), Some(tree.day(&tz, date).unwrap()).filter(|n| n.count > 0));
        prop_assert_eq!(inputs.len(), usize::from(stored.is_some() && de <= now));
        let got = kvik_model::calendar::evaluate(&tree, ds, de, now, &inputs);
        prop_assert_eq!(got, oracle::window(&p, &rs, ds, de, now));
    }
}

/// A link to `zone` in the bundled database, or `zone` where none is used.
fn link_of(zone: &str) -> &str {
    match zone {
        "America/New_York" => "US/Eastern",
        "Asia/Kolkata" => "Asia/Calcutta",
        "Asia/Kathmandu" => "Asia/Katmandu",
        "Australia/Sydney" => "Australia/NSW",
        "Australia/Lord_Howe" => "Australia/LHI",
        "Pacific/Chatham" => "NZ-CHAT",
        "America/Havana" => "Cuba",
        "Africa/Cairo" => "Egypt",
        "Asia/Tokyo" => "Japan",
        "Europe/London" => "GB",
        "UTC" => "Etc/UTC",
        _ => zone,
    }
}

proptest! {
    /// Row 20 and §5.7 for days whose midnight is in a gap (also one that
    /// straddles midnight), a skipped date and a midnight that occurs twice
    /// (Q-77): the result is a window over the span; a stored node, where the
    /// edges allow one, is the merge of its quarter-hours and an input, also
    /// to a query naming a link to the zone.
    #[test]
    fn row_20_days_with_midnight_in_a_gap_or_twice(
        case in proptest::sample::select(vec![
            ("Pacific/Apia", (2011, 12, 30)),
            ("Pacific/Apia", (2011, 12, 31)),
            ("America/Santiago", (2026, 9, 6)),
            ("America/Toronto", (1919, 3, 31)),
            ("America/Havana", (2026, 3, 8)),
            ("America/Havana", (2026, 11, 1)),
        ]),
        link in any::<bool>(),
        head in arb_readings(Kind::Gauge, 12),
        tail in arb_readings(Kind::Gauge, 12),
    ) {
        use kvik_model::calendar::{CalendarFile, Tzdb, Zoning, aligned};
        let (zone, (y, mo, d)) = case;
        let date = Date::constant(y, mo, d);
        let db = Tzdb::bundled();
        let day = Zoning::Named(zone.into()).day(&db, date).unwrap();
        let (ds, de) = (day.start, day.end);
        let mut rs: BTreeMap<Nanos, Value> = BTreeMap::new();
        rs.extend(head.into_iter().map(|(t, v)| (t - T0 + ds - 20 * MINUTE, v)));
        rs.extend(tail.into_iter().map(|(t, v)| (t - T0 + de - 20 * MINUTE, v)));
        let rs: Vec<(Nanos, Value)> = rs.into_iter().collect();
        let p = Params {
            expected_interval: Some(SECOND),
            ..Params::new(Kind::Gauge, 7 * SECOND)
        };
        let tree = SeriesTree::build(p, &rs);
        let file = CalendarFile::build(&tree, &Zoning::Named(zone.into()), &db).unwrap();
        let query = if link { link_of(zone) } else { zone };
        let q = Zoning::Named(query.into()).day(&db, date).unwrap();
        let inputs = kvik_model::calendar::inputs(Some(&file), &db, &[q], None, ds, de);
        let stored = file.days.get(&date);
        prop_assert_eq!(stored.is_some(), aligned((ds, de)) && rs.iter().any(|(t, _)| ds <= *t && *t < de));
        if let Some(n) = stored {
            prop_assert_eq!(&n.node, &oracle::node(&p, &rs, ds, de));
        }
        prop_assert_eq!(inputs.len(), usize::from(stored.is_some()));
        let now = de + HOUR;
        let got = kvik_model::calendar::evaluate(&tree, ds, de, now, &inputs);
        prop_assert_eq!(got, oracle::window(&p, &rs, ds, de, now));
    }
}

/// Row 20a: "Calendar day or month of a series with no stored 15-minute level
/// (expected interval above 60 s): no calendar node; the result is a window
/// over its facts and any stored nodes inside the span." The interval-series
/// case is `row_20a_interval_series_…`; a span in which the expected interval
/// crosses the threshold needs parameter changes, which the model does not
/// have (Q-60).
#[test]
fn row_20a_no_calendar_node_without_a_stored_15_minute_level() {
    let tz = TimeZone::get("Europe/Paris").unwrap();
    for date in [Date::constant(2026, 3, 29), Date::constant(2026, 6, 1)] {
        let (ds, de) = kvik_model::tree::day_span(&tz, date).unwrap();
        let p = Params {
            expected_interval: Some(2 * MINUTE),
            ..Params::new(Kind::Gauge, 6 * MINUTE)
        };
        let rs: Vec<(Nanos, Value)> = (0..30)
            .map(|k| {
                (
                    ds - 5 * MINUTE + k * 50 * MINUTE,
                    Value::Num(f64::from(k as i32)),
                )
            })
            .collect();
        let tree = SeriesTree::build(p, &rs);
        assert_eq!(tree.day(&tz, date), Err(TreeError::NoCalendarNode));
        assert!(tree.stored(1).next().is_none());
        assert_eq!(tree.window(ds, de, de), oracle::window(&p, &rs, ds, de, de));
    }
}

#[test]
fn row_20_dst_transition_days_explicit() {
    let cases = [
        ("Europe/Paris", (2026, 3, 29), 92),
        ("Europe/Paris", (2026, 10, 25), 100),
        ("America/New_York", (2026, 3, 8), 92),
        ("America/New_York", (2026, 11, 1), 100),
        ("Australia/Sydney", (2026, 4, 5), 100),
        ("Australia/Sydney", (2026, 10, 4), 92),
        ("America/Santiago", (2026, 9, 6), 92), // midnight does not exist
        ("Asia/Kathmandu", (2026, 6, 1), 96),
        ("Australia/Lord_Howe", (2026, 4, 5), 98), // 30-minute shift
        ("Australia/Lord_Howe", (2026, 10, 4), 94),
    ];
    for (zone, (y, mo, d), q) in cases {
        let tz = TimeZone::get(zone).unwrap();
        assert_eq!(
            quarters_in_day(&tz, Date::constant(y, mo, d)).unwrap(),
            q,
            "{zone}"
        );
    }
    // The UTC tree does not depend on any zone.
    let p = Params::new(Kind::Gauge, MINUTE);
    let rs = vec![(T0 + HOUR, Value::Num(1.0))];
    let a = SeriesTree::build(p, &rs);
    let b = SeriesTree::build(p, &rs);
    assert!(a.same_summaries(&b));
}

/// Row 18a: "Counter wraps, glitches low, or is silent for hours: `wrap_at`
/// rule; regression (0, counted); full step credited after the gap, coverage
/// shows it and rate is flagged."
#[test]
fn row_18a_counter_wraps_glitches_low_or_is_silent_for_hours() {
    let mut p = Params::new(Kind::Counter, MINUTE);
    let inc = |rs: &[(Nanos, Value)], p: &Params| {
        oracle::window(p, rs, T0, T0 + 10 * HOUR, T0 + 10 * HOUR).aggregate(&Aggregate::Increase)
    };
    // Wrap: 65530 → 4 with wrap_at 65536 is a step of 10.
    p.counter.wrap_at = Some(65536.0);
    let rs = vec![(T0, Value::Num(65530.0)), (T0 + SECOND, Value::Num(4.0))];
    assert_eq!(inc(&rs, &p), Some(AggValue::Exact(exact(10.0))));
    // Glitch low without wrap_at: a regression (0, counted); the next step is
    // measured from the glitch.
    p.counter.wrap_at = None;
    let rs = vec![
        (T0, Value::Num(1000.0)),
        (T0 + SECOND, Value::Num(990.0)),
        (T0 + 2 * SECOND, Value::Num(1005.0)),
    ];
    let w = oracle::window(&p, &rs, T0, T0 + HOUR, T0 + HOUR);
    assert_eq!(
        w.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(15.0)))
    );
    let kvik_model::summary::Body::Counter {
        regressions,
        resets,
        ..
    } = w.body
    else {
        unreachable!()
    };
    assert_eq!((regressions, resets), (1, 0));
    // Silent for hours: the full step is credited after the gap; coverage
    // shows the gap.
    let rs = vec![(T0, Value::Num(100.0)), (T0 + 5 * HOUR, Value::Num(400.0))];
    let w = oracle::window(&p, &rs, T0, T0 + 5 * HOUR + SECOND, T0 + 5 * HOUR + SECOND);
    assert_eq!(
        w.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(300.0)))
    );
    assert_eq!(w.coverage, i128::from(MINUTE + SECOND));
    assert!(w.rate_flagged(), "rate is flagged");
    assert_eq!(
        w.aggregate(&Aggregate::Rate),
        Some(AggValue::Exact(exact(300.0) / exact(5.0 * 3600.0)))
    );
    // The tree agrees.
    let tree = SeriesTree::build(p, &rs);
    assert_eq!(
        tree.window(T0, T0 + 5 * HOUR + SECOND, T0 + 5 * HOUR + SECOND),
        w
    );
}

/// Row 22: "System clock jumps: timers of observed series monotonic; window
/// `now` never runs backwards." Commit order holds across a backward jump.
#[test]
fn row_22_system_clock_jump_timers_unaffected() {
    let (mut m, s) = model();
    let mut held = Runner::new(
        Condition::HeldFor {
            above: 5.0,
            n: 10 * SECOND,
        },
        m.mono,
    );
    let p1 = m.ingest(vec![num(s, T0, 9.0)]).unwrap().positions[0];
    let r1 = m.wall;
    let now1 = m.now;
    assert!(held.on_value(m.mono, Some(9.0)).is_empty());
    m.jump_wall(-HOUR);
    assert_eq!(m.now, now1, "the window clock never runs backwards");
    let p2 = m.ingest(vec![num(s, T0 + SECOND, 9.0)]).unwrap().positions[0];
    let f2 = m
        .facts()
        .unwrap()
        .into_iter()
        .find(|f| f.position == p2)
        .unwrap();
    assert!(f2.received_at < r1, "received_at may go backwards");
    assert!(p2 > p1, "positions still order");
    m.jump_wall(2 * HOUR);
    assert!(held.on_tick(m.mono).is_empty(), "a wall jump fires nothing");
    m.advance(10 * SECOND);
    assert_eq!(held.on_tick(m.mono), vec![Event::Fired]);
}

/// Row 23: "Series renamed: alias entry; id and data unchanged."
#[test]
fn row_23_series_renamed_alias_fact() {
    let (mut m, s) = model();
    m.ingest(vec![num(s, T0, 1.0)]).unwrap();
    let before = m.facts().unwrap();
    m.rename(s, "plant/line-1/inlet-temperature").unwrap();
    let r = m.registry().unwrap();
    assert_eq!(r.resolve("plant/line-1/inlet-temperature").unwrap().id, s);
    assert_eq!(r.resolve("plant/line-1/temperature").unwrap().id, s);
    assert_eq!(m.facts().unwrap(), before);
    m.restart();
    assert_eq!(
        m.registry()
            .unwrap()
            .resolve("plant/line-1/inlet-temperature")
            .unwrap()
            .id,
        s
    );
}

/// Row 24: "Unit, kind or scale changes: new series; old one retired."
#[test]
fn row_24_unit_or_kind_change_is_new_series() {
    let (mut m, s) = model();
    m.ingest(vec![num(s, T0, 1.0)]).unwrap();
    let new_series = m
        .redefine(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "K",
            10 * SECOND,
        )
        .unwrap();
    assert_ne!(s, new_series);
    assert!(m.registry().unwrap().get(s).unwrap().retired);
    assert_eq!(
        m.registry()
            .unwrap()
            .resolve("plant/line-1/temperature")
            .unwrap()
            .id,
        new_series
    );
    let t_r = m.now;
    assert_eq!(m.emit(num(s, t_r, 1.0)), Err(Refusal::Retired.into()));
    assert!(
        m.emit(num(s, t_r - 1, 1.0)).is_ok(),
        "earlier facts still accepted"
    );
    assert_eq!(m.facts().unwrap().len(), 1, "old series not rewritten");
}

/// Row 25: "Series retired at `t_r`: facts with `t < t_r` accepted; later
/// refused and counted."
#[test]
fn row_25_series_retired_at_t_r() {
    let (mut m, s) = model();
    let t_r = T0 + MINUTE;
    m.ingest(vec![num(s, T0, 1.0)]).unwrap();
    m.retire(s, t_r).unwrap();
    // Facts before t_r are still accepted; from t_r on they are refused.
    m.ingest(vec![num(s, t_r - 1, 2.0)]).unwrap();
    assert_eq!(m.emit(num(s, t_r, 3.0)), Err(Refusal::Retired.into()));
    assert_eq!(m.counters.refused[&Refusal::Retired], 1);
    assert_eq!(m.registry().unwrap().live().count(), 0, "hidden");
    assert!(m.registry().unwrap().get(s).is_some(), "reachable by id");
    assert_eq!(m.facts().unwrap().len(), 2, "facts stay");
    // A reading that waited in the queue while the retirement committed is
    // refused at commit and counted.
    let (mut m, s) = model();
    m.emit(num(s, t_r, 1.0)).unwrap();
    m.emit(num(s, t_r - 1, 1.0)).unwrap();
    m.retire(s, t_r).unwrap();
    assert_eq!(m.commit(None).unwrap().positions.len(), 1);
    assert_eq!(m.counters.refused[&Refusal::Retired], 1);
    assert!(m.facts().unwrap().iter().all(|f| f.t < t_r));
}

/// Row 26: "Reader holds an old epoch: its files stay while its lease names
/// that epoch and its counter keeps changing; leases of a different boot are
/// void."
#[test]
fn row_26_reader_holds_an_old_epoch() {
    let (mut m, inputs) = crowded_model();
    let reader = m.lease_reader();
    let (_, facts) = m.read(&reader).unwrap();
    m.merge_chunks(None).unwrap();
    m.upload();
    // The reader refreshes every 30 minutes, keeping its epoch.
    let mut reader = reader;
    for _ in 0..6 {
        m.advance(30 * MINUTE);
        reader = m.refresh_reader(&reader, false);
        m.housekeeping().unwrap();
        assert!(inputs.iter().all(|f| m.disk.hot.contains(f)), "files stay");
        let (loc, again) = m.read(&reader).unwrap();
        assert!(loc.iter().all(|(_, l)| *l == Locality::Hot));
        assert_eq!(again, facts);
    }
    // It stops refreshing: an hour later its lease has expired.
    m.advance(HOUR);
    m.housekeeping().unwrap();
    assert!(inputs.iter().all(|f| !m.disk.hot.contains(f)));
    // A lease of another boot is void at once: with leases expiring only
    // after three hours, the inputs stay an hour after the merge on the same
    // boot, and leave after a reboot.
    for reboot in [false, true] {
        let (mut m, inputs) = crowded_model();
        m.config.lease_expiry = 3 * HOUR;
        let reader = m.lease_reader();
        m.merge_chunks(None).unwrap();
        m.upload();
        if reboot {
            m.power_loss();
        } else {
            m.crash();
        }
        m.restart();
        m.advance(HOUR);
        m.housekeeping().unwrap();
        assert_eq!(inputs.iter().all(|f| !m.disk.hot.contains(f)), reboot);
        assert_eq!(m.live_lease_epochs().is_empty(), reboot);
        m.close_reader(&reader);
    }
    // A reader that takes the latest epoch no longer holds the inputs.
    let (mut m, inputs) = crowded_model();
    let reader = m.lease_reader();
    m.merge_chunks(None).unwrap();
    m.upload();
    m.refresh_reader(&reader, true);
    m.advance(HOUR);
    m.housekeeping().unwrap();
    assert!(inputs.iter().all(|f| !m.disk.hot.contains(f)));
}

/// Row 27: "Bucket unreachable for days: queue grows, no eviction, degraded
/// health."
#[test]
fn row_27_bucket_unreachable_for_days() {
    let (mut m, s) = model();
    m.disk.bucket.reachable = false;
    for _ in 0..5 {
        let now = m.wall;
        m.ingest(vec![num(s, now, 1.0)]).unwrap();
        m.seal_active();
        assert_eq!(m.upload(), 0);
        m.advance(DAY);
    }
    assert_eq!(m.disk.upload_queue().len(), 5);
    assert!(m.health().reasons.contains(&Reason::UploadStalled));
    m.advance(100 * DAY);
    assert!(m.housekeeping().unwrap().is_empty(), "no eviction");
    m.disk.bucket.reachable = true;
    assert_eq!(m.upload(), 5);
    assert_eq!(m.health().level, Level::Ok);
}

/// Row 29: "Late fact for a minute whose detail is not local: recorded; node
/// `summary_stale` until fetched."
#[test]
fn row_29_late_fact_for_span_without_local_detail() {
    let cfg = Config {
        disk_bytes: 400,
        entry_bytes: 64,
        low_mark: 0,
        critical_mark: 0,
        ..Config::default()
    };
    let (mut m, s) = model_with(cfg);
    for i in 0..6 {
        m.ingest(vec![num(s, T0 + i * SECOND / 4, i as f64)])
            .unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    m.upload();
    m.advance(2 * HOUR);
    let removed = m.housekeeping().unwrap();
    assert!(
        removed
            .iter()
            .any(|f| m.disk.files[f].class == FileClass::Current),
        "current detail moved out under the fill threshold"
    );
    m.ingest(vec![num(s, T0 + SECOND / 2, 99.0)]).unwrap();
    assert!(m.tree(s).unwrap().is_stale(0, T0));
    assert!(m.tree(s).unwrap().is_stale(1, floor_to(T0, QUARTER)));
    m.fetch_detail(s, T0).unwrap();
    assert!(!m.tree(s).unwrap().is_stale(0, T0));
    leaf_ok(&m, s, T0);
}

/// Row 30: "Very many series: ceiling refuses new series clearly." Memory per
/// series is a load test (roadmap 10.1).
#[test]
fn row_30_series_ceiling() {
    let cfg = Config {
        series_ceiling: 2,
        ..Config::default()
    };
    let (mut m, _) = model_with(cfg);
    m.define_with(
        "site/pump-3/flow",
        Kind::Gauge,
        ValueType::Number,
        "l/min",
        SECOND,
        SeriesOptions::expected(SECOND),
    )
    .unwrap();
    let e = m.define_with(
        "vehicle/8/speed",
        Kind::Gauge,
        ValueType::Number,
        "km/h",
        SECOND,
        SeriesOptions::expected(SECOND),
    );
    assert!(matches!(
        e,
        Err(Error::Registry(
            kvik_model::registry::RegistryError::Ceiling(2)
        ))
    ));
}

/// Row 31: "Value NaN or infinite, or a negative counter value: recorded as bad
/// quality without a value; counted."
#[test]
fn row_31_nan_or_infinite_recorded_as_bad_without_value() {
    let (mut m, s) = model();
    for (i, v) in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY]
        .into_iter()
        .enumerate()
    {
        m.emit(num(s, T0 + i as i64, v)).unwrap();
    }
    m.commit(None).unwrap();
    assert_eq!(m.counters.non_finite, 3);
    let facts = m.facts().unwrap();
    assert_eq!(facts.len(), 3);
    assert!(
        facts
            .iter()
            .all(|f| f.quality == Quality::Bad && f.value == Value::Absent)
    );
    assert!(facts.iter().all(|f| f.flags.no_value));
    // Bad quality is ignored by summaries.
    assert_eq!(m.window(s, T0, T0 + MINUTE).unwrap().count, 0);
    // A negative counter value is recorded the same way (§2.5, §5.5), and
    // compared as "no value" by the duplicate rule (§2.4).
    let c = m
        .define_with(
            "plant/line-1/energy",
            Kind::Counter,
            ValueType::Number,
            "kWh",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    m.ingest(vec![
        num(c, T0, 100.0),
        num(c, T0 + SECOND, -5.0),
        num(c, T0 + 2 * SECOND, 110.0),
    ])
    .unwrap();
    assert_eq!(m.counters.negative_counter, 1);
    let f = m.current().unwrap()[&(c, T0 + SECOND)].clone();
    assert!(f.quality == Quality::Bad && f.value == Value::Absent && f.flags.no_value);
    let r = m.ingest(vec![num(c, T0 + SECOND, -7.0)]).unwrap();
    assert_eq!(
        r.duplicates, 1,
        "both are \"no value\" with the same supplied quality"
    );
    // The bad reading is ignored: the step is from 100 to 110, no reset.
    let w = m.window(c, T0, T0 + MINUTE).unwrap();
    assert_eq!(
        w.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(10.0)))
    );
    let kvik_model::summary::Body::Counter {
        resets,
        regressions,
        ..
    } = w.body
    else {
        unreachable!()
    };
    assert_eq!((resets, regressions), (0, 0));
}

/// Row 22a: "Host clock not yet synchronised at boot: `unsynced_clock`;
/// host-stamped facts wait." The wait pins the model's abstraction (see the
/// table in `OPEN-QUESTIONS.md`): the emit returns back-pressure and the
/// adapter emits the reading again, rather than the reading waiting in the
/// ingest queue.
#[test]
fn row_22a_host_clock_not_yet_synchronised() {
    let (mut m, s) = model();
    m.power_loss();
    m.clock_synchronised = false;
    m.restart();
    let now = m.wall;
    // A host-stamped reading waits: the emit returns back-pressure and the
    // adapter emits it again later.
    let host = num(s, now, 1.0).host();
    assert_eq!(m.emit(host.clone()), Err(Wait::UnsyncedClock.into()));
    assert_eq!(
        m.queued(),
        0,
        "not stamped into the queue with a wrong time"
    );
    // A device-stamped reading is accepted and flagged.
    let p = m.ingest(vec![num(s, now - HOUR, 2.0)]).unwrap().positions[0];
    let f = m
        .facts()
        .unwrap()
        .into_iter()
        .find(|f| f.position == p)
        .unwrap();
    assert!(f.flags.unsynced_clock);
    assert!(!f.flags.late, "late is not set before synchronisation");
    m.clock_synchronised = true;
    m.emit(host).unwrap();
    let p = m.commit(None).unwrap().positions[0];
    let f = m
        .facts()
        .unwrap()
        .into_iter()
        .find(|f| f.position == p)
        .unwrap();
    assert!(f.flags.host_time && !f.flags.unsynced_clock);
}

/// Row 32: "Format version newer than the reader: refused with a clear error."
/// Golden files follow with the format (roadmap 4.2).
#[test]
fn row_32_newer_format_refused() {
    let (mut m, _) = sealed_model();
    let f = *m.disk.hot.iter().next().unwrap();
    m.disk.files.get_mut(&f).unwrap().version = FORMAT_VERSION + 1;
    let r = m.open_reader();
    assert_eq!(m.read(&r), Err(Error::NewerFormat(f, FORMAT_VERSION + 1)));
}

/// Row 35: "Window with no new data, or edge inside the open minute: timer
/// evaluation; nothing credited past the credit limit or S."
#[test]
fn row_35_window_with_no_new_data_or_edge_in_open_minute() {
    let (mut m, s) = model(); // S = 10 s
    let t = m.wall - 3 * SECOND - SECOND / 3; // inside the open minute
    m.ingest(vec![num(s, t, 4.0)]).unwrap();
    // Timer evaluation with no new reading: credited up to now, not past it.
    let w = m.window(s, t - MINUTE, t + HOUR).unwrap();
    assert_eq!(w.coverage, i128::from(m.now - t));
    // Later, nothing is credited past the staleness limit.
    m.advance(MINUTE);
    let w = m.window(s, t - MINUTE, t + HOUR).unwrap();
    assert_eq!(w.coverage, i128::from(10 * SECOND));
    // A window with no reading inside uses the held value while it is valid.
    let w = m.window(s, t + SECOND, t + 2 * SECOND).unwrap();
    assert_eq!(w.count, 0);
    assert_eq!(
        w.aggregate(&Aggregate::Last),
        Some(AggValue::Value(Value::Num(4.0)))
    );
}

/// Row 20a through the engine: with no stored 15-minute level (a gauge whose
/// expected interval is 2 minutes; an interval series with d = 30 min) the
/// calendar file holds no node, and a day's or month's result is a window over
/// its facts.
#[test]
fn row_20a_calendar_results_without_a_stored_level_are_windows_over_facts() {
    let now = utc("2026-04-02T00:00:00Z");
    let cfg = Config {
        site_zone: "Europe/Paris".into(),
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], now);
    let s = m
        .define_with(
            "line-1/meter/power",
            Kind::Gauge,
            ValueType::Number,
            "W",
            6 * MINUTE,
            SeriesOptions::expected(2 * MINUTE),
        )
        .unwrap();
    feed(
        &mut m,
        s,
        &every(utc("2026-02-28T20:00:00Z"), now, 50 * MINUTE),
    );
    let p = m.tree(s).unwrap().params;
    let rs = oracle::readings(m.current().unwrap(), s);
    assert!(m.calendar_file(s).unwrap().days.is_empty());
    assert!(m.calendar_file(s).unwrap().months.is_empty());
    for period in [
        Period::Day(Date::constant(2026, 3, 29)),
        Period::Month(Month {
            year: 2026,
            month: 3,
        }),
    ] {
        let a = m.calendar(s, period).unwrap();
        assert!(a.inputs.is_empty());
        assert_eq!(a.node, oracle::window(&p, &rs, a.from, a.to, m.now));
    }
    // An interval series with d = 30 min in a +05:45 site zone.
    let cfg = Config {
        site_zone: "Asia/Kathmandu".into(),
        ..Config::default()
    };
    let now = utc("2026-06-03T00:00:00Z");
    let mut m = Model::new(cfg, vec![], now);
    let s = m
        .define_with(
            "line-1/meter/energy",
            Kind::Interval,
            ValueType::Number,
            "Wh",
            None,
            SeriesOptions::interval(30 * MINUTE, 0),
        )
        .unwrap();
    let rs: Vec<(Nanos, f64)> = every(
        utc("2026-05-31T00:00:00Z"),
        utc("2026-06-02T12:00:00Z"),
        30 * MINUTE,
    )
    .into_iter()
    .map(|(t, _)| (t, 2.0))
    .collect();
    feed(&mut m, s, &rs);
    assert!(m.calendar_file(s).unwrap().days.is_empty());
    let a = m
        .calendar(s, Period::Day(Date::constant(2026, 6, 1)))
        .unwrap();
    assert_eq!(
        (a.from, a.to),
        (utc("2026-05-31T18:15:00Z"), utc("2026-06-01T18:15:00Z"))
    );
    assert!(a.inputs.is_empty());
    assert!(a.node.apportioned());
    assert_eq!(
        a.node.aggregate(&Aggregate::Sum),
        Some(AggValue::Exact(exact(96.0)))
    );
    let p = m.tree(s).unwrap().params;
    let rs = oracle::readings(m.current().unwrap(), s);
    assert_eq!(a.node, oracle::window(&p, &rs, a.from, a.to, m.now));
}

/// Row 21: "Site zone changed: calendar nodes rebuilt from the effective time
/// only; logged." The site zone moves from Paris to Berlin (the same offsets
/// in 2026, another zone) from local midnight of 20 May. Days before keep
/// Paris; May's month node now covers days of both zones, so a query in
/// Paris takes only its Paris days as inputs, and one in Berlin only its
/// Berlin days (§5.7: "so not a month spanning a site-zone change").
#[test]
fn row_21_site_zone_change_rebuilds_from_its_effective_time() {
    let now = utc("2026-06-03T00:00:00Z");
    let (mut m, s) = calendar_model("Europe/Paris", now, HOUR);
    feed(
        &mut m,
        s,
        &every(
            utc("2026-05-10T00:00:00Z"),
            utc("2026-06-02T00:00:00Z"),
            37 * MINUTE,
        ),
    );
    let before = m.calendar_file(s).unwrap().clone();
    let effective = utc("2026-05-19T22:00:00Z");
    let position = m.set_site_zone("Europe/Berlin", effective).unwrap();
    let after = m.calendar_file(s).unwrap().clone();
    let changed = before.changed_days(&after);
    let db = kvik_model::calendar::Tzdb::bundled();
    let (paris, berlin) = (
        db.id("Europe/Paris").unwrap(),
        db.id("Europe/Berlin").unwrap(),
    );
    assert_ne!(paris, berlin);
    let from_20 = Date::constant(2026, 5, 20);
    assert_eq!(
        changed,
        after
            .days
            .range(from_20..)
            .map(|(d, _)| *d)
            .collect::<Vec<_>>()
    );
    for (d, n) in &after.days {
        assert_eq!(
            n.zones,
            vec![if *d < from_20 { paris } else { berlin }],
            "{d}"
        );
        if *d < from_20 {
            assert_eq!(Some(n), before.days.get(d), "past days keep the old zone");
        }
    }
    // Logged, with the position of the entry.
    let log = m.calendar_log.last().unwrap();
    assert_eq!(
        log.cause,
        kvik_model::engine::CalendarCause::SiteZone { position }
    );
    assert_eq!(log.days, changed);
    let may = Month {
        year: 2026,
        month: 5,
    };
    let june = Month {
        year: 2026,
        month: 6,
    };
    assert_eq!(log.months, vec![may, june]);
    let p = m.tree(s).unwrap().params;
    let rs = oracle::readings(m.current().unwrap(), s);
    let (from, to) = (utc("2026-04-30T22:00:00Z"), utc("2026-05-31T22:00:00Z"));
    let site = m.calendar(s, Period::Month(may)).unwrap();
    assert_eq!(
        site.inputs,
        vec![(from, to)],
        "the month node, in the site zone"
    );
    let day_spans = |range: std::ops::Range<i8>| -> Vec<(Nanos, Nanos)> {
        range
            .map(|d| {
                let n = &after.days[&Date::constant(2026, 5, d)];
                (n.start, n.end)
            })
            .collect()
    };
    let in_paris = m
        .calendar_in(s, Period::Month(may), "Europe/Paris")
        .unwrap();
    assert_eq!(in_paris.inputs, day_spans(10..20));
    let in_berlin = m
        .calendar_in(s, Period::Month(may), "Europe/Berlin")
        .unwrap();
    assert_eq!(in_berlin.inputs, day_spans(20..32));
    for a in [&site, &in_paris, &in_berlin] {
        assert_eq!((a.from, a.to), (from, to));
        assert_eq!(a.node, oracle::window(&p, &rs, from, to, m.now));
    }
    // The rebuild equals a build from the log alone (invariant 8), also after
    // a restart, and after compaction, a tree checkpoint and a restore, which
    // carry the site-zone entry as a registry entry. The store is opened with
    // the zone now in force.
    assert_eq!(m.calendars().unwrap(), &m.rebuilt_calendars().unwrap());
    m.config.site_zone = "Europe/Berlin".into();
    m.restart();
    assert_eq!(m.calendar_file(s).unwrap(), &after);
    m.seal_active();
    m.compact(None).unwrap();
    m.checkpoint().unwrap();
    m.seal_active();
    m.upload();
    m.upload_checkpoints();
    let mut r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    assert_eq!(r.calendar_file(s).unwrap(), &after, "from the checkpoint");
    r.restore_rest().unwrap();
    assert!(!r.restoring());
    assert_eq!(
        r.calendar_file(s).unwrap(),
        &after,
        "once the detail is back"
    );
    assert_eq!(r.calendar(s, Period::Month(may)).unwrap().node, site.node);
}

/// Row 21: "tzdata changes its rules: calendar nodes rebuilt for the affected
/// days only; logged." A later tzdata release moves Chile's DST to the
/// second Saturday of August and May. Until the rebuild, the calendar file
/// still holds the old version, so its nodes are no input (§5.7) and results
/// follow the new rules from the UTC data; the rebuild changes only the days
/// whose spans moved.
#[test]
fn row_21_tzdata_change_rebuilds_the_affected_days() {
    use kvik_model::calendar::Tzdb;
    let now = utc("2026-07-01T12:00:00Z");
    let (mut m, s) = calendar_model("America/Santiago", now, HOUR);
    feed(
        &mut m,
        s,
        &every(
            utc("2026-04-01T00:00:00Z"),
            utc("2026-04-11T00:00:00Z"),
            41 * MINUTE,
        ),
    );
    feed(
        &mut m,
        s,
        &every(
            utc("2026-06-01T06:00:00Z"),
            utc("2026-06-04T00:00:00Z"),
            41 * MINUTE,
        ),
    );
    let old = m.calendar_file(s).unwrap().clone();
    assert_eq!(old.tzdata, "bundled");
    let rules = TimeZone::posix("<-04>4<-03>,M8.2.6/24,M5.2.6/24").unwrap();
    let db = Tzdb::bundled()
        .with_rules("test-2", "America/Santiago", rules)
        .unwrap();
    m.install_tzdb(db);
    let p = m.tree(s).unwrap().params;
    let rs = oracle::readings(m.current().unwrap(), s);
    // 7 April: −04 under the old rules, still −03 under the new ones.
    let apr7 = Period::Day(Date::constant(2026, 4, 7));
    let pending = m.calendar(s, apr7).unwrap();
    assert_eq!(
        (pending.from, pending.to),
        (utc("2026-04-07T03:00:00Z"), utc("2026-04-08T03:00:00Z"))
    );
    assert!(
        pending.inputs.is_empty(),
        "a node of another tzdata version"
    );
    assert_eq!(
        pending.node,
        oracle::window(&p, &rs, pending.from, pending.to, m.now)
    );
    let log = m.rebuild_calendars().unwrap();
    assert_eq!(log.len(), 1);
    // The new version is a log entry, at the position the rebuild names.
    let position = m.entries().unwrap().last().unwrap().position;
    assert_eq!(
        log[0].cause,
        kvik_model::engine::CalendarCause::Tzdata {
            from: "bundled".into(),
            to: "test-2".into(),
            position,
        }
    );
    assert!(matches!(
        &m.entries().unwrap().last().unwrap().body,
        kvik_model::engine::EntryBody::Registry(kvik_model::registry::RegistryEvent::Tzdata { version })
            if version == "test-2"
    ));
    // 1–3 April and June keep their spans; 4 April (whose end moves) and
    // 5–10 April are rebuilt.
    let days: Vec<Date> = (4..=10).map(|d| Date::constant(2026, 4, d)).collect();
    assert_eq!(log[0].days, days);
    assert_eq!(
        log[0].months,
        vec![Month {
            year: 2026,
            month: 4
        }]
    );
    let new = m.calendar_file(s).unwrap();
    assert_eq!(new.tzdata, "test-2");
    for d in old.days.keys().filter(|d| !days.contains(d)) {
        assert_eq!(old.days.get(d), new.days.get(d), "{d} unaffected");
    }
    let rebuilt = m.calendar(s, apr7).unwrap();
    assert_eq!(rebuilt.inputs, vec![(rebuilt.from, rebuilt.to)]);
    assert_eq!(rebuilt.node, pending.node);
    assert!(
        m.rebuild_calendars().unwrap().is_empty(),
        "nothing left to rebuild"
    );
    assert_eq!(m.calendar_log.len(), 1);
}

/// Row 21a: "Calendar query in a named zone whose offset is not a multiple of
/// 15 minutes (historical offsets): days taken in that zone for the whole
/// span; no stored calendar node used; evaluated as a window over the UTC
/// data, with facts at its edges." Monrovia kept −00:44:30 until 1972 and
/// Dublin −00:25:21 until 1916; a store in either site zone stores no day
/// node for such days either (§5.7).
#[test]
fn row_21a_named_zone_with_a_historical_offset() {
    for (zone, year, wall, from, step) in [
        (
            "Africa/Monrovia",
            1970,
            "1970-06-04T00:00:00Z",
            "1970-05-31T00:00:00Z",
            13 * SECOND,
        ),
        (
            "Europe/Dublin",
            1912,
            "1912-06-04T00:00:00Z",
            "1912-05-31T00:00:00Z",
            11 * SECOND,
        ),
    ] {
        for site in ["UTC", zone] {
            let (mut m, s) = calendar_model(site, utc(wall), 10 * MINUTE);
            feed(&mut m, s, &every(utc(from), utc(wall) - DAY, step));
            let p = m.tree(s).unwrap().params;
            let rs = oracle::readings(m.current().unwrap(), s);
            let date = Date::constant(year, 6, 1);
            let a = m.calendar_in(s, Period::Day(date), zone).unwrap();
            assert_ne!(floor_to(a.from, QUARTER), a.from, "{zone}");
            assert!(a.inputs.is_empty());
            assert_eq!(
                a.node,
                oracle::window(&p, &rs, a.from, a.to, m.now),
                "{zone} in {site}"
            );
            // Readings inside the partial minutes at both edges, and only
            // those inside the span, count.
            let inside = rs.iter().filter(|(t, _)| a.from <= *t && *t < a.to).count();
            assert_eq!(a.node.count, u64::try_from(inside).unwrap());
            let edge = |x: Nanos| {
                rs.iter()
                    .any(|(t, _)| *t < x && floor_to(*t, MINUTE) == floor_to(x, MINUTE))
            };
            assert!(edge(a.from) && edge(a.to));
            if site == zone {
                assert!(!m.calendar_file(s).unwrap().days.contains_key(&date));
                assert_eq!(m.calendar(s, Period::Day(date)).unwrap(), a);
            }
            let month = m
                .calendar_in(s, Period::Month(Month::of(date)), zone)
                .unwrap();
            assert!(month.inputs.is_empty());
            assert_eq!(
                month.node,
                oracle::window(&p, &rs, month.from, month.to, m.now)
            );
        }
    }
}

/// Row 21a for interval series: "on an interval series, a multiple of 15
/// minutes but not of d (+05:45 with d = 30 min): an interval crossing a day
/// edge is apportioned (§6)." With d = 1 min, which the density rule lets
/// store the 15-minute level, the day edges in +05:45 are multiples of d, so
/// the stored day node is an input and nothing is apportioned; in Monrovia's
/// −00:44:30 the same series has its stored quarter-hours in the middle and
/// apportioned edges.
#[test]
fn row_21a_interval_series_in_named_zones() {
    let now = utc("2026-06-04T00:00:00Z");
    let define = |m: &mut Model, d: Nanos| {
        m.define_with(
            &format!("line-1/meter-{}/energy", d / MINUTE),
            Kind::Interval,
            ValueType::Number,
            "Wh",
            None,
            SeriesOptions::interval(d, 0),
        )
        .unwrap()
    };
    let cfg = Config {
        site_zone: "Asia/Kathmandu".into(),
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], now);
    let (s30, s1) = (define(&mut m, 30 * MINUTE), define(&mut m, MINUTE));
    for (s, d) in [(s30, 30 * MINUTE), (s1, MINUTE)] {
        let rs: Vec<(Nanos, f64)> =
            every(utc("2026-05-30T00:00:00Z"), utc("2026-06-03T00:00:00Z"), d)
                .into_iter()
                .map(|(t, k)| (t, 1.0 + k % 4.0))
                .collect();
        feed(&mut m, s, &rs);
    }
    let date = Period::Day(Date::constant(2026, 6, 1));
    let span = (utc("2026-05-31T18:15:00Z"), utc("2026-06-01T18:15:00Z"));
    for (s, zone, stored) in [
        (s30, "Asia/Kathmandu", false),
        (s30, "Asia/Katmandu", false),
        (s1, "Asia/Kathmandu", true),
        (s1, "Asia/Katmandu", true),
    ] {
        let p = m.tree(s).unwrap().params;
        let rs = oracle::readings(m.current().unwrap(), s);
        let a = m.calendar_in(s, date, zone).unwrap();
        assert_eq!((a.from, a.to), span);
        assert_eq!(a.inputs, if stored { vec![span] } else { vec![] }, "{zone}");
        assert_eq!(a.node.apportioned(), !stored);
        assert_eq!(a.node, oracle::window(&p, &rs, a.from, a.to, m.now));
    }
    // d = 1 min in a zone off 15-minute boundaries.
    let cfg = Config {
        site_zone: "Africa/Monrovia".into(),
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], utc("1970-06-04T00:00:00Z"));
    let s = define(&mut m, MINUTE);
    feed(
        &mut m,
        s,
        &every(
            utc("1970-05-31T12:00:00Z"),
            utc("1970-06-02T12:00:00Z"),
            MINUTE,
        ),
    );
    let a = m
        .calendar(s, Period::Day(Date::constant(1970, 6, 1)))
        .unwrap();
    assert_eq!(
        (a.from, a.to),
        (utc("1970-06-01T00:44:30Z"), utc("1970-06-02T00:44:30Z"))
    );
    assert!(a.inputs.is_empty());
    assert!(a.node.apportioned());
    let p = m.tree(s).unwrap().params;
    let rs = oracle::readings(m.current().unwrap(), s);
    assert_eq!(a.node, oracle::window(&p, &rs, a.from, a.to, m.now));
}
