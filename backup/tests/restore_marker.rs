//! The last-uploaded marker at a restore (`format.md` A.17; Q-614): the three cases no chain or tail check sees now
//! fail loudly, a marker behind the records restores, a marker ahead of a lagging listing is retried up to its bound,
//! and a missing, unreadable or mismatched marker each fails with its own error.

mod support;

use std::sync::Arc;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::marker::{Marker, Summary};
use kvik_backup::plan::{PlanMode, TAIL_PROBES};
use kvik_backup::restore::{MARKER_LISTING_TRIES, Prepared, RestoreDriver, RestoreError, Restorer};
use kvik_backup::uploaded::UploadedRecord;
use kvik_files::record::{Body, Rebuild, Record};
use kvik_log::fs::MemFs;
use support::*;

fn rec(epoch: u64, body: Body) -> Record {
    Record::new(epoch, 0, 12, body)
}

fn carried(epoch: u64, old: u64, body: Body) -> Record {
    Record {
        old_epoch: Some(old),
        ..rec(epoch, body)
    }
}

fn rebuild(epoch: u64) -> Record {
    rec(epoch, Body::Rebuild(Rebuild::default()))
}

fn up(previous: u64, record: Record) -> UploadedRecord {
    UploadedRecord { previous, record }
}

fn key(epoch: u64) -> String {
    Keys::new(ID).record(0, epoch)
}

fn marker_key(sequence: u64) -> String {
    Keys::new(ID).marker(0, sequence)
}

/// Two segments of generation 0 and their sealed-segment bodies.
fn segments() -> (Source, Body, Body) {
    let src = source(ID, 2, false);
    let s1 = Body::Sealed(vec![src.segments[0].0.clone()]);
    let s2 = Body::Sealed(vec![src.segments[1].0.clone()]);
    (src, s1, s2)
}

/// Epochs 1, 2, then `skip` epochs skipped before the third and fourth records.
fn plain(s1: &Body, s2: &Body, skip: u64) -> Vec<UploadedRecord> {
    vec![
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1.clone())),
        up(2, rec(3 + skip, s2.clone())),
        up(3 + skip, rec(4 + skip, Body::Empty)),
    ]
}

/// Epochs 1, 2 and the last old record at `last`, then a rebuilt manifest from `last + skip + 1`, not yet whole.
fn unfinished(s1: &Body, s2: &Body, last: u64, skip: u64) -> Vec<UploadedRecord> {
    let r = last + skip + 1;
    vec![
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1.clone())),
        up(2, rec(last, s2.clone())),
        up(0, rebuild(r)),
        up(r, carried(r + 1, 1, Body::Empty)),
        up(r + 1, carried(r + 2, 2, s1.clone())),
    ]
}

/// The records of `chain`, each followed by its marker while fewer than `marked` are put.
fn bucket_marked(mut src: Source, chain: &[UploadedRecord], marked: usize) -> Arc<FaultSink> {
    let mem = MemSink::new();
    upload_files(&mem, &mut src);
    use_copies(&src);
    for (i, u) in chain.iter().enumerate() {
        put_record(&mem, ID, u.previous, &u.record);
        if i < marked {
            let summary = Summary::of(chain[..=i].iter().map(|u| &u.record));
            put_marker(&mem, ID, 0, i as u64 + 1, summary);
        }
    }
    Arc::new(FaultSink::new(mem))
}

fn bucket(src: Source, chain: &[UploadedRecord]) -> Arc<FaultSink> {
    bucket_marked(src, chain, chain.len())
}

fn prepare(sink: &Arc<FaultSink>, mode: PlanMode) -> Result<Prepared, RestoreError> {
    let fs = MemFs::new();
    block_on(restorer(sink.clone(), &fs, mode, KEY).prepare(100))
}

fn both(sink: &Arc<FaultSink>, check: impl Fn(Result<Prepared, RestoreError>)) {
    for mode in [PlanMode::Drill, PlanMode::Full] {
        check(prepare(sink, mode));
    }
}

fn lagging(r: Result<Prepared, RestoreError>) {
    assert!(
        matches!(r, Err(RestoreError::ListingIncomplete { generation: 0 })),
        "{r:?}"
    );
}

fn unreached(sequence: u64, epoch: u64) -> impl Fn(Result<Prepared, RestoreError>) {
    move |r| match r {
        Err(RestoreError::MarkerUnreached {
            generation: 0,
            sequence: s,
            epoch: e,
        }) if (s, e) == (sequence, epoch) => {}
        other => panic!("{other:?}"),
    }
}

/// No record or marker of a later generation reached the bucket.
fn nothing_recorded(sink: &FaultSink) {
    let keys = Keys::new(ID);
    assert!(!sink.inner.objects().keys().any(|k| matches!(
        keys.parse(k),
        Some(ObjectName::Record { generation, .. } | ObjectName::Marker { generation, .. })
            if generation > 0
    )));
}

#[test]
fn q614_a_bucket_with_each_record_marked_restores_in_both_modes() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
}

