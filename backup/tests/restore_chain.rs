//! The chain of uploaded manifest records at a restore (`format.md` A.17 Uploaded forms; Q-611, Q-612): each break
//! of the chain, a record the listing hides, the tail checks found, not found and unanswered, and the files of the
//! generation read that no record names (Q-613). Each record is followed by its last-uploaded marker (Q-614), whose
//! own cases are in `restore_marker.rs`.

mod support;

use std::sync::Arc;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::marker::Summary;
use kvik_backup::plan::{
    ChainBreak, PlanError, PlanMode, TAIL_PROBES, after_last, check_chain, read_tail,
    records_to_read,
};
use kvik_backup::restore::{Prepared, RestoreDriver, RestoreError};
use kvik_backup::uploaded::UploadedRecord;
use kvik_files::record::{Body, Rebuild, Record};
use kvik_log::fs::MemFs;
use kvik_sink_api::{BackupSink, SinkError};
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

/// Two segments of generation 0 and their sealed-segment records at epochs 2 and 3.
fn segments() -> (Source, Body, Body) {
    let src = source(ID, 2, false);
    let s1 = Body::Sealed(vec![src.segments[0].0.clone()]);
    let s2 = Body::Sealed(vec![src.segments[1].0.clone()]);
    (src, s1, s2)
}

/// Epochs 1 to 4, each naming the one before.
fn plain(s1: &Body, s2: &Body) -> Vec<UploadedRecord> {
    vec![
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1.clone())),
        up(2, rec(3, s2.clone())),
        up(3, rec(4, Body::Empty)),
    ]
}

/// Epochs 1 to 3, then a rebuilt manifest from epoch 5 carrying them; whole with its closing record at epoch 9.
fn rebuilt(s1: &Body, s2: &Body, whole: bool) -> Vec<UploadedRecord> {
    let mut v = plain(s1, s2);
    v.truncate(3);
    v.extend([
        up(0, rebuild(5)),
        up(5, carried(6, 1, Body::Empty)),
        up(6, carried(7, 2, s1.clone())),
        up(7, carried(8, 3, s2.clone())),
    ]);
    if whole {
        v.push(up(8, rec(9, Body::Empty)));
    }
    v
}

