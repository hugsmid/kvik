//! Shared generators and the random operation machine for property tests.
#![allow(dead_code, missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use kvik_model::engine::{
    CompactionCrash, Config, ConsumerDecl, ConsumerId, CrashPoint, Fault, Model, Selection,
    WriteOutcome,
};
use kvik_model::summary::Params;
use kvik_model::types::{
    Fact, Kind, MINUTE, Nanos, Position, QUARTER, Quality, Reading, SECOND, SeriesId, Value,
    ValueType,
};
use proptest::prelude::*;

/// A fixed epoch for tests: 2026-03-29T00:00:00Z.
pub const T0: Nanos = 1_774_742_400 * SECOND;

/// Staleness limits, including sub-second and non-integral seconds.
pub fn arb_staleness() -> impl Strategy<Value = Nanos> {
    prop_oneof![
        Just(SECOND / 2),
        Just(SECOND),
        Just(2 * SECOND + SECOND / 2),
        Just(7 * SECOND),
        Just(MINUTE),
        Just(QUARTER + 1),
    ]
}

/// Event-time offsets from `T0` within about 40 minutes, biased towards node
/// boundaries (±1 ns), sub-second instants and exact seconds.
pub fn arb_offset() -> impl Strategy<Value = Nanos> {
    let boundary = (
        prop_oneof![Just(SECOND), Just(MINUTE), Just(QUARTER)],
        0i64..40,
        prop_oneof![Just(-1i64), Just(0), Just(1), Just(SECOND / 4)],
    )
        .prop_map(|(unit, k, d)| (k * unit + d).max(0));
    prop_oneof![
        3 => boundary,
        2 => (0i64..2400).prop_map(|s| s * SECOND),
        2 => (0i64..2400 * 4).prop_map(|q| q * SECOND / 4 + 7),
        1 => 0i64..(40 * MINUTE),
    ]
}

/// A small pool of event-time offsets so that the same `(series, t)` recurs:
/// corrections, identical repeats and A-then-B-then-A.
pub fn arb_pool_offset() -> impl Strategy<Value = Nanos> {
    proptest::sample::select(vec![
        0,
        1,
        SECOND / 2,
        SECOND - 1,
        SECOND,
        3 * SECOND,
        MINUTE - 1,
        MINUTE,
        QUARTER - 1,
        QUARTER,
        QUARTER + 7 * SECOND,
    ])
}

/// A value of the given kind. Small pools make A-then-B-then-A likely.
pub fn arb_value(kind: Kind) -> BoxedStrategy<Value> {
    match kind {
        Kind::Gauge => prop_oneof![
            (-4i32..8).prop_map(|v| Value::Num(f64::from(v) * 12.5)),
            (-1000.0f64..1000.0).prop_map(Value::Num),
        ]
        .boxed(),
        Kind::Counter => prop_oneof![
            5 => (0u32..50).prop_map(|v| Value::Num(f64::from(v) * 10.0)),
            1 => (0u32..5).prop_map(|v| Value::Num(f64::from(v))),
        ]
        .boxed(),
        Kind::State => prop_oneof![
            Just(Value::Str("idle".into())),
            Just(Value::Str("running".into())),
            Just(Value::Str("fault".into())),
        ]
        .boxed(),
        Kind::Text => {
            prop_oneof![Just(Value::Str("a".into())), Just(Value::Str("b".into()))].boxed()
        }
    }
}

pub fn arb_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Gauge),
        Just(Kind::Counter),
        Just(Kind::State),
        Just(Kind::Text)
    ]
}

pub fn arb_params() -> impl Strategy<Value = Params> {
    (
        arb_kind(),
        proptest::option::weighted(0.85, arb_staleness()),
        proptest::option::weighted(0.2, Just(400.0f64)),
        prop_oneof![Just(0.1f64), Just(0.5), Just(1.0)],
    )
        .prop_map(|(k, s, wrap, ceiling)| {
            let mut p = Params::new(k, s);
            p.counter.wrap_at = wrap;
            p.counter.reset_ceiling = ceiling;
            p
        })
}

/// Distinct, sorted readings for one series.
pub fn arb_readings(kind: Kind, max: usize) -> impl Strategy<Value = Vec<(Nanos, Value)>> {
    proptest::collection::btree_map(arb_offset(), arb_value(kind), 0..max)
        .prop_map(|m| m.into_iter().map(|(t, v)| (T0 + t, v)).collect())
}

