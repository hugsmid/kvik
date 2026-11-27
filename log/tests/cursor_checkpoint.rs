//! Gate checkpoints and gate cursors in cursor files (`format.md` A.15, A.11): every representable checkpoint
//! round-trips, and no input makes the decoders panic.

use kvik_log::cursor::{
    Checkpoint, CheckpointFact, CheckpointInstance, CheckpointSeries, CheckpointState,
    CheckpointValue, CursorState, GateCursor, GateId, Record, append_record, decode,
    decode_checkpoint, encode_checkpoint, read,
};
use kvik_log::fs::MemFs;
use kvik_log::writer::Position;
use proptest::prelude::*;
use std::path::Path;

fn pos() -> impl Strategy<Value = Position> {
    (0u32..3, any::<u64>()).prop_map(|(generation, sequence)| Position {
        generation,
        sequence,
    })
}

fn value() -> impl Strategy<Value = CheckpointValue> {
    prop_oneof![
        Just(CheckpointValue::Absent),
        (-1e9f64..1e9).prop_map(CheckpointValue::Num),
        any::<bool>().prop_map(CheckpointValue::Bool),
        "[a-z ]{0,255}".prop_map(CheckpointValue::Text),
    ]
}

fn id() -> impl Strategy<Value = GateId> {
    (pos(), any::<Option<i64>>(), any::<u32>(), any::<u32>()).prop_map(|(at, due, gate, series)| {
        GateId {
            at,
            due,
            gate,
            series,
        }
    })
}

fn instance() -> impl Strategy<Value = CheckpointInstance> {
    let state = prop_oneof![
        Just(CheckpointState::Unknown),
        Just(CheckpointState::Stale),
        any::<u8>().prop_map(CheckpointState::Level),
        any::<bool>().prop_map(CheckpointState::Match),
        value().prop_map(CheckpointState::Value),
    ];
    (
        (any::<u32>(), any::<u32>(), state, id(), any::<bool>()),
        (
            any::<u16>(),
            prop::collection::vec((any::<u8>(), any::<i64>()), 0..=16),
            prop::option::of(value()),
            any::<i64>(),
        ),
    )
        .prop_map(
            |(
                (gate, series, state, set_by, reseeded),
                (crossed, pending, reference, last_eval),
            )| {
                CheckpointInstance {
                    gate,
                    series,
                    state,
                    set_by,
                    reseeded,
                    crossed,
                    pending,
                    reference,
                    last_eval,
                }
            },
        )
}

fn series() -> impl Strategy<Value = CheckpointSeries> {
    let fact = (
        any::<i64>(),
        pos(),
        value(),
        any::<bool>(),
        any::<Option<u64>>(),
    )
        .prop_map(|(t, position, value, usable, changed_at)| CheckpointFact {
            t,
            position,
            value,
            usable,
            changed_at,
        });
    (any::<u32>(), prop::option::of(fact), any::<Option<i64>>()).prop_map(
        |(series, live, last_arrival)| CheckpointSeries {
            series,
            live,
            last_arrival,
        },
    )
}

fn checkpoint() -> impl Strategy<Value = Checkpoint> {
    (
        pos(),
        any::<i64>(),
        prop::collection::vec(instance(), 0..6),
        prop::collection::vec(series(), 0..6),
    )
        .prop_map(|(through, now, mut instances, mut series)| {
            instances.sort_by_key(|i| (i.gate, i.series));
            instances.dedup_by_key(|i| (i.gate, i.series));
            series.sort_by_key(|s| s.series);
            series.dedup_by_key(|s| s.series);
            Checkpoint {
                through,
                now,
                instances,
                series,
            }
        })
}

fn cursor() -> impl Strategy<Value = GateCursor> {
    prop_oneof![
        Just(GateCursor::Nothing),
        id().prop_map(GateCursor::Transition),
        (pos(), any::<i64>()).prop_map(|(at, now)| GateCursor::Frontier { at, now }),
    ]
}

proptest! {
    #[test]
    fn every_checkpoint_round_trips(c in checkpoint()) {
        let body = encode_checkpoint(&c).unwrap();
        prop_assert_eq!(decode_checkpoint(&body), Some(c));
    }

    #[test]
    fn a_cursor_file_with_a_checkpoint_round_trips(c in checkpoint(), g in cursor()) {
        let fs = MemFs::new();
        let dir = Path::new("/s");
        let state = CursorState {
            consumer: "c".into(),
            position: c.through,
            latches: Vec::new(),
            reset: None,
            gate: Some(g),
        };
        let rec = Record { state: &state, checkpoint: Some(&c), fresh: true };
        append_record(&fs, dir, [1; 16], 0, 0, rec, None).unwrap();
        let f = read(&fs, dir, &[1; 16], "c").unwrap().unwrap();
        prop_assert_eq!((f.state, f.checkpoint), (state, Some(c)));
    }

    #[test]
    fn no_bytes_make_the_decoders_panic(b in prop::collection::vec(any::<u8>(), 0..400), cut in any::<prop::sample::Index>()) {
        let _ = decode_checkpoint(&b);
        let _ = decode(&b, &[1; 16]);
        let body = encode_checkpoint(&Checkpoint {
            through: Position::default(),
            now: 0,
            instances: Vec::new(),
            series: Vec::new(),
        })
        .unwrap();
        let mut spliced = body.clone();
        spliced.extend_from_slice(&b);
        let _ = decode_checkpoint(&spliced[..cut.index(spliced.len() + 1)]);
    }
}
