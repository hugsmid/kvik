//! Arrival batches (roadmap 4.3): facts and entries bodies and batch assembly — round trips, canonical form, the
//! checks of `format.md` A.5 and A.6, and size limits.

mod support;

use kvik_log::batch::BatchBuilder;
use kvik_log::block::{FLAG_SKIPPABLE, block_type};
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{
    CounterRules, DAY, Definition, EntriesBlock, Entry, ExcessPrecision, IntervalLength, Kind,
    MarkState, ParameterChange, Recovery, Retention, Timing, Versioning,
};
use kvik_log::facts::{FactFlags, FactRow, FactsBlock, Quality, Value, ValueType};
use kvik_log::scan::{LossCause, ScanLimit, scan};
use kvik_log::segment::SegmentImage;
use kvik_log::{BodyError, ContentError, Element, EncodeError, FormatError};
use proptest::prelude::*;
use support::*;

// ---------------------------------------------------------------------------------------------------------------
// Strategies: every value valid by construction.

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u64>().prop_map(Value::F64),
        (any::<i64>(), 0u8..=18).prop_map(|(units, scale)| Value::Decimal { units, scale }),
        any::<u16>().prop_map(Value::State),
        any::<bool>().prop_map(Value::Bool),
        prop_oneof![".{0,40}", "[a-z]{0,3}"].prop_map(|s: String| Value::Text(truncate(s, 255))),
        (1u8..=5, 0u8..=18).prop_map(|(vt, s)| {
            let value_type = ValueType::from_code(vt).unwrap();
            Value::Absent {
                value_type,
                scale: if value_type == ValueType::Decimal {
                    s
                } else {
                    0
                },
            }
        }),
    ]
}

fn truncate(mut s: String, max: usize) -> String {
    while s.len() > max {
        s.pop();
    }
    s
}

fn row() -> impl Strategy<Value = FactRow> {
    let quality = prop_oneof![4 => Just(Quality::Good), 1 => Just(Quality::Estimated), 1 => Just(Quality::Suspect), 1 => Just(Quality::Bad)];
    let flags = prop_oneof![3 => Just(FactFlags::default()), 1 => any::<[bool; 6]>().prop_map(|b| FactFlags {
        future: b[0],
        late: b[1],
        revise: b[2],
        host_time: b[3],
        unsynced_clock: b[4],
        rounded: b[5],
    })];
    (
        any::<u32>(),
        any::<i64>(),
        quality,
        flags,
        value(),
        prop::option::weighted(0.2, any::<u64>()),
    )
        .prop_map(
            |(series_id, t, quality, flags, value, changed_at)| FactRow {
                series_id,
                t,
                quality,
                flags,
                value,
                changed_at,
            },
        )
}

/// Rows with runs of repeated rows, so run-length columns occur.
fn rows(max: usize) -> impl Strategy<Value = Vec<FactRow>> {
    prop::collection::vec((row(), 1usize..6), 1..max).prop_map(|v| {
        let mut out: Vec<FactRow> = v
            .into_iter()
            .flat_map(|(r, n)| std::iter::repeat_n(r, n))
            .collect();
        out.truncate(65_536);
        out
    })
}

fn name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9/._-]{0,40}"
}

fn zone() -> impl Strategy<Value = String> {
    "[A-Z][a-z]{1,10}/[A-Z][a-z_]{1,20}"
}

fn positive() -> impl Strategy<Value = i64> {
    1i64..=i64::MAX
}

fn definition() -> impl Strategy<Value = Definition> {
    let interval = prop_oneof![
        (positive())
            .prop_flat_map(|length| (Just(length), 0..length))
            .prop_map(|(length, phase)| IntervalLength::Fixed { length, phase }),
        (zone(), 0..DAY).prop_map(|(zone, day_start)| IntervalLength::Day { zone, day_start }),
        zone().prop_map(|zone| IntervalLength::Month { zone }),
    ];
    (
        (
            any::<u32>(),
            name(),
            1u8..=5,
            1u8..=5,
            0u8..=18,
            "[a-zA-Z%/]{0,12}",
        ),
        (
            prop::option::of(positive()),
            prop::option::of(positive()),
            any::<bool>(),
            prop::option::of(any::<u64>()),
            any::<u32>(),
            any::<i64>(),
        ),
        (
            prop::collection::vec("[A-Za-z_]{1,63}", 1..5),
            0u8..=2,
            0u8..=2,
            interval,
            prop::option::of(any::<i64>()),
        ),
        (0u8..=2, 0u8..=18, any::<bool>()),
    )
        .prop_map(
            |(
                (id, name, kind, vt, scale, unit),
                (ei, s, roc, wrap_at, ppm, lateness),
                (states, ver, timing, interval, period),
                (xp, max, set),
            )| {
                let kind = [
                    Kind::Gauge,
                    Kind::Counter,
                    Kind::State,
                    Kind::Text,
                    Kind::Interval,
                ][usize::from(kind - 1)];
                let value_type = ValueType::from_code(vt).unwrap();
                let decimal = value_type == ValueType::Decimal;
                Definition {
                    id,
                    name,
                    kind,
                    value_type,
                    scale: decimal.then_some(scale),
                    unit,
                    expected_interval: ei,
                    staleness: s,
                    record_on_change: roc,
                    counter: (kind == Kind::Counter).then_some(CounterRules {
                        wrap_at,
                        reset_ceiling_ppm: ppm,
                    }),
                    lateness_bound: lateness,
                    state_names: if value_type == ValueType::State {
                        states
                    } else {
                        vec![]
                    },
                    versioning: [Versioning::None, Versioning::Time, Versioning::Version]
                        [usize::from(ver)],
                    timing: [Timing::Observed, Timing::Scheduled, Timing::Forecast]
                        [usize::from(timing)],
                    interval: (kind == Kind::Interval).then_some(interval),
                    retention: period.map_or(Retention::LongTerm, |period| Retention::Diagnostic {
                        period,
                    }),
                    excess_precision: decimal.then_some(match xp {
                        0 => ExcessPrecision::Round,
                        1 => ExcessPrecision::Refuse,
                        _ => ExcessPrecision::AutoRaise {
                            max_scale: max.max(scale),
                        },
                    }),
                    staleness_set: set,
                }
            },
        )
}

