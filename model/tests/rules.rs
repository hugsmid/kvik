//! Tests of specification rules not covered by an edge-case row, named after
//! the section they check.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use kvik_model::condition::{Condition, ConditionState};
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, DeliveryId, DeliveryKind, EntryBody, Envelope,
    Error, Fault, FileClass, FileId, Funnel, FunnelId, Level, Locality, ManifestOp, Model,
    Operator, Reason, Refusal, Selection, Signal, Tier, TriggerCause, Wait, WriteOutcome,
};
use kvik_model::oracle;
use kvik_model::registry::{ChangedAtKind, RegistryError, RetentionClass, SeriesOptions, Timing};
use kvik_model::summary::{AggValue, Aggregate, Body, Params, direct};
use kvik_model::tree::SeriesTree;
use kvik_model::types::{
    DAY, Exact, Fact, HOUR, Kind, MINUTE, Nanos, Position, QUARTER, Quality, Reading, SECOND,
    SeriesId, Value, ValueType, exact,
};
use kvik_model::view;

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
    assert_eq!(m.counters.late, 1);
}

/// §2.2: the first clause of `late` applies to observed series only; the
/// second, `received_at − t` above the lateness bound, to any series, with
/// the bound set per series (default 5 minutes).
#[test]
fn s2_2_late_first_clause_observed_only_bound_per_series() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let sched = m
        .define_with(
            "site/tariff-1/price",
            Kind::Gauge,
            ValueType::Number,
            "c/kWh",
            10 * SECOND,
            SeriesOptions {
                lateness_bound: Some(30 * MINUTE),
                ..SeriesOptions::timing(Timing::Scheduled)
            },
        )
        .unwrap();
    let now = m.wall;
    m.ingest(vec![Reading::num(sched, now + DAY, 1.0)]).unwrap();
    // A day below the latest recorded t, but within the series' bound.
    m.ingest(vec![Reading::num(sched, now - 20 * MINUTE, 1.0)])
        .unwrap();
    m.ingest(vec![Reading::num(sched, now - 31 * MINUTE, 1.0)])
        .unwrap();
    let late: Vec<bool> = m.facts().unwrap().iter().map(|f| f.flags.late).collect();
    assert_eq!(late, vec![false, false, true]);
}

/// §2.2: `future` applies to observed series only; a scheduled or forecast
/// reading far ahead is recorded unflagged with its quality kept.
#[test]
fn s2_2_future_observed_series_only() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let sched = m
        .define_with(
            "site/tariff-1/price",
            Kind::Gauge,
            ValueType::Number,
            "c/kWh",
            10 * SECOND,
            SeriesOptions::timing(Timing::Scheduled),
        )
        .unwrap();
    let now = m.wall;
    m.ingest(vec![Reading::num(sched, now + DAY, 1.0)]).unwrap();
    let f = &m.facts().unwrap()[0];
    assert!(!f.flags.future && !f.flags.late);
    assert_eq!(f.quality, Quality::Good);
    assert_eq!(m.counters.future, 0);
}

/// §2.5: until the system reports clock synchronisation, batches carry
/// `unsynced_clock`, `future` and `late` are not set, host-stamped readings
/// wait (back-pressure, counted) and device-stamped readings are accepted.
/// §2.2: a host-stamped fact is flagged `host_time`.
#[test]
fn s2_5_unsynchronised_clock() {
    let (mut m, s) = model(10 * SECOND);
    m.clock_synchronised = false;
    let now = m.wall;
    assert_eq!(
        m.emit(Reading::num(s, now, 1.0).host()),
        Err(Wait::UnsyncedClock.into())
    );
    assert_eq!(m.counters.waits[&Wait::UnsyncedClock], 1);
    m.ingest(vec![
        Reading::num(s, now - DAY, 2.0),
        Reading::num(s, now + DAY, 3.0),
    ])
    .unwrap();
    for f in m.facts().unwrap() {
        assert!(f.flags.unsynced_clock && !f.flags.host_time);
        assert!(!f.flags.late && !f.flags.future);
        assert_eq!(f.quality, Quality::Good);
    }
    m.clock_synchronised = true;
    let now = m.wall;
    let p = m
        .ingest(vec![Reading::num(s, now, 1.0).host()])
        .unwrap()
        .positions[0];
    let f = m
        .facts()
        .unwrap()
        .into_iter()
        .find(|f| f.position == p)
        .unwrap();
    assert!(f.flags.host_time && !f.flags.unsynced_clock);
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

/// §2.2: "The adapter-supplied quality is kept with the fact for the
/// duplicate rule (§2.4)"; §2.4: "Flags and quality assigned by the engine
/// are not part of identity." A future reading is lowered to suspect; its
/// resend as good once due is a duplicate.
#[test]
fn s2_4_identity_uses_adapter_supplied_quality() {
    let (mut m, s) = model(10 * SECOND);
    let t = m.wall + 2 * MINUTE;
    m.ingest(vec![Reading::num(s, t, 5.0)]).unwrap();
    let f = m.current().unwrap()[&(s, t)].clone();
    assert_eq!(
        (f.quality, f.supplied_quality),
        (Quality::Suspect, Quality::Good)
    );
    m.advance(3 * MINUTE);
    let r = m.ingest(vec![Reading::num(s, t, 5.0)]).unwrap();
    assert_eq!((r.positions.len(), r.duplicates), (0, 1));
    // Supplied as suspect, it differs from the fact supplied as good.
    let r = m
        .ingest(vec![Reading {
            quality: Quality::Suspect,
            ..Reading::num(s, t, 5.0)
        }])
        .unwrap();
    assert_eq!(r.positions.len(), 1);
}

/// §2.4: "If the current fact for a key is on no local tier (its detail moved
/// out under §8 fill pressure or retention class), the fact is recorded and
/// counted `duplicates_unchecked` (§12 D5). The check never reads off-machine
/// data." The cold tier is local: a current fact held there is checked.
#[test]
fn s2_4_current_fact_in_cold_tier_is_checked() {
    let cfg = Config {
        disk_bytes: 400,
        low_mark: 0,
        critical_mark: 0,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    for i in 0..6 {
        m.ingest(vec![Reading::num(s, T0 + i, 1.0)]).unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    for f in m.disk.hot.clone() {
        if m.disk.files[&f].sealed {
            m.copy_to_cold(f);
        }
    }
    m.advance(2 * HOUR);
    assert!(
        !m.housekeeping().unwrap().is_empty(),
        "fill pressure moves detail out of hot"
    );
    let r = m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    assert_eq!((r.positions.len(), r.duplicates), (0, 1));
    assert_eq!(m.counters.duplicates_unchecked, 0);
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
    // Decided choice D6: the previous value's hold continues up to S.
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

/// §2.1: interval length and phase are for interval series only, with
/// d > 0 and 0 ≤ phase < d; record on change does not apply to an interval
/// series; counter rules are for counters only.
#[test]
fn s2_1_interval_and_record_on_change_definitions() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let def = |m: &mut Model, kind, options| {
        m.define_with(
            "site/meter-1/energy",
            kind,
            ValueType::Number,
            "kWh",
            None,
            options,
        )
    };
    for (kind, options) in [
        (Kind::Interval, SeriesOptions::default()),
        (Kind::Interval, SeriesOptions::interval(0, 0)),
        (Kind::Interval, SeriesOptions::interval(MINUTE, MINUTE)),
        (Kind::Interval, SeriesOptions::interval(MINUTE, -1)),
        (Kind::Gauge, SeriesOptions::interval(MINUTE, 0)),
    ] {
        assert_eq!(
            def(&mut m, kind, options),
            Err(RegistryError::BadIntervalSpec.into())
        );
    }
    let roc = SeriesOptions {
        record_on_change: true,
        ..SeriesOptions::interval(MINUTE, 0)
    };
    assert_eq!(
        def(&mut m, Kind::Interval, roc),
        Err(RegistryError::RecordOnChangeInterval.into())
    );
    let wrap = SeriesOptions {
        counter: kvik_model::summary::CounterRules {
            wrap_at: Some(100.0),
            reset_ceiling: 0.1,
        },
        ..SeriesOptions::default()
    };
    assert_eq!(
        def(&mut m, Kind::Gauge, wrap),
        Err(RegistryError::BadCounterRules.into())
    );
    let s = def(&mut m, Kind::Interval, SeriesOptions::interval(MINUTE, 0)).unwrap();
    let d = m.registry().unwrap().get(s).unwrap().clone();
    assert_eq!(
        (d.expected_interval(), d.staleness),
        (Some(MINUTE), Some(3 * MINUTE)),
        "S = 3 × d unless set"
    );
    // Counter rules set on a definition reach the summaries (§5.5).
    let c = m
        .define_with(
            "site/meter-2/energy",
            Kind::Counter,
            ValueType::Number,
            "kWh",
            10 * SECOND,
            wrap,
        )
        .unwrap();
    m.ingest(vec![
        Reading::num(c, T0, 90.0),
        Reading::num(c, T0 + SECOND, 5.0),
    ])
    .unwrap();
    let w = m.window(c, T0, T0 + MINUTE).unwrap();
    assert_eq!(
        w.aggregate(&Aggregate::Increase),
        Some(AggValue::Exact(exact(15.0)))
    );
}

/// §5.1 density rule: the 1-minute level is stored only for series whose
/// expected interval is 5 s or shorter, the 15-minute level only for 60 s or
/// shorter; a sparser series answers those levels from its facts, and stores
/// no calendar nodes (§5.7). State and text series have no expected interval
/// by default and store no level.
#[test]
fn s5_1_density_rule() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let cases = [
        (Some(5 * SECOND), [true, true]),
        (Some(5 * SECOND + 1), [false, true]),
        (Some(MINUTE), [false, true]),
        (Some(MINUTE + 1), [false, false]),
    ];
    let mut ids = Vec::new();
    for (i, (e, _)) in cases.iter().enumerate() {
        let options = SeriesOptions {
            expected_interval: *e,
            ..SeriesOptions::default()
        };
        let s = m
            .define_with(
                &format!("site/meter-{i}/power"),
                Kind::Gauge,
                ValueType::Number,
                "W",
                None,
                options,
            )
            .unwrap();
        // S = 3 × expected interval unless set (§2.1).
        assert_eq!(
            m.registry().unwrap().get(s).unwrap().staleness,
            e.map(|e| 3 * e)
        );
        ids.push(s);
    }
    let state = m
        .define("site/door-1/state", Kind::State, ValueType::Name, "", None)
        .unwrap();
    let rs: Vec<Reading> = (0..40)
        .flat_map(|k| {
            ids.iter()
                .map(move |s| Reading::num(*s, T0 + k * 7 * SECOND, f64::from(k as i32)))
        })
        .collect();
    m.ingest(rs).unwrap();
    m.ingest(vec![Reading {
        value: Value::Str("open".into()),
        ..Reading::num(state, T0, 0.0)
    }])
    .unwrap();
    let tz = jiff::tz::TimeZone::UTC;
    let date = jiff::civil::Date::constant(2026, 3, 29);
    for (s, (_, stores)) in ids.iter().zip(cases) {
        let tree = m.tree(*s).unwrap();
        for (level, stored) in stores.iter().enumerate() {
            assert_eq!(
                tree.stored(level).next().is_some(),
                *stored,
                "{s} level {level}"
            );
        }
        assert_eq!(tree.day(&tz, date).is_ok(), stores[1]);
        let rs = oracle::readings(m.current().unwrap(), *s);
        let p = tree.params;
        for (a, b) in [(T0, T0 + QUARTER), (T0 + SECOND, T0 + 4 * MINUTE + 3)] {
            assert_eq!(
                m.window(*s, a, b).unwrap(),
                oracle::window(&p, &rs, a, b, m.now)
            );
        }
    }
    let tree = m.tree(state).unwrap();
    assert!(tree.stored(0).next().is_none() && tree.stored(1).next().is_none());
    assert_eq!(
        tree.day(&tz, date),
        Err(kvik_model::tree::TreeError::NoCalendarNode)
    );
    for (s, live) in m.trees().unwrap() {
        assert!(live.same_summaries(&m.rebuild_trees().unwrap()[s]));
    }
}

/// §6 Aggregates: rate is the increase over the elapsed seconds between the
/// readings that bound it; the result carries coverage and is flagged when a
/// step it includes spans more than S, in force at the step's earlier
/// reading; it is empty when no step falls in the window.
#[test]
fn s6_rate_over_elapsed_time_between_bounding_readings() {
    let p = Params::new(Kind::Counter, 3 * MINUTE);
    let rate = |rs: &[(Nanos, Value)], a: Nanos, b: Nanos| {
        let tree = SeriesTree::build(p, rs);
        let w = tree.window(a, b, b);
        assert_eq!(w, oracle::window(&p, rs, a, b, b));
        let got = w.aggregate(&Aggregate::Rate).map(|v| match v {
            AggValue::Exact(x) => x,
            AggValue::Value(_) => unreachable!(),
        });
        let want = oracle::rate(&p, rs, a, b, b);
        assert_eq!(got.clone().zip(Some(w.rate_flagged())), want);
        (got, w.rate_flagged(), w.coverage)
    };
    let q = |a: i64, b: i64| Some(Exact::new(a.into(), b.into()));
    // Silent for an hour: 150 over 3600 s, flagged; coverage shows the gap.
    let rs = vec![(T0, n(100.0)), (T0 + HOUR, n(250.0))];
    let (r, flagged, coverage) = rate(&rs, T0, T0 + HOUR + SECOND);
    assert_eq!(r, q(150, 3600));
    assert!(flagged);
    assert_eq!(coverage, i128::from(3 * MINUTE + SECOND));
    // Steps within S: unflagged; the leading step from the reading before the
    // start is included, so the elapsed time runs from that reading.
    let rs = vec![
        (T0, n(0.0)),
        (T0 + MINUTE, n(30.0)),
        (T0 + 2 * MINUTE, n(90.0)),
    ];
    assert_eq!(rate(&rs, T0 + 30 * SECOND, T0 + 3 * MINUTE).0, q(90, 120));
    assert!(!rate(&rs, T0 + 30 * SECOND, T0 + 3 * MINUTE).1);
    assert_eq!(rate(&rs, T0 + 90 * SECOND, T0 + 3 * MINUTE).0, q(60, 60));
    // No step in the window: empty, also with a valid held value.
    assert_eq!(rate(&rs, T0 + 2 * MINUTE + SECOND, T0 + 3 * MINUTE).0, None);
    assert_eq!(rate(&rs[..1], T0, T0 + MINUTE).0, None);
    // A reset step counts its new value; a regression contributes 0, is
    // counted and the next step is measured from it (§5.5, §12 D2).
    let rs = vec![
        (T0, n(1000.0)),
        (T0 + MINUTE, n(5.0)),
        (T0 + 2 * MINUTE, n(4.0)),
        (T0 + 3 * MINUTE, n(10.0)),
    ];
    assert_eq!(rate(&rs, T0, T0 + 4 * MINUTE).0, q(11, 180));
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let c = m
        .define(
            "site/meter-1/energy",
            Kind::Counter,
            ValueType::Number,
            "kWh",
            3 * MINUTE,
        )
        .unwrap();
    m.ingest(
        rs.iter()
            .map(|(t, v)| Reading::num(c, *t, v.as_f64().unwrap()))
            .collect(),
    )
    .unwrap();
    assert_eq!(m.counter_regressions(c).unwrap(), 1);
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

/// §4 Overflow: catch-up resumes after the last position already queued for the
/// consumer in this session, not from its confirmed cursor, so nothing repeats.
#[test]
fn s4_overflow_catch_up_resumes_after_last_enqueued() {
    let c = ConsumerId(1);
    let consumer = ConsumerDecl {
        id: c,
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
    assert_eq!(
        m.disk.cursors[&c],
        Position::ORIGIN,
        "nothing confirmed yet"
    );
    m.drain().unwrap();
    let ps = seqs(m.observed[&c].iter().map(|o| o.position));
    assert_eq!(ps, vec![2, 3, 4, 5, 6], "no repeats, in order");
}

/// §5.2: coverage applies to every kind, text included.
#[test]
fn s5_2_text_nodes_carry_coverage() {
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

/// §8 Corrupt block: a sealed log segment that is neither uploaded nor
/// compacted is the only copy; corruption there loses its range and is
/// reported as a fatal health condition. It is never repaired in place.
#[test]
fn s8_corrupt_log_segment_with_no_other_copy_loses_range() {
    let (mut m, s) = model(10 * SECOND);
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
    let h = m.health();
    assert_eq!(h.level, Level::Fatal);
    assert!(h.reasons.contains(&Reason::Quarantined(f)));
}

/// §10 invariant 9: a restore preserves the positions held in sealed,
/// backed-up files; facts committed after the last upload are lost (§8).
#[test]
fn s10_9_restore_preserves_backed_up_positions() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.upload();
    let backed_up: Vec<Position> = m.facts().unwrap().iter().map(|f| f.position).collect();
    m.ingest(vec![Reading::num(s, T0 + SECOND, 2.0)]).unwrap();
    let r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    let restored: Vec<Position> = r.facts().unwrap().iter().map(|f| f.position).collect();
    assert_eq!(restored, backed_up);
    assert_eq!(m.facts().unwrap().len(), 2);
}

/// §11 Health: statistics counters are persisted on a best-effort basis; the
/// model keeps them across a restart.
#[test]
fn s11_counters_persist_across_restart() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0), Reading::num(s, T0, 1.0)])
        .unwrap();
    m.restart();
    assert_eq!(m.counters.duplicates_dropped, 1);
}

