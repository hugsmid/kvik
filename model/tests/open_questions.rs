//! Executable demonstrations of `OPEN-QUESTIONS.md`: each test pins what the
//! model does today where the specification is open, so that a change in the
//! specification shows up here.

mod common;

use common::*;
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, CrashPoint, Error, Fault, Model, Selection, WriteOutcome,
};
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

/// Q-15: a device-stamped series reports every second. After a forward jump
/// of the system clock the window clock moves an hour ahead at once, so the
/// series' 5-minute rolling window is empty although the series is live.
#[test]
fn q15_forward_clock_jump_empties_rolling_window() {
    let (mut m, s) = model(); // S = 10 s
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

/// Q-24: a sync fails, then the machine loses power before a restart. Recovery
/// after the reboot cannot know the sync failed; the model applies the boot rule
/// and keeps a batch that reached the disk whole.
#[test]
fn q24_sync_error_then_power_loss_keeps_whole_batch() {
    let (mut m, s) = model();
    m.emit(Reading::num(s, m.wall, 1.0)).unwrap();
    assert_eq!(
        m.commit(Some(Fault::SyncError(WriteOutcome::Survived))),
        Err(Error::SyncFailed)
    );
    m.power_loss();
    m.restart();
    assert_eq!(m.facts().unwrap().len(), 1);
}

/// Q-25: after a reboot, the model's recovery syncs and stamps the mark with
/// the new boot id even when nothing was past the mark, so a later process
/// crash on the same boot truncates as row 7 requires.
#[test]
fn q25_recovery_stamps_mark_after_reboot() {
    let (mut m, s) = model();
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
