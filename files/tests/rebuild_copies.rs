//! Before and after a rebuild (Q-107, Q-103 (d), Q-171): the wait, the provisional rebuild and its holds across a
//! restart, copies of the corrupt record from the mirror path or deep, and the acknowledgement.

mod support;

use std::time::Duration;

use kvik_files::appender::{AcknowledgeError, Appender, Gap, Held, Opened, RebuildState};
use kvik_files::catalogue::Catalogue;
use kvik_files::image::{Image, read};
use kvik_files::rebuild::{
    Answer, CorruptStart, Decision, Finality, RebuildWait, rebuild_manifest, repair_with_copy,
    resolve,
};
use kvik_files::record::Body;
use kvik_log::fs::{FaultOp, MemFs};
use kvik_log::manifest::LossCause;
use support::rebuild::*;
use support::*;

#[test]
fn q107_the_wait_is_counted_from_the_first_failed_ask_of_each_start() {
    let mins = |m: u64| Duration::from_secs(60 * m);
    let mut s = CorruptStart::new(RebuildWait::default());
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Unreachable, mins(3)),
        Decision::Wait(Some(mins(18)))
    );
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Unreachable, mins(17)),
        Decision::Wait(Some(mins(18)))
    );
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Unreachable, mins(18)),
        Decision::RebuildProvisional
    );
    // A restart starts the wait again.
    let mut s = CorruptStart::new(RebuildWait::default());
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Unreachable, mins(40)),
        Decision::Wait(Some(mins(55)))
    );
    // Deep answers during the wait: the start leaves the wait by itself.
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::HoldsNone, mins(41)),
        Decision::RebuildFinal
    );
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Holds(vec![1]), mins(41)),
        Decision::Repair(vec![1])
    );
    // The mirror path's copy wins without deep; `never` waits for deep.
    let mut s = CorruptStart::new(RebuildWait::Never);
    assert_eq!(
        s.decide(&Answer::Holds(vec![2]), &Answer::Unreachable, mins(0)),
        Decision::Repair(vec![2])
    );
    assert_eq!(
        s.decide(&Answer::HoldsNone, &Answer::Unreachable, mins(9_999)),
        Decision::Wait(None)
    );
}

#[test]
fn q103_d_a_provisional_rebuild_holds_everything_irreversible_and_cannot_be_acknowledged() {
    let (h, i) = lost_record_history();
    let (bytes, img) = image_with_corrupt(&h, &[i]);
    let fs = fs_with(&bytes);
    let (mut app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    assert_eq!(
        app.rebuild_state(),
        RebuildState::Provisional {
            epoch: p.rebuild_epoch
        }
    );
    assert!(app.holds());
    let e = app.append(Body::Empty, NOW).unwrap();
    app.append(Body::Empty, NOW).unwrap();
    assert_eq!(app.irreversible(e), Err(Held::Provisional));
    assert!(matches!(
        app.acknowledge_rebuild(p.rebuild_epoch, NOW),
        Err(AcknowledgeError::NotFinal)
    ));
    // Deep still unreachable: nothing changes.
    let g = gap(&img.corrupt[0]);
    assert_eq!(app.pending(), &[g]);
    assert_eq!(
        resolve(&mut app, g, &Answer::HoldsNone, &Answer::Unreachable).unwrap(),
        RebuildState::Provisional {
            epoch: p.rebuild_epoch
        }
    );
    // Deep answers that it holds none: final, acknowledged once.
    assert_eq!(
        resolve(&mut app, g, &Answer::HoldsNone, &Answer::HoldsNone).unwrap(),
        RebuildState::Final {
            epoch: p.rebuild_epoch
        }
    );
    assert!(app.irreversible(e).is_ok());
    assert!(app.acknowledge_rebuild(p.rebuild_epoch, NOW).unwrap());
    assert!(!app.acknowledge_rebuild(p.rebuild_epoch, NOW).unwrap());
    assert!(
        app.catalogue()
            .rebuild(p.rebuild_epoch)
            .unwrap()
            .acknowledged
    );
    // A later start reads the acknowledgement.
    drop(app);
    let Opened::Ready(app) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!()
    };
    assert!(
        app.catalogue()
            .rebuild(p.rebuild_epoch)
            .unwrap()
            .acknowledged
    );
}