/// §3 Back-pressure: a full ingest queue makes the emit wait; the reading is not
/// refused, the wait is counted, and the same reading goes through once the
/// queue has drained.
#[test]
fn s3_full_ingest_queue_waits() {
    let mut m = Model::new(
        Config {
            ingest_capacity: 1,
            ..Config::default()
        },
        vec![],
        T0 + HOUR,
    );
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    m.emit(Reading::num(s, T0, 1.0)).unwrap();
    let waiting = Reading::num(s, T0 + SECOND, 2.0);
    assert_eq!(m.emit(waiting.clone()), Err(Wait::QueueFull.into()));
    assert_eq!(m.counters.waits[&Wait::QueueFull], 1);
    assert!(m.counters.refused.is_empty());
    m.commit(None).unwrap();
    m.emit(waiting).unwrap();
    m.commit(None).unwrap();
    assert_eq!(m.facts().unwrap().len(), 2);
}

/// §3 Recovery: after a reboot, a batch that reached the disk whole is kept,
/// but if the recovery sync fails the engine stays read-only with fatal health,
/// and nothing past the old mark becomes visible. A later restart whose sync
/// succeeds keeps the batch.
#[test]
fn s3_recovery_sync_failure_stays_read_only() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.emit(Reading::num(s, T0 + SECOND, 2.0)).unwrap();
    let r = m.commit(Some(Fault::PowerLoss(WriteOutcome::Survived)));
    assert_eq!(r, Err(Error::Crashed));
    let mark = m.disk.mark;
    m.restart_with(true);
    assert!(m.is_read_only());
    assert_eq!(m.health().level, Level::Fatal);
    assert_eq!(m.disk.mark, mark, "mark not moved");
    assert_eq!(m.facts().unwrap().len(), 1);
    assert_eq!(m.read(&m.open_reader()).unwrap().1.len(), 1);
    assert_eq!(m.commit(None), Err(Error::ReadOnly));
    m.restart();
    assert!(!m.is_read_only());
    assert_eq!(m.facts().unwrap().len(), 2);
    assert_eq!(m.disk.mark.unwrap().boot, m.boot);
}

const C: ConsumerId = ConsumerId(1);

fn consumer_model() -> (Model, SeriesId) {
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

/// §6 Edge stretches: the state transition from the reading before a window
/// to its first reading inside is credited to the window, like the counter step,
/// even when that reading is more than S before the window start.
#[test]
fn s6_leading_edge_credits_state_transition() {
    let p = Params::new(Kind::State, 2 * SECOND);
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

/// §6 Windows: every window is evaluated in event time against `now`. After a
/// forward jump of the system clock, `now` moves an hour ahead at once, so a
/// device-stamped series' 5-minute rolling window is empty although the series
/// keeps reporting. (Clock jumps in health are not modelled.)
#[test]
fn s6_rolling_window_in_event_time_after_clock_jump() {
    let (mut m, s) = consumer_model(); // S = 10 s
    let w0 = m.wall;
    let readings: Vec<Reading> = (1..=300)
        .map(|i| Reading::num(s, w0 - 300 * SECOND + i * SECOND, 1.0))
        .collect();
    m.ingest(readings).unwrap();
    let before = m.window(s, m.now - 300 * SECOND, m.now).unwrap();
    assert_eq!(before.coverage, i128::from(299 * SECOND));
    m.jump_wall(HOUR);
    m.advance(SECOND);
    // The device clock did not jump.
    m.ingest(vec![Reading::num(s, w0 + SECOND, 1.0)]).unwrap();
    assert_eq!(m.latest(s).unwrap().unwrap().t, w0 + SECOND);
    let after = m.window(s, m.now - 300 * SECOND, m.now).unwrap();
    assert_eq!((after.count, after.coverage), (0, 0));
}

/// §2.4: a reading recorded without a value is compared as "no value", so a
/// NaN and then an infinity at the same `t` and quality are one fact. (By the
/// specification a different supplied quality is not a duplicate; the model
/// still treats it as one: see `duplicate_check_compares_engine_assigned_quality`
/// in `model.rs`.)
#[test]
fn s2_4_reading_without_value_compared_as_no_value() {
    let (mut m, s) = consumer_model();
    let t = m.wall;
    m.ingest(vec![Reading::num(s, t, f64::NAN)]).unwrap();
    let r = m.ingest(vec![Reading::num(s, t, f64::INFINITY)]).unwrap();
    assert!(r.positions.is_empty());
    assert_eq!(m.counters.duplicates_dropped, 1);
    let f = &m.current().unwrap()[&(s, t)];
    assert_eq!((f.value.clone(), f.quality), (Value::Absent, Quality::Bad));
}

/// §3 Write or sync failure: after a failed sync the batch's fate is unknown;
/// after a reboot, recovery keeps it if it reached the disk whole. It was never
/// delivered, and the adapter's replay of it is absorbed as a duplicate.
#[test]
fn s3_failed_sync_then_power_loss_may_keep_batch() {
    let (mut m, s) = consumer_model();
    let reading = Reading::num(s, m.wall, 1.0);
    m.emit(reading.clone()).unwrap();
    assert_eq!(
        m.commit(Some(Fault::SyncError(WriteOutcome::Survived))),
        Err(Error::SyncFailed)
    );
    assert!(m.observed.get(&C).is_none_or(Vec::is_empty));
    m.power_loss();
    m.restart();
    assert_eq!(m.facts().unwrap().len(), 1);
    let r = m.ingest(vec![reading]).unwrap();
    assert!(r.positions.is_empty());
    assert_eq!(m.counters.duplicates_dropped, 1);
}

/// §3 Recovery: after a reboot, recovery writes the mark with the new boot id
/// even when nothing was past it, so a later process crash on the same boot
/// truncates as row 7 requires.
#[test]
fn s3_recovery_writes_mark_after_reboot() {
    let (mut m, s) = consumer_model();
    m.power_loss();
    m.restart();
    assert_eq!(m.disk.mark.unwrap().boot, m.boot);
    m.emit(Reading::num(s, m.wall, 1.0)).unwrap();
    let r = m.commit(Some(Fault::Crash(CrashPoint::AfterWrite(
        WriteOutcome::Survived,
    ))));
    assert_eq!(r, Err(Error::Crashed));
    m.restart();
    assert!(m.facts().unwrap().is_empty());
}

/// §3 Recovery: a missing mark counts as a different boot, so a batch written
/// but not synced before a process crash on the same boot is kept; recovery
/// syncs it before anything sees it, and it was never acknowledged.
#[test]
fn s3_missing_mark_counts_as_different_boot() {
    let (mut m, s) = consumer_model();
    m.emit(Reading::num(s, m.wall, 1.0)).unwrap();
    let r = m.commit(Some(Fault::Crash(CrashPoint::AfterWrite(
        WriteOutcome::Survived,
    ))));
    assert_eq!(r, Err(Error::Crashed));
    m.disk.mark = None;
    m.restart();
    assert_eq!(m.facts().unwrap().len(), 1);
    assert_eq!(m.disk.mark.unwrap().boot, m.boot);
}

fn backed_up_model() -> (Model, SeriesId) {
    let c = ConsumerDecl {
        id: ConsumerId(1),
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![c], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            10 * SECOND,
        )
        .unwrap();
    for i in 0..4 {
        m.ingest(vec![Reading::num(s, T0 + i * SECOND, i as f64)])
            .unwrap();
    }
    m.seal_active();
    m.upload();
    (m, s)
}

/// §2.2, §8 Restore step 1: a restore claims the next generation and starts it
/// at sequence 1; a restart keeps the generation, and a restore of the
/// restored store claims another.
#[test]
fn s8_restore_claims_next_generation_from_sequence_1() {
    let (mut m, s) = backed_up_model();
    let end = m.disk.all_entries().keys().next_back().copied().unwrap();
    assert_eq!(end, Position::new(0, 5));
    let mut r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    assert_eq!((r.disk.generation, r.disk.restored_end), (1, Some(end)));
    assert!(m.disk.bucket.claims.contains(&1));
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 1)]);
    r.crash();
    r.restart();
    assert_eq!(r.disk.generation, 1);
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR + 1, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 2)]);
    r.seal_active();
    r.upload();
    let r2 =
        Model::restore_from(r.config.clone(), vec![], &mut r.disk.bucket, r.wall, false).unwrap();
    assert_eq!(r2.disk.generation, 2);
    assert_eq!(r2.disk.restored_end, Some(Position::new(1, 2)));
}

/// §8 Generations: two restores from one bucket claim different generations,
/// so the same position never names two facts.
#[test]
fn s8_two_restores_from_one_bucket_claim_different_generations() {
    let (m, s) = backed_up_model();
    let mut bucket = m.disk.bucket.clone();
    let mut a = Model::restore_from(m.config.clone(), vec![], &mut bucket, m.wall, false).unwrap();
    let mut b = Model::restore_from(m.config.clone(), vec![], &mut bucket, m.wall, false).unwrap();
    let pa = a.ingest(vec![Reading::num(s, T0 + HOUR, 2.0)]).unwrap();
    let pb = b.ingest(vec![Reading::num(s, T0 + HOUR, 3.0)]).unwrap();
    assert_eq!(pa.positions, vec![Position::new(1, 1)]);
    assert_eq!(pb.positions, vec![Position::new(2, 1)]);
}

/// §8 Generations: a restore from a bucket with nothing backed up is refused.
#[test]
fn s8_restore_from_empty_bucket_refused() {
    let (mut m, _) = model(10 * SECOND);
    let r = Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false);
    assert_eq!(r.err(), Some(Error::EmptyBucket));
    assert!(m.disk.bucket.claims.is_empty());
}

/// §8 Restore drill: a drill restores read-only and claims nothing.
#[test]
fn s8_drill_is_read_only_and_claims_nothing() {
    let (mut m, s) = backed_up_model();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        vec![],
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        false,
        true,
    )
    .unwrap();
    assert!(m.disk.bucket.claims.is_empty());
    assert_eq!(r.facts().unwrap().len(), 4);
    assert!(r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).is_err());
    assert_eq!(r.upload(), 0);
}

