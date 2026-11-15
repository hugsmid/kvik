//! The restore executor (roadmap 8.3b; `spec.md` §8 Restore, Generations, Restore drill; Q-82, Q-39 (f)).

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::{ClaimMarker, Keys};
use kvik_backup::plan::{PlanError, PlanMode};
use kvik_backup::restore::{RestoreDriver, RestoreError};
use kvik_files::desc::FileRef;
use kvik_files::{MANIFEST, MANIFEST_NEW};
use kvik_log::fs::{FaultOp, Fs, MemFs, OpenMode};
use kvik_log::manifest::LossCause;
use kvik_sink::fake::{FakeBucket, FakeOp};
use kvik_sink_api::{BackupSink, SinkError};
use support::*;

fn seg(i: u64) -> FileRef {
    FileRef {
        kind: kvik_files::desc::kind::LOG_SEGMENT,
        file_id: i,
    }
}

fn file_key(src: &Source, i: usize) -> String {
    Keys::new(src.id).file(&src.segments[i].0)
}

#[test]
fn a_full_restore_claims_generation_one_uploads_its_record_and_installs_every_file() {
    let mut src = source(ID, 3, true);
    let objects = bucket(&mut src);
    let sink = Arc::new(mem_with(&objects));
    let fs = MemFs::new();
    let mut r = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    let (p, o) = restore(&mut r).unwrap();
    assert!(!p.resumed);
    assert_eq!(
        (p.store_id, p.record.generation, p.record.drill),
        (ID, 1, false)
    );
    assert_eq!(o.installed, [seg(1), seg(2), seg(3)]);
    assert!(o.lost.is_empty());
    assert_installed(&fs, &src, &[]);
    let after = sink.objects();
    let keys = Keys::new(ID);
    let marker = ClaimMarker::decode(&after[&keys.claim(1)]).unwrap();
    assert_eq!(
        marker,
        ClaimMarker {
            generation: 1,
            nonce: p.record.stamp.nonce
        }
    );
    let (restores, lost) = manifest(&fs);
    assert_eq!(restores.len(), 1);
    assert!(lost.is_empty());
    let uploaded = TestEnvelope::new(KEY)
        .open(&keys.record(1, 1), &after[&keys.record(1, 1)])
        .unwrap();
    assert_eq!(uploaded, restores[0].encode().unwrap());
    assert_eq!(restores[0].generation, 1);
    assert_eq!(
        fs.read(&Path::new(DIR).join("STORE")),
        Some(src.store.clone())
    );
    let pr = r.progress();
    assert_eq!((pr.files_total, pr.files_left, pr.bytes_left), (3, 0, 0));
    assert_eq!(
        pr.bytes_total,
        src.copies.values().map(|c| c.size).sum::<u64>()
    );
    assert!(after.contains_key(&keys.probe()));
    assert!(
        !after
            .keys()
            .any(|k| k.starts_with(&format!("{}/", keys.probe())))
    );
    assert_eq!(after.len(), objects.len() + 3);
}

#[test]
fn progress_counts_the_files_and_bytes_left() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    let fs = MemFs::new();
    let mut r = restorer(mem_with(&objects), &fs, PlanMode::Full, KEY);
    block_on(r.prepare(1)).unwrap();
    let total: u64 = src.copies.values().map(|c| c.size).sum();
    let p = r.progress();
    assert_eq!(
        (p.files_total, p.files_left, p.bytes_total, p.bytes_left),
        (2, 2, total, total)
    );
    block_on(r.step(Duration::ZERO));
    let p = r.progress();
    assert_eq!(
        (p.files_left, p.bytes_left),
        (1, total - src.copies[&seg(1)].size)
    );
}

