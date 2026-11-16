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
    f
}

fn agree(f: &[u8], mark: prop::sample::Index) -> Result<(), TestCaseError> {
    let len = f.len() as u64;
    let mark = (mark.index(f.len() + 1) as u64) & !7;
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
