//! The scan bound (`format.md` A, common rules; Q-110): the CRC shift and prefix table against direct checksums,
//! and the bounded scan against the reference scan on damaged segments.

mod support;

use kvik_log::FormatError;
use kvik_log::batch::BatchBuilder;
use kvik_log::block::FLAG_SKIPPABLE;
use kvik_log::crc::{PrefixCrc, crc32c, crc32c_shift};
use kvik_log::scan::{ScanLimit, ScanReport, legacy, scan_with_stats};
use kvik_log::segment::SegmentImage;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use support::*;

proptest! {
    #[test]
    fn shifted_crc_of_a_prefix_combines_with_the_suffix_to_the_direct_crc(
        a in prop::collection::vec(any::<u8>(), 0..300),
        b in prop::collection::vec(any::<u8>(), 0..300),
    ) {
        let whole = crc32c(&[a.as_slice(), b.as_slice()].concat());
        prop_assert_eq!(crc32c_shift(crc32c(&a), b.len() as u64) ^ crc32c(&b), whole);
        prop_assert_eq!(whole ^ crc32c_shift(crc32c(&a), b.len() as u64), crc32c(&b));
    }

    #[test]
    fn a_prefix_table_range_equals_the_direct_crc(
        bytes in prop::collection::vec(any::<u8>(), 0..600),
        ranges in prop::collection::vec((0usize..80, 0usize..300), 1..12),
    ) {
        let mut t = PrefixCrc::new(&bytes);
        for (s, len) in ranges {
            let direct = bytes.get(8 * s..8 * s + len).map(crc32c);
            prop_assert_eq!(t.range((8 * s) as u64, len as u64), direct);
        }
    }
}

/// The scan with chain ends read ahead (Q-873), at its own block and at blocks small enough for these files to cross,
/// gives `scan`'s report, and so do the walked chains, which walk each candidate's chain.
fn ends_agree(
    f: &[u8],
    limit: ScanLimit,
    scan: &Result<ScanReport<'_>, FormatError>,
) -> Result<(), TestCaseError> {
    let (walked, _) = legacy::scan_with_walked_links(f, &ID, limit);
    prop_assert_eq!(&walked, scan, "walked, {:?}", limit);
    // Each block read passes over the starts up to a batch back, so small blocks are kept to small files.
    let len = f.len() as u64;
    for span in [8, 24, 64, 1_024, 65_536, 1 << 20] {
        if (len / span) * (len.min(9 << 20) / 8) > 1 << 25 {
            continue;
        }
        let (ends, stats) = legacy::scan_with_span(f, &ID, limit, span);
        prop_assert_eq!(&ends, scan, "span {}, {:?}", span, limit);
        prop_assert!(stats.within_bound(f.len() as u64), "{:?}", stats);
    }
    Ok(())
}

/// One batch of a generated segment.
#[derive(Clone, Debug)]
enum Piece {
    Facts(u8),
    Entries,
    /// A facts block, then a skippable block whose body is a whole batch image at its own offset.
    Image(u8),
    /// A facts block, then an unknown critical block.
    Critical,
    /// A facts block, then this many skippable blocks.
    Wide(u8),
}

#[derive(Clone, Debug)]
enum Damage {
    Flip(prop::sample::Index, u8),
    Word(prop::sample::Index, u64),
    Zero(prop::sample::Index, u16),
    Copy(prop::sample::Index, prop::sample::Index, u16),
    Cut(prop::sample::Index),
}

fn piece() -> impl Strategy<Value = Piece> {
    prop_oneof![
        8 => (1u8..4).prop_map(Piece::Facts),
        4 => Just(Piece::Entries),
        6 => (0u8..3).prop_map(Piece::Image),
        1 => Just(Piece::Critical),
        3 => (2u8..5).prop_map(Piece::Wide),
    ]
}

