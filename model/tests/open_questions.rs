//! Executable demonstrations of `OPEN-QUESTIONS.md`: each test pins what the
//! model does today where the specification is open, so that a change in the
//! specification shows up here. When a question is settled, its test moves to
//! `rules.rs` or `model.rs`.

mod common;

use std::collections::BTreeMap;

use common::*;
use kvik_model::condition::{Condition, ConditionState};
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, DeliveryId, EntryBody, Error, FileClass, FileId, Funnel,
    FunnelId, Level, Model, Reason, Selection, TriggerCause,
};
use kvik_model::types::{
    HOUR, Kind, MINUTE, Position, Reading, SECOND, SeriesId, Value, ValueType,
};

/// A store as [`checkpointed_model`] with two more segments, `0:9` and
/// `0:10`, both uploaded; the one holding `0:9` is then missing from the
/// bucket, so the restored end is `0:8`. Returns the segment holding `0:10`.
fn gap_model() -> (Model, SeriesId, FileId) {
    let (mut m, s, _) = checkpointed_model();
    m.ingest(vec![Reading::num(s, T0 + 250 * SECOND, 6.0)])
        .unwrap();
    m.seal_active();
    let missing = *m.disk.files.keys().next_back().unwrap();
    m.ingest(vec![Reading::num(s, T0 + 260 * SECOND, 7.0)])
        .unwrap();
    m.seal_active();
    let beyond = *m.disk.files.keys().next_back().unwrap();
    m.upload();
    m.disk.bucket.objects.remove(&missing);
    (m, s, beyond)
}

/// Q-37: a restore reads every bucket object whatever its generation, and its
/// record carries no files forward, so a second restore after a restore with
/// a gap finds the old gap again and sets aside the generation the first
/// restore started. This is a known defect until Q-37 is settled.
#[test]
fn q37_second_restore_finds_the_old_gap_again() {
    let (mut m, s, _) = gap_model();
    let mut r =
        Model::restore_from(m.config.clone(), vec![], &mut m.disk.bucket, m.wall, false).unwrap();
    assert_eq!(r.disk.restored_end, Some(Position::new(0, 8)));
    let p = r.ingest(vec![Reading::num(s, T0 + HOUR, 9.0)]).unwrap();
    assert_eq!(p.positions, vec![Position::new(1, 1)]);
    r.seal_active();
    r.upload();
    let r2 =
        Model::restore_from(r.config.clone(), vec![], &mut r.disk.bucket, r.wall, false).unwrap();
    assert_eq!(r2.disk.generation, 2);
    assert_eq!(r2.disk.restored_end, Some(Position::new(0, 8)));
    assert!(
        r2.facts()
            .unwrap()
            .iter()
            .all(|f| f.position.generation == 0)
    );
}

/// Q-38: segments beyond the restored end are downloaded in step 1 with the
/// log after the checkpoint, kept, and set aside by position.
#[test]
fn q38_segments_beyond_restored_end_downloaded_in_step_1_and_set_aside() {
    let (mut m, _, beyond) = gap_model();
    let r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.disk.published().contains(&beyond));
    assert!(
        r.facts()
            .unwrap()
            .iter()
            .all(|f| f.position <= Position::new(0, 8))
    );
}

/// Q-39 (a): a downloaded file holding a position whose content differs from
/// the local copy is not installed; the restore stays in progress and the
/// same step fails again.
#[test]
fn q39_conflicting_file_leaves_restore_in_progress() {
    let (mut m, _, _) = checkpointed_model();
    let p = Position::new(0, 2);
    let log = *m
        .disk
        .bucket
        .objects
        .iter()
        .find(|(_, (_, f))| f.class == FileClass::Segment && f.entries().any(|e| e.position == p))
        .unwrap()
        .0;
    let (_, file) = m.disk.bucket.objects.get_mut(&log).unwrap();
    for e in file.batches.iter_mut().flatten() {
        if let (true, EntryBody::Fact(f)) = (e.position == p, &mut e.body) {
            f.value = Value::Num(7.0);
        }
    }
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let conflict = Err(Error::Conflict {
        file: log,
        position: p,
    });
    assert_eq!(r.restore_rest(), conflict);
    assert!(r.restoring());
    assert_eq!(r.restore_step().map(|_| ()), conflict);
}