#[test]
fn q103_d_deeps_copy_replaces_what_was_reconstructed_by_appended_records() {
    let (h, i) = lost_record_history();
    let (bytes, img) = image_with_corrupt(&h, &[i]);
    let fs = fs_with(&bytes);
    let (mut app, _) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    let identity = match &h.records[i].body {
        Body::LostRange(l) => l.identity,
        _ => unreachable!(),
    };
    assert_eq!(
        app.catalogue().loss(identity).unwrap().record.cause,
        LossCause::Unknown
    );
    let copy = h.records[i].encode().unwrap();
    let g = gap(&img.corrupt[0]);
    // A copy that is not the corrupt record is refused.
    let other = h.records[i + 1].encode().unwrap();
    assert!(resolve(&mut app, g, &Answer::HoldsNone, &Answer::Holds(other)).is_err());
    let s = resolve(&mut app, g, &Answer::Unreachable, &Answer::Holds(copy)).unwrap();
    assert!(matches!(s, RebuildState::Final { .. }));
    let l = app.catalogue().loss(identity).unwrap();
    assert_eq!(l.record.cause, LossCause::Corruption);
    assert_eq!(*l.epochs.last().unwrap(), app.catalogue().last_epoch());
    let back = read(&fs.read(&manifest_path()).unwrap(), &STORE_ID).unwrap();
    assert_eq!(
        back.records.last().unwrap().record.old_epoch,
        Some(h.records[i].epoch)
    );
}

#[test]
fn q103_d_a_copy_of_a_merge_a_later_record_retired_the_output_of_is_taken() {
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
    ]));
    let first_merge = merge_index(&h);
    let (bytes, img) = image_with_corrupt(&h, &[first_merge]);
    let fs = fs_with(&bytes);
    let (mut app, _) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    let copy = h.records[first_merge].encode().unwrap();
    let s = resolve(
        &mut app,
        gap(&img.corrupt[0]),
        &Answer::HoldsNone,
        &Answer::Holds(copy),
    )
    .unwrap();
    assert!(matches!(s, RebuildState::Final { .. }));
    assert_eq!(
        live(app.catalogue()),
        live(&Catalogue::replay(&h.records).unwrap())
    );
    let last = read(&fs.read(&manifest_path()).unwrap(), &STORE_ID).unwrap();
    assert_eq!(
        last.records.last().unwrap().record.old_epoch,
        Some(h.records[first_merge].epoch)
    );
}

#[test]
fn a_copy_of_the_corrupt_record_is_written_in_place_or_the_manifest_written_again() {
    let (h, i) = lost_record_history();
    let (bytes, img) = image_with_corrupt(&h, &[i]);
    // The same bytes: in place.
    let fs = fs_with(&bytes);
    fs.clear_ops();
    repair_with_copy(&fs, &dir(), &img, &bytes, &h.records[i].encode().unwrap()).unwrap();
    assert!(
        fs.ops()
            .iter()
            .all(|o| o.op != kvik_log::fs::FaultOp::Rename)
    );
    assert_eq!(
        fs.read(&manifest_path()).unwrap(),
        manifest_bytes(&h.records)
    );
    // A longer copy (it carries an ignorable field): the manifest is written again with it in place.
    let mut longer = h.records[i].clone();
    longer
        .ignorable
        .push((0x8001, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]));
    let fs = fs_with(&bytes);
    repair_with_copy(&fs, &dir(), &img, &bytes, &longer.encode().unwrap()).unwrap();
    let Opened::Ready(app) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!()
    };
    let mut want = h.records.clone();
    want[i] = longer;
    assert_eq!(app.catalogue(), &Catalogue::replay(&want).unwrap());
    // A record of another epoch is not the copy.
    let fs = fs_with(&bytes);
    assert!(repair_with_copy(&fs, &dir(), &img, &bytes, &h.records[0].encode().unwrap()).is_err());
}

#[test]
fn the_operator_call_rebuilds_a_readable_manifest_too() {
    let h = store();
    let fs = fs_with(&manifest_bytes(&h.records));
    let (app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Final,
        NOW,
    )
    .unwrap();
    let rb = p.rebuild_record().unwrap();
    assert_eq!(
        (rb.presumed_epoch, rb.last_valid_before, rb.losses_restated),
        (0, 0, 0)
    );
    assert_eq!(
        live(app.catalogue()),
        live(&Catalogue::replay(&h.records).unwrap())
    );
}

#[test]
fn a_rebuilt_acknowledgement_names_the_rebuild_by_its_first_epoch_across_a_second_rebuild() {
    let (h, i) = lost_record_history();
    let (bytes, _) = image_with_corrupt(&h, &[i]);
    let fs = fs_with(&bytes);
    let (mut app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Final,
        NOW,
    )
    .unwrap();
    app.acknowledge_rebuild(p.rebuild_epoch, NOW).unwrap();
    app.append(Body::Empty, NOW).unwrap();
    drop(app);
    let (app, q) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Final,
        NOW,
    )
    .unwrap();
    assert!(
        app.catalogue()
            .rebuild(p.rebuild_epoch)
            .unwrap()
            .acknowledged
    );
    assert!(
        !app.catalogue()
            .rebuild(q.rebuild_epoch)
            .unwrap()
            .acknowledged
    );
}

