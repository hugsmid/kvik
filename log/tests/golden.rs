//! Golden files: v1 log segments and a mark file, committed under `tests/golden/` (see its README).
//!
//! The files are produced by `generate` below. The tests check that the committed bytes equal what the generator
//! produces now, that each file decodes to what it is meant to show, and that decoding a file without damage and
//! encoding it again reproduces it byte for byte (`format.md` A, canonical writing). To regenerate after a
//! deliberate format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-log --test golden`.

mod support;

use std::fs;
use std::path::PathBuf;

use kvik_log::batch::BatchBuilder;
use kvik_log::block::{FLAG_SKIPPABLE, block_type};
use kvik_log::body::{INDEX_HAS_ENTRIES, IndexRecord};
use kvik_log::mark::{MarkSlot, encode_mark_file, read_mark};
use kvik_log::scan::{HeaderState, LossCause, ScanLimit, SealState, Tail, scan};
use kvik_log::segment::SegmentImage;
use kvik_log::{Element, FormatError};
use support::*;

const LEN: u64 = 2_048;
const BOOT: [u8; 16] = [0xB0; 16];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn in_use() -> (SegmentImage, Vec<u64>) {
    segment(ID, LEN, 1, &three_batches())
}

fn sealed_image() -> (SegmentImage, u64) {
    let (mut s, _) = in_use();
    let records = vec![IndexRecord {
        series_id: 1,
        fact_count: 4,
        min_t: T0,
        max_t: T0 + 30 * SECOND,
    }];
    let end = s.write_seal_blocks(records).unwrap();
    (s, end)
}

fn ends_of(bytes: &[u8]) -> Vec<u64> {
    scan(bytes, &ID, ScanLimit::End)
        .unwrap()
        .batches
        .iter()
        .map(|b| b.end())
        .collect()
}

/// Every golden file, by name.
fn generate() -> Vec<(&'static str, Vec<u8>)> {
    let mut files = Vec::new();

    // Created ahead of use, not yet in use (a short stand-in for `segment_size`).
    let s = SegmentImage::create_ahead(ID, T0, 1_024).unwrap();
    files.push(("created-ahead.open", s.into_bytes()));

    // In use: three batches, then zeros.
    let (s, offsets) = in_use();
    files.push(("in-use.open", s.bytes().to_vec()));

    // A torn final batch: the third batch's first 64 bytes reached the disk.
    let mut t = s.clone();
    t.zero_fill_from(offsets[2] + 64).unwrap();
    files.push(("torn-tail.open", t.into_bytes()));

    // Corruption inside the segment with a kept batch after it: one bit of the second batch's facts body.
    let mut c = s.clone();
    c.bytes_mut()[offsets[1] as usize + 40] ^= 0x10;
    files.push(("corrupt-middle.open", c.into_bytes()));

    // A gap: the second batch starts at sequence 10.
    let batches = vec![
        three_batches().remove(0),
        (
            facts_batch(
                10,
                T0 + 5 * SECOND,
                &[(1, T0, 19.5), (1, T0 + 10 * SECOND, 19.75)],
            ),
            T0 + 6 * SECOND,
        ),
    ];
    files.push(("gap.open", segment(ID, LEN, 1, &batches).0.into_bytes()));

    // Each seal state of A.4: sealed; seal step 1 done, length not set; seal blocks torn.
    let (mut s, end) = sealed_image();
    let pending = s.bytes().to_vec();
    s.set_len(end);
    files.push(("sealed.kvl", s.into_bytes()));
    files.push(("seal-length-pending.open", pending.clone()));
    let mut torn = sealed_image().0;
    torn.zero_fill_from(end - 40).unwrap();
    files.push(("seal-torn.open", torn.into_bytes()));

    // A block of unknown, skippable type inside a batch, and one of unknown, critical type.
    let r = T0 + SECOND;
    let mut b = facts_batch(2, r, &[(1, T0, 20.5)]);
    b.push_unknown(0x50, FLAG_SKIPPABLE, b"ignorable".to_vec())
        .unwrap();
    let batches = vec![three_batches().remove(0), (b, r + 1_000)];
    files.push((
        "unknown-skippable.open",
        segment(ID, LEN, 1, &batches).0.into_bytes(),
    ));
    let mut b = facts_batch(2, r, &[(1, T0, 20.5)]);
    b.push_unknown(0x51, 0, b"critical".to_vec()).unwrap();
    let batches = vec![three_batches().remove(0), (b, r + 1_000)];
    files.push((
        "unknown-critical.open",
        segment(ID, LEN, 1, &batches).0.into_bytes(),
    ));

    // Unknown elements inside bodies (A, common rules): ignorable ones are skipped; critical ones are refused.
    let facts = |extra: (u8, u8, Vec<u8>)| {
        let mut b = facts_raw(
            1,
            &[
                (1, 0, vec![1, 0, 0, 0]),
                (2, 0, T0.to_le_bytes().to_vec()),
                (5, 0, vec![1]),
                (6, 0, 20.5f64.to_bits().to_le_bytes().to_vec()),
                extra,
            ],
        );
        kvik_log::body::DataPrefix {
            first_sequence: 2,
            received_at: r,
            count: 1,
        }
        .write(&mut b);
        b
    };
    let alias_with = |extra: (u16, Vec<u8>)| {
        let (t, mut fields) = alias(1, "room/temp");
        fields.push(extra);
        (t, fields)
    };
    let with_blocks = |blocks: Vec<(u8, Vec<u8>)>| {
        let mut b = BatchBuilder::new();
        for (t, body) in blocks {
            b.push(t, body).unwrap();
        }
        segment(ID, LEN, 1, &[three_batches().remove(0), (b, r + 1_000)])
            .0
            .into_bytes()
    };
    let ignorable_entry: support::Entry = (0x90, vec![(1, b"later".to_vec())]);
    files.push((
        "unknown-ignorable-content.open",
        with_blocks(vec![
            (block_type::FACTS, facts((0x90, 0, b"ignorable".to_vec()))),
            (
                block_type::ENTRIES,
                entries_body(
                    3,
                    r,
                    &[ignorable_entry, alias_with((0x8001, b"ignorable".to_vec()))],
                ),
            ),
        ]),
    ));
    files.push((
        "unknown-critical-column.open",
        with_blocks(vec![(block_type::FACTS, facts((0x10, 0, vec![])))]),
    ));
    files.push((
        "unknown-critical-entry.open",
        with_blocks(vec![(
            block_type::ENTRIES,
            entries_body(2, r, &[(0x10, vec![])]),
        )]),
    ));
    files.push((
        "unknown-critical-field.open",
        with_blocks(vec![(
            block_type::ENTRIES,
            entries_body(2, r, &[alias_with((0x0100, vec![]))]),
        )]),
    ));

    // A mark with one torn slot: slot 0 (counter 7) names the end of the second batch of `in-use.open`; slot 1
    // (counter 8, the third batch) was torn while being written.
    let e = ends_of(&files[1].1);
    let slot = |counter, offset, last_sequence| MarkSlot {
        generation: 0,
        store_id: STORE,
        boot_id: BOOT,
        counter,
        file_id: ID.file_id,
        offset,
        last_sequence,
        max_series_id: 1,
    };
    let mut mark = encode_mark_file([Some(slot(7, e[1], 3)), Some(slot(8, e[2], 5))]);
    mark[4_096 + 64] ^= 0x01;
    files.push(("torn-slot.mark", mark));
    files
}

