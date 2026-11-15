//! The manifest rebuild (`spec.md` §7 Manifest; Q-52 (a), Q-103 (a)–(d), (g), Q-107): one corrupt record in turn
//! and pairs of them against the uncorrupted fold, the scenarios of each part, the provisional rebuild and its
//! holds, the copy written back, and the kill loop over writing the rebuilt manifest.

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use kvik_files::appender::{AcknowledgeError, Appender, Held, Opened, RebuildState};
use kvik_files::catalogue::Catalogue;
use kvik_files::desc::{FileClass, FileRef, kind};
use kvik_files::image::{Image, read};
use kvik_files::rebuild::{
    Answer, CorruptStart, Decision, FLOOR_MARGIN, Finality, LocalState, Location, Place,
    RebuildError, RebuildInput, RebuildWait, plan, rebuild_manifest, repair_with_copy, resolve,
};
use kvik_files::record::{Body, Record, record_type};
use kvik_log::manifest::{LossCause, read_manifest};
use proptest::prelude::*;
use support::*;

const NOW: i64 = T0 + 1_000_000;

/// A history closed by an empty record, so that every record of the history has a successor.
fn closed(mut h: History) -> History {
    let e = h.records.last().unwrap().epoch + 1;
    h.records.push(Record::new(e, 0, T0, Body::Empty));
    h
}

fn image_with_corrupt(h: &History, indices: &[usize]) -> (Vec<u8>, Image) {
    let mut bytes = manifest_bytes(&h.records);
    for &i in indices {
        corrupt_record(&mut bytes, &h.records, i);
    }
    let img = read(&bytes, &STORE_ID).unwrap();
    (bytes, img)
}

fn input<'a>(img: &'a Image, h: &'a History) -> RebuildInput<'a> {
    RebuildInput {
        image: img,
        files: &h.files,
        losses: &h.losses,
        lease_epoch: 0,
        mark_file_id: h.mark_file_id,
        written_at: NOW,
    }
}

fn state(h: &History) -> LocalState<'_> {
    LocalState {
        files: &h.files,
        losses: &h.losses,
        lease_epoch: 0,
        mark_file_id: h.mark_file_id,
    }
}

fn live(c: &Catalogue) -> BTreeSet<FileRef> {
    c.live().map(|e| e.description.file_ref()).collect()
}

fn retired(c: &Catalogue) -> BTreeSet<FileRef> {
    c.files()
        .filter(|e| !e.is_live())
        .map(|e| e.description.file_ref())
        .collect()
}

/// The identities a corrupt record named as the loss's own (first) record or as an acknowledgement.
/// A loss identity.
type Identity = (u32, u64);

fn named_by(r: &Record) -> (Option<Identity>, Vec<Identity>) {
    match &r.body {
        Body::LostRange(l) if !l.extends => (Some(l.identity), Vec::new()),
        Body::Acknowledgement(a) => (None, a.losses.iter().map(|l| l.identity).collect()),
        _ => (None, Vec::new()),
    }
}