/// §8 Generations: a writer that finds a higher generation claimed stops
/// uploading and becomes read-only.
#[test]
fn s8_superseded_writer_stops_uploading() {
    let (mut m, s) = backed_up_model();
    let _r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    m.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    m.seal_active();
    let before = m.disk.bucket.objects.len();
    assert_eq!(m.upload(), 0);
    assert_eq!(m.disk.bucket.objects.len(), before);
    assert!(m.is_read_only());
}

/// §8 Restore step 1: the restored end is the last position before the first
/// gap; later positions are set aside, outside views and delivery, and a
/// cursor below the gap catches up only to the restored end.
#[test]
fn s8_restored_end_before_first_gap() {
    let (mut m, s) = backed_up_model(); // positions 0:1 to 0:5 backed up
    m.ingest(vec![Reading::num(s, T0 + 10 * SECOND, 10.0)])
        .unwrap(); // 0:6
    m.seal_active();
    let missing = *m.disk.files.keys().next_back().unwrap();
    m.ingest(vec![Reading::num(s, T0 + 11 * SECOND, 11.0)])
        .unwrap(); // 0:7
    m.seal_active();
    m.upload();
    m.disk.bucket.objects.remove(&missing);
    let c = ConsumerId(1);
    let cursors = [(c, Position::new(0, 4))].into_iter().collect();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &cursors,
        m.wall,
        false,
        false,
    )
    .unwrap();
    assert_eq!(r.disk.restored_end, Some(Position::new(0, 5)));
    assert!(
        r.facts()
            .unwrap()
            .iter()
            .all(|f| f.position <= Position::new(0, 5))
    );
    r.drain().unwrap();
    let got: Vec<Position> = r.observed[&c].iter().map(|o| o.position).collect();
    assert_eq!(got, vec![Position::new(0, 5)]);
}

/// §8 Restore drill: a drill sets aside what a restore would, so positions
/// beyond the first gap stay out of its views.
#[test]
fn s8_drill_sets_aside_beyond_restored_end() {
    let (mut m, s) = backed_up_model(); // positions 0:1 to 0:5 backed up
    m.ingest(vec![Reading::num(s, T0 + 10 * SECOND, 10.0)])
        .unwrap(); // 0:6
    m.seal_active();
    let missing = *m.disk.files.keys().next_back().unwrap();
    m.ingest(vec![Reading::num(s, T0 + 11 * SECOND, 11.0)])
        .unwrap(); // 0:7
    m.seal_active();
    m.upload();
    m.disk.bucket.objects.remove(&missing);
    let r = Model::restore_with_cursors(
        m.config.clone(),
        vec![],
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        false,
        true,
    )
    .unwrap();
    assert!(m.disk.bucket.claims.is_empty());
    assert_eq!(r.disk.restored_end, Some(Position::new(0, 5)));
    assert!(
        r.facts()
            .unwrap()
            .iter()
            .all(|f| f.position <= Position::new(0, 5))
    );
}

/// §8 Restore step 2: a cursor at or below the restored end catches up to it,
/// flagged `replayed`; the consumer then receives `generation_changed` and
/// `resync` like every other.
#[test]
fn s8_cursor_at_or_below_restored_end_continues() {
    let (mut m, _) = backed_up_model();
    let c = ConsumerId(1);
    let cursor = Position::new(0, 3);
    let cursors = [(c, cursor)].into_iter().collect();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &cursors,
        m.wall,
        false,
        false,
    )
    .unwrap();
    r.drain().unwrap();
    let got: Vec<(Position, bool)> = r.observed[&c]
        .iter()
        .map(|o| (o.position, o.replayed))
        .collect();
    assert_eq!(
        got,
        vec![(Position::new(0, 4), true), (Position::new(0, 5), true)]
    );
    assert_eq!(
        r.signals[&c],
        vec![
            Signal::GenerationChanged {
                cursor: Some(cursor),
                restored_end: Position::new(0, 5)
            },
            Signal::ResyncDue
        ]
    );
}

/// §8 Restore step 2: "A consumer without a cursor is treated as beyond the
/// restored end"; cursor files are not backed up. It is not replayed, and its
/// `generation_changed` says it kept no cursor.
#[test]
fn s8_missing_cursor_counts_as_beyond_restored_end() {
    let (mut m, _) = backed_up_model();
    let c = ConsumerId(1);
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        false,
        false,
    )
    .unwrap();
    r.drain().unwrap();
    assert!(r.observed.get(&c).is_none_or(|o| o.is_empty()));
    assert_eq!(
        r.signals[&c],
        vec![
            Signal::GenerationChanged {
                cursor: None,
                restored_end: Position::new(0, 5)
            },
            Signal::ResyncDue
        ]
    );
}

/// §8 Restore step 2: a cursor beyond the restored end gets
/// `generation_changed` and `resync`, no replay of the old generation, and the
/// new generation live from its first position.
#[test]
fn s8_cursor_beyond_restored_end_generation_changed() {
    let (mut m, s) = backed_up_model();
    let c = ConsumerId(1);
    let cursor = Position::new(0, 9);
    let cursors = [(c, cursor)].into_iter().collect();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &cursors,
        m.wall,
        false,
        false,
    )
    .unwrap();
    assert_eq!(
        r.signals[&c],
        vec![
            Signal::GenerationChanged {
                cursor: Some(cursor),
                restored_end: Position::new(0, 5)
            },
            Signal::ResyncDue
        ]
    );
    r.drain().unwrap();
    assert!(r.observed.get(&c).is_none_or(|o| o.is_empty()));
    r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    r.drain().unwrap();
    let got: Vec<(Position, bool)> = r.observed[&c]
        .iter()
        .map(|o| (o.position, o.replayed))
        .collect();
    assert_eq!(got, vec![(Position::new(1, 1), false)]);
}

/// §8 Generations: "A writer that finds a higher generation claimed stops
/// uploading and becomes read-only (fatal health, §3)." Read-only survives a
/// restart, so the superseded writer never accepts ingest again.
#[test]
fn s8_superseded_writer_stays_read_only_after_restart() {
    let (mut m, s) = backed_up_model();
    let _r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    assert_eq!(m.upload(), 0);
    assert!(m.is_read_only());
    m.crash();
    m.restart();
    assert!(m.is_read_only());
    assert_eq!(
        m.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).err(),
        Some(Error::ReadOnly)
    );
    assert_eq!(m.upload(), 0);
}

/// §8 Generations: "A writer that finds a higher generation claimed stops
/// uploading and becomes read-only (fatal health, §3)." The health report
/// names the cause, not a failed sync.
#[test]
fn s8_superseded_writer_reports_fatal_superseded() {
    let (mut m, _) = backed_up_model();
    let _r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    m.upload();
    let h = m.health();
    assert_eq!(h.level, Level::Fatal);
    assert!(h.reasons.contains(&Reason::Superseded));
    assert!(!h.reasons.contains(&Reason::SyncFailed));
    m.crash();
    m.restart();
    assert!(m.health().reasons.contains(&Reason::Superseded));
}

/// §8 Restore drill: "A drill restores read-only and claims nothing." Being
/// read-only by design, a drill reports no health condition of its own.
#[test]
fn s8_drill_reports_no_fatal_health() {
    let (mut m, _) = backed_up_model();
    let r = Model::restore_with_cursors(
        m.config.clone(),
        vec![],
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        false,
        true,
    )
    .unwrap();
    assert!(r.is_read_only());
    let h = r.health();
    assert_eq!(h.level, Level::Ok);
    assert!(!h.reasons.contains(&Reason::SyncFailed));
}

/// §8 Restore step 1: "The restored end is the last position before the first
/// gap in the restored log". Every store begins in generation 0, so with no
/// generation-0 position restored the log has a gap before its first position
/// and the restored end is before everything.
#[test]
fn s8_restored_end_starts_at_generation_0() {
    let (mut m, s) = backed_up_model();
    let mut r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    for i in 0..3 {
        r.ingest(vec![Reading::num(s, T0 + HOUR + i * SECOND, 1.0)])
            .unwrap(); // 1:1 to 1:3
    }
    r.seal_active();
    r.upload();
    let mut bucket = r.disk.bucket.clone();
    bucket
        .objects
        .retain(|_, (_, f)| f.entries().all(|e| e.position.generation > 0));
    assert!(!bucket.objects.is_empty());
    let r2 = Model::restore_from(r.config.clone(), vec![], &mut bucket, r.wall, false).unwrap();
    assert_eq!(r2.disk.restored_end, Some(Position::ORIGIN));
    assert!(r2.facts().unwrap().is_empty());
}

/// The bucket's log segment holding `p`.
fn segment_holding(m: &Model, p: Position) -> FileId {
    *m.disk
        .bucket
        .objects
        .iter()
        .find(|(_, (_, f))| f.class == FileClass::Segment && f.entries().any(|e| e.position == p))
        .unwrap()
        .0
}

/// §8 Continuous backup: "a tree checkpoint at least daily, and only after
/// all log up to its position is backed up. Each tree checkpoint carries the
/// registry file and each series' latest recorded `t`, as of its position."
#[test]
fn s8_checkpoint_carries_registry_and_latest_t_after_log_backed_up() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.ingest(vec![Reading::num(s, T0 + SECOND, 2.0)]).unwrap();
    let p = m.checkpoint().unwrap();
    assert_eq!(
        m.upload_checkpoints(),
        0,
        "its log is still in the open segment"
    );
    m.ingest(vec![Reading::num(s, T0 + 2 * SECOND, 3.0)])
        .unwrap();
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    let cp = &m.disk.bucket.checkpoints[&p];
    assert_eq!(cp.position, p);
    assert_eq!(cp.latest_t[&s], T0 + SECOND);
    assert!(cp.registry.get(s).is_some());
    assert!(cp.trees[&s].readings().is_empty(), "nodes only, no facts");
    assert_eq!(cp.trees[&s].node(0, T0).count, 2);
}

/// §8 Restore step 1: "Write one manifest record (generation, checkpoint,
/// restored end) before ingest is accepted."
#[test]
fn s8_restore_record_written_before_ingest() {
    let (mut m, s, cp) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let record = ManifestOp::Restore {
        generation: 1,
        checkpoint: Some(cp),
        restored_end: Position::new(0, 8),
    };
    let at = r.disk.manifest.iter().position(|x| x.op == record).unwrap();
    assert_eq!(
        r.disk
            .manifest
            .iter()
            .filter(|x| matches!(x.op, ManifestOp::Restore { .. }))
            .count(),
        1
    );
    assert!(r.restoring());
    // A drill raises no health condition of its own, also while restoring.
    let drill = Model::restore_with_cursors(
        m.config.clone(),
        vec![],
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        true,
        true,
    )
    .unwrap();
    assert!(drill.restoring() && drill.is_read_only());
    assert_eq!(drill.health().level, Level::Ok);
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 1)]);
    let first_new = r
        .disk
        .manifest
        .iter()
        .position(|x| {
            matches!(&x.op, ManifestOp::Publish { added, .. }
                if added.iter().any(|a| r.disk.files[&a.file].entries().any(|e| e.position == Position::new(1, 1))))
        })
        .unwrap();
    assert!(at < first_new);
}

/// §8 Restore step 1: "Download the latest tree checkpoint at or below the
/// restored end (labelled derived-unverified), with its registry file and
/// latest `t` per series, and the sealed log segments after its position;
/// replay them into the tree."
#[test]
fn s8_restore_from_latest_checkpoint_at_or_below_restored_end() {
    let (mut m, s, cp) = checkpointed_model();
    m.ingest(vec![Reading::num(s, T0 + 250 * SECOND, 6.0)])
        .unwrap(); // 0:9
    m.seal_active();
    let missing = segment_holding_local(&m, Position::new(0, 9));
    m.ingest(vec![Reading::num(s, T0 + 260 * SECOND, 7.0)])
        .unwrap(); // 0:10
    let beyond = m.checkpoint().unwrap();
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    assert!(m.disk.bucket.checkpoints.contains_key(&beyond));
    m.disk.bucket.objects.remove(&missing);
    let r = restore_in_progress(&mut m, &BTreeMap::new());
    assert_eq!(r.disk.restored_end, Some(Position::new(0, 8)));
    assert_eq!(
        r.disk
            .restore
            .as_ref()
            .unwrap()
            .checkpoint
            .as_ref()
            .unwrap()
            .position,
        cp
    );
    let local: Vec<Position> = r.facts().unwrap().iter().map(|f| f.position).collect();
    assert_eq!(local, vec![Position::new(0, 7), Position::new(0, 8)]);
    assert!(r.registry().unwrap().get(s).is_some());
    assert!(r.checkpoint_unverified());
    // The checkpoint's node, and a minute computed from the replayed log.
    assert_eq!(
        r.tree(s).unwrap().node(0, T0),
        m.tree(s).unwrap().node(0, T0)
    );
    assert_eq!(
        r.tree(s).unwrap().node(0, T0 + 3 * MINUTE),
        m.tree(s).unwrap().node(0, T0 + 3 * MINUTE)
    );
}

fn segment_holding_local(m: &Model, p: Position) -> FileId {
    *m.disk
        .files
        .iter()
        .find(|(_, f)| f.class == FileClass::Segment && f.entries().any(|e| e.position == p))
        .unwrap()
        .0
}

/// §8 Restore step 1: "With no checkpoint, download the whole sealed log."
#[test]
fn s8_restore_without_checkpoint_downloads_whole_log() {
    let (mut m, _) = backed_up_model();
    let r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, true).unwrap();
    assert!(r.disk.restore.as_ref().unwrap().checkpoint.is_none());
    assert_eq!(r.facts().unwrap().len(), 4);
    assert!(!r.restoring());
    assert!(!r.checkpoint_unverified());
}

