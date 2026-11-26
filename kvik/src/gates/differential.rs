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
        v: u8,
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
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (0..12i64, prop_oneof![6 => Just(1u32), 1 => 2..4u32], 0..=10u8,
              prop_oneof![9 => Just(0i64), 1 => 1..8i64], prop::bool::weighted(0.1), 0..4u64)
            .prop_map(|(dt, series, v, ahead, bad, version)| Op::Fact { dt, series, v, ahead, bad, version }),
        1 => (0..5i64, 0..4usize).prop_map(|(dt, template)| Op::Watch { dt, template }),
        1 => (0..5i64, 0..4usize).prop_map(|(dt, k)| Op::Unwatch { dt, k }),
        1 => (0..5i64).prop_map(|dt| Op::Other { dt }),
    ]
}

fn arb_levels() -> impl Strategy<Value = Vec<Level>> {
    let holds = prop_oneof![2 => Just(0i64), 1 => Just(3), 1 => Just(7)];
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
                out.push(
                    Level::at(at)
                        .clear((at - g).max(floor))
                        .hold(u * SECOND, d * SECOND),
                );
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

fn arb_templates() -> impl Strategy<Value = Vec<Gate>> {
    let stale = prop_oneof![Just(None), Just(Some(15 * SECOND)), Just(Some(4 * SECOND))];
    (
        arb_levels(),
        arb_levels(),
        stale.clone(),
        stale,
        prop::collection::vec(0..=10u8, 1..4),
        (0..6i64, 0..6i64),
    )
        .prop_map(|(l1, l2, st1, st2, members, (on, off))| {
            let set = Kind::Set {
                members: members
                    .into_iter()
                    .map(|m| Value::Num(f64::from(m)))
                    .collect(),
                hold_on: on * SECOND,
                hold_off: off * SECOND,
            };
            vec![
                gate(Kind::Levels(l1), st1),
                gate(set, None),
                gate(Kind::Change { deadband: 1.5 }, st1),
                gate(Kind::Levels(l2), st2),
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
        self.entries.push(Entry::Define {
            position,
            commit_time: commit,
            series: SeriesId(2),
            names: vec!["hall/b/temp".into()],
            params: Params::new(SeriesKind::Gauge, None),
        });
        self.items.push((
            commit,
            Item {
                position,
                body: Body::Other,
            },
        ));
    }

    pub fn last_commit(&self) -> Nanos {
        self.items.last().map_or(0, |(c, _)| *c)
    }
}

fn build(templates: &[Gate], first: &[usize], ops: &[Op], versioned: bool) -> Log {
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
                v,
                ahead,
                bad,
                version,
            } => {
                now += dt * SECOND;
                let q = if bad { Quality::Bad } else { Quality::Good };
                let value = Value::Num(f64::from(v));
                // A series is versioned for its whole life (spec §2.2): only S is, in a versioned case.
                let changed_at = (versioned && series == S.0).then_some(version);
                log.fact_v(
                    SeriesId(series),
                    now,
                    now + ahead * SECOND,
                    value,
                    q,
                    changed_at,
                );
            }
            Op::Watch { dt, template } => {
                now += dt * SECOND;
                watch(&mut log, &mut in_force, now, template);
            }
            Op::Unwatch { dt, k } => {
                now += dt * SECOND;
                if !in_force.is_empty() {
                    let (id, _) = in_force.remove(k % in_force.len());
                    log.unwatch(now, id);
                }
            }
            Op::Other { dt } => {
                now += dt * SECOND;
                log.other(now);
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
            core = Core::restore(core.snapshot(), defs, &log.replay);
        }
        start = end;
        batch += 1;
    }
    out.extend(core.advance(until));
    (out, core)
}

/// Asserts the core gives `judge`'s transitions, in order, and its final states.
pub(super) fn check(log: &Log, drive: &Drive) -> Result<(), TestCaseError> {
    let until = log.last_commit() + 40 * SECOND;
    let model = judge(&log.entries, until).expect("valid log");
    let (got, core) = run(log, drive, until);
    prop_assert_eq!(&got, &model.transitions);
    prop_assert_eq!(core.states(), model.states);
    Ok(())
}

proptest! {
    #[test]
    fn the_core_gives_what_judge_gives_over_any_split_floor_and_restore(
        templates in arb_templates(),
        first in prop::collection::vec(0..4usize, 0..3),
        ops in prop::collection::vec(arb_op(), 0..48),
        drive in arb_drive(),
        versioned in prop::bool::weighted(0.3),
    ) {
        let log = build(&templates, &first, &ops, versioned);
        check(&log, &drive)?;
    }
}