fn damage() -> impl Strategy<Value = Damage> {
    let i = any::<prop::sample::Index>;
    prop_oneof![
        (i(), any::<u8>()).prop_map(|(a, b)| Damage::Flip(a, b)),
        (i(), any::<u64>()).prop_map(|(a, w)| Damage::Word(a, w)),
        (i(), 1u16..300).prop_map(|(a, n)| Damage::Zero(a, n)),
        (i(), i(), 8u16..600).prop_map(|(a, b, n)| Damage::Copy(a, b, n)),
        i().prop_map(Damage::Cut),
    ]
}

fn facts(seq: u64, rows: u8) -> BatchBuilder {
    let rows: Vec<_> = (0..rows)
        .map(|i| (1, T0 + i64::from(i) * SECOND, f64::from(i)))
        .collect();
    facts_batch(seq, T0, &rows)
}

/// A segment of `pieces`, sealed or not, with `damage` applied.
fn build(pieces: &[Piece], seal: Option<bool>, damage: &[Damage]) -> Vec<u8> {
    let mut s = SegmentImage::create_ahead(ID, T0, 16_384).unwrap();
    s.take_into_use(1).unwrap();
    let mut seq = 1;
    for (n, p) in pieces.iter().enumerate() {
        let t = T0 + n as i64;
        let mut skip = 0;
        let b = match p {
            Piece::Facts(rows) => facts(seq, *rows),
            Piece::Entries => entries_batch(seq, T0, &[alias(1, "a")]),
            Piece::Image(rows) => {
                let mut b = facts(seq, 1);
                let at = s.write_offset() + b.data_len() + 16;
                let inner = facts(seq + 1, rows + 1);
                skip = u64::from(rows + 1);
                let inner = inner.encode(&ID, at, t).unwrap();
                b.push_unknown(0x50, FLAG_SKIPPABLE, inner).unwrap();
                b
            }
            Piece::Critical => {
                let mut b = facts(seq, 1);
                b.push_unknown(0x30, 0, vec![7; 12]).unwrap();
                b
            }
            Piece::Wide(k) => {
                let mut b = facts(seq, 1);
                for i in 0..*k {
                    b.push_unknown(0x50, FLAG_SKIPPABLE, vec![i; 8 * usize::from(i) + 4])
                        .unwrap();
                }
                b
            }
        };
        seq = b.sequences().unwrap().1 + 1 + skip;
        if s.append(&b, t).is_err() {
            break;
        }
    }
    if let Some(set_len) = seal
        && let Ok(end) = s.write_seal_blocks(vec![])
        && set_len
    {
        s.set_len(end);
    }
    let mut f = s.into_bytes();
    apply(&mut f, damage);
    f
}

fn apply(f: &mut Vec<u8>, damage: &[Damage]) {
    for d in damage {
        let len = f.len();
        if len == 0 {
            break;
        }
        match d {
            Damage::Flip(i, bit) => f[i.index(len)] ^= 1 << (bit % 8),
            Damage::Word(i, w) => {
                let at = i.index(len) & !7;
                let n = (len - at).min(8);
                f[at..at + n].copy_from_slice(&w.to_le_bytes()[..n]);
            }
            Damage::Zero(i, n) => {
                let at = i.index(len);
                let end = (at + usize::from(*n)).min(len);
                f[at..end].fill(0);
            }
            Damage::Copy(a, b, n) => {
                let (from, to) = (a.index(len) & !7, b.index(len) & !7);
                let n = usize::from(*n).min(len - from).min(len - to);
                f.copy_within(from..from + n, to);
            }
            Damage::Cut(i) => f.truncate(i.index(len + 1)),
        }
    }
}

fn agree(f: &[u8], mark: prop::sample::Index) -> Result<(), TestCaseError> {
    agree_at(f, (mark.index(f.len() + 1) as u64) & !7)
}

fn agree_at(f: &[u8], mark: u64) -> Result<(), TestCaseError> {
    let len = f.len() as u64;
    for limit in [ScanLimit::End, ScanLimit::Mark(mark)] {
        let (new, stats) = scan_with_stats(f, &ID, limit);
        let (old, _) = legacy::scan(f, &ID, limit);
        prop_assert_eq!(&new, &old, "{:?}", limit);
        prop_assert!(stats.within_bound(len), "{:?} for {} bytes", stats, len);
        ends_agree(f, limit, &new)?;
    }
    let (new, stats) = legacy::scan_ignoring_seal(f, &ID, false);
    let (old, _) = legacy::scan_ignoring_seal(f, &ID, true);
    prop_assert_eq!(new, old, "ignoring the seal");
    prop_assert!(stats.within_bound(len), "{:?} for {} bytes", stats, len);
    Ok(())
}