/// §8 Restore step 1: "the sealed log segments after its position; replay
/// them into the tree", with row 29: "Late fact for a minute whose detail is
/// not local: Recorded; node `summary_stale` until fetched".
#[test]
fn s8_replayed_minute_without_its_detail_is_stale() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.tree(s).unwrap().is_stale(0, T0 + 2 * MINUTE));
    assert!(!r.tree(s).unwrap().is_stale(0, T0 + 3 * MINUTE));
    r.restore_rest().unwrap();
    assert!(!r.tree(s).unwrap().is_stale(0, T0 + 2 * MINUTE));
    assert_eq!(
        r.tree(s).unwrap().node(0, T0 + 2 * MINUTE),
        m.tree(s).unwrap().node(0, T0 + 2 * MINUTE)
    );
}

/// As above for a series that stores only the 15-minute level (§5.1): a
/// minute the replay touches inside a quarter-hour the checkpoint holds is
/// flagged `summary_stale` until the detail is back, since its other minutes
/// are not local; then the quarter-hour equals the live store's.
#[test]
fn s8_replayed_minute_of_a_series_without_a_1_minute_level_is_stale() {
    let (mut m, s, _) = checkpointed_model_with(30 * SECOND);
    assert!(m.tree(s).unwrap().stored(0).next().is_none());
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.tree(s).unwrap().stored(0).next().is_none());
    assert!(r.tree(s).unwrap().is_stale(0, T0 + 2 * MINUTE));
    assert!(r.tree(s).unwrap().is_stale(0, T0 + 3 * MINUTE));
    assert!(r.query_window(s, T0, T0 + QUARTER).unwrap().summary_stale);
    r.restore_rest().unwrap();
    assert!(!r.tree(s).unwrap().is_stale(1, T0));
    assert_eq!(
        r.tree(s).unwrap().node(1, T0),
        m.tree(s).unwrap().node(1, T0)
    );
    assert_eq!(r.counters.checkpoint_differences, 0);
}

/// §8 Restore step 2: "The store serves queries at ≥ 1-minute resolution,
/// flagged `restoring`".
#[test]
fn s8_restoring_queries_at_one_minute_resolution() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let a = r.query_window(s, T0, T0 + MINUTE).unwrap();
    assert!(a.restoring && !a.summary_stale);
    assert_eq!(a.node, m.window(s, T0, T0 + MINUTE).unwrap());
    // The reading held into an empty minute comes from the checkpoint's node.
    let b = r.query_window(s, T0 + MINUTE, T0 + 2 * MINUTE).unwrap();
    assert_eq!(b.node, m.window(s, T0 + MINUTE, T0 + 2 * MINUTE).unwrap());
    assert_eq!(b.node.coverage, i128::from(MINUTE));
    assert!(
        r.query_window(s, T0 + 2 * MINUTE, T0 + 3 * MINUTE)
            .unwrap()
            .summary_stale
    );
    r.restore_rest().unwrap();
    let c = r.query_window(s, T0 + SECOND, T0 + 4 * MINUTE).unwrap();
    assert!(!c.restoring);
    assert_eq!(c.node, m.window(s, T0 + SECOND, T0 + 4 * MINUTE).unwrap());
}

/// §8 Restore step 2: "A cursor of an older generation at or below the
/// restored end receives the facts after it up to the restored end through
/// catch-up, flagged `replayed`, if their segments are local; otherwise §4
/// Resync applies."
#[test]
fn s8_cursor_catches_up_only_if_its_segments_are_local() {
    let (mut m, _, _) = checkpointed_model();
    let c = ConsumerId(1);
    let end = Position::new(0, 8);
    for (cursor, want) in [
        (
            Position::new(0, 6),
            vec![(Position::new(0, 7), true), (end, true)],
        ),
        (Position::new(0, 3), vec![]),
    ] {
        let cursors = [(c, cursor)].into_iter().collect();
        let mut r = restore_in_progress(&mut m, &cursors);
        r.restore_rest().unwrap();
        r.drain().unwrap();
        let got: Vec<(Position, bool)> = r
            .observed
            .get(&c)
            .map(|o| o.iter().map(|x| (x.position, x.replayed)).collect())
            .unwrap_or_default();
        assert_eq!(got, want, "cursor {cursor}");
        assert_eq!(r.disk.cursors[&c], end);
        assert!(matches!(
            r.signals[&c][..],
            [Signal::GenerationChanged { .. }, Signal::ResyncDue]
        ));
    }
}

/// §8 Restore step 2: "Until the revised-key index is back, facts whose `t`
/// is at or before the latest `t` known for their series (from the checkpoint
/// or the restored log) and which match no local fact are recorded and
/// counted `duplicates_unchecked`."
#[test]
fn s8_duplicates_unchecked_until_revised_key_index_back() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    // Later than the latest known `t` (200 s): recorded, not counted.
    let p = r
        .ingest(vec![Reading::num(s, T0 + 300 * SECOND, 9.0)])
        .unwrap();
    assert_eq!((p.positions.len(), r.counters.duplicates_unchecked), (1, 0));
    // At or before it and matching a local fact: dropped.
    let p = r
        .ingest(vec![Reading::num(s, T0 + 200 * SECOND, 5.0)])
        .unwrap();
    assert_eq!((p.positions.len(), p.duplicates), (0, 1));
    // At or before it and matching no local fact: recorded and counted.
    let p = r.ingest(vec![Reading::num(s, T0, 0.0)]).unwrap();
    assert_eq!((p.positions.len(), r.counters.duplicates_unchecked), (1, 1));
}

/// §8 Restore step 3: "Download current chunks and the revised-key index,
/// then revision chunks and log segments."
#[test]
fn s8_download_order_current_then_revision_then_log() {
    let (mut m, _, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let mut order = Vec::new();
    while let Some(f) = r.restore_step().unwrap() {
        order.push(r.disk.files[&f].class);
    }
    // Current chunks of the registry entries and of the series.
    assert_eq!(
        order,
        vec![
            FileClass::Current,
            FileClass::Current,
            FileClass::Revision,
            FileClass::Segment
        ]
    );
    assert!(!r.restoring());
}

/// §8 Restore step 3: "A position present in two files is one fact and must
/// have identical content."
#[test]
fn s8_position_in_two_files_must_have_identical_content() {
    let (mut m, _, _) = checkpointed_model();
    let p = Position::new(0, 2);
    let log = segment_holding(&m, p);
    let (_, file) = m.disk.bucket.objects.get_mut(&log).unwrap();
    for e in file.batches.iter_mut().flatten() {
        if let (true, EntryBody::Fact(f)) = (e.position == p, &mut e.body) {
            f.value = n(7.0);
        }
    }
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert_eq!(
        r.restore_rest(),
        Err(Error::Conflict {
            file: log,
            position: p
        })
    );
}

/// §8 Restore step 4: "When detail for a span is back, recompute its nodes
/// from facts and compare with the checkpoint; any difference is reported and
/// the recomputed value wins. The checkpoint label is removed when all detail
/// has been verified."
#[test]
fn s8_detail_back_verifies_checkpoint_and_removes_label() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.checkpoint_unverified());
    r.restore_step().unwrap(); // current chunk of the registry entries
    r.restore_step().unwrap(); // current chunk of the series
    assert!(r.checkpoint_unverified(), "a revision is still remote");
    r.restore_step().unwrap(); // revision chunk: every fact of the span is local
    assert!(!r.checkpoint_unverified());
    r.restore_rest().unwrap();
    assert!(r.disk.restore.as_ref().unwrap().differences.is_empty());
    assert_eq!(r.counters.checkpoint_differences, 0);
    assert!(r.tree(s).unwrap().same_summaries(m.tree(s).unwrap()));
}

/// §8 Restore step 4: "recompute its nodes from facts and compare with the
/// checkpoint; any difference is reported and the recomputed value wins."
#[test]
fn s8_checkpoint_difference_reported_and_recomputed_wins() {
    let (mut m, s, cp) = checkpointed_model();
    let held = m.disk.bucket.checkpoints.get_mut(&cp).unwrap();
    let params = held.trees[&s].params;
    held.trees
        .insert(s, SeriesTree::build(params, &[(T0, n(50.0))]).nodes_only());
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert_eq!(
        r.window(s, T0, T0 + MINUTE).unwrap().first,
        Some((T0, n(50.0)))
    );
    r.restore_rest().unwrap();
    assert_eq!(r.disk.restore.as_ref().unwrap().differences, vec![(s, T0)]);
    assert_eq!(r.counters.checkpoint_differences, 1);
    assert!(r.tree(s).unwrap().same_summaries(m.tree(s).unwrap()));
    assert!(!r.checkpoint_unverified());
}

/// §8 Restore: "A restart after the restore record is written resumes the
/// restore in its generation, fetching the files listed by the manifest
/// records it restores from that are not yet local."
#[test]
fn s8_restart_after_record_resumes_in_its_generation() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    r.restore_step().unwrap();
    r.crash();
    r.restart();
    assert_eq!(r.disk.generation, 1);
    assert!(r.restoring());
    assert_eq!(r.restore_queue().len(), 3);
    r.restore_rest().unwrap();
    assert_eq!(r.current().unwrap(), m.current().unwrap());
    assert!(r.tree(s).unwrap().same_summaries(m.tree(s).unwrap()));
    assert!(!r.checkpoint_unverified());
    assert_eq!(r.disk.generation, 1);
    assert_eq!(r.disk.bucket.claims, [1].into_iter().collect());
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 1)]);
}

/// §8 Restore: "A crash before the record starts the restore again with the
/// next generation."
#[test]
fn s8_crash_before_record_restarts_with_next_generation() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = Model::restore_interrupted_before_record(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
    )
    .unwrap();
    assert!(!r.is_up());
    assert!(m.disk.bucket.claims.contains(&1));
    assert!(
        !r.disk
            .manifest
            .iter()
            .any(|x| matches!(x.op, ManifestOp::Restore { .. }))
    );
    r.restart();
    assert_eq!(r.disk.generation, 2);
    assert!(r.disk.bucket.claims.contains(&1) && r.disk.bucket.claims.contains(&2));
    assert!(
        r.disk
            .manifest
            .iter()
            .any(|x| matches!(x.op, ManifestOp::Restore { generation: 2, .. }))
    );
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(2, 1)]);
}

/// §8 Restore step 2: "The store serves queries at ≥ 1-minute resolution".
/// Resolution is that of the requested bounds: a whole-minute window reaching
/// past a `now` inside a minute is served.
#[test]
fn s8_restoring_window_past_now_inside_a_minute() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    r.advance(30 * SECOND);
    let a = r.query_window(s, T0 + HOUR, T0 + HOUR + MINUTE).unwrap();
    assert!(a.restoring);
    assert_eq!(
        r.query_window(s, T0 + HOUR, T0 + HOUR + 30 * SECOND),
        Err(Error::Restoring)
    );
}

/// §8 Restore: "A restart after the restore record is written resumes the
/// restore in its generation"; step 1 writes the record "before ingest is
/// accepted", then downloads the checkpoint and "the sealed log segments after
/// its position; replay them into the tree". A crash during step 1 therefore
/// finishes step 1, with the latest `t` per series, before ingest.
#[test]
fn s8_crash_during_step_1_finishes_it_before_ingest() {
    let (mut m, s, _) = checkpointed_model();
    let c = ConsumerId(1);
    let cursors = [(c, Position::new(0, 6))].into_iter().collect();
    let mut r = Model::restore_interrupted_after_record(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &cursors,
        m.wall,
        0,
    )
    .unwrap();
    assert!(!r.is_up());
    assert!(!r.disk.restore.as_ref().unwrap().step_one_done);
    r.restart();
    assert_eq!(r.disk.generation, 1);
    let local: Vec<Position> = r.facts().unwrap().iter().map(|f| f.position).collect();
    assert_eq!(local, vec![Position::new(0, 7), Position::new(0, 8)]);
    // The latest `t` known (200 s, from the log after the checkpoint) counts.
    let p = r
        .ingest(vec![Reading::num(s, T0 + 150 * SECOND, 9.0)])
        .unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 1)]);
    assert_eq!(r.counters.duplicates_unchecked, 1);
    r.drain().unwrap();
    let got: Vec<Position> = r.observed[&c].iter().map(|o| o.position).collect();
    assert_eq!(
        got,
        vec![
            Position::new(0, 7),
            Position::new(0, 8),
            Position::new(1, 1)
        ]
    );
}

// ----- delivery (§4, §6) -----

const F: FunnelId = FunnelId(1);

/// A consumer with no configured selection and two gauges, `a` and `b`.
fn routed_model(capacity: usize) -> (Model, SeriesId, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(Default::default()),
        capacity,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let a = m
        .define(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
        )
        .unwrap();
    let b = m
        .define(
            "site/pump-3/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
        )
        .unwrap();
    (m, a, b)
}

/// One fact for `s` at the current wall time; returns its position.
fn put(m: &mut Model, s: SeriesId, v: f64) -> Position {
    let now = m.wall;
    m.ingest(vec![Reading::num(s, now, v)]).unwrap().positions[0]
}

fn threshold() -> Condition {
    Condition::Threshold {
        on: 50.0,
        clear: 40.0,
    }
}

/// The condition part of the one condition funnel in the last `resync`
/// consumer `C` received.
fn last_resync_condition(m: &Model) -> (ConditionState, bool) {
    let r = m.received[&C]
        .iter()
        .rev()
        .find_map(|r| r.delivery.resync().cloned())
        .expect("a resync");
    let c = r
        .funnels
        .iter()
        .find_map(|f| f.condition.clone())
        .expect("a condition funnel");
    (c.state, c.transition)
}

