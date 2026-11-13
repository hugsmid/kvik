//! Tests of specification rules not covered by an edge-case row, named after
//! the section they check.

mod common;

use common::*;
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, Error, Fault, FileClass, Level, Model, Reason,
    Selection, Wait, WriteOutcome,
};
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
    assert_eq!(m.disk.cursors[&c], 0, "nothing confirmed yet");
    m.drain().unwrap();
    let ps: Vec<u64> = m.observed[&c].iter().map(|o| o.position).collect();
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
    let backed_up: Vec<u64> = m.facts().unwrap().iter().map(|f| f.position).collect();
    m.ingest(vec![Reading::num(s, T0 + SECOND, 2.0)]).unwrap();
    let r = Model::restore_from(m.config.clone(), vec![], &m.disk.bucket, m.wall, false);
    let restored: Vec<u64> = r.facts().unwrap().iter().map(|f| f.position).collect();
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
