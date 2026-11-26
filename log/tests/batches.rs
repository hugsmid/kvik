//! Arrival batches (roadmap 4.3): facts and entries bodies — round trips, canonical form and near-valid bodies. The
//! checks of `format.md` A.5 and A.6 and batch assembly have their own files.

mod support;

use kvik_log::batch::BatchBuilder;
use kvik_log::block::block_type;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{
    CounterRules, DAY, Definition, EntriesBlock, Entry, ExcessPrecision, GATE_LEVELS, GATE_MEMBERS,
    GateKind, GateLevel, GateSelector, GateSpec, GateValue, IntervalLength, Kind, MarkState,
    ParameterChange, Recovery, Retention, RouteCondition, RouteGate, RouteOp, RouteOperator,
    Routing, Selection, StateArg, Timing, Versioning,
};
use kvik_log::facts::{FactFlags, FactRow, FactsBlock, Quality, Value, ValueType};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::segment::SegmentImage;
use kvik_log::{BodyError, Element, EncodeError};
use proptest::prelude::*;
use support::checks::*;
use support::*;

// ---------------------------------------------------------------------------------------------------------------
// Strategies: every value valid by construction.

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u64>().prop_map(Value::F64),
        ((i64::MIN + 1)..=i64::MAX, 0u8..=18)
            .prop_map(|(units, scale)| Value::Decimal { units, scale }),
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
            prop::collection::btree_set("[A-Za-z_]{1,63}", 1..5)
                .prop_map(|names| names.into_iter().collect::<Vec<_>>()),
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
        (
            prop::option::of(0u8..=1),
            prop::option::of(any::<i64>()),
            prop::option::of(prop_oneof![
                Just(ExcessPrecision::Round),
                Just(ExcessPrecision::Refuse),
                (0u8..=18).prop_map(|max_scale| ExcessPrecision::AutoRaise { max_scale }),
            ]),
        ),
    )
        .prop_map(
            |(id, eff, ei, s, (roc, wrap, ppm, late), (class, period, excess))| {
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
                    excess_precision: excess,
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
        (
            prop::collection::vec("[a-z0-9][a-z0-9._-]{0,63}", 0..4),
            prop::option::of((any::<u64>(), 1..=u64::MAX)),
        ),
    )
        .prop_map(
            |(after, boot_id, tail_bytes, readable_commits, m, id_floor, (consumers, stamp))| {
                Recovery {
                    after,
                    stamp: stamp
                        .map(|(sequence, nonce)| kvik_log::mark::ResetStamp { sequence, nonce }),
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
                }
            },
        )
}

fn selection(resolved: bool) -> impl Strategy<Value = Selection> {
    let ids = prop::collection::vec(any::<u32>(), 0..4);
    prop_oneof![
        Just(Selection::None),
        ids.clone().prop_map(Selection::Ids),
        (name(), ids).prop_map(move |(pattern, r)| Selection::Pattern {
            pattern,
            resolved: if resolved { r } else { Vec::new() },
        }),
        Just(Selection::All),
    ]
}

fn level() -> impl Strategy<Value = f64> {
    -1e12f64..1e12
}

fn route_condition() -> impl Strategy<Value = RouteCondition> {
    prop_oneof![
        (level(), 0f64..1e6).prop_map(|(on, d)| RouteCondition::Threshold { on, clear: on - d }),
        (0f64..1e6).prop_map(|deadband| RouteCondition::OnChange { deadband }),
        (level(), 0..i64::MAX).prop_map(|(above, n)| RouteCondition::HeldFor { above, n }),
        (0..i64::MAX).prop_map(|n| RouteCondition::StaleFor { n }),
    ]
}

fn route_operator() -> impl Strategy<Value = RouteOperator> {
    (1u8..=10, positive(), any::<bool>(), any::<u32>()).prop_map(|(aggregate, width, b, i)| {
        RouteOperator {
            aggregate,
            width,
            state: (aggregate == 10).then_some(if i % 2 == 0 {
                StateArg::Bool(b)
            } else {
                StateArg::State(i)
            }),
        }
    })
}