proptest! {
    #[test]
    fn one_corrupt_record_loses_no_file_no_loss_and_no_identity(h in history(), i in any::<prop::sample::Index>()) {
        let h = closed(h);
        let i = i.index(h.records.len() - 1);
        let (_, img) = image_with_corrupt(&h, &[i]);
        prop_assert_eq!(img.corrupt.len(), 1);
        let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
        let want = Catalogue::replay(&h.records).unwrap();
        prop_assert_eq!(live(&p.catalogue), live(&want));
        prop_assert_eq!(retired(&p.catalogue), retired(&want));
        prop_assert!(p.discard.is_empty());
        prop_assert_eq!(p.unlocated().count(), 0);
        let rb = p.rebuild_record().unwrap();
        prop_assert_eq!(rb.identities_rebuilt, 0);
        prop_assert_eq!(rb.presumed_epoch, img.corrupt[0].after_epoch + 1);
        // Every loss keeps its identity and range; only the corrupt record's own facts may differ.
        let (own, acked) = named_by(&h.records[i]);
        prop_assert_eq!(p.catalogue.losses().count(), want.losses().count());
        for l in want.losses() {
            let got = p.catalogue.loss(l.record.identity);
            prop_assert!(got.is_some(), "loss {:?} lost", l.record.identity);
            let got = got.unwrap();
            prop_assert_eq!((got.record.first, got.record.last), (l.record.first, l.record.last));
            if own == Some(l.record.identity) {
                // An extension record repeats the cause; else a recovery entry gives it, or it is unknown.
                prop_assert!(got.record.cause == l.record.cause
                    || matches!(got.record.cause, LossCause::Unknown | LossCause::TruncatedTail));
            } else {
                prop_assert_eq!(got.record.cause, l.record.cause);
            }
            if !acked.contains(&l.record.identity) {
                prop_assert_eq!(got.acknowledged, l.acknowledged);
            }
        }
        // Every valid record is carried, in order, with its old epoch.
        let carried: Vec<&Record> = p.records.iter().filter(|r| r.old_epoch.is_some()).collect();
        prop_assert_eq!(carried.len(), h.records.len() - 1);
        for (c, o) in carried.iter().zip(h.records.iter().enumerate().filter(|(k, _)| *k != i).map(|(_, r)| r)) {
            prop_assert_eq!(c.old_epoch, Some(o.epoch));
            prop_assert_eq!(&c.body, &o.body);
        }
        // Epochs rise above every old one.
        let top = h.records.last().unwrap().epoch;
        prop_assert!(p.records[0].epoch > top);
        prop_assert!(p.records.windows(2).all(|w| w[0].epoch < w[1].epoch));
        prop_assert_eq!(p.records.last().unwrap().body.clone(), Body::Empty);
    }

    #[test]
    fn a_rebuilt_manifest_is_written_read_back_by_both_readers_and_appended_to(h in history(), i in any::<prop::sample::Index>()) {
        let h = closed(h);
        let i = i.index(h.records.len() - 1);
        let (bytes, _) = image_with_corrupt(&h, &[i]);
        let fs = fs_with(&bytes);
        prop_assert!(matches!(Appender::open(&fs, &dir(), STORE_ID, 0).unwrap(), Opened::Corrupt(_)));
        let (mut app, p) = rebuild_manifest(&fs, &dir(), STORE_ID, state(&h), &FakeVerifier::default(), Finality::Final, NOW).unwrap();
        prop_assert_eq!(app.catalogue(), &p.catalogue);
        prop_assert!(fs.read(&dir().join("MANIFEST.new")).is_none());
        let on_disk = fs.read(&manifest_path()).unwrap();
        prop_assert_eq!(read_manifest(&on_disk, &STORE_ID).unwrap().records.len(), p.records.len());
        // An append after the rename is visible by path (the handle is on the new file).
        app.append(Body::Empty, NOW).unwrap();
        prop_assert_eq!(read(&fs.read(&manifest_path()).unwrap(), &STORE_ID).unwrap().records.len(), p.records.len() + 1);
        // A rebuild of the rebuilt manifest keeps each carried record's first old epoch.
        drop(app);
        let (_, q) = rebuild_manifest(&fs, &dir(), STORE_ID, state(&h), &FakeVerifier::default(), Finality::Final, NOW).unwrap();
        prop_assert_eq!(live(&q.catalogue), live(&p.catalogue));
        for (a, b) in q.records.iter().skip(1).zip(&p.records) {
            prop_assert_eq!(a.old_epoch, Some(b.old_epoch.unwrap_or(b.epoch)));
        }
        prop_assert_eq!(q.catalogue.rebuilds().count(), 2);
    }

    #[test]
    fn two_corrupt_records_lose_no_file_and_no_position_and_count_what_loses_its_identity(
        h in history(), i in any::<prop::sample::Index>(), j in any::<prop::sample::Index>(),
    ) {
        let h = closed(h);
        let n = h.records.len() - 1;
        let (i, j) = (i.index(n), j.index(n));
        let (_, img) = image_with_corrupt(&h, &[i.min(j), i.max(j)]);
        let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
        let want = Catalogue::replay(&h.records).unwrap();
        prop_assert_eq!(live(&p.catalogue), live(&want));
        let mut lost_identity = 0u32;
        for l in want.losses() {
            for s in [l.record.first, l.record.last] {
                prop_assert!(p.catalogue.loss_holding(s).is_some(), "position {:?} of a loss is not listed", s);
            }
            if p.catalogue.loss(l.record.identity).is_none() {
                lost_identity += 1;
            }
        }
        prop_assert!(lost_identity <= p.rebuild_record().unwrap().identities_rebuilt);
    }

    #[test]
    fn a_kill_while_writing_a_rebuilt_manifest_leaves_the_old_or_the_new_one(
        h in history(), i in any::<prop::sample::Index>(), at in any::<prop::sample::Index>(), seed in any::<u64>(), how in 0u8..3,
    ) {
        let h = closed(h);
        let i = i.index(h.records.len() - 1);
        let (bytes, _) = image_with_corrupt(&h, &[i]);
        let fs = fs_with(&bytes);
        fs.clear_ops();
        let (_, clean) = rebuild_manifest(&fs, &dir(), STORE_ID, state(&h), &FakeVerifier::default(), Finality::Final, NOW).unwrap();
        let total = fs.ops().len();
        let fs = fs_with(&bytes);
        fs.crash_after(at.index(total + 1));
        let _ = rebuild_manifest(&fs, &dir(), STORE_ID, state(&h), &FakeVerifier::default(), Finality::Final, NOW);
        match how {
            0 => fs.restart(),
            1 => fs.power_loss(),
            _ => fs.power_loss_partial(seed),
        }
        prop_assert!(fs.read(&manifest_path()).is_some(), "MANIFEST is never missing");
        let c = match Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() {
            Opened::Ready(a) => a.catalogue().clone(),
            Opened::Corrupt(_) => {
                let (a, _) = rebuild_manifest(&fs, &dir(), STORE_ID, state(&h), &FakeVerifier::default(), Finality::Final, NOW).unwrap();
                a.catalogue().clone()
            }
        };
        prop_assert!(fs.read(&dir().join("MANIFEST.new")).is_none());
        prop_assert_eq!(c, clean.catalogue);
    }
}