fn parameter_change() -> impl Strategy<Value = ParameterChange> {
    (
        any::<u32>(),
        any::<i64>(),
        prop::option::of(prop::option::of(positive())),
        prop::option::of((prop::option::of(positive()), any::<bool>())),
        (
            prop::option::of(any::<bool>()),
            prop::option::of(prop::option::of(any::<u64>())),
            prop::option::of(any::<u32>()),
            prop::option::of(any::<i64>()),
        ),
        (prop::option::of(0u8..=1), prop::option::of(any::<i64>())),
    )
        .prop_map(
            |(id, eff, ei, s, (roc, wrap, ppm, late), (class, period))| {
                // Field 7 is carried only with field 8.
                let staleness = if ei.is_some() && s.is_none() {
                    Some((None, false))
                } else {
                    s
                };
                let mut p = ParameterChange {
                    id,
                    effective: None,
                    expected_interval: ei,
                    staleness,
                    record_on_change: roc,
                    wrap_at: wrap,
                    reset_ceiling_ppm: ppm,
                    lateness_bound: late,
                    retention_class: class,
                    retention_period: period,
                };
                let timed = p.expected_interval.is_some()
                    || p.staleness.is_some()
                    || p.record_on_change.is_some()
                    || p.wrap_at.is_some()
                    || p.reset_ceiling_ppm.is_some()
                    || p.lateness_bound.is_some();
                p.effective = timed.then_some(eff);
                p
            },
        )
}

fn recovery() -> impl Strategy<Value = Recovery> {
    (
        any::<u64>(),
        any::<[u8; 16]>(),
        any::<u64>(),
        any::<u32>(),
        0u8..=3,
        any::<u32>(),
        prop::collection::vec("[a-z0-9][a-z0-9._-]{0,63}", 0..4),
    )
        .prop_map(
            |(after, boot_id, tail_bytes, readable_commits, m, id_floor, consumers)| Recovery {
                after,
                boot_id,
                tail_bytes,
                readable_commits,
                mark_state: [
                    MarkState::Readable,
                    MarkState::Missing,
                    MarkState::Unreadable,
                    MarkState::Stale,
                ][usize::from(m)],
                id_floor,
                consumers,
            },
        )
}

/// Every entry type except the recovery entry, which is alone in its batch.
fn entry() -> impl Strategy<Value = Entry> {
    prop_oneof![
        definition().prop_map(Entry::Definition),
        parameter_change().prop_map(Entry::ParameterChange),
        (any::<u32>(), name()).prop_map(|(id, name)| Entry::Alias { id, name }),
        (any::<u32>(), any::<i64>()).prop_map(|(id, t_r)| Entry::Retirement { id, t_r }),
        (zone(), any::<i64>()).prop_map(|(zone, effective)| Entry::SiteZone { zone, effective }),
        "[0-9]{4}[a-z]".prop_map(|version| Entry::Tzdata { version }),
        (any::<u32>(), 0u8..=18).prop_map(|(id, scale)| Entry::ScaleRaise { id, scale }),
    ]
}

