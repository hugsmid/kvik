use std::collections::HashSet;
use std::time::Duration;

use kvik_log::entries::Entry;
use kvik_log::reader::FoundFact;
use kvik_log::writer::Position as LogPos;
use proptest::prelude::*;

use super::*;
use crate::durable::history::{History, Key, WINDOW_FACTS};
use crate::durable::keys::Found;
use crate::durable::registry::definition_of;
use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Quality, SeriesId};

const T0: i64 = 1_767_225_600_000_000_000;
const S: i64 = 1_000_000_000;

const CLOCK: Clock = Clock {
    wall: T0 + 3_600 * S,
    synced: true,
    future_tolerance: 60 * S,
};

fn decider(history: History) -> Decider {
    let mut d = Decider {
        history,
        ..Decider::default()
    };
    let gauge = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let versioned = gauge.clone().versioned(ChangedAtKind::Version);
    for (id, spec) in [(1, gauge.clone()), (2, versioned), (3, gauge)] {
        let def = definition_of(&spec, id, Duration::from_secs(300)).unwrap();
        d.registry.apply(&Entry::Definition(Box::new(def)));
    }
    d
}

fn reading(series: u32, t: i64, v: f64, changed_at: Option<u64>) -> Reading {
    Reading {
        changed_at,
        quality: if v < 0.0 { Quality::Bad } else { Quality::Good },
        ..Reading::new(SeriesId(series), Timestamp(t), Value::F64(v))
    }
}

/// Local data as a log holds it: every committed fact with its call, shown once its call is readable.
#[derive(Debug, Default)]
struct Mem {
    facts: Vec<(u64, FactRow)>,
    readable_below: u64,
    unreadable: HashSet<Key>,
    reads: usize,
}

impl LocalFacts for Mem {
    fn at(&mut self, keys: &[Key]) -> Found {
        self.reads += 1;
        let mut out = Found::default();
        for (n, (call, row)) in self.facts.iter().enumerate() {
            let key = (row.series_id, row.t);
            if *call < self.readable_below && keys.contains(&key) && !self.unreadable.contains(&key)
            {
                out.facts.entry(key).or_default().push(FoundFact {
                    position: LogPos {
                        generation: 0,
                        sequence: n as u64 + 1,
                    },
                    received_at: 0,
                    row: row.clone(),
                });
            }
        }
        out.unreadable = keys
            .iter()
            .filter(|k| self.unreadable.contains(k))
            .copied()
            .collect();
        out
    }
    fn between(&mut self, _: u32, _: i64, _: i64) -> Option<Vec<FoundFact>> {
        unreachable!("the decision reads keys only")
    }
}

/// [`LocalFacts`] for a decider that must never take the slow path.
#[derive(Debug)]
struct Never;

impl LocalFacts for Never {
    fn at(&mut self, keys: &[Key]) -> Found {
        panic!("slow path for {keys:?}")
    }
    fn between(&mut self, _: u32, _: i64, _: i64) -> Option<Vec<FoundFact>> {
        unreachable!()
    }
}

/// Plans and applies a call, committing its facts to `mem`.
fn commit(d: &mut Decider, mem: &mut Mem, readings: &[Reading], intent: Intent, call: u64) -> Plan {
    let mut plan = d.plan(readings, intent, CLOCK, call, mem);
    for item in &plan.items {
        if let Item::Fact(r) = item {
            mem.facts.push((call, r.clone()));
        }
    }
    d.apply(&mut plan);
    plan
}

/// A history in which the first reading's key leaves the window: a reading at `T0`, then one an hour later.
fn left_behind() -> (Decider, Mem) {
    let mut d = decider(History::default());
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(1, T0 + 3_600 * S, 1.0, None)],
        Intent::Normal,
        2,
    );
    (d, mem)
}

#[test]
fn a_call_mixing_versioned_and_unversioned_series_decides_duplicates_and_outranked_per_key() {
    let mut d = decider(History::default());
    let mut mem = Mem::default();
    let first = [reading(2, T0, 1.0, Some(5)), reading(1, T0, 1.0, None)];
    commit(&mut d, &mut mem, &first, Intent::Normal, 1);
    let readings = [
        reading(1, T0, 1.0, None),
        reading(2, T0, 2.0, Some(3)),
        reading(2, T0, 1.0, Some(5)),
        reading(1, T0 + S, 1.0, None),
        reading(2, T0 + S, 1.0, Some(1)),
        reading(2, T0 + S, 3.0, Some(0)),
        reading(1, T0 + S, 1.0, None),
    ];
    let plan = d.plan(&readings, Intent::Normal, CLOCK, 2, &mut Never);
    let got: Vec<_> = plan
        .decisions
        .iter()
        .map(|x| match x {
            Decision::Record { outranked, .. } => Some(*outranked),
            Decision::Duplicate => None,
            Decision::Refused(r) => panic!("refused: {r:?}"),
        })
        .collect();
    assert_eq!(
        got,
        [
            None,
            Some(true),
            None,
            Some(false),
            Some(false),
            Some(true),
            None
        ]
    );
    assert_eq!(plan.depends, Some(1));
}