proptest! {
    #[test]
    fn the_bounded_scan_agrees_with_the_reference_scan_on_damaged_segments(
        pieces in prop::collection::vec(piece(), 1..12),
        seal in prop::option::of(any::<bool>()),
        damage in prop::collection::vec(damage(), 0..6),
        mark in any::<prop::sample::Index>(),
    ) {
        agree(&build(&pieces, seal, &damage), mark)?;
    }
}

/// A batch of a facts block and a skippable block holding a valid batch image at its own offset, `over` bytes past
/// the 8 MiB of data a batch may hold (negative: below it). Returns the next sequence.
/// A batch of `over` bytes past the 8 MiB limit: a facts block and a skippable one, or one block of an unknown type
/// that is not skippable (`one`).
fn big_batch(f: &mut Vec<u8>, seq: u64, over: i64, t: i64, one: bool) -> u64 {
    use kvik_log::batch::MAX_BATCH_DATA;
    use kvik_log::block::block_type;
    let at = f.len() as u64;
    let first = facts_body(seq, T0, &[(1, T0, 0.0)]);
    let head = if one {
        16
    } else {
        16 + first.len().div_ceil(8) as u64 * 8 + 16
    };
    let mut filler = vec![0x11u8; (MAX_BATCH_DATA as i64 + over - head as i64) as usize];
    let inner_at = at + head + 4 * 4_096;
    let inner = facts(seq + 1, 2).encode(&ID, inner_at, t).unwrap();
    filler[4 * 4_096..4 * 4_096 + inner.len()].copy_from_slice(&inner);
    let blocks = if one {
        vec![(0x50, 0, filler)]
    } else {
        vec![
            (block_type::FACTS, 0, first),
            (0x50, FLAG_SKIPPABLE, filler),
        ]
    };
    f.extend_from_slice(&raw_batch(&ID, at, &blocks, seq, seq, t));
    seq + 3
}

/// Small batches around two batches near the 8 MiB limit, the batch before each damaged: the first is met by a
/// boundary read once the scan has found damage, the second by resynchronisation.
fn near_limit(over: [i64; 2], damage: &[Damage]) -> Vec<u8> {
    near_limit_with(over, false, damage)
}

/// [`near_limit`], the second batch near the limit one block that is not skippable when `one`.
fn near_limit_with(over: [i64; 2], one: bool, damage: &[Damage]) -> Vec<u8> {
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(kvik_log::header::HEADER_LEN);
    let mut seq = 1;
    let small = |f: &mut Vec<u8>, seq: &mut u64, damaged: bool| {
        let at = f.len();
        f.extend_from_slice(&facts(*seq, 2).encode(&ID, at as u64, T0).unwrap());
        if damaged {
            let last = f.len() - 1;
            f[last - 8] ^= 1;
        }
        *seq += 2;
    };
    small(&mut f, &mut seq, false);
    small(&mut f, &mut seq, true);
    small(&mut f, &mut seq, false);
    seq = big_batch(&mut f, seq, over[0], T0, false);
    small(&mut f, &mut seq, true);
    seq = big_batch(&mut f, seq, over[1], T0, one);
    small(&mut f, &mut seq, false);
    f.resize(f.len() + 4_096, 0);
    apply(&mut f, damage);
    f
}

/// A runner for a property whose cases each scan megabytes: a 64th of the configured cases, from `least` to 160, so
/// that the deep runs fit their time limit (`.github/workflows/ci.yml`, the `deep` job). `PROPTEST_CASES` overrides a
/// `proptest!` block's own case count, so these properties build their runner themselves.
fn heavy(least: u32) -> TestRunner {
    TestRunner::new(ProptestConfig {
        cases: (ProptestConfig::default().cases / 64).clamp(least, 160),
        failure_persistence: None,
        ..ProptestConfig::default()
    })
}