/// §4 Future facts and §6 Cadence: a stale-for condition on a forecast
/// series, and any condition over one without a windowed operator, are
/// rejected when funnels are compiled; `resync` for a funnel over a forecast
/// series carries condition state only. A stale-for condition on a scheduled
/// series is not modelled and refused.
#[test]
fn s4_funnels_over_scheduled_and_forecast_series() {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(BTreeSet::new()),
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let def = |m: &mut Model, name: &str, options| {
        m.define_with(
            name,
            Kind::Gauge,
            ValueType::Number,
            "1",
            10 * MINUTE,
            options,
        )
        .unwrap()
    };
    let f = def(
        &mut m,
        "site/weather-1/irradiance",
        SeriesOptions {
            versioned: Some(ChangedAtKind::Time),
            ..SeriesOptions::timing(Timing::Forecast)
        },
    );
    let sched = def(
        &mut m,
        "site/tariff-1/price",
        SeriesOptions::timing(Timing::Scheduled),
    );
    let stale = Condition::StaleFor {
        staleness: 10 * MINUTE,
        n: 0,
    };
    let above = Condition::Threshold {
        on: 5.0,
        clear: 4.0,
    };
    let avg = Operator {
        aggregate: Aggregate::Avg,
        width: HOUR,
    };
    for (s, op, cond) in [
        (f, None, stale.clone()),
        (f, Some(avg.clone()), stale.clone()),
        (f, None, above.clone()),
        (sched, None, stale),
    ] {
        assert!(matches!(
            m.add_route(C, FunnelId(1), Funnel::condition(s, op, cond)),
            Err(Error::BadRoute(_))
        ));
    }
    m.add_route(
        C,
        FunnelId(1),
        Funnel::condition(f, Some(avg), above.clone()),
    )
    .unwrap();
    m.add_route(C, FunnelId(2), Funnel::condition(sched, None, above))
        .unwrap();
    let now = m.wall;
    let at = u64::try_from(now).unwrap();
    m.ingest(vec![
        Reading::versioned(f, now - 30 * MINUTE, 9.0, at),
        Reading::versioned(f, now + HOUR, 9.0, at),
        Reading::num(sched, now - MINUTE, 1.0),
        Reading::num(sched, now + 10 * MINUTE, 9.0),
    ])
    .unwrap();
    m.drain().unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let r = m.received[&C]
        .iter()
        .rev()
        .find_map(|r| r.delivery.resync().cloned())
        .unwrap();
    let state = |id| r.funnels.iter().find(|x| x.funnel == id).unwrap().clone();
    assert_eq!(
        state(FunnelId(1)).live,
        vec![(f, None)],
        "condition state only"
    );
    assert_eq!(
        state(FunnelId(1)).condition.unwrap().state,
        ConditionState::Active(true)
    );
    // The scheduled value for later takes effect at its `t`, seen through
    // the condition funnel's timer evaluation.
    assert_eq!(
        state(FunnelId(2)).condition.unwrap().state,
        ConditionState::Active(false)
    );
    m.advance(10 * MINUTE);
    m.tick().unwrap();
    m.drain().unwrap();
    let fired = m.received[&C].iter().any(|r| {
        r.delivery.trigger().is_some_and(|t| {
            t.funnel == FunnelId(2)
                && t.state == ConditionState::Active(true)
                && t.cause == TriggerCause::Timer
        })
    });
    assert!(fired);
}

/// §4 Routing: "Changes are log entries and take effect at their position;
/// catch-up applies the routes in force at each position." A route added at
/// run time routes only later facts, a removed one none after its removal,
/// live and in catch-up alike.
#[test]
fn s4_run_time_routes_take_effect_at_their_position() {
    let (mut m, a, b) = routed_model(100);
    put(&mut m, a, 1.0);
    let added = m
        .add_route(C, F, Funnel::facts(Selection::Series([a].into())))
        .unwrap();
    let p2 = put(&mut m, a, 2.0);
    put(&mut m, b, 3.0);
    let removed = m.remove_route(C, F).unwrap();
    put(&mut m, a, 4.0);
    assert!(added < p2 && p2 < removed);
    assert!(
        m.entries()
            .unwrap()
            .iter()
            .any(|e| e.position == added && matches!(e.body, EntryBody::Route(_)))
    );
    m.drain().unwrap();
    assert_eq!(labels(&session_stream(&m, C, 1)), ["F5"]);
    // Nothing confirmed: after a crash the whole log is replayed with the
    // routes in force at each position.
    m.disk.cursors.insert(C, Position::ORIGIN);
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(labels(&session_stream(&m, C, 2)), ["R5", "S"]);
    // A funnel id in force cannot be added twice, nor one not in force removed.
    assert!(matches!(m.remove_route(C, F), Err(Error::BadRoute(_))));
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    assert!(matches!(
        m.add_route(C, F, Funnel::facts(Selection::All)),
        Err(Error::BadRoute(_))
    ));
    assert!(matches!(
        m.add_route(ConsumerId(9), F, Funnel::facts(Selection::All)),
        Err(Error::UnknownConsumer(_))
    ));
}

/// §4 Resync: "Sent on resume after downtime (§6)"; §6 Restart and resume:
/// "Facts after the cursor are delivered flagged `replayed` … The funnel then
/// sends `resync`." Live delivery follows the `resync`.
#[test]
fn s4_resync_follows_replay_on_restart() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    let p1 = put(&mut m, a, 1.0);
    m.drain().unwrap();
    put(&mut m, a, 2.0);
    put(&mut m, a, 3.0);
    m.poll(C, 1).unwrap(); // taken, never confirmed
    assert_eq!(m.disk.cursors[&C], p1);
    m.crash();
    m.restart();
    m.catch_up(C, usize::MAX).unwrap();
    put(&mut m, a, 4.0);
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    assert_eq!(labels(&stream), ["R5", "R6", "S", "F7"]);
    assert_eq!(
        stream[2].position,
        Position::new(0, 6),
        "complete up to 0:6"
    );
    assert_eq!(m.signals[&C], [Signal::ResyncDue]);
    // The `resync` carries the live state of each selected series.
    let r = stream[2].resync().unwrap();
    assert_eq!(r.replaced, None);
    let live = &r.funnels.iter().find(|f| f.funnel == F).unwrap().live;
    assert_eq!(
        live[0].1.as_ref().map(|f| f.value.clone()),
        Some(Value::Num(3.0))
    );
}

/// §7: "Log segments are never retired by compaction"; §8: they leave the
/// fast disk only after 90 days or under fill pressure, once verified
/// elsewhere. A consumer behind a compacted position is replayed from them,
/// not sent `resync` in place of the replay.
#[test]
fn s7_consumer_behind_compacted_position_is_replayed() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    put(&mut m, a, 1.0);
    m.drain().unwrap();
    m.advance(SECOND);
    put(&mut m, a, 2.0);
    m.seal_active();
    m.compact(None).unwrap();
    m.upload();
    m.advance(2 * HOUR);
    m.housekeeping().unwrap();
    let r = m.open_reader();
    assert!(
        r.files
            .iter()
            .any(|f| m.disk.files[f].class == FileClass::Segment && m.disk.hot.contains(f))
    );
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(labels(&session_stream(&m, C, 2)), ["R5", "S"]);
}

/// §4 Resync: sent "when the cursor is older than the oldest local log
/// segment"; §8 Behind consumers: such a consumer "receives `resync`, not a
/// replay. The engine never downloads from deep for catch-up." Confirming the
/// `resync` moves the cursor past the positions it replaced.
#[test]
fn s4_resync_replaces_replay_when_cursor_too_far_behind() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    let p1 = put(&mut m, a, 1.0);
    m.drain().unwrap();
    put(&mut m, a, 2.0);
    let p3 = put(&mut m, a, 3.0);
    m.seal_active();
    assert!(m.upload() > 0);
    m.advance(91 * DAY);
    assert!(
        !m.housekeeping().unwrap().is_empty(),
        "the segment left hot"
    );
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    assert_eq!(labels(&stream), ["S"]);
    assert_eq!(stream[0].resync().unwrap().replaced, Some((p1, p3)));
    assert_eq!(m.disk.cursors[&C], p3);
}

/// §3 Recovery rule 3 and §4 Resync: after a recovery on a new boot, every
/// consumer receives `resync` "before any position above the last kept batch
/// is delivered to it"; §6: "this replay stops at the last kept batch, and the
/// one `resync` that follows serves both rules."
#[test]
fn s4_resync_after_rule_3_recovery_before_later_positions() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    put(&mut m, a, 1.0);
    m.drain().unwrap();
    put(&mut m, a, 2.0);
    let now = m.wall;
    m.emit(Reading::num(a, now, 3.0)).unwrap();
    assert_eq!(
        m.commit(Some(Fault::PowerLoss(WriteOutcome::Survived))),
        Err(Error::Crashed)
    );
    m.restart();
    let kept = m.entries().unwrap().last().unwrap().position;
    assert_eq!(kept, Position::new(0, 6), "the unsynced batch was kept");
    put(&mut m, a, 4.0);
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    assert_eq!(labels(&stream), ["R5", "R6", "S", "R7"]);
    assert_eq!(stream[2].position, kept);
}

/// §8 Restore step 2: a cursor at or below the restored end catches up to it,
/// and "every consumer then receives `generation_changed` and `resync`": both
/// follow the catch-up in the consumer's stream.
#[test]
fn s4_generation_changed_and_resync_follow_catch_up_after_restore() {
    let (mut m, _) = backed_up_model();
    let cursors = [(C, Position::new(0, 3))].into_iter().collect();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &cursors,
        m.wall,
        false,
        false,
    )
    .unwrap();
    r.drain().unwrap();
    let kinds: Vec<(Position, &str)> = r.received[&C]
        .iter()
        .map(|x| {
            let k = match &x.delivery.kind {
                DeliveryKind::Fact(_) => "fact",
                DeliveryKind::GenerationChanged { .. } => "generation_changed",
                DeliveryKind::Resync(_) => "resync",
                DeliveryKind::Trigger(_) => "trigger",
            };
            (x.delivery.position, k)
        })
        .collect();
    let end = Position::new(0, 5);
    assert_eq!(
        kinds,
        [
            (Position::new(0, 4), "fact"),
            (end, "fact"),
            (end, "generation_changed"),
            (end, "resync")
        ]
    );
}

/// §4 Order: "Timer deliveries carry the last committed position at
/// evaluation plus a per-funnel sequence"; §4 Timers: "held for N s" uses the
/// monotonic clock; §6 Cadence: a funnel is evaluated on each routed fact and
/// on a timer.
#[test]
fn s4_timer_deliveries_carry_last_position_and_funnel_sequence() {
    let (mut m, a, _) = routed_model(100);
    let held = Condition::HeldFor {
        above: 5.0,
        n: 10 * SECOND,
    };
    m.add_route(C, F, Funnel::condition(a, None, held)).unwrap();
    let p = put(&mut m, a, 9.0);
    m.advance(5 * SECOND);
    m.tick().unwrap();
    m.jump_wall(HOUR);
    m.tick().unwrap();
    assert!(
        m.receive(C, 10).unwrap().is_empty(),
        "a wall jump fires nothing"
    );
    m.advance(5 * SECOND);
    m.tick().unwrap();
    let q = put(&mut m, a, 1.0);
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got.iter().map(|d| (d.id(), d.position)).collect::<Vec<_>>(),
        [
            (
                DeliveryId::Timer {
                    generation: 0,
                    funnel: F,
                    sequence: 1
                },
                p
            ),
            (
                DeliveryId::Timer {
                    generation: 0,
                    funnel: F,
                    sequence: 2
                },
                q
            )
        ]
    );
    let causes: Vec<(TriggerCause, ConditionState)> = got
        .iter()
        .map(|d| d.trigger().map(|t| (t.cause, t.state.clone())).unwrap())
        .collect();
    assert_eq!(
        causes,
        [
            (TriggerCause::Timer, ConditionState::Active(true)),
            (TriggerCause::Fact, ConditionState::Active(false))
        ]
    );
}

/// §4 Timers: "Timer deliveries that fell due while the engine was down are
/// not replayed; `resync` covers them."
#[test]
fn s4_timer_deliveries_falling_due_while_down_are_not_replayed() {
    let (mut m, a, _) = routed_model(100);
    let held = Condition::HeldFor {
        above: 5.0,
        n: 10 * SECOND,
    };
    m.add_route(C, F, Funnel::condition(a, None, held)).unwrap();
    put(&mut m, a, 9.0);
    m.drain().unwrap();
    m.crash();
    m.advance(20 * SECOND);
    m.restart();
    m.tick().unwrap();
    m.drain().unwrap();
    let stream = session_stream(&m, C, 2);
    assert_eq!(labels(&stream), ["S"], "no timer delivery, no replay");
}

/// §6 Latch: "Each condition funnel persists, with its consumer's cursor, the
/// last emitted state"; invariant 13: a latch on disk is never ahead of the
/// consumer's confirmation. A transition taken but not confirmed is not in
/// the latch, so the `resync` after a crash delivers it again as a
/// transition; once confirmed, an equal state is no new transition.
#[test]
fn s6_latch_moves_on_confirmation_only() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::condition(a, None, threshold()))
        .unwrap();
    put(&mut m, a, 90.0);
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
    assert!(m.disk.latches.is_empty(), "taken, not confirmed");
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(true), true)
    );
    assert_eq!(m.disk.latches[&(C, F)].state, ConditionState::Active(true));
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(true), false)
    );
    // The same through a consumer restart.
    put(&mut m, a, 10.0);
    m.receive(C, 10).unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(false), true)
    );
}

/// §6 Restart and resume: the operator "is rebuilt from the tree and recent
/// window as of now … A condition that became true while the consumer was
/// down is therefore delivered, never suppressed"; invariant 16, row 16. A
/// value held above its level for longer than the hold while the consumer was
/// down is reported true in the `resync`, a transition from a latched false.
#[test]
fn s6_held_for_true_while_down_delivered_in_resync() {
    let (mut m, a, _) = routed_model(100);
    let held = Condition::HeldFor {
        above: 5.0,
        n: 10 * SECOND,
    };
    m.add_route(C, F, Funnel::condition(a, None, held)).unwrap();
    put(&mut m, a, 9.0);
    m.advance(10 * SECOND);
    m.tick().unwrap();
    put(&mut m, a, 1.0);
    m.drain().unwrap();
    assert_eq!(m.disk.latches[&(C, F)].state, ConditionState::Active(false));
    // Held above for 30 s while the consumer is down; it restarts before
    // taking the transition, and then the engine restarts too.
    m.advance(SECOND);
    put(&mut m, a, 9.0);
    m.advance(30 * SECOND);
    m.tick().unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(true), true)
    );
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(true), false)
    );
    // Above for less than the hold: false, and it fires once the hold is
    // complete.
    put(&mut m, a, 1.0);
    m.advance(SECOND);
    put(&mut m, a, 9.0);
    m.advance(4 * SECOND);
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(last_resync_condition(&m).0, ConditionState::Active(false));
    m.advance(6 * SECOND);
    m.tick().unwrap();
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
}

