//! The upload driver (roadmap 8.1c) against `files::Appender` on `MemFs` and an in-memory bucket: every sealed file
//! uploaded and verified once, kills at every step, faults at every sink call, restarts, the order of the
//! copy-verified record against the manifest records, the provisional hold and a rebuilt manifest (Q-280).

mod upload_support;

use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{TestEnvelope, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::upload::{BackupDriver, Step, UploadError, Uploader};
use kvik_files::catalogue::Tier;
use kvik_files::rebuild::{
    Answer, Finality, LocalFile, LocalState, Place, Verifier, rebuild_manifest, resolve,
};
use kvik_files::record::Body;
use kvik_files::tier::CopyVerified;
use kvik_log::fs::Fs;
use kvik_sink_api::{SinkError, VerifyError};
use upload_support::*;

const MIN: Duration = Duration::from_secs(60);

fn three() -> Harness {
    let mut h = Harness::new();
    for len in [10, 70_000, 3] {
        h.seal(len);
    }
    h
}

#[test]
fn every_sealed_file_is_uploaded_and_verified_exactly_once() {
    let h = three();
    let mut d = h.driver();
    let mut now = Duration::ZERO;
    run_idle(&mut d, &mut now);
    h.check_done();
    let keys = Keys::new(ID);
    for (desc, _) in &h.segments {
        assert_eq!(h.puts_of(&keys.file(desc)), 1);
    }
    let p = d.progress();
    assert_eq!((p.queued_files, p.uploaded_files), (0, 3));
    assert_eq!(p.backed_up_through, Some((0, 30)));
    assert_eq!(p.failing_since, None);
    // Idle stays idle, and settles the last record no further.
    let before = h.records().len();
    assert!(matches!(block_on(d.step(now)), Step::Idle));
    assert_eq!(h.records().len(), before);
}

#[test]
fn store_goes_up_before_any_other_object() {
    let h = three();
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    let first = h.events().into_iter().find_map(|e| match e {
        Event::Put(k) => Some(k),
        Event::Append(..) => None,
    });
    assert_eq!(first, Some(Keys::new(ID).store()));
}

#[test]
fn the_copy_verified_record_comes_after_its_upload_and_before_the_records_that_name_the_file() {
    let mut h = Harness::new();
    let f = h.seal(100);
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    let keys = Keys::new(ID);
    let ev = h.events();
    let put = |k: &str| ev.iter().position(|e| *e == Event::Put(k.into())).unwrap();
    let copy = ev
        .iter()
        .position(|e| matches!(e, Event::Append(_, b) if CopyVerified::from_body(b).is_some()))
        .unwrap();
    let Event::Append(copy_epoch, _) = ev[copy] else {
        unreachable!()
    };
    // Epochs: 1 the initial record, 2 the sealed-segment record, 3 the copy, 4 the empty record settling it.
    assert_eq!(copy_epoch, 3);
    assert!(put(&keys.file(&f)) < copy);
    assert!(copy < put(&keys.record(0, 2)));
    assert!(put(&keys.record(0, 2)) < put(&keys.record(0, 3)));
    assert_eq!(ev.last(), Some(&Event::Put(keys.record(0, 3))));
    assert!(ev.contains(&Event::Append(4, Body::Empty)));
    assert_eq!(h.puts_of(&keys.record(0, 4)), 0);
    h.check_done();
}

#[test]
fn the_last_record_goes_up_only_after_a_successor_is_synced() {
    let mut h = Harness::new();
    h.seal(5);
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    let n = h.records().len();
    let keys = Keys::new(ID);
    assert_eq!(h.puts_of(&keys.record(0, n as u64)), 0);
    h.seal(5);
    run_idle(&mut d, &mut Duration::default());
    assert_eq!(h.puts_of(&keys.record(0, n as u64)), 1);
    h.check_done();
}

#[test]
fn a_kill_at_every_sink_call_resumes_without_a_duplicate_or_a_lost_file() {
    let total = {
        let h = three();
        run_idle(&mut h.driver(), &mut Duration::default());
        h.ops()
    };
    for k in 0..total {
        let h = three();
        h.kill_sink_from(Some(k));
        let mut d = h.driver();
        let mut now = Duration::ZERO;
        let s = run_to_fault(&mut d, now);
        assert!(matches!(s, Step::Retry { .. }), "{k}: {s:?}");
        now += MIN;
        drop(d);
        h.heal();
        h.fs.restart();
        h.reopen();
        let mut d = h.driver();
        run_idle(&mut d, &mut now);
        h.check_done();
        for (desc, _) in &h.segments {
            assert!(h.puts_of(&Keys::new(ID).file(desc)) <= 2, "{k}");
        }
    }
}

#[test]
fn a_lost_reply_at_every_sink_call_repeats_identical_bytes_only() {
    let total = {
        let h = three();
        run_idle(&mut h.driver(), &mut Duration::default());
        h.ops()
    };
    for k in 0..total {
        let h = three();
        h.fault_at(k, Fault::LoseReply);
        let mut d = h.driver();
        run_idle(&mut d, &mut Duration::default());
        h.check_done();
    }
}

#[test]
fn a_crash_at_every_manifest_write_resumes_without_a_duplicate_or_a_lost_file() {
    for power_loss in [false, true] {
        for n in 0..40 {
            let h = three();
            h.fs.crash_after(n);
            let mut d = h.driver();
            let mut now = Duration::ZERO;
            let s = run(&mut d, &mut now, 10_000);
            let crashed = h.fs.crashed();
            drop(d);
            h.fs.restart();
            if power_loss {
                h.fs.power_loss();
            }
            h.reopen();
            let mut d = h.driver();
            run_idle(&mut d, &mut now);
            h.check_done();
            if !crashed {
                assert!(matches!(s, Step::Idle));
                break;
            }
        }
    }
}

#[test]
fn a_driver_dropped_between_any_two_steps_resumes() {
    for k in 0..30 {
        let h = three();
        let mut d = h.driver();
        let mut now = Duration::ZERO;
        let mut idle = false;
        for _ in 0..k {
            if matches!(block_on(d.step(now)), Step::Idle) {
                idle = true;
                break;
            }
        }
        drop(d);
        h.reopen();
        run_idle(&mut h.driver(), &mut now);
        h.check_done();
        if idle {
            break;
        }
    }
}

#[test]
fn a_restart_mid_queue_repeats_at_most_one_put_per_file() {
    let mut h = Harness::new();
    for _ in 0..5 {
        h.seal(50);
    }
    let mut d = h.driver();
    let mut now = Duration::ZERO;
    for _ in 0..4 {
        assert!(matches!(block_on(d.step(now)), Step::Progress));
    }
    drop(d);
    h.reopen();
    let mut d = h.driver();
    run_idle(&mut d, &mut now);
    h.check_done();
    let keys = Keys::new(ID);
    let repeated: usize = h
        .segments
        .iter()
        .map(|(desc, _)| h.puts_of(&keys.file(desc)) - 1)
        .sum();
    assert_eq!(repeated, 0);
}

#[test]
fn a_bucket_failing_each_call_in_turn_is_retried_with_backoff() {
    let total = {
        let h = three();
        run_idle(&mut h.driver(), &mut Duration::default());
        h.ops()
    };
    let errors = [
        SinkError::Transient { reason: "x".into() },
        SinkError::Busy { queued: 4 },
    ];
    for e in errors {
        for k in 0..total {
            let h = three();
            h.fault_at(k, Fault::Fail(e.clone()));
            let mut d = h.driver();
            let mut now = Duration::ZERO;
            let mut retries = 0;
            loop {
                match block_on(d.step(now)) {
                    Step::Progress => {}
                    Step::Idle => break,
                    Step::Retry { after, error } => {
                        assert_eq!(error, e);
                        assert_eq!(after, cfg().retry_first);
                        assert_eq!(d.progress().failing_since, Some(now));
                        retries += 1;
                        now += after;
                    }
                    s => panic!("{k}: {s:?}"),
                }
            }
            assert_eq!(retries, 1, "{k}");
            assert_eq!(d.progress().retries, 1);
            assert_eq!(d.progress().failing_since, None);
            h.check_done();
        }
    }
}

#[test]
fn retries_back_off_to_the_maximum_and_progress_resets_them() {
    let h = three();
    h.kill_sink_from(Some(0));
    let mut d = h.driver();
    let mut afters = Vec::new();
    for i in 0..12 {
        match block_on(d.step(MIN * i)) {
            Step::Retry { after, .. } => afters.push(after.as_secs()),
            s => panic!("{s:?}"),
        }
    }
    assert_eq!(afters, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]);
    let p = d.progress();
    assert_eq!((p.retries, p.failing_since), (12, Some(Duration::ZERO)));
    assert!(p.last_error.unwrap().contains("killed"));
    h.heal();
    assert!(matches!(block_on(d.step(MIN * 20)), Step::Progress));
    assert_eq!(d.progress().failing_since, None);
    h.kill_sink_from(Some(h.ops()));
    match block_on(d.step(MIN * 21)) {
        Step::Retry { after, .. } => assert_eq!(after, cfg().retry_first),
        s => panic!("{s:?}"),
    }
}