fn bucket(mut src: Source, chain: &[UploadedRecord]) -> Arc<FaultSink> {
    let mem = MemSink::new();
    upload_files(&mem, &mut src);
    use_copies(&src);
    for (i, u) in chain.iter().enumerate() {
        put_record(&mem, ID, u.previous, &u.record);
        let summary = Summary::of(chain[..=i].iter().map(|u| &u.record));
        put_marker(&mem, ID, 0, i as u64 + 1, summary);
    }
    Arc::new(FaultSink::new(mem))
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

fn broken(r: Result<Prepared, RestoreError>, epoch: u64, previous: u64) {
    match r {
        Err(RestoreError::BrokenChain {
            generation: 0,
            epoch: e,
            previous: p,
        }) if (e, p) == (epoch, previous) => {}
        other => panic!("{other:?}"),
    }
}

fn lagging(r: Result<Prepared, RestoreError>) {
    assert!(
        matches!(r, Err(RestoreError::ListingIncomplete { generation: 0 })),
        "{r:?}"
    );
}

/// No record of a later generation reached the bucket.
fn nothing_recorded(sink: &FaultSink) {
    let keys = Keys::new(ID);
    assert!(!sink.inner.objects().keys().any(|k| matches!(
        keys.parse(k),
        Some(kvik_backup::keys::ObjectName::Record { generation, .. }) if generation > 0
    )));
}

#[test]
fn check_chain_takes_each_record_naming_the_one_listed_before_it_skipped_epochs_included() {
    let (_, s1, s2) = segments();
    assert_eq!(check_chain(&plain(&s1, &s2)), Ok(()));
    let skipping = [
        up(0, rec(1, Body::Empty)),
        up(1, rec(4, Body::Empty)),
        up(4, rec(9, Body::Empty)),
    ];
    assert_eq!(check_chain(&skipping), Ok(()));
    assert_eq!(check_chain(&rebuilt(&s1, &s2, true)), Ok(()));
    assert_eq!(check_chain(&[]), Ok(()));
}

#[test]
fn check_chain_names_each_break() {
    let e = |p: u64| rec(p, Body::Empty);
    let cases = [
        (
            vec![up(1, e(2)), up(2, e(3))],
            ChainBreak::Missing {
                epoch: 2,
                previous: 1,
            },
        ),
        (
            vec![up(0, e(1)), up(2, e(3))],
            ChainBreak::Missing {
                epoch: 3,
                previous: 2,
            },
        ),
        (
            vec![up(0, e(1)), up(1, e(2)), up(1, e(3))],
            ChainBreak::Forked {
                epoch: 3,
                previous: 1,
            },
        ),
        (
            vec![up(0, e(1)), up(1, e(2)), up(0, e(3))],
            ChainBreak::Forked {
                epoch: 3,
                previous: 0,
            },
        ),
        (
            vec![up(0, e(1)), up(1, e(2)), up(1, e(2))],
            ChainBreak::Repeated { epoch: 2 },
        ),
        (
            vec![up(0, e(1)), up(1, e(5)), up(1, e(2))],
            ChainBreak::Repeated { epoch: 2 },
        ),
    ];
    for (chain, want) in cases {
        assert_eq!(check_chain(&chain), Err(want), "{chain:?}");
    }
}

#[test]
fn read_tail_is_the_epochs_between_the_records_read_and_the_next_listed_or_epoch_1_with_nothing_read()
 {
    let (_, s1, s2) = segments();
    let records = |v: Vec<UploadedRecord>| v.into_iter().map(|u| u.record).collect::<Vec<_>>();
    let partial = records(rebuilt(&s1, &s2, false));
    assert_eq!(read_tail(&partial, records_to_read(&partial)), vec![4]);
    let whole = records(rebuilt(&s1, &s2, true));
    assert_eq!(
        read_tail(&whole, records_to_read(&whole)),
        Vec::<u64>::new()
    );
    let mut next = records(rebuilt(&s1, &s2, false));
    next[3].epoch = 4;
    assert_eq!(read_tail(&next, records_to_read(&next)), Vec::<u64>::new());
    let mut far = records(rebuilt(&s1, &s2, false));
    for r in &mut far[3..] {
        r.epoch += 20;
    }
    assert_eq!(
        read_tail(&far, records_to_read(&far)),
        (4..=3 + TAIL_PROBES).collect::<Vec<_>>()
    );
    let unread = &partial[3..];
    assert_eq!(read_tail(unread, records_to_read(unread)), vec![1]);
}

#[test]
fn after_last_asks_for_the_next_epochs_up_to_the_bound_and_never_past_the_last_epoch() {
    assert_eq!(
        after_last(4).collect::<Vec<_>>(),
        (5..=4 + TAIL_PROBES).collect::<Vec<_>>()
    );
    assert_eq!(after_last(u64::MAX - 1).collect::<Vec<_>>(), vec![u64::MAX]);
    assert_eq!(after_last(u64::MAX).count(), 0);
}

#[test]
fn an_unbroken_chain_restores_in_both_modes() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
}

#[test]
fn a_record_the_bucket_lost_between_two_listed_ones_refuses_the_restore_and_records_nothing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    sink.set(&key(3), Some(Fault::Lose));
    both(&sink, |r| broken(r, 4, 3));
    nothing_recorded(&sink);
}

#[test]
fn a_record_the_listing_hides_between_two_listed_ones_is_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    sink.set(&key(3), Some(Fault::Unlisted));
    both(&sink, lagging);
    nothing_recorded(&sink);
}

#[test]
fn the_first_record_lost_refuses_the_restore_and_hidden_is_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    sink.set(&key(1), Some(Fault::Lose));
    both(&sink, |r| broken(r, 2, 1));
    sink.set(&key(1), Some(Fault::Unlisted));
    both(&sink, lagging);
}

#[test]
fn a_record_naming_an_earlier_record_than_the_one_listed_before_it_refuses_the_restore() {
    let (src, s1, s2) = segments();
    let mut chain = plain(&s1, &s2);
    chain[2].previous = 1;
    let sink = bucket(src, &chain);
    both(&sink, |r| broken(r, 3, 1));
    assert_eq!(sink.count("stat", &key(1)), 0);
}

#[test]
fn a_record_naming_none_after_the_first_refuses_the_restore() {
    let (src, s1, s2) = segments();
    let mut chain = plain(&s1, &s2);
    chain[3].previous = 0;
    let sink = bucket(src, &chain);
    both(&sink, |r| broken(r, 4, 0));
}

#[test]
fn skipped_epochs_are_no_break() {
    let (src, s1, s2) = segments();
    let chain = [
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1)),
        up(2, rec(5, s2)),
        up(5, rec(6, Body::Empty)),
    ];
    let sink = bucket(src, &chain);
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
}

