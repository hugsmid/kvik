//! `kvik_log::manifest::listed_segments`, which a log start uses to find sealed segments no record lists (Q-221 (b)),
//! read against records this crate encodes (`format.md` A.14).

mod support;

use std::collections::BTreeSet;

use kvik_files::catalogue::Tier;
use kvik_files::desc::{FileClass, FileDescription};
use kvik_files::record::{Body, Publish, Record};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::manifest::{listed_segments, read_manifest};
use kvik_log::mark::ResetStamp;
use support::*;

#[test]
fn listed_segments_reads_the_log_segments_of_sealed_and_publish_records_by_generation() {
    let mut later = segment(5, 30, 40);
    later.generation = 2;
    later.range = Some(((2, 30), (2, 40)));
    let records = [
        Record::new(1, 0, T0, Body::Empty),
        Record::new(
            2,
            0,
            T0,
            Body::Sealed(vec![segment(1, 1, 9), segment(3, 20, 29)]),
        ),
        Record::new(
            3,
            0,
            T0,
            Body::Publish(Publish {
                layouts: Vec::new(),
                published: vec![compacted(
                    8,
                    FileClass::Current,
                    (1, 9),
                    &[seg_ref(2), comp_ref(7)],
                )],
                retired: vec![compacted(7, FileClass::Current, (1, 9), &[seg_ref(4)])],
            }),
        ),
        Record::new(4, 2, T0, Body::Sealed(vec![later])),
    ];
    let bytes = manifest_bytes(&records);
    let read = read_manifest(&bytes, &STORE_ID).unwrap();
    let want: BTreeSet<(u32, u64)> = [(0, 1), (0, 3), (2, 5)].into();
    assert_eq!(listed_segments(&bytes, &read), want);
}

fn restored(description: FileDescription) -> RestoredFile {
    RestoredFile {
        description,
        tier: Tier::Hot,
        copy: DeepCopy {
            size: 4096,
            sha256: [7; 32],
        },
    }
}

fn in_generation(mut d: FileDescription, generation: u32) -> FileDescription {
    d.generation = generation;
    d.range = d
        .range
        .map(|((_, a), (_, b))| ((generation, a), (generation, b)));
    d
}

#[test]
fn listed_segments_counts_the_segments_a_restore_record_lists_set_aside_ones_included() {
    let restore = RestoreRecord {
        generation: 2,
        stamp: ResetStamp {
            sequence: 1,
            nonce: 9,
        },
        source_generation: 1,
        restored_end: (1, 20),
        carried_ends: Vec::new(),
        losses: Vec::new(),
        acknowledgements: Vec::new(),
        entry_losses: Vec::new(),
        files: vec![
            restored(segment(1, 1, 9)),
            restored(in_generation(segment(4, 10, 20), 1)),
            // Above the restored end: set aside, still listed.
            restored(in_generation(segment(6, 21, 30), 1)),
            restored(compacted(8, FileClass::Current, (1, 9), &[seg_ref(1)])),
        ],
        drill: false,
    };
    assert_eq!(restore.check(), Ok(()));
    let later = in_generation(segment(7, 1, 5), 2);
    let records = [
        Record::new(1, 2, T0, restore.to_body()),
        Record::new(2, 2, T0, Body::Sealed(vec![later])),
    ];
    let bytes = manifest_bytes(&records);
    let read = read_manifest(&bytes, &STORE_ID).unwrap();
    let want: BTreeSet<(u32, u64)> = [(0, 1), (1, 4), (1, 6), (2, 7)].into();
    assert_eq!(listed_segments(&bytes, &read), want);
}