#[test]
fn the_bounded_scan_agrees_with_the_reference_scan_across_the_batch_and_pruning_limits() {
    let over = prop::sample::select(vec![-16i64, -8, 0, 8]);
    heavy(12)
        .run(
            &(
                [over.clone(), over],
                prop::collection::vec(damage(), 0..3),
                any::<prop::sample::Index>(),
            ),
            |(over, damage, mark)| agree(&near_limit(over, &damage), mark),
        )
        .unwrap();
}

#[test]
fn batches_of_exactly_the_limit_are_kept_and_a_word_over_it_is_not() {
    use kvik_log::batch::MAX_BATCH_DATA;
    for (over, at_limit) in [([0, 0], 2), ([8, 0], 1), ([0, 8], 1), ([8, 8], 0)] {
        let f = near_limit(over, &[]);
        agree_at(&f, (f.len() as u64 / 2) & !7).unwrap();
        let (r, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
        let r = r.unwrap();
        let kept = r
            .batches
            .iter()
            .filter(|b| b.commit.batch_bytes == MAX_BATCH_DATA);
        assert_eq!(kept.count(), at_limit, "{over:?}");
        assert!(
            r.batches
                .iter()
                .all(|b| b.commit.batch_bytes <= MAX_BATCH_DATA)
        );
        assert!(stats.within_bound(f.len() as u64), "{stats:?}");
    }
}

#[test]
fn a_critical_block_of_exactly_the_limit_found_by_resynchronisation_refuses_the_segment() {
    for (over, refused) in [(0, true), (8, false)] {
        let f = near_limit_with([8, over], true, &[]);
        agree_at(&f, (f.len() as u64 / 2) & !7).unwrap();
        let r = scan_with_stats(&f, &ID, ScanLimit::End).0;
        assert_eq!(r.is_err(), refused, "{over}: {:?}", r.map(|r| r.regions));
    }
}

#[test]
fn a_commit_block_alone_at_the_first_damage_is_a_readable_commit_of_the_tail() {
    use kvik_log::scan::Tail;
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(kvik_log::header::HEADER_LEN);
    let at = f.len();
    f.extend_from_slice(&facts(1, 2).encode(&ID, at as u64, T0).unwrap());
    let pos = f.len();
    let commit = f[pos - 64..].to_vec();
    f.extend_from_slice(&commit);
    f.resize(f.len() + 4_096, 0);
    agree_at(&f, pos as u64).unwrap();
    let r = scan_with_stats(&f, &ID, ScanLimit::End).0.unwrap();
    let Some(Tail::Torn { start, commits, .. }) = r.tail else {
        panic!("{:?}", r.tail);
    };
    assert_eq!(start, pos as u64);
    assert_eq!(
        commits.iter().map(|c| c.offset).collect::<Vec<_>>(),
        [pos as u64]
    );
}

#[test]
fn a_header_failing_its_checksum_a_ring_above_a_valid_one_is_not_taken_as_valid() {
    use kvik_log::batch::MAX_BATCH_DATA;
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(kvik_log::header::HEADER_LEN);
    let mut seq = 1;
    let small = |f: &mut Vec<u8>, seq: &mut u64, damage: Option<usize>| {
        let at = f.len();
        f.extend_from_slice(&facts(*seq, 2).encode(&ID, at as u64, T0).unwrap());
        if let Some(d) = damage {
            f[at + d] ^= 1;
        }
        *seq += 2;
        f.len() - at
    };
    small(&mut f, &mut seq, None);
    let len = small(&mut f, &mut seq, Some(40));
    let ring = 16 << 20;
    let x = f.len();
    for _ in 0..100 {
        small(&mut f, &mut seq, None);
    }
    let rest = (x + ring - len - f.len()) as i64;
    let first = (rest / 2) & !7;
    for total in [first, rest - first] {
        seq = big_batch(&mut f, seq, total - 64 - MAX_BATCH_DATA as i64, T0, false);
    }
    small(&mut f, &mut seq, Some(40));
    let b = f.len();
    assert_eq!(b, x + ring);
    small(&mut f, &mut seq, Some(12));
    for _ in 0..4 {
        small(&mut f, &mut seq, None);
    }
    f.resize(f.len() + 4_096, 0);
    agree_at(&f, b as u64).unwrap();
    let r = scan_with_stats(&f, &ID, ScanLimit::End).0.unwrap();
    assert!(r.batches.iter().any(|k| k.offset == x as u64));
    assert!(r.batches.iter().all(|k| k.offset != b as u64));
}

#[test]
fn a_boundary_read_a_ring_above_the_valid_headers_checks_the_header_checksum() {
    use kvik_log::batch::MAX_BATCH_DATA;
    let mut s = SegmentImage::create_ahead(ID, T0, 4_096).unwrap();
    s.take_into_use(1).unwrap();
    let mut f = s.into_bytes();
    f.truncate(kvik_log::header::HEADER_LEN);
    let mut seq = 1;
    let small = |f: &mut Vec<u8>, seq: &mut u64| {
        let at = f.len();
        f.extend_from_slice(&facts(*seq, 2).encode(&ID, at as u64, T0).unwrap());
        *seq += 2;
    };
    while f.len() < 1 << 17 {
        small(&mut f, &mut seq);
    }
    f.extend_from_slice(&[0xff; 8]);
    let x = f.len();
    let ring = 16 << 20;
    let rest = (x + ring - f.len()) as i64;
    let first = (rest / 2) & !7;
    for total in [first, rest - first] {
        seq = big_batch(&mut f, seq, total - 64 - MAX_BATCH_DATA as i64, T0, false);
    }
    let b = f.len();
    assert_eq!(b, x + ring);
    small(&mut f, &mut seq);
    f[b + 12] ^= 1;
    for _ in 0..4 {
        small(&mut f, &mut seq);
    }
    f.resize(f.len() + 4_096, 0);
    agree_at(&f, b as u64).unwrap();
    let r = scan_with_stats(&f, &ID, ScanLimit::End).0.unwrap();
    assert!(r.batches.iter().any(|k| k.offset == x as u64));
    assert!(r.batches.iter().all(|k| k.offset != b as u64));
}

/// An engine-shaped segment of one window: about 7 MiB of batches, each an alias entry and a facts block of 20 rows
/// (`two`), or three facts blocks of 200 rows; returns the bytes and the end of the data.
fn engine_shaped(two: bool) -> &'static (Vec<u8>, usize) {
    use kvik_log::block::block_type;
    use std::sync::OnceLock;
    static CELLS: [OnceLock<(Vec<u8>, usize)>; 2] = [OnceLock::new(), OnceLock::new()];
    CELLS[usize::from(two)].get_or_init(|| {
        let rows = if two { 20 } else { 200 };
        let total = 7u64 << 20;
        let mut s = SegmentImage::create_ahead(ID, T0, total + (1 << 20)).unwrap();
        s.take_into_use(1).unwrap();
        let mk = |seq: u64| -> Vec<(u32, i64, f64)> {
            (0..rows)
                .map(|i| {
                    (
                        1 + i % 7,
                        T0 + i64::from(i) * SECOND + seq as i64,
                        f64::from(i),
                    )
                })
                .collect()
        };
        let mut seq = 1u64;
        while s.write_offset() < total {
            let b = if two {
                let mut b = entries_batch(seq, T0, &[alias(1, "a")]);
                b.push(block_type::FACTS, facts_body(seq + 1, T0, &mk(seq)))
                    .unwrap();
                b
            } else {
                let mut b = facts_batch(seq, T0, &mk(seq));
                for k in 1..3 {
                    b.push(
                        block_type::FACTS,
                        facts_body(seq + k * u64::from(rows), T0, &mk(seq)),
                    )
                    .unwrap();
                }
                b
            };
            seq = b.sequences().unwrap().1 + 1;
            s.append(&b, T0).unwrap();
        }
        let end = s.write_offset() as usize;
        (s.into_bytes(), end)
    })
}

