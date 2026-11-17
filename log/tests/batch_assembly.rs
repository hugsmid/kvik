//! Batches (`format.md` A.4, A.7): block splitting, the recovery entry, size limits, and the checks applied by the
//! reader.

mod support;

use kvik_log::batch::BatchBuilder;
use kvik_log::block::{FLAG_SKIPPABLE, block_type};
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{EntriesBlock, Entry, MarkState, Recovery};
use kvik_log::facts::Value;
use kvik_log::scan::{LossCause, ScanLimit, scan};
use kvik_log::segment::SegmentImage;
use kvik_log::{BodyError, ContentError, Element, EncodeError, FormatError};
use proptest::prelude::*;
use support::checks::*;
use support::*;

#[test]
fn blocks_split_canonically() {
    let c = |items| BatchContent {
        first_sequence: 10,
        received_at: T0,
        items,
    };
    // Arrival order: entries, facts, entries again.
    let items = vec![
        Item::Entry(Entry::Alias {
            id: 1,
            name: "a".into(),
        }),
        fact(Value::Bool(true)),
        fact(Value::Bool(false)),
        Item::Entry(Entry::Retirement { id: 1, t_r: T0 }),
    ];
    let blocks = c(items).blocks().unwrap();
    let firsts: Vec<_> = blocks
        .iter()
        .map(|b| match b {
            kvik_log::content::DataBody::Facts(f) => ('f', f.first_sequence, f.rows.len()),
            kvik_log::content::DataBody::Entries(e) => ('e', e.first_sequence, e.entries.len()),
        })
        .collect();
    assert_eq!(firsts, [('e', 10, 1), ('f', 11, 2), ('e', 13, 1)]);
    // 65,537 facts: a block of 65,536 rows and one of 1.
    let blocks = c(vec![fact(Value::Bool(true)); 65_537]).blocks().unwrap();
    assert!(
        matches!(&blocks[..], [kvik_log::content::DataBody::Facts(a), kvik_log::content::DataBody::Facts(b)] if a.rows.len() == 65_536 && b.rows.len() == 1 && b.first_sequence == 10 + 65_536)
    );
    // Texts of 255 bytes: 288 plain bytes per row, so 29,127 rows per block (8 MiB plain-encoded).
    let text = fact(Value::Text("t".repeat(255)));
    let content = c(vec![text.clone(); 29_128]);
    let blocks = content.blocks().unwrap();
    assert!(
        matches!(&blocks[..], [kvik_log::content::DataBody::Facts(a), kvik_log::content::DataBody::Facts(b)] if a.rows.len() == 29_127 && b.rows.len() == 1)
    );
    assert!(content.builder().is_ok());
    // More than 8 MiB of data blocks.
    assert_eq!(
        c(vec![text; 31_000]).builder().err(),
        Some(EncodeError::TooLarge)
    );
    assert_eq!(c(vec![]).blocks().err(), Some(EncodeError::EmptyBatch));
}

fn rec() -> Entry {
    Entry::Recovery(Recovery {
        after: 3,
        stamp: Some(kvik_log::mark::ResetStamp {
            sequence: 4,
            nonce: 9,
        }),
        boot_id: [1; 16],
        tail_bytes: 40,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 1,
        consumers: vec![],
    })
}

/// One batch of one entries block holding `entries`, after a damaged word when `damaged` (so that the scan reads it
/// on the bounded path, without checking its body again), then zeros: whether the scan keeps it.
fn kept_after(entries: Vec<Entry>, damaged: bool) -> bool {
    let mut f = vec![0u8; kvik_log::header::HEADER_LEN];
    if damaged {
        f.extend_from_slice(&[0xFF; 8]);
    }
    let at = f.len() as u64;
    let mut b = BatchBuilder::new();
    let body = EntriesBlock {
        first_sequence: 1,
        received_at: T0,
        entries,
    };
    b.push(block_type::ENTRIES, body.encode().unwrap()).unwrap();
    f.extend(b.encode(&ID, at, 0).unwrap());
    f.resize(f.len() + 64, 0);
    let r = scan(&f, &ID, ScanLimit::End).unwrap();
    r.batches.iter().any(|b| b.offset == at)
}

