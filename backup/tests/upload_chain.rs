//! The predecessor each uploaded manifest record names (`format.md` A.17 Uploaded forms, Q-610), as the upload
//! driver writes it: across restarts, and at a rebuilt manifest whether the driver restarts or outlives it.

mod upload_support;

use std::time::Duration;

use kvik_backup::fake::block_on;
use kvik_backup::keys::Keys;
use kvik_backup::plan::check_chain;
use kvik_backup::upload::BackupDriver;
use kvik_files::record::Body;
use upload_support::*;

/// (previous, epoch) of each record in the bucket.
fn links(h: &Harness) -> Vec<(u64, u64)> {
    in_bucket(&h.sink.mem)
        .iter()
        .map(|u| (u.previous, u.record.epoch))
        .collect()
}

/// What a restore's check makes of the bucket: each record names the one listed before it (Q-611).
fn assert_chained(h: &Harness) {
    assert_eq!(check_chain(&in_bucket(&h.sink.mem)), Ok(()));
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
    assert_chained(&h);
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
    assert_chained(&h);
    h.check_done();
}

/// The rebuild record names none; the records before it and after it each name the one before them.
fn assert_rebuilt_chain(h: &Harness) {
    let all = in_bucket(&h.sink.mem);
    let r = all
        .iter()
        .position(|u| matches!(u.record.body, Body::Rebuild(_)) && u.record.old_epoch.is_none())
        .expect("a rebuild record uploaded");
    assert_eq!(all[r].previous, 0);
    assert_chained(h);
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

#[test]
fn a_driver_that_outlives_a_rebuild_with_old_records_pending_uploads_a_chain_a_restore_accepts() {
    let mut h = sealed(3);
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    let before: Vec<u64> = links(&h).iter().map(|l| l.1).collect();
    h.seal(20);
    h.seal(20);
    assert!(matches!(
        block_on(d.step(Duration::ZERO)),
        kvik_backup::upload::Step::Progress
    ));
    let old_last = h.records().last().unwrap().epoch;
    *lock(&h.app()) = rebuild_final(&h);
    run_idle(&mut d, &mut Duration::default());
    assert_rebuilt_chain(&h);
    assert!(
        links(&h)
            .iter()
            .any(|&(_, e)| e <= old_last && !before.contains(&e)),
        "an old record pending at the rebuild went up after it"
    );
    h.check_done();
}

#[test]
fn a_restart_on_a_rebuilt_manifest_leaves_the_old_tail_unuploaded_and_the_chain_whole() {
    let mut h = sealed(3);
    run_idle(&mut h.driver(), &mut Duration::default());
    h.seal(20);
    h.seal(20);
    let tail: Vec<u64> = h.records().iter().rev().take(2).map(|r| r.epoch).collect();
    h.close();
    h.set_app(rebuild_final(&h));
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    for e in tail {
        assert!(!h.sink.mem.objects().contains_key(&keys.record(0, e)));
    }
    assert_rebuilt_chain(&h);
    h.check_done();
}