#[test]
fn a_kill_at_every_operation_of_a_rebuild_with_power_loss() {
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: Some(1),
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: true },
        Step::Seal {
            len: 5,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
        Step::Acknowledge(0),
    ]));
    for i in 0..h.records.len() - 1 {
        let (bytes, _) = image_with_corrupt(&h, &[i]);
        let fs = fs_with(&bytes);
        fs.clear_ops();
        let (_, clean) = rebuild_manifest(
            &fs,
            &dir(),
            STORE_ID,
            state(&h),
            &FakeVerifier::default(),
            Finality::Final,
            NOW,
        )
        .unwrap();
        let total = fs.ops().len();
        for n in 0..=total {
            for seed in 0..4u64 {
                let fs = fs_with(&bytes);
                fs.crash_after(n);
                let _ = rebuild_manifest(
                    &fs,
                    &dir(),
                    STORE_ID,
                    state(&h),
                    &FakeVerifier::default(),
                    Finality::Final,
                    NOW,
                );
                if seed == 0 {
                    fs.power_loss();
                } else {
                    fs.power_loss_partial(seed);
                }
                let c = match Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() {
                    Opened::Ready(a) => a.catalogue().clone(),
                    Opened::Corrupt(_) => rebuild_manifest(
                        &fs,
                        &dir(),
                        STORE_ID,
                        state(&h),
                        &FakeVerifier::default(),
                        Finality::Final,
                        NOW,
                    )
                    .unwrap()
                    .0
                    .catalogue()
                    .clone(),
                };
                assert_eq!(
                    c, clean.catalogue,
                    "record {i}, crash after {n}, seed {seed}"
                );
            }
        }
    }
}