/// A damage site: a flipped bit, a zeroed 512-byte sector or a zeroed 4 KiB page.
#[derive(Clone, Copy, Debug)]
enum Site {
    Flip(u8),
    Sector,
    Page,
}

#[test]
fn the_bounded_scan_agrees_with_the_reference_scan_under_disjoint_damage_in_one_window() {
    let site = prop_oneof![
        any::<u8>().prop_map(Site::Flip),
        Just(Site::Sector),
        Just(Site::Page)
    ];
    let inputs = (
        any::<bool>(),
        site,
        prop::collection::vec(any::<prop::sample::Index>(), 1..=64),
        any::<prop::sample::Index>(),
    );
    heavy(16)
        .run(&inputs, |(two, site, at, mark)| {
            let (clean, end) = engine_shaped(two);
            let mut f = clean.clone();
            // One site in each of `at.len()` equal slices of the data, so the sites are disjoint by construction.
            let from = kvik_log::header::HEADER_LEN + 4_096;
            let slice = (end - from) / at.len();
            for (k, i) in at.iter().enumerate() {
                let p = from + k * slice + i.index(slice - 4_096);
                match site {
                    Site::Flip(bit) => f[p] ^= 1 << (bit % 8),
                    Site::Sector => f[p & !511..(p & !511) + 512].fill(0),
                    Site::Page => f[p & !4_095..(p & !4_095) + 4_096].fill(0),
                }
            }
            agree(&f, mark)
        })
        .unwrap();
}