/// Q-39 (b): the record precedes every download, so a restore that stops
/// before its record has nothing local to clear.
#[test]
fn q39_nothing_local_before_the_record() {
    let (mut m, _, _) = checkpointed_model();
    let r = Model::restore_interrupted_before_record(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
    )
    .unwrap();
    assert!(r.disk.files.is_empty());
}

/// Q-39 (c): `restoring` ends when every file the restore reads is local. The
/// `derived-unverified` label can go earlier, once every span the checkpoint
/// covers has been verified.
#[test]
fn q39_restoring_ends_when_every_file_is_local() {
    let (mut m, _, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    r.restore_step().unwrap(); // current chunk
    r.restore_step().unwrap(); // revision chunk
    assert!(!r.checkpoint_unverified());
    assert!(r.restoring(), "a log segment is still remote");
    r.restore_step().unwrap();
    assert!(!r.restoring());
}

/// Q-39 (d): the revised-key index counts as back once every current chunk
/// is local; the model takes it from the restored facts. A replay of a
/// revised key's earlier value is then dropped although the revision chunk
/// holding it is still remote.
#[test]
fn q39_revised_key_index_back_with_current_chunks() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    let replay = Reading::num(s, T0 + 20 * SECOND, 1.0);
    let mut before = r.clone();
    let p = before.ingest(vec![replay.clone()]).unwrap();
    assert_eq!(p.positions.len(), 1, "recorded unchecked");
    assert_eq!(before.counters.duplicates_unchecked, 1);
    let f = r.restore_step().unwrap().unwrap();
    assert_eq!(r.disk.files[&f].class, FileClass::Current);
    let p = r.ingest(vec![replay]).unwrap();
    assert_eq!((p.positions.len(), p.duplicates), (0, 1));
    assert_eq!(r.counters.duplicates_unchecked, 0);
}

/// Q-39: while restoring, a window bound inside a minute is refused, and no
/// tree checkpoint is taken.
#[test]
fn q39_no_sub_minute_query_or_checkpoint_while_restoring() {
    let (mut m, s, _) = checkpointed_model();
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert_eq!(
        r.query_window(s, T0 + SECOND, T0 + MINUTE),
        Err(Error::Restoring)
    );
    assert_eq!(r.checkpoint(), Err(Error::Restoring));
    r.restore_rest().unwrap();
    assert!(r.query_window(s, T0 + SECOND, T0 + MINUTE).is_ok());
    assert!(r.checkpoint().is_ok());
}

/// Q-39: a restore in progress is no health condition of its own; §8 does not
/// say whether `restoring` shows in health.
#[test]
fn q39_restoring_is_no_health_condition() {
    let (mut m, _, _) = checkpointed_model();
    let r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(r.restoring());
    assert_eq!(r.health().level, Level::Ok);
}

/// Q-39: when starting a restore again after a crash before its record, or
/// finishing its step 1 after a crash, fails (here a position held twice with
/// different content), the engine stays down and health reports it.
#[test]
fn q39_failed_restart_of_a_restore_is_reported() {
    let (mut m, _, _) = checkpointed_model();
    let p = Position::new(0, 7);
    let (hash, mut copy) = m
        .disk
        .bucket
        .objects
        .values()
        .find(|(_, f)| f.class == FileClass::Segment && f.entries().any(|e| e.position == p))
        .cloned()
        .unwrap();
    for e in copy.batches.iter_mut().flatten() {
        if let (true, EntryBody::Fact(f)) = (e.position == p, &mut e.body) {
            f.value = Value::Num(7.0);
        }
    }
    m.disk.bucket.objects.insert(FileId(99), (hash, copy));
    let conflict = Some(Error::Conflict {
        file: FileId(99),
        position: p,
    });
    let mut before = Model::restore_interrupted_before_record(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
    )
    .unwrap();
    let mut after = Model::restore_interrupted_after_record(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        &BTreeMap::new(),
        m.wall,
        0,
    )
    .unwrap();
    let claimed_before = before.disk.bucket.claims.len();
    for r in [&mut before, &mut after] {
        r.restart();
        assert!(!r.is_up());
        assert_eq!(r.disk.restore_failed, conflict);
        let h = r.health();
        assert_eq!(h.level, Level::Fatal);
        assert!(h.reasons.contains(&Reason::RestoreFailed));
    }
    // The failed retry's claim persists, so another retry takes the next
    // generation (§8 Generations).
    assert_eq!(before.disk.bucket.claims.len(), claimed_before + 1);
}