#[test]
fn a_claim_taken_by_another_restore_moves_to_the_next_generation() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    let sink = Arc::new(mem_with(&objects));
    let keys = Keys::new(ID);
    let theirs = ClaimMarker {
        generation: 1,
        nonce: 1,
    }
    .encode();
    block_on(sink.put(&keys.claim(1), theirs.clone().into(), sha256(&theirs))).unwrap();
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!((p.record.generation, p.record.source_generation), (2, 0));
    assert_eq!(sink.objects()[&keys.claim(1)], theirs);
}

#[test]
fn a_higher_claim_after_the_record_supersedes_the_restore() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    let sink = Arc::new(mem_with(&objects));
    let fs = MemFs::new();
    block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1)).unwrap();
    let keys = Keys::new(ID);
    let later = ClaimMarker {
        generation: 2,
        nonce: 9,
    }
    .encode();
    block_on(sink.put(&keys.claim(2), later.clone().into(), sha256(&later))).unwrap();
    let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(2));
    assert!(
        matches!(r, Err(RestoreError::Superseded { by: 2 })),
        "{r:?}"
    );
}

#[test]
fn a_directory_with_files_and_no_restore_record_is_refused_before_the_bucket_is_asked() {
    let mut src = source(ID, 1, false);
    let sink = Arc::new(mem_with(&bucket(&mut src)));
    let fs = MemFs::new();
    for name in ["x", MANIFEST] {
        let fs = fs.clone();
        let f = fs
            .open(&Path::new(DIR).join(name), OpenMode::CreateNew)
            .unwrap();
        f.write_at(b"not a restore", 0).unwrap();
        let before = sink.ops();
        let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
        assert!(matches!(r, Err(RestoreError::NotEmpty)), "{r:?}");
        assert_eq!(sink.ops(), before);
    }
}

#[test]
fn a_wrong_key_fails_before_any_file_is_judged_and_records_nothing() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    for mode in [PlanMode::Full, PlanMode::Drill] {
        let sink = Arc::new(mem_with(&objects));
        let fs = MemFs::new();
        let r = block_on(restorer(sink.clone(), &fs, mode.clone(), OTHER_KEY).prepare(1));
        assert!(matches!(r, Err(RestoreError::WrongKey)), "{r:?}");
        assert_eq!(sink.puts(), 0);
        assert_eq!(sink.objects(), objects);
        assert!(fs.paths().is_empty());
    }
}

#[test]
fn a_wrong_key_on_a_resume_fails_before_any_file_and_changes_nothing() {
    let mut src = source(ID, 2, false);
    let fs = MemFs::new();
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1)).unwrap();
    let paths = fs.paths();
    let objects = sink.inner.objects();
    let mut r = restorer(sink.clone(), &fs, PlanMode::Full, OTHER_KEY);
    assert!(matches!(
        block_on(r.prepare(2)),
        Err(RestoreError::WrongKey)
    ));
    assert_eq!((fs.paths(), sink.inner.objects()), (paths, objects));
    assert_eq!(sink.count("get", &file_key(&src, 0)), 0);
    assert!(manifest(&fs).1.is_empty());
}