#[test]
fn stale_complete_batches_ahead_of_kept_data_stay_under_the_budget() {
    use kvik_log::block::block_type;
    // A batch, then older complete batches at their own offsets (sequences below it, as left by a write at an
    // earlier offset over two windows), then a later batch: each stale batch fails only on its sequence.
    let mut f = vec![0u8; kvik_log::header::HEADER_LEN];
    let push = |f: &mut Vec<u8>, seq: u64| {
        let mut b = entries_batch(seq, T0, &[alias(1, "a")]);
        let rows: Vec<_> = (0..200).map(|i| (1, T0 + i * SECOND, 0.0)).collect();
        b.push(block_type::FACTS, facts_body(seq + 1, T0, &rows))
            .unwrap();
        let at = f.len() as u64;
        f.extend_from_slice(&b.encode(&ID, at, T0).unwrap());
    };
    push(&mut f, 1_000_000);
    let mut seq = 1;
    while f.len() < 15 << 20 {
        push(&mut f, seq);
        seq += 201;
    }
    let last = f.len() as u64;
    push(&mut f, 2_000_000);
    let (new, stats) = scan_with_stats(&f, &ID, ScanLimit::End);
    let (old, _) = legacy::scan(&f, &ID, ScanLimit::End);
    let new = new.unwrap();
    assert_eq!(Ok(&new), old.as_ref());
    assert_eq!(new.batches.last().map(|b| b.offset), Some(last));
    assert!(
        stats.failed_bytes_peak < kvik_log::scan::FAILED_READ_BUDGET,
        "{stats:?}"
    );
}

/// One block header of a crafted run (Q-873).
#[derive(Clone, Debug)]
enum Head {
    /// A data header of type `kind` (an index into [`DATA_TYPES`]) whose block is `steps` 8-byte steps long, its body
    /// `short` bytes short of that, and its checksum wrong.
    Data {
        kind: u8,
        steps: u16,
        short: u8,
    },
    /// A data header claiming a block longer than a batch, or about one.
    Long(i8),
    /// A data header whose block ends where the next header of the run starts.
    Link(u8),
    /// A commit block, of the commit length or not; `named` picks a start whose chain ends here, if any, else
    /// `batch_bytes` is `back` 8-byte steps, 0 included, or past a batch when `back` is 1,000 or more; a byte more if
    /// `odd`.
    Commit {
        exact: bool,
        named: prop::sample::Index,
        back: u16,
        odd: bool,
    },
    /// Bytes that are no header.
    Junk(u64),
    Zero,
}

const DATA_TYPES: [u8; 6] = [
    kvik_log::block::block_type::FACTS,
    kvik_log::block::block_type::ENTRIES,
    0x50,
    0x30,
    kvik_log::block::block_type::SEGMENT_INDEX,
    kvik_log::block::block_type::TRAILER,
];