#[test]
fn other_content_under_a_file_key_fails_and_is_never_overwritten() {
    let mut h = Harness::new();
    let f = h.seal(20);
    let key = Keys::new(ID).file(&f);
    h.sink.mem.set(&key, b"someone else's");
    let mut d = h.driver();
    let s = run(&mut d, &mut Duration::default(), 100);
    assert!(
        matches!(
            &s,
            Step::Failed(UploadError::Verify(VerifyError::Sink(
                SinkError::Exists { .. }
            )))
        ),
        "{s:?}"
    );
    assert_eq!(h.sink.mem.objects()[&key], b"someone else's");
    assert!(d.progress().last_error.is_some());
}

#[test]
fn a_local_file_without_its_recorded_hash_fails_by_name() {
    let mut h = Harness::new();
    let f = h.seal(20);
    let path = h
        .dir
        .join("log")
        .join(format!("00000000-{:016x}.kvl", f.file_id));
    h.fs.poke(&path, 3, &[0xff]);
    let mut d = h.driver();
    match run(&mut d, &mut Duration::default(), 100) {
        Step::Failed(UploadError::LocalHash { file }) => assert_eq!(file, f.file_ref()),
        s => panic!("{s:?}"),
    }
    assert_eq!(h.puts_of(&Keys::new(ID).file(&f)), 0);
}