pub fn value_type(kind: Kind) -> ValueType {
    match kind {
        Kind::Gauge | Kind::Counter => ValueType::Number,
        Kind::State => ValueType::Name,
        Kind::Text => ValueType::Text,
    }
}

// ----- random operation machine -----

pub const KINDS: [Kind; 3] = [Kind::Gauge, Kind::Counter, Kind::State];

#[derive(Clone, Debug)]
pub enum Op {
    Emit {
        series: usize,
        t: Nanos,
        value: Value,
        bad: bool,
    },
    Commit(Option<Fault>),
    Crash,
    /// The machine loses power (a new boot); with `Some(fails)`, it restarts
    /// at once, and the recovery sync fails if `fails`.
    PowerLoss(Option<bool>),
    /// Restart; with `true`, the sync recovery performs after a reboot fails.
    Restart(bool),
    Advance(Nanos),
    JumpWall(Nanos),
    Poll {
        c: usize,
        n: usize,
    },
    Confirm {
        c: usize,
    },
    CatchUp {
        c: usize,
        n: usize,
    },
    Reconnect {
        c: usize,
    },
    Seal,
    Compact(Option<CompactionCrash>),
    Upload,
    Bucket(bool),
    CopyCold,
    Housekeeping,
}

pub fn arb_fault() -> impl Strategy<Value = Option<Fault>> {
    let outcome = prop_oneof![
        Just(WriteOutcome::Lost),
        Just(WriteOutcome::Torn),
        Just(WriteOutcome::Survived)
    ];
    prop_oneof![
        12 => Just(None),
        1 => Just(Some(Fault::Crash(CrashPoint::BeforeWrite))),
        1 => outcome.clone().prop_map(|o| Some(Fault::Crash(CrashPoint::AfterWrite(o)))),
        1 => Just(Some(Fault::Crash(CrashPoint::AfterSyncBeforeMark))),
        1 => Just(Some(Fault::Crash(CrashPoint::AfterSync))),
        1 => (0usize..3).prop_map(|n| Some(Fault::Crash(CrashPoint::DuringDelivery(n)))),
        1 => outcome.clone().prop_map(|o| Some(Fault::PowerLoss(o))),
        1 => outcome.prop_map(|o| Some(Fault::SyncError(o))),
    ]
}

pub fn arb_op() -> impl Strategy<Value = Op> {
    let emit = (0usize..3)
        .prop_flat_map(|s| {
            (
                Just(s),
                prop_oneof![3 => arb_pool_offset(), 1 => arb_offset()],
                arb_value(KINDS[s]),
                proptest::bool::weighted(0.1),
            )
        })
        .prop_map(|(series, t, value, bad)| Op::Emit {
            series,
            t: T0 + t,
            value,
            bad,
        });
    prop_oneof![
        20 => emit,
        8 => arb_fault().prop_map(Op::Commit),
        1 => Just(Op::Crash),
        2 => proptest::option::of(proptest::bool::weighted(0.4)).prop_map(Op::PowerLoss),
        1 => proptest::bool::weighted(0.3).prop_map(Op::Restart),
        3 => prop_oneof![Just(SECOND / 3), Just(SECOND), Just(MINUTE), Just(2 * 3600 * SECOND)]
            .prop_map(Op::Advance),
        1 => prop_oneof![Just(-3600 * SECOND), Just(3600 * SECOND)].prop_map(Op::JumpWall),
        4 => (0usize..2, 1usize..4).prop_map(|(c, n)| Op::Poll { c, n }),
        2 => (0usize..2).prop_map(|c| Op::Confirm { c }),
        3 => (0usize..2, 1usize..5).prop_map(|(c, n)| Op::CatchUp { c, n }),
        1 => (0usize..2).prop_map(|c| Op::Reconnect { c }),
        1 => Just(Op::Seal),
        2 => prop_oneof![
            4 => Just(None),
            1 => Just(Some(CompactionCrash::AfterWrite)),
            1 => Just(Some(CompactionCrash::AfterPublish))
        ]
        .prop_map(Op::Compact),
        2 => Just(Op::Upload),
        1 => any::<bool>().prop_map(Op::Bucket),
        1 => Just(Op::CopyCold),
        2 => Just(Op::Housekeeping),
    ]
}

pub fn consumers() -> Vec<ConsumerDecl> {
    vec![
        ConsumerDecl {
            id: ConsumerId(0),
            selection: Selection::All,
            capacity: 3,
        },
        ConsumerDecl {
            id: ConsumerId(1),
            selection: Selection::Series([SeriesId(0)].into_iter().collect()),
            capacity: 2,
        },
    ]
}

