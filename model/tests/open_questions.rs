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
    r.restore_step().unwrap(); // current chunk of the registry entries
    r.restore_step().unwrap(); // current chunk of the series
    r.restore_step().unwrap(); // revision chunk
    assert!(!r.checkpoint_unverified());
    assert!(r.restoring(), "a log segment is still remote");
    r.restore_step().unwrap();
    assert!(!r.restoring());
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

/// Q-16: a stale-for condition measures time since the last arrival of any
/// reading, monotonic while the engine runs; a resume rebuilds it from the
/// last reading's `received_at`. A stale-for condition that is true now is
/// reported true in the `resync` after a restart, no transition from a
/// latched true; a series that reports again after the restart clears it.
#[test]
fn q16_stale_for_true_now_reported_true_on_resync() {
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
    m.advance(SECOND);
    m.restart();
    m.drain().unwrap();
    assert_eq!(resync_condition(&m), (ConditionState::Active(true), false));
    put(&mut m, s, 2.0);
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(false)
    );
    // Silent again, but not yet for S + N: a resume reports it false.
    m.advance(10 * SECOND);
    m.reconnect(C).unwrap();
    m.drain().unwrap();
    assert_eq!(resync_condition(&m).0, ConditionState::Active(false));
    m.advance(5 * SECOND);
    m.tick().unwrap();
    let got = m.receive(C, 10).unwrap();
    assert_eq!(
        got[0].trigger().unwrap().state,
        ConditionState::Active(true)
    );
}

/// Q-16 and §4 Timers ("system clock corrections never fire or suppress
/// them"): on resume the time since the last arrival is taken on the host
/// clock that stamped `received_at`, from the last reading committed, not
/// against `now`, which a backward correction leaves ahead, nor from the
/// highest stamp, which it leaves behind. A reading just after a correction
/// of an hour back does not look an hour old, and one followed by silence
/// past S + N is stale.
#[test]
fn q16_backward_clock_correction_neither_fires_nor_suppresses_stale_for() {
    let stale = Condition::StaleFor {
        staleness: 10 * SECOND,
        n: 5 * SECOND,
    };
    for (before, silent, want) in [
        (false, SECOND, false),
        (true, SECOND, false),
        (true, 30 * SECOND, true),
    ] {
        let (mut m, s) = condition_model(stale.clone(), 100);
        if before {
            put(&mut m, s, 1.0);
        }
        m.jump_wall(-HOUR);
        m.advance(SECOND);
        put(&mut m, s, 2.0);
        m.drain().unwrap();
        m.crash();
        m.advance(silent);
        m.restart();
        m.drain().unwrap();
        assert_eq!(
            resync_condition(&m).0,
            ConditionState::Active(want),
            "reading before the correction: {before}, silent {silent}"
        );
    }
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
            generation: 0,
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
            generation: 0,
            funnel: F,
            sequence: 2
        }
    );
}

