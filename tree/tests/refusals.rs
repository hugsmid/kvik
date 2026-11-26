//! Every refusal of `format-tree.md` T.11 and T.12 has a named vector here, and the table covers [`Refusal::ALL`].

mod files;

use std::collections::HashSet;

use files::*;
use kvik_tree::fuzz::reframe;
use kvik_tree::part::{Part, block_type};
use kvik_tree::root::Root;
use kvik_tree::{Kind, MINUTE, Node, Refusal, encode};

enum File {
    Part,
    Root,
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

fn day() -> Vec<u8> {
    day_part().encode().unwrap()
}

fn root_bytes() -> Vec<u8> {
    sample_root().encode().unwrap()
}

fn trailer_at(b: &[u8]) -> usize {
    u64::from_le_bytes(b[b.len() - 16..b.len() - 8].try_into().unwrap()) as usize
}

/// The day part with one block body changed, everything after it laid out again.
fn edit_block(which: impl Fn(u8, usize) -> bool, f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    let (h, mut blocks) = split(&day());
    let mut seen = std::collections::HashMap::new();
    for (t, body) in &mut blocks {
        let n = seen.entry(*t).or_insert(0usize);
        if which(*t, *n) {
            f(body);
        }
        *n += 1;
    }
    assemble(&h, &blocks)
}

fn edit_trailer(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    edit_block(|t, _| t == block_type::TRAILER, f)
}

fn first_nodes(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    edit_block(|t, n| t == block_type::NODES && n == 0, f)
}

fn edit_root(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = root_bytes();
    f(&mut b);
    reframe(&mut b);
    b
}

fn header(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = day();
    f(&mut b);
    reframe(&mut b);
    b
}

const ROOT_BODY: usize = 80;
const PARTS: usize = ROOT_BODY + 88 + 64;
const COVERED: usize = PARTS + 3 * 80;
const FLOORS: usize = COVERED + 4 * 24;
const RETIRED: usize = FLOORS + 2 * 24;

fn gauge(start: i64, n: u32) -> Vec<u8> {
    encode(&node(Kind::Gauge, start, 15 * MINUTE, n, 1)).unwrap()
}

fn one(base_start: i64, entries: &[RawEntry]) -> Vec<u8> {
    with_nodes(vec![nodes_body(1, 2, base_start, pos(3, 5), entries)])
}

fn vectors() -> Vec<(Refusal, File, Vec<u8>)> {
    use File::*;
    use Refusal as R;
    let q = 15 * MINUTE;
    let valid = |start| (0, 0, 0, 0, gauge(start, 3));
    vec![
        (R::Header, Part, {
            let mut b = day();
            b[0] = b'X';
            b
        }),
        (R::Kind, Part, header(|b| b[4] = 19)),
        (R::NewerMajor, Part, header(|b| b[5] = 2)),
        (R::OlderMajor, Part, header(|b| b[5] = 0)),
        (R::HeaderReserved, Part, header(|b| b[57] = 1)),
        (R::EndMarker, Part, {
            let mut b = day();
            *b.last_mut().unwrap() = 1;
            b
        }),
        (R::TrailerOffset, Part, {
            let mut b = day();
            let t = trailer_at(&b) as u64 + 4;
            let n = b.len();
            put(&mut b, n - 16, &t.to_le_bytes());
            b
        }),
        (R::BlockHeader, Part, {
            let mut b = day();
            let t = trailer_at(&b);
            b[t + 12] ^= 1;
            b
        }),
        (R::BlockReserved, Part, header(|b| b[64 + 2] = 1)),
        (R::BodyLength, Part, {
            let mut b = day();
            put(&mut b, 64 + 4, &(16u32 << 20 | 8).to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[64..76]);
            put(&mut b, 76, &crc.to_le_bytes());
            let (h, blocks) = split(&day());
            let fixed = assemble(&h, &blocks);
            let t = trailer_at(&fixed);
            let crc = kvik_log::crc::crc32c(&b[..t]);
            put(&mut b, t + 16 + 96, &crc.to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[t + 16..b.len() - 16]);
            put(&mut b, t + 8, &crc.to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[t..t + 12]);
            put(&mut b, t + 12, &crc.to_le_bytes());
            b
        }),
        (R::BodyChecksum, Part, {
            let mut b = day();
            b[64 + 16 + 30] ^= 1;
            let t = trailer_at(&b);
            let crc = kvik_log::crc::crc32c(&b[..t]);
            put(&mut b, t + 16 + 96, &crc.to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[t + 16..b.len() - 16]);
            put(&mut b, t + 8, &crc.to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[t..t + 12]);
            put(&mut b, t + 12, &crc.to_le_bytes());
            b
        }),
        (R::Padding, Part, {
            let entry = (0, 0, 0, 0, gauge(T0, 2));
            let mut body = nodes_body(1, 2, T0, pos(3, 5), &[entry]);
            while body.len() % 8 != 7 {
                body = nodes_body(
                    1,
                    2,
                    T0,
                    pos(3, 5),
                    &[(0, 0, 0, 0, gauge(T0, 2 + body.len() as u32 % 5))],
                );
            }
            let mut b = with_nodes(vec![body.clone()]);
            b[64 + 16 + body.len()] = 1;
            reframe(&mut b);
            b
        }),
        (R::UnknownBlock, Part, header(|b| b[64] = 0x70)),
        (R::BlockOrder, Part, header(|b| b[64] = block_type::TRAILER)),
        (R::Truncated, Part, first_nodes(|b| b.truncate(60))),
        (
            R::NotShortest,
            Part,
            first_nodes(|b| {
                b[0] |= 0x80;
                b.insert(1, 0);
            }),
        ),
        (
            R::OutOfRange,
            Part,
            first_nodes(|b| {
                b.splice(0..1, [0xFF; 9].into_iter().chain([0x02]));
            }),
        ),
        (R::Reserved, Part, edit_trailer(|b| b[9] = 1)),
        (R::RecordSize, Part, edit_trailer(|b| b[12] = 24)),
        (R::RecordSizeSmall, Part, edit_trailer(|b| b[12] = 8)),
        (R::ArrayLength, Part, edit_trailer(|b| b[10] = 5)),
        (
            R::LevelTableEmpty,
            Part,
            edit_trailer(|b| {
                b[10] = 0;
                b.truncate(104);
            }),
        ),
        (R::LevelId, Part, edit_trailer(|b| b[104 + 16] = 1)),
        (
            R::LevelWidth,
            Part,
            edit_trailer(|b| put(b, 104 + 8, &30_000_000_000i64.to_le_bytes())),
        ),
        (R::UnknownClass, Part, edit_trailer(|b| b[8] = 3)),
        (
            R::Period,
            Part,
            edit_trailer(|b| put(b, 24, &0u64.to_le_bytes())),
        ),
        (R::FileChecksum, Part, {
            let mut b = day();
            let t = trailer_at(&b);
            b[t + 16 + 96] ^= 1;
            let crc = kvik_log::crc::crc32c(&b[t + 16..b.len() - 16]);
            put(&mut b, t + 8, &crc.to_le_bytes());
            let crc = kvik_log::crc::crc32c(&b[t..t + 12]);
            put(&mut b, t + 12, &crc.to_le_bytes());
            b
        }),
        (R::EmptyBlock, Part, one(T0, &[])),
        (R::UnknownLevel, Part, first_nodes(|b| b[1] = 9)),
        (R::BaseStart, Part, first_nodes(|b| b[22] = 1)),
        (R::Start, Part, one(i64::MAX / q * q, &[valid(T0)])),
        (R::Alignment, Part, one(T0 + MINUTE, &[valid(T0)])),
        (R::EntryFlag, Part, first_nodes(|b| b[23] = 2)),
        (
            R::AsOf,
            Part,
            with_nodes(vec![nodes_body(
                1,
                2,
                T0,
                pos(3, u64::MAX),
                &[valid(T0), (0, 0, 0, 1, gauge(T0 + q, 3))],
            )]),
        ),
        (R::BasePosition, Part, first_nodes(|b| b[25] = 1)),
        (R::NodeForm, Part, first_nodes(|b| b[27] = 0x11)),
        (R::Node, Part, first_nodes(|b| b[26] -= 1)),
        (
            R::NodeSpan,
            Part,
            one(T0, &[(0, 0, 0, 0, gauge(T0 + q, 3))]),
        ),
        (
            R::NodeEmpty,
            Part,
            one(
                T0,
                &[(
                    0,
                    0,
                    0,
                    0,
                    encode(&Node::empty(Kind::Gauge, T0, T0 + q)).unwrap(),
                )],
            ),
        ),
        (
            R::NodeKind,
            Part,
            one(
                T0,
                &[
                    valid(T0),
                    (
                        0,
                        0,
                        0,
                        0,
                        encode(&node(Kind::Counter, T0 + q, q, 3, 1)).unwrap(),
                    ),
                ],
            ),
        ),
        (
            R::Order,
            Part,
            with_nodes(vec![
                nodes_body(1, 2, T0 + q, pos(3, 5), &[valid(T0 + q)]),
                nodes_body(1, 2, T0, pos(3, 5), &[valid(T0)]),
            ]),
        ),
        (R::OutsidePeriod, Part, one(T0 + DAY, &[valid(T0 + DAY)])),
        (
            R::Split,
            Part,
            with_nodes(vec![
                nodes_body(1, 2, T0, pos(3, 5), &[valid(T0)]),
                nodes_body(1, 2, T0 + q, pos(3, 5), &[valid(T0 + q)]),
            ]),
        ),
        (
            R::Directory,
            Part,
            edit_block(|t, _| t == block_type::DIRECTORY, |b| b[8 + 20] ^= 1),
        ),
        (
            R::DirectoryRoot,
            Part,
            edit_block(|t, _| t == block_type::DIRECTORY_ROOT, |b| b[8 + 24] ^= 8),
        ),
        (R::Summary, Part, edit_trailer(|b| b[48] = 8)),
        (
            R::PartOrder,
            Root,
            edit_root(|b| put(b, PARTS + 80, &3u64.to_le_bytes())),
        ),
        (
            R::FileId,
            Root,
            edit_root(|b| put(b, ROOT_BODY + 40, &18u64.to_le_bytes())),
        ),
        (
            R::CoveredRange,
            Root,
            edit_root(|b| put(b, COVERED + 48 + 8, &(T0 - 10 * DAY).to_le_bytes())),
        ),
        (
            R::FloorOrder,
            Root,
            edit_root(|b| put(b, FLOORS + 24, &7u64.to_le_bytes())),
        ),
        (R::FloorFlag, Root, edit_root(|b| b[FLOORS + 16] = 2)),
        (
            R::RetiredOrder,
            Root,
            edit_root(|b| put(b, RETIRED, &3u64.to_le_bytes())),
        ),
        (R::RootSeq, Root, edit_root(|b| b[16] = 13)),
    ]
}

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
    for parts in [
        vec![(3, 9, 0, 1, 7, 20), (5, 9, 0, 1, 20, 40)],
        vec![(3, 9, 0, 7, 7, 40), (5, 9, 6, 1, 0, 7)],
        vec![
            (3, 9, 0, 7, 0, 10),
            (4, 9, 0, 7, 20, 30),
            (5, 9, 2, 1, 10, 19),
        ],
        vec![
            (3, 9, 2, 1, 11, 19),
            (4, 9, 0, 7, 20, 30),
            (5, 9, 0, 7, 0, 11),
        ],
    ] {
        assert!(root_with_parts(&parts).encode().is_err(), "{parts:?}");
        assert_eq!(
            root_refusal(&root_bytes_with_parts(&parts)),
            Err(Refusal::PartOrder),
            "{parts:?}"
        );
    }
}
