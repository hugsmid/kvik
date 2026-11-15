//! Manifest records (`format.md` A.14): round trips, canonical bytes, strict checks, golden files, and that the
//! log's own manifest reader reads what this crate writes.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_files::desc::{FileClass, kind};
use kvik_files::image::read;
use kvik_files::record::{
    AckedLoss, Acknowledgement, Body, Publish, Rebuild, Record, RecordError, is_canonical,
    record_type,
};
use kvik_log::Element;
use kvik_log::block::{FLAG_SKIPPABLE, encode_block};
use kvik_log::manifest::{LossCause, LostRange, RECORD_LOST_RANGE, encode_record, read_manifest};
use kvik_log::mark::ResetStamp;
use proptest::prelude::*;
use support::*;

fn decode_block(b: &[u8]) -> Result<Record, RecordError> {
    let len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    Record::decode(b[0], b[1] & FLAG_SKIPPABLE != 0, &b[16..16 + len])
}

proptest! {
    #[test]
    fn a_record_round_trips_and_its_bytes_are_canonical(r in record()) {
        let b = r.encode().unwrap();
        prop_assert_eq!(b.len() % 8, 0);
        prop_assert_eq!(decode_block(&b), Ok(r.clone()));
        prop_assert!(is_canonical(&b));
        // Decoding and encoding again gives the same bytes.
        prop_assert_eq!(decode_block(&b).unwrap().encode().unwrap(), b);
    }

    #[test]
    fn a_damaged_record_is_never_read_as_another(r in record(), at in any::<prop::sample::Index>(), bit in 0u8..8) {
        let mut b = r.encode().unwrap();
        let i = at.index(b.len());
        b[i] ^= 1 << bit;
        let mut m = manifest_bytes(&[Record::new(1, 0, T0, Body::Empty)]);
        m.extend(&b);
        // The damaged record is invalid or, if the damage hit only padding, still the same record.
        let img = read(&m, &STORE_ID);
        if let Ok(img) = img && img.records.len() == 2 {
            prop_assert_eq!(&img.records[1].record, &r);
        }
    }

    #[test]
    fn the_log_reader_reads_every_manifest_this_crate_writes(h in history()) {
        let bytes = manifest_bytes(&h.records);
        let ours = read(&bytes, &STORE_ID).unwrap();
        let theirs = read_manifest(&bytes, &STORE_ID).unwrap();
        prop_assert_eq!(theirs.records.len(), ours.records.len());
        prop_assert!(!theirs.torn);
        let epochs: Vec<u64> = theirs.records.iter().map(|r| r.epoch).collect();
        let want: Vec<u64> = h.records.iter().map(|r| r.epoch).collect();
        prop_assert_eq!(epochs, want);
        let lost: Vec<_> = h.records.iter().filter_map(|r| match &r.body {
            Body::LostRange(l) => Some(l.clone()),
            _ => None,
        }).collect();
        prop_assert_eq!(theirs.lost_ranges().cloned().collect::<Vec<_>>(), lost);
    }
}

fn publish() -> Body {
    Body::Publish(Publish {
        published: vec![compacted(
            5,
            FileClass::Current,
            (1, 20),
            &[seg_ref(1), seg_ref(2)],
        )],
        retired: vec![compacted(3, FileClass::Current, (1, 9), &[seg_ref(1)])],
    })
}

#[test]
fn the_checks_of_each_record_type() {
    let ok = |b: Body| Record::new(2, 0, T0, b).encode().is_ok();
    assert!(ok(publish()));
    // A publish naming nothing, a file twice, a file both published and retired, out of order.
    assert!(!ok(Body::Publish(Publish::default())));
    let d = compacted(5, FileClass::Current, (1, 20), &[seg_ref(1)]);
    assert!(!ok(Body::Publish(Publish {
        published: vec![d.clone(), d.clone()],
        retired: vec![]
    })));
    assert!(!ok(Body::Publish(Publish {
        published: vec![d.clone()],
        retired: vec![d.clone()]
    })));
    let e = compacted(4, FileClass::Current, (1, 20), &[seg_ref(1)]);
    assert!(!ok(Body::Publish(Publish {
        published: vec![d.clone(), e.clone()],
        retired: vec![]
    })));
    // A sealed-segment record names log segments only.
    assert!(ok(Body::Sealed(vec![segment(1, 1, 5)])));
    assert!(!ok(Body::Sealed(vec![d])));
    assert!(!ok(Body::Sealed(vec![])));
    // A lost range in order; an acknowledgement naming something, in order.
    assert!(!ok(Body::LostRange(lost(
        5,
        6,
        5,
        LossCause::Unknown,
        false
    ))));
    assert!(!ok(Body::Acknowledgement(Acknowledgement::default())));
    let a = |i| AckedLoss {
        identity: (0, i),
        first: (0, i),
        last: (0, i),
    };
    assert!(!ok(Body::Acknowledgement(Acknowledgement {
        losses: vec![a(5), a(4)],
        rebuild: None
    })));
    assert!(ok(Body::Acknowledgement(Acknowledgement {
        losses: vec![],
        rebuild: Some(3)
    })));
    assert!(!ok(Body::Rebuild(Rebuild {
        floors: vec![(2, 1), (1, 1)],
        ..Rebuild::default()
    })));
}

