//! Executable demonstrations of `OPEN-QUESTIONS.md`: each test pins what the
//! model does today where the specification is open, so that a change in the
//! specification shows up here. When a question is settled, its test moves to
//! `rules.rs` or `model.rs`.

mod common;

use std::collections::BTreeMap;

use common::*;
use kvik_model::engine::{EntryBody, Error, FileClass, FileId, Model};
use kvik_model::types::{HOUR, MINUTE, Position, Reading, SECOND, SeriesId, Value};

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
