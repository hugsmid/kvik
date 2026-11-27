//! The core against `kvik_model::gate::judge` over generated logs (spec §6 Gates):
//! random batch splits, floors between batches, watches and unwatches mid-log (seeded starts), other
//! series' facts interleaved, and snapshot and restore at random batch boundaries.

use std::collections::BTreeMap;

use kvik_model::gate::{ConditionId, Entry, Gate, Input, Kind, Level, Selector, Transition, judge};
use kvik_model::summary::Params;
use kvik_model::types::{
    Fact, Flags, Kind as SeriesKind, Nanos, Position, Quality, SECOND, SeriesId, Value,
};
use proptest::prelude::*;

use super::{Body, Core, FactItem, Item, Replay};

pub(super) const S: SeriesId = SeriesId(1);

#[derive(Clone, Debug)]
pub(super) enum Op {
    Fact {
        dt: i64,
        series: u32,
        v: Val,
        ahead: i64,
        bad: bool,
        version: u64,
    },
    Watch {
        dt: i64,
        template: usize,
    },
    Unwatch {
        dt: i64,
        k: usize,
    },
    Other {
        dt: i64,
    },
    /// A definition entry giving a series one of [`NAMES`], or retiring it (`name` beyond them).
    Define {
        dt: i64,
        series: u32,
        name: usize,
    },
    Reset {
        dt: i64,
    },
}

/// Names a definition gives; the first two match [`PATTERN`].
const NAMES: [&str; 3] = ["room/a/temp", "room/b/temp", "hall/a/temp"];
pub(super) const PATTERN: &str = "room/*/temp";

/// A fact's value: numbers mostly, and the other kinds every rule must pass over (spec §6 Gates).
#[derive(Clone, Debug)]
pub(super) enum Val {
    Num(u8),
    Str(u8),
    Bool(bool),
    Absent,
}

impl Val {
    fn value(&self) -> Value {
        match self {
            Val::Num(n) => Value::Num(f64::from(*n)),
            Val::Str(n) => Value::Str(format!("s{n}")),
            Val::Bool(b) => Value::Bool(*b),
            Val::Absent => Value::Absent,
        }
    }
}

fn arb_val() -> impl Strategy<Value = Val> {
    prop_oneof![
        12 => (0..=10u8).prop_map(Val::Num),
        2 => (0..3u8).prop_map(Val::Str),
        1 => any::<bool>().prop_map(Val::Bool),
        1 => Just(Val::Absent),
    ]
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (0..12i64, prop_oneof![6 => Just(1u32), 1 => 2..4u32], arb_val(),
              prop_oneof![9 => Just(0i64), 1 => 1..8i64], prop::bool::weighted(0.1), 0..4u64)
            .prop_map(|(dt, series, v, ahead, bad, version)| Op::Fact { dt, series, v, ahead, bad, version }),
        1 => (0..5i64, 0..5usize).prop_map(|(dt, template)| Op::Watch { dt, template }),
        1 => (0..5i64, 0..4usize).prop_map(|(dt, k)| Op::Unwatch { dt, k }),
        1 => (0..5i64).prop_map(|dt| Op::Other { dt }),
        2 => (0..5i64, 1..4u32, 0..4usize).prop_map(|(dt, series, name)| Op::Define { dt, series, name }),
        1 => (0..5i64).prop_map(|dt| Op::Reset { dt }),
    ]
}

/// The unit a case's times are drawn in, the holds drawn (in units) and the two `stale_after` drawn.
#[derive(Clone, Copy)]
struct Scale {
    unit: Nanos,
    holds: [i64; 3],
    stale: [i64; 2],
}

const SECONDS: Scale = Scale {
    unit: SECOND,
    holds: [0, 3, 7],
    stale: [15, 4],
};

/// Holds past 12 h, so `2L` meets the seed's 24 h cap (Q-1390).
const HOURS: Scale = Scale {
    unit: 3_600 * SECOND,
    holds: [0, 3, 13],
    stale: [9, 30],
};

