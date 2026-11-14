//! Tests of specification rules not covered by an edge-case row, named after
//! the section they check.

mod common;

use std::collections::BTreeMap;

use common::*;
use kvik_model::condition::{Condition, ConditionState};
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, DeliveryId, DeliveryKind, EntryBody, Error,
    Fault, FileClass, FileId, Funnel, FunnelId, Level, ManifestOp, Model, Operator, Reason,
    Selection, Signal, TriggerCause, Wait, WriteOutcome,
};
use kvik_model::oracle;
use kvik_model::summary::{AggValue, Aggregate, Body, Params, direct};
use kvik_model::tree::SeriesTree;
use kvik_model::types::{
    DAY, HOUR, Kind, MINUTE, Nanos, Position, Quality, Reading, SECOND, SeriesId, Value, ValueType,
    exact,
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
            matches!(x.op, ManifestOp::Add { file, .. }
                if r.disk.files[&file].entries().any(|e| e.position == Position::new(1, 1)))
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
    assert_eq!(
        order,
        vec![FileClass::Current, FileClass::Revision, FileClass::Segment]
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
    r.restore_step().unwrap(); // current chunk
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
    assert_eq!(r.restore_queue().len(), 2);
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
                    funnel: F,
                    sequence: 1
                },
                p
            ),
            (
                DeliveryId::Timer {
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