/// §6 Restart and resume: the operator is rebuilt "from the tree and recent
/// window as of now", exactly: a held-for condition is true on resume only if
/// the value was above its level throughout the last N, judged on the facts
/// and the values they hold, so a dip shorter than a second counts and N need
/// not be whole seconds.
#[test]
fn s6_held_for_rebuilt_exactly_from_the_facts() {
    let held = Condition::HeldFor {
        above: 5.0,
        n: 3 * SECOND / 2,
    };
    let ms = |k: i64| k * SECOND / 1000;
    // (value, wait after it): above for 1.6 s; above for 2 s with a dip of
    // 0.3 s; above for 1.4 s.
    for (steps, want) in [
        (vec![(1.0, ms(100)), (9.0, ms(1600))], true),
        (vec![(9.0, ms(300)), (1.0, ms(300)), (9.0, ms(1400))], false),
        (vec![(1.0, ms(100)), (9.0, ms(1400))], false),
    ] {
        let (mut m, a, _) = routed_model(100);
        m.add_route(C, F, Funnel::condition(a, None, held.clone()))
            .unwrap();
        for (v, wait) in steps {
            put(&mut m, a, v);
            m.advance(wait);
        }
        m.reconnect(C).unwrap();
        m.drain().unwrap();
        assert_eq!(last_resync_condition(&m).0, ConditionState::Active(want));
    }
}

/// §6 Latch: each condition funnel persists its own latch. A transition of
/// a funnel that is removed and added again with another condition before
/// the consumer confirms it does not become the new funnel's latch.
#[test]
fn s6_latch_belongs_to_one_funnel_incarnation() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, F, Funnel::condition(a, None, threshold()))
        .unwrap();
    put(&mut m, a, 90.0);
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
    m.remove_route(C, F).unwrap();
    let held = Condition::HeldFor {
        above: 100.0,
        n: 10 * SECOND,
    };
    m.add_route(C, F, Funnel::condition(a, None, held)).unwrap();
    m.confirm_delivery(C, got[0].id()).unwrap();
    assert!(!m.disk.latches.contains_key(&(C, F)));
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(false), true)
    );
}

/// §8 Restore step 2: "Every consumer then receives `generation_changed` and
/// `resync`." It stays owed until the consumer confirms it: a crash before
/// the consumer takes it does not lose it.
#[test]
fn s8_generation_changed_owed_until_confirmed() {
    let (mut m, _) = backed_up_model();
    let mut r = Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        false,
        false,
    )
    .unwrap();
    r.crash();
    r.restart();
    r.drain().unwrap();
    let kinds = |r: &Model, session: u32| -> Vec<&'static str> {
        r.received[&C]
            .iter()
            .filter(|x| x.session == session)
            .map(|x| match x.delivery.kind {
                DeliveryKind::GenerationChanged { .. } => "generation_changed",
                DeliveryKind::Resync(_) => "resync",
                _ => "other",
            })
            .collect()
    };
    let last = r.received[&C].last().unwrap().session;
    assert_eq!(kinds(&r, last), ["generation_changed", "resync"]);
    r.crash();
    r.restart();
    r.drain().unwrap();
    assert_eq!(kinds(&r, last + 1), ["resync"]);
}

/// §4 Replay: replayed facts "are never fed to a condition operator a second
/// time"; §6 Restart and resume: "The operator is rebuilt from the tree and
/// recent window as of now; it is never replayed." A windowed operator sees
/// the facts in its window now, not their replay.
#[test]
fn s6_replayed_facts_never_reach_a_condition() {
    let (mut m, a, _) = routed_model(100);
    m.add_route(C, FunnelId(2), Funnel::facts(Selection::All))
        .unwrap();
    let avg = Operator {
        aggregate: Aggregate::Avg,
        width: MINUTE,
    };
    m.add_route(C, F, Funnel::condition(a, Some(avg), threshold()))
        .unwrap();
    put(&mut m, a, 10.0);
    m.drain().unwrap();
    m.advance(5 * SECOND);
    put(&mut m, a, 90.0);
    m.advance(5 * SECOND);
    put(&mut m, a, 90.0);
    m.advance(5 * SECOND);
    m.crash();
    m.restart();
    m.drain().unwrap();
    // Replayed facts, then `resync` with the average now (above 50): no
    // transition from the replay itself.
    assert_eq!(labels(&session_stream(&m, C, 2)), ["R6", "R7", "S"]);
    assert_eq!(
        last_resync_condition(&m),
        (ConditionState::Active(true), true)
    );
}

/// §4 Overflow: catch-up after overflow re-attaches "at the next position
/// under the writer's delivery lock (no gap, no repeat)", also when the
/// consumer has condition funnels and routes change meanwhile.
#[test]
fn s4_overflow_catch_up_with_conditions_and_route_changes() {
    let (mut m, a, b) = routed_model(2);
    m.add_route(C, FunnelId(2), Funnel::facts(Selection::Series([a].into())))
        .unwrap();
    for i in 0..4 {
        put(&mut m, a, f64::from(i));
    }
    assert!(m.in_catch_up(C));
    put(&mut m, b, 9.0);
    m.add_route(C, FunnelId(3), Funnel::facts(Selection::Series([b].into())))
        .unwrap();
    put(&mut m, b, 10.0);
    m.drain().unwrap();
    assert_eq!(
        labels(&session_stream(&m, C, 1)),
        ["F4", "F5", "R6", "R7", "R10"]
    );
}

fn delivery_history(ops: &[DOp]) -> Result<Model, proptest::test_runner::TestCaseError> {
    let (mut m, ids) = delivery_machine();
    for op in ops {
        apply_delivery(&mut m, &ids, op).map_err(proptest::test_runner::TestCaseError::fail)?;
    }
    settle(&mut m);
    m.drain().unwrap();
    Ok(m)
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(256),
        ..proptest::test_runner::Config::default()
    })]

    /// §4 At-least-once and Order: consumers de-duplicate condition
    /// deliveries by `(funnel, sequence)`, so no two condition deliveries to
    /// a consumer share one, across restarts, reconnects and route changes.
    #[test]
    fn s4_condition_deliveries_unique_by_funnel_and_sequence(
        ops in proptest::collection::vec(arb_delivery_op(), 1..100)
    ) {
        let m = delivery_history(&ops)?;
        for (c, rs) in &m.received {
            let mut seen = std::collections::BTreeSet::new();
            for r in rs {
                if let Some(t) = r.delivery.trigger() {
                    proptest::prop_assert!(seen.insert((t.funnel, t.sequence)), "consumer {:?} got {:?} twice", c, (t.funnel, t.sequence));
                }
            }
        }
    }

    /// §4 Resync and §6 Restart and resume: on resume the replay stops at the
    /// position the `resync` names, and no fact above it is delivered before
    /// the `resync` (after a §3 Recovery rule 3 recovery, the last kept
    /// batch); a `generation_changed` comes just before its `resync`.
    #[test]
    fn s4_resync_separates_replay_from_later_positions(
        ops in proptest::collection::vec(arb_delivery_op(), 1..100)
    ) {
        let m = delivery_history(&ops)?;
        for (c, rs) in &m.received {
            for (i, r) in rs.iter().enumerate() {
                let Some(_) = r.delivery.resync() else { continue };
                let p = r.delivery.position;
                let same = |x: &&kvik_model::engine::Received| x.session == r.session;
                for x in rs[..i].iter().filter(same) {
                    if x.delivery.fact().is_some() {
                        proptest::prop_assert!(x.delivery.position <= p, "consumer {:?}: {} before resync at {}", c, x.delivery.position, p);
                    }
                }
                for x in rs[i + 1..].iter().filter(same) {
                    if x.delivery.fact().is_some() {
                        proptest::prop_assert!(x.delivery.position > p, "consumer {:?}: {} after resync at {}", c, x.delivery.position, p);
                    }
                }
            }
        }
    }

    /// §4 Overflow and row 14: catch-up resumes "after the last position
    /// already enqueued for it in the current session … no gap, no repeat":
    /// within a session, the facts delivered are exactly the routed durable
    /// positions between the first and the last one delivered.
    #[test]
    fn s4_no_gap_no_repeat_within_a_session(
        ops in proptest::collection::vec(arb_delivery_op(), 1..100)
    ) {
        let m = delivery_history(&ops)?;
        let dur = durable(&m);
        for d in delivery_consumers() {
            let mut by_session: BTreeMap<u32, Vec<Position>> = BTreeMap::new();
            for o in m.observed.get(&d.id).into_iter().flatten() {
                by_session.entry(o.session).or_default().push(o.position);
            }
            for (s, ps) in by_session {
                let (first, last) = (ps[0], ps[ps.len() - 1]);
                let want: Vec<Position> = dur
                    .range(first..=last)
                    .filter(|(p, f)| routed_at(&m, &d, **p, f.series))
                    .map(|(p, _)| *p)
                    .collect();
                proptest::prop_assert_eq!(&ps, &want, "consumer {} session {}", d.id.0, s);
            }
        }
    }
}

// ----- versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §8) -----

/// A store with a consumer of every series and one versioned gauge whose
/// `changed_at` is an opaque counter.
fn versioned_model() -> (Model, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let s = m
        .define_with(
            "site/forecast-1/load",
            Kind::Gauge,
            ValueType::Number,
            "kW",
            10 * SECOND,
            SeriesOptions::versioned(ChangedAtKind::Version),
        )
        .unwrap();
    (m, s)
}

/// §2.2: "A reading on a versioned series without it, or with it on any other
/// series, is refused and counted." §2.4: "On a versioned series `revise` is
/// refused and counted."
#[test]
fn s2_2_changed_at_on_versioned_series_only() {
    let (mut m, v) = versioned_model();
    let g = m
        .define(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
        )
        .unwrap();
    assert_eq!(
        m.emit(Reading::num(v, T0, 1.0)),
        Err(Refusal::ChangedAtMissing.into())
    );
    assert_eq!(
        m.emit(Reading::versioned(g, T0, 1.0, 1)),
        Err(Refusal::ChangedAtUnexpected.into())
    );
    assert_eq!(
        m.emit_revise(Reading::versioned(v, T0, 1.0, 1)),
        Err(Refusal::ReviseOnVersioned.into())
    );
    for r in [
        Refusal::ChangedAtMissing,
        Refusal::ChangedAtUnexpected,
        Refusal::ReviseOnVersioned,
    ] {
        assert_eq!(m.counters.refused[&r], 1);
    }
    m.ingest(vec![Reading::versioned(v, T0, 1.0, 1)]).unwrap();
    assert_eq!(m.current().unwrap()[&(v, T0)].changed_at, Some(1));
}

/// §2.1 `timing`: a forecast series "must be versioned with `changed_at kind`
/// `time`".
#[test]
fn s2_1_forecast_series_versioned_with_time() {
    let (mut m, _) = versioned_model();
    let forecast = |versioned| SeriesOptions {
        versioned,
        timing: Timing::Forecast,
        ..SeriesOptions::default()
    };
    for versioned in [None, Some(ChangedAtKind::Version)] {
        assert_eq!(
            m.define_with(
                "site/forecast-2/load",
                Kind::Gauge,
                ValueType::Number,
                "kW",
                None,
                forecast(versioned)
            ),
            Err(Error::Registry(RegistryError::ForecastNotVersioned))
        );
    }
    m.define_with(
        "site/forecast-2/load",
        Kind::Gauge,
        ValueType::Number,
        "kW",
        None,
        forecast(Some(ChangedAtKind::Time)),
    )
    .unwrap();
}

/// §2.2 Rank: "a fact's position, or in a versioned series `(changed_at,
/// position)`, compared lexicographically"; §2.4: "A fact whose `changed_at`
/// is above that of every fact in its key's history is never a duplicate,
/// even when its value returns to an earlier one", and a fact with equal
/// `changed_at` takes the tie by position.
#[test]
fn s2_2_rank_by_changed_at_then_position() {
    let (mut m, s) = versioned_model();
    m.ingest(vec![Reading::versioned(s, T0, 1.0, 5)]).unwrap();
    m.ingest(vec![Reading::versioned(s, T0, 2.0, 7)]).unwrap();
    // A return to the first value with a higher `changed_at`.
    let r = m.ingest(vec![Reading::versioned(s, T0, 1.0, 9)]).unwrap();
    assert_eq!(r.positions.len(), 1);
    assert_eq!(m.current().unwrap()[&(s, T0)].changed_at, Some(9));
    // The same `changed_at` and value: a duplicate. The same `changed_at`
    // with another value: recorded, and current by position.
    assert_eq!(
        m.ingest(vec![Reading::versioned(s, T0, 1.0, 9)])
            .unwrap()
            .duplicates,
        1
    );
    let p = m
        .ingest(vec![Reading::versioned(s, T0, 3.0, 9)])
        .unwrap()
        .positions[0];
    assert_eq!(m.current().unwrap()[&(s, T0)].position, p);
    // Identity includes `changed_at`: value 2.0 with `changed_at` 8 is new,
    // and outranked.
    let r = m.ingest(vec![Reading::versioned(s, T0, 2.0, 8)]).unwrap();
    assert_eq!(r.positions.len(), 1);
    assert_eq!(m.current().unwrap()[&(s, T0)].position, p);
    assert_eq!(m.counters.outranked_on_arrival, 1);
}