#[test]
fn a_record_of_another_generation_under_this_generations_key_is_invalid() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2)[..3]);
    let other = Record::new(4, 1, 12, Body::Empty);
    let sealed = TestEnvelope::new(KEY)
        .seal(&key(4), &uploaded(3, &other))
        .unwrap();
    block_on(
        sink.inner
            .put(&key(4), sealed.clone().into(), sha256(&sealed)),
    )
    .unwrap();
    both(&sink, |r| {
        assert!(matches!(r, Err(RestoreError::Record(_))), "{r:?}");
    });
}

#[test]
fn a_listing_that_repeats_a_record_fails_as_the_sinks_fault() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    sink.set(&key(2), Some(Fault::ListedTwice));
    both(&sink, |r| {
        assert!(
            matches!(r, Err(RestoreError::Sink(SinkError::Failed { .. }))),
            "{r:?}"
        );
    });
}

#[test]
fn a_break_before_a_whole_rebuilt_manifest_does_not_refuse_the_restore_that_reads_it() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, true));
    sink.set(&key(2), Some(Fault::Lose));
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
}

#[test]
fn a_break_after_the_records_read_refuses_the_restore() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, false));
    sink.set(&key(7), Some(Fault::Lose));
    both(&sink, |r| broken(r, 8, 7));
}

#[test]
fn a_hidden_rebuild_record_of_a_whole_rebuilt_manifest_is_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, true));
    sink.set(&key(5), Some(Fault::Unlisted));
    both(&sink, lagging);
}

#[test]
fn q612_a_record_after_the_last_listed_that_stat_reports_is_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    sink.set(&key(4), Some(Fault::Unlisted));
    both(&sink, lagging);
    nothing_recorded(&sink);
}

#[test]
fn q612_an_unanswered_tail_stat_fails_the_restore_with_the_sinks_error_and_records_nothing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    for mode in [PlanMode::Drill, PlanMode::Full] {
        sink.set(&key(5), Some(Fault::Delay(1)));
        let r = prepare(&sink, mode);
        assert!(
            matches!(r, Err(RestoreError::Sink(SinkError::Transient { .. }))),
            "{r:?}"
        );
    }
    nothing_recorded(&sink);
}

#[test]
fn q612_the_tail_stat_follows_the_listing_before_any_record_is_fetched() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &plain(&s1, &s2));
    prepare(&sink, PlanMode::Drill).unwrap();
    let log = sink.log();
    let at = |op: &str, k: &str| log.iter().position(|(o, x)| *o == op && x == k).unwrap();
    let listed = at("list", &Keys::new(ID).records_prefix(0));
    let tail = at("stat", &key(5));
    assert!(listed < tail && tail < at("get", &key(1)));
}

#[test]
fn q612_the_end_of_the_records_read_before_a_partial_rebuilt_manifest_hidden_or_unanswered_fails() {
    let (src, s1, s2) = segments();
    let mut chain = rebuilt(&s1, &s2, false);
    chain.insert(3, up(3, rec(4, Body::Empty)));
    let sink = bucket(src, &chain);
    sink.set(&key(4), Some(Fault::Unlisted));
    both(&sink, lagging);
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, false));
    for mode in [PlanMode::Drill, PlanMode::Full] {
        sink.set(&key(4), Some(Fault::Delay(1)));
        let r = prepare(&sink, mode);
        assert!(
            matches!(r, Err(RestoreError::Sink(SinkError::Transient { .. }))),
            "{r:?}"
        );
    }
}

#[test]
fn q612_no_record_after_the_records_read_before_a_partial_rebuilt_manifest_restores_them() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, false));
    both(&sink, |r| assert_eq!(r.unwrap().record.files.len(), 2));
    assert!(sink.count("stat", &key(4)) >= 2);
}

/// The key of a compacted file (kind 2) of `generation` with no description behind it.
fn other_kind_key(generation: u32, file_id: u64) -> String {
    let root = Keys::new(ID).store().trim_end_matches("STORE").to_owned();
    format!("{root}files/02/{generation:08x}-{file_id:016x}")
}

fn put_object(sink: &FaultSink, key: &str) {
    let bytes = b"an object no record names".to_vec();
    let sha = sha256(&bytes);
    block_on(sink.inner.put(key, bytes.into(), sha)).unwrap();
}