#[test]
fn q614_a_lost_last_record_fails_as_the_marker_unreached_and_records_nothing() {
    for skip in [0, TAIL_PROBES + 3] {
        let (src, s1, s2) = segments();
        let sink = bucket(src, &plain(&s1, &s2, skip));
        sink.set(&key(4 + skip), Some(Fault::Lose));
        both(&sink, unreached(4, 4 + skip));
        nothing_recorded(&sink);
    }
}

#[test]
fn q614_a_last_record_hidden_behind_skipped_epochs_beyond_the_bound_is_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, TAIL_PROBES + 3));
    sink.set(&key(4 + TAIL_PROBES + 3), Some(Fault::Unlisted));
    both(&sink, lagging);
    nothing_recorded(&sink);
}

#[test]
fn q614_a_lost_last_old_record_before_an_unfinished_rebuilt_manifest_fails_as_the_marker_unreached()
{
    for skip in [0, TAIL_PROBES + 3] {
        let (src, s1, s2) = segments();
        let last = 6 + skip;
        let sink = bucket(src, &unfinished(&s1, &s2, last, skip));
        sink.set(&key(last), Some(Fault::Lose));
        both(&sink, unreached(6, last));
        nothing_recorded(&sink);
    }
}

#[test]
fn q614_a_lost_last_record_of_an_unfinished_rebuilt_manifest_fails_as_the_marker_unreached() {
    let (src, s1, s2) = segments();
    let chain = unfinished(&s1, &s2, 3, 1);
    let last = chain.last().unwrap().record.epoch;
    let sink = bucket(src, &chain);
    sink.set(&key(last), Some(Fault::Lose));
    both(&sink, unreached(6, last));
    nothing_recorded(&sink);
}

#[test]
fn q614_the_last_old_record_hidden_behind_skipped_epochs_beyond_the_bound_before_an_unfinished_rebuilt_manifest_is_a_lagging_listing()
 {
    let (src, s1, s2) = segments();
    let last = 3 + TAIL_PROBES + 3;
    let sink = bucket(src, &unfinished(&s1, &s2, last, TAIL_PROBES + 3));
    sink.set(&key(last), Some(Fault::Unlisted));
    both(&sink, lagging);
}

#[test]
fn q614_a_record_put_before_a_restart_left_its_marker_unput_restores() {
    let (src, s1, s2) = segments();
    let chain = plain(&s1, &s2, 0);
    let sink = bucket_marked(src, &chain, chain.len() - 1);
    assert!(!sink.inner.objects().contains_key(&marker_key(4)));
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
}

#[test]
fn q614_a_marker_ahead_of_a_lagging_listing_is_retried_and_passes_once_the_listing_catches_up() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    sink.set(&key(4), Some(Fault::Unlisted));
    for mode in [PlanMode::Drill, PlanMode::Full] {
        let fs = MemFs::new();
        let mut r = restorer(sink.clone(), &fs, mode, KEY);
        for _ in 1..MARKER_LISTING_TRIES {
            lagging(block_on(r.prepare(100)));
        }
        sink.set(&key(4), None);
        assert_eq!(block_on(r.prepare(100)).unwrap().record.files.len(), 2);
        sink.set(&key(4), Some(Fault::Unlisted));
    }
}

#[test]
fn q614_a_marker_ahead_of_the_listing_at_every_try_fails_as_not_listed_at_the_bound() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    sink.set(&key(4), Some(Fault::Unlisted));
    for mode in [PlanMode::Drill, PlanMode::Full] {
        let fs = MemFs::new();
        let mut r = restorer(sink.clone(), &fs, mode, KEY);
        for _ in 1..MARKER_LISTING_TRIES {
            lagging(block_on(r.prepare(100)));
        }
        match block_on(r.prepare(100)) {
            Err(RestoreError::MarkerNotListed {
                generation: 0,
                epoch: 4,
                tries: MARKER_LISTING_TRIES,
            }) => {}
            other => panic!("{other:?}"),
        }
    }
    nothing_recorded(&sink);
}

#[test]
fn q614_another_failure_between_lagging_tries_starts_the_count_again() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    sink.set(&key(4), Some(Fault::Unlisted));
    let fs = MemFs::new();
    let mut r = restorer(sink.clone(), &fs, PlanMode::Drill, KEY);
    for _ in 1..MARKER_LISTING_TRIES {
        lagging(block_on(r.prepare(100)));
    }
    sink.set(&key(4), Some(Fault::Lose));
    unreached(4, 4)(block_on(r.prepare(100)));
    sink.set(&key(4), Some(Fault::Unlisted));
    for _ in 1..MARKER_LISTING_TRIES {
        lagging(block_on(r.prepare(100)));
    }
}

#[test]
fn q614_the_latest_marker_is_read_before_the_records_are_listed() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    prepare(&sink, PlanMode::Drill).unwrap();
    let log = sink.log();
    let at = |op: &str, k: &str| log.iter().position(|(o, x)| *o == op && x == k).unwrap();
    assert!(at("get", &marker_key(4)) < at("list", &Keys::new(ID).records_prefix(0)));
}

