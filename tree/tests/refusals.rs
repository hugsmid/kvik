//! Every refusal of `format-tree.md` T.11 and T.12 has a named vector here, and the table covers [`Refusal::ALL`].

mod files;
mod vectors;

use std::collections::HashSet;

use files::*;
use kvik_tree::fuzz::reframe;
use kvik_tree::part::{Part, block_type};
use kvik_tree::root::Root;
use kvik_tree::{Kind, MINUTE, Refusal, encode};
use vectors::*;

#[test]
fn every_refusal_has_a_vector_and_each_vector_is_refused_as_named() {
    let mut seen = HashSet::new();
    let mut wrong = Vec::new();
    for (want, file, bytes) in vectors() {
        let got = match file {
            File::Part => Part::decode(&bytes).map(|_| ()),
            File::Root => Root::decode(&bytes).map(|_| ()),
        };
        let got = got.map_err(|e| e.refusal);
        if got != Err(want) {
            wrong.push((want, got));
        }
        assert!(seen.insert(want), "two vectors for {want:?}");
    }
    assert!(wrong.is_empty(), "vectors refused otherwise: {wrong:?}");
    let missing: Vec<_> = Refusal::ALL.iter().filter(|r| !seen.contains(r)).collect();
    assert!(missing.is_empty(), "refusals without a vector: {missing:?}");
}

#[test]
fn root_refusals_shared_with_parts() {
    let cases: Vec<(Refusal, Vec<u8>)> = vec![
        (Refusal::UnknownLevel, edit_root(|b| b[COVERED] = 9)),
        (Refusal::Alignment, edit_root(|b| b[COVERED + 8] ^= 1)),
        (Refusal::UnknownClass, edit_root(|b| b[PARTS + 16] = 7)),
        (
            Refusal::Period,
            edit_root(|b| put(b, PARTS + 32, &0u64.to_le_bytes())),
        ),
        (Refusal::ArrayLength, edit_root(|b| b[ROOT_BODY + 56] = 5)),
        (Refusal::RecordSize, edit_root(|b| b[ROOT_BODY + 78] = 96)),
        (Refusal::Kind, edit_root(|b| b[4] = 6)),
        (Refusal::BlockOrder, {
            let mut b = root_bytes();
            b.extend_from_slice(&[0; 16]);
            b
        }),
    ];
    for (want, bytes) in cases {
        assert_eq!(
            Root::decode(&bytes).map(|_| ()).map_err(|e| e.refusal),
            Err(want),
            "{want:?}"
        );
    }
}

#[test]
fn base_position_of_the_last_run_is_checked() {
    let bytes = with_nodes(vec![nodes_body(
        1,
        2,
        T0,
        pos(3, 5),
        &[(0, 0, 0, 1, gauge(T0, 3))],
    )]);
    assert_eq!(
        Part::decode(&bytes).map(|_| ()).map_err(|e| e.refusal),
        Err(Refusal::BasePosition)
    );
}

#[test]
fn version_refusals_are_the_ones_a_newer_writer_can_cause() {
    let v: Vec<_> = Refusal::ALL.iter().filter(|r| r.is_version()).collect();
    assert_eq!(
        v,
        [
            &Refusal::NewerMajor,
            &Refusal::UnknownBlock,
            &Refusal::RecordSize,
            &Refusal::UnknownClass,
            &Refusal::EntryFlag,
            &Refusal::NodeForm,
            &Refusal::FloorFlag
        ]
    );
}

fn part_refusal(bytes: &[u8]) -> Result<(), Refusal> {
    Part::decode(bytes).map(|_| ()).map_err(|e| e.refusal)
}

fn root_refusal(bytes: &[u8]) -> Result<(), Refusal> {
    Root::decode(bytes).map(|_| ()).map_err(|e| e.refusal)
}

fn counter(start: i64, width: i64) -> Vec<u8> {
    encode(&node(Kind::Counter, start, width, 3, 1)).unwrap()
}

#[test]
fn nodes_of_one_series_at_two_levels_of_different_forms_are_refused() {
    let bytes = with_nodes(vec![
        nodes_body(1, 2, T0, pos(3, 5), &[(0, 0, 0, 0, gauge(T0, 3))]),
        nodes_body(1, 3, T0, pos(3, 5), &[(0, 0, 0, 0, counter(T0, width(3)))]),
    ]);
    assert_eq!(part_refusal(&bytes), Err(Refusal::NodeKind));
    let run = |level: u8, kind| kvik_tree::part::Run {
        series: 1,
        level,
        entries: vec![kvik_tree::part::Entry {
            start: T0,
            stale: false,
            as_of: pos(3, 5),
            node: Some(node(kind, T0, width(level), 3, 1)),
        }],
    };
    let part = Part {
        runs: vec![run(2, Kind::Gauge), run(3, Kind::Counter)],
        ..empty_part()
    };
    assert!(part.encode().is_err());
}