/// Q-61: a restore starts a new store whose sequences begin again, so the
/// model puts the generation into a condition delivery's identity: the first
/// transition after a restore is `(1, F, 1)`, not the `(0, F, 1)` a consumer
/// may still remember.
#[test]
fn q61_condition_delivery_identity_includes_the_generation() {
    let (mut m, s) = condition_model(
        Condition::Threshold {
            on: 50.0,
            clear: 40.0,
        },
        100,
    );
    put(&mut m, s, 90.0);
    let before = m.receive(C, 10).unwrap()[0].id();
    assert_eq!(
        before,
        DeliveryId::Timer {
            generation: 0,
            funnel: F,
            sequence: 1
        }
    );
    m.seal_active();
    m.upload();
    let mut r = Model::restore_from(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        m.wall,
        false,
    )
    .unwrap();
    r.drain().unwrap();
    r.advance(SECOND);
    put(&mut r, s, 10.0);
    let after = r.receive(C, 10).unwrap()[0].id();
    assert_eq!(
        after,
        DeliveryId::Timer {
            generation: 1,
            funnel: F,
            sequence: 1
        }
    );
    assert_ne!(after, before);
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

/// Q-63: a threshold whose value now lies between its clear and activation
/// levels is resumed with the latched state (false with no latch).
#[test]
fn q63_threshold_between_levels_resumes_from_the_latch() {
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

/// Q-71: a log segment stays listed beside the chunks compacted from it, so
/// one epoch lists two live files holding the same position. The model
/// checks invariant 14 among compacted files only, and does not compact a
/// segment again whose positions live chunks hold (also after a restore).
#[test]
fn q71_log_segment_and_its_chunks_listed_in_one_epoch() {
    let (mut m, s, _) = checkpointed_model();
    let p = Position::new(0, 2);
    let epoch = m.disk.latest_epoch();
    let holding: Vec<FileClass> = m
        .disk
        .listed_at(epoch)
        .into_iter()
        .filter(|f| m.disk.files[f].entries().any(|e| e.position == p))
        .map(|f| m.disk.files[&f].class)
        .collect();
    assert_eq!(holding, vec![FileClass::Segment, FileClass::Current]);
    assert_eq!(inv14_overlap(&m), None);
    let mut r = Model::restore_from(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        m.wall,
        false,
    )
    .unwrap();
    r.ingest(vec![Reading::num(s, T0 + HOUR, 1.0)]).unwrap();
    r.seal_active();
    let out = r.compact(None).unwrap();
    // The segment never compacted before the restore is compacted now; the
    // one compacted into the restored chunks is not.
    assert!(!out.is_empty());
    assert!(
        out.iter()
            .all(|f| r.disk.files[f].entries().all(|e| e.position != p))
    );
    assert_eq!(inv14_overlap(&r), None);
}

/// Q-72: a replay whose key's detail is off every local tier is recorded
/// unchecked although the revised-key index holds it. On a series that is
/// not versioned it reverts the correction; on a versioned one it is
/// outranked.
#[test]
fn q72_replay_off_the_machine_recorded_unchecked_although_the_index_holds_it() {
    use kvik_model::registry::{ChangedAtKind, SeriesOptions};
    let cfg = Config {
        disk_bytes: 64 * 4,
        low_mark: 0,
        critical_mark: 0,
        ..Config::default()
    };
    let mut m = Model::new(cfg, vec![], T0 + HOUR);
    let s = m
        .define(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
        )
        .unwrap();
    let v = m
        .define_with(
            "site/forecast-1/load",
            Kind::Gauge,
            ValueType::Number,
            "kW",
            10 * SECOND,
            SeriesOptions::versioned(ChangedAtKind::Version),
        )
        .unwrap();
    m.ingest(vec![
        Reading::num(s, T0, 1.0),
        Reading::versioned(v, T0, 1.0, 1),
    ])
    .unwrap();
    m.ingest(vec![
        Reading::num(s, T0, 2.0),
        Reading::versioned(v, T0, 2.0, 2),
    ])
    .unwrap();
    m.seal_active();
    m.compact(None).unwrap();
    m.upload();
    // Fill pressure moves the current detail; the revision chunks leave at
    // 90 days. Only the registry's chunk stays.
    m.advance(91 * 24 * HOUR);
    m.housekeeping().unwrap();
    assert!(m.disk.local_positions().iter().all(|p| p.sequence <= 2));
    let r = m.ingest(vec![Reading::num(s, T0, 1.0)]).unwrap();
    assert_eq!((r.positions.len(), m.counters.duplicates_unchecked), (1, 1));
    assert_eq!(
        m.current().unwrap()[&(s, T0)].value,
        Value::Num(1.0),
        "reverted"
    );
    let r = m.ingest(vec![Reading::versioned(v, T0, 1.0, 1)]).unwrap();
    assert_eq!((r.positions.len(), m.counters.duplicates_unchecked), (1, 2));
    assert_eq!(
        m.current().unwrap()[&(v, T0)].value,
        Value::Num(2.0),
        "outranked"
    );
}
