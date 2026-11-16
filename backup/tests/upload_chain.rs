//! The predecessor each uploaded manifest record names (`format.md` A.17 Uploaded forms, Q-610), as the upload
//! driver writes it: across restarts, and at a rebuilt manifest whether the driver restarts or outlives it.

mod upload_support;

use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::TestEnvelope;
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::uploaded::UploadedRecord;
use kvik_files::record::Body;
use upload_support::*;

/// Every record object in the bucket, opened and decoded, in epoch order.
fn in_bucket(h: &Harness) -> Vec<UploadedRecord> {
    let (keys, env) = (Keys::new(ID), TestEnvelope::new(KEY));
    let mut out: Vec<UploadedRecord> = h
        .sink
        .mem
        .objects()
        .iter()
        .filter(|(k, _)| matches!(keys.parse(k), Some(ObjectName::Record { .. })))
        .map(|(k, v)| UploadedRecord::decode(&env.open(k, v).unwrap()).unwrap())
        .collect();
    out.sort_by_key(|u| u.record.epoch);
    out
}

/// (previous, epoch) of each record in the bucket.
fn links(h: &Harness) -> Vec<(u64, u64)> {
    in_bucket(h)
        .iter()
        .map(|u| (u.previous, u.record.epoch))
        .collect()
}

/// Each record names the one before it, the first none.
fn assert_chained(links: &[(u64, u64)]) {
    let mut before = 0;
    for &(previous, epoch) in links {
        assert_eq!(previous, before, "record {epoch} in {links:?}");
        before = epoch;
    }
}

fn sealed(n: usize) -> Harness {
    let mut h = Harness::new();
    for _ in 0..n {
        h.seal(20);
    }
    h
}

#[test]
fn consecutive_uploaded_records_name_each_other_and_the_first_names_none() {
    let h = sealed(3);
    run_idle(&mut h.driver(), &mut Duration::default());
    let links = links(&h);
    assert_eq!(links.len(), h.records().len() - 1);
    assert_eq!(links[0], (0, 1));
    assert_chained(&links);
    h.check_done();
}

#[test]
fn records_uploaded_after_a_restart_name_the_record_before_them_in_the_manifest() {
    let mut h = sealed(2);
    run_idle(&mut h.driver(), &mut Duration::default());
    let first_run = links(&h).len();
    h.seal(20);
    h.reopen();
    h.seal(20);
    run_idle(&mut h.driver(), &mut Duration::default());
    let links = links(&h);
    assert!(links.len() > first_run + 1);
    assert_chained(&links);
    h.check_done();
}

/// The rebuild record names none; the records before it and after it each name the one before them.
fn assert_rebuilt_chain(h: &Harness) {
    let all = in_bucket(h);
    let r = all
        .iter()
        .position(|u| matches!(u.record.body, Body::Rebuild(_)) && u.record.old_epoch.is_none())
        .expect("a rebuild record uploaded");
    assert_eq!(all[r].previous, 0);
    let link = |us: &[UploadedRecord]| -> Vec<(u64, u64)> {
        us.iter().map(|u| (u.previous, u.record.epoch)).collect()
    };
    assert_chained(&link(&all[..r]));
    assert_chained(&link(&all[r..]));
    assert!(all.len() > r + 2, "the rebuilt manifest went up");
}

#[test]
fn after_a_restart_on_a_rebuilt_manifest_its_rebuild_record_names_none() {
    let h = sealed(3);
    run_idle(&mut h.driver(), &mut Duration::default());
    h.close();
    h.set_app(rebuild_final(&h));
    run_idle(&mut h.driver(), &mut Duration::default());
    assert_rebuilt_chain(&h);
    h.check_done();
}

#[test]
fn a_driver_that_outlives_a_rebuild_names_none_at_its_rebuild_record() {
    let h = sealed(3);
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    *lock(&h.app()) = rebuild_final(&h);
    run_idle(&mut d, &mut Duration::default());
    assert_rebuilt_chain(&h);
    h.check_done();
}