fn arb_hold(sc: Scale) -> impl Strategy<Value = i64> + Clone {
    let [a, b, c] = sc.holds;
    prop_oneof![2 => Just(a), 1 => Just(b), 1 => Just(c)].prop_map(move |h| h * sc.unit)
}

fn arb_levels(sc: Scale) -> impl Strategy<Value = Vec<Level>> {
    let holds = arb_hold(sc);
    proptest::sample::subsequence((1..=9).collect::<Vec<i32>>(), 1..=3)
        .prop_flat_map(move |ats| {
            let n = ats.len();
            (
                Just(ats),
                prop::collection::vec(prop_oneof![Just(0.0), Just(0.5), Just(1.5)], n),
                prop::collection::vec((holds.clone(), holds.clone()), n),
            )
        })
        .prop_map(|(ats, gaps, holds)| {
            let mut out: Vec<Level> = Vec::new();
            for ((a, g), (u, d)) in ats.into_iter().zip(gaps).zip(holds) {
                let at = f64::from(a);
                let floor = out.last().map_or(f64::MIN, |l| l.clear);
                out.push(Level::at(at).clear((at - g).max(floor)).hold(u, d));
            }
            out
        })
}

pub(super) fn gate(kind: Kind, stale_after: Option<Nanos>) -> Gate {
    Gate {
        selector: Selector::Series(S),
        input: Input::Value,
        kind,
        stale_after,
    }
}

fn arb_templates(sc: Scale) -> impl Strategy<Value = Vec<Gate>> {
    let [a, b] = sc.stale;
    let stale = prop_oneof![Just(None), Just(Some(a * sc.unit)), Just(Some(b * sc.unit))];
    let member = prop_oneof![
        3 => (0..=10u8).prop_map(Val::Num),
        1 => (0..3u8).prop_map(Val::Str),
        1 => any::<bool>().prop_map(Val::Bool),
    ];
    (
        arb_levels(sc),
        arb_levels(sc),
        (stale.clone(), stale.clone(), stale),
        prop::collection::vec(member, 1..4),
        (arb_hold(sc), arb_hold(sc)),
    )
        .prop_map(|(l1, l2, (st1, st2, st3), members, (on, off))| {
            let set = Kind::Set {
                members: members.iter().map(Val::value).collect(),
                hold_on: on,
                hold_off: off,
            };
            let each = Gate {
                selector: Selector::Each(PATTERN.into()),
                ..gate(Kind::Levels(l2.clone()), st2)
            };
            vec![
                gate(Kind::Levels(l1), st1),
                gate(set, st3),
                gate(Kind::Change { deadband: 1.5 }, st1),
                gate(Kind::Levels(l2), st2),
                each,
            ]
        })
}

/// A log: model entries and the same entries as core items, in position order.
#[derive(Clone, Debug, Default)]
pub(super) struct Log {
    pub entries: Vec<Entry>,
    pub items: Vec<(Nanos, Item)>,
    pub replay: Replay,
    seq: u64,
}

impl Log {
    fn next(&mut self) -> Position {
        self.seq += 1;
        Position::new(0, self.seq)
    }

    pub fn fact(&mut self, series: SeriesId, commit: Nanos, t: Nanos, value: Value, q: Quality) {
        self.fact_v(series, commit, t, value, q, None);
    }

    /// A fact with a revision, for a versioned series (spec §2.2).
    pub fn fact_v(
        &mut self,
        series: SeriesId,
        commit: Nanos,
        t: Nanos,
        value: Value,
        q: Quality,
        changed_at: Option<u64>,
    ) {
        let position = self.next();
        let item = FactItem {
            series,
            t,
            value: value.clone(),
            quality: q,
            changed_at,
        };
        let outranked = self.replay.push(commit, position, item.clone());
        self.entries.push(Entry::Fact {
            commit_time: commit,
            fact: Fact {
                series,
                t,
                position,
                received_at: commit,
                value,
                quality: q,
                supplied_quality: q,
                changed_at,
                flags: Flags::default(),
            },
        });
        let body = Body::Fact {
            fact: item,
            outranked,
        };
        self.items.push((commit, Item { position, body }));
    }