#[test]
fn golden_files_match_the_generator() {
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some_and(|v| v == "1");
    let files = generate();
    for (name, bytes) in &files {
        let path = dir().join(name);
        if bless {
            fs::write(&path, bytes).unwrap();
        }
        let committed = fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            committed == *bytes,
            "{name} differs from the generator: the format changed"
        );
    }
    // No other file is in the directory.
    let mut names: Vec<_> = fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut expected: Vec<_> = files
        .iter()
        .map(|f| f.0.to_string())
        .chain(["README.md".to_string()])
        .collect();
    expected.sort();
    assert_eq!(names, expected);
}

fn read(name: &str) -> Vec<u8> {
    fs::read(dir().join(name)).unwrap()
}

fn seqs(bytes: &[u8]) -> Vec<(u64, u64)> {
    let r = scan(bytes, &ID, ScanLimit::End).unwrap();
    r.batches
        .iter()
        .map(|b| (b.commit.first_sequence, b.commit.last_sequence))
        .collect()
}

#[test]
fn golden_files_decode_to_what_they_show() {
    let b = read("created-ahead.open");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert!(matches!(r.header, HeaderState::Valid(h) if h.base_sequence == 0));
    assert!(!r.in_use());
    assert_eq!(r.tail, Some(Tail::Clean { offset: 64 }));

    let b = read("in-use.open");
    assert_eq!(seqs(&b), [(1, 1), (2, 3), (4, 5)]);
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(
        r.batches[0].blocks[0].block.header.block_type,
        block_type::ENTRIES
    );
    assert_eq!(r.batches[2].blocks.len(), 2);
    assert!(matches!(r.tail, Some(Tail::Clean { .. })));

    let b = read("torn-tail.open");
    assert_eq!(seqs(&b), [(1, 1), (2, 3)]);
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert!(r.regions.is_empty());
    assert!(matches!(r.tail, Some(Tail::Torn { start, .. }) if start == r.batches[1].end()));

    let b = read("corrupt-middle.open");
    assert_eq!(seqs(&b), [(1, 1), (4, 5)]);
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(r.regions.len(), 1);
    assert_eq!(r.regions[0].cause, LossCause::Corruption);
    assert_eq!(
        (
            r.regions[0].last_sequence_before,
            r.regions[0].first_sequence_after
        ),
        (Some(1), Some(4))
    );

    assert_eq!(seqs(&read("gap.open")), [(1, 1), (10, 11)]);

    let b = read("sealed.kvl");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    let seal = r.seal.as_ref().unwrap();
    assert_eq!(seal.state, SealState::Sealed);
    assert_eq!(seal.end, b.len() as u64);
    assert!(seal.file_crc_matches(&b));
    assert_eq!(
        seal.index.clone().flatten().unwrap().flags,
        INDEX_HAS_ENTRIES
    );
    assert_eq!((seal.trailer.entry_count, seal.trailer.batch_count), (5, 3));
    assert_eq!(r.batches.len(), 3);

    let b = read("seal-length-pending.open");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(r.seal.unwrap().state, SealState::LengthPending);

    let b = read("seal-torn.open");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!((r.seal.is_none(), r.batches.len()), (true, 3));
    assert!(matches!(r.tail, Some(Tail::Torn { .. })));

    let b = read("unknown-skippable.open");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(r.batches.len(), 2);
    assert_eq!(r.batches[1].blocks[1].prefix, None);
    assert_eq!(r.batches[1].blocks[1].block.body, b"ignorable");

    let b = read("unknown-critical.open");
    let e = scan(&b, &ID, ScanLimit::End)
        .map(|r| r.batches.len())
        .unwrap_err();
    assert!(matches!(
        e,
        FormatError::Unsupported {
            element: Element::BlockType(0x51),
            ..
        }
    ));

    let b = read("unknown-ignorable-content.open");
    let r = scan(&b, &ID, ScanLimit::End).unwrap();
    assert_eq!(seqs(&b), [(1, 1), (2, 4)]);
    let content = kvik_log::content::BatchContent::decode(&r.batches[1]).unwrap();
    assert_eq!(content.items.len(), 3);
    assert!(matches!(
        content.items[1],
        kvik_log::content::Item::Entry(kvik_log::entries::Entry::Ignored { entry_type: 0x90 })
    ));
    for (name, element) in [
        ("unknown-critical-column.open", Element::Column(0x10)),
        ("unknown-critical-entry.open", Element::EntryType(0x10)),
        (
            "unknown-critical-field.open",
            Element::Field {
                entry_type: 3,
                field: 0x0100,
            },
        ),
    ] {
        let b = read(name);
        let e = scan(&b, &ID, ScanLimit::End)
            .map(|r| r.batches.len())
            .unwrap_err();
        assert!(
            matches!(e, FormatError::Unsupported { element: el, .. } if el == element),
            "{name}: {e:?}"
        );
    }

    // The mark falls back to its older slot; a reader reads `in-use.open` only up to it.
    let (slot, mark) = read_mark(&read("torn-slot.mark"), &STORE).unwrap().unwrap();
    assert_eq!(
        (slot, mark.counter, mark.last_sequence, mark.boot_id),
        (0, 7, 3, BOOT)
    );
    let b = read("in-use.open");
    let r = scan(&b, &ID, ScanLimit::Mark(mark.offset)).unwrap();
    assert_eq!(
        r.batches.last().unwrap().commit.last_sequence,
        mark.last_sequence
    );
    assert!(r.regions.is_empty());
}

/// Decodes a segment without damage and writes it again from what was decoded.
fn reencode(bytes: &[u8]) -> Vec<u8> {
    let r = scan(bytes, &ID, ScanLimit::End).unwrap();
    let HeaderState::Valid(h) = r.header else {
        panic!("invalid header")
    };
    let mut s = SegmentImage::create_ahead(ID, h.created_at, bytes.len() as u64).unwrap();
    if h.base_sequence > 0 {
        s.take_into_use(h.base_sequence).unwrap();
    }
    for b in &r.batches {
        assert_eq!(
            s.append(&BatchBuilder::from_batch(b), b.commit.commit_time)
                .unwrap(),
            b.offset
        );
    }
    if let Some(seal) = &r.seal {
        let index = seal.index.clone().flatten().unwrap();
        s.write_seal_blocks(index.records).unwrap();
        s.set_len(bytes.len() as u64);
    }
    s.into_bytes()
}

#[test]
fn encoding_again_reproduces_the_files_without_damage() {
    for name in [
        "created-ahead.open",
        "in-use.open",
        "gap.open",
        "sealed.kvl",
        "seal-length-pending.open",
        "unknown-skippable.open",
    ] {
        let b = read(name);
        assert!(reencode(&b) == b, "{name}");
    }
}
