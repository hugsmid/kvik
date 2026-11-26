//! The catalogue against a naive reference, the appender against reading the file back, and the kill loop over
//! appends and the start's rewrite with `MemFs` crashes, power losses and failed syncs (`spec.md` §7 Manifest, §9
//! row 10, invariant 14).

mod support;

use std::io;

use kvik_files::appender::{AcknowledgeError, Appender, Held, Opened};
use kvik_files::catalogue::{Catalogue, CatalogueError};
use kvik_files::desc::FileClass;
use kvik_files::image::read;
use kvik_files::record::{AckedLoss, Acknowledgement, Body, Publish, Record};
use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::manifest::LossCause;
use proptest::prelude::*;
use support::*;

fn open(fs: &MemFs) -> Appender {
    match Appender::open(fs, &dir(), STORE_ID, 0).unwrap() {
        Opened::Ready(a) => *a,
        Opened::Corrupt(_) => panic!("corrupt"),
    }
}

fn initial() -> MemFs {
    fs_with(&manifest_bytes(&[Record::new(1, 0, T0, Body::Empty)]))
}

fn bodies(h: &History) -> Vec<Body> {
    h.records[1..].iter().map(|r| r.body.clone()).collect()
}

fn reread(fs: &MemFs) -> Vec<Record> {
    read(&fs.read(&manifest_path()).unwrap(), &STORE_ID)
        .unwrap()
        .records()
        .cloned()
        .collect()
}

proptest! {
    #[test]
    fn the_catalogue_equals_a_naive_scan_at_every_epoch(h in history()) {
        let c = Catalogue::replay(&h.records).unwrap();
        for r in &h.records {
            let got: std::collections::BTreeSet<_> =
                c.at_epoch(r.epoch).iter().map(|e| e.description.file_ref()).collect();
            prop_assert_eq!(got, naive_listed_at(&h.records, r.epoch));
        }
        let naive = naive_losses(&h.records);
        prop_assert_eq!(c.losses().count(), naive.len());
        for l in c.losses() {
            let (a, b, acked) = naive[&l.record.identity];
            prop_assert_eq!((l.record.first, l.record.last, l.acknowledged), (a, b, acked));
        }
    }

    #[test]
    fn appending_one_by_one_equals_reading_the_file_back(h in history()) {
        let fs = initial();
        let mut m = open(&fs);
        for b in bodies(&h) {
            m.append(b, T0).unwrap();
        }
        let back = reread(&fs);
        prop_assert_eq!(&Catalogue::replay(&back).unwrap(), m.catalogue());
        // A start reads the same and continues above it.
        let again = open(&fs);
        prop_assert_eq!(again.catalogue(), m.catalogue());
        prop_assert_eq!(again.next_epoch(), m.next_epoch());
        // The appended manifest equals the history with its epochs renumbered.
        let want: Vec<&Body> = h.records.iter().map(|r| &r.body).collect();
        let got: Vec<&Body> = back.iter().map(|r| &r.body).collect();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn a_kill_at_any_point_of_appends_loses_no_synced_record_and_finds_no_corrupt_one(
        h in history(), at in any::<prop::sample::Index>(), seed in any::<u64>(), how in 0u8..3,
    ) {
        let bodies = bodies(&h);
        // The operations of a run without faults.
        let fs = initial();
        fs.clear_ops();
        let mut m = open(&fs);
        for b in &bodies {
            m.append(b.clone(), T0).unwrap();
        }
        drop(m);
        let total = fs.ops().len();
        let fs = initial();
        fs.crash_after(at.index(total + 1));
        let mut acked = 0usize;
        if let Ok(Opened::Ready(mut m)) = Appender::open(&fs, &dir(), STORE_ID, 0) {
            for b in &bodies {
                if m.append(b.clone(), T0).is_err() {
                    break;
                }
                acked += 1;
            }
        }
        match how {
            0 => fs.restart(),
            1 => fs.power_loss(),
            _ => fs.power_loss_partial(seed),
        }
        let m = open(&fs);
        let back = reread(&fs);
        // Some prefix, holding every record whose append returned.
        let kept = back.len() - 1;
        prop_assert!(kept >= acked && kept <= bodies.len());
        for (r, b) in back[1..].iter().zip(&bodies) {
            prop_assert_eq!(&r.body, b);
        }
        prop_assert_eq!(m.catalogue(), &Catalogue::replay(&back).unwrap());
        // The repaired file is durable as it stands, and appends continue.
        drop(m);
        fs.power_loss();
        let mut m = open(&fs);
        prop_assert_eq!(reread(&fs).len(), back.len());
        m.append(Body::Empty, T0).unwrap();
        prop_assert_eq!(reread(&fs).len(), back.len() + 1);
    }
}

#[test]
fn a_record_kept_from_the_page_cache_after_a_failed_sync_is_written_again_by_the_next_start() {
    let fs = initial();
    let mut m = open(&fs);
    m.append(Body::Empty, T0).unwrap();
    fs.fail_nth(FaultOp::Sync, "MANIFEST", 0, io::ErrorKind::Other);
    assert!(m.append(Body::Empty, T0).is_err());
    assert!(m.is_faulted());
    assert!(m.append(Body::Empty, T0).is_err());
    drop(m);
    // The record is visible (page cache) but not durable.
    assert_eq!(reread(&fs).len(), 3);
    fs.restart();
    let m = open(&fs);
    assert_eq!(m.catalogue().last_epoch(), 3);
    drop(m);
    // The start wrote it again and synced it: it survives a power loss.
    fs.power_loss();
    assert_eq!(reread(&fs).len(), 3);
    // Without the start's rewrite the failed record would have been lost: the control.
    let fs = initial();
    let mut m = open(&fs);
    fs.fail_nth(FaultOp::Sync, "MANIFEST", 0, io::ErrorKind::Other);
    assert!(m.append(Body::Empty, T0).is_err());
    drop(m);
    fs.power_loss();
    assert_eq!(reread(&fs).len(), 1);
}

#[test]
fn a_torn_tail_is_dropped_and_the_file_truncated() {
    let records = vec![
        Record::new(1, 0, T0, Body::Empty),
        Record::new(2, 0, T0, Body::Empty),
    ];
    let mut bytes = manifest_bytes(&records);
    let third = Record::new(3, 0, T0, Body::Empty).encode().unwrap();
    bytes.extend(&third[..20]);
    let fs = fs_with(&bytes);
    let m = open(&fs);
    assert_eq!(m.dropped_tail(), 20);
    assert_eq!(fs.read(&manifest_path()).unwrap().len(), bytes.len() - 20);
    assert_eq!(m.next_epoch(), 3);
}

#[test]
fn new_epochs_continue_above_leases_and_every_epoch_the_last_record_names() {
    let mut last = Record::new(
        5,
        0,
        T0,
        Body::Acknowledgement(Acknowledgement {
            losses: vec![],
            rebuild: Some(40),
        }),
    );
    last.old_epoch = Some(30);
    let fs = fs_with(&manifest_bytes(&[Record::new(1, 0, T0, Body::Empty), last]));
    let Opened::Ready(m) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!()
    };
    assert_eq!(m.next_epoch(), 41);
    let Opened::Ready(m) = Appender::open(&fs, &dir(), STORE_ID, 90).unwrap() else {
        panic!()
    };
    assert_eq!(m.next_epoch(), 91);
}