// ----- delivery -----

const C: ConsumerId = ConsumerId(1);
const F: FunnelId = FunnelId(1);

/// A consumer with no configured selection, one gauge with S = 10 s, and one
/// condition funnel `F` over it on the live-state value.
fn condition_model(cond: Condition, capacity: usize) -> (Model, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(Default::default()),
        capacity,
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
    m.add_route(C, F, Funnel::condition(s, None, cond)).unwrap();
    (m, s)
}

fn put(m: &mut Model, s: SeriesId, v: f64) -> Position {
    let now = m.wall;
    m.ingest(vec![Reading::num(s, now, v)]).unwrap().positions[0]
}

fn resync_condition(m: &Model) -> (ConditionState, bool) {
    let c = m.received[&C]
        .iter()
        .rev()
        .find_map(|r| r.delivery.resync().cloned())
        .expect("a resync")
        .funnels
        .into_iter()
        .find_map(|f| f.condition)
        .expect("a condition funnel");
    (c.state, c.transition)
}

/// Q-16: a stale-for condition measures monotonic time since the last
/// arrival, which a restart does not keep. The `resync` after a restart
/// reports it false, a transition from a latched true, and it fires again
/// only after S + N more of silence.
#[test]
fn q16_stale_for_resyncs_false_after_restart() {
    let stale = Condition::StaleFor {
        staleness: 10 * SECOND,
        n: 5 * SECOND,
    };
    let (mut m, s) = condition_model(stale, 100);
    put(&mut m, s, 1.0);
    m.advance(15 * SECOND);
    m.tick().unwrap();
    m.drain().unwrap();
    assert_eq!(m.disk.latches[&(C, F)].state, ConditionState::Active(true));
    m.crash();
    m.restart();
    m.drain().unwrap();
    assert_eq!(resync_condition(&m), (ConditionState::Active(false), true));
    m.advance(15 * SECOND);
    m.tick().unwrap();
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
}

/// Q-61: every condition delivery, on a routed fact or on the timer, is
/// identified by `(funnel, sequence)`, and the sequence is kept on disk when
/// it is issued. A delivery taken but not confirmed before a crash does not
/// have its sequence issued again for a different transition, which a
/// consumer de-duplicating by `(funnel, sequence)` would drop.
#[test]
fn q61_condition_delivery_sequence_survives_a_crash() {
    let (mut m, s) = condition_model(
        Condition::Threshold {
            on: 50.0,
            clear: 40.0,
        },
        100,
    );
    put(&mut m, s, 90.0);
    let first = m.receive(C, 10).unwrap();
    assert_eq!(
        first[0].id(),
        DeliveryId::Timer {
            funnel: F,
            sequence: 1
        }
    );
    assert_eq!(first[0].trigger().unwrap().cause, TriggerCause::Fact);
    m.crash();
    m.restart();
    m.drain().unwrap();
    put(&mut m, s, 10.0);
    let next = m.receive(C, 10).unwrap();
    assert_eq!(
        next[0].id(),
        DeliveryId::Timer {
            funnel: F,
            sequence: 2
        }
    );
}

/// Q-62: a transition while the consumer is catching up after overflow is
/// not sent then (it is not in the log); on re-attaching to live the
/// consumer receives the present state, because it differs from the last
/// state emitted to it.
#[test]
fn q62_transition_during_overflow_sent_on_reattach() {
    let (mut m, s) = condition_model(
        Condition::Threshold {
            on: 50.0,
            clear: 40.0,
        },
        2,
    );
    m.add_route(C, FunnelId(2), Funnel::facts(Selection::All))
        .unwrap();
    put(&mut m, s, 1.0);
    put(&mut m, s, 2.0);
    let p = put(&mut m, s, 90.0);
    assert!(m.in_catch_up(C));
    m.drain().unwrap();
    let stream = session_stream(&m, C, 1);
    assert_eq!(labels(&stream), ["F4", "F5", "R6", "T1:1"]);
    let t = stream[3].trigger().unwrap();
    assert_eq!(
        (t.cause, &t.state),
        (TriggerCause::Reattach, &ConditionState::Active(true))
    );
    assert_eq!(stream[3].position, p);
}

