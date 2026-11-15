//! Guards of the restore executor: a drill never writes, claim retries, lagging listings, the bucket probe and the
//! records read past a rebuilt manifest not yet whole (`spec.md` §8 Generations, Restore, Restore drill).

mod support;

use std::sync::Arc;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{MemSink, TestEnvelope, block_on};
use kvik_backup::keys::{ClaimMarker, Keys, ObjectName};
use kvik_backup::plan::PlanMode;
use kvik_backup::restore::{Prepared, RestoreDriver, RestoreError};
use kvik_backup::upload::Refused;
use kvik_files::record::{Body, Rebuild, Record};
use kvik_log::fs::MemFs;
use kvik_sink::fake::FakeBucket;
use kvik_sink_api::{BackupSink, SinkError};
use support::*;

#[test]
fn a_drill_over_a_full_restore_whose_record_upload_failed_is_refused_and_puts_nothing() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let record = Keys::new(ID).record(1, 1);
    sink.set(&record, Some(Fault::Delay(u32::MAX)));
    let fs = MemFs::new();
    let full = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
    assert!(matches!(full, Err(RestoreError::Sink(_))), "{full:?}");
    sink.set(&record, None);
    let objects = sink.inner.objects();
    let before = sink.log().len();
    let drill = block_on(restorer(sink.clone(), &fs, PlanMode::Drill, KEY).prepare(2));
    assert!(sink.log()[before..].iter().all(|(op, _)| *op != "put"));
    assert_eq!(sink.inner.objects(), objects);
    assert!(!objects.contains_key(&record));
    assert!(matches!(drill, Err(RestoreError::Io(_))), "{drill:?}");
}

#[test]
fn a_claim_rejected_with_nothing_stored_retries_the_same_generation() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let claim = Keys::new(ID).claim(1);
    sink.set(&claim, Some(Fault::RejectPut(2)));
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!(p.record.generation, 1);
    assert_eq!(sink.count("put", &claim), 3);
    let mine = ClaimMarker::decode(&sink.inner.objects()[&claim]).unwrap();
    assert_eq!(mine.nonce, p.record.stamp.nonce);
    assert_eq!(claims(&sink.inner.objects(), ID), [claim]);
}

#[test]
fn a_claim_rejected_after_a_marker_was_stored_takes_the_next_generation() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let keys = Keys::new(ID);
    sink.set(&keys.claim(1), Some(Fault::StoreThenMismatch));
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!((p.record.generation, p.record.source_generation), (2, 0));
    assert_eq!(sink.count("put", &keys.claim(1)), 1);
}

#[test]
fn the_three_claim_tries_count_per_generation() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let keys = Keys::new(ID);
    sink.set(&keys.claim(1), Some(Fault::StoreThenMismatch));
    sink.set(&keys.claim(2), Some(Fault::RejectPut(2)));
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!(p.record.generation, 2);
    assert_eq!(sink.count("put", &keys.claim(2)), 3);
}

#[test]
fn a_claim_rejected_three_times_with_nothing_stored_fails_and_skips_no_generation() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let claim = Keys::new(ID).claim(1);
    sink.set(&claim, Some(Fault::RejectPut(10)));
    let fs = MemFs::new();
    let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
    assert!(
        matches!(r, Err(RestoreError::Sink(SinkError::HashMismatch { .. }))),
        "{r:?}"
    );
    assert_eq!(sink.count("put", &claim), 3);
    assert!(claims(&sink.inner.objects(), ID).is_empty());
}

#[test]
fn a_generation_whose_records_list_empty_while_its_restore_record_is_stored_is_refused_not_skipped()
{
    let mut src = source(ID, 2, false);
    let bucket = fake_bucket(&mut src);
    bucket.set_list_lag(1_000);
    let first = MemFs::new();
    let (p, _) = restore(&mut restorer(bucket.sink(), &first, PlanMode::Full, KEY)).unwrap();
    assert_eq!(p.record.generation, 1);
    let record = Keys::new(ID).record(1, 1);
    assert!(bucket.object(&record).is_some());
    for mode in [PlanMode::Drill, PlanMode::Full] {
        let fs = MemFs::new();
        let r = block_on(restorer(bucket.sink(), &fs, mode.clone(), KEY).prepare(1));
        assert!(
            matches!(r, Err(RestoreError::ListingIncomplete { generation: 1 })),
            "{mode:?}: {r:?}"
        );
        assert!(fs.paths().is_empty(), "{mode:?}");
    }
}

fn fake_bucket(src: &mut Source) -> FakeBucket {
    let bucket = FakeBucket::new();
    upload(&bucket.sink(), src);
    use_copies(src);
    bucket
}

fn refused(r: Result<Prepared, RestoreError>) -> Option<Refused> {
    match r {
        Err(RestoreError::Refused(x)) => Some(x),
        _ => None,
    }
}