fn content() -> impl Strategy<Value = BatchContent> {
    let items = prop_oneof![
        1 => recovery().prop_map(|r| vec![Item::Entry(Entry::Recovery(r))]),
        6 => prop::collection::vec(prop_oneof![row().prop_map(Item::Fact), entry().prop_map(Item::Entry)], 1..30),
    ];
    (1u64..1 << 40, any::<i64>(), items).prop_map(|(first_sequence, received_at, items)| {
        BatchContent {
            first_sequence,
            received_at,
            items,
        }
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Round trips and canonical form.

proptest! {
    #[test]
    fn facts_round_trip_and_are_canonical(rows in rows(40), first in 1u64..u64::MAX / 2, received_at in any::<i64>()) {
        let block = FactsBlock { first_sequence: first, received_at, rows };
        let body = block.encode().unwrap();
        prop_assert_eq!(FactsBlock::decode(&body).unwrap(), block.clone());
        prop_assert_eq!(FactsBlock::decode_canonical(&body).unwrap(), block);
    }

    #[test]
    fn entries_round_trip_and_are_canonical(
        entries in prop::collection::vec(prop_oneof![5 => entry(), 1 => recovery().prop_map(Entry::Recovery)], 1..12),
        first in 1u64..u64::MAX / 2,
        received_at in any::<i64>(),
    ) {
        let block = EntriesBlock { first_sequence: first, received_at, entries };
        let body = block.encode().unwrap();
        prop_assert_eq!(EntriesBlock::decode(&body).unwrap(), block.clone());
        prop_assert_eq!(EntriesBlock::decode_canonical(&body).unwrap(), block);
    }

    #[test]
    fn batches_round_trip_through_a_segment(contents in prop::collection::vec(content(), 1..4)) {
        let mut seg = SegmentImage::create_ahead(ID, T0, 1 << 20).unwrap();
        let mut next = contents[0].first_sequence;
        seg.take_into_use(next).unwrap();
        let mut written = Vec::new();
        for (i, c) in contents.into_iter().enumerate() {
            let c = BatchContent { first_sequence: next, ..c };
            next = c.last_sequence() + 1;
            seg.append(&c.builder().unwrap(), i as i64).unwrap();
            written.push(c);
        }
        let r = scan(seg.bytes(), &ID, ScanLimit::End).unwrap();
        prop_assert!(r.regions.is_empty());
        let decoded: Vec<_> = r.batches.iter().map(|b| BatchContent::decode_canonical(b).unwrap()).collect();
        prop_assert_eq!(decoded, written);
    }

    #[test]
    fn decoding_any_valid_body_then_encoding_reproduces_it(rows in rows(20), entries in prop::collection::vec(entry(), 1..6)) {
        let f = FactsBlock { first_sequence: 7, received_at: T0, rows }.encode().unwrap();
        prop_assert_eq!(FactsBlock::decode(&f).unwrap().encode().unwrap(), f);
        let e = EntriesBlock { first_sequence: 7, received_at: T0, entries }.encode().unwrap();
        prop_assert_eq!(EntriesBlock::decode(&e).unwrap().encode().unwrap(), e);
    }
}

#[test]
fn the_encoders_follow_the_tables() {
    // The bodies hand-laid from A.5 and A.6 in `support` (and in the golden files) are the canonical ones.
    let rows = [(1, T0, 20.5), (1, T0 + 10 * SECOND, 20.75)];
    let typed = FactsBlock {
        first_sequence: 2,
        received_at: T0 + 10 * SECOND,
        rows: rows
            .iter()
            .map(|&(series_id, t, v)| FactRow {
                series_id,
                t,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: Value::F64(f64::to_bits(v)),
                changed_at: None,
            })
            .collect(),
    };
    assert_eq!(
        typed.encode().unwrap(),
        facts_body(2, T0 + 10 * SECOND, &rows)
    );
    let def = Definition {
        id: 1,
        name: "room/temperature".into(),
        kind: Kind::Gauge,
        value_type: ValueType::F64,
        scale: None,
        unit: "degC".into(),
        expected_interval: Some(10 * SECOND),
        staleness: Some(30 * SECOND),
        record_on_change: false,
        counter: None,
        lateness_bound: 3_600 * SECOND,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: None,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    };
    let typed = EntriesBlock {
        first_sequence: 1,
        received_at: T0,
        entries: vec![
            Entry::Definition(def),
            Entry::Alias {
                id: 1,
                name: "a".into(),
            },
        ],
    };
    assert_eq!(
        typed.encode().unwrap(),
        entries_body(1, T0, &[definition_fields(), alias(1, "a")])
    );
}

fn definition_fields() -> support::Entry {
    support::definition(1, "room/temperature", "degC")
}

#[test]
fn run_length_is_used_exactly_when_smaller() {
    let row = |q| FactRow {
        series_id: 1,
        t: 0,
        quality: q,
        flags: FactFlags::default(),
        value: Value::Bool(true),
        changed_at: None,
    };
    let encoding_of = |rows: Vec<FactRow>, column: u8| {
        let b = FactsBlock {
            first_sequence: 1,
            received_at: 0,
            rows,
        }
        .encode()
        .unwrap();
        let n = usize::from(b[20]);
        (0..n)
            .map(|i| &b[24 + 12 * i..])
            .find(|e| e[0] == column)
            .map(|e| e[1])
    };
    // Two equal rows: run-length (2 bytes) is not smaller than plain (2 bytes).
    assert_eq!(encoding_of(vec![row(Quality::Bad); 2], 5), Some(0));
    assert_eq!(encoding_of(vec![row(Quality::Bad); 3], 5), Some(1));
    assert_eq!(encoding_of(vec![row(Quality::Bad); 3], 3), Some(1));
    // All good: the quality column is absent.
    assert_eq!(encoding_of(vec![row(Quality::Good); 3], 3), None);
    assert_eq!(
        encoding_of(vec![row(Quality::Good), row(Quality::Bad)], 3),
        Some(0)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Facts checks (A.5).

fn plain_columns(n: u32) -> Vec<(u8, u8, Vec<u8>)> {
    let n = n as usize;
    vec![
        (1, 0, vec![0; 4 * n]),
        (2, 0, vec![0; 8 * n]),
        (5, 0, vec![1; n]),
        (6, 0, vec![0; 8 * n]),
    ]
}

fn with(n: u32, f: impl FnOnce(&mut Vec<(u8, u8, Vec<u8>)>)) -> Result<FactsBlock, BodyError> {
    let mut c = plain_columns(n);
    f(&mut c);
    FactsBlock::decode(&facts_raw(n, &c))
}

fn set(c: &mut [(u8, u8, Vec<u8>)], id: u8, enc: u8, payload: Vec<u8>) {
    let e = c.iter_mut().find(|e| e.0 == id).unwrap();
    e.1 = enc;
    e.2 = payload;
}

fn insert(c: &mut Vec<(u8, u8, Vec<u8>)>, id: u8, enc: u8, payload: Vec<u8>) {
    let at = c.iter().position(|e| e.0 > id).unwrap_or(c.len());
    c.insert(at, (id, enc, payload));
}

fn invalid(e: ContentError) -> Result<FactsBlock, BodyError> {
    Err(BodyError::Invalid(e))
}

#[test]
fn facts_checks() {
    use ContentError as E;
    assert!(with(2, |_| {}).is_ok());
    assert_eq!(
        FactsBlock::decode(&facts_raw(0, &plain_columns(0))),
        invalid(E::RowCount(0))
    );
    assert_eq!(
        FactsBlock::decode(&facts_raw(65_537, &[])),
        invalid(E::RowCount(65_537))
    );
    assert_eq!(FactsBlock::decode(&[0; 23]), invalid(E::Prefix));
    let mut b = facts_raw(2, &plain_columns(2));
    b[21] = 11;
    assert_eq!(FactsBlock::decode(&b), invalid(E::DirEntrySize(11)));
    let mut b = facts_raw(2, &plain_columns(2));
    b[20] = 200;
    assert_eq!(FactsBlock::decode(&b), invalid(E::Directory));
    for id in [1, 2, 5, 6] {
        assert_eq!(
            with(2, |c| c.retain(|e| e.0 != id)),
            invalid(E::MissingColumn { column: id }),
            "{id}"
        );
    }
    assert_eq!(
        with(2, |c| c.insert(1, (1, 0, vec![0; 8]))),
        invalid(E::ColumnOrder { column: 1 })
    );
    assert_eq!(
        with(2, |c| c.swap(0, 1)),
        invalid(E::ColumnOrder { column: 1 })
    );
    assert_eq!(
        with(2, |c| c.push((0x10, 0, vec![]))),
        Err(BodyError::Unsupported(Element::Column(0x10)))
    );
    assert!(with(2, |c| c.push((0x90, 0, vec![1, 2, 3]))).is_ok());
    assert_eq!(
        with(2, |c| set(c, 1, 1, vec![2, 0, 0, 0, 0])),
        invalid(E::Encoding { column: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 2, vec![1, 1])),
        invalid(E::Encoding { column: 5 })
    );
    assert_eq!(
        with(2, |c| set(c, 2, 0, vec![0; 15])),
        invalid(E::Length { column: 2 })
    );
    assert_eq!(
        with(3, |c| set(c, 5, 1, vec![2, 1, 1, 1])),
        invalid(E::RunLength { column: 5 })
    );
    assert!(with(3, |c| set(c, 5, 1, vec![3, 1])).is_ok());
    assert_eq!(
        with(2, |c| insert(c, 3, 0, vec![0, 4])),
        invalid(E::Value { column: 3, row: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![1, 6])),
        invalid(E::Value { column: 5, row: 1 })
    );
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![0, 1])),
        invalid(E::Value { column: 5, row: 0 })
    );
    assert_eq!(
        with(2, |c| insert(c, 4, 0, vec![0, 0, 0, 1])),
        Err(BodyError::Unsupported(Element::FactFlags(0x0100)))
    );
    // Bool and state values with non-zero upper bytes; a no-value row with a value.
    let v = |x: u64| {
        [0u64, x]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>()
    };
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![4, 4]);
            set(c, 6, 0, v(2))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![3, 3]);
            set(c, 6, 0, v(65_535))
        })
        .is_ok()
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![3, 3]);
            set(c, 6, 0, v(65_536))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            insert(c, 4, 0, vec![0, 0, 0x20, 0]);
            set(c, 6, 0, v(5))
        }),
        invalid(E::Value { column: 6, row: 1 })
    );
    // Text: heap presence, bounds, UTF-8, length.
    let text = |off: u64, len: u64| v(off | len << 32);
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![5, 5])),
        invalid(E::Presence { column: 7 })
    );
    assert_eq!(
        with(2, |c| insert(c, 7, 0, vec![])),
        invalid(E::Presence { column: 7 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 2));
            insert(c, 7, 0, b"hi".to_vec())
        })
        .is_ok()
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(1, 2));
            insert(c, 7, 0, b"hi".to_vec())
        }),
        invalid(E::Text { row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 2));
            insert(c, 7, 0, vec![0xC3, 0x28])
        }),
        invalid(E::Text { row: 1 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![1, 5]);
            set(c, 6, 0, text(0, 256));
            insert(c, 7, 0, vec![b'a'; 256])
        }),
        invalid(E::Text { row: 1 })
    );
    // changed_at: column 8 exactly when a row has flag bit 6, zero on the others.
    assert_eq!(
        with(2, |c| insert(c, 8, 0, vec![0; 16])),
        invalid(E::Presence { column: 8 })
    );
    assert_eq!(
        with(2, |c| insert(c, 4, 0, vec![0x40, 0, 0, 0])),
        invalid(E::Presence { column: 8 })
    );
    assert_eq!(
        with(2, |c| {
            insert(c, 4, 0, vec![0x40, 0, 0, 0]);
            insert(
                c,
                8,
                0,
                [9u8; 8].into_iter().chain(1u64.to_le_bytes()).collect(),
            )
        }),
        invalid(E::Presence { column: 8 })
    );
    assert!(
        with(2, |c| {
            insert(c, 4, 0, vec![0x40, 0, 0, 0]);
            insert(c, 8, 0, vec![9; 8].into_iter().chain([0; 8]).collect())
        })
        .is_ok()
    );
    // scale: column 9 exactly when a row is decimal, 0 to 18, zero on the others.
    assert_eq!(
        with(2, |c| set(c, 5, 0, vec![2, 1])),
        invalid(E::Presence { column: 9 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![19, 0])
        }),
        invalid(E::Value { column: 9, row: 0 })
    );
    assert_eq!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![4, 4])
        }),
        invalid(E::Presence { column: 9 })
    );
    assert!(
        with(2, |c| {
            set(c, 5, 0, vec![2, 1]);
            insert(c, 9, 0, vec![18, 0])
        })
        .is_ok()
    );
    // Payloads outside the body, over the directory, or overlapping.
    let mut b = facts_raw(2, &plain_columns(2));
    b[24 + 4..24 + 8].copy_from_slice(&10_000u32.to_le_bytes());
    assert_eq!(
        FactsBlock::decode(&b),
        invalid(E::PayloadOutside { column: 1 })
    );
    let mut b = facts_raw(2, &plain_columns(2));
    b[24 + 4..24 + 8].copy_from_slice(&30u32.to_le_bytes());
    assert_eq!(
        FactsBlock::decode(&b),
        invalid(E::PayloadOutside { column: 1 })
    );
    let mut b = facts_raw(2, &plain_columns(2));
    let t_offset = u32::from_le_bytes(b[24 + 12 + 4..24 + 12 + 8].try_into().unwrap());
    b[24 + 4..24 + 8].copy_from_slice(&(t_offset - 4).to_le_bytes());
    assert_eq!(FactsBlock::decode(&b), invalid(E::Overlap { column: 2 }));
}