/// Q-651: past damage, a boundary read's batch is read without checking its bodies again, and a recovery entry that
/// is not alone in its batch is still found there, from the entry headers, wherever it lies in its body.
#[test]
fn after_damage_a_recovery_entry_not_alone_is_still_invalid_content() {
    let named = |name: &str| Entry::Alias {
        id: 1,
        name: name.into(),
    };
    for entries in [
        vec![rec(), named("a")],
        vec![named("a"), rec()],
        vec![named("a name that pads its entry"), named("b"), rec()],
    ] {
        assert!(!kept_after(entries.clone(), true), "{entries:?}");
        assert!(!kept_after(entries, false));
    }
    assert!(kept_after(vec![rec()], true));
    let mut f = vec![0u8; kvik_log::header::HEADER_LEN];
    f.extend_from_slice(&[0xFF; 8]);
    let at = f.len() as u64;
    f.extend(
        entries_batch(1, T0, &[alias(1, "a")])
            .encode(&ID, at, 0)
            .unwrap(),
    );
    let bad_at = f.len() as u64;
    let mut b = BatchBuilder::new();
    b.push(
        block_type::ENTRIES,
        EntriesBlock {
            first_sequence: 2,
            received_at: T0,
            entries: vec![
                rec(),
                Entry::Alias {
                    id: 1,
                    name: "a".into(),
                },
            ],
        }
        .encode()
        .unwrap(),
    )
    .unwrap();
    f.extend(b.encode(&ID, bad_at, 0).unwrap());
    let alone_at = f.len() as u64;
    let alone = BatchContent {
        first_sequence: 10,
        received_at: T0,
        items: vec![Item::Entry(rec())],
    };
    f.extend(alone.builder().unwrap().encode(&ID, alone_at, 0).unwrap());
    f.resize(f.len() + 64, 0);
    let r = scan(&f, &ID, ScanLimit::End).unwrap();
    let kept: Vec<u64> = r.batches.iter().map(|b| b.offset).collect();
    assert_eq!(kept, [at, alone_at]);
    assert_eq!(
        r.regions
            .iter()
            .map(|g| (g.start, g.end, g.cause))
            .collect::<Vec<_>>(),
        [
            (
                kvik_log::header::HEADER_LEN as u64,
                at,
                LossCause::Corruption
            ),
            (bad_at, alone_at, LossCause::InvalidContent)
        ]
    );
}

proptest! {
    /// Q-651: a batch read without checking its body again (past damage) and one read with every check (before any)
    /// are kept alike, whatever entries precede or follow a recovery entry and however they are padded.
    #[test]
    fn a_judged_batch_finds_a_recovery_entry_as_the_full_check_does(
        entries in prop::collection::vec(
            prop_oneof![
                Just(rec()),
                "[a-z]{1,30}".prop_map(|name| Entry::Alias { id: 1, name }),
                "[a-z0-9]{1,15}".prop_map(|version| Entry::Tzdata { version }),
            ],
            1..6,
        ),
    ) {
        prop_assert_eq!(kept_after(entries.clone(), true), kept_after(entries, false));
    }
}

