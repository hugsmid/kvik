//! Shared generators and the random operation machine for property tests.
#![allow(dead_code, missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use kvik_model::condition::{Condition, ConditionState};
use kvik_model::engine::{
    CompactionCrash, Config, ConsumerDecl, ConsumerId, CrashPoint, DeliveryKind, EntryBody,
    Envelope, Fault, Funnel, FunnelId, Model, Operator, RouteChange, Selection, WriteOutcome,
};
use kvik_model::oracle;
use kvik_model::registry::{ChangedAtKind, SeriesOptions};
use kvik_model::summary::Aggregate;
use kvik_model::summary::Params;
use kvik_model::types::{
    DAY, Fact, HOUR, Kind, MINUTE, Nanos, Position, QUARTER, Quality, Reading, SECOND, SeriesId,
    Value, ValueType,
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

/// Index in [`machine`]'s series of a versioned gauge (§2.1), whose readings
/// carry `changed_at` from a small pool.
pub const VERSIONED: usize = 3;

#[derive(Clone, Debug)]
pub enum Op {
    Emit {
        series: usize,
        t: Nanos,
        value: Value,
        bad: bool,
        /// Submitted with `revise` intent (§2.4).
        revise: bool,
        /// `changed_at`, used for the versioned series only.
        changed_at: u64,
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
    let emit = (0usize..=VERSIONED)
        .prop_flat_map(|s| {
            (
                Just(s),
                // The versioned series revisits a few keys, so that facts are
                // outranked.
                if s == VERSIONED {
                    proptest::sample::select(vec![0, SECOND, MINUTE]).boxed()
                } else {
                    prop_oneof![3 => arb_pool_offset(), 1 => arb_offset()].boxed()
                },
                arb_value(KINDS[s % KINDS.len()]),
                proptest::bool::weighted(0.1),
                proptest::bool::weighted(0.05),
                0u64..4,
            )
        })
        .prop_map(|(series, t, value, bad, revise, changed_at)| Op::Emit {
            series,
            t: T0 + t,
            value,
            bad,
            revise,
            changed_at,
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

/// Series names from several fields, one per kind in [`KINDS`], then the
/// versioned series.
pub const NAMES: [&str; 4] = [
    "plant/line-1/temperature",
    "site/pump-2/volume",
    "vehicle/7/gear",
    "site/forecast-1/load",
];

/// A model with three series (gauge, counter, state), wall clock at T0 + 1 h.
pub fn machine() -> (Model, Vec<SeriesId>) {
    let cfg = Config {
        seal_after_batches: 3,
        ..Config::default()
    };
    let mut m = Model::new(cfg, consumers(), T0 + 3600 * SECOND);
    let mut ids: Vec<SeriesId> = KINDS
        .iter()
        .enumerate()
        .map(|(i, k)| {
            m.define(NAMES[i], *k, value_type(*k), "1", 5 * SECOND)
                .unwrap()
        })
        .collect();
    ids.push(
        m.define_with(
            NAMES[VERSIONED],
            Kind::Gauge,
            ValueType::Number,
            "1",
            5 * SECOND,
            SeriesOptions::versioned(ChangedAtKind::Version),
        )
        .unwrap(),
    );
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
            revise,
            changed_at,
        } => {
            let r = Reading {
                series: ids[*series],
                t: *t,
                value: value.clone(),
                quality: if *bad { Quality::Bad } else { Quality::Good },
                changed_at: (*series == VERSIONED).then_some(*changed_at),
            };
            let _ = if *revise { m.emit_revise(r) } else { m.emit(r) };
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

/// The ranges `(after, up_to]` that a `resync` delivered to consumer `c`
/// replaced, because its cursor was too far behind for catch-up (§4 Resync).
pub fn resync_replaced(m: &Model, c: u32) -> Vec<(Position, Position)> {
    m.received
        .get(&ConsumerId(c))
        .into_iter()
        .flatten()
        .filter_map(|r| r.delivery.resync().and_then(|x| x.replaced))
        .collect()
}

/// Positions held by log segments on a local tier (hot, or cold), now.
pub fn local_segment_positions(m: &Model) -> BTreeSet<Position> {
    use kvik_model::engine::{FileClass, Locality};
    m.disk
        .published()
        .into_iter()
        .filter(|f| {
            m.disk.files[f].class == FileClass::Segment
                && (m.disk.locality(*f) == Locality::Hot || m.disk.cold.contains(f))
        })
        .flat_map(|f| {
            m.disk.files[&f]
                .entries()
                .map(|e| e.position)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Positions held in local log segments when each `resync` was issued, by
/// consumer and issue number (`Resync::issued`).
pub type ResyncBasis = BTreeMap<(ConsumerId, usize), BTreeSet<Position>>;

fn resyncs_issued(m: &Model, c: ConsumerId) -> usize {
    m.signals.get(&c).map_or(0, |v| {
        v.iter()
            .filter(|x| matches!(x, kvik_model::engine::Signal::ResyncDue))
            .count()
    })
}

/// Runs `f` on `m`, recording for every `resync` issued meanwhile the
/// positions held in local log segments at the time. No operation that issues
/// a `resync` moves a segment between tiers, so the state before it is the
/// state at issue.
pub fn track_resyncs<R>(
    m: &mut Model,
    basis: &mut ResyncBasis,
    f: impl FnOnce(&mut Model) -> R,
) -> R {
    let ids: Vec<ConsumerId> = m.consumers.iter().map(|c| c.id).collect();
    let before: Vec<usize> = ids.iter().map(|c| resyncs_issued(m, *c)).collect();
    let local = local_segment_positions(m);
    let out = f(m);
    for (c, b) in ids.iter().zip(before) {
        for k in b..resyncs_issued(m, *c) {
            basis.insert((*c, k), local.clone());
        }
    }
    out
}

/// The ranges `(after, up_to]` a `resync` replaced for consumer `c` where the
/// specification sends one: the cursor was older than the oldest local log
/// segment when the `resync` was issued, so some entry in the range was not in
/// a local log segment then.
pub fn resync_replaced_justified(
    m: &Model,
    basis: &ResyncBasis,
    c: u32,
) -> Vec<(Position, Position)> {
    let entries: Vec<Position> = m.disk.all_entries().into_keys().collect();
    m.received
        .get(&ConsumerId(c))
        .into_iter()
        .flatten()
        .filter_map(|r| r.delivery.resync())
        .filter_map(|x| {
            let (a, b) = x.replaced?;
            let local = basis.get(&(ConsumerId(c), x.issued))?;
            entries
                .iter()
                .any(|p| a < *p && *p <= b && !local.contains(p))
                .then_some((a, b))
        })
        .collect()
}

/// What consumer `c` received in `session`, in order.
pub fn session_stream(m: &Model, c: ConsumerId, session: u32) -> Vec<Envelope> {
    m.received
        .get(&c)
        .into_iter()
        .flatten()
        .filter(|r| r.session == session)
        .map(|r| r.delivery.clone())
        .collect()
}

/// A short label per delivery: `F<seq>` for a fact (`R<seq>` if replayed),
/// `T<funnel>:<sequence>` for a condition delivery, `G` for
/// `generation_changed`, `S` for `resync`. Positions must be in generation 0.
pub fn labels(stream: &[Envelope]) -> Vec<String> {
    stream
        .iter()
        .map(|d| match &d.kind {
            DeliveryKind::Fact(_) => format!(
                "{}{}",
                if d.replayed { "R" } else { "F" },
                seqs([d.position])[0]
            ),
            DeliveryKind::Trigger(t) => format!("T{}:{}", t.funnel.0, t.sequence),
            DeliveryKind::GenerationChanged { .. } => "G".into(),
            DeliveryKind::Resync(_) => "S".into(),
        })
        .collect()
}

/// Restarts the engine if it is down, read-only, or has batches past the
/// durable mark, so that what is on disk is settled.
pub fn settle(m: &mut Model) {
    if !m.is_up() || m.is_read_only() || m.disk.tail_len() > 0 {
        m.restart();
    }
}

/// A position in generation 0.
pub fn p0(sequence: u64) -> Position {
    Position::new(0, sequence)
}

/// Sequences of positions that must all be in generation 0: a history without
/// a restore. Panics otherwise, so a generation bug cannot hide behind it.
pub fn seqs(positions: impl IntoIterator<Item = Position>) -> Vec<u64> {
    positions
        .into_iter()
        .map(|p| {
            assert_eq!(p.generation, 0, "position {p} outside generation 0");
            p.sequence
        })
        .collect()
}

/// A store with a tree checkpoint at `0:6`: facts at `T0` + 0, 20, 40 and
/// 130 s and a revision of the one at 20 s, compacted into a current and a
/// revision chunk; then facts at 150 s (the minute of the one at 130 s) and
/// 200 s after the checkpoint (`0:7`, `0:8`). Everything is sealed and backed
/// up, and the checkpoint is uploaded.
pub fn checkpointed_model() -> (Model, SeriesId, Position) {
    let c = ConsumerDecl {
        id: ConsumerId(1),
        selection: Selection::All,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![c], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            2 * MINUTE,
        )
        .unwrap();
    for (t, v) in [(0, 0.0), (20, 1.0), (40, 2.0), (130, 3.0), (20, 100.0)] {
        m.ingest(vec![Reading::num(s, T0 + t * SECOND, v)]).unwrap();
    }
    m.seal_active();
    m.compact(None).unwrap();
    let cp = m.checkpoint().unwrap();
    assert_eq!(cp, Position::new(0, 6));
    for (t, v) in [(150, 4.0), (200, 5.0)] {
        m.ingest(vec![Reading::num(s, T0 + t * SECOND, v)]).unwrap();
    }
    m.seal_active();
    assert_eq!(m.upload_checkpoints(), 1);
    (m, s, cp)
}

/// A restore of `m`'s bucket that returns as soon as it accepts ingest.
pub fn restore_in_progress(m: &mut Model, cursors: &BTreeMap<ConsumerId, Position>) -> Model {
    Model::restore_with_cursors(
        m.config.clone(),
        m.consumers.clone(),
        &mut m.disk.bucket,
        cursors,
        m.wall,
        true,
        false,
    )
    .unwrap()
}

// ----- delivery machine: routes, conditions and timers -----

/// A funnel a route operation adds, over series index `0` or `1`.
#[derive(Clone, Debug)]
pub enum RouteKind {
    /// Every fact of the series, or of every series.
    Facts(Option<usize>),
    /// Threshold 50 / clear 40 on the live-state value, or on a 30 s average.
    Threshold(usize, bool),
    /// Held above 50 for 10 s.
    HeldFor(usize),
    /// On change with a dead-band of 5.
    OnChange(usize),
    /// Stale for S + 5 s.
    StaleFor(usize),
}

#[derive(Clone, Debug)]
pub enum DOp {
    /// One reading at the current wall time, committed (with an optional fault).
    Put {
        series: usize,
        value: f64,
        fault: Option<Fault>,
    },
    Advance(Nanos),
    JumpWall(Nanos),
    Tick,
    Crash,
    PowerLoss,
    Restart,
    /// Catch up by up to `2n` entries, then take up to `n` deliveries.
    Receive {
        c: usize,
        n: usize,
    },
    /// Confirm the last delivery taken in the current session.
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
    AddRoute {
        c: usize,
        funnel: u32,
        kind: RouteKind,
    },
    RemoveRoute {
        c: usize,
        funnel: u32,
    },
    Seal,
    Compact,
    Upload,
    Housekeeping,
}

pub const D_STALENESS: Nanos = 5 * SECOND;

pub fn arb_route_kind() -> impl Strategy<Value = RouteKind> {
    prop_oneof![
        2 => proptest::option::of(0usize..2).prop_map(RouteKind::Facts),
        2 => (0usize..2, any::<bool>()).prop_map(|(s, w)| RouteKind::Threshold(s, w)),
        1 => (0usize..2).prop_map(RouteKind::HeldFor),
        1 => (0usize..2).prop_map(RouteKind::OnChange),
        1 => (0usize..2).prop_map(RouteKind::StaleFor),
    ]
}

pub fn arb_delivery_op() -> impl Strategy<Value = DOp> {
    let put = (
        0usize..2,
        proptest::sample::select(vec![0.0, 10.0, 45.0, 60.0, 90.0, 90.0]),
        prop_oneof![
            40 => Just(None),
            1 => Just(Some(Fault::Crash(CrashPoint::AfterSync))),
            1 => (0usize..3).prop_map(|n| Some(Fault::Crash(CrashPoint::DuringDelivery(n)))),
            1 => Just(Some(Fault::PowerLoss(WriteOutcome::Survived))),
            1 => Just(Some(Fault::PowerLoss(WriteOutcome::Torn))),
        ],
    )
        .prop_map(|(series, value, fault)| DOp::Put {
            series,
            value,
            fault,
        });
    prop_oneof![
        16 => put,
        8 => prop_oneof![
            8 => Just(SECOND),
            6 => Just(5 * SECOND),
            2 => Just(30 * SECOND),
            1 => Just(2 * HOUR),
            1 => Just(91 * DAY),
        ]
        .prop_map(DOp::Advance),
        1 => prop_oneof![Just(-HOUR), Just(HOUR)].prop_map(DOp::JumpWall),
        8 => Just(DOp::Tick),
        1 => Just(DOp::Crash),
        1 => Just(DOp::PowerLoss),
        1 => Just(DOp::Restart),
        12 => (0usize..2, 1usize..8).prop_map(|(c, n)| DOp::Receive { c, n }),
        8 => (0usize..2).prop_map(|c| DOp::Confirm { c }),
        5 => (0usize..2, 1usize..20).prop_map(|(c, n)| DOp::CatchUp { c, n }),
        1 => (0usize..2).prop_map(|c| DOp::Reconnect { c }),
        2 => (0usize..2, 1u32..4, arb_route_kind()).prop_map(|(c, funnel, kind)| DOp::AddRoute {
            c,
            funnel,
            kind
        }),
        1 => (0usize..2, 0u32..4).prop_map(|(c, funnel)| DOp::RemoveRoute { c, funnel }),
        1 => Just(DOp::Seal),
        1 => Just(DOp::Compact),
        1 => Just(DOp::Upload),
        1 => Just(DOp::Housekeeping),
    ]
}

pub fn funnel_of(kind: &RouteKind, ids: &[SeriesId]) -> Funnel {
    let avg = Operator {
        aggregate: Aggregate::Avg,
        width: 30 * SECOND,
    };
    match *kind {
        RouteKind::Facts(None) => Funnel::facts(Selection::All),
        RouteKind::Facts(Some(s)) => Funnel::facts(Selection::Series([ids[s]].into())),
        RouteKind::Threshold(s, windowed) => Funnel::condition(
            ids[s],
            windowed.then_some(avg),
            Condition::Threshold {
                on: 50.0,
                clear: 40.0,
            },
        ),
        RouteKind::HeldFor(s) => Funnel::condition(
            ids[s],
            None,
            Condition::HeldFor {
                above: 50.0,
                n: 10 * SECOND,
            },
        ),
        RouteKind::OnChange(s) => {
            Funnel::condition(ids[s], None, Condition::OnChange { deadband: 5.0 })
        }
        RouteKind::StaleFor(s) => Funnel::condition(
            ids[s],
            None,
            Condition::StaleFor {
                staleness: D_STALENESS,
                n: 5 * SECOND,
            },
        ),
    }
}

/// The consumers of the delivery machine: as [`consumers`], with queues of 8
/// and 4.
pub fn delivery_consumers() -> Vec<ConsumerDecl> {
    let mut cs = consumers();
    cs[0].capacity = 8;
    cs[1].capacity = 4;
    cs
}

/// A model with two gauges (S = 5 s) and the consumers of
/// [`delivery_consumers`], each with a condition funnel `1` over the first
/// gauge: consumer 0 a threshold on a 30 s average, consumer 1 a held-for
/// condition.
pub fn delivery_machine() -> (Model, Vec<SeriesId>) {
    let cfg = Config {
        seal_after_batches: 3,
        ..Config::default()
    };
    let mut m = Model::new(cfg, delivery_consumers(), T0 + HOUR);
    let ids: Vec<SeriesId> = ["plant/line-1/temperature", "plant/line-2/temperature"]
        .iter()
        .map(|n| {
            m.define(n, Kind::Gauge, ValueType::Number, "degC", D_STALENESS)
                .unwrap()
        })
        .collect();
    for (c, kind) in [
        (0, RouteKind::Threshold(0, true)),
        (1, RouteKind::HeldFor(0)),
    ] {
        m.add_route(ConsumerId(c), FunnelId(1), funnel_of(&kind, &ids))
            .unwrap();
    }
    (m, ids)
}

/// Applies one delivery-machine operation, restarting first if the engine is
/// down and the operation needs it. Checks that every delivery taken is of a
/// durable fact (invariant 2's first half).
pub fn apply_delivery(m: &mut Model, ids: &[SeriesId], op: &DOp) -> Result<(), String> {
    let needs_up = !matches!(
        op,
        DOp::Crash | DOp::PowerLoss | DOp::Restart | DOp::Advance(_) | DOp::JumpWall(_)
    );
    if needs_up && !m.is_up() {
        m.restart();
    }
    let cid = |c: usize| ConsumerId(c as u32);
    match op {
        DOp::Put {
            series,
            value,
            fault,
        } => {
            let now = m.wall;
            let _ = m.emit(Reading::num(ids[*series], now, *value));
            let _ = m.commit(*fault);
        }
        DOp::Advance(d) => m.advance(*d),
        DOp::JumpWall(d) => m.jump_wall(*d),
        DOp::Tick => m.tick().map_err(|e| e.to_string())?,
        DOp::Crash => m.crash(),
        DOp::PowerLoss => m.power_loss(),
        DOp::Restart => m.restart(),
        DOp::Receive { c, n } => {
            // Engine-side catch-up runs whenever the queue has room.
            let _ = m.catch_up(cid(*c), 2 * *n);
            let got = m.receive(cid(*c), *n).map_err(|e| e.to_string())?;
            let dur = durable(m);
            for d in got {
                if let Some(f) = d.fact()
                    && dur.get(&d.position) != Some(f)
                {
                    return Err(format!("delivered position {} is not durable", d.position));
                }
            }
        }
        DOp::Confirm { c } => {
            // Fails (and is ignored) if the last delivery was taken in an
            // earlier session.
            if let Some(last) = m.received.get(&cid(*c)).and_then(|r| r.last()) {
                let _ = m.confirm_delivery(cid(*c), last.delivery.id());
            }
        }
        DOp::CatchUp { c, n } => {
            let _ = m.catch_up(cid(*c), *n);
        }
        DOp::Reconnect { c } => {
            let _ = m.reconnect(cid(*c));
        }
        DOp::AddRoute { c, funnel, kind } => {
            let _ = m.add_route(cid(*c), FunnelId(*funnel), funnel_of(kind, ids));
        }
        DOp::RemoveRoute { c, funnel } => {
            let _ = m.remove_route(cid(*c), FunnelId(*funnel));
        }
        DOp::Seal => m.seal_active(),
        DOp::Compact => {
            let _ = m.compact(None);
        }
        DOp::Upload => {
            m.upload();
        }
        DOp::Housekeeping => {
            let _ = m.housekeeping();
        }
    }
    Ok(())
}

/// Whether a fact of `series` at position `p` is routed to the consumer by a
/// funnel without a condition in force at `p`, from the configured selection
/// and the routing entries in the log (§4 Routing).
pub fn routed_at(m: &Model, decl: &ConsumerDecl, p: Position, series: SeriesId) -> bool {
    let mut funnels: BTreeMap<FunnelId, Funnel> =
        [(FunnelId::CONFIGURED, Funnel::facts(decl.selection.clone()))].into();
    for e in m.disk.all_entries().into_values() {
        if e.position >= p {
            break;
        }
        match e.body {
            EntryBody::Route(RouteChange::Add {
                consumer,
                funnel,
                spec,
            }) if consumer == decl.id => {
                funnels.insert(funnel, spec);
            }
            EntryBody::Route(RouteChange::Remove { consumer, funnel }) if consumer == decl.id => {
                funnels.remove(&funnel);
            }
            _ => {}
        }
    }
    funnels.values().any(|f| {
        f.condition.is_none()
            && match &f.selection {
                Selection::All => true,
                Selection::Series(set) => set.contains(&series),
            }
    })
}

/// The value a condition funnel saw at instant `x`, computed directly from
/// the current view: the live-state value at `x`, or the operator's window
/// ending at `x` by the window oracle.
pub fn oracle_funnel_value_at(m: &Model, f: &Funnel, x: Nanos) -> Option<f64> {
    let Selection::Series(set) = &f.selection else {
        return None;
    };
    let s = *set.iter().next()?;
    let current = m.current().ok()?;
    match &f.operator {
        Some(op) => {
            let p = kvik_model::summary::Params::new(Kind::Gauge, D_STALENESS);
            let rs = oracle::readings(current, s);
            oracle::window(&p, &rs, x - op.width, x, x)
                .aggregate(&op.aggregate)?
                .to_f64()
        }
        None => current
            .range((s, Nanos::MIN)..=(s, x))
            .rev()
            .map(|(_, f)| f)
            .find(|f| f.summarised())
            .and_then(|f| match f.value {
                Value::Num(v) => Some(v),
                _ => None,
            }),
    }
}

/// The value a condition funnel sees now.
pub fn oracle_funnel_value(m: &Model, f: &Funnel) -> Option<f64> {
    oracle_funnel_value_at(m, f, m.now)
}

/// The condition state now, as invariant 16 needs it, computed directly: a
/// threshold by its levels, between them by the latch (Q-63); on change, the
/// value now; held for, true when the value is above throughout the last N;
/// stale for, true when no reading of the series has arrived for S + N on the
/// host clock (Q-16: measured by `received_at`).
pub fn condition_now(m: &Model, f: &Funnel, latch: Option<&ConditionState>) -> ConditionState {
    let cond = f.condition.as_ref().expect("a condition funnel");
    let value = oracle_funnel_value(m, f);
    let latched = matches!(latch, Some(ConditionState::Active(true)));
    match cond {
        Condition::Threshold { on, clear } => ConditionState::Active(match value {
            Some(v) if v > *on => true,
            Some(v) if v < *clear => false,
            _ => latched,
        }),
        Condition::OnChange { .. } => ConditionState::Value(value),
        Condition::HeldFor { above, n } => {
            // Above throughout [now − N, now]: the value held at now − N and
            // every reading after it, up to now.
            let Selection::Series(set) = &f.selection else {
                unreachable!()
            };
            let s = *set.iter().next().unwrap();
            let after: Vec<f64> = m
                .current()
                .unwrap()
                .range((s, m.now - n + 1)..=(s, m.now))
                .map(|(_, x)| x)
                .filter(|x| x.summarised())
                .map(|x| match x.value {
                    Value::Num(v) => v,
                    _ => f64::NAN,
                })
                .collect();
            ConditionState::Active(
                oracle_funnel_value_at(m, f, m.now - n).is_some_and(|v| v > *above)
                    && after.iter().all(|v| *v > *above),
            )
        }
        Condition::StaleFor { staleness, n } => {
            let Selection::Series(set) = &f.selection else {
                unreachable!()
            };
            let s = *set.iter().next().unwrap();
            // The last reading committed, by position.
            let last = m
                .disk
                .all_facts()
                .into_iter()
                .filter(|x| x.series == s)
                .max_by_key(|x| x.position)
                .map(|x| x.received_at);
            // On the host clock that stamped `received_at` (Q-16).
            ConditionState::Active(last.is_some_and(|r| m.wall - r >= staleness + n))
        }
    }
}