#[test]
fn non_canonical_facts_bodies_pass_the_checks_but_not_the_canonical_decoder() {
    let canonical = facts_raw(3, &plain_columns(3));
    // value_type of three equal rows is canonical in run-length, not plain.
    assert!(FactsBlock::decode(&canonical).is_ok());
    assert_eq!(
        FactsBlock::decode_canonical(&canonical),
        Err(BodyError::NonCanonical)
    );
    let mut c = plain_columns(3);
    set(&mut c, 5, 1, vec![3, 1]);
    let good = facts_raw(3, &c);
    assert!(FactsBlock::decode_canonical(&good).is_ok());
    // Non-zero padding between payloads; bytes after the last payload; an all-good quality column; an ignorable column.
    let mut b = good.clone();
    let pad_at = 24 + 12 * 4 + 12;
    b[pad_at] = 1;
    assert!(FactsBlock::decode(&b).is_ok());
    assert_eq!(
        FactsBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
    let mut b = good.clone();
    b.push(0);
    assert_eq!(
        FactsBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
    let mut c2 = c.clone();
    insert(&mut c2, 3, 0, vec![0; 3]);
    assert_eq!(
        FactsBlock::decode_canonical(&facts_raw(3, &c2)),
        Err(BodyError::NonCanonical)
    );
    let mut c3 = c.clone();
    c3.push((0x90, 0, vec![]));
    assert_eq!(
        FactsBlock::decode_canonical(&facts_raw(3, &c3)),
        Err(BodyError::NonCanonical)
    );
}

#[test]
fn facts_encode_refuses_what_it_cannot_write() {
    let row = |value| FactRow {
        series_id: 1,
        t: 0,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value,
        changed_at: None,
    };
    let block = |rows| FactsBlock {
        first_sequence: 1,
        received_at: 0,
        rows,
    };
    assert_eq!(
        block(vec![]).encode(),
        Err(EncodeError::Content(ContentError::RowCount(0)))
    );
    assert_eq!(
        block(vec![row(Value::Bool(true)); 65_537]).encode(),
        Err(EncodeError::Content(ContentError::RowCount(65_537)))
    );
    assert!(block(vec![row(Value::Bool(true)); 65_536]).encode().is_ok());
    assert_eq!(
        block(vec![row(Value::Text("x".repeat(256)))]).encode(),
        Err(EncodeError::Content(ContentError::Text { row: 0 }))
    );
    assert!(
        block(vec![row(Value::Text("x".repeat(255)))])
            .encode()
            .is_ok()
    );
    assert_eq!(
        block(vec![row(Value::Decimal {
            units: 1,
            scale: 19
        })])
        .encode(),
        Err(EncodeError::Content(ContentError::Value {
            column: 9,
            row: 0
        }))
    );
    assert_eq!(
        block(vec![row(Value::Absent {
            value_type: ValueType::F64,
            scale: 1
        })])
        .encode(),
        Err(EncodeError::Content(ContentError::Value {
            column: 9,
            row: 0
        }))
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Entries checks (A.6).

type Fields = Vec<(u16, Vec<u8>)>;

fn decode_one(t: u8, fields: Fields) -> Result<EntriesBlock, BodyError> {
    EntriesBlock::decode(&entries_body(1, T0, &[(t, fields)]))
}

fn def_fields() -> Fields {
    definition_fields().1
}

fn replace(mut f: Fields, id: u16, v: Vec<u8>) -> Fields {
    match f.iter().position(|e| e.0 >= id) {
        Some(i) if f[i].0 == id => f[i].1 = v,
        Some(i) => f.insert(i, (id, v)),
        None => f.push((id, v)),
    }
    f
}

fn remove(mut f: Fields, id: u16) -> Fields {
    f.retain(|e| e.0 != id);
    f
}

fn field_err(field: u16) -> Result<EntriesBlock, BodyError> {
    Err(BodyError::Invalid(ContentError::Field { entry: 0, field }))
}

fn presence(field: u16) -> Result<EntriesBlock, BodyError> {
    Err(BodyError::Invalid(ContentError::FieldPresence {
        entry: 0,
        field,
    }))
}

fn i64b(v: i64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// A decimal counter of a fixed interval kind would mix rules; each base below is valid on its own.
fn decimal_def() -> Fields {
    let f = replace(def_fields(), 4, vec![2]);
    let f = replace(f, 5, vec![4]);
    replace(f, 25, vec![0])
}

#[test]
fn definition_field_ranges() {
    assert!(decode_one(1, def_fields()).is_ok());
    let cases: Vec<(u16, Fields)> = vec![
        (1, replace(def_fields(), 1, vec![1, 0])),
        (2, replace(def_fields(), 2, vec![])),
        (2, replace(def_fields(), 2, vec![b'a'; 256])),
        (2, replace(def_fields(), 2, vec![0xFF])),
        (3, replace(def_fields(), 3, vec![6])),
        (3, replace(def_fields(), 3, vec![0])),
        (4, replace(def_fields(), 4, vec![6])),
        (5, replace(decimal_def(), 5, vec![19])),
        (6, replace(def_fields(), 6, vec![b'u'; 64])),
        (7, replace(def_fields(), 7, i64b(0))),
        (7, replace(def_fields(), 7, i64b(-1))),
        (7, replace(def_fields(), 7, vec![1; 4])),
        (8, replace(def_fields(), 8, i64b(0))),
        (9, replace(def_fields(), 9, vec![2])),
        (12, replace(def_fields(), 12, vec![0; 4])),
        (14, replace(def_fields(), 14, vec![3])),
        (15, replace(def_fields(), 15, vec![3])),
        (23, replace(def_fields(), 23, vec![2])),
        (25, replace(decimal_def(), 25, vec![3])),
        (
            26,
            replace(
                replace(replace(decimal_def(), 25, vec![2]), 5, vec![6]),
                26,
                vec![5],
            ),
        ),
        (
            26,
            replace(replace(decimal_def(), 25, vec![2]), 26, vec![19]),
        ),
        (27, replace(def_fields(), 27, vec![2])),
    ];
    for (field, f) in cases {
        assert_eq!(decode_one(1, f), field_err(field), "field {field}");
    }
    // Counters, states, intervals and the diagnostic class.
    let counter = replace(
        replace(replace(def_fields(), 3, vec![2]), 10, vec![]),
        11,
        100_000u32.to_le_bytes().to_vec(),
    );
    assert!(decode_one(1, counter.clone()).is_ok());
    assert_eq!(
        decode_one(1, replace(counter.clone(), 10, vec![0; 4])),
        field_err(10)
    );
    assert_eq!(
        decode_one(1, replace(counter.clone(), 11, vec![0; 2])),
        field_err(11)
    );
    assert_eq!(decode_one(1, remove(counter, 10)), presence(10));
    let state = replace(replace(def_fields(), 4, vec![3]), 13, b"off".to_vec());
    assert!(decode_one(1, state.clone()).is_ok());
    let two = {
        let mut f = state.clone();
        let i = f.iter().position(|e| e.0 == 13).unwrap();
        f.insert(i + 1, (13, b"on".to_vec()));
        f
    };
    assert!(
        matches!(&decode_one(1, two).unwrap().entries[0], Entry::Definition(d) if d.state_names == ["off", "on"])
    );
    assert_eq!(
        decode_one(1, replace(state.clone(), 13, vec![])),
        field_err(13)
    );
    assert_eq!(
        decode_one(1, replace(state.clone(), 13, vec![b'n'; 64])),
        field_err(13)
    );
    assert_eq!(decode_one(1, remove(state, 13)), presence(13));
    let fixed = replace(
        replace(
            replace(replace(def_fields(), 3, vec![5]), 16, vec![0]),
            17,
            i64b(60),
        ),
        18,
        i64b(0),
    );
    assert!(decode_one(1, fixed.clone()).is_ok());
    assert_eq!(
        decode_one(1, replace(fixed.clone(), 16, vec![3])),
        field_err(16)
    );
    assert_eq!(
        decode_one(1, replace(fixed.clone(), 17, i64b(0))),
        field_err(17)
    );
    assert_eq!(
        decode_one(1, replace(fixed.clone(), 18, i64b(60))),
        field_err(18)
    );
    assert_eq!(
        decode_one(1, replace(fixed.clone(), 18, i64b(-1))),
        field_err(18)
    );
    assert_eq!(
        decode_one(1, replace(fixed.clone(), 19, b"UTC".to_vec())),
        presence(19)
    );
    assert_eq!(decode_one(1, remove(fixed, 17)), presence(17));
    let day = replace(
        replace(
            replace(replace(def_fields(), 3, vec![5]), 16, vec![1]),
            19,
            b"Europe/Oslo".to_vec(),
        ),
        22,
        i64b(5 * 3_600 * SECOND),
    );
    assert!(decode_one(1, day.clone()).is_ok());
    assert_eq!(
        decode_one(1, replace(day.clone(), 22, i64b(DAY))),
        field_err(22)
    );
    assert_eq!(
        decode_one(1, replace(day.clone(), 19, vec![])),
        field_err(19)
    );
    assert_eq!(
        decode_one(1, replace(day.clone(), 19, vec![b'z'; 64])),
        field_err(19)
    );
    assert_eq!(decode_one(1, remove(day, 22)), presence(22));
    let month = replace(
        replace(replace(def_fields(), 3, vec![5]), 16, vec![2]),
        19,
        b"Europe/Oslo".to_vec(),
    );
    assert!(decode_one(1, month.clone()).is_ok());
    assert_eq!(decode_one(1, replace(month, 22, i64b(0))), presence(22));
    let diag = replace(replace(def_fields(), 23, vec![1]), 24, i64b(30 * DAY));
    assert!(decode_one(1, diag.clone()).is_ok());
    assert_eq!(decode_one(1, remove(diag, 24)), presence(24));
    assert_eq!(
        decode_one(1, replace(def_fields(), 24, i64b(DAY))),
        presence(24)
    );
    // Presence of fields that apply only to some series.
    for (id, v) in [
        (5u16, vec![2]),
        (10, vec![]),
        (11, vec![0; 4]),
        (13, b"x".to_vec()),
        (16, vec![0]),
        (25, vec![0]),
    ] {
        assert_eq!(
            decode_one(1, replace(def_fields(), id, v)),
            presence(id),
            "field {id}"
        );
    }
    for id in [1u16, 2, 3, 4, 6, 7, 8, 9, 12, 14, 15, 23, 27] {
        assert_eq!(
            decode_one(1, remove(def_fields(), id)),
            presence(id),
            "field {id}"
        );
    }
    assert_eq!(decode_one(1, remove(decimal_def(), 5)), presence(5));
    assert_eq!(decode_one(1, remove(decimal_def(), 25)), presence(25));
    assert_eq!(
        decode_one(1, replace(decimal_def(), 25, vec![2])),
        presence(26)
    );
    assert_eq!(
        decode_one(1, replace(decimal_def(), 26, vec![4])),
        presence(26)
    );
}

#[test]
fn field_framing_order_and_unknown_elements() {
    use ContentError as E;
    let mut f = def_fields();
    f.swap(0, 1);
    assert_eq!(
        decode_one(1, f),
        Err(BodyError::Invalid(E::FieldOrder { entry: 0, field: 1 }))
    );
    let mut f = def_fields();
    f.insert(2, (2, b"again".to_vec()));
    assert_eq!(
        decode_one(1, f),
        Err(BodyError::Invalid(E::FieldOrder { entry: 0, field: 2 }))
    );
    // Unknown fields: critical refused, ignorable skipped (and not canonical).
    assert_eq!(
        decode_one(1, replace(def_fields(), 28, vec![])),
        Err(BodyError::Unsupported(Element::Field {
            entry_type: 1,
            field: 28
        }))
    );
    assert_eq!(
        decode_one(1, replace(def_fields(), 20, i64b(0))),
        Err(BodyError::Unsupported(Element::Field {
            entry_type: 1,
            field: 20
        }))
    );
    let ignorable = replace(def_fields(), 0x8001, vec![1, 2, 3]);
    assert!(decode_one(1, ignorable.clone()).is_ok());
    assert_eq!(
        EntriesBlock::decode_canonical(&entries_body(1, T0, &[(1, ignorable)])),
        Err(BodyError::NonCanonical)
    );
    // Unknown entry types; the routing change is not laid out yet.
    assert_eq!(
        decode_one(0x10, vec![]),
        Err(BodyError::Unsupported(Element::EntryType(0x10)))
    );
    assert_eq!(
        decode_one(5, vec![]),
        Err(BodyError::Unsupported(Element::EntryType(5)))
    );
    assert_eq!(
        decode_one(0, vec![]),
        Err(BodyError::Unsupported(Element::EntryType(0)))
    );
    let b = EntriesBlock::decode(&entries_body(
        1,
        T0,
        &[(0x90, vec![(1, vec![9])]), alias(1, "a")],
    ))
    .unwrap();
    assert_eq!(b.entries[0], Entry::Ignored { entry_type: 0x90 });
    assert_eq!(b.entries.len(), 2);
    // Overruns: an entry past the body, a field past its entry, an entry count past the entries.
    let body = entries_body(1, T0, &[alias(1, "a")]);
    let mut b = body.clone();
    b[24 + 4..24 + 8].copy_from_slice(&1_000u32.to_le_bytes());
    assert_eq!(
        EntriesBlock::decode(&b),
        Err(BodyError::Invalid(E::EntryOverrun { entry: 0 }))
    );
    let mut b = body.clone();
    b[24 + 8 + 2..24 + 8 + 4].copy_from_slice(&40u16.to_le_bytes());
    assert_eq!(
        EntriesBlock::decode(&b),
        Err(BodyError::Invalid(E::EntryOverrun { entry: 0 }))
    );
    let mut b = body.clone();
    b[24 + 4..24 + 8].copy_from_slice(&14u32.to_le_bytes());
    assert_eq!(
        EntriesBlock::decode(&b),
        Err(BodyError::Invalid(E::EntryOverrun { entry: 0 }))
    );
    let mut b = body.clone();
    b[16..20].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        EntriesBlock::decode(&b),
        Err(BodyError::Invalid(E::EntryOverrun { entry: 1 }))
    );
    // Non-zero entry reserved bytes and trailing bytes pass the checks, not the canonical decoder.
    let mut b = body.clone();
    b[25] = 1;
    assert!(EntriesBlock::decode(&b).is_ok());
    assert_eq!(
        EntriesBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
    let mut b = body;
    b.extend_from_slice(&[0; 8]);
    assert!(EntriesBlock::decode(&b).is_ok());
    assert_eq!(
        EntriesBlock::decode_canonical(&b),
        Err(BodyError::NonCanonical)
    );
}

#[test]
fn other_entry_types() {
    use ContentError as E;
    let u32b = |v: u32| v.to_le_bytes().to_vec();
    // Parameter change.
    let base = vec![
        (1, u32b(1)),
        (7, i64b(SECOND)),
        (8, i64b(3 * SECOND)),
        (20, i64b(T0)),
        (27, vec![0]),
    ];
    assert!(decode_one(2, base.clone()).is_ok());
    assert!(decode_one(2, vec![(1, u32b(1)), (23, vec![1]), (24, i64b(DAY))]).is_ok());
    assert_eq!(decode_one(2, remove(base.clone(), 27)), presence(27));
    assert_eq!(
        decode_one(2, remove(remove(base.clone(), 8), 27)),
        presence(8)
    );
    assert_eq!(decode_one(2, remove(base.clone(), 20)), presence(20));
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (20, i64b(T0)), (23, vec![0])]),
        presence(20)
    );
    for id in [2u16, 5, 13, 14, 19, 22, 25, 26] {
        assert_eq!(
            decode_one(2, replace(base.clone(), id, vec![1])),
            Err(BodyError::Invalid(E::FieldNotAllowed {
                entry: 0,
                field: id
            })),
            "field {id}"
        );
    }
    assert_eq!(
        decode_one(2, replace(base.clone(), 21, vec![1])),
        Err(BodyError::Unsupported(Element::Field {
            entry_type: 2,
            field: 21
        }))
    );
    assert_eq!(decode_one(2, replace(base, 8, i64b(-5))), field_err(8));
    // Alias, retirement, site zone, tzdata, scale raise.
    assert_eq!(decode_one(3, vec![(1, u32b(1))]), presence(2));
    assert_eq!(
        decode_one(4, vec![(1, u32b(1)), (21, vec![0; 7])]),
        field_err(21)
    );
    assert!(decode_one(6, vec![(20, i64b(0)), (30, b"Europe/Oslo".to_vec())]).is_ok());
    assert_eq!(
        decode_one(6, vec![(20, i64b(0)), (30, vec![])]),
        field_err(30)
    );
    assert!(decode_one(7, vec![(31, b"2026b".to_vec())]).is_ok());
    assert_eq!(decode_one(7, vec![(31, b"2026 b".to_vec())]), field_err(31));
    assert_eq!(decode_one(7, vec![(31, vec![b'9'; 16])]), field_err(31));
    assert_eq!(
        decode_one(9, vec![(1, u32b(1)), (5, vec![19])]),
        field_err(5)
    );
    // Recovery.
    let rec = vec![
        (40, 5u64.to_le_bytes().to_vec()),
        (42, vec![7; 16]),
        (43, 100u64.to_le_bytes().to_vec()),
        (44, u32b(1)),
        (45, vec![3]),
        (46, u32b(9)),
        (47, b"alerts".to_vec()),
        (47, b"db-sync".to_vec()),
    ];
    let b = decode_one(8, rec.clone()).unwrap();
    assert!(
        matches!(&b.entries[0], Entry::Recovery(r) if r.mark_state == MarkState::Stale && r.consumers.len() == 2)
    );
    assert!(decode_one(8, replace(rec.clone(), 41, vec![1, 2])).is_ok());
    assert_eq!(
        decode_one(8, replace(rec.clone(), 45, vec![4])),
        field_err(45)
    );
    assert_eq!(
        decode_one(8, replace(rec.clone(), 42, vec![7; 15])),
        field_err(42)
    );
    assert_eq!(
        decode_one(8, replace(rec.clone(), 47, b"Alerts".to_vec())),
        field_err(47)
    );
    assert_eq!(decode_one(8, remove(rec, 46)), presence(46));
}

#[test]
fn entries_encode_refuses_what_it_cannot_write() {
    let one = |e| {
        EntriesBlock {
            first_sequence: 1,
            received_at: 0,
            entries: vec![e],
        }
        .encode()
    };
    assert!(matches!(
        one(Entry::Alias {
            id: 1,
            name: String::new()
        }),
        Err(EncodeError::Content(ContentError::Field { field: 2, .. }))
    ));
    assert!(matches!(
        one(Entry::ScaleRaise { id: 1, scale: 19 }),
        Err(EncodeError::Content(ContentError::Field { field: 5, .. }))
    ));
    assert!(matches!(
        one(Entry::ParameterChange(ParameterChange {
            id: 1,
            lateness_bound: Some(1),
            ..Default::default()
        })),
        Err(EncodeError::Content(ContentError::FieldPresence {
            field: 20,
            ..
        }))
    ));
    assert!(matches!(
        one(Entry::Ignored { entry_type: 0x90 }),
        Err(EncodeError::Content(_))
    ));
    // An entry above 8 MiB.
    let big = Recovery {
        after: 0,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: vec!["c".repeat(64); 8 * 1024 * 1024 / 68 + 1],
    };
    assert_eq!(one(Entry::Recovery(big)), Err(EncodeError::TooLarge));
}

// ---------------------------------------------------------------------------------------------------------------
// Batches (A.4, A.7): block splitting, the recovery entry, size limits, and the checks applied by the reader.

fn fact(v: Value) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t: T0,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: v,
        changed_at: None,
    })
}

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
        boot_id: [1; 16],
        tail_bytes: 40,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 1,
        consumers: vec![],
    })
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

