//! The manifest rebuild, scenario by scenario: each part of Q-103 that roadmap 6.3a covers ((a)–(c), (g)), losses
//! the scan finds, leases and reserved records.

mod support;

use kvik_files::catalogue::Catalogue;
use kvik_files::desc::{FileClass, kind};
use kvik_files::rebuild::{
    FLOOR_MARGIN, Finality, LocalState, Location, Place, RebuildError, plan, rebuild_manifest,
};
use kvik_files::record::{Body, Record, record_type};
use kvik_files::restore::RestoreRecord;
use kvik_log::manifest::LossCause;
use kvik_log::mark::ResetStamp;
use support::rebuild::*;
use support::*;

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

fn restore_body() -> Body {
    RestoreRecord {
        generation: 1,
        stamp: ResetStamp {
            sequence: 1,
            nonce: 1,
        },
        source_generation: 0,
        restored_end: (0, 0),
        carried_ends: Vec::new(),
        losses: Vec::new(),
        acknowledgements: Vec::new(),
        entry_losses: Vec::new(),
        files: Vec::new(),
        drill: false,
    }
    .to_body()
}

#[test]
fn a_rebuilt_manifest_carrying_a_restore_record_opens_again() {
    let mut h = store();
    for r in &mut h.records {
        r.epoch += 1;
        r.generation = 1;
    }
    h.records.insert(0, Record::new(1, 1, T0, restore_body()));
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
    // With the generation's restore record valid, another corrupt record is rebuilt.
    let mut h = store();
    let at = h.records[2].epoch;
    for r in &mut h.records[2..] {
        r.epoch += 1;
        r.generation = 1;
    }
    h.records.insert(2, Record::new(at, 1, T0, restore_body()));
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

#[test]
fn q103_a_retired_files_removed_locally_stay_retired_and_a_merge_over_them_is_restated() {
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
        Step::RemoveRetired,
    ]));
    assert!(
        h.files
            .iter()
            .all(|f| f.description.file_ref() != comp_ref(1))
    );
    let want = Catalogue::replay(&h.records).unwrap();
    for i in 0..h.records.len() - 1 {
        let (_, img) = image_with_corrupt(&h, &[i]);
        let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
        assert_eq!(live(&p.catalogue), live(&want), "record {i}");
        assert_eq!(retired(&p.catalogue), retired(&want), "record {i}");
        assert_eq!(p.unlocated().count(), 0, "record {i}");
    }
}

#[test]
fn q103_a_an_unlisted_output_of_log_segments_is_verified_before_it_is_published() {
    let mut h = store();
    h.files.push(local(&segment(4, 19, 24), Place::Hot));
    let out = compacted(9, FileClass::Current, (19, 24), &[seg_ref(4)]);
    h.files.push(local(&out, Place::Hot));
    let (_, img) = image_with_corrupt(&h, &[1]);
    let v = FakeVerifier {
        bad: [comp_ref(9)].into(),
    };
    let p = plan(&input(&img, &h), &v).unwrap();
    assert_eq!(p.discard, vec![comp_ref(9)]);
    assert!(!live(&p.catalogue).contains(&comp_ref(9)));
    // Its segment not local: live from its named inputs alone.
    h.files.retain(|f| f.description.file_ref() != seg_ref(4));
    let (_, img) = image_with_corrupt(&h, &[1]);
    let p = plan(&input(&img, &h), &v).unwrap();
    assert!(live(&p.catalogue).contains(&comp_ref(9)));
}

#[test]
fn q175_an_output_whose_unnamed_input_is_gone_stays_live_and_is_never_discarded() {
    // Merges A (compacted 1 and 2 into 3) and B (3 and 4 into 5) both corrupt, no later merge; 3 removed locally.
    let h = closed(play(&[
        Step::Seal {
            len: 4,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Seal {
            len: 4,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
        Step::Seal {
            len: 4,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
    ]));
    let merges: Vec<usize> = h
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(&r.body, Body::Publish(p) if !p.retired.is_empty()))
        .map(|(k, _)| k)
        .collect();
    for removed in [
        vec![comp_ref(3)],
        vec![comp_ref(1), comp_ref(2), comp_ref(3)],
    ] {
        let mut h = h.clone();
        h.files
            .retain(|f| !removed.contains(&f.description.file_ref()));
        let (_, img) = image_with_corrupt(&h, &merges);
        // A verifier that would fail it, were it asked: it is not, as an input is not at hand.
        let v = FakeVerifier {
            bad: [comp_ref(5)].into(),
        };
        let p = plan(&input(&img, &h), &v).unwrap();
        assert!(p.discard.is_empty(), "{removed:?}");
        assert_eq!(p.undescribed, vec![comp_ref(3)]);
        let l = live(&p.catalogue);
        assert!(
            l.contains(&comp_ref(5)) && !l.contains(&comp_ref(4)),
            "{removed:?}"
        );
        // 1 and 2 cannot be followed through 3: they stay as the valid records leave them.
        assert!(l.contains(&comp_ref(1)) && l.contains(&comp_ref(2)));
    }
}