#[test]
fn q613_files_of_the_source_generation_that_no_record_read_names_are_reported_and_left_out() {
    let (src, s1, _) = segments();
    let second = src.segments[1].0.file_ref();
    let sink = bucket(src, &[up(0, rec(1, Body::Empty)), up(1, rec(2, s1))]);
    put_object(&sink, &other_kind_key(0, 0xaa));
    let other = kvik_files::desc::FileRef {
        kind: 2,
        file_id: 0xaa,
    };
    both(&sink, |r| {
        let p = r.unwrap();
        assert_eq!(p.unnamed, vec![second, other]);
        assert_eq!(p.record.files.len(), 1);
    });
}

#[test]
fn q613_named_files_and_files_of_other_generations_are_not_reported() {
    let (mut src, s1, s2) = segments();
    let mut later = src.segments[0].0.clone();
    later.generation = 1;
    later.file_id = 9;
    src.segments.push((later.clone(), b"generation 1".to_vec()));
    let sink = bucket(src, &plain(&s1, &s2));
    put_object(&sink, &other_kind_key(1, 0xbb));
    assert!(
        sink.inner
            .objects()
            .contains_key(&Keys::new(ID).file(&later))
    );
    let p = prepare(&sink, PlanMode::Drill).unwrap();
    assert_eq!(p.unnamed, Vec::new());
}

/// Epochs 1, 2, then `skip` epochs skipped before the third record.
fn skipping(s1: &Body, s2: &Body, skip: u64) -> Vec<UploadedRecord> {
    vec![
        up(0, rec(1, Body::Empty)),
        up(1, rec(2, s1.clone())),
        up(2, rec(3 + skip, s2.clone())),
    ]
}

#[test]
fn q614_a_last_record_hidden_behind_skipped_epochs_within_the_bound_is_a_lagging_listing() {
    for skip in [1, 3, TAIL_PROBES - 1] {
        let (src, s1, s2) = segments();
        let sink = bucket(src, &skipping(&s1, &s2, skip));
        sink.set(&key(3 + skip), Some(Fault::Unlisted));
        both(&sink, lagging);
        nothing_recorded(&sink);
    }
}

#[test]
fn q614_a_last_record_hidden_behind_more_skipped_epochs_than_the_bound_is_found_by_the_marker() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &skipping(&s1, &s2, TAIL_PROBES));
    sink.set(&key(3 + TAIL_PROBES), Some(Fault::Unlisted));
    both(&sink, lagging);
    nothing_recorded(&sink);
}

#[test]
fn q614_the_last_old_record_hidden_behind_skipped_epochs_before_a_partial_rebuilt_manifest_is_a_lagging_listing()
 {
    let (src, s1, s2) = segments();
    let mut chain = plain(&s1, &s2);
    chain.truncate(2);
    chain.extend([
        up(2, rec(6, s2.clone())),
        up(0, rebuild(9)),
        up(9, carried(10, 1, Body::Empty)),
    ]);
    let sink = bucket(src, &chain);
    sink.set(&key(6), Some(Fault::Unlisted));
    both(&sink, lagging);
}

/// A rebuilt manifest not yet whole, with nothing uploaded before it: what a restore lists when the old records are
/// hidden or were never uploaded.
fn unfinished_only(s1: &Body) -> Vec<UploadedRecord> {
    vec![
        up(0, rebuild(5)),
        up(5, carried(6, 1, Body::Empty)),
        up(6, carried(7, 2, s1.clone())),
    ]
}

#[test]
fn q614_old_records_hidden_before_a_listed_unfinished_rebuilt_manifest_are_a_lagging_listing() {
    let (src, s1, s2) = segments();
    let sink = bucket(src, &rebuilt(&s1, &s2, false));
    for e in 1..=3 {
        sink.set(&key(e), Some(Fault::Unlisted));
    }
    both(&sink, lagging);
}

#[test]
fn q614_an_unfinished_rebuilt_manifest_with_nothing_before_it_reads_nothing() {
    let (src, s1, _) = segments();
    let sink = bucket(src, &unfinished_only(&s1));
    both(&sink, |r| {
        assert!(
            matches!(r, Err(RestoreError::Plan(PlanError::NoRecords))),
            "{r:?}"
        );
    });
}

#[test]
fn q614_a_break_inside_an_unfinished_rebuilt_manifest_read_from_nothing_refuses_the_restore() {
    let (src, s1, _) = segments();
    let mut chain = unfinished_only(&s1);
    chain.push(up(7, carried(8, 3, Body::Empty)));
    let sink = bucket(src, &chain);
    sink.set(&key(7), Some(Fault::Lose));
    both(&sink, |r| broken(r, 8, 7));
}