/// Series names from several fields, one per kind in [`KINDS`].
pub const NAMES: [&str; 3] = [
    "plant/line-1/temperature",
    "site/pump-2/volume",
    "vehicle/7/gear",
];

/// A model with three series (gauge, counter, state), wall clock at T0 + 1 h.
pub fn machine() -> (Model, Vec<SeriesId>) {
    let cfg = Config {
        seal_after_batches: 3,
        ..Config::default()
    };
    let mut m = Model::new(cfg, consumers(), T0 + 3600 * SECOND);
    let ids = KINDS
        .iter()
        .enumerate()
        .map(|(i, k)| {
            m.define(NAMES[i], *k, value_type(*k), "1", 5 * SECOND)
                .unwrap()
        })
        .collect();
    (m, ids)
}

/// Durable facts (any tier), by position.
pub fn durable(m: &Model) -> BTreeMap<Position, Fact> {
    m.disk
        .all_facts()
        .into_iter()
        .map(|f| (f.position, f))
        .collect()
}

/// Applies one operation, restarting first if the engine is down and the
/// operation needs it. Returns a description of any invariant violation found
/// on the way (invariant 2's "delivered ⇒ durable" and invariant 10).
pub fn apply(m: &mut Model, ids: &[SeriesId], op: &Op) -> Result<(), String> {
    let before = durable(m);
    let needs_up = !matches!(
        op,
        Op::Crash
            | Op::PowerLoss(_)
            | Op::Restart(_)
            | Op::Advance(_)
            | Op::JumpWall(_)
            | Op::Bucket(_)
    );
    if needs_up && !m.is_up() {
        m.restart();
    }
    let cid = |c: usize| ConsumerId(c as u32);
    match op {
        Op::Emit {
            series,
            t,
            value,
            bad,
        } => {
            let _ = m.emit(Reading {
                series: ids[*series],
                t: *t,
                value: value.clone(),
                quality: if *bad { Quality::Bad } else { Quality::Good },
            });
        }
        Op::Commit(f) => {
            let _ = m.commit(*f);
        }
        Op::Crash => m.crash(),
        Op::PowerLoss(restart) => {
            m.power_loss();
            if let Some(fails) = restart {
                m.restart_with(*fails);
            }
        }
        Op::Restart(fails) => m.restart_with(*fails),
        Op::Advance(d) => m.advance(*d),
        Op::JumpWall(d) => m.jump_wall(*d),
        Op::Poll { c, n } => {
            let got = m.poll(cid(*c), *n).map_err(|e| e.to_string())?;
            let dur = durable(m);
            for d in got {
                if dur.get(&d.position) != Some(&d.fact) {
                    return Err(format!("delivered position {} is not durable", d.position));
                }
            }
        }
        Op::Confirm { c } => {
            if let Some(last) = m.observed.get(&cid(*c)).and_then(|o| o.last()) {
                let _ = m.confirm(cid(*c), last.position);
            }
        }
        Op::CatchUp { c, n } => {
            let _ = m.catch_up(cid(*c), *n);
        }
        Op::Reconnect { c } => {
            let _ = m.reconnect(cid(*c));
        }
        Op::Seal => m.seal_active(),
        Op::Compact(c) => {
            let _ = m.compact(*c);
        }
        Op::Upload => {
            m.upload();
        }
        Op::Bucket(r) => m.disk.bucket.reachable = *r,
        Op::CopyCold => {
            let files: Vec<_> = m.disk.hot.iter().copied().collect();
            for f in files {
                if m.disk.files[&f].sealed {
                    m.copy_to_cold(f);
                }
            }
        }
        Op::Housekeeping => {
            let _ = m.housekeeping();
        }
    }
    let after = durable(m);
    for (p, f) in &before {
        if after.get(p) != Some(f) {
            return Err(format!(
                "operation {op:?} removed or changed fact at position {p}"
            ));
        }
    }
    Ok(())
}

/// Positions observed per consumer.
pub fn observed_positions(m: &Model, c: u32) -> BTreeSet<Position> {
    m.observed
        .get(&ConsumerId(c))
        .map(|o| o.iter().map(|x| x.position).collect())
        .unwrap_or_default()
}

/// Restarts the engine if it is down, read-only, or has batches past the
/// durable mark, so that what is on disk is settled.
pub fn settle(m: &mut Model) {
    if !m.is_up() || m.is_read_only() || m.disk.tail_len() > 0 {
        m.restart();
    }
}