fn provisional(h: &History, indices: &[usize]) -> (MemFs, Image, Appender, u64) {
    let (bytes, img) = image_with_corrupt(h, indices);
    let fs = fs_with(&bytes);
    let (app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    (fs, img, app, p.rebuild_epoch)
}

fn reopen(fs: &MemFs) -> Appender {
    match Appender::open(fs, &dir(), STORE_ID, 0).unwrap() {
        Opened::Ready(a) => *a,
        Opened::Corrupt(_) => panic!("corrupt"),
    }
}

#[test]
fn q171_a_rebuild_no_acknowledgement_names_stays_provisional_across_a_restart() {
    let (h, i) = lost_record_history();
    let (fs, img, app, e) = provisional(&h, &[i]);
    drop(app);
    let mut app = reopen(&fs);
    assert_eq!(app.rebuild_state(), RebuildState::Provisional { epoch: e });
    assert!(app.holds());
    let last = app.catalogue().last_epoch();
    app.append(Body::Empty, NOW).unwrap();
    assert_eq!(app.irreversible(last), Err(Held::Provisional));
    assert!(matches!(
        app.acknowledge_rebuild(e, NOW),
        Err(AcknowledgeError::NotFinal)
    ));
    // After a restart the corrupt record is known from the rebuild record's fields 1 and 2.
    let rb = app.catalogue().rebuild(e).unwrap().record.clone();
    let g = Gap {
        after: rb.last_valid_before,
        before: rb.presumed_epoch + 1,
    };
    assert_eq!(app.pending(), &[g]);
    assert_eq!(g.after, img.corrupt[0].after_epoch);
    // The mirror path cannot be reached: still provisional, although deep holds none.
    assert!(matches!(
        resolve(&mut app, g, &Answer::Unreachable, &Answer::HoldsNone).unwrap(),
        RebuildState::Provisional { .. }
    ));
    assert_eq!(
        resolve(&mut app, g, &Answer::HoldsNone, &Answer::HoldsNone).unwrap(),
        RebuildState::Final { epoch: e }
    );
    assert_eq!(app.irreversible(last).unwrap().epoch(), last);
    assert!(app.acknowledge_rebuild(e, NOW).unwrap());
    drop(app);
    // Acknowledged: a later start holds nothing.
    let app = reopen(&fs);
    assert_eq!(app.rebuild_state(), RebuildState::None);
    assert!(!app.holds());
}

#[test]
fn q171_a_final_rebuild_not_yet_acknowledged_holds_again_after_a_restart() {
    let (h, i) = lost_record_history();
    let (bytes, _) = image_with_corrupt(&h, &[i]);
    let fs = fs_with(&bytes);
    let (app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Final,
        NOW,
    )
    .unwrap();
    assert!(!app.holds());
    drop(app);
    let mut app = reopen(&fs);
    assert_eq!(
        app.rebuild_state(),
        RebuildState::Provisional {
            epoch: p.rebuild_epoch
        }
    );
    let g = app.pending()[0];
    resolve(&mut app, g, &Answer::HoldsNone, &Answer::HoldsNone).unwrap();
    assert!(!app.holds());
}

#[test]
fn q171_a_rebuild_of_two_corrupt_records_is_final_only_once_both_have_their_answers() {
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: Some(2),
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
    ]));
    let (_fs, img, mut app, e) = provisional(&h, &[2, 4]);
    assert_eq!(img.corrupt.len(), 2);
    let (g0, g1) = (gap(&img.corrupt[0]), gap(&img.corrupt[1]));
    assert_eq!(app.pending(), &[g0, g1]);
    resolve(&mut app, g0, &Answer::HoldsNone, &Answer::HoldsNone).unwrap();
    assert!(app.holds());
    let copy = h.records[4].encode().unwrap();
    // A copy for one gap is not taken for the other.
    assert!(
        resolve(
            &mut app,
            g0,
            &Answer::HoldsNone,
            &Answer::Holds(copy.clone())
        )
        .is_ok()
    );
    assert!(
        resolve(
            &mut app,
            g1,
            &Answer::HoldsNone,
            &Answer::Holds(h.records[2].encode().unwrap())
        )
        .is_err()
    );
    assert_eq!(
        resolve(&mut app, g1, &Answer::Holds(copy), &Answer::Unreachable).unwrap(),
        RebuildState::Final { epoch: e }
    );
}

