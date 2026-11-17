//! The manifest rebuild (`spec.md` §7 Manifest; Q-52 (a), Q-103): one corrupt record in turn and pairs of them
//! against the uncorrupted fold, and the kill loop over writing the rebuilt manifest.

mod support;

use kvik_files::appender::{Appender, Opened};
use kvik_files::catalogue::Catalogue;
use kvik_files::image::read;
use kvik_files::rebuild::{Finality, plan, rebuild_manifest};
use kvik_files::record::{Body, Record};
use std::sync::Arc;

use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::manifest::{LossCause, read_manifest};
use kvik_log::store::{NewStore, StoreDir};
use proptest::prelude::*;
use support::rebuild::*;
use support::*;

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
        // Counted only where the span's epochs leave room for more than one record (Q-174).
        let s = img.corrupt[0];
        if s.before_epoch - s.after_epoch <= 2 {
            prop_assert_eq!(rb.identities_rebuilt, 0);
        }
        prop_assert!(p.undescribed.is_empty());
        prop_assert_eq!(rb.presumed_epoch, s.after_epoch + 1);
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
        h in history(), i in any::<prop::sample::Index>(), k in any::<prop::sample::Index>(),
    ) {
        // Two empty records at the end, so that two distinct records with successors can be picked.
        let h = closed(closed(h));
        let n = h.records.len() - 1;
        let i = i.index(n);
        let j = (i + 1 + k.index(n - 1)) % n;
        let (_, img) = image_with_corrupt(&h, &[i.min(j), i.max(j)]);
        let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
        let want = Catalogue::replay(&h.records).unwrap();
        // Unless an intermediate output is described nowhere (Q-175), files are as before the damage.
        // A retired file that both records describing it lost, and that is gone from disk, is simply not known.
        if p.undescribed.is_empty() {
            prop_assert_eq!(live(&p.catalogue), live(&want));
            let was = retired(&want);
            prop_assert!(retired(&p.catalogue).is_subset(&was));
            for f in &was {
                prop_assert!(p.catalogue.file(*f).is_none_or(|e| !e.is_live()));
                prop_assert!(p.catalogue.file(*f).is_some() || !h.files.iter().any(|l| l.description.file_ref() == *f));
            }
        }
        prop_assert!(p.discard.is_empty());
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

#[test]
fn two_corrupt_merges_retire_every_input_down_the_chain() {
    // Merge A (compacted 1 and 2 into 3) and merge B (3 and 4 into 5) are both corrupt; a later valid merge retires
    // 5. 3 is named only as 5's input, so its own inputs 1 and 2 are found through its local description.
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
    assert_eq!(merges.len(), 3);
    let (_, img) = image_with_corrupt(&h, &[merges[0], merges[1]]);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    let want = Catalogue::replay(&h.records).unwrap();
    assert_eq!(live(&p.catalogue), live(&want));
    assert_eq!(retired(&p.catalogue), retired(&want));
    for f in [comp_ref(1), comp_ref(2), comp_ref(3)] {
        assert!(
            !p.catalogue.file(f).unwrap().is_live(),
            "{f:?} is live beside its replacement"
        );
    }
    // With 3 removed locally as well, nothing describes it: its inputs cannot be followed (Q-175).
    let mut gone = h.clone();
    gone.files
        .retain(|f| f.description.file_ref() != comp_ref(3));
    let (_, img) = image_with_corrupt(&gone, &[merges[0], merges[1]]);
    let p = plan(&input(&img, &gone), &FakeVerifier::default()).unwrap();
    assert_eq!(p.undescribed, vec![comp_ref(3)]);
}

/// Q-871, format A.12 Open: a rebuild stopped between renaming `MANIFEST.new` over `MANIFEST` and its directory sync,
/// then a restart on the same boot. Opening the store syncs its directory before anything is appended, so a later
/// power loss never brings the corrupt manifest back in place of the rebuilt one and the records appended to it.
#[test]
fn opening_the_store_makes_the_rename_of_a_rebuild_stopped_after_it_durable() {
    let (h, i) = lost_record_history();
    let (bytes, _) = image_with_corrupt(&h, &[i]);
    let store = || {
        let fs = MemFs::new();
        let afs: Arc<dyn Fs> = Arc::new(fs.clone());
        let new = NewStore {
            store_id: STORE_ID,
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        drop(StoreDir::open(afs.clone(), &dir(), Some(&new)).unwrap());
        let f = fs.open(&manifest_path(), OpenMode::ReadWrite).unwrap();
        f.set_len(0).unwrap();
        f.write_at(&bytes, 0).unwrap();
        f.sync_all().unwrap();
        (fs, afs)
    };
    let rebuild = |fs: &MemFs| {
        rebuild_manifest(
            fs,
            &dir(),
            STORE_ID,
            state(&h),
            &FakeVerifier::default(),
            Finality::Final,
            NOW,
        )
    };
    let ops_to_rename = {
        let (fs, _) = store();
        fs.clear_ops();
        rebuild(&fs).unwrap();
        fs.ops()
            .iter()
            .position(|o| o.op == kvik_log::fs::FaultOp::Rename)
            .unwrap()
            + 1
    };
    for seed in 0..32 {
        let (fs, afs) = store();
        fs.crash_after(ops_to_rename);
        assert!(rebuild(&fs).is_err());
        fs.restart();
        let store = StoreDir::open(afs, &dir(), None).unwrap();
        let Opened::Ready(mut app) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
            panic!("the rebuilt manifest reads")
        };
        let appended = app.append(Body::Empty, NOW).unwrap();
        drop((app, store));
        fs.power_loss_partial(seed);
        fs.restart();
        let img = read(&fs.read(&manifest_path()).unwrap(), &STORE_ID).unwrap();
        assert!(!img.is_corrupt(), "seed {seed}");
        assert_eq!(img.last_epoch(), appended, "seed {seed}");
    }
}
