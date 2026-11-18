//! The checks of `format.md` A.6 on entries bodies: field ranges, framing, order and unknown elements, every entry
//! type, routing changes and what the encoder refuses.

mod support;

use kvik_log::entries::{
    DAY, EntriesBlock, Entry, MarkState, ParameterChange, Recovery, RouteCondition, RouteOp,
    Routing, Selection,
};
use kvik_log::{BodyError, ContentError, Element, EncodeError};
use support::checks::*;
use support::*;

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
        matches!(&decode_one(1, two.clone()).unwrap().entries[0], Entry::Definition(d) if d.state_names == ["off", "on"])
    );
    let twice = {
        let mut f = two;
        let i = f.iter().rposition(|e| e.0 == 13).unwrap();
        f[i].1 = b"off".to_vec();
        f
    };
    assert_eq!(decode_one(1, twice), field_err(13));
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
    // Unknown entry types. The routing change (type 5) is laid out since roadmap 5.2, superseding Q-106 (f): an
    // empty one is invalid content, not a version refusal.
    assert_eq!(
        decode_one(0x10, vec![]),
        Err(BodyError::Unsupported(Element::EntryType(0x10)))
    );
    assert_eq!(decode_one(5, vec![]), presence(50));
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
    assert_eq!(b.entries[0], Entry::Ignored { count: 1 });
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
    // Fields 25 and 26 take effect at the entry's position (`format.md` A.6, Q-100), with no field 20.
    assert!(decode_one(2, vec![(1, u32b(1)), (25, vec![2]), (26, vec![6])]).is_ok());
    assert!(decode_one(2, vec![(1, u32b(1)), (25, vec![0])]).is_ok());
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (25, vec![2])]),
        presence(26)
    );
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (25, vec![1]), (26, vec![6])]),
        presence(26)
    );
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (26, vec![6])]),
        presence(26)
    );
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (25, vec![2]), (26, vec![19])]),
        field_err(26)
    );
    assert_eq!(
        decode_one(2, vec![(1, u32b(1)), (20, i64b(T0)), (25, vec![0])]),
        presence(20)
    );
    for id in [2u16, 5, 13, 14, 19, 22] {
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
    // Field 41, the reset stamp: 16 bytes with a non-zero nonce, or absent.
    assert!(matches!(&b.entries[0], Entry::Recovery(r) if r.stamp.is_none()));
    assert_eq!(
        decode_one(8, replace(rec.clone(), 41, vec![1, 2])),
        field_err(41)
    );
    let mut st = 7u64.to_le_bytes().to_vec();
    st.extend_from_slice(&0u64.to_le_bytes());
    assert_eq!(
        decode_one(8, replace(rec.clone(), 41, st.clone())),
        field_err(41)
    );
    st[8] = 5;
    let b = decode_one(8, replace(rec.clone(), 41, st)).unwrap();
    assert!(matches!(&b.entries[0], Entry::Recovery(r)
        if r.stamp == Some(kvik_log::mark::ResetStamp { sequence: 7, nonce: 5 })));
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

fn routing_fields() -> Fields {
    vec![
        (50, b"alarms".to_vec()),
        (51, vec![1]),
        (52, 3u32.to_le_bytes().to_vec()),
        (53, vec![2]),
        (54, 7u32.to_le_bytes().to_vec()),
        (55, b"plant/*/power".to_vec()),
        (60, vec![1]),
        (61, 9u32.to_le_bytes().to_vec()),
        (61, 10u32.to_le_bytes().to_vec()),
        (63, vec![1]),
        (64, 3000f64.to_bits().to_le_bytes().to_vec()),
        (65, 2500f64.to_bits().to_le_bytes().to_vec()),
    ]
}

#[test]
fn routing_change_layout() {
    // A funnel added with a condition over a pattern resolved once, and `emits` of two ids.
    let b = decode_one(5, routing_fields()).unwrap();
    let expected = Routing {
        consumer: "alarms".into(),
        op: RouteOp::Add,
        funnel: Some(3),
        selection: Some(Selection::Pattern {
            pattern: "plant/*/power".into(),
            resolved: vec![7],
        }),
        operator: None,
        emits: Selection::Ids(vec![9, 10]),
        condition: Some(RouteCondition::Threshold {
            on: 3000.0,
            clear: 2500.0,
        }),
    };
    assert_eq!(b.entries, vec![Entry::Routing(Box::new(expected.clone()))]);
    let body = EntriesBlock {
        first_sequence: 1,
        received_at: T0,
        entries: b.entries.clone(),
    }
    .encode()
    .unwrap();
    assert_eq!(body, entries_body(1, T0, &[(5, routing_fields())]));
    // Checks: the consumer name, the change kind, field presence per kind, ranges.
    assert_eq!(
        decode_one(5, replace(routing_fields(), 50, b"Alarms".to_vec())),
        field_err(50)
    );
    assert_eq!(
        decode_one(5, replace(routing_fields(), 51, vec![4])),
        field_err(51)
    );
    assert_eq!(decode_one(5, remove(routing_fields(), 52)), presence(52));
    assert_eq!(decode_one(5, remove(routing_fields(), 53)), presence(53));
    assert_eq!(decode_one(5, remove(routing_fields(), 60)), presence(60));
    assert_eq!(
        decode_one(
            5,
            replace(
                routing_fields(),
                65,
                3001f64.to_bits().to_le_bytes().to_vec()
            )
        ),
        field_err(65)
    );
    assert_eq!(
        decode_one(
            5,
            replace(
                routing_fields(),
                64,
                f64::NAN.to_bits().to_le_bytes().to_vec()
            )
        ),
        field_err(64)
    );
    assert_eq!(decode_one(5, remove(routing_fields(), 65)), presence(65));
    assert_eq!(
        decode_one(5, replace(routing_fields(), 66, i64b(1))),
        presence(66)
    );
    // `emits` never carries resolved ids with a pattern.
    let mut f = remove(routing_fields(), 61);
    f = replace(f, 60, vec![2]);
    f = replace(f, 61, 1u32.to_le_bytes().to_vec());
    f = replace(f, 62, b"a/*/b".to_vec());
    assert_eq!(decode_one(5, f), presence(61));
    // A removal carries the funnel id and `emits` only.
    let removal = vec![
        (50, b"alarms".to_vec()),
        (51, vec![2]),
        (52, 3u32.to_le_bytes().to_vec()),
        (60, vec![0]),
    ];
    assert!(decode_one(5, removal.clone()).is_ok());
    assert_eq!(
        decode_one(5, replace(removal.clone(), 53, vec![3])),
        presence(53)
    );
    // An `emits` declaration carries no funnel id.
    let emits = replace(remove(removal, 52), 51, vec![3]);
    assert!(decode_one(5, emits.clone()).is_ok());
    assert_eq!(
        decode_one(5, replace(emits, 52, 1u32.to_le_bytes().to_vec())),
        presence(52)
    );
    // An operator: width positive, the state argument only for time in state.
    let op = |f: Fields| replace(replace(f, 56, vec![1]), 57, i64b(60_000_000_000));
    assert!(decode_one(5, op(routing_fields())).is_ok());
    assert_eq!(
        decode_one(5, replace(op(routing_fields()), 57, i64b(0))),
        field_err(57)
    );
    assert_eq!(
        decode_one(5, replace(op(routing_fields()), 58, vec![0, 1, 0, 0, 0])),
        presence(58)
    );
    assert_eq!(
        decode_one(5, replace(op(routing_fields()), 56, vec![10])),
        presence(58)
    );
    assert!(
        decode_one(
            5,
            replace(
                replace(op(routing_fields()), 56, vec![10]),
                58,
                vec![1, 4, 0, 0, 0]
            )
        )
        .is_ok()
    );
    assert_eq!(
        decode_one(5, remove(op(routing_fields()), 57)),
        presence(57)
    );
    // Unknown critical fields of the type are refused.
    assert_eq!(
        decode_one(5, replace(routing_fields(), 59, vec![])),
        Err(BodyError::Unsupported(Element::Field {
            entry_type: 5,
            field: 59
        }))
    );
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
        one(Entry::Ignored { count: 1 }),
        Err(EncodeError::Content(_))
    ));
    // An entry above 8 MiB.
    let big = Recovery {
        after: 0,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: vec!["c".repeat(64); 8 * 1024 * 1024 / 68 + 1],
    };
    assert_eq!(one(Entry::Recovery(big)), Err(EncodeError::TooLarge));
}