#[test]
fn a_recovery_entry_is_alone_in_its_batch() {
    let c = BatchContent {
        first_sequence: 5,
        received_at: T0,
        items: vec![Item::Entry(rec()), fact(Value::Bool(true))],
    };
    assert_eq!(
        c.blocks().err(),
        Some(EncodeError::Content(ContentError::RecoveryNotAlone))
    );
    // Written anyway, with valid checksums: the reader takes it as invalid content.
    let mut b = BatchBuilder::new();
    b.push(
        block_type::ENTRIES,
        EntriesBlock {
            first_sequence: 1,
            received_at: T0,
            entries: vec![
                rec(),
                Entry::Alias {
                    id: 1,
                    name: "a".into(),
                },
            ],
        }
        .encode()
        .unwrap(),
    )
    .unwrap();
    let (s, _) = segment(ID, 1024, 1, &[(b, T0)]);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert!(r.batches.is_empty());
    assert!(matches!(
        r.tail,
        Some(kvik_log::scan::Tail::Torn {
            cause: LossCause::InvalidContent,
            ..
        })
    ));
    // Alone, it is read back.
    let alone = BatchContent {
        first_sequence: 1,
        received_at: T0,
        items: vec![Item::Entry(rec())],
    };
    let (s, _) = segment(ID, 1024, 1, &[(alone.builder().unwrap(), T0)]);
    let r = scan(s.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(
        BatchContent::decode_canonical(&r.batches[0]).unwrap(),
        alone
    );
}

#[test]
fn the_reader_applies_the_body_checks() {
    let batches = three_batches();
    let (s, offsets) = segment(ID, 2048, 1, &batches);
    // A facts body with quality 4 and every checksum recomputed: invalid content, the batches around it kept.
    let mut c = plain_columns(2);
    insert(&mut c, 3, 0, vec![0, 4]);
    let mut body = facts_raw(2, &c);
    kvik_log::body::DataPrefix {
        first_sequence: 2,
        received_at: T0,
        count: 2,
    }
    .write(&mut body);
    let bad = raw_batch(&ID, offsets[1], &[(block_type::FACTS, 0, body)], 2, 3, T0);
    let mut img = s.clone();
    img.zero_fill_from(offsets[1]).unwrap();
    put(img.bytes_mut(), offsets[1], &bad);
    let third_at = offsets[1] + bad.len() as u64;
    put(
        img.bytes_mut(),
        third_at,
        &batches[2].0.encode(&ID, third_at, batches[2].1).unwrap(),
    );
    let r = scan(img.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert_eq!(r.regions[0].cause, LossCause::InvalidContent);
    // An unknown critical column in a batch at a block boundary: a version refusal at the block.
    let mut c = plain_columns(1);
    c.push((0x11, 0, vec![]));
    let mut body = facts_raw(1, &c);
    kvik_log::body::DataPrefix {
        first_sequence: 2,
        received_at: T0,
        count: 1,
    }
    .write(&mut body);
    let mut img = SegmentImage::create_ahead(ID, T0, 2048).unwrap();
    img.take_into_use(1).unwrap();
    img.append(&batches[0].0, batches[0].1).unwrap();
    let at = img.write_offset();
    put(
        img.bytes_mut(),
        at,
        &raw_batch(&ID, at, &[(block_type::FACTS, 0, body)], 2, 2, T0),
    );
    assert_eq!(
        scan(img.bytes(), &ID, ScanLimit::End).map(|r| r.batches.len()),
        Err(FormatError::Unsupported {
            offset: at,
            element: Element::Column(0x11)
        })
    );
    // An unknown ignorable column and a skippable unknown block: read, and the content decodes (not canonically).
    let mut c = plain_columns(1);
    c.push((0x91, 0, vec![5]));
    let mut body = facts_raw(1, &c);
    kvik_log::body::DataPrefix {
        first_sequence: 2,
        received_at: T0,
        count: 1,
    }
    .write(&mut body);
    let blocks = vec![
        (block_type::FACTS, 0, body),
        (0x55, FLAG_SKIPPABLE, vec![1]),
    ];
    put(img.bytes_mut(), at, &raw_batch(&ID, at, &blocks, 2, 2, T0));
    let r = scan(img.bytes(), &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    let content = BatchContent::decode(&r.batches[1]).unwrap();
    assert_eq!(content.items.len(), 1);
    assert_eq!(
        BatchContent::decode_canonical(&r.batches[1]),
        Err(BodyError::NonCanonical)
    );
}

#[test]
fn golden_batches_are_canonical() {
    for name in ["in-use.open", "gap.open", "sealed.kvl"] {
        let b = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden")
                .join(name),
        )
        .unwrap();
        let r = scan(&b, &ID, ScanLimit::End).unwrap();
        for batch in &r.batches {
            let c = BatchContent::decode_canonical(batch).unwrap();
            assert_eq!(
                c.builder()
                    .unwrap()
                    .encode(&ID, batch.offset, batch.commit.commit_time)
                    .unwrap(),
                b[batch.offset as usize..batch.end() as usize]
            );
        }
    }
}