#[test]
fn a_key_below_the_window_is_read_once_per_call_and_enters_the_revised_key_index() {
    let (mut d, mut mem) = left_behind();
    mem.readable_below = 3;
    d.readable_below(3);
    let again = [
        reading(1, T0, 1.0, None),
        reading(1, T0, 2.0, None),
        reading(1, T0, 1.0, None),
    ];
    let plan = commit(&mut d, &mut mem, &again, Intent::Normal, 3);
    assert_eq!(mem.reads, 1);
    assert_eq!(plan.slow, 3);
    assert!(matches!(plan.decisions[0], Decision::Duplicate));
    assert!(matches!(
        plan.decisions[1],
        Decision::Record {
            unchecked: false,
            ..
        }
    ));
    assert!(matches!(plan.decisions[2], Decision::Duplicate));
    assert_eq!(plan.depends, None);
    // Two facts now: the key is in the revised-key index and needs no read.
    let plan = commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 2.0, None)],
        Intent::Normal,
        4,
    );
    assert_eq!((mem.reads, plan.slow), (1, 0));
    assert!(matches!(plan.decisions[0], Decision::Duplicate));
    assert_eq!(d.history.held().revised_keys, 1);
}

#[test]
fn a_key_of_several_facts_leaves_the_window_whole_into_the_revised_key_index() {
    let mut d = decider(History::default());
    let mut mem = Mem::default();
    let first = [
        reading(1, T0, 1.0, None),
        reading(1, T0, 2.0, None),
        reading(1, T0, 3.0, None),
    ];
    commit(&mut d, &mut mem, &first, Intent::Normal, 1);
    mem.readable_below = 2;
    d.readable_below(2);
    commit(
        &mut d,
        &mut mem,
        &[reading(1, T0 + 3_600 * S, 1.0, None)],
        Intent::Normal,
        2,
    );
    assert_eq!(d.history.held().revised_facts, 3);
    let plan = commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 3.0, None)],
        Intent::Normal,
        3,
    );
    assert!(matches!(plan.decisions[0], Decision::Duplicate));
    assert_eq!(mem.reads, 0);
}

#[test]
fn a_reading_above_the_latest_t_takes_the_fast_path_and_one_in_the_window_reads_nothing() {
    let (mut d, mut mem) = left_behind();
    let readings = [
        reading(1, T0 + 3_601 * S, 1.0, None),
        reading(1, T0 + 3_000 * S, 1.0, None),
        reading(1, T0 + 3_600 * S, 1.0, None),
    ];
    let plan = commit(&mut d, &mut mem, &readings, Intent::Normal, 3);
    assert_eq!((mem.reads, plan.slow), (0, 0));
    assert!(matches!(plan.decisions[2], Decision::Duplicate));
}

#[test]
fn a_key_that_cannot_be_read_is_recorded_unchecked_unless_a_known_fact_matches() {
    let (mut d, mut mem) = left_behind();
    mem.readable_below = 3;
    d.readable_below(3);
    mem.unreadable.insert((1, T0));
    let plan = commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 1.0, None)],
        Intent::Normal,
        3,
    );
    assert!(matches!(
        plan.decisions[0],
        Decision::Record {
            unchecked: true,
            ..
        }
    ));
    // The unchecked fact is in the overlay until its call is readable: a repeat matches it.
    let plan = commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 1.0, None)],
        Intent::Normal,
        4,
    );
    assert!(matches!(plan.decisions[0], Decision::Duplicate));
    assert_eq!(plan.depends, Some(3));
}

#[test]
fn a_fact_that_leaves_the_window_before_its_call_is_readable_stays_in_the_overlay() {
    let (mut d, mut mem) = left_behind();
    assert_eq!(d.history.held().overlay, 1);
    let plan = commit(
        &mut d,
        &mut mem,
        &[reading(1, T0, 1.0, None)],
        Intent::Normal,
        3,
    );
    assert!(matches!(plan.decisions[0], Decision::Duplicate));
    assert_eq!(plan.depends, Some(1));
    d.readable_below(3);
    assert_eq!(d.history.held().overlay, 0);
}

/// One step of a decision history: a call (series, seconds, value, `changed_at`) with revise intent or not, or
/// every call so far acknowledged.
#[derive(Clone, Debug)]
enum Step {
    Call(Vec<(u32, i64, u8, u64)>, bool),
    Ack,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let t = prop_oneof![4 => 0i64..30, 1 => 0i64..400];
    let reading = (1u32..4, t, 0u8..3, 0u64..3);
    prop::collection::vec(
        prop_oneof![
            6 => (prop::collection::vec(reading, 1..8), prop::bool::weighted(0.1))
                .prop_map(|(r, rev)| Step::Call(r, rev)),
            2 => Just(Step::Ack),
        ],
        1..60,
    )
}