/// §4 Outranked facts: "A fact outranked at commit (§2.3) is delivered in
/// position order, flagged `outranked`, and counted per series
/// (`outranked_on_arrival`). It does not change live state or staleness, is
/// never fed to an operator or condition and must not be acted on." The flag
/// is a function of the log: a replay in catch-up carries it too.
#[test]
fn s4_outranked_fact_delivered_flagged_never_fed_to_a_condition() {
    let (mut m, s) = versioned_model();
    m.add_route(C, F, Funnel::condition(s, None, threshold()))
        .unwrap();
    let now = m.wall;
    m.ingest(vec![Reading::versioned(s, now, 10.0, 2)]).unwrap();
    let p = m
        .ingest(vec![Reading::versioned(s, now, 99.0, 1)])
        .unwrap()
        .positions[0];
    assert_eq!(m.counters.outranked_on_arrival, 1);
    assert_eq!(m.latest(s).unwrap().unwrap().value, n(10.0));
    m.drain().unwrap();
    let flagged: Vec<(Position, bool)> = m.received[&C]
        .iter()
        .filter(|r| r.delivery.fact().is_some())
        .map(|r| (r.delivery.position, r.delivery.outranked))
        .collect();
    assert_eq!(flagged.last(), Some(&(p, true)));
    assert_eq!(flagged.iter().filter(|(_, o)| *o).count(), 1);
    assert!(
        m.received[&C]
            .iter()
            .all(|r| r.delivery.trigger().is_none()),
        "99 above the threshold was never fed to the condition"
    );
    // Replayed after a restart from an earlier cursor, still flagged.
    m.crash();
    m.disk.cursors.insert(C, Position::ORIGIN);
    m.restart();
    m.drain().unwrap();
    let replayed: Vec<&Envelope> = m.received[&C]
        .iter()
        .filter(|r| r.session == 2 && r.delivery.position == p && r.delivery.fact().is_some())
        .map(|r| &r.delivery)
        .collect();
    assert!(replayed.len() == 1 && replayed[0].replayed && replayed[0].outranked);
}

/// §5.6: "Each node carries a version incremented on change. An outranked
/// fact (§2.3) changes no node."
#[test]
fn s5_6_outranked_fact_changes_no_node() {
    let (mut m, s) = versioned_model();
    m.ingest(vec![Reading::versioned(s, T0, 1.0, 2)]).unwrap();
    let before = m.tree(s).unwrap().clone();
    let q = T0 - T0.rem_euclid(15 * MINUTE);
    m.ingest(vec![Reading::versioned(s, T0, 50.0, 1)]).unwrap();
    m.ingest(vec![Reading::versioned(s, T0 + SECOND, 50.0, 0)])
        .unwrap();
    let r = m
        .ingest(vec![Reading::versioned(s, T0 + SECOND, 60.0, 0)])
        .unwrap();
    assert_eq!(r.positions.len(), 1);
    let after = m.tree(s).unwrap();
    assert_eq!(
        after.version(q),
        before.version(q) + 2,
        "only the two facts of a new key"
    );
    assert_eq!(after.node(0, T0 - T0.rem_euclid(MINUTE)).count, 2);
}

/// §2.4 Version index: "for every key of a versioned series, its highest
/// recorded `changed_at`. The rank of an arriving fact is therefore known
/// whenever the index is local." §8 Restore step 2: "The rank of an arriving
/// fact on a versioned series is known from step 1 on, the version index being
/// exact at the restored end (from the checkpoint and the replay)." A fact
/// below the index is outranked while the fact that outranks it is still in
/// the bucket, also after a restart.
#[test]
fn s8_version_index_ranks_facts_while_restoring() {
    let (mut m, s) = versioned_model();
    for (t, v, c) in [(0, 1.0, 3), (1, 2.0, 1), (0, 4.0, 5)] {
        m.ingest(vec![Reading::versioned(s, T0 + t * SECOND, v, c)])
            .unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    let cp = m.checkpoint().unwrap();
    assert_eq!(m.disk.checkpoints.last().unwrap().versions[&(s, T0)], 5);
    m.ingest(vec![Reading::versioned(s, T0 + SECOND, 3.0, 2)])
        .unwrap();
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.restoring());
    assert!(
        r.disk
            .restore
            .as_ref()
            .unwrap()
            .checkpoint
            .as_ref()
            .unwrap()
            .position
            == cp
    );
    // The fact with `changed_at` 5 is in a current chunk not yet back.
    let p = r
        .ingest(vec![Reading::versioned(s, T0, 9.0, 4)])
        .unwrap()
        .positions[0];
    assert_eq!(r.counters.outranked_on_arrival, 1);
    assert!(
        r.current()
            .unwrap()
            .get(&(s, T0))
            .is_none_or(|f| f.position != p)
    );
    // From the replay after the checkpoint: `changed_at` 2 at `T0 + 1 s`.
    r.ingest(vec![Reading::versioned(s, T0 + SECOND, 8.0, 1)])
        .unwrap();
    assert_eq!(r.counters.outranked_on_arrival, 2);
    r.crash();
    r.restart();
    assert!(
        r.current()
            .unwrap()
            .get(&(s, T0))
            .is_none_or(|f| f.position != p)
    );
    assert_eq!(r.current().unwrap()[&(s, T0 + SECOND)].changed_at, Some(2));
    r.restore_rest().unwrap();
    assert_eq!(r.current().unwrap()[&(s, T0)].value, n(4.0));
    assert_eq!(r.current().unwrap()[&(s, T0 + SECOND)].value, n(3.0));
}

// ----- compaction and files (§7) -----

/// A store with `n` compactions, each of one log segment holding one fact of
/// series `s` in the first 15 minutes after `T0`. Returns the current chunks
/// of `s` in order.
fn chunked_model(n: i64) -> (Model, SeriesId, Vec<FileId>) {
    let (mut m, s) = model(10 * SECOND);
    let mut chunks = Vec::new();
    for i in 0..n {
        m.ingest(vec![Reading::num(s, T0 + i * SECOND, i as f64)])
            .unwrap();
        m.seal_active();
        for f in m.compact(None).unwrap() {
            if m.disk.files[&f].class == FileClass::Current
                && m.disk.files[&f].entries().any(|e| e.fact().is_some())
            {
                chunks.push(f);
            }
        }
    }
    (m, s, chunks)
}

/// The record that added `f`.
fn record_adding(m: &Model, f: FileId) -> &kvik_model::engine::ManifestRecord {
    m.disk
        .manifest
        .iter()
        .find(|r| matches!(&r.op, ManifestOp::Publish { added, .. } if added.iter().any(|a| a.file == f)))
        .unwrap()
}

/// §7: "Compaction rearranges facts from whole sealed log segments into
/// per-series chunks"; "Current facts and revisions go to separate chunks";
/// "One record may add files, retire files and move files between tiers
/// atomically"; "Log segments are never retired by compaction".
#[test]
fn s7_compaction_publishes_per_series_chunks_in_one_record() {
    let (mut m, a) = model(10 * SECOND);
    let b = m
        .define(
            "vehicle/8/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    m.ingest(vec![Reading::num(a, T0, 1.0), Reading::num(b, T0, 2.0)])
        .unwrap();
    m.ingest(vec![Reading::num(a, T0, 3.0)]).unwrap();
    m.seal_active();
    let segments = m.disk.listed_at(m.disk.latest_epoch());
    let out = m.compact(None).unwrap();
    // Current and revision chunks of `a`, the current chunk of `b`, and the
    // chunk of the registry entries.
    assert_eq!(out.len(), 4);
    let record = record_adding(&m, out[0]);
    let ManifestOp::Publish { added, retired, .. } = &record.op else {
        unreachable!()
    };
    assert_eq!(added.iter().map(|x| x.file).collect::<Vec<_>>(), out);
    assert!(added.iter().all(|x| x.from == segments));
    assert!(retired.is_empty());
    for f in &out {
        let series: BTreeSet<SeriesId> = m.disk.files[f]
            .entries()
            .filter_map(|e| e.fact().map(|x| x.series))
            .collect();
        assert!(series.len() <= 1, "one series per chunk");
    }
    let listed = m.disk.listed_at(m.disk.latest_epoch());
    assert!(segments.iter().all(|f| listed.contains(f)));
}

/// §7: "A revision chunk holds only facts outranked by a fact within the same
/// compaction's inputs, so the current view never needs revision chunks. A
/// current chunk may hold facts outranked later or by a fact in another file;
/// readers resolve the current view across current chunks by rank."
#[test]
fn s7_revision_chunks_hold_only_facts_outranked_within_the_inputs() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.ingest(vec![Reading::num(s, T0, 2.0)]).unwrap();
    m.seal_active();
    // Both segments in one compaction: the first fact is a revision.
    let mut one = m.clone();
    let out = one.compact(None).unwrap();
    let revisions: Vec<FileId> = out
        .iter()
        .copied()
        .filter(|f| one.disk.files[f].class == FileClass::Revision)
        .collect();
    assert_eq!(revisions.len(), 1);
    // Each segment compacted alone: no revision chunk; the outranked fact
    // stays in a current chunk and the current view is still by rank.
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    m.ingest(vec![Reading::num(s, T0, 2.0)]).unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    assert!(
        m.disk
            .files
            .values()
            .all(|f| f.class != FileClass::Revision)
    );
    let r = m.open_reader();
    let chunks: Vec<Fact> = r
        .files
        .iter()
        .filter(|f| m.disk.files[*f].class == FileClass::Current)
        .flat_map(|f| m.disk.files[f].entries().filter_map(|e| e.fact().cloned()))
        .collect();
    assert_eq!(view::current(chunks.iter())[&(s, T0)].value, n(2.0));
    m.restart();
    assert_eq!(m.current().unwrap()[&(s, T0)].value, n(2.0));
}

/// §7: "Compaction of compacted files (merging overlapping chunks created by
/// late data) publishes outputs and retires inputs in one record. Policy:
/// merge when a series' chunks overlapping any 15-minute span exceed 4."
#[test]
fn s7_merge_publishes_outputs_and_retires_inputs_in_one_record() {
    let (mut m, s, chunks) = chunked_model(4);
    assert_eq!(
        m.merge_chunks(None).unwrap(),
        Vec::new(),
        "four do not exceed 4"
    );
    m.ingest(vec![Reading::num(s, T0, 100.0)]).unwrap(); // a correction
    m.seal_active();
    let mut chunks = chunks;
    chunks.extend(
        m.compact(None)
            .unwrap()
            .into_iter()
            .filter(|f| m.disk.files[f].class == FileClass::Current),
    );
    let before = durable(&m);
    let out = m.merge_chunks(None).unwrap();
    let record = record_adding(&m, out[0]);
    let ManifestOp::Publish { added, retired, .. } = &record.op else {
        unreachable!()
    };
    assert_eq!(added.iter().map(|x| x.file).collect::<Vec<_>>(), out);
    let mut retired = retired.clone();
    retired.sort();
    assert_eq!(retired, chunks);
    // The outputs: one current chunk, and a revision chunk for the corrected
    // fact, which is now outranked within the merged inputs.
    let classes: Vec<FileClass> = out.iter().map(|f| m.disk.files[f].class).collect();
    assert_eq!(classes, vec![FileClass::Current, FileClass::Revision]);
    assert_eq!(durable(&m), before);
    assert_eq!(m.current().unwrap()[&(s, T0)].value, n(100.0));
}

/// §7: "Retired files are removed locally only after the grace period (1 hour
/// of the writer's monotonic clock since the later of the retirement record
/// and the writer's start), once the files that replaced them are verified in
/// another tier". A jump of the wall clock does not count, and a restart
/// starts the hour again.
#[test]
fn s7_retired_files_removed_after_grace_on_the_monotonic_clock() {
    let (mut m, _, chunks) = chunked_model(5);
    let out = m.merge_chunks(None).unwrap();
    m.advance(2 * HOUR);
    assert!(
        m.housekeeping().unwrap().is_empty(),
        "replacements not verified elsewhere"
    );
    m.upload();
    assert!(out.iter().all(|f| m.disk.in_bucket(*f)));
    // A merge after the upload: its inputs are the fresh chunk only after
    // the grace period.
    let (mut m, _, chunks2) = chunked_model(5);
    let old = m.open_reader();
    let (_, facts) = m.read(&old).unwrap();
    m.merge_chunks(None).unwrap();
    m.upload();
    m.jump_wall(2 * HOUR);
    assert!(
        m.housekeeping().unwrap().is_empty(),
        "the wall clock does not count"
    );
    m.advance(HOUR - 1);
    m.crash();
    m.restart();
    m.advance(1);
    assert!(
        m.housekeeping().unwrap().is_empty(),
        "a restart starts the hour again"
    );
    m.advance(HOUR - 1);
    let mut gone = m.housekeeping().unwrap();
    gone.sort();
    assert_eq!(gone, chunks2);
    assert_eq!(chunks.len(), chunks2.len());
    // Removed locally, the files still read from the bucket.
    let (loc, again) = m.read(&old).unwrap();
    assert!(loc.iter().any(|(_, l)| *l == Locality::Remote));
    assert!(loc.iter().all(|(_, l)| *l != Locality::Missing));
    assert_eq!(again, facts);
}

