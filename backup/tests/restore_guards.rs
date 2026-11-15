//! Guards of the restore executor: a drill never writes, claim retries, lagging listings and the bucket probe
//! (`spec.md` §8 Generations, Restore, Restore drill).

mod support;

use std::sync::Arc;

use kvik_backup::fake::block_on;
use kvik_backup::keys::{ClaimMarker, Keys};
use kvik_backup::plan::PlanMode;
use kvik_backup::restore::{RestoreDriver, RestoreError};
use kvik_log::fs::MemFs;
use kvik_sink_api::SinkError;
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