#[test]
fn a_rebuild_acknowledgement_is_refused_at_append_while_provisional() {
    let (h, i) = lost_record_history();
    let (fs, _, mut app, e) = provisional(&h, &[i]);
    let len = fs.read(&manifest_path()).unwrap().len();
    let ack = Body::Acknowledgement(kvik_files::record::Acknowledgement {
        losses: Vec::new(),
        rebuild: Some(e),
    });
    assert!(matches!(
        app.append(ack, NOW),
        Err(kvik_files::appender::AppendError::NotFinal)
    ));
    assert_eq!(fs.read(&manifest_path()).unwrap().len(), len);
    assert!(!app.is_faulted());
}

#[test]
fn a_copy_shorter_than_its_corrupt_span_is_written_in_place_and_the_rest_stays_corrupt() {
    // Two adjacent corrupt records read as one span; a copy of the first fills only its start.
    let (h, i) = lost_record_history();
    let (bytes, img) = image_with_corrupt(&h, &[i, i + 1]);
    assert_eq!(img.corrupt.len(), 1);
    let fs = fs_with(&bytes);
    fs.clear_ops();
    let copy = h.records[i].encode().unwrap();
    repair_with_copy(&fs, &dir(), &img, &bytes, &copy).unwrap();
    assert!(fs.ops().iter().all(|o| o.op != FaultOp::Rename));
    let Opened::Corrupt(again) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!("the second record is still corrupt")
    };
    assert_eq!(again.records.len(), h.records.len() - 1);
    assert_eq!(again.records[i].record, h.records[i]);
    assert_eq!(again.corrupt.len(), 1);
    assert_eq!(again.corrupt[0].after_epoch, h.records[i].epoch);
}

#[test]
fn a_kill_while_writing_a_copy_back_leaves_the_corrupt_record_or_its_copy() {
    let (h, i) = lost_record_history();
    let (bytes, img) = image_with_corrupt(&h, &[i]);
    let mut longer = h.records[i].clone();
    longer.ignorable.push((0x8001, vec![7; 9]));
    for copy in [h.records[i].encode().unwrap(), longer.encode().unwrap()] {
        let fs = fs_with(&bytes);
        fs.clear_ops();
        repair_with_copy(&fs, &dir(), &img, &bytes, &copy).unwrap();
        let total = fs.ops().len();
        for n in 0..=total {
            for seed in 0..4u64 {
                let fs = fs_with(&bytes);
                fs.crash_after(n);
                let _ = repair_with_copy(&fs, &dir(), &img, &bytes, &copy);
                match seed {
                    0 => fs.restart(),
                    1 => fs.power_loss(),
                    _ => fs.power_loss_partial(seed),
                }
                match Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() {
                    Opened::Ready(a) => {
                        assert_eq!(a.catalogue().losses().count(), 1, "n {n} seed {seed}")
                    }
                    Opened::Corrupt(c) => assert_eq!(c.records.len(), h.records.len() - 1),
                }
                assert!(fs.read(&dir().join("MANIFEST.new")).is_none());
            }
        }
    }
}

#[test]
fn a_kill_while_appending_deeps_copy_keeps_the_holds_until_it_is_taken() {
    let (h, i) = lost_record_history();
    let copy = h.records[i].encode().unwrap();
    let (fs, img, mut app, _) = provisional(&h, &[i]);
    let g = gap(&img.corrupt[0]);
    let before = fs.read(&manifest_path()).unwrap();
    fs.clear_ops();
    resolve(
        &mut app,
        g,
        &Answer::HoldsNone,
        &Answer::Holds(copy.clone()),
    )
    .unwrap();
    let total = fs.ops().len();
    drop(app);
    for n in 0..=total {
        for seed in 0..4u64 {
            let fs = fs_with(&before);
            let mut app = reopen(&fs);
            let g = app.pending()[0];
            fs.crash_after(n);
            let _ = resolve(
                &mut app,
                g,
                &Answer::HoldsNone,
                &Answer::Holds(copy.clone()),
            );
            drop(app);
            match seed {
                0 => fs.restart(),
                1 => fs.power_loss(),
                _ => fs.power_loss_partial(seed),
            }
            // The rebuild is unacknowledged, so the next start holds again, with or without the copy appended.
            let mut app = reopen(&fs);
            assert!(app.holds(), "n {n} seed {seed}");
            let g = app.pending()[0];
            resolve(
                &mut app,
                g,
                &Answer::HoldsNone,
                &Answer::Holds(copy.clone()),
            )
            .unwrap();
            assert!(!app.holds());
            let identity = match &h.records[i].body {
                Body::LostRange(l) => l.identity,
                _ => unreachable!(),
            };
            assert_eq!(
                app.catalogue().loss(identity).unwrap().record.cause,
                LossCause::Corruption
            );
        }
    }
}