#[test]
fn the_last_record_is_not_acted_on_until_a_successor_is_synced() {
    let fs = initial();
    let mut m = open(&fs);
    // At a start, the last valid record has no successor yet.
    assert_eq!(m.irreversible(1), Err(Held::NotSettled));
    let e = m.append(Body::Empty, T0).unwrap();
    assert!(m.settled(1).is_some());
    assert!(m.settled(e).is_none());
    // A crash now with a torn successor: the open drops it, and the record stays unsettled.
    let s = m.settle(e, T0).unwrap();
    assert_eq!(s.epoch(), e);
    assert_eq!(m.catalogue().last_epoch(), e + 1);
    assert!(m.irreversible(e).is_ok());
    // A record already settled appends nothing.
    m.settle(1, T0).unwrap();
    assert_eq!(m.catalogue().last_epoch(), e + 1);
}

#[test]
fn records_that_contradict_the_manifest_are_refused_before_anything_is_written() {
    let fs = initial();
    let mut m = open(&fs);
    let s1 = segment(1, 1, 9);
    let a = compacted(1, FileClass::Current, (1, 9), &[seg_ref(1)]);
    let b = compacted(2, FileClass::Current, (1, 9), &[seg_ref(1)]);
    m.append(Body::Sealed(vec![s1]), T0).unwrap();
    m.append(
        Body::Publish(Publish {
            published: vec![a.clone(), b.clone()],
            retired: vec![],
        }),
        T0,
    )
    .unwrap();
    let len = fs.read(&manifest_path()).unwrap().len();
    // An output of `a` while `a` stays live (invariant 14).
    let c = compacted(3, FileClass::Current, (1, 9), &[comp_ref(1)]);
    let e = m
        .append(
            Body::Publish(Publish {
                published: vec![c.clone()],
                retired: vec![],
            }),
            T0,
        )
        .unwrap_err();
    assert!(matches!(
        e,
        kvik_files::appender::AppendError::Catalogue(CatalogueError::InputStillLive { .. })
    ));
    // A file described otherwise than before.
    let mut a2 = a.clone();
    a2.hash = [9; 32];
    assert!(
        m.append(
            Body::Publish(Publish {
                published: vec![],
                retired: vec![a2],
            }),
            T0
        )
        .is_err()
    );
    assert_eq!(fs.read(&manifest_path()).unwrap().len(), len);
    // The merge, then publishing `a` again, is refused.
    m.append(
        Body::Publish(Publish {
            published: vec![c],
            retired: vec![a.clone()],
        }),
        T0,
    )
    .unwrap();
    assert!(
        m.append(
            Body::Publish(Publish {
                published: vec![a],
                retired: vec![],
            }),
            T0
        )
        .is_err()
    );
    assert!(!m.is_faulted());
}