#[test]
fn a_bucket_without_conditional_writes_is_refused_before_the_claim() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    let keys = Keys::new(ID);
    for earlier in [false, true] {
        for fault in [Fault::Overwrite, Fault::OverwriteQuietly, Fault::Pretend] {
            let sink = Arc::new(FaultSink::new(mem_with(&objects)));
            if earlier {
                sink.inner.set(&keys.probe(), b"an earlier probe");
            }
            sink.set(&keys.probe(), Some(fault));
            let fs = MemFs::new();
            let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
            let ctx = format!("earlier {earlier}, {fault:?}: {r:?}");
            assert_eq!(refused(r), Some(Refused::NoConditionalWrites), "{ctx}");
            assert!(claims(&sink.inner.objects(), ID).is_empty(), "{ctx}");
            assert!(fs.paths().is_empty(), "{ctx}");
        }
    }
}

#[test]
fn a_bucket_without_checksum_checks_is_refused_before_the_claim_and_a_drill_does_not_probe() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    let keys = Keys::new(ID);
    for hashes in [Hashes::Trusted, Hashes::StoreRejected, Hashes::Swallowed] {
        let sink = Arc::new(FaultSink::new(mem_with(&objects)));
        sink.set_hashes(hashes);
        let fs = MemFs::new();
        let r = block_on(restorer(sink.clone(), &fs, PlanMode::Full, KEY).prepare(1));
        assert_eq!(refused(r), Some(Refused::NoChecksums), "{hashes:?}");
        if matches!(hashes, Hashes::Trusted) {
            let probes: Vec<Vec<u8>> = sink
                .inner
                .objects()
                .into_iter()
                .filter(|(k, _)| matches!(keys.parse(k), Some(ObjectName::ChecksumProbe(_))))
                .map(|(_, v)| v)
                .collect();
            assert_eq!(probes, [b"kvik checksum probe".to_vec()]);
        }
        assert!(claims(&sink.inner.objects(), ID).is_empty());
        assert!(fs.paths().is_empty());
        let before = sink.log().len();
        restore(&mut restorer(
            sink.clone(),
            &MemFs::new(),
            PlanMode::Drill,
            KEY,
        ))
        .unwrap();
        assert!(sink.log()[before..].iter().all(|(op, _)| *op != "put"));
    }
}

#[test]
fn a_bucket_that_passed_keeps_its_probe_unchanged_and_passes_again() {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(mem_with(&bucket(&mut src)));
    let keys = Keys::new(ID);
    restore(&mut restorer(
        sink.clone(),
        &MemFs::new(),
        PlanMode::Full,
        KEY,
    ))
    .unwrap();
    let kept = sink.objects()[&keys.probe()].clone();
    sink.remove(&keys.claim(1));
    sink.remove(&keys.record(1, 1));
    let (p, _) = restore(&mut restorer(
        sink.clone(),
        &MemFs::new(),
        PlanMode::Full,
        KEY,
    ))
    .unwrap();
    assert_eq!(p.record.generation, 1);
    assert_eq!(sink.objects()[&keys.probe()], kept);
    let prefix = format!("{}/", keys.probe());
    assert!(!sink.objects().keys().any(|k| k.starts_with(&prefix)));
}

#[test]
fn a_sink_fault_during_the_probe_is_a_sink_error_not_a_refusal() {
    let mut src = source(ID, 2, false);
    let objects = bucket(&mut src);
    for fault in [Fault::Delay(1), Fault::RejectPut(1)] {
        let sink = Arc::new(FaultSink::new(mem_with(&objects)));
        sink.set(&Keys::new(ID).probe(), Some(fault));
        let r = block_on(restorer(sink.clone(), &MemFs::new(), PlanMode::Full, KEY).prepare(1));
        assert!(matches!(r, Err(RestoreError::Sink(_))), "{fault:?}: {r:?}");
    }
}

#[test]
fn q108_a_rebuilt_manifest_not_yet_whole_in_the_bucket_is_not_read() {
    let mut src = source(ID, 3, false);
    let sink = MemSink::new();
    upload(&sink, &mut src);
    use_copies(&src);
    let (keys, env) = (Keys::new(ID), TestEnvelope::new(KEY));
    let mut carried = Record::new(7, 0, 12, Body::Sealed(vec![src.segments[0].0.clone()]));
    carried.old_epoch = Some(2);
    for r in [
        Record::new(5, 0, 12, Body::Rebuild(Rebuild::default())),
        Record {
            old_epoch: Some(1),
            ..Record::new(6, 0, 12, Body::Empty)
        },
        carried,
    ] {
        let k = keys.record(0, r.epoch);
        let sealed = env.seal(&k, &r.encode().unwrap()).unwrap();
        block_on(sink.put(&k, sealed.clone().into(), sha256(&sealed))).unwrap();
    }
    let fs = MemFs::new();
    let (p, o) = restore(&mut restorer(sink, &fs, PlanMode::Drill, KEY)).unwrap();
    let listed: Vec<_> = p
        .record
        .files
        .iter()
        .map(|f| f.description.clone())
        .collect();
    let all: Vec<_> = src.segments.iter().map(|(d, _)| d.clone()).collect();
    assert_eq!(listed, all);
    assert_eq!(o.installed.len(), 3);
}
