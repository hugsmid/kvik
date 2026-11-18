//! The body checks that keep no row or entry (Q-651) against the decoders, the same verdict on every body, and the
//! facts check's text table against `std::str::from_utf8`, and the state-name check against a plain reference.

mod support;

use kvik_log::entries::EntriesBlock;
use kvik_log::facts::{FactFlags, FactRow, FactsBlock, Quality, Value, ValueType};
use kvik_log::utf8::HeapText;
use kvik_log::{BodyError, ContentError};
use proptest::prelude::*;
use support::*;

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u64>().prop_map(Value::F64),
        (any::<i64>(), 0u8..20).prop_map(|(units, scale)| Value::Decimal { units, scale }),
        any::<u16>().prop_map(Value::State),
        any::<bool>().prop_map(Value::Bool),
        prop_oneof![Just(""), Just("a"), Just("€uro"), Just("ünï")]
            .prop_map(|s| Value::Text(s.into())),
        Just(Value::Absent {
            value_type: ValueType::Text,
            scale: 0
        }),
    ]
}

fn row() -> impl Strategy<Value = FactRow> {
    (
        any::<u32>(),
        any::<i64>(),
        value(),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(|(series_id, t, value, changed_at)| FactRow {
            series_id,
            t,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value,
            changed_at,
        })
}

fn heap_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        0x00u8..0x80,
        0x80u8..0xC0,
        0xC0u8..0xC2,
        0xC2u8..0xE0,
        Just(0xE0u8),
        Just(0xEDu8),
        0xE1u8..0xF0,
        Just(0xF0u8),
        0xF1u8..0xF5,
        0xF5u8..=0xFF,
    ]
}

/// A body of `slices.len()` text rows naming `(offset, length)` slices of one heap, as a crafted file may.
fn shared_heap_body(heap: &[u8], slices: &[(u32, u32)]) -> Vec<u8> {
    let n = slices.len();
    let vt = vec![5u8; n];
    let value: Vec<u8> = slices
        .iter()
        .flat_map(|&(off, len)| ((u64::from(len) << 32) | u64::from(off)).to_le_bytes())
        .collect();
    facts_raw(
        n as u32,
        &[
            (1, 0, vec![1; 4 * n]),
            (2, 0, vec![0; 8 * n]),
            (5, 0, vt),
            (6, 0, value),
            (7, 0, heap.to_vec()),
        ],
    )
}

fn verdicts_agree(body: &[u8]) -> Result<(), TestCaseError> {
    prop_assert_eq!(
        FactsBlock::check(body),
        FactsBlock::decode(body).map(|_| ())
    );
    Ok(())
}

proptest! {
    #[test]
    fn heap_text_agrees_with_std_on_every_slice_once_its_table_is_built(
        heap in prop::collection::vec(heap_byte(), 1..120),
    ) {
        let mut h = HeapText::default();
        // The first call checks the whole heap directly; every later one asks the table.
        prop_assert_eq!(h.is_utf8(&heap, 0, heap.len()), std::str::from_utf8(&heap).is_ok());
        for a in 0..=heap.len() {
            for b in a..=heap.len() {
                prop_assert_eq!(h.is_utf8(&heap, a, b), std::str::from_utf8(&heap[a..b]).is_ok(), "{:?}", &heap[a..b]);
            }
        }
    }

    #[test]
    fn check_and_decode_agree_on_damaged_bodies(
        rows in prop::collection::vec(row(), 1..40),
        damage in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
    ) {
        let Ok(mut body) = (FactsBlock { first_sequence: 1, received_at: T0, rows }).encode() else {
            return Ok(());
        };
        verdicts_agree(&body)?;
        for (i, b) in damage {
            let at = i.index(body.len());
            body[at] = b;
            verdicts_agree(&body)?;
        }
    }

    #[test]
    fn check_and_decode_agree_on_rows_sharing_a_heap(
        heap in prop::collection::vec(heap_byte(), 0..400),
        slices in prop::collection::vec((0u32..420, 0u32..300), 1..200),
    ) {
        verdicts_agree(&shared_heap_body(&heap, &slices))?;
    }

    #[test]
    fn check_and_decode_agree_on_many_rows_of_valid_text(
        start in prop::collection::vec(0u32..85, 1..400),
    ) {
        let heap = "€".repeat(85).into_bytes();
        let slices: Vec<(u32, u32)> = start.iter().map(|&s| (3 * s, 255 - 3 * s)).collect();
        let body = shared_heap_body(&heap, &slices);
        prop_assert!(FactsBlock::check(&body).is_ok());
        verdicts_agree(&body)?;
    }
}

