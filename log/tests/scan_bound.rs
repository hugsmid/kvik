//! The scan bound (`format.md` A, common rules; Q-110): the CRC shift and prefix table against direct checksums,
//! and the bounded scan against the reference scan on damaged segments.

mod support;

use kvik_log::batch::BatchBuilder;
use kvik_log::block::FLAG_SKIPPABLE;
use kvik_log::crc::{PrefixCrc, crc32c, crc32c_shift};
use kvik_log::scan::{ScanLimit, legacy, scan_with_stats};
use kvik_log::segment::SegmentImage;
use proptest::prelude::*;
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
        prop_assert_eq!(new, old, "{:?}", limit);
        prop_assert!(stats.within_bound(len), "{:?} for {} bytes", stats, len);
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
fn big_batch(f: &mut Vec<u8>, seq: u64, over: i64, t: i64) -> u64 {
    use kvik_log::batch::MAX_BATCH_DATA;
    use kvik_log::block::block_type;
    let at = f.len() as u64;
    let first = facts_body(seq, T0, &[(1, T0, 0.0)]);
    let head = 16 + first.len().div_ceil(8) as u64 * 8 + 16;
    let mut filler = vec![0x11u8; (MAX_BATCH_DATA as i64 + over - head as i64) as usize];
    let inner_at = at + head + 4 * 4_096;
    let inner = facts(seq + 1, 2).encode(&ID, inner_at, t).unwrap();
    filler[4 * 4_096..4 * 4_096 + inner.len()].copy_from_slice(&inner);
    let blocks = [
        (block_type::FACTS, 0, first),
        (0x50, FLAG_SKIPPABLE, filler),
    ];
    f.extend_from_slice(&raw_batch(&ID, at, &blocks, seq, seq, t));
    seq + 3
}

/// Small batches around two batches near the 8 MiB limit, the batch before each damaged: the first is met by a
/// boundary read once the scan has found damage, the second by resynchronisation.
fn near_limit(over: [i64; 2], damage: &[Damage]) -> Vec<u8> {
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
    seq = big_batch(&mut f, seq, over[0], T0);
    small(&mut f, &mut seq, true);
    seq = big_batch(&mut f, seq, over[1], T0);
    small(&mut f, &mut seq, false);
    f.resize(f.len() + 4_096, 0);
    apply(&mut f, damage);
    f
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn the_bounded_scan_agrees_with_the_reference_scan_across_the_batch_and_pruning_limits(
        over in [prop::sample::select(vec![-16i64, -8, 0, 8]), prop::sample::select(vec![-16i64, -8, 0, 8])],
        damage in prop::collection::vec(damage(), 0..3),
        mark in any::<prop::sample::Index>(),
    ) {
        agree(&near_limit(over, &damage), mark)?;
    }
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn the_bounded_scan_agrees_with_the_reference_scan_under_disjoint_damage_in_one_window(
        two in any::<bool>(),
        site in prop_oneof![any::<u8>().prop_map(Site::Flip), Just(Site::Sector), Just(Site::Page)],
        at in prop::collection::vec(any::<prop::sample::Index>(), 1..=64),
        mark in any::<prop::sample::Index>(),
    ) {
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
        agree(&f, mark)?;
    }
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