fn readings_of(rs: &[(u32, i64, u8, u64)]) -> Vec<Reading> {
    rs.iter()
        .map(|&(series, secs, v, c)| {
            let v = [1.0, 2.0, -1.0][usize::from(v)];
            reading(series, T0 + secs * S, v, (series == 2).then_some(c))
        })
        .collect()
}

proptest! {
    #[test]
    fn decisions_with_a_bounded_history_equal_those_with_every_fact_in_memory(
        history in steps(),
        span in 1i64..20,
        cap in 1usize..6,
        budget in prop_oneof![Just(usize::MAX), 0usize..2_000],
    ) {
        let mut bounded = decider(History::with_window_and_budget(span * S, cap, budget));
        let mut all = decider(History::with_window(i64::MAX, usize::MAX));
        let mut mem = Mem::default();
        let mut call = 1;
        for step in &history {
            match step {
                Step::Ack => {
                    mem.readable_below = call;
                    bounded.readable_below(call);
                    all.readable_below(call);
                }
                Step::Call(rs, revise) => {
                    let readings = readings_of(rs);
                    let intent = if *revise { Intent::Revise } else { Intent::Normal };
                    let mut reference = all.plan(&readings, intent, CLOCK, call, &mut Never);
                    all.apply(&mut reference);
                    let plan = commit(&mut bounded, &mut mem, &readings, intent, call);
                    prop_assert_eq!(&plan.decisions, &reference.decisions);
                    // A match in the log is durable: a dependence on a call already readable is none.
                    let readable = mem.readable_below;
                    prop_assert_eq!(
                        plan.depends.filter(|c| *c >= readable),
                        reference.depends.filter(|c| *c >= readable)
                    );
                    prop_assert_eq!(&plan.items, &reference.items);
                    call += 1;
                    let held = bounded.history.held();
                    prop_assert!(held.window <= held.series * cap, "{held:?}");
                }
            }
        }
    }

    #[test]
    fn the_decision_state_is_bounded_by_the_window_and_the_revised_keys_not_by_the_readings(
        n in 1usize..4_000,
        revisions in prop::collection::btree_set(0usize..4_000, 0..20),
    ) {
        let mut d = decider(History::default());
        let mut mem = Mem::default();
        let series = 3usize;
        let mut revised = 0;
        for k in 0..n {
            let id = (k % series) as u32 + 1;
            let t = T0 + (k / series) as i64 * S;
            let c = (id == 2).then_some(1);
            let mut call = vec![reading(id, t, 1.0, c)];
            if revisions.contains(&k) {
                call.push(reading(id, t, 2.0, c.map(|c| c + 1)));
                revised += 1;
            }
            commit(&mut d, &mut mem, &call, Intent::Normal, k as u64 + 1);
            d.readable_below(k as u64 + 2);
            mem.readable_below = k as u64 + 2;
        }
        let held = d.history.held();
        // The window: 15 minutes of one reading a second per series, within the 1,024-fact cap, revisions included.
        prop_assert!(held.window <= series * 901 + revised, "{held:?}");
        prop_assert!(held.window <= series * WINDOW_FACTS);
        prop_assert!(held.revised_keys <= revised);
        prop_assert_eq!(held.revised_facts, 2 * held.revised_keys);
        prop_assert_eq!(held.overlay, 0);
        prop_assert_eq!(held.series, series.min(n));
        prop_assert_eq!(mem.reads, 0);
    }

    #[test]
    fn the_decision_state_stays_within_its_byte_budget(
        n in 1usize..3_000,
        series in 1u32..60,
        budget in 1_000usize..40_000,
        revisions in prop::collection::btree_set(0usize..3_000, 0..40),
    ) {
        let mut d = decider(History::with_budget(budget));
        for id in 4..series + 4 {
            let spec = SeriesSpec::new(format!("a/s{id}/v"), Kind::Gauge { value: ValueType::F64 }, "W");
            let def = definition_of(&spec, id, Duration::from_secs(300)).unwrap();
            d.registry.apply(&Entry::Definition(Box::new(def)));
        }
        let mut mem = Mem::default();
        for k in 0..n {
            let id = (k as u32 % series) + 4;
            let t = T0 + (k as u32 / series) as i64 * S;
            let mut call = vec![reading(id, t, 1.0, None)];
            if revisions.contains(&k) {
                call.push(reading(id, t, 2.0, None));
            }
            commit(&mut d, &mut mem, &call, Intent::Normal, k as u64 + 1);
            d.readable_below(k as u64 + 2);
            mem.readable_below = k as u64 + 2;
            let held = d.history.held();
            // Within the budget once the call is acknowledged, but for the call just applied.
            prop_assert!(held.bytes <= budget + 2 * 200, "{held:?}");
        }
        // Everything acknowledged: a replay of the first reading is still a duplicate, from the log.
        let plan = commit(&mut d, &mut mem, &[reading(4, T0, 1.0, None)], Intent::Normal, n as u64 + 1);
        prop_assert_eq!(&plan.decisions, &vec![Decision::Duplicate]);
    }
}