fn head() -> impl Strategy<Value = Head> {
    prop_oneof![
        8 => (0u8..6, 2u16..200, 0u8..8).prop_map(|(kind, steps, short)| Head::Data { kind, steps, short }),
        1 => (-2i8..3).prop_map(Head::Long),
        8 => (0u8..4).prop_map(Head::Link),
        4 => (prop::bool::weighted(0.9), any::<prop::sample::Index>(), 0u16..1_010, prop::bool::weighted(0.1))
            .prop_map(|(exact, named, back, odd)| Head::Commit { exact, named, back, odd }),
        2 => any::<u64>().prop_map(Head::Junk),
        1 => Just(Head::Zero),
    ]
}

/// Whether resynchronisation reads the start `o` further, below `limit`, by a plain walk of the chain of block headers
/// from it: it ends, within a batch, in a commit block of the commit length that fits and whose `batch_bytes` is the
/// distance from `o` (`format.md` A.2).
fn named_by_its_chain(f: &[u8], o: usize, limit: usize) -> bool {
    use kvik_log::batch::MAX_BATCH_DATA;
    use kvik_log::block::{BlockHeader, block_type};
    let mut p = o;
    while p - o <= MAX_BATCH_DATA as usize && p + 16 <= limit {
        let Some(h) = BlockHeader::decode(f[p..p + 16].try_into().unwrap()) else {
            return false;
        };
        match h.block_type {
            0 | block_type::SEGMENT_INDEX | block_type::TRAILER => return false,
            block_type::COMMIT => {
                return h.body_len == 48
                    && p + 64 <= limit
                    && p > o
                    && u64::from_le_bytes(f[p + 48..p + 56].try_into().unwrap()) == (p - o) as u64;
            }
            _ if h.block_len() > MAX_BATCH_DATA => return false,
            _ => p += h.block_len() as usize,
        }
    }
    false
}

/// Where the chain of headers from `p` ends: a commit block of the commit length that fits, by a plain walk.
fn chain_end(f: &[u8], mut p: usize) -> Option<usize> {
    use kvik_log::block::{BlockHeader, block_type};
    while p + 64 <= f.len() {
        let h = BlockHeader::decode(f[p..p + 16].try_into().unwrap())?;
        match h.block_type {
            0 => return None,
            block_type::COMMIT => return (h.body_len == 48).then_some(p),
            block_type::SEGMENT_INDEX | block_type::TRAILER => return None,
            _ if h.block_len() > kvik_log::batch::MAX_BATCH_DATA => return None,
            _ => p += h.block_len() as usize,
        }
    }
    None
}