/// Q-63: a condition with memory is resumed from the latch. A threshold
/// whose value now lies between its clear and activation levels keeps the
/// latched state (false with no latch); a held-for condition is true only if
/// it is above now and was latched true, otherwise its hold starts at the
/// resume, although it may have been held above for longer while down.
#[test]
fn q63_conditions_with_memory_resume_from_the_latch() {
    let band = Condition::Threshold {
        on: 50.0,
        clear: 40.0,
    };
    let (mut m, s) = condition_model(band, 100);
    put(&mut m, s, 90.0);
    m.drain().unwrap();
    m.crash();
    put_while_down(&mut m, s, 45.0);
    m.drain().unwrap();
    assert_eq!(resync_condition(&m), (ConditionState::Active(true), false));
    m.disk.latches.clear();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(resync_condition(&m), (ConditionState::Active(false), true));

    let held = Condition::HeldFor {
        above: 5.0,
        n: 10 * SECOND,
    };
    let (mut m, s) = condition_model(held, 100);
    put(&mut m, s, 9.0);
    m.advance(10 * SECOND);
    m.tick().unwrap();
    put(&mut m, s, 1.0);
    m.drain().unwrap();
    assert_eq!(m.disk.latches[&(C, F)].state, ConditionState::Active(false));
    // While the consumer is down the value is held above for 30 s and the
    // condition fires; the consumer restarts before taking it.
    put(&mut m, s, 9.0);
    m.advance(30 * SECOND);
    m.tick().unwrap();
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(resync_condition(&m), (ConditionState::Active(false), false));
    m.advance(10 * SECOND);
    m.tick().unwrap();
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
}

/// Restarts `m` and commits one fact for `s` before the consumer catches up,
/// standing in for a value that arrived while the consumer was down.
fn put_while_down(m: &mut Model, s: SeriesId, v: f64) {
    m.restart();
    m.receive(C, 10).unwrap();
    put(m, s, v);
    m.reconnect(C).unwrap();
}

/// Q-64: routing changes are log entries, and a tree checkpoint carries the
/// registry, not the routes. During a restore from a checkpoint, a route added
/// before the checkpoint's position is not in force until the file holding
/// its entry is back, so new facts are not delivered by it meanwhile.
#[test]
fn q64_route_before_checkpoint_not_in_force_until_its_entry_is_back() {
    let consumer = ConsumerDecl {
        id: C,
        selection: Selection::Series(Default::default()),
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            2 * MINUTE,
        )
        .unwrap();
    m.add_route(C, F, Funnel::facts(Selection::All)).unwrap();
    for t in [0, 20, 40] {
        m.ingest(vec![Reading::num(s, T0 + t * SECOND, 1.0)])
            .unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    m.checkpoint().unwrap();
    m.ingest(vec![Reading::num(s, T0 + 150 * SECOND, 4.0)])
        .unwrap();
    m.seal_active();
    m.upload();
    assert_eq!(m.upload_checkpoints(), 1);
    let mut r = restore_in_progress(&mut m, &BTreeMap::new());
    assert!(!r.routes(C).unwrap().contains_key(&F));
    let now = r.wall;
    let p = r.ingest(vec![Reading::num(s, now, 5.0)]).unwrap().positions[0];
    r.drain().unwrap();
    assert!(!observed_positions(&r, C.0).contains(&p));
    r.restore_rest().unwrap();
    assert!(r.routes(C).unwrap().contains_key(&F));
    let q = r.ingest(vec![Reading::num(s, now, 6.0)]).unwrap().positions[0];
    r.drain().unwrap();
    assert!(observed_positions(&r, C.0).contains(&q));
}