#[test]
fn nodes_of_two_series_of_different_forms_are_accepted() {
    let run = |series, kind| kvik_tree::part::Run {
        series,
        level: 2,
        entries: vec![kvik_tree::part::Entry {
            start: T0,
            stale: false,
            as_of: pos(3, 5),
            node: Some(node(kind, T0, width(2), 3, 1)),
        }],
    };
    let part = Part {
        runs: vec![run(1, Kind::Gauge), run(2, Kind::Counter)],
        ..empty_part()
    };
    let bytes = part.encode().unwrap();
    assert_eq!(Part::decode(&bytes).unwrap(), part);
}

#[test]
fn nodes_of_one_run_in_two_blocks_of_different_forms_are_refused() {
    let q = width(2);
    let bytes = with_nodes(vec![
        nodes_body(1, 2, T0, pos(3, 5), &[(0, 0, 0, 0, gauge(T0, 3))]),
        nodes_body(1, 2, T0 + q, pos(3, 5), &[(0, 0, 0, 0, counter(T0 + q, q))]),
    ]);
    assert_eq!(part_refusal(&bytes), Err(Refusal::NodeKind));
}

fn record_size_of(block: u8, size: u16) -> Vec<u8> {
    edit_block(|t, _| t == block, |b| put(b, 4, &size.to_le_bytes()))
}

#[test]
fn a_directory_or_directory_root_record_size_above_v1_is_a_version_refusal() {
    use block_type::{DIRECTORY, DIRECTORY_ROOT};
    for (block, larger, smaller) in [(DIRECTORY, 56, 40), (DIRECTORY_ROOT, 40, 24)] {
        let got = Part::decode(&record_size_of(block, larger)).unwrap_err();
        assert_eq!(got.refusal, Refusal::RecordSize, "block {block:#x}");
        assert!(got.refusal.is_version());
        assert_eq!(
            part_refusal(&record_size_of(block, smaller)),
            Err(Refusal::RecordSizeSmall),
            "block {block:#x}"
        );
    }
}

/// The sample root with its parts replaced; each `(file_id, seq, day, days, least series, greatest series)`.
fn root_with_parts(parts: &[(u64, u64, i64, i64, u64, u64)]) -> kvik_tree::root::Root {
    let mut r = sample_root();
    let template = r.parts[0];
    r.parts = parts
        .iter()
        .map(
            |&(file_id, seq, day, days, lo, hi)| kvik_tree::root::PartRef {
                file_id,
                seq,
                period_start: T0 + day * DAY,
                period_len: (days * DAY) as u64,
                least_start: T0 + day * DAY,
                greatest_end: T0 + (day + days) * DAY,
                least_series: lo,
                greatest_series: hi,
                ..template
            },
        )
        .collect();
    r.retired.clear();
    r
}

#[test]
fn parts_of_one_seq_are_accepted_when_periods_or_series_ranges_are_disjoint() {
    for parts in [
        vec![(3, 9, 0, 1, 7, 20), (5, 9, 0, 1, 21, 40)],
        vec![(3, 9, 0, 1, 7, 40), (5, 9, 1, 1, 7, 40)],
        vec![
            (3, 9, 0, 7, 0, 10),
            (4, 9, 0, 7, 20, 30),
            (5, 9, 2, 1, 11, 19),
        ],
    ] {
        let root = root_with_parts(&parts);
        let bytes = root.encode().unwrap_or_else(|e| panic!("{parts:?}: {e}"));
        assert_eq!(Root::decode(&bytes).unwrap(), root);
    }
}

/// The bytes of [`root_with_parts`], encoded with distinct `seq`s and then given the ones asked for.
fn root_bytes_with_parts(parts: &[(u64, u64, i64, i64, u64, u64)]) -> Vec<u8> {
    let distinct: Vec<_> = parts
        .iter()
        .enumerate()
        .map(|(i, &(f, _, d, n, lo, hi))| (f, 100 + i as u64, d, n, lo, hi))
        .collect();
    let mut b = root_with_parts(&distinct).encode().unwrap();
    for (i, p) in parts.iter().enumerate() {
        put(&mut b, PARTS + 80 * i + 8, &p.1.to_le_bytes());
    }
    reframe(&mut b);
    b
}