#[test]
fn an_object_above_the_limit_fails_before_any_put() {
    let mut h = Harness::new();
    h.seal(5_000);
    let mut c = cfg();
    c.max_object = 4_000;
    let mut d = Uploader::new(
        h.sink.clone(),
        TestEnvelope::new(KEY),
        Keys::new(ID),
        h.local(),
        h.port(),
        c,
    );
    let s = run(&mut d, &mut Duration::default(), 100);
    assert!(matches!(s, Step::Failed(UploadError::Envelope(_))), "{s:?}");
}

#[test]
fn q284_an_object_not_reported_after_its_put_is_looked_up_until_lost_then_uploaded_again() {
    let mut h = Harness::new();
    let f = h.seal(20);
    let key = Keys::new(ID).file(&f);
    h.stat_fault(&key, Some(StatFault::Absent(1_000)));
    let mut d = h.driver();
    let (mut now, mut put_at, mut reput_at, mut lookups) = (Duration::ZERO, None, None, 0);
    loop {
        let before = h.puts_of(&key);
        let s = block_on(d.step(now));
        let after = h.puts_of(&key);
        match (before, after) {
            (0, 1) => put_at = Some(now),
            (1, 2) => {
                reput_at = Some(now);
                h.stat_fault(&key, None);
            }
            (1, 1) if reput_at.is_none() => lookups += 1,
            _ => {}
        }
        match s {
            Step::Retry { after, .. } => now += after,
            Step::Progress => {}
            Step::Idle => break,
            s => panic!("{s:?}"),
        }
    }
    let (put_at, reput_at) = (put_at.unwrap(), reput_at.unwrap());
    assert!(reput_at - put_at >= 10 * MIN);
    assert!(lookups >= 3);
    assert_eq!(h.puts_of(&key), 2);
    h.check_done();
}