/// The bytes of a block of type `t` with `fields` after the record prefix of epoch 2.
fn raw(t: u8, flags: u8, fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&2u64.to_le_bytes());
    body.extend_from_slice(&[0; 16]);
    for (id, v) in fields {
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&(v.len() as u16).to_le_bytes());
        body.extend_from_slice(v);
        body.resize(body.len().div_ceil(4) * 4, 0);
    }
    let mut out = Vec::new();
    encode_block(t, flags, &body, &mut out).unwrap();
    out
}

#[test]
fn unknown_elements_are_refused_or_skipped_by_the_critical_and_ignorable_rule() {
    // An unknown critical field of a known type is a version refusal; an ignorable one is kept and skipped.
    let b = raw(record_type::EMPTY, 0, &[(7, vec![1])]);
    assert_eq!(
        decode_block(&b),
        Err(RecordError::Unsupported(Element::Field {
            entry_type: record_type::EMPTY,
            field: 7
        }))
    );
    let b = raw(record_type::EMPTY, 0, &[(0x8001, vec![1, 2])]);
    let r = decode_block(&b).unwrap();
    assert_eq!(r.ignorable, vec![(0x8001, vec![1, 2])]);
    assert!(is_canonical(&b));
    // An unknown critical record type is refused; with `skippable` it is carried as a field list.
    assert_eq!(
        decode_block(&raw(0x51, 0, &[])),
        Err(RecordError::Unsupported(Element::BlockType(0x51)))
    );
    let b = raw(
        0x51,
        FLAG_SKIPPABLE,
        &[
            (1, vec![9]),
            (100, 7u64.to_le_bytes().to_vec()),
            (200, vec![]),
        ],
    );
    let r = decode_block(&b).unwrap();
    assert_eq!(r.old_epoch, Some(7));
    assert!(is_canonical(&b));
    // A reserved type (restore, quarantine, tier move) is carried as a field list, field 100 added in order.
    let b = raw(
        record_type::TIER_MOVE,
        0,
        &[(1, vec![1; 12]), (300, vec![2])],
    );
    let mut r = decode_block(&b).unwrap();
    r.old_epoch = Some(2);
    let again = r.encode().unwrap();
    assert_eq!(decode_block(&again).unwrap(), r);
    assert!(is_canonical(&again));
    // A required field twice, a cause out of range, field 100 twice: invalid.
    let l = Record::new(
        2,
        0,
        T0,
        Body::LostRange(lost(1, 1, 2, LossCause::Unknown, false)),
    );
    let mut b = l.encode().unwrap();
    let cause_at = b.windows(4).position(|w| w == [7, 0, 1, 0]).unwrap();
    b[cause_at + 4] = 6;
    let len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    assert!(matches!(
        Record::decode(b[0], false, &b[16..16 + len]),
        Err(RecordError::Invalid(_))
    ));
    let twice = raw(
        record_type::EMPTY,
        0,
        &[
            (100, 1u64.to_le_bytes().to_vec()),
            (100, 1u64.to_le_bytes().to_vec()),
        ],
    );
    assert!(matches!(decode_block(&twice), Err(RecordError::Invalid(_))));
    // Fields out of canonical order are read but not canonical.
    let r = Record::new(
        2,
        0,
        0,
        Body::Acknowledgement(Acknowledgement {
            losses: vec![],
            rebuild: Some(3),
        }),
    );
    let mut fields = vec![(2u16, 3u64.to_le_bytes().to_vec())];
    fields.push((100, 1u64.to_le_bytes().to_vec()));
    let b = raw(record_type::ACKNOWLEDGEMENT, 0, &fields);
    assert!(is_canonical(&b));
    let swapped = raw(
        record_type::ACKNOWLEDGEMENT,
        0,
        &[fields[1].clone(), fields[0].clone()],
    );
    assert_eq!(decode_block(&swapped).unwrap().old_epoch, Some(1));
    assert!(!is_canonical(&swapped));
    assert_eq!(decode_block(&b).unwrap().body, r.body);
}

#[test]
fn a_rebuilt_lost_range_with_cause_unknown_is_read_by_the_log() {
    let records = vec![
        Record::new(1, 0, T0, Body::Empty),
        Record::new(
            2,
            0,
            T0,
            Body::LostRange(lost(4, 4, 6, LossCause::Unknown, false)),
        ),
        Record::new(3, 0, T0, publish()),
        Record::new(4, 0, T0, Body::Sealed(vec![segment(1, 1, 9)])),
        Record::new(5, 0, T0, Body::Empty),
    ];
    let m = read_manifest(&manifest_bytes(&records), &STORE_ID).unwrap();
    assert_eq!(m.records.len(), 5);
    assert_eq!(
        m.records[1].lost.as_ref().unwrap().cause,
        LossCause::Unknown
    );
}