#[test]
fn parts_of_one_seq_overlapping_in_period_and_series_are_refused() {
    for (later, parts) in [
        (1, vec![(3, 9, 0, 1, 7, 20), (5, 9, 0, 1, 20, 40)]),
        (1, vec![(3, 9, 0, 7, 7, 40), (5, 9, 6, 1, 0, 7)]),
        (
            2,
            vec![
                (3, 9, 0, 7, 0, 10),
                (4, 9, 0, 7, 20, 30),
                (5, 9, 2, 1, 10, 19),
            ],
        ),
        (
            2,
            vec![
                (3, 9, 2, 1, 11, 19),
                (4, 9, 0, 7, 20, 30),
                (5, 9, 0, 7, 0, 11),
            ],
        ),
    ] {
        assert!(root_with_parts(&parts).encode().is_err(), "{parts:?}");
        let e = Root::decode(&root_bytes_with_parts(&parts)).unwrap_err();
        assert_eq!(e.refusal, Refusal::PartOrder, "{parts:?}");
        assert_eq!(
            e.at,
            (PARTS + 80 * later) as u64,
            "{parts:?}: the later of the pair"
        );
    }
}

proptest::proptest! {
    #[test]
    fn parts_of_one_seq_are_refused_exactly_when_a_pair_overlaps_in_period_and_series(
        raw in proptest::collection::vec((0u64..2, 0i64..4, 1i64..3, 0u64..6, 0u64..6), 1..7)
    ) {
        let parts: Vec<_> = raw
            .iter()
            .enumerate()
            .map(|(i, &(seq, day, days, lo, hi))| (i as u64 + 1, seq, day, days, lo, hi))
            .collect();
        let clash = |a: &(u64, u64, i64, i64, u64, u64), b: &(u64, u64, i64, i64, u64, u64)| {
            a.1 == b.1 && a.2 < b.2 + b.3 && b.2 < a.2 + a.3 && a.4.max(b.4) <= a.5.min(b.5)
        };
        let any = parts
            .iter()
            .enumerate()
            .any(|(i, a)| parts[..i].iter().any(|b| clash(a, b)));
        let got = root_refusal(&root_bytes_with_parts(&parts));
        proptest::prop_assert_eq!(got, if any { Err(Refusal::PartOrder) } else { Ok(()) });
    }
}

/// A part whose first block claims a body above 16 MiB that fits before the trailer.
fn body_above_16_mib() -> Vec<u8> {
    let (h, blocks) = split(&empty_part().encode().unwrap());
    let body = vec![0u8; (16 << 20) + 8];
    let mut prefix = h;
    let bh = kvik_log::block::BlockHeader {
        block_type: block_type::NODES,
        flags: 0,
        body_len: body.len() as u32,
        body_crc32c: kvik_log::crc::crc32c(&body),
    };
    prefix.extend_from_slice(&bh.encode());
    prefix.extend_from_slice(&body);
    assemble(&prefix, &blocks)
}

fn with_tail_offset(at: u64) -> Vec<u8> {
    let mut b = day();
    let n = b.len();
    put(&mut b, n - 16, &at.to_le_bytes());
    b
}