#[test]
fn q614_records_with_no_marker_fail_as_missing_unless_accepted() {
    let (src, s1, s2) = segments();
    let sink = bucket_marked(src, &plain(&s1, &s2, 0), 0);
    both(&sink, |r| {
        assert!(
            matches!(r, Err(RestoreError::MarkerMissing { generation: 0 })),
            "{r:?}"
        );
    });
    nothing_recorded(&sink);
    for mode in [PlanMode::Drill, PlanMode::Full] {
        let fs = MemFs::new();
        let mut c = cfg(mode);
        c.accept_unmarked = true;
        let mut r = Restorer::new(
            sink.clone(),
            Box::new(|_| TestEnvelope::new(KEY)),
            local(&fs),
            hand_plan,
            Box::new(|| 77),
            c,
        );
        assert_eq!(block_on(r.prepare(100)).unwrap().record.files.len(), 2);
    }
}

#[test]
fn q614_accepting_unmarked_records_still_checks_a_marker_that_is_there() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    sink.set(&key(4), Some(Fault::Lose));
    let fs = MemFs::new();
    let mut c = cfg(PlanMode::Drill);
    c.accept_unmarked = true;
    let mut r = Restorer::new(
        sink.clone(),
        Box::new(|_| TestEnvelope::new(KEY)),
        local(&fs),
        hand_plan,
        Box::new(|| 77),
        c,
    );
    unreached(4, 4)(block_on(r.prepare(100)));
}

#[test]
fn q614_a_generation_whose_records_are_all_lost_is_not_skipped_while_its_marker_stands() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    for e in 1..=4 {
        sink.set(&key(e), Some(Fault::Lose));
    }
    both(&sink, unreached(4, 4));
}

#[test]
fn q614_an_unreadable_latest_marker_fails_as_unreadable() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    let mut b = sink.inner.objects()[&marker_key(4)].clone();
    let mid = b.len() / 2;
    b[mid] ^= 1;
    sink.inner.set(&marker_key(4), &b);
    both(&sink, |r| {
        assert!(
            matches!(
                r,
                Err(RestoreError::MarkerUnreadable {
                    generation: 0,
                    sequence: 4,
                    ..
                })
            ),
            "{r:?}"
        );
    });
    nothing_recorded(&sink);
}

#[test]
fn q614_a_marker_naming_another_sequence_than_its_key_is_unreadable() {
    let (src, s1, s2) = segments();
    let chain = plain(&s1, &s2, 0);
    let sink = bucket_marked(src, &chain, 3);
    let m = Marker {
        generation: 0,
        sequence: 3,
        summary: Summary::of(chain.iter().map(|u| &u.record)),
    };
    let k = marker_key(4);
    let sealed = TestEnvelope::new(KEY).seal(&k, &m.encode()).unwrap();
    sink.inner.set(&k, &sealed);
    both(&sink, |r| {
        assert!(
            matches!(r, Err(RestoreError::MarkerUnreadable { sequence: 4, .. })),
            "{r:?}"
        );
    });
}

#[test]
fn q614_a_marker_hidden_by_the_listing_is_found_by_stat() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    sink.set(&key(4), Some(Fault::Lose));
    for s in 1..=4 {
        sink.set(&marker_key(s), Some(Fault::Unlisted));
    }
    both(&sink, unreached(4, 4));
}

#[test]
fn q614_records_read_ending_before_the_markers_range_with_both_ends_listed_fail_as_a_mismatch() {
    let (src, s1, _) = segments();
    let chain = [
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1.clone())),
        up(0, rebuild(3)),
        up(3, carried(4, 1, Body::Empty)),
    ];
    let sink = bucket_marked(src, &chain, 0);
    let summary = Summary {
        epoch: 4,
        open: 1,
        read: Some((1, 4)),
    };
    put_marker(&sink.inner, ID, 0, 1, summary);
    both(&sink, |r| match r {
        Err(RestoreError::MarkerMismatch {
            generation: 0,
            sequence: 1,
            marked: (1, 4),
            read: Some((1, 2)),
        }) => {}
        other => panic!("{other:?}"),
    });
}

#[test]
fn q614_a_full_restore_puts_its_generations_first_marker_after_its_record() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2, 0));
    prepare(&sink, PlanMode::Full).unwrap();
    let keys = Keys::new(ID);
    let log = sink.log();
    let put = |k: &str| log.iter().position(|(o, x)| *o == "put" && x == k).unwrap();
    assert!(put(&keys.record(1, 1)) < put(&keys.marker(1, 1)));
    let b = &sink.inner.objects()[&keys.marker(1, 1)];
    let m = Marker::decode(&TestEnvelope::new(KEY).open(&keys.marker(1, 1), b).unwrap()).unwrap();
    assert_eq!(
        m.summary,
        Summary {
            epoch: 1,
            open: 1,
            read: Some((1, 1))
        }
    );
    assert_eq!(
        prepare(&sink, PlanMode::Drill)
            .unwrap()
            .record
            .source_generation,
        1
    );
}