/// A lost-range record carrying the reset stamp in force (field 13, `format.md` A.14) reads the same in this crate and
/// in the log, whichever wrote it.
#[test]
fn a_lost_range_with_a_reset_stamp_reads_the_same_in_both_crates() {
    let l = LostRange {
        stamp: Some(ResetStamp {
            sequence: 9,
            nonce: 7,
        }),
        ..lost(3, 3, 4, LossCause::TruncatedTail, false)
    };
    let ours = manifest_bytes(&[Record::new(1, 0, T0, Body::LostRange(l.clone()))]);
    let mut theirs = manifest_bytes(&[]);
    theirs.extend(encode_record(RECORD_LOST_RANGE, 1, 0, T0, Some(&l)));
    for bytes in [&ours, &theirs] {
        assert_eq!(
            read(bytes, &STORE_ID).unwrap().records[0].record.body,
            Body::LostRange(l.clone())
        );
        assert_eq!(
            read_manifest(bytes, &STORE_ID)
                .unwrap()
                .lost_ranges()
                .cloned()
                .collect::<Vec<_>>(),
            vec![l.clone()]
        );
    }
}

// ---- Golden files ----

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn every_record() -> Vec<Record> {
    let mut carried = Record::new(
        8,
        0,
        T0 + 8,
        Body::Acknowledgement(Acknowledgement {
            losses: vec![AckedLoss {
                identity: (0, 4),
                first: (0, 4),
                last: (0, 6),
            }],
            rebuild: Some(6),
        }),
    );
    carried.old_epoch = Some(3);
    vec![
        Record::new(1, 0, T0, Body::Empty),
        Record::new(
            2,
            0,
            T0 + 2,
            Body::Sealed(vec![segment(1, 1, 9), segment(2, 10, 20)]),
        ),
        Record::new(3, 0, T0 + 3, publish()),
        Record::new(
            4,
            0,
            T0 + 4,
            Body::LostRange(lost(4, 4, 6, LossCause::Corruption, false)),
        ),
        Record::new(
            5,
            0,
            T0 + 5,
            Body::LostRange(lost(4, 4, 7, LossCause::Unknown, true)),
        ),
        Record::new(
            6,
            0,
            T0 + 6,
            Body::Rebuild(Rebuild {
                presumed_epoch: 3,
                last_valid_before: 2,
                files_found: 4,
                losses_restated: 1,
                floors: vec![
                    (kind::LOG_SEGMENT, 3 + (1 << 32)),
                    (kind::COMPACTED, 5 + (1 << 32)),
                ],
                identities_rebuilt: 0,
                gaps: vec![(2, 4)],
            }),
        ),
        carried,
    ]
}

/// Every golden file, by name.
fn generate() -> Vec<(&'static str, Vec<u8>)> {
    let records = every_record();
    let all = manifest_bytes(&records);
    let mut out = vec![("every-record.manifest", all.clone())];
    // A torn tail: the last record cut after 24 bytes.
    let last = records.last().unwrap().encode().unwrap();
    let mut torn = all.clone();
    torn.truncate(all.len() - last.len() + 24);
    out.push(("torn-tail.manifest", torn));
    // A corrupt record: one bit of the third record's body flipped, the records after it intact.
    let mut bad = all;
    corrupt_record(&mut bad, &records, 2);
    out.push(("corrupt-record.manifest", bad));
    out
}

#[test]
fn golden_files_are_what_the_generator_writes_and_read_as_they_show() {
    let files = generate();
    if std::env::var_os("KVIK_BLESS_GOLDEN").is_some() {
        for (name, bytes) in &files {
            fs::write(golden_dir().join(name), bytes).unwrap();
        }
    }
    for (name, bytes) in &files {
        let on_disk =
            fs::read(golden_dir().join(name)).unwrap_or_else(|_| panic!("{name} missing"));
        assert_eq!(&on_disk, bytes, "{name} differs from the generator");
    }
    let records = every_record();
    let img = read(&files[0].1, &STORE_ID).unwrap();
    assert_eq!(img.records().cloned().collect::<Vec<_>>(), records);
    assert!(!img.torn && !img.is_corrupt());
    // Decoding and encoding again gives the file byte for byte.
    assert_eq!(
        manifest_bytes(&img.records().cloned().collect::<Vec<_>>()),
        files[0].1
    );
    let img = read(&files[1].1, &STORE_ID).unwrap();
    assert_eq!((img.records.len(), img.torn), (records.len() - 1, true));
    let img = read(&files[2].1, &STORE_ID).unwrap();
    assert_eq!(img.records.len(), records.len() - 1);
    assert_eq!(img.corrupt.len(), 1);
    assert_eq!(img.corrupt[0].presumed_epoch(), 3);
    assert_eq!(img.corrupt[0].before_epoch, 4);
}