fn gate_value() -> impl Strategy<Value = GateValue> {
    prop_oneof![
        level().prop_map(GateValue::F64),
        (any::<i64>(), any::<u8>()).prop_map(|(units, scale)| GateValue::Decimal { units, scale }),
        any::<u32>().prop_map(GateValue::State),
        any::<bool>().prop_map(GateValue::Bool),
        "[a-z ]{1,40}".prop_map(GateValue::Text),
    ]
}

/// Rising boundaries by construction: each `at` and `clear` is the previous one plus a positive step.
fn gate_levels() -> impl Strategy<Value = Vec<GateLevel>> {
    (
        level(),
        prop::collection::vec(
            (1f64..1e6, 0f64..1e3, 0..i64::MAX, 0..i64::MAX),
            1..=GATE_LEVELS,
        ),
    )
        .prop_map(|(base, steps)| {
            let mut at = base;
            steps
                .into_iter()
                .map(|(step, gap, hold_up, hold_down)| {
                    at += step;
                    GateLevel {
                        at,
                        clear: at - gap.min(step / 2.0),
                        hold_up,
                        hold_down,
                    }
                })
                .collect()
        })
}

fn gate_kind() -> impl Strategy<Value = GateKind> {
    prop_oneof![
        gate_levels().prop_map(GateKind::Levels),
        (
            prop::collection::vec(gate_value(), 1..=GATE_MEMBERS),
            0..i64::MAX,
            0..i64::MAX
        )
            .prop_map(|(members, hold_on, hold_off)| GateKind::Set {
                members,
                hold_on,
                hold_off
            }),
        (0f64..1e6).prop_map(|deadband| GateKind::Change { deadband }),
    ]
}

fn gate_spec() -> impl Strategy<Value = GateSpec> {
    (
        "[a-z0-9][a-z0-9_-]{0,63}",
        prop_oneof![
            any::<u32>().prop_map(GateSelector::One),
            "[a-z]{1,8}/\\*".prop_map(GateSelector::Each),
        ],
        gate_kind(),
        prop::option::of(1..i64::MAX),
    )
        .prop_map(|(name, selector, kind, stale_after)| GateSpec {
            name,
            selector,
            input: 1,
            kind,
            stale_after,
        })
}

fn routing() -> impl Strategy<Value = Routing> {
    (
        "[a-z0-9][a-z0-9._-]{0,63}",
        0u8..5,
        any::<u32>(),
        selection(true),
        prop::option::of(route_operator()),
        selection(false),
        prop::option::of(route_condition()),
        gate_spec(),
    )
        .prop_map(
            |(consumer, op, funnel, sel, operator, emits, condition, spec)| {
                let op = [
                    RouteOp::Add,
                    RouteOp::Remove,
                    RouteOp::Emits,
                    RouteOp::Watch,
                    RouteOp::Unwatch,
                ][usize::from(op)];
                let add = op == RouteOp::Add;
                let gate = matches!(op, RouteOp::Watch | RouteOp::Unwatch).then(|| {
                    Box::new(RouteGate {
                        id: funnel,
                        spec: (op == RouteOp::Watch).then_some(spec),
                    })
                });
                Routing {
                    consumer,
                    op,
                    funnel: matches!(op, RouteOp::Add | RouteOp::Remove).then_some(funnel),
                    selection: add.then_some(sel),
                    operator: operator.filter(|_| add),
                    emits,
                    condition: condition.filter(|_| add),
                    gate,
                }
            },
        )
}

