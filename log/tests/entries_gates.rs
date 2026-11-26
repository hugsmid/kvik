//! Routing changes 4 (watch a gate) and 5 (unwatch one): the layout of `format.md` A.6 fields 67 to 77, the byte
//! vectors, and every check the decoder makes on them.

mod support;

use kvik_log::entries::{
    EntriesBlock, Entry, GateKind, GateLevel, GateSelector, GateSpec, GateValue, RouteGate,
    RouteOp, Routing, Selection,
};
use kvik_log::{BodyError, ContentError, Element};
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

fn push(mut f: Fields, id: u16, v: Vec<u8>) -> Fields {
    let at = f.iter().position(|e| e.0 > id).unwrap_or(f.len());
    f.insert(at, (id, v));
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

fn f64b(v: f64) -> Vec<u8> {
    v.to_bits().to_le_bytes().to_vec()
}

fn i64b(v: i64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn level(at: f64, clear: f64, up: i64, down: i64) -> Vec<u8> {
    [f64b(at), f64b(clear), i64b(up), i64b(down)].concat()
}

const MIN: i64 = 60_000_000_000;

/// A levels gate on series 7 with two boundaries and a staleness limit.
fn watch_levels() -> Fields {
    vec![
        (50, b"hall".to_vec()),
        (51, vec![4]),
        (54, 7u32.to_le_bytes().to_vec()),
        (60, vec![0]),
        (67, 2u32.to_le_bytes().to_vec()),
        (68, b"humidity".to_vec()),
        (69, vec![1]),
        (71, vec![1]),
        (72, vec![1]),
        (73, level(70.0, 65.0, 2 * MIN, 0)),
        (73, level(85.0, 80.0, 0, 0)),
        (77, i64b(10 * MIN)),
    ]
}

/// A set gate on each series of a pattern, one member of each value form.
fn watch_set() -> Fields {
    vec![
        (50, b"hall".to_vec()),
        (51, vec![4]),
        (55, b"plant/*/code".to_vec()),
        (60, vec![0]),
        (67, 3u32.to_le_bytes().to_vec()),
        (68, b"faults".to_vec()),
        (69, vec![2]),
        (71, vec![1]),
        (72, vec![2]),
        (74, [vec![1], f64b(0.0)].concat()),
        (74, [vec![2], i64b(-125), vec![2]].concat()),
        (74, [vec![3], 4u32.to_le_bytes().to_vec()].concat()),
        (74, vec![4, 1]),
        (74, [vec![5], b"stop".to_vec()].concat()),
        (75, [i64b(MIN), i64b(0)].concat()),
    ]
}

fn watch_change() -> Fields {
    vec![
        (50, b"hall".to_vec()),
        (51, vec![4]),
        (54, 9u32.to_le_bytes().to_vec()),
        (60, vec![0]),
        (67, 4u32.to_le_bytes().to_vec()),
        (68, b"power".to_vec()),
        (69, vec![1]),
        (71, vec![1]),
        (72, vec![3]),
        (76, f64b(0.5)),
    ]
}

fn unwatch() -> Fields {
    vec![
        (50, b"hall".to_vec()),
        (51, vec![5]),
        (60, vec![0]),
        (67, 2u32.to_le_bytes().to_vec()),
    ]
}

fn routing(op: RouteOp, gate: RouteGate) -> Routing {
    Routing {
        consumer: "hall".into(),
        op,
        funnel: None,
        selection: None,
        operator: None,
        emits: Selection::None,
        condition: None,
        gate: Some(Box::new(gate)),
    }
}

fn expected_levels() -> Routing {
    routing(
        RouteOp::Watch,
        RouteGate {
            id: 2,
            spec: Some(GateSpec {
                name: "humidity".into(),
                selector: GateSelector::One(7),
                input: 1,
                kind: GateKind::Levels(vec![
                    GateLevel {
                        at: 70.0,
                        clear: 65.0,
                        hold_up: 2 * MIN,
                        hold_down: 0,
                    },
                    GateLevel {
                        at: 85.0,
                        clear: 80.0,
                        hold_up: 0,
                        hold_down: 0,
                    },
                ]),
                stale_after: Some(10 * MIN),
            }),
        },
    )
}

fn expected_set() -> Routing {
    routing(
        RouteOp::Watch,
        RouteGate {
            id: 3,
            spec: Some(GateSpec {
                name: "faults".into(),
                selector: GateSelector::Each("plant/*/code".into()),
                input: 1,
                kind: GateKind::Set {
                    members: vec![
                        GateValue::F64(0.0),
                        GateValue::Decimal {
                            units: -125,
                            scale: 2,
                        },
                        GateValue::State(4),
                        GateValue::Bool(true),
                        GateValue::Text("stop".into()),
                    ],
                    hold_on: MIN,
                    hold_off: 0,
                },
                stale_after: None,
            }),
        },
    )
}

fn expected_change() -> Routing {
    routing(
        RouteOp::Watch,
        RouteGate {
            id: 4,
            spec: Some(GateSpec {
                name: "power".into(),
                selector: GateSelector::One(9),
                input: 1,
                kind: GateKind::Change { deadband: 0.5 },
                stale_after: None,
            }),
        },
    )
}

/// Decodes to `expected` and encodes back to exactly the same bytes.
fn vector(fields: Fields, expected: Routing) {
    let body = entries_body(1, T0, &[(5, fields)]);
    let b = EntriesBlock::decode(&body).unwrap();
    assert_eq!(b.entries, vec![Entry::Routing(Box::new(expected))]);
    let again = EntriesBlock {
        first_sequence: 1,
        received_at: T0,
        entries: b.entries,
    }
    .encode()
    .unwrap();
    assert_eq!(again, body);
}

#[test]
fn watch_and_unwatch_vectors_decode_and_encode_byte_for_byte() {
    vector(watch_levels(), expected_levels());
    vector(watch_set(), expected_set());
    vector(watch_change(), expected_change());
    vector(
        unwatch(),
        routing(RouteOp::Unwatch, RouteGate { id: 2, spec: None }),
    );
}

#[test]
fn watch_vector_bytes_match_format_md() {
    let body = entries_body(1, T0, &[(5, watch_change())]);
    let hex: String = body[24..].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        concat!(
            "05000000",                 // type 5, reserved
            "58000000",                 // length 88
            "3200040068616c6c",         // 50 consumer "hall"
            "3300010004000000",         // 51 change 4, watch
            "3600040009000000",         // 54 series 9
            "3c00010000000000",         // 60 emits none
            "4300040004000000",         // 67 gate id 4
            "44000500706f776572000000", // 68 name "power"
            "4500010001000000",         // 69 one series
            "4700010001000000",         // 71 input value
            "4800010003000000",         // 72 kind change
            "4c000800000000000000e03f", // 76 dead-band 0.5
        )
    );
}

#[test]
fn change_code_four_is_a_watch_and_five_an_unwatch() {
    let b = decode_one(5, watch_change()).unwrap();
    let Entry::Routing(r) = &b.entries[0] else {
        panic!("a routing change")
    };
    assert_eq!(r.op, RouteOp::Watch);
    let b = decode_one(5, unwatch()).unwrap();
    let Entry::Routing(r) = &b.entries[0] else {
        panic!("a routing change")
    };
    assert_eq!(r.op, RouteOp::Unwatch);
    assert_eq!(
        decode_one(5, replace(unwatch(), 51, vec![6])),
        field_err(51)
    );
}

#[test]
fn a_watch_needs_its_fields_and_carries_no_funnel_fields() {
    for id in [67, 68, 69, 71, 72] {
        assert_eq!(decode_one(5, remove(watch_change(), id)), presence(id));
    }
    assert_eq!(decode_one(5, remove(watch_change(), 54)), presence(54));
    assert_eq!(decode_one(5, remove(watch_change(), 76)), presence(76));
    assert_eq!(decode_one(5, remove(watch_levels(), 73)), presence(73));
    assert_eq!(decode_one(5, remove(watch_set(), 74)), presence(74));
    assert_eq!(decode_one(5, remove(watch_set(), 75)), presence(75));
    assert_eq!(
        decode_one(5, replace(watch_change(), 52, 1u32.to_le_bytes().to_vec())),
        presence(52)
    );
    assert_eq!(
        decode_one(5, replace(watch_change(), 53, vec![1])),
        presence(53)
    );
    assert_eq!(
        decode_one(5, replace(watch_change(), 63, vec![2])),
        presence(63)
    );
    // Kind-specific fields only with their kind.
    assert_eq!(
        decode_one(5, replace(watch_change(), 73, level(1.0, 1.0, 0, 0))),
        presence(73)
    );
    assert_eq!(
        decode_one(5, replace(watch_levels(), 76, f64b(0.0))),
        presence(76)
    );
    assert_eq!(
        decode_one(5, replace(watch_levels(), 75, [i64b(0), i64b(0)].concat())),
        presence(75)
    );
    assert_eq!(
        decode_one(5, replace(watch_set(), 73, level(1.0, 1.0, 0, 0))),
        presence(73)
    );
    // The selector's series field matches its kind.
    assert_eq!(
        decode_one(5, replace(watch_change(), 55, b"a/*".to_vec())),
        presence(55)
    );
    assert_eq!(
        decode_one(5, replace(watch_set(), 54, 1u32.to_le_bytes().to_vec())),
        presence(54)
    );
    assert_eq!(
        decode_one(5, push(watch_change(), 54, 10u32.to_le_bytes().to_vec())),
        presence(54)
    );
}

#[test]
fn an_unwatch_carries_the_gate_id_alone() {
    assert_eq!(decode_one(5, remove(unwatch(), 67)), presence(67));
    for (id, v) in [
        (68, b"humidity".to_vec()),
        (69, vec![1]),
        (71, vec![1]),
        (72, vec![3]),
        (76, f64b(0.0)),
        (77, i64b(1)),
    ] {
        assert_eq!(decode_one(5, replace(unwatch(), id, v)), presence(id));
    }
    assert_eq!(
        decode_one(5, replace(unwatch(), 54, 1u32.to_le_bytes().to_vec())),
        presence(54)
    );
    assert_eq!(
        decode_one(5, replace(unwatch(), 52, 1u32.to_le_bytes().to_vec())),
        presence(52)
    );
}

#[test]
fn gate_fields_are_refused_on_funnel_changes() {
    let removal = vec![
        (50, b"hall".to_vec()),
        (51, vec![2]),
        (52, 3u32.to_le_bytes().to_vec()),
        (60, vec![0]),
    ];
    assert!(decode_one(5, removal.clone()).is_ok());
    assert_eq!(
        decode_one(5, replace(removal, 67, 1u32.to_le_bytes().to_vec())),
        presence(67)
    );
}

#[test]
fn gate_field_values_are_range_checked() {
    let bad = |f: Fields, id: u16| assert_eq!(decode_one(5, f), field_err(id));
    bad(replace(watch_change(), 68, vec![b'a'; 65]), 68);
    bad(replace(watch_change(), 69, vec![3]), 69);
    bad(replace(watch_change(), 71, vec![2]), 71);
    bad(replace(watch_change(), 72, vec![4]), 72);
    bad(replace(watch_change(), 76, f64b(-0.1)), 76);
    bad(replace(watch_change(), 76, f64b(f64::INFINITY)), 76);
    bad(replace(watch_change(), 77, i64b(0)), 77);
    bad(replace(watch_change(), 77, i64b(-1)), 77);
    // Levels: finite, clear at or below at, holds not negative, rising, at most 16.
    let levels = |ls: Vec<Vec<u8>>| {
        let mut f = remove(watch_levels(), 73);
        for l in ls {
            f = push(f, 73, l);
        }
        f
    };
    bad(levels(vec![level(f64::NAN, 1.0, 0, 0)]), 73);
    bad(levels(vec![level(1.0, 2.0, 0, 0)]), 73);
    bad(levels(vec![level(1.0, 1.0, -1, 0)]), 73);
    bad(levels(vec![level(1.0, 1.0, 0, -1)]), 73);
    bad(
        levels(vec![level(2.0, 1.0, 0, 0), level(2.0, 1.5, 0, 0)]),
        73,
    );
    bad(
        levels(vec![level(2.0, 1.0, 0, 0), level(3.0, 0.5, 0, 0)]),
        73,
    );
    bad(levels(vec![vec![0; 31]]), 73);
    let n = |k: usize| levels((0..k).map(|i| level(i as f64, i as f64, 0, 0)).collect());
    assert!(decode_one(5, n(16)).is_ok());
    bad(n(17), 73);
    // Members: each value form's length, booleans 0 or 1, text 1 to 255 bytes, at most 64.
    let members = |ms: Vec<Vec<u8>>| {
        let mut f = remove(watch_set(), 74);
        for m in ms {
            f = push(f, 74, m);
        }
        f
    };
    bad(members(vec![vec![1, 0, 0, 0]]), 74);
    bad(members(vec![[vec![1], f64b(f64::NAN)].concat()]), 74);
    bad(members(vec![vec![4, 2]]), 74);
    bad(members(vec![vec![5]]), 74);
    bad(members(vec![[vec![5], vec![b'a'; 256]].concat()]), 74);
    bad(members(vec![vec![5, 0xff]]), 74);
    bad(members(vec![vec![0, 0]]), 74);
    bad(members(vec![vec![6, 0]]), 74);
    let m = |k: usize| members((0..k).map(|i| vec![3, i as u8, 0, 0, 0]).collect());
    assert!(decode_one(5, m(64)).is_ok());
    bad(m(65), 74);
    bad(replace(watch_set(), 75, [i64b(-1), i64b(0)].concat()), 75);
    bad(replace(watch_set(), 75, [i64b(0), i64b(-1)].concat()), 75);
    bad(replace(watch_set(), 75, i64b(0)), 75);
}

#[test]
fn field_seventy_and_ids_above_seventy_seven_are_unknown_critical_fields_of_a_routing_change() {
    for id in [70, 78] {
        assert_eq!(
            decode_one(5, replace(watch_change(), id, vec![])),
            Err(BodyError::Unsupported(Element::Field {
                entry_type: 5,
                field: id
            }))
        );
    }
}