#[test]
fn q284_lookups_while_the_bucket_answers_nothing_else_never_count_an_object_lost() {
    let mut h = Harness::new();
    let f = h.seal(20);
    let key = Keys::new(ID).file(&f);
    h.stat_fault(&key, Some(StatFault::Absent(1_000)));
    let mut d = h.driver();
    let mut now = Duration::ZERO;
    while h.puts_of(&key) == 0 {
        if let Step::Retry { after, .. } = block_on(d.step(now)) {
            now += after;
        }
    }
    h.others_fail(true);
    for _ in 0..40 {
        match block_on(d.step(now)) {
            Step::Retry { after, .. } => now += after,
            s => panic!("{s:?}"),
        }
    }
    assert!(now > 60 * MIN);
    assert_eq!(h.puts_of(&key), 1);
}

#[test]
fn other_content_reported_after_a_put_fails_without_another_put() {
    let mut h = Harness::new();
    let f = h.seal(20);
    let key = Keys::new(ID).file(&f);
    h.stat_fault(&key, Some(StatFault::Other));
    let mut d = h.driver();
    let s = run(&mut d, &mut Duration::default(), 100);
    assert!(
        matches!(
            s,
            Step::Failed(UploadError::Verify(VerifyError::Reported { .. }))
        ),
        "{s:?}"
    );
    assert_eq!(h.puts_of(&key), 1);
    assert_eq!(fresh_copies(&h.records(), f.file_ref()), 0);
}

#[test]
fn the_progress_counts_follow_the_queue() {
    let mut h = Harness::new();
    h.seal(100);
    h.seal(200);
    let mut d = h.driver();
    let now = Duration::ZERO;
    while d.progress().uploaded_files == 0 {
        assert!(matches!(block_on(d.step(now)), Step::Progress));
    }
    let p = d.progress();
    assert_eq!((p.queued_files, p.queued_bytes), (1, 200));
    assert_eq!((p.uploaded_files, p.uploaded_bytes), (1, 120));
    assert_eq!(p.backed_up_through, None);
    run_idle(&mut d, &mut Duration::default());
    let p = d.progress();
    assert_eq!(
        (p.queued_files, p.queued_bytes, p.queued_records),
        (0, 0, 1)
    );
    assert_eq!(p.backed_up_through, Some((0, 20)));
}

#[test]
fn files_sealed_before_any_driver_existed_are_queued_from_the_catalogue() {
    let mut h = Harness::new();
    h.seal(10);
    h.seal(10);
    h.reopen();
    h.seal(10);
    run_idle(&mut h.driver(), &mut Duration::default());
    h.check_done();
}

struct AllVerify;

impl Verifier for AllVerify {
    fn verify(&self, _: &LocalFile, _: &[&LocalFile]) -> bool {
        true
    }
}

fn local_files(h: &Harness) -> Vec<LocalFile> {
    h.segments
        .iter()
        .map(|(d, _)| LocalFile {
            description: d.clone(),
            place: Place::Hot,
            verified: true,
        })
        .collect()
}