/// Every entry type except the recovery entry, which is alone in its batch.
fn entry() -> impl Strategy<Value = Entry> {
    prop_oneof![
        routing().prop_map(|r| Entry::Routing(Box::new(r))),
        definition().prop_map(|d| Entry::Definition(Box::new(d))),
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
        1 => recovery().prop_map(|r| vec![Item::from(Entry::Recovery(r))]),
        6 => prop::collection::vec(prop_oneof![row().prop_map(Item::Fact), entry().prop_map(Item::from)], 1..30),
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
            Entry::Definition(Box::new(def)),
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

#[test]
fn ignored_entries_are_counted_not_stored() {
    // Nearly 8 MiB of zero-length entries of an ignorable type (as many as fit a batch): one value, and the reader's
    // check keeps none.
    let n = ((8 << 20) - 16 - 24) / 8;
    let mut body = vec![0u8; 24];
    kvik_log::body::DataPrefix {
        first_sequence: 1,
        received_at: T0,
        count: n as u32,
    }
    .write(&mut body);
    for _ in 0..n {
        body.extend_from_slice(&[0x90, 0, 0, 0, 0, 0, 0, 0]);
    }
    let b = EntriesBlock::decode(&body).unwrap();
    assert_eq!(b.entries, [Entry::Ignored { count: n as u32 }]);
    assert_eq!(b.entries[0].sequences(), n as u64);
    let mut builder = BatchBuilder::new();
    builder.push(block_type::ENTRIES, body).unwrap();
    let mut img = SegmentImage::create_ahead(ID, T0, (9 << 20) + 4096).unwrap();
    img.take_into_use(1).unwrap();
    img.append(&builder, T0).unwrap();
    let r = scan(img.bytes(), &ID, ScanLimit::End).unwrap();
    let c = BatchContent::decode(&r.batches[0]).unwrap();
    assert_eq!(c.last_sequence(), n as u64);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Long runs of equal rows: 2- and 3-byte run lengths, and runs that cross the 65,536-row and the 8 MiB
    /// plain-encoded block limits.
    #[test]
    fn long_runs_split_at_both_block_limits(
        runs in prop::collection::vec(
            (
                prop_oneof![Just(Value::Bool(true)), Just(Value::Text("t".repeat(255))), Just(Value::F64(7))],
                prop_oneof![1usize..4, 128usize..400, 16_384usize..17_000, 29_000usize..29_300, 65_500usize..65_600],
            ),
            1..3,
        ),
    ) {
        let items: Vec<Item> = runs.iter().flat_map(|(v, n)| std::iter::repeat_n(fact(v.clone()), *n)).collect();
        let content = BatchContent { first_sequence: 1, received_at: T0, items: items.clone() };
        let blocks = content.blocks().unwrap();
        let mut rows = Vec::new();
        let mut next = 1u64;
        for (k, b) in blocks.iter().enumerate() {
            let kvik_log::content::DataBody::Facts(f) = b else { panic!("only facts") };
            prop_assert_eq!(f.first_sequence, next);
            let plain: u64 = f.rows.iter().map(FactRow::plain_len).sum();
            prop_assert!(f.rows.len() <= 65_536 && plain <= 8 << 20);
            // Every block but the last is as full as the limits allow.
            if let Some(kvik_log::content::DataBody::Facts(n)) = blocks.get(k + 1) {
                prop_assert!(f.rows.len() == 65_536 || plain + n.rows[0].plain_len() > 8 << 20);
            }
            let body = f.encode().unwrap();
            prop_assert_eq!(&FactsBlock::decode_canonical(&body).unwrap(), f);
            next += f.rows.len() as u64;
            rows.extend(f.rows.iter().cloned().map(Item::Fact));
        }
        prop_assert_eq!(rows, items);
        // Within 8 MiB of data blocks the batch is framed; above it the writer refuses it.
        match content.builder() {
            Ok(b) => prop_assert!(b.encode(&ID, 64, T0).is_ok()),
            Err(e) => prop_assert_eq!(e, EncodeError::TooLarge),
        }
    }
}
