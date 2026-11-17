//! The upload driver over a directory a full restore wrote (Q-610): its uploaded form of the restore record is the one
//! the restore put, so a listing that does not show that record only repeats a `put` of identical bytes, and the
//! records after it chain on; its markers follow the restore's, with none repeated for the restore record (Q-614).

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{DirectPort, TestEnvelope, block_on};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::marker::{Marker, Summary};
use kvik_backup::plan::{PlanMode, check_chain};
use kvik_backup::upload::{BackupDriver, Step, UploadConfig, Uploader};
use kvik_backup::uploaded::UploadedRecord;
use kvik_files::appender::{Appender, Opened};
use kvik_files::record::Body;
use kvik_log::fs::MemFs;
use support::*;

#[test]
fn the_upload_driver_over_a_restored_directory_repeats_the_restore_record_byte_for_byte_and_chains_on()
 {
    let mut src = source(ID, 2, false);
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink.clone(), &fs, PlanMode::Full, KEY)).unwrap();
    let g = p.record.generation;
    let keys = Keys::new(ID);
    let first = keys.record(g, 1);
    let put_by_restore = sink.inner.objects()[&first].clone();
    let puts_before = sink.count("put", &first);
    let Opened::Ready(mut app) = Appender::open(&fs, Path::new(DIR), ID, 0).unwrap() else {
        panic!("a corrupt restored manifest");
    };
    app.append(Body::Empty, 200).unwrap();
    app.append(Body::Empty, 200).unwrap();
    sink.set(&first, Some(Fault::Unlisted));
    let cfg = UploadConfig {
        max_object: 1 << 30,
        retry_first: Duration::from_secs(1),
        retry_max: Duration::from_secs(60),
    };
    let mut d = Uploader::new(
        sink.clone(),
        TestEnvelope::new(KEY),
        keys.clone(),
        local(&fs),
        DirectPort::new(*app, local(&fs), ID, 200),
        cfg,
    );
    let mut now = Duration::ZERO;
    for _ in 0..1_000 {
        match block_on(d.step(now)) {
            Step::Progress => {}
            Step::Idle => break,
            Step::Retry { after, .. } => now += after,
            s => panic!("{s:?}"),
        }
    }
    assert!(
        sink.count("put", &first) > puts_before,
        "the restore record was put again"
    );
    assert_eq!(sink.inner.objects()[&first], put_by_restore);
    let env = TestEnvelope::new(KEY);
    let mut records: Vec<UploadedRecord> = sink
        .inner
        .objects()
        .iter()
        .filter(|(k, _)| matches!(keys.parse(k), Some(ObjectName::Record { generation, .. }) if generation == g))
        .map(|(k, v)| UploadedRecord::decode(&env.open(k, v).unwrap()).unwrap())
        .collect();
    records.sort_by_key(|u| u.record.epoch);
    let links: Vec<(u64, u64)> = records
        .iter()
        .map(|u| (u.previous, u.record.epoch))
        .collect();
    assert_eq!(links, [(0, 1), (1, 2), (2, 3)]);
    assert_eq!(check_chain(&records), Ok(()));
    let markers: Vec<Marker> = sink
        .inner
        .objects()
        .iter()
        .filter(|(k, _)| matches!(keys.parse(k), Some(ObjectName::Marker { generation, .. }) if generation == g))
        .map(|(k, v)| Marker::decode(&env.open(k, v).unwrap()).unwrap())
        .collect();
    let named: Vec<(u64, u64)> = markers
        .iter()
        .map(|m| (m.sequence, m.summary.epoch))
        .collect();
    assert_eq!(named, [(1, 1), (2, 2), (3, 3)]);
    assert_eq!(
        markers[2].summary,
        Summary::of(records.iter().map(|u| &u.record))
    );
}
