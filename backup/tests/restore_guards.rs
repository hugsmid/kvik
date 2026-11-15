//! Guards of the restore executor: a drill never writes, claim retries, lagging listings and the bucket probe
//! (`spec.md` §8 Generations, Restore, Restore drill).

mod support;

use std::sync::Arc;

use kvik_backup::fake::block_on;
use kvik_backup::keys::Keys;
use kvik_backup::plan::PlanMode;
use kvik_backup::restore::{RestoreDriver, RestoreError};
use kvik_log::fs::MemFs;
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