#[test]
fn q280_a_rebuilt_manifest_verifies_each_copy_again_without_a_new_upload() {
    let h = three();
    run_idle(&mut h.driver(), &mut Duration::default());
    let puts_before = h.events().len();
    h.close();
    let files = local_files(&h);
    let state = LocalState {
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
    };
    let (app, _) =
        rebuild_manifest(&h.fs, &h.dir, ID, state, &AllVerify, Finality::Final, T0).unwrap();
    let snap = app.catalogue().clone();
    for (d, _) in &h.segments {
        let e = snap.file(d.file_ref()).unwrap();
        assert!(e.deep.is_some() && !e.verified.contains(&Tier::Deep));
    }
    h.set_app(app);
    let mut d = h.driver();
    run_idle(&mut d, &mut Duration::default());
    let keys = Keys::new(ID);
    for (desc, _) in &h.segments {
        assert_eq!(h.puts_of(&keys.file(desc)), 1, "no second upload");
    }
    assert!(h.events().len() > puts_before);
    assert_eq!(d.progress().uploaded_files, 0);
    h.check_done();
}

#[test]
fn q280_a_rebuilt_copy_the_bucket_no_longer_reports_is_uploaded_again() {
    let h = three();
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    let gone = keys.file(&h.segments[1].0);
    h.sink.mem.remove(&gone);
    h.close();
    let files = local_files(&h);
    let state = LocalState {
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
    };
    let (app, _) =
        rebuild_manifest(&h.fs, &h.dir, ID, state, &AllVerify, Finality::Final, T0).unwrap();
    h.set_app(app);
    run_idle(&mut h.driver(), &mut Duration::default());
    assert_eq!(h.puts_of(&gone), 2);
    assert_eq!(h.puts_of(&keys.file(&h.segments[0].0)), 1);
    h.check_done();
}

#[test]
fn a_provisional_rebuild_holds_every_upload_until_it_is_final() {
    let mut h = Harness::new();
    for _ in 0..3 {
        h.seal(30);
    }
    h.close();
    // Corrupt the second sealed-segment record (epoch 3).
    let records = h.records();
    let mut at = 64usize;
    for r in &records[..2] {
        at += r.encode().unwrap().len();
    }
    let path = h.dir.join("MANIFEST");
    let mut b = h.fs.read(&path).unwrap();
    b[at + 8] ^= 1;
    h.fs.poke(&path, at + 8, &b[at + 8..at + 9]);
    let files = local_files(&h);
    let state = LocalState {
        files: &files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
    };
    let (app, _) = rebuild_manifest(
        &h.fs,
        &h.dir,
        ID,
        state,
        &AllVerify,
        Finality::Provisional,
        T0,
    )
    .unwrap();
    assert!(app.holds());
    let app = h.set_app(app);
    let mut d = h.driver();
    for i in 0..5 {
        assert!(matches!(block_on(d.step(MIN * i)), Step::Held));
    }
    assert_eq!(h.ops(), 0);
    assert!(h.sink.mem.objects().is_empty());
    {
        let mut a = upload_support::lock(&app);
        let gaps = a.pending().to_vec();
        for g in gaps {
            resolve(&mut a, g, &Answer::HoldsNone, &Answer::HoldsNone).unwrap();
        }
        assert!(!a.holds());
    }
    run_idle(&mut d, &mut Duration::default());
    h.check_done();
}

#[test]
fn the_driver_reads_local_files_through_the_store_directory() {
    let mut h = Harness::new();
    let f = h.seal(10);
    let name = format!("00000000-{:016x}.kvl", f.file_id);
    assert!(
        h.fs.open(&h.dir.join("log").join(name), kvik_log::fs::OpenMode::Read)
            .is_ok()
    );
    let env = TestEnvelope::new(KEY);
    run_idle(&mut h.driver(), &mut Duration::default());
    let key = Keys::new(ID).file(&f);
    let obj = h.sink.mem.objects()[&key].clone();
    assert_eq!(env.open(&key, &obj).unwrap(), h.segments[0].1);
}
