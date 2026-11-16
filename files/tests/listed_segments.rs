//! `kvik_log::manifest::listed_segments`, which a log start uses to find sealed segments no record lists (Q-221 (b)),
//! read against records this crate encodes (`format.md` A.14).

mod support;

use std::collections::BTreeSet;

use kvik_files::desc::FileClass;
use kvik_files::record::{Body, Publish, Record};
use kvik_log::manifest::{listed_segments, read_manifest};
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