// ---------------------------------------------------------------------------------------------------------------
// Near-valid bodies: encoder output with a few byte edits or a cut, fed straight to the body decoders (A.11).

fn mutate(
    mut b: Vec<u8>,
    edits: &[(prop::sample::Index, u8)],
    cut: Option<prop::sample::Index>,
) -> Vec<u8> {
    for (at, v) in edits {
        let i = at.index(b.len());
        b[i] = *v;
    }
    if let Some(c) = cut {
        b.truncate(c.index(b.len() + 1));
    }
    b
}

proptest! {
    #[test]
    fn mutated_bodies_never_panic_and_what_decodes_encodes_again(
        rows in rows(12),
        entries in prop::collection::vec(prop_oneof![5 => entry(), 1 => recovery().prop_map(Entry::Recovery)], 1..5),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
        cut in prop::option::weighted(0.2, any::<prop::sample::Index>()),
    ) {
        let f = mutate(FactsBlock { first_sequence: 3, received_at: T0, rows }.encode().unwrap(), &edits, cut);
        if let Ok(d) = FactsBlock::decode(&f) {
            let again = d.encode().unwrap();
            prop_assert_eq!(FactsBlock::decode(&again).unwrap(), d.clone());
            prop_assert_eq!(FactsBlock::decode_canonical(&f).is_ok(), again == f);
        }
        let e = mutate(EntriesBlock { first_sequence: 3, received_at: T0, entries }.encode().unwrap(), &edits, cut);
        if let Ok(d) = EntriesBlock::decode(&e)
            && !d.entries.iter().any(|x| matches!(x, Entry::Ignored { .. }))
        {
            let again = d.encode().unwrap();
            prop_assert_eq!(EntriesBlock::decode(&again).unwrap(), d.clone());
            prop_assert_eq!(EntriesBlock::decode_canonical(&e).is_ok(), again == e);
        }
    }
}

#[test]
fn a_refusal_wins_over_invalid_content() {
    // An unknown critical field repeated (out of order for a known field) is a version refusal, not a lost range.
    let mut f = def_fields();
    f.insert(2, (0x0100, vec![]));
    f.insert(3, (0x0100, vec![]));
    assert_eq!(
        decode_one(1, f),
        Err(BodyError::Unsupported(Element::Field {
            entry_type: 1,
            field: 0x0100
        }))
    );
    // Undefined flag bits on a later row win over an out-of-range value on an earlier one.
    let r = with(2, |c| {
        insert(c, 3, 0, vec![4, 0]);
        insert(c, 4, 0, vec![0, 0, 0, 0x80]);
    });
    assert_eq!(r, Err(BodyError::Unsupported(Element::FactFlags(0x8000))));
}