#[test]
fn an_empty_bucket_has_no_store_and_two_stores_are_refused() {
    let fs = MemFs::new();
    let r = block_on(restorer(MemSink::new(), &fs, PlanMode::Full, KEY).prepare(1));
    assert!(matches!(r, Err(RestoreError::NoStore)), "{r:?}");
    let mut a = source(ID, 1, false);
    let mut b = source([0x11; 16], 1, false);
    let sink = MemSink::new();
    upload(&sink, &mut a);
    upload(&sink, &mut b);
    let r = block_on(restorer(sink, &fs, PlanMode::Full, KEY).prepare(1));
    match r {
        Err(RestoreError::SeveralStores(ids)) => assert_eq!(ids, [[0x11; 16], ID]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_bucket_with_no_manifest_records_is_refused() {
    let src = source(ID, 1, false);
    let sink = MemSink::new();
    let keys = Keys::new(ID);
    let sealed = TestEnvelope::new(KEY)
        .seal(&keys.store(), &src.store)
        .unwrap();
    block_on(sink.put(&keys.store(), sealed.clone().into(), sha256(&sealed))).unwrap();
    let fs = MemFs::new();
    let r = block_on(restorer(sink, &fs, PlanMode::Drill, KEY).prepare(1));
    assert!(
        matches!(r, Err(RestoreError::Plan(PlanError::NoRecords))),
        "{r:?}"
    );
    assert!(fs.paths().is_empty());
}

#[test]
fn a_file_absent_on_three_tries_over_ten_minutes_is_lost_and_recorded() {
    let mut src = source(ID, 3, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let key = file_key(&src, 1);
    sink.set(&key, Some(Fault::Lose));
    let fs = MemFs::new();
    let mut r = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    let (_, o) = restore(&mut r).unwrap();
    assert_eq!(o.installed, [seg(1), seg(3)]);
    let (file, loss, at) = o.lost[0].clone();
    assert_eq!((o.lost.len(), file), (1, seg(2)));
    assert!(at >= 10 * MIN, "lost at {at:?}");
    assert!(sink.count("stat", &key) >= 3);
    assert_eq!(
        (loss.first, loss.last, loss.identity),
        ((0, 11), (0, 20), (0, 11))
    );
    assert_eq!((loss.file_id, loss.cause), (2, LossCause::LostInBucket));
    assert_eq!(manifest(&fs).1, [loss]);
    assert_installed(&fs, &src, &[seg(2)]);
    assert_eq!(r.progress().files_left, 0);
}

#[test]
fn a_file_absent_while_the_store_descriptor_is_absent_too_is_never_lost() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    let mut r = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    block_on(r.prepare(1)).unwrap();
    sink.set(&file_key(&src, 0), Some(Fault::Lose));
    sink.set(&Keys::new(ID).store(), Some(Fault::Lose));
    let o = steps(&mut r, Duration::ZERO, 600 * MIN).unwrap();
    assert!(o.lost.is_empty());
    assert_eq!(o.installed, [seg(2)]);
    assert!(o.now > 600 * MIN);
    assert_eq!(r.progress().files_left, 1);
}

#[test]
fn a_reported_hash_other_than_the_copy_is_a_loss_at_once() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let key = file_key(&src, 0);
    sink.inner.set(&key, b"other bytes under the same key");
    let fs = MemFs::new();
    let (_, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!(
        o.lost.iter().map(|l| (l.0, l.2)).collect::<Vec<_>>(),
        [(seg(1), Duration::ZERO)]
    );
    assert_eq!((sink.count("stat", &key), sink.count("get", &key)), (1, 0));
}

#[test]
fn three_downloads_failing_with_the_right_key_are_a_loss_and_two_are_not() {
    for fault in [Fault::Damage, Fault::Mismatch] {
        let mut src = source(ID, 2, true);
        let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
        let key = file_key(&src, 0);
        sink.set(&key, Some(fault));
        let fs = MemFs::new();
        let mut r = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
        block_on(r.prepare(1)).unwrap();
        let mut now = Duration::ZERO;
        let mut lost = None;
        while lost.is_none() {
            match block_on(r.step(now)) {
                kvik_backup::restore::RestoreStep::Lost { file, .. } => lost = Some(file),
                kvik_backup::restore::RestoreStep::Retry { after, .. } => now += after,
                kvik_backup::restore::RestoreStep::Installed { .. } => {}
                other => panic!("{other:?}"),
            }
            if lost.is_none() {
                assert!(sink.count("get", &key) < 3, "{fault:?}");
            }
        }
        assert_eq!(
            (lost, sink.count("get", &key)),
            (Some(seg(1)), 3),
            "{fault:?}"
        );
    }
}

#[test]
fn a_plaintext_hash_other_than_the_description_is_lost_after_three_downloads() {
    let mut src = source(ID, 2, false);
    src.segments[1].0.hash = [0xee; 32];
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    let (_, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!(o.lost.iter().map(|l| l.0).collect::<Vec<_>>(), [seg(2)]);
    assert_eq!(sink.count("get", &file_key(&src, 1)), 3);
    assert_eq!(fs.read(&segment_path(&src.segments[1].0)), None);
}

#[test]
fn a_download_failing_transiently_is_retried_and_never_counted() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let key = file_key(&src, 0);
    sink.set(&key, Some(Fault::SlowGet(10)));
    let fs = MemFs::new();
    let (_, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert!(o.lost.is_empty());
    assert_eq!(sink.count("get", &key), 11);
    assert_installed(&fs, &src, &[]);
}

#[test]
fn a_delayed_object_is_retried_and_never_lost() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    sink.set(&file_key(&src, 1), Some(Fault::Delay(60)));
    let fs = MemFs::new();
    let (_, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert!(o.lost.is_empty());
    assert_eq!(o.installed, [seg(1), seg(2)]);
    assert!(o.now > 10 * MIN && o.retries >= 30);
    assert_installed(&fs, &src, &[]);
}

#[test]
fn each_object_lost_damaged_or_delayed_in_turn() {
    let mut src = source(ID, 3, true);
    let objects = bucket(&mut src);
    let keys = Keys::new(ID);
    for key in objects.keys() {
        for fault in [Fault::Lose, Fault::Damage, Fault::Delay(4)] {
            let sink = Arc::new(FaultSink::new(mem_with(&objects)));
            sink.set(key, Some(fault));
            let fs = MemFs::new();
            let mut r = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
            let mut prepared = block_on(r.prepare(1));
            for _ in 0..8 {
                match &prepared {
                    Err(RestoreError::Sink(SinkError::Transient { .. })) => {
                        prepared = block_on(r.prepare(1))
                    }
                    _ => break,
                }
            }
            let file = src.segments.iter().position(|(d, _)| &keys.file(d) == key);
            let ctx = format!("{key} {fault:?}");
            match (file, fault, key == &keys.store()) {
                (Some(i), Fault::Lose | Fault::Damage, _) => {
                    prepared.expect(&ctx);
                    let o = steps(&mut r, Duration::ZERO, 120 * MIN).expect(&ctx);
                    let f = src.segments[i].0.file_ref();
                    assert_eq!(o.lost.iter().map(|l| l.0).collect::<Vec<_>>(), [f], "{ctx}");
                    assert_installed(&fs, &src, &[f]);
                }
                (_, Fault::Delay(_), _) => {
                    prepared.expect(&ctx);
                    let o = steps(&mut r, Duration::ZERO, 120 * MIN).expect(&ctx);
                    assert!(o.lost.is_empty(), "{ctx}");
                    assert_installed(&fs, &src, &[]);
                }
                (None, Fault::Lose, true) => {
                    let e = prepared.expect_err(&ctx);
                    assert!(
                        matches!(&e, RestoreError::Sink(SinkError::NotFound { key: k }) if k == key),
                        "{ctx}: {e:?}"
                    );
                }
                // A lost record object is not visible to a listing (Q-304).
                (None, Fault::Lose, false) => {}
                (None, Fault::Damage, _) => {
                    let e = prepared.expect_err(&ctx);
                    assert!(matches!(e, RestoreError::Envelope(_)), "{ctx}: {e:?}");
                    assert!(fs.paths().is_empty(), "{ctx}");
                }
                other => unreachable!("{other:?}"),
            }
        }
    }
}

fn fake_bucket(src: &mut Source) -> FakeBucket {
    let bucket = FakeBucket::new();
    upload(&bucket.sink(), src);
    use_copies(src);
    bucket
}

fn snapshot(b: &FakeBucket) -> Vec<(String, Vec<u8>)> {
    b.keys()
        .into_iter()
        .map(|k| (k.clone(), b.object(&k).unwrap()))
        .collect()
}

#[test]
fn a_drill_leaves_the_bucket_byte_identical_and_makes_no_put() {
    let mut src = source(ID, 3, true);
    let bucket = fake_bucket(&mut src);
    let keys = Keys::new(ID);
    let claim = ClaimMarker {
        generation: 1,
        nonce: 4,
    }
    .encode();
    block_on(
        bucket
            .sink()
            .put(&keys.claim(1), claim.clone().into(), sha256(&claim)),
    )
    .unwrap();
    let before = snapshot(&bucket);
    let puts = bucket.count(FakeOp::Put);
    let fs = MemFs::new();
    let (p, o) = restore(&mut restorer(bucket.sink(), &fs, PlanMode::Drill, KEY)).unwrap();
    assert_eq!(bucket.count(FakeOp::Put), puts);
    assert_eq!(snapshot(&bucket), before);
    assert!(p.record.drill);
    assert_eq!(p.record.generation, 2);
    assert_eq!(o.installed, [seg(1), seg(2), seg(3)]);
    assert_installed(&fs, &src, &[]);
    let (restores, _) = manifest(&fs);
    assert_eq!(restores.len(), 1);
}

#[test]
fn a_second_drill_gives_the_same_record_apart_from_the_nonce() {
    let mut src = source(ID, 2, false);
    let bucket = fake_bucket(&mut src);
    let puts = bucket.count(FakeOp::Put);
    let run = || {
        let fs = MemFs::new();
        restore(&mut restorer(bucket.sink(), &fs, PlanMode::Drill, KEY))
            .unwrap()
            .0
            .record
    };
    let (mut a, b) = (run(), run());
    assert_ne!(a.stamp.nonce, b.stamp.nonce);
    a.stamp.nonce = b.stamp.nonce;
    assert_eq!(a, b);
    assert_eq!(bucket.count(FakeOp::Put), puts);
}

#[test]
fn a_drill_says_what_it_restored_and_what_was_missing_and_writes_nothing() {
    let mut src = source(ID, 3, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    sink.set(&file_key(&src, 2), Some(Fault::Lose));
    sink.set(&file_key(&src, 0), Some(Fault::Damage));
    let fs = MemFs::new();
    let (p, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Drill, KEY)).unwrap();
    assert!(p.record.drill);
    assert_eq!(o.installed, [seg(2)]);
    let lost: Vec<FileRef> = o.lost.iter().map(|l| l.0).collect();
    assert_eq!(lost, [seg(1), seg(3)]);
    assert_eq!(
        manifest(&fs)
            .1
            .iter()
            .map(|l| l.file_id)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert!(sink.log().iter().all(|(op, _)| *op != "put"));
    assert_eq!(sink.inner.puts(), 0);
}

#[test]
fn a_drill_reads_the_claims_once_and_a_resumed_drill_reads_none() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    block_on(restorer(sink.clone(), &fs, PlanMode::Drill, KEY).prepare(1)).unwrap();
    let claim = Keys::new(ID).claim(1);
    assert_eq!(sink.count("stat", &claim), 1);
    let p = block_on(restorer(sink.clone(), &fs, PlanMode::Drill, KEY).prepare(2)).unwrap();
    assert!(p.resumed);
    assert_eq!(sink.count("stat", &claim), 1);
    assert!(sink.log().iter().all(|(op, _)| *op != "put"));
}

#[test]
fn a_restore_resumed_after_its_record_claims_nothing_more_and_fetches_only_what_is_left() {
    let mut src = source(ID, 3, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    let mut first = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    let p1 = block_on(first.prepare(1)).unwrap();
    block_on(first.step(Duration::ZERO));
    drop(first);
    let claims_before = claims(&sink.inner.objects(), ID);
    let mut second = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    let (p2, o) = restore(&mut second).unwrap();
    assert!(p2.resumed);
    assert_eq!(p2.record, p1.record);
    assert_eq!(claims(&sink.inner.objects(), ID), claims_before);
    assert_eq!(o.installed, [seg(2), seg(3)]);
    assert_eq!(sink.count("get", &file_key(&src, 0)), 1);
    assert_installed(&fs, &src, &[]);
}

#[test]
fn a_resumed_restore_skips_a_file_already_recorded_lost() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    sink.inner.set(&file_key(&src, 0), b"damaged");
    let fs = MemFs::new();
    let mut first = restorer(sink.clone(), &fs, PlanMode::Full, KEY);
    block_on(first.prepare(1)).unwrap();
    assert!(matches!(
        block_on(first.step(Duration::ZERO)),
        kvik_backup::restore::RestoreStep::Lost { .. }
    ));
    drop(first);
    let (_, o) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert!(o.lost.is_empty());
    assert_eq!(o.installed, [seg(2)]);
    assert_eq!(sink.count("stat", &file_key(&src, 0)), 1);
    assert_eq!(manifest(&fs).1.len(), 1);
}

#[test]
fn a_leftover_manifest_before_the_record_is_removed_and_the_next_generation_claimed() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(mem_with(&bucket(&mut src)));
    let fs = MemFs::new();
    fs.fail_nth(FaultOp::Rename, MANIFEST, 0, std::io::ErrorKind::Other);
    let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
    assert!(matches!(r, Err(RestoreError::Io(_))), "{r:?}");
    assert!(fs.read(&Path::new(DIR).join(MANIFEST_NEW)).is_some());
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert!(!p.resumed);
    assert_eq!(p.record.generation, 2);
    assert!(fs.read(&Path::new(DIR).join(MANIFEST_NEW)).is_none());
    assert_eq!(claims(&sink.objects(), ID).len(), 2);
}

fn record_on_disk(fs: &MemFs) -> bool {
    let Some(bytes) = fs.read(&Path::new(DIR).join(MANIFEST)) else {
        return false;
    };
    let Some(h) = kvik_log::header::FileHeader::decode(&bytes) else {
        return false;
    };
    kvik_files::image::read(&bytes, &h.store_id).is_ok_and(|img| {
        img.records()
            .any(|r| r.body.record_type() == kvik_files::record::record_type::RESTORE)
    })
}

/// Runs `prepare` and steps to `Done`; `false` as soon as a call fails after `killed()` turned true.
fn attempt<D: RestoreDriver>(d: &mut D, killed: impl Fn() -> bool) -> bool {
    if let Err(e) = block_on(d.prepare(100)) {
        assert!(killed(), "{e:?}");
        return false;
    }
    let mut now = Duration::ZERO;
    loop {
        match block_on(d.step(now)) {
            kvik_backup::restore::RestoreStep::Done => return true,
            kvik_backup::restore::RestoreStep::Installed { .. } => {}
            kvik_backup::restore::RestoreStep::Retry { after, reason, .. } => {
                assert!(killed(), "{reason}");
                if killed() {
                    return false;
                }
                now += after;
            }
            other => {
                assert!(killed(), "{other:?}");
                return false;
            }
        }
    }
}

/// The finished restore equals the clean one apart from its generation and nonce; a full restore's record is in the
/// bucket under its generation, and a drill changed nothing there.
fn check_finished(
    fs: &MemFs,
    src: &Source,
    clean: &kvik_files::restore::RestoreRecord,
    got: &kvik_files::restore::RestoreRecord,
    after: &std::collections::BTreeMap<String, Vec<u8>>,
    before: &std::collections::BTreeMap<String, Vec<u8>>,
) {
    let mut same = got.clone();
    same.generation = clean.generation;
    same.stamp = clean.stamp;
    assert_eq!(&same, clean);
    assert_installed(fs, src, &[]);
    let (restores, lost) = manifest(fs);
    assert_eq!((restores.len(), lost.len()), (1, 0));
    assert_eq!(restores[0].generation, got.generation);
    if got.drill {
        assert_eq!(after, before);
    } else {
        let keys = Keys::new(ID);
        let up = TestEnvelope::new(KEY)
            .open(
                &keys.record(got.generation, 1),
                &after[&keys.record(got.generation, 1)],
            )
            .unwrap();
        assert_eq!(up, restores[0].encode().unwrap());
        let mine = ClaimMarker::decode(&after[&keys.claim(got.generation)]).unwrap();
        assert_eq!(mine.nonce, got.stamp.nonce);
    }
}

fn clean_run(
    objects: &std::collections::BTreeMap<String, Vec<u8>>,
    mode: PlanMode,
) -> kvik_files::restore::RestoreRecord {
    let fs = MemFs::new();
    restore(&mut restorer(mem_with(objects), &fs, mode, KEY))
        .unwrap()
        .0
        .record
}

#[test]
fn a_restore_killed_at_every_sink_operation_completes_on_the_next_call() {
    let mut src = source(ID, 3, true);
    let objects = bucket(&mut src);
    for mode in [PlanMode::Full, PlanMode::Drill] {
        let clean = clean_run(&objects, mode.clone());
        let mut kills = 0;
        for k in 0.. {
            let sink = Arc::new(mem_with(&objects));
            let fs = MemFs::new();
            sink.kill_at(Some(k));
            let mut r = restorer(sink.clone(), &fs, mode.clone(), KEY);
            if attempt(&mut r, || sink.ops() > k) {
                break;
            }
            kills += 1;
            drop(r);
            let had_record = record_on_disk(&fs);
            let claims_before = claims(&sink.objects(), ID);
            sink.kill_at(None);
            let (p, _) = restore(&mut restorer(sink.clone(), &fs, mode.clone(), KEY)).unwrap();
            assert_eq!(p.resumed, had_record, "kill at {k}");
            if had_record {
                assert_eq!(claims(&sink.objects(), ID), claims_before, "kill at {k}");
            }
            check_finished(&fs, &src, &clean, &p.record, &sink.objects(), &objects);
        }
        assert!(kills > 10, "{mode:?}: {kills}");
    }
}

#[derive(Clone, Copy, Debug)]
enum Crash {
    Restart,
    PowerLoss,
    Partial,
}

#[test]
fn a_restore_killed_at_every_file_operation_completes_after_a_restart() {
    let mut src = source(ID, 3, true);
    let objects = bucket(&mut src);
    for crash in [Crash::Restart, Crash::PowerLoss, Crash::Partial] {
        for mode in [PlanMode::Full, PlanMode::Drill] {
            let clean = clean_run(&objects, mode.clone());
            let mut kills = 0;
            for n in 0.. {
                let sink = Arc::new(mem_with(&objects));
                let fs = MemFs::new();
                fs.crash_after(n);
                let mut r = restorer(sink.clone(), &fs, mode.clone(), KEY);
                if attempt(&mut r, || fs.crashed()) {
                    break;
                }
                kills += 1;
                drop(r);
                match crash {
                    Crash::Restart => fs.restart(),
                    Crash::PowerLoss => fs.power_loss(),
                    Crash::Partial => fs.power_loss_partial(n as u64),
                }
                let had_record = record_on_disk(&fs);
                let claims_before = claims(&sink.objects(), ID);
                let (p, _) = restore(&mut restorer(sink.clone(), &fs, mode.clone(), KEY))
                    .unwrap_or_else(|e| panic!("{crash:?} {mode:?} at {n}: {e:?}"));
                assert_eq!(p.resumed, had_record, "{crash:?} {mode:?} at {n}");
                if had_record {
                    assert_eq!(claims(&sink.objects(), ID), claims_before);
                }
                check_finished(&fs, &src, &clean, &p.record, &sink.objects(), &objects);
            }
            assert!(kills > 10, "{crash:?} {mode:?}: {kills}");
        }
    }
}