/// §7 Leases: "The writer expires a lease when its counter has not changed for
/// 1 hour of the writer's monotonic clock since the writer first observed
/// that value; at writer start, every lease of the current boot counts as
/// first observed then. A lease with a different boot id is void."
#[test]
fn s7_leases_expire_on_the_writers_monotonic_clock() {
    let (mut m, _, _) = chunked_model(5);
    let reader = m.lease_reader();
    let epoch = reader.epoch;
    // The writer observes the lease when it reads the lease files.
    assert_eq!(m.live_lease_epochs(), vec![epoch]);
    m.merge_chunks(None).unwrap();
    m.upload();
    m.advance(HOUR - 1);
    assert_eq!(m.live_lease_epochs(), vec![epoch]);
    m.advance(1);
    assert!(m.live_lease_epochs().is_empty(), "unchanged for an hour");
    // A refresh is observed and makes the lease live again.
    let reader = m.refresh_reader(&reader, false);
    assert_eq!(m.live_lease_epochs(), vec![epoch]);
    m.advance(HOUR - 1);
    m.crash();
    m.restart();
    m.advance(HOUR - 1);
    assert_eq!(
        m.live_lease_epochs(),
        vec![epoch],
        "first observed at the writer's start"
    );
    m.advance(1);
    assert!(m.live_lease_epochs().is_empty());
    m.refresh_reader(&reader, false);
    assert_eq!(m.live_lease_epochs(), vec![epoch]);
    m.power_loss();
    m.restart();
    assert!(
        m.live_lease_epochs().is_empty(),
        "a lease of another boot is void"
    );
    m.refresh_reader(&reader, true);
    assert!(
        m.live_lease_epochs().is_empty(),
        "also when refreshed after the reboot"
    );
}

// ----- tiers and eviction (§3 Disk space, §8) -----

/// The manifest's tier moves, in order.
fn moves(m: &Model) -> Vec<Vec<(FileId, Tier)>> {
    m.disk
        .manifest
        .iter()
        .filter_map(|r| match &r.op {
            ManifestOp::Publish { moved, .. } if !moved.is_empty() => {
                Some(moved.iter().map(|x| (x.file, x.to)).collect())
            }
            _ => None,
        })
        .collect()
}

/// §3 Disk space: "Low mark … → compaction pauses, health degraded. Tiering
/// and eviction, which free space, continue at every mark."
#[test]
fn s3_low_mark_pauses_compaction_not_tiering() {
    let cfg = Config {
        disk_bytes: 100_000,
        low_mark: 20_000,
        critical_mark: 5_000,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.upload();
    m.disk.other_bytes = 85_000;
    assert_eq!(m.compact(None), Err(Error::Paused));
    m.advance(91 * DAY);
    assert_eq!(m.housekeeping().unwrap().len(), 1, "the old segment leaves");
}

/// §8 Eviction at 90 days: "Revision chunks (age per series, 'never'
/// allowed) and sealed log segments leave hot only after they are verified in
/// cold or deep. Current detail and the stored tree stay." §7: "Tier moves
/// keep the file's identity and change its location"; one record may move
/// files atomically.
#[test]
fn s8_eviction_at_90_days_with_revision_age_per_series() {
    let (mut m, a) = model(10 * SECOND);
    let b = m
        .define(
            "vehicle/8/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    m.config.revision_age.insert(b, None);
    for s in [a, b] {
        m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
        m.ingest(vec![Reading::num(s, T0, 2.0)]).unwrap();
    }
    m.seal_active();
    let out = m.compact(None).unwrap();
    let revision = |s: SeriesId| {
        *out.iter()
            .find(|f| {
                m.disk.files[*f].class == FileClass::Revision
                    && m.disk.files[*f]
                        .entries()
                        .any(|e| e.fact().unwrap().series == s)
            })
            .unwrap()
    };
    let (ra, rb) = (revision(a), revision(b));
    m.advance(91 * DAY);
    assert!(
        m.housekeeping().unwrap().is_empty(),
        "nothing verified elsewhere"
    );
    m.copy_to_cold(ra);
    m.upload();
    let segment = m.disk.compacted_segments().into_iter().next().unwrap();
    let mut gone = m.housekeeping().unwrap();
    gone.sort();
    assert_eq!(
        gone,
        vec![segment, ra],
        "never for series b; current detail stays"
    );
    assert!(m.disk.hot.contains(&rb));
    assert_eq!(
        moves(&m),
        vec![vec![(segment, Tier::Deep), (ra, Tier::Cold)]]
    );
    let before = durable(&m);
    m.restart();
    assert_eq!(durable(&m), before);
}

/// §8 Retention class: "For a diagnostic series (§2.1), raw detail (current
/// and revision chunks) whose span ends more than the retention period before
/// `now` leaves hot once verified in cold or deep … The stored tree stays hot.
/// Nothing is deleted: such detail is fetched on demand like moved current
/// detail, so … the duplicate check for it is unchecked (§2.4)." §2.1: "A
/// change of class is a registry entry that takes effect at its position, not
/// by event time; it moves no detail back."
#[test]
fn s8_retention_class_moves_diagnostic_detail_after_its_period() {
    let (mut m, s) = model(10 * SECOND);
    let g = m
        .define(
            "vehicle/8/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    for x in [s, g] {
        m.ingest(vec![
            Reading::num(x, T0, 1.0),
            Reading::num(x, T0 + SECOND, 2.0),
        ])
        .unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    m.upload();
    let p = m.registry().unwrap().get(s).unwrap().retention;
    assert_eq!(p, RetentionClass::LongTerm);
    m.set_retention(s, RetentionClass::diagnostic()).unwrap();
    m.advance(29 * DAY);
    assert!(m.housekeeping().unwrap().is_empty(), "within the period");
    m.advance(2 * DAY);
    let gone = m.housekeeping().unwrap();
    assert_eq!(gone.len(), 1);
    let chunk = &m.disk.files[&gone[0]];
    assert_eq!(chunk.class, FileClass::Current);
    assert!(chunk.entries().all(|e| e.fact().unwrap().series == s));
    // The tree stays. The log segment, younger than 90 days, still holds
    // the facts locally, so the duplicate check runs.
    assert_eq!(m.tree(s).unwrap().node(0, T0).count, 2);
    assert_eq!(
        m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap().duplicates,
        1
    );
    // Once the segment has left too, it is unchecked for the diagnostic
    // series only.
    m.advance(60 * DAY);
    m.housekeeping().unwrap();
    let r = m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    assert_eq!((r.positions.len(), m.counters.duplicates_unchecked), (1, 1));
    let r = m.ingest(vec![Reading::num(g, T0, 1.0)]).unwrap();
    assert_eq!(r.duplicates, 1, "a long-term series is checked");
    // Back to long-term: nothing moves back.
    m.set_retention(s, RetentionClass::LongTerm).unwrap();
    m.housekeeping().unwrap();
    assert!(!m.disk.hot.contains(&gone[0]));
    m.restart();
    assert_eq!(
        m.registry().unwrap().get(s).unwrap().retention,
        RetentionClass::LongTerm
    );
}

/// §8 Fill pressure: "Above 70 % fill, verified sealed log segments leave
/// first, then the oldest current raw detail (verified first, fetched on
/// demand)" (§12 D4). The registry's chunk stays.
#[test]
fn s8_fill_pressure_moves_log_segments_before_current_detail() {
    let cfg = Config {
        disk_bytes: 64 * 12,
        low_mark: 0,
        critical_mark: 0,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    let s = m
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            10 * SECOND,
        )
        .unwrap();
    let mut chunks = Vec::new();
    for hour in [3, 1, 2] {
        let t = T0 - hour * HOUR;
        m.ingest(
            (0..3)
                .map(|i| Reading::num(s, t + i * SECOND, 1.0))
                .collect(),
        )
        .unwrap();
        m.seal_active();
        chunks.extend(
            m.compact(None)
                .unwrap()
                .into_iter()
                .filter(|f| m.disk.files[f].entries().any(|e| e.fact().is_some())),
        );
    }
    let segments = m.disk.compacted_segments();
    m.upload();
    // 9 facts in segments, 9 in chunks, 1 registry entry twice: 20 entries;
    // 70 % of the disk holds 7. The segments leave (10 remain), then one
    // chunk of 3.
    let gone = m.housekeeping().unwrap();
    let left_segments: Vec<FileId> = gone
        .iter()
        .copied()
        .filter(|f| segments.contains(f))
        .collect();
    assert_eq!(left_segments.len(), segments.len(), "every segment first");
    // Then the chunk of the oldest data (3 hours ago), not the first written.
    let rest: Vec<FileId> = gone
        .iter()
        .copied()
        .filter(|f| !segments.contains(f))
        .collect();
    assert_eq!(rest, vec![chunks[0]]);
    assert!(m.disk.used_bytes(64) <= 64 * 12 / 100 * 70);
    assert_eq!(moves(&m).len(), 1, "one record");
    assert!(
        m.disk
            .hot
            .iter()
            .any(|f| m.disk.files[f].entries().all(|e| e.fact().is_none()))
    );
}

// ----- indexes, flags and ages across a restore and a merge -----

/// §8 Continuous backup: "Each tree checkpoint carries the registry file,
/// each series' latest recorded `t`, the revised-key index and the version
/// index, as of its position." §8 Restore step 2: facts "which match no local
/// fact and no entry of the revised-key index are recorded and counted
/// `duplicates_unchecked`". A replay of an earlier value of a key revised
/// before the checkpoint is dropped before any chunk is back (row 2a).
#[test]
fn s8_checkpoint_carries_the_revised_key_index() {
    let (mut m, s, cp) = checkpointed_model();
    let key = (s, T0 + 20 * SECOND);
    assert_eq!(m.disk.bucket.checkpoints[&cp].revised[&key].len(), 2);
    assert!(
        !m.disk.bucket.checkpoints[&cp]
            .revised
            .contains_key(&(s, T0))
    );
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let p = r.ingest(vec![Reading::num(s, key.1, 1.0)]).unwrap();
    assert_eq!((p.positions.len(), p.duplicates), (0, 1));
    assert_eq!(r.counters.duplicates_unchecked, 0);
    // A key with one fact at the checkpoint, not yet local: unchecked.
    let p = r.ingest(vec![Reading::num(s, T0, 0.0)]).unwrap();
    assert_eq!((p.positions.len(), r.counters.duplicates_unchecked), (1, 1));
    r.crash();
    r.restart();
    let p = r.ingest(vec![Reading::num(s, key.1, 1.0)]).unwrap();
    assert_eq!(p.duplicates, 1, "also after a restart");
    r.restore_rest().unwrap();
    assert_eq!(r.current().unwrap()[&key].value, n(100.0));
}

/// §4 Outranked facts: a fact outranked at commit is delivered flagged
/// `outranked`; the flag is a function of the log, so its replay in catch-up
/// during a restore carries it although the fact that outranks it is still in
/// the bucket (§2.4 Version index, §8 Restore step 2).
#[test]
fn s4_outranked_flag_in_catch_up_during_a_restore() {
    let (mut m, s) = versioned_model();
    let a = m
        .ingest(vec![Reading::versioned(s, T0, 1.0, 5)])
        .unwrap()
        .positions[0];
    m.seal_active();
    m.compact(None).unwrap();
    m.checkpoint().unwrap();
    let b = m
        .ingest(vec![Reading::versioned(s, T0, 2.0, 4)])
        .unwrap()
        .positions[0];
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    m.drain().unwrap();
    assert!(
        m.received[&C]
            .iter()
            .any(|r| r.delivery.position == b && r.delivery.outranked)
    );
    let cursors: BTreeMap<ConsumerId, Position> = [(C, a)].into_iter().collect();
    let mut r = restore_in_progress(&mut m, &cursors);
    assert!(r.restoring());
    r.drain().unwrap();
    let replayed: Vec<&Envelope> = r.received[&C]
        .iter()
        .map(|x| &x.delivery)
        .filter(|d| d.position == b && d.fact().is_some())
        .collect();
    assert_eq!(replayed.len(), 1);
    assert!(replayed[0].replayed && replayed[0].outranked);
}

/// §8 Eviction at 90 days: a revision chunk's age is not reset when a merge
/// of compacted files (§7) rewrites it; it keeps the seal time of its oldest
/// input.
#[test]
fn s8_merge_keeps_the_age_of_its_oldest_input() {
    let (mut m, s) = model(10 * SECOND);
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    m.advance(60 * DAY);
    for i in 0..4 {
        m.ingest(vec![Reading::num(s, T0 + i * SECOND, 10.0 + i as f64)])
            .unwrap();
        m.seal_active();
        m.compact(None).unwrap();
    }
    let out = m.merge_chunks(None).unwrap();
    let revision = *out
        .iter()
        .find(|f| m.disk.files[*f].class == FileClass::Revision)
        .unwrap();
    m.upload();
    m.advance(29 * DAY);
    assert!(!m.housekeeping().unwrap().contains(&revision));
    m.advance(DAY);
    assert!(
        m.housekeeping().unwrap().contains(&revision),
        "90 days after its oldest input"
    );
}

/// §8 Restore step 2: "A key that gains a second fact meanwhile, in the
/// replay or by ingest, has its index entry completed when its chunk is
/// back." A key with one fact before the checkpoint, revised after it; a
/// merge after the checkpoint puts the old fact in a revision chunk. A replay
/// of the old value is recorded unchecked before the current chunks are back
/// and dropped once they are, while the revision chunk and the log segment
/// holding the old fact are still in the bucket.
#[test]
fn s8_index_entry_completed_when_the_current_chunks_are_back() {
    let (mut m, s) = model(10 * SECOND);
    m.config.merge_threshold = 1;
    m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    m.checkpoint().unwrap();
    m.ingest(vec![Reading::num(s, T0, 2.0)]).unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    let merged = m.merge_chunks(None).unwrap();
    assert_eq!(merged.len(), 2, "a current and a revision chunk");
    assert_eq!(m.upload_checkpoints(), 1);
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let mut early = r.clone();
    let p = early.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    assert_eq!(
        (p.positions.len(), early.counters.duplicates_unchecked),
        (1, 1)
    );
    while let Some(f) = r.restore_queue().first().copied() {
        if r.disk.bucket.objects[&f].1.class != FileClass::Current {
            break;
        }
        r.restore_step().unwrap();
    }
    assert!(r.restoring());
    let p = r.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    assert_eq!((p.positions.len(), p.duplicates), (0, 1));
    assert_eq!(r.counters.duplicates_unchecked, 0);
    assert_eq!(r.current().unwrap()[&(s, T0)].value, n(2.0));
}
