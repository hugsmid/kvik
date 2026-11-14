//! One test per row of the edge-case table in spec §9, named after the row.
//! Rows whose spec test is a property test also have a property here.

mod common;

use common::*;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik_model::condition::{Condition, ConditionState, Event, Runner};
use kvik_model::engine::{
    CompactionCrash, Config, ConsumerDecl, ConsumerId, CrashPoint, Error, FORMAT_VERSION, Fault,
    FileClass, Funnel, FunnelId, Level, Model, Reason, Refusal, Selection, TriggerCause, Wait,
    WriteOutcome,
};
use kvik_model::oracle;
use kvik_model::summary::{AggValue, Aggregate, Params};
use kvik_model::tree::{SeriesTree, quarters_in_day};
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
        .define(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            10 * SECOND,
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
    m.ingest(vec![num(s, T0, 2.0)]).unwrap();
    assert_eq!(m.counters.duplicates_unchecked, 1);
}

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

/// A crash during delivery comes after the durable mark: a consumer never sees
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

/// A failed recovery sync leaves the engine read-only before it writes
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
    assert_eq!(m.counters.compaction_discarded, 2);
    // No segment is open, yet recovery stamped the mark with the new boot.
    assert_eq!(m.disk.mark.unwrap().boot, m.boot);
}

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
    assert_eq!(m.counters.compaction_discarded, 2);
    assert_eq!(m.disk.listed_at(m.open_reader().epoch), inputs);
    assert_eq!(durable(&m), before);
}

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
    assert_eq!(m.housekeeping(), Err(Error::Paused));
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
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            SECOND,
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
        .define(
            "vehicle/7/speed",
            Kind::Gauge,
            ValueType::Number,
            "km/h",
            SECOND,
        )
        .unwrap();
    let b = m
        .define(
            "vehicle/7/rpm",
            Kind::Gauge,
            ValueType::Number,
            "1/min",
            SECOND,
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

#[test]
fn row_17a_record_on_change_keep_alive_never_stale() {
    let (mut m, s) = model(); // S = 10 s
    let t0 = m.wall - MINUTE;
    // The value never changes; a keep-alive fact arrives every 10 s.
    let rs: Vec<Reading> = (0..6).map(|i| num(s, t0 + i * 10 * SECOND, 5.0)).collect();
    m.ingest(rs).unwrap();
    let w = m.window(s, t0, m.wall).unwrap();
    assert_eq!(w.coverage, i128::from(MINUTE), "never stale while healthy");
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
    /// for days outside that range).
    #[test]
    fn row_20_dst_days(
        zone in proptest::sample::select(vec![
            "UTC", "Europe/Paris", "Europe/Berlin", "America/New_York", "America/Santiago",
            "Australia/Sydney", "Asia/Kolkata", "Asia/Kathmandu", "Pacific/Chatham", "America/Havana",
            "Australia/Lord_Howe", "Asia/Tokyo", "Africa/Cairo", "America/Sao_Paulo",
        ]),
        day in 0i32..(366 * 6),
        rs in arb_readings(Kind::Gauge, 20),
    ) {
        let tz = TimeZone::get(zone).unwrap();
        let date = Date::constant(2024, 1, 1).checked_add(jiff::Span::new().days(day)).unwrap();
        let q = quarters_in_day(&tz, date).unwrap();
        prop_assert!((92..=100).contains(&q) && q % 2 == 0, "{} {} has {} quarter-hours", zone, date, q);
        // Readings placed on this local day; day node from the tree = direct.
        let (ds, _) = kvik_model::tree::day_span(&tz, date).unwrap();
        let rs: Vec<(Nanos, Value)> = rs.into_iter().map(|(t, v)| (t - T0 + ds, v)).collect();
        let p = Params::new(Kind::Gauge, 7 * SECOND);
        let tree = SeriesTree::build(p, &rs);
        prop_assert_eq!(tree.day(&tz, date).unwrap(), oracle::day(&p, &rs, &tz, date).unwrap());
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
    // The tree agrees.
    let tree = SeriesTree::build(p, &rs);
    assert_eq!(
        tree.window(T0, T0 + 5 * HOUR + SECOND, T0 + 5 * HOUR + SECOND),
        w
    );
}

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
}

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

#[test]
fn row_30_series_ceiling() {
    let cfg = Config {
        series_ceiling: 2,
        ..Config::default()
    };
    let (mut m, _) = model_with(cfg);
    m.define(
        "site/pump-3/flow",
        Kind::Gauge,
        ValueType::Number,
        "l/min",
        SECOND,
    )
    .unwrap();
    let e = m.define(
        "vehicle/8/speed",
        Kind::Gauge,
        ValueType::Number,
        "km/h",
        SECOND,
    );
    assert!(matches!(
        e,
        Err(Error::Registry(
            kvik_model::registry::RegistryError::Ceiling(2)
        ))
    ));
}

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
    // Bad quality is ignored by summaries.
    assert_eq!(m.window(s, T0, T0 + MINUTE).unwrap().count, 0);
}

#[test]
fn row_32_newer_format_refused() {
    let (mut m, _) = sealed_model();
    let f = *m.disk.hot.iter().next().unwrap();
    m.disk.files.get_mut(&f).unwrap().version = FORMAT_VERSION + 1;
    let r = m.open_reader();
    assert_eq!(m.read(&r), Err(Error::NewerFormat(f, FORMAT_VERSION + 1)));
}

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