/// A segment whose data are crafted block headers at 8-byte steps, each claiming a body that runs over the headers
/// after it, and commit blocks naming starts whose chains end in them: the candidates of resynchronisation.
fn header_run(heads: &[Head], gaps: &[u8], valid_header: bool) -> Vec<u8> {
    use kvik_log::block::{BlockHeader, block_type};
    let mut f = if valid_header {
        build(&[Piece::Facts(1)], None, &[])
    } else {
        let mut f = vec![0u8; kvik_log::header::HEADER_LEN];
        f.extend_from_slice(&[0xFF; 8]);
        f
    };
    f.truncate(f.len().next_multiple_of(8));
    f.extend_from_slice(&[0xEE; 8]);
    let mut commits = Vec::new();
    let mut links: Vec<(usize, u8)> = Vec::new();
    let mut starts = Vec::new();
    for (i, h) in heads.iter().enumerate() {
        let p = f.len();
        starts.push(p);
        let header = |block_type, body_len| {
            BlockHeader {
                block_type,
                flags: 1,
                body_len,
                body_crc32c: 0xDEAD_BEEF,
            }
            .encode()
        };
        match h {
            Head::Data { kind, steps, short } => {
                let len = (8 * u32::from(*steps)).saturating_sub(16 + u32::from(*short));
                f.extend_from_slice(&header(DATA_TYPES[usize::from(*kind)], len));
            }
            Head::Long(d) => {
                let len = (kvik_log::batch::MAX_BATCH_DATA as i64 - 16 + 8 * i64::from(*d)) as u32;
                f.extend_from_slice(&header(block_type::FACTS, len));
            }
            Head::Commit {
                exact,
                named,
                back,
                odd,
            } => {
                f.extend_from_slice(&header(block_type::COMMIT, if *exact { 48 } else { 40 }));
                f.extend_from_slice(&[0x11; 48]);
                commits.push((p, *named, *back, *odd));
            }
            Head::Link(kind) => {
                links.push((i, *kind));
                f.extend_from_slice(&[0; 16]);
            }
            Head::Junk(w) => f.extend_from_slice(&w.to_le_bytes()),
            Head::Zero => f.extend_from_slice(&[0; 8]),
        }
        f.resize(
            f.len() + 8 * usize::from(gaps.get(i).copied().unwrap_or(0) % 3),
            0x5A,
        );
    }
    f.resize(f.len() + 256, 0);
    for (i, kind) in links {
        let (p, next) = (
            starts[i],
            starts.get(i + 1).copied().unwrap_or(f.len() - 64),
        );
        let h = BlockHeader {
            block_type: DATA_TYPES[usize::from(kind)],
            flags: 1,
            body_len: (next - p - 16) as u32,
            body_crc32c: 0xDEAD_BEEF,
        };
        f[p..p + 16].copy_from_slice(&h.encode());
    }
    let from = f.len().min(kvik_log::header::HEADER_LEN + 8);
    for (c, named, back, odd) in commits {
        let ending: Vec<usize> = (from..c)
            .step_by(8)
            .filter(|&s| chain_end(&f, s) == Some(c))
            .collect();
        let bb = if back >= 1_000 {
            kvik_log::batch::MAX_BATCH_DATA + 8 * u64::from(back - 1_000) - 32
        } else if ending.is_empty() || back < 100 {
            8 * u64::from(back)
        } else {
            (c - ending[named.index(ending.len())]) as u64
        } + u64::from(odd);
        f[c + 48..c + 56].copy_from_slice(&bb.to_le_bytes());
    }
    f
}

proptest! {
    #[test]
    fn crafted_header_runs_give_the_reports_the_walked_chains_give(
        heads in prop::collection::vec(head(), 1..160),
        gaps in prop::collection::vec(any::<u8>(), 0..160),
        valid_header in any::<bool>(),
        mark in any::<prop::sample::Index>(),
        damage in prop::collection::vec(damage(), 0..3),
    ) {
        let mut f = header_run(&heads, &gaps, valid_header);
        apply(&mut f, &damage);
        let mark = (mark.index(f.len() + 1) as u64) & !7;
        for limit in [ScanLimit::End, ScanLimit::Mark(mark)] {
            let (scan, stats) = scan_with_stats(&f, &ID, limit);
            prop_assert!(stats.within_bound(f.len() as u64), "{:?}", stats);
            ends_agree(&f, limit, &scan)?;
        }
    }
}

proptest! {
    #[test]
    fn chain_ends_find_the_candidates_a_plain_walk_finds(
        heads in prop::collection::vec(head(), 1..160),
        gaps in prop::collection::vec(any::<u8>(), 0..160),
        damage in prop::collection::vec(damage(), 0..3),
        cut in any::<prop::sample::Index>(),
        span in prop::sample::select(vec![8u64, 16, 40, 128, 1_024, 4 << 20]),
    ) {
        let mut f = header_run(&heads, &gaps, false);
        apply(&mut f, &damage);
        let limit = cut.index(f.len() + 1);
        let from = kvik_log::header::HEADER_LEN;
        let want: Vec<u64> = (from..limit.saturating_sub(80) + 1)
            .step_by(8)
            .filter(|&o| named_by_its_chain(&f, o, limit))
            .map(|o| o as u64)
            .collect();
        let to = (limit.saturating_sub(80) + 1).max(from) as u64;
        let got = legacy::chain_candidates(&f, from as u64, to, limit as u64, span);
        prop_assert_eq!(got, want);
    }
}