/// An entry as `support` writes it: its type and fields.
fn entry() -> impl Strategy<Value = (u8, Vec<(u16, Vec<u8>)>)> {
    let name = prop_oneof![
        Just(b"a".to_vec()),
        Just("ü".as_bytes().to_vec()),
        Just(vec![0xC3]),
        Just(Vec::new()),
        prop::collection::vec(heap_byte(), 0..8),
    ];
    let states = prop::collection::vec(
        prop_oneof![
            Just(b"on".to_vec()),
            Just(b"off".to_vec()),
            Just(vec![0xFF]),
            Just(b"x".to_vec())
        ],
        0..6,
    );
    prop_oneof![
        (any::<u32>(), name.clone())
            .prop_map(|(id, n)| (3u8, vec![(1u16, id.to_le_bytes().to_vec()), (2, n)])),
        name.clone().prop_map(|n| (7u8, vec![(31u16, n)])),
        (name.clone(), any::<i64>())
            .prop_map(|(n, t)| (6u8, vec![(20u16, t.to_le_bytes().to_vec()), (30, n)])),
        Just((0x80u8, Vec::new())),
        prop::collection::vec(
            prop_oneof![
                Just(b"c1".to_vec()),
                Just(b"C!".to_vec()),
                Just(Vec::new()),
                Just(vec![0xC3])
            ],
            0..3,
        )
        .prop_map(|consumers| {
            let mut stamp = 5u64.to_le_bytes().to_vec();
            stamp.extend_from_slice(&9u64.to_le_bytes());
            let mut f = vec![
                (40u16, 4u64.to_le_bytes().to_vec()),
                (41, stamp),
                (42, vec![7; 16]),
                (43, 100u64.to_le_bytes().to_vec()),
                (44, 1u32.to_le_bytes().to_vec()),
                (45, vec![0]),
                (46, 3u32.to_le_bytes().to_vec()),
            ];
            f.extend(consumers.into_iter().map(|c| (47u16, c)));
            (8u8, f)
        }),
        (any::<u32>(), states).prop_map(|(id, names)| {
            let mut d = definition(id, "s", "");
            d.1[3].1 = vec![3];
            let at = d.1.iter().position(|(f, _)| *f == 14).unwrap();
            d.1.splice(at..at, names.into_iter().map(|n| (13u16, n)));
            d
        }),
        any::<u32>().prop_map(|id| definition(id, "series", "W")),
    ]
}

proptest! {
    #[test]
    fn entries_check_and_decode_agree_on_damaged_bodies(
        entries in prop::collection::vec(entry(), 1..12),
        damage in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
    ) {
        let mut body = entries_body(1, T0, &entries);
        prop_assert_eq!(EntriesBlock::check(&body), EntriesBlock::decode(&body).map(|_| ()));
        for (i, b) in damage {
            let at = i.index(body.len());
            body[at] = b;
            prop_assert_eq!(EntriesBlock::check(&body), EntriesBlock::decode(&body).map(|_| ()));
        }
    }
}

/// A definition with `names` as its state names.
fn states(names: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut d = definition(1, "s", "");
    d.1[3].1 = vec![3];
    let at = d.1.iter().position(|(f, _)| *f == 14).unwrap();
    d.1.splice(at..at, names.map(|n| (13u16, n)));
    entries_body(1, T0, &[d])
}

/// Q-182 with Q-651: at the limit of 65,536 state names a repeated name is still found; one more name is refused
/// whatever the names.
#[test]
fn state_names_at_the_limit_are_told_distinct_and_past_it_refused() {
    let name = |k: u32| {
        (0..3)
            .map(|d| 0x21 + (k >> (6 * d) & 63) as u8)
            .collect::<Vec<u8>>()
    };
    let distinct = states((0..65_536).map(name));
    assert!(EntriesBlock::check(&distinct).is_ok());
    let repeated = states((0..65_536).map(|k| name(k.min(65_534))));
    assert!(EntriesBlock::check(&repeated).is_err());
    let over = states((0..65_537).map(name));
    assert!(EntriesBlock::check(&over).is_err());
    for b in [&distinct, &repeated, &over] {
        assert_eq!(EntriesBlock::check(b), EntriesBlock::decode(b).map(|_| ()));
    }
}

/// A state name for the reference property: short and long names from a small alphabet, so that some repeat by
/// chance, and names that are invalid by length or UTF-8.
fn state_name() -> impl Strategy<Value = Vec<u8>> {
    let from = |len: std::ops::Range<usize>| {
        prop::collection::vec(prop::sample::select(b"abcd".to_vec()), len)
    };
    prop_oneof![
        4 => from(1..4),
        2 => from(7..10),
        1 => from(12..20),
        1 => Just("é".as_bytes().to_vec()),
        1 => Just(vec![0xC3]),
        1 => Just(b"ab\xFFab\xFF".to_vec()),
        1 => Just(Vec::new()),
        1 => Just(vec![b'a'; 63]),
        1 => Just(vec![b'a'; 64]),
        1 => prop::collection::vec(any::<u8>(), 1..10),
    ]
}

proptest! {
    /// Q-651, Q-870: the state names of a definition are accepted exactly when each is 1 to 63 bytes of UTF-8 and no
    /// two are equal, as a sort finds them, on both sides of the pairwise limit.
    #[test]
    fn state_names_are_checked_as_a_plain_reference_checks_them(
        mut names in prop::collection::vec(state_name(), 0..48),
        copy in prop::option::of((any::<prop::sample::Index>(), any::<prop::sample::Index>())),
    ) {
        if let (Some((from, to)), false) = (copy, names.is_empty()) {
            let v = names[from.index(names.len())].clone();
            let at = to.index(names.len());
            names[at] = v;
        }
        let valid = names
            .iter()
            .all(|n| (1..=63).contains(&n.len()) && std::str::from_utf8(n).is_ok());
        let distinct = names.iter().collect::<std::collections::BTreeSet<_>>().len() == names.len();
        let expected = if names.is_empty() {
            Err(BodyError::Invalid(ContentError::FieldPresence { entry: 0, field: 13 }))
        } else if valid && distinct {
            Ok(())
        } else {
            Err(BodyError::Invalid(ContentError::Field { entry: 0, field: 13 }))
        };
        let body = states(names.into_iter());
        prop_assert_eq!(EntriesBlock::check(&body), expected);
        prop_assert_eq!(EntriesBlock::decode(&body).map(|_| ()), expected);
    }
}