#[test]
fn each_condition_of_a_refusal_has_a_vector() {
    use File::*;
    use Refusal as R;
    let q = 15 * MINUTE;
    let valid = |start| (0, 0, 0, 0, gauge(start, 3));
    let tail = day().len() as u64 - 16;
    let far = (i64::MAX - T0) as u64 / MINUTE as u64;
    let cases: Vec<(&str, Refusal, File, Vec<u8>)> = vec![
        (
            "entry before the period",
            R::OutsidePeriod,
            Part,
            one(T0 - q, &[valid(T0 - q)]),
        ),
        (
            "runs: series descending",
            R::Order,
            Part,
            with_nodes(vec![
                nodes_body(2, 2, T0, pos(3, 5), &[valid(T0)]),
                nodes_body(1, 2, T0, pos(3, 5), &[valid(T0)]),
            ]),
        ),
        (
            "runs: level descending",
            R::Order,
            Part,
            with_nodes(vec![
                nodes_body(1, 3, T0, pos(3, 5), &[(0, 0, 0, 0, counter(T0, width(3)))]),
                nodes_body(1, 2, T0, pos(3, 5), &[(0, 0, 0, 0, counter(T0, q))]),
            ]),
        ),
        (
            "trailer offset inside the header",
            R::TrailerOffset,
            Part,
            with_tail_offset(8),
        ),
        (
            "trailer offset at the tail",
            R::TrailerOffset,
            Part,
            with_tail_offset(tail),
        ),
        (
            "trailer block not ending at the tail",
            R::TrailerOffset,
            Part,
            with_tail_offset(64),
        ),
        (
            "body above 16 MiB inside the file",
            R::BodyLength,
            Part,
            body_above_16_mib(),
        ),
        (
            "gap beyond i64 nanoseconds",
            R::Start,
            Part,
            one(T0, &[valid(T0), (u64::MAX, 0, 0, 0, gauge(T0 + q, 3))]),
        ),
        (
            "gap past i64 from the previous end",
            R::Start,
            Part,
            one(T0, &[valid(T0), (far, 0, 0, 0, gauge(T0 + q, 3))]),
        ),
        (
            "level id zero",
            R::LevelId,
            Part,
            edit_trailer(|b| b[104] = 0),
        ),
        (
            "level width negative",
            R::LevelWidth,
            Part,
            edit_trailer(|b| put(b, 104 + 8, &(-MINUTE).to_le_bytes())),
        ),
        (
            "level width zero",
            R::LevelWidth,
            Part,
            edit_trailer(|b| put(b, 104 + 8, &0i64.to_le_bytes())),
        ),
        (
            "part period ending beyond i64",
            R::Period,
            Part,
            edit_trailer(|b| put(b, 16, &(i64::MAX - DAY + 1).to_le_bytes())),
        ),
        (
            "root part period ending beyond i64",
            R::Period,
            Root,
            edit_root(|b| put(b, PARTS + 24, &i64::MAX.to_le_bytes())),
        ),
        (
            "covered range empty",
            R::CoveredRange,
            Root,
            edit_root(|b| put(b, COVERED + 16, &(T0 - 35 * DAY).to_le_bytes())),
        ),
        (
            "retired file_id twice",
            R::RetiredOrder,
            Root,
            edit_root(|b| put(b, RETIRED + 16, &4u64.to_le_bytes())),
        ),
        (
            "retired file_id at next_file_id",
            R::FileId,
            Root,
            edit_root(|b| put(b, RETIRED + 16, &20u64.to_le_bytes())),
        ),
        (
            "two parts of one seq overlapping",
            R::PartOrder,
            Root,
            edit_root(|b| put(b, PARTS + 80 + 8, &4u64.to_le_bytes())),
        ),
    ];
    let mut wrong = Vec::new();
    for (name, want, file, bytes) in cases {
        let got = match file {
            File::Part => part_refusal(&bytes),
            File::Root => root_refusal(&bytes),
        };
        if got != Err(want) {
            wrong.push((name, want, got));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// T.11: a refusal names its offset. Offsets of the trailer's fields are the day part's trailer body (152,568)
/// plus the T.11 offset; of the root's records, the T.12 offsets from the body at 80.
#[test]
fn every_vector_is_refused_at_its_offset() {
    use Refusal as R;
    let want: &[(Refusal, u64)] = &[
        (R::Header, 0),
        (R::Kind, 4),
        (R::NewerMajor, 5),
        (R::OlderMajor, 5),
        (R::HeaderReserved, 8),
        (R::EndMarker, 152744),
        (R::TrailerOffset, 152736),
        (R::BlockHeader, 152552),
        (R::BlockReserved, 64),
        (R::BodyLength, 68),
        (R::BodyChecksum, 72),
        (R::Padding, 199),
        (R::UnknownBlock, 64),
        (R::BlockOrder, 64),
        (R::Truncated, 107),
        (R::NotShortest, 80),
        (R::OutOfRange, 80),
        (R::Reserved, 152577),
        (R::RecordSize, 152580),
        (R::RecordSizeSmall, 152580),
        (R::ArrayLength, 152672),
        (R::LevelTableEmpty, 152672),
        (R::LevelId, 152688),
        (R::LevelWidth, 152680),
        (R::UnknownClass, 152576),
        (R::Period, 152584),
        (R::FileChecksum, 152664),
        (R::EmptyBlock, 102),
        (R::UnknownLevel, 81),
        (R::BaseStart, 102),
        (R::Start, 102),
        (R::Alignment, 102),
        (R::EntryFlag, 103),
        (R::AsOf, 201),
        (R::BasePosition, 80),
        (R::NodeForm, 107),
        (R::Node, 192),
        (R::NodeSpan, 107),
        (R::NodeEmpty, 107),
        (R::NodeKind, 64),
        (R::Order, 200),
        (R::OutsidePeriod, 64),
        (R::Split, 68),
        (R::Directory, 152144),
        (R::DirectoryRoot, 152504),
        (R::Summary, 152560),
        (R::PartOrder, 312),
        (R::FileId, 392),
        (R::CoveredRange, 528),
        (R::FloorOrder, 592),
        (R::FloorFlag, 584),
        (R::RetiredOrder, 616),
        (R::RootSeq, 16),
    ];
    let got: Vec<(Refusal, u64)> = vectors()
        .into_iter()
        .map(|(r, file, bytes)| {
            let e = match file {
                File::Part => Part::decode(&bytes).map(|_| ()),
                File::Root => Root::decode(&bytes).map(|_| ()),
            };
            (r, e.unwrap_err().at)
        })
        .collect();
    assert_eq!(got, want);
}