    pub fn watch(&mut self, commit: Nanos, id: ConditionId, gate: Gate) {
        let position = self.next();
        self.entries.push(Entry::Watch {
            position,
            commit_time: commit,
            id,
            gate: gate.clone(),
        });
        let body = Body::Watch { id, gate };
        self.items.push((commit, Item { position, body }));
    }

    pub fn unwatch(&mut self, commit: Nanos, id: ConditionId) {
        let position = self.next();
        self.entries.push(Entry::Unwatch {
            position,
            commit_time: commit,
            id,
        });
        let body = Body::Unwatch { id };
        self.items.push((commit, Item { position, body }));
    }

    pub fn other(&mut self, commit: Nanos) {
        let position = self.next();
        self.entries.push(Entry::Other {
            position,
            commit_time: commit,
        });
        self.items.push((
            commit,
            Item {
                position,
                body: Body::Other,
            },
        ));
    }

    /// A definition entry giving `series` the names `names` (none: retired).
    pub fn define(&mut self, commit: Nanos, series: SeriesId, names: &[&str]) {
        let position = self.next();
        let names: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
        self.entries.push(Entry::Define {
            position,
            commit_time: commit,
            series,
            names: names.clone(),
            params: Params::new(SeriesKind::Gauge, None),
        });
        self.replay.define(position, series, names.clone());
        let body = Body::Define { series, names };
        self.items.push((commit, Item { position, body }));
    }

    pub fn reset(&mut self, commit: Nanos) {
        let position = self.next();
        self.entries.push(Entry::Reset {
            position,
            commit_time: commit,
        });
        self.items.push((
            commit,
            Item {
                position,
                body: Body::Reset,
            },
        ));
    }

    pub fn last_commit(&self) -> Nanos {
        self.items.last().map_or(0, |(c, _)| *c)
    }
}

fn build(templates: &[Gate], first: &[usize], ops: &[Op], versioned: bool, unit: Nanos) -> Log {
    let mut log = Log::default();
    let mut in_force: Vec<(ConditionId, usize)> = Vec::new();
    let mut ids = 0u32;
    let mut watch = |log: &mut Log, in_force: &mut Vec<_>, now, template: usize| {
        ids += 1;
        log.watch(now, ConditionId(ids), templates[template].clone());
        in_force.push((ConditionId(ids), template));
    };
    for t in first {
        watch(&mut log, &mut in_force, 0, *t);
    }
    let mut now = 0;
    for op in ops {
        match *op {
            Op::Fact {
                dt,
                series,
                ref v,
                ahead,
                bad,
                version,
            } => {
                now += dt * unit;
                let value = v.value();
                let q = if bad || matches!(value, Value::Absent) {
                    Quality::Bad
                } else {
                    Quality::Good
                };
                // A series is versioned for its whole life (spec §2.2): only S is, in a versioned case.
                let changed_at = (versioned && series == S.0).then_some(version);
                log.fact_v(
                    SeriesId(series),
                    now,
                    now + ahead * unit,
                    value,
                    q,
                    changed_at,
                );
            }
            Op::Watch { dt, template } => {
                now += dt * unit;
                watch(&mut log, &mut in_force, now, template);
            }
            Op::Unwatch { dt, k } => {
                now += dt * unit;
                if !in_force.is_empty() {
                    let (id, _) = in_force.remove(k % in_force.len());
                    log.unwatch(now, id);
                }
            }
            Op::Other { dt } => {
                now += dt * unit;
                log.other(now);
            }
            Op::Define { dt, series, name } => {
                now += dt * unit;
                let names: &[&str] = NAMES.get(name).map_or(&[], std::slice::from_ref);
                log.define(now, SeriesId(series), names);
            }
            Op::Reset { dt } => {
                now += dt * unit;
                log.reset(now);
            }
        }
    }
    log
}