#[test]
fn acknowledgements_cover_a_range_and_a_growing_loss_needs_a_new_one() {
    let mut c = Catalogue::new();
    let ack = |e, id, first, last| {
        Record::new(
            e,
            0,
            T0,
            Body::Acknowledgement(Acknowledgement {
                losses: vec![AckedLoss {
                    identity: (0, id),
                    first: (0, first),
                    last: (0, last),
                }],
                rebuild: None,
            }),
        )
    };
    // A loss reported but not yet recorded can be acknowledged.
    c.apply(&ack(1, 4, 4, 6)).unwrap();
    c.apply(&Record::new(
        2,
        0,
        T0,
        Body::LostRange(lost(4, 4, 6, LossCause::Corruption, false)),
    ))
    .unwrap();
    assert!(c.loss((0, 4)).unwrap().acknowledged);
    // It grows downward: degraded again until acknowledged with the new range.
    c.apply(&Record::new(
        3,
        0,
        T0,
        Body::LostRange(lost(4, 3, 6, LossCause::Corruption, true)),
    ))
    .unwrap();
    let l = c.loss((0, 4)).unwrap();
    assert_eq!(
        (l.record.first, l.record.last, l.acknowledged),
        ((0, 3), (0, 6), false)
    );
    c.apply(&ack(4, 4, 3, 6)).unwrap();
    assert!(c.loss((0, 4)).unwrap().acknowledged);
    assert!(matches!(
        c.apply(&ack(4, 4, 3, 6)),
        Err(CatalogueError::EpochNotAbove { .. })
    ));
}

#[test]
fn acknowledging_a_rebuild_no_record_names_is_refused() {
    let fs = initial();
    let mut m = open(&fs);
    assert!(matches!(
        m.acknowledge_rebuild(7, T0),
        Err(AcknowledgeError::UnknownRebuild(7))
    ));
}

#[test]
fn settling_a_record_not_yet_written_is_refused_and_no_false_proof_is_given() {
    let fs = initial();
    let mut m = open(&fs);
    let next = m.next_epoch();
    assert!(matches!(
        m.settle(next, T0),
        Err(kvik_files::appender::AppendError::NoRecord(e)) if e == next
    ));
    assert!(matches!(
        m.settle(0, T0),
        Err(kvik_files::appender::AppendError::NoRecord(0))
    ));
    // Nothing was appended for the refused calls.
    assert_eq!(m.catalogue().last_epoch(), 1);
    // The one written record is settled by an empty successor, and only then may be acted on irreversibly.
    assert_eq!(m.irreversible(1), Err(Held::NotSettled));
    assert_eq!(m.settle(1, T0).unwrap().epoch(), 1);
    assert_eq!(m.irreversible(1).unwrap().epoch(), 1);
    assert!(m.settled(m.catalogue().last_epoch()).is_none());
}

/// Q-871, `format.md` A.12 Open: the open makes the name `MANIFEST` durable before anything is appended to it, as a
/// rename stopped before its directory sync on the same boot leaves it in the page cache only.
#[test]
fn the_open_syncs_the_directory_so_a_renamed_manifest_survives_a_power_loss() {
    let fs = MemFs::new();
    let new = dir().join(kvik_files::MANIFEST_NEW);
    let f = fs.open(&new, kvik_log::fs::OpenMode::CreateNew).unwrap();
    f.write_at(&manifest_bytes(&[Record::new(1, 0, T0, Body::Empty)]), 0)
        .unwrap();
    f.sync_all().unwrap();
    drop(f);
    fs.sync_dir(&dir()).unwrap();
    fs.rename(&new, &manifest_path()).unwrap();
    let mut m = open(&fs);
    m.append(Body::Empty, T0).unwrap();
    drop(m);
    fs.power_loss();
    assert_eq!(reread(&fs).len(), 2);
}