// ---- Scenarios ----

/// Segments 1 and 2, compacted into 1 (current) and 2 (revision); segment 3 compacted into 3; 1 and 3 merged
/// into 4 (retiring them); then an empty record.
fn store() -> History {
    closed(play(&[
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
        Step::Compact { two: true },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
    ]))
}

fn index_of(h: &History, pred: impl Fn(&Body) -> bool) -> usize {
    h.records.iter().position(|r| pred(&r.body)).unwrap()
}

fn merge_index(h: &History) -> usize {
    index_of(
        h,
        |b| matches!(b, Body::Publish(p) if !p.retired.is_empty()),
    )
}

#[test]
fn q103_a_a_corrupt_merge_whose_output_does_not_verify_is_discarded_and_its_inputs_stay_live() {
    let h = store();
    let (_, img) = image_with_corrupt(&h, &[merge_index(&h)]);
    let v = FakeVerifier {
        bad: [comp_ref(4)].into(),
    };
    let p = plan(&input(&img, &h), &v).unwrap();
    assert_eq!(p.discard, vec![comp_ref(4)]);
    let l = live(&p.catalogue);
    assert!(l.contains(&comp_ref(1)) && l.contains(&comp_ref(3)) && !l.contains(&comp_ref(4)));
    // With the output verified, it is live and its inputs retired, as before the damage.
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let l = live(&p.catalogue);
    assert!(l.contains(&comp_ref(4)) && !l.contains(&comp_ref(1)) && !l.contains(&comp_ref(3)));
    let restated = p
        .records
        .iter()
        .find_map(|r| match &r.body {
            Body::Publish(x) if r.old_epoch.is_none() => Some(x.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(restated.published[0].file_ref(), comp_ref(4));
    assert_eq!(
        restated
            .retired
            .iter()
            .map(|d| d.file_ref())
            .collect::<Vec<_>>(),
        vec![comp_ref(1), comp_ref(3)]
    );
}

#[test]
fn q103_a_an_output_whose_inputs_are_not_local_is_live_from_its_named_inputs_alone() {
    // A corrupt merge whose input has left the local tiers: an input leaves only once its replacement is verified
    // in another tier, so the output is live and its inputs retired without a local verification.
    let mut h = store();
    h.files.retain(|f| f.description.file_ref() != comp_ref(1));
    let (_, img) = image_with_corrupt(&h, &[merge_index(&h)]);
    let v = FakeVerifier {
        bad: [comp_ref(4)].into(),
    };
    let p = plan(&input(&img, &h), &v).unwrap();
    let l = live(&p.catalogue);
    assert!(l.contains(&comp_ref(4)) && !l.contains(&comp_ref(1)) && !l.contains(&comp_ref(3)));
    assert!(p.discard.is_empty());
    assert_eq!(
        live(&p.catalogue),
        live(&Catalogue::replay(&h.records).unwrap())
    );
}

#[test]
fn a_loss_in_a_segment_that_fails_its_own_checks_or_with_a_retired_file_gone_is_found_again() {
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: Some(2),
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
    ]));
    let own = index_of(&h, |b| matches!(b, Body::LostRange(_)));
    for variant in 0..3 {
        let mut h = h.clone();
        match variant {
            // The segment holding the loss fails its own hash.
            0 => h.files[0].verified = false,
            // A retired compacted file over the loss is removed from disk.
            1 => h.files.retain(|f| f.description.file_ref() != comp_ref(1)),
            // The segment is quarantined.
            _ => h.files[0].place = Place::Quarantine,
        }
        let (_, img) = image_with_corrupt(&h, &[own]);
        let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
        let l = p
            .catalogue
            .loss((0, 2))
            .unwrap_or_else(|| panic!("variant {variant}: the loss is lost"));
        assert_eq!((l.record.first, l.record.last), ((0, 2), (0, 3)));
    }
    // A live segment on no local path (moved to deep) holds its range: no loss is made of it.
    let mut h = h.clone();
    h.files.retain(|f| f.description.file_ref() != seg_ref(2));
    h.losses.push(kvik_files::rebuild::FoundLoss {
        first: (0, 7),
        last: (0, 12),
        ..h.losses[0].clone()
    });
    let (_, img) = image_with_corrupt(&h, &[own]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert!(p.catalogue.loss((0, 7)).is_none());
    assert_eq!(p.locations[&seg_ref(2)], Location::Unlocated);
}

#[test]
fn a_rebuild_takes_epochs_above_every_lease() {
    let h = store();
    let (bytes, _) = image_with_corrupt(&h, &[1]);
    let fs = fs_with(&bytes);
    let st = LocalState {
        lease_epoch: 500,
        ..state(&h)
    };
    let (mut app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        st,
        &FakeVerifier::default(),
        Finality::Final,
        NOW,
    )
    .unwrap();
    assert_eq!(p.rebuild_epoch, 501);
    assert_eq!(
        app.append(Body::Empty, NOW).unwrap(),
        p.records.last().unwrap().epoch + 1
    );
}

#[test]
fn a_rebuilt_manifest_carrying_a_restore_record_opens_again() {
    let mut h = store();
    for r in &mut h.records {
        r.epoch += 1;
        r.generation = 1;
    }
    h.records.insert(
        0,
        Record::new(
            1,
            1,
            T0,
            Body::Other {
                record_type: record_type::RESTORE,
                skippable: false,
                fields: vec![(1, vec![0; 12])],
            },
        ),
    );
    let (bytes, _) = image_with_corrupt(&h, &[3]);
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
    assert_eq!(app.catalogue(), &p.catalogue);
}

#[test]
fn q103_a_a_file_a_later_record_retires_was_published_by_the_corrupt_record_with_its_inputs() {
    // Compacted 1 and 2 merged into 3 by the record that is corrupt; a later valid merge retires 3.
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
    let mut without = h.clone();
    // Output 3 is not local any more, so only the later record's description of it is left.
    without
        .files
        .retain(|f| f.description.file_ref() != comp_ref(3));
    let (_, img) = image_with_corrupt(&without, &[first_merge]);
    let p = plan(&input(&img, &without), &FakeVerifier::default()).unwrap();
    let want = Catalogue::replay(&h.records).unwrap();
    assert_eq!(live(&p.catalogue), live(&want));
    assert!(!live(&p.catalogue).contains(&comp_ref(1)));
}

#[test]
fn q103_a_an_unlisted_output_of_a_crashed_compaction_is_live_when_it_verifies() {
    // A compaction wrote output 9 from segment 4 and crashed before its record (§9 row 10); then a record is
    // corrupt, so the manifest is rebuilt and the output is judged from the files alone.
    let mut h = store();
    h.files.push(local(&segment(4, 19, 24), Place::Hot));
    let out = compacted(9, FileClass::Current, (19, 24), &[seg_ref(4)]);
    h.files.push(local(&out, Place::Hot));
    let (_, img) = image_with_corrupt(&h, &[1]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let l = live(&p.catalogue);
    assert!(l.contains(&comp_ref(9)) && l.contains(&seg_ref(4)));
}

#[test]
fn q103_a_quarantined_files_are_never_live_and_files_no_path_holds_are_unlocated() {
    let mut h = store();
    for f in &mut h.files {
        if f.description.file_ref() == comp_ref(2) {
            f.place = Place::Quarantine;
        }
        if f.description.file_ref() == seg_ref(1) {
            f.place = Place::Cold;
        }
    }
    h.files.retain(|f| f.description.file_ref() != seg_ref(2));
    let (_, img) = image_with_corrupt(&h, &[0]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert_eq!(p.quarantined, vec![comp_ref(2)]);
    assert_eq!(p.locations[&seg_ref(1)], Location::Cold);
    assert_eq!(p.locations[&seg_ref(2)], Location::Unlocated);
    assert!(!p.locations.contains_key(&comp_ref(2)));
    // A file that fails its own checks is not a valid local file.
    let mut h = store();
    h.files[0].verified = false;
    let (_, img) = image_with_corrupt(&h, &[0]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert_eq!(p.invalid, vec![seg_ref(1)]);
}

#[test]
fn q103_b_a_restore_record_is_never_rebuilt() {
    let mut h = store();
    for r in &mut h.records[3..] {
        r.generation = 1;
    }
    let (_, img) = image_with_corrupt(&h, &[2]);
    assert!(matches!(
        plan(&input(&img, &h), &FakeVerifier::default()),
        Err(RebuildError::RestoreRecord(1))
    ));
    // With the generation's restore record valid (carried as a field list), another corrupt record is rebuilt.
    let mut h = store();
    let at = h.records[2].epoch;
    for r in &mut h.records[2..] {
        r.epoch += 1;
        r.generation = 1;
    }
    h.records.insert(
        2,
        Record::new(
            at,
            1,
            T0,
            Body::Other {
                record_type: record_type::RESTORE,
                skippable: false,
                fields: vec![(1, vec![0; 12])],
            },
        ),
    );
    let (_, img) = image_with_corrupt(&h, &[4]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert!(
        p.records
            .iter()
            .any(|r| r.body.record_type() == record_type::RESTORE && r.old_epoch == Some(at))
    );
}

#[test]
fn q103_c_identities_come_from_valid_records_causes_from_records_or_recovery_entries_else_unknown()
{
    // Loss A (identity 2) acknowledged; loss B (identity 8) with a recovery entry's cause.
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: Some(2),
            cause_given: false,
        },
        Step::Acknowledge(0),
        Step::Seal {
            len: 6,
            lost: Some(1),
            cause_given: true,
        },
    ]));
    let own = |id: u64| {
        index_of(
            &h,
            |b| matches!(b, Body::LostRange(l) if l.identity == (0, id)),
        )
    };
    // Corrupt A's own record: the acknowledgement still names its identity; the cause is unknown.
    let (_, img) = image_with_corrupt(&h, &[own(2)]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let a = p.catalogue.loss((0, 2)).unwrap();
    assert_eq!(a.record.cause, LossCause::Unknown);
    assert!(
        a.acknowledged,
        "the valid acknowledgement still covers the loss"
    );
    assert_eq!(p.rebuild_record().unwrap().losses_restated, 1);
    // Corrupt B's own record: the recovery entry gives the cause.
    let (_, img) = image_with_corrupt(&h, &[own(8)]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert_eq!(
        p.catalogue.loss((0, 8)).unwrap().record.cause,
        LossCause::TruncatedTail
    );
    assert!(!p.catalogue.loss((0, 8)).unwrap().acknowledged);
    // Corrupt the acknowledgement: the loss is unacknowledged again, to be acknowledged again.
    let ack = index_of(&h, |b| matches!(b, Body::Acknowledgement(_)));
    let (_, img) = image_with_corrupt(&h, &[ack]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert!(!p.catalogue.loss((0, 2)).unwrap().acknowledged);
    assert_eq!(p.rebuild_record().unwrap().losses_restated, 0);
}

#[test]
fn q103_c_a_loss_grown_downward_whose_two_records_are_corrupt_is_listed_under_its_first_missing_position_and_counted()
 {
    let mut h = closed(play(&[
        Step::Seal {
            len: 8,
            lost: Some(1),
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Empty,
    ]));
    // The loss (identity 2) grew downward to 1 by an extension record; the scan finds 1–2.
    // The empty record the steps end with, before the closing one.
    let ext_at = h.records.len() - 2;
    let e = h.records[ext_at].epoch;
    h.records[ext_at] = Record::new(
        e,
        0,
        T0,
        Body::LostRange(lost(2, 1, 2, LossCause::Corruption, true)),
    );
    h.losses[0].first = (0, 1);
    let own = index_of(&h, |b| matches!(b, Body::LostRange(l) if !l.extends));
    let ext = index_of(&h, |b| matches!(b, Body::LostRange(l) if l.extends));
    assert!(
        own + 1 < ext,
        "two corrupt records with a valid one between"
    );
    let (_, img) = image_with_corrupt(&h, &[own, ext]);
    assert_eq!(img.corrupt.len(), 2);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert!(p.catalogue.loss((0, 2)).is_none());
    let l = p.catalogue.loss((0, 1)).unwrap();
    assert_eq!(
        (l.record.first, l.record.last, l.acknowledged),
        ((0, 1), (0, 2), false)
    );
    assert_eq!(p.rebuild_record().unwrap().identities_rebuilt, 1);
    // With only the extension corrupt, the identity is kept from the valid first record.
    let (_, img) = image_with_corrupt(&h, &[ext]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let l = p.catalogue.loss((0, 2)).unwrap();
    assert_eq!((l.record.first, l.record.last), ((0, 1), (0, 2)));
    assert_eq!(p.rebuild_record().unwrap().identities_rebuilt, 0);
}

#[test]
fn q103_g_file_id_floors_are_far_above_every_id_seen() {
    let mut h = store();
    h.mark_file_id = 77;
    let (_, img) = image_with_corrupt(&h, &[1]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let rb = p.rebuild_record().unwrap();
    assert_eq!(
        rb.floors,
        vec![
            (kind::LOG_SEGMENT, 77 + FLOOR_MARGIN),
            (kind::COMPACTED, 4 + FLOOR_MARGIN)
        ]
    );
    assert_eq!(p.catalogue.id_floor(kind::COMPACTED), 4 + FLOOR_MARGIN);
    assert_eq!(rb.files_found, h.files.len() as u64);
}

#[test]
fn a_rebuild_takes_no_file_as_verified_in_another_tier() {
    let h = store();
    let mut c = Catalogue::replay(&h.records).unwrap();
    c.mark_verified(seg_ref(1), kvik_files::catalogue::Tier::Deep);
    assert!(!c.file(seg_ref(1)).unwrap().verified.is_empty());
    let (_, img) = image_with_corrupt(&h, &[1]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert!(p.catalogue.files().all(|e| e.verified.is_empty()));
    let mut c2 = p.catalogue.clone();
    c2.mark_verified(seg_ref(1), kvik_files::catalogue::Tier::Deep);
    c2.apply(&p.records[0].clone()).unwrap_err();
}

// ---- Before the rebuild: the copy sources and the wait (Q-107) ----

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

fn lost_record_history() -> (History, usize) {
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
        Step::Compact { two: false },
    ]));
    let i = index_of(&h, |b| matches!(b, Body::LostRange(_)));
    (h, i)
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
    let span = img.corrupt[0];
    assert_eq!(
        resolve(&mut app, &span, &Answer::Unreachable).unwrap(),
        RebuildState::Provisional {
            epoch: p.rebuild_epoch
        }
    );
    // Deep answers that it holds none: final, acknowledged once.
    assert_eq!(
        resolve(&mut app, &span, &Answer::HoldsNone).unwrap(),
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
    let span = img.corrupt[0];
    // A copy that is not the corrupt record is refused.
    let other = h.records[i + 1].encode().unwrap();
    assert!(resolve(&mut app, &span, &Answer::Holds(other)).is_err());
    let s = resolve(&mut app, &span, &Answer::Holds(copy)).unwrap();
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
    let s = resolve(&mut app, &img.corrupt[0], &Answer::Holds(copy)).unwrap();
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