/// How the core is driven over a log: where batches split, floors between them, where it is restored.
#[derive(Clone, Debug)]
pub(super) struct Drive {
    pub cuts: Vec<bool>,
    pub floors: Vec<u8>,
    pub restores: Vec<bool>,
}

fn arb_drive() -> impl Strategy<Value = Drive> {
    (
        prop::collection::vec(prop::bool::weighted(0.3), 64),
        prop::collection::vec(0..=4u8, 64),
        prop::collection::vec(prop::bool::weighted(0.15), 64),
    )
        .prop_map(|(cuts, floors, restores)| Drive {
            cuts,
            floors,
            restores,
        })
}

fn pick<T: Copy + Default>(v: &[T], i: usize) -> T {
    v.get(i % v.len().max(1)).copied().unwrap_or_default()
}

/// Gates in force after `items`, as the routing view would give them.
fn defs_through(items: &[(Nanos, Item)]) -> BTreeMap<ConditionId, Gate> {
    let mut defs = BTreeMap::new();
    for (_, i) in items {
        match &i.body {
            Body::Watch { id, gate } => {
                defs.insert(*id, gate.clone());
            }
            Body::Unwatch { id } => {
                defs.remove(id);
            }
            _ => {}
        }
    }
    defs
}

/// Feeds the log to a core as `drive` says, then fires timers up to `until`.
pub(super) fn run(log: &Log, drive: &Drive, until: Nanos) -> (Vec<Transition>, Core) {
    let mut core = Core::new();
    let mut out = Vec::new();
    let mut start = 0;
    let mut batch = 0;
    while start < log.items.len() {
        let commit = log.items[start].0;
        let mut end = start + 1;
        while end < log.items.len() && log.items[end].0 == commit && !pick(&drive.cuts, end) {
            end += 1;
        }
        let items: Vec<Item> = log.items[start..end]
            .iter()
            .map(|(_, i)| i.clone())
            .collect();
        out.extend(
            core.apply_batch(commit, &items, &log.replay)
                .expect("valid log"),
        );
        if let Some((next, _)) = log.items.get(end) {
            let frac = i64::from(pick(&drive.floors, batch));
            if frac > 0 {
                let now = core.frontier().now;
                out.extend(core.advance(now + (next - now) * frac / 4));
            }
        }
        if pick(&drive.restores, batch) {
            let defs = defs_through(&log.items[..end]);
            core = Core::restore(core.snapshot(), defs, &log.replay).expect("history in memory");
        }
        start = end;
        batch += 1;
    }
    out.extend(core.advance(until));
    (out, core)
}

/// Asserts the core gives `judge`'s transitions, in order, and its final states.
pub(super) fn check(log: &Log, drive: &Drive, unit: Nanos) -> Result<(), TestCaseError> {
    let until = log.last_commit() + 40 * unit;
    let model = judge(&log.entries, until).expect("valid log");
    let (got, core) = run(log, drive, until);
    prop_assert_eq!(&got, &model.transitions);
    prop_assert_eq!(core.states(), model.states);
    Ok(())
}

proptest! {
    #[test]
    fn the_core_gives_what_judge_gives_over_any_split_floor_and_restore(
        templates in arb_templates(SECONDS),
        first in prop::collection::vec(0..5usize, 0..3),
        ops in prop::collection::vec(arb_op(), 0..48),
        drive in arb_drive(),
        versioned in prop::bool::weighted(0.3),
    ) {
        let log = build(&templates, &first, &ops, versioned, SECONDS.unit);
        check(&log, &drive, SECONDS.unit)?;
    }

    #[test]
    fn the_core_gives_what_judge_gives_with_holds_past_the_seeds_day_cap(
        templates in arb_templates(HOURS),
        first in prop::collection::vec(0..5usize, 0..3),
        ops in prop::collection::vec(arb_op(), 0..48),
        drive in arb_drive(),
    ) {
        let log = build(&templates, &first, &ops, false, HOURS.unit);
        check(&log, &drive, HOURS.unit)?;
    }
}
