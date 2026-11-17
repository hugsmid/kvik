//! The bounded decider under a tiny memory cap against the durable fixes' decider, which keeps every fact with its
//! position (the oracle), through calls, acknowledgements polled in any order and losses found while open. The fake
//! log removes exactly the lost positions, so this proves the decision state and nothing about regions.

use std::time::Duration;

use kvik_log::entries::Entry;
use kvik_log::reader::FoundFact;
use proptest::prelude::*;

use super::*;
use crate::durable::history::History;
use crate::durable::ingest as oracle;
use crate::durable::keys::Found;
use crate::durable::registry::definition_of;
use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Quality, SeriesId};

const S: i64 = 1_000_000_000;
const WALL: i64 = 1_767_225_600 * S;
/// Every series' S; `late` by the reference is more than 20 s behind it.
const STALENESS: Duration = Duration::from_secs(10);

/// The log: every fact queued, with its call and position, shown once the log has completed its call, less the lost
/// positions.
#[derive(Debug, Default)]
struct Log {
    facts: Vec<(u64, u64, FactRow)>,
    /// Every call below this one is complete.
    done: u64,
    lost: Vec<(u64, u64)>,
    next_sequence: u64,
    span_reads: usize,
}

impl Log {
    fn shows(&self, call: u64, sequence: u64) -> bool {
        call < self.done
            && !self
                .lost
                .iter()
                .any(|&(a, b)| a <= sequence && sequence <= b)
    }

    fn shown(&self) -> impl Iterator<Item = FoundFact> + '_ {
        self.facts
            .iter()
            .filter(|(c, p, _)| self.shows(*c, *p))
            .map(|(_, p, row)| FoundFact {
                position: pos(*p),
                received_at: 0,
                row: row.clone(),
            })
    }
}

impl LocalFacts for Log {
    fn at(&mut self, keys: &[Key]) -> Found {
        let mut out = Found::default();
        for f in self.shown() {
            let key = (f.row.series_id, f.row.t);
            if keys.contains(&key) {
                out.facts.entry(key).or_default().push(f);
            }
        }
        out
    }

    fn read_between(&mut self, series: u32, from: i64, to: i64) -> (Vec<FoundFact>, bool) {
        self.span_reads += 1;
        let facts = self
            .shown()
            .filter(|f| f.row.series_id == series && (from..=to).contains(&f.row.t))
            .collect();
        (facts, true)
    }
}

fn pos(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

/// One reading: series, seconds from `WALL`, value, `changed_at`.
type R = (u32, i64, u8, u64);

#[derive(Clone, Debug)]
enum Step {
    /// A call, with revise intent or not, with the clock synchronised or not.
    Call(Vec<R>, bool, bool),
    /// The log completes this many more calls.
    Complete(u8),
    /// The calls in flight are polled; a complete call is seen when its flag (cycled) is set.
    Poll(Vec<bool>),
    /// The positions of the `n`-th complete call with positions (cycled), through as many more calls, are lost, and
    /// reported lost through as many more positions as the third field says (Q-937);
    /// then a poll, and a call for each series and offset, of one reading that far below its old reference, the
    /// lowest first.
    Lose(usize, u8, u8, Vec<bool>, Vec<u8>),
}

struct Rig {
    bounded: Decider,
    oracle: oracle::Decider,
    log: Log,
    next_call: u64,
    /// Calls queued with facts, in call order, until the bounded decider has seen them acknowledged.
    inflight: Vec<(u64, LogPos, LogPos)>,
    placed: Vec<(u64, LogPos, LogPos)>,
    /// The ranges the log lost, for the oracle, and the ranges reported, for the bounded decider.
    fresh: Vec<(LogPos, LogPos)>,
    reported: Vec<Fresh>,
    budget: usize,
    forgettings: usize,
}

fn define(registry: &mut crate::durable::registry::Registry) {
    let gauge = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .staleness(STALENESS);
    let versioned = gauge.clone().versioned(ChangedAtKind::Version);
    for (id, spec) in [(1, gauge.clone()), (2, versioned), (3, gauge)] {
        let def = definition_of(&spec, id, Duration::from_secs(300)).unwrap();
        registry.apply(&Entry::Definition(Box::new(def)));
    }
}

fn reading(&(series, secs, v, c): &R) -> Reading {
    let v = [1.0, 2.0, -1.0][usize::from(v % 3)];
    Reading {
        changed_at: (series == 2).then_some(c),
        quality: if v < 0.0 { Quality::Bad } else { Quality::Good },
        ..Reading::new(SeriesId(series), Timestamp(WALL + secs * S), Value::F64(v))
    }
}

fn clock(synced: bool) -> Clock {
    Clock {
        wall: WALL,
        synced,
        future_tolerance: 60 * S,
    }
}

impl Rig {
    fn new(span: i64, cap: usize, budget: usize) -> Self {
        let mut bounded = Decider {
            history: History::with_window_and_budget(span, cap, budget),
            ..Decider::default()
        };
        let mut o = oracle::Decider::default();
        define(&mut bounded.registry);
        define(&mut o.registry);
        Rig {
            bounded,
            oracle: o,
            log: Log::default(),
            next_call: 1,
            inflight: Vec::new(),
            placed: Vec::new(),
            fresh: Vec::new(),
            reported: Vec::new(),
            budget,
            forgettings: 0,
        }
    }

    fn call(&mut self, rs: &[R], revise: bool, synced: bool) -> Result<(), TestCaseError> {
        let readings: Vec<Reading> = rs.iter().map(reading).collect();
        let intent = if revise {
            Intent::Revise
        } else {
            Intent::Normal
        };
        let call = self.next_call;
        self.next_call += 1;
        let c = clock(synced);
        let want = self.oracle.plan(
            &readings,
            intent,
            oracle::Clock {
                wall: c.wall,
                synced: c.synced,
                future_tolerance: c.future_tolerance,
            },
            call,
        );
        self.oracle.apply(&want);
        let mut got = self
            .bounded
            .plan(&readings, intent, clock(synced), call, &mut self.log);
        let as_oracle: Vec<oracle::Decision> = got
            .decisions
            .iter()
            .map(|d| match *d {
                Decision::Refused(r) => oracle::Decision::Refused(r),
                Decision::Duplicate => oracle::Decision::Duplicate,
                Decision::Record {
                    item,
                    outranked,
                    absent,
                    ..
                } => oracle::Decision::Record {
                    item,
                    outranked,
                    absent,
                },
            })
            .collect();
        prop_assert_eq!(&as_oracle, &want.decisions, "call {}", call);
        prop_assert_eq!(&got.items, &want.items, "call {}", call);
        for (d, item) in got.decisions.iter().filter_map(|d| match d {
            Decision::Record {
                item,
                late,
                future,
                unchecked,
                ..
            } => Some(((*late, *future, *unchecked), *item)),
            _ => None,
        }) {
            let Item::Fact(row) = &got.items[item] else {
                unreachable!()
            };
            prop_assert_eq!(d, (row.flags.late, row.flags.future, false));
        }
        prop_assert_eq!(got.reference_unread, 0);
        // A match in the log is durable: a dependence on a call the log shows is none.
        let done = self.log.done;
        prop_assert_eq!(
            got.depends.filter(|c| *c >= done),
            want.depends.filter(|c| *c >= done)
        );
        let n = got.items.len() as u64;
        if n > 0 {
            let first = self.log.next_sequence + 1;
            for (i, item) in got.items.iter().enumerate() {
                if let Item::Fact(row) = item {
                    self.log.facts.push((call, first + i as u64, row.clone()));
                }
            }
            self.log.next_sequence += n;
            self.inflight.push((call, pos(first), pos(first + n - 1)));
        }
        self.bounded.apply(&mut got);
        let held = self.bounded.history.held();
        prop_assert!(held.bytes <= self.budget, "{held:?}");
        Ok(())
    }

    fn poll(&mut self, seen: &[bool]) {
        let done = self.log.done;
        let acks: Vec<(u64, Option<(LogPos, LogPos)>)> = self
            .inflight
            .iter()
            .enumerate()
            .map(|(i, &(c, a, b))| {
                let ready = c < done && seen.get(i % seen.len().max(1)).copied().unwrap_or(true);
                (c, ready.then_some((a, b)))
            })
            .collect();
        let fresh = std::mem::take(&mut self.fresh);
        let reported = std::mem::take(&mut self.reported);
        let new = reported.iter().any(|f| f.grew);
        let taken = self.bounded.settle(acks, self.next_call, &reported);
        self.forgettings += usize::from(new);
        for c in &taken {
            let i = self.inflight.iter().position(|(x, _, _)| x == c).unwrap();
            let (c, a, b) = self.inflight.remove(i);
            self.oracle.placed(c, a, b);
            self.placed.push((c, a, b));
        }
        for (a, b) in fresh {
            self.oracle.lose(a, b);
        }
        if new {
            assert_eq!(self.bounded.history.held().bytes, 0);
        }
    }

    fn lose(&mut self, n: usize, more: u8, wider: u8) -> bool {
        let done = self.log.done;
        let complete: Vec<(u64, u64)> = self
            .log
            .facts
            .iter()
            .filter(|(c, _, _)| *c < done)
            .map(|(c, p, _)| (*c, *p))
            .collect();
        if complete.is_empty() {
            return false;
        }
        let calls: Vec<u64> = {
            let mut v: Vec<u64> = complete.iter().map(|(c, _)| *c).collect();
            v.dedup();
            v
        };
        let i = n % calls.len();
        let last_call = calls[(i + usize::from(more)).min(calls.len() - 1)];
        let in_range = |c: u64| calls[i] <= c && c <= last_call;
        let a = complete.iter().find(|(c, _)| in_range(*c)).unwrap().1;
        let b = complete.iter().rev().find(|(c, _)| in_range(*c)).unwrap().1;
        let head = complete.iter().map(|(_, p)| *p).max().unwrap_or(b);
        let grew = (a..=b).any(|p| !self.log.lost.iter().any(|&(x, y)| x <= p && p <= y));
        self.log.lost.push((a, b));
        self.fresh.push((pos(a), pos(b)));
        self.reported.push(Fresh {
            first: pos(a),
            last: pos(b.saturating_add(u64::from(wider)).min(head)),
            grew,
        });
        true
    }

    fn run(&mut self, steps: &[Step]) -> Result<(), TestCaseError> {
        for step in steps {
            match step {
                Step::Call(rs, revise, synced) => self.call(rs, *revise, *synced)?,
                Step::Complete(k) => {
                    self.log.done = (self.log.done.max(1) + u64::from(*k)).min(self.next_call);
                }
                Step::Poll(seen) => self.poll(seen),
                Step::Lose(n, more, wider, seen, offsets) => {
                    let before = self.oracle.reference.clone();
                    if self.lose(*n, *more, *wider) {
                        self.poll(seen);
                        let mut offsets = offsets.clone();
                        offsets.sort_unstable_by(|a, b| b.cmp(a));
                        for (&id, &rf) in &before {
                            for &o in &offsets {
                                let secs = (rf - WALL) / S + 5 - i64::from(o);
                                self.call(&[(id, secs, o, 9)], false, true)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let secs = prop_oneof![6 => -150i64..70, 1 => -420i64..-300, 1 => 60i64..120];
    let reading = (1u32..4, secs, 0u8..3, 0u64..3);
    let mask = prop::collection::vec(prop::bool::weighted(0.6), 1..6);
    prop::collection::vec(
        prop_oneof![
            6 => (
                prop::collection::vec(reading, 1..6),
                prop::bool::weighted(0.1),
                prop::bool::weighted(0.85)
            )
                .prop_map(|(r, rev, synced)| Step::Call(r, rev, synced)),
            2 => (0u8..3).prop_map(Step::Complete),
            2 => mask.clone().prop_map(Step::Poll),
            1 => (
                any::<usize>(),
                0u8..2,
                prop_oneof![2 => Just(0u8), 1 => 1u8..20],
                mask,
                prop::collection::vec(0u8..40, 1..4)
            )
                .prop_map(|(n, more, wider, seen, offsets)| Step::Lose(n, more, wider, seen, offsets)),
        ],
        1..60,
    )
}

proptest! {
    #[test]
    fn decisions_with_losses_while_open_under_a_tiny_cap_equal_the_oracles(
        history in steps(),
        span in 1i64..60,
        cap in 1usize..4,
        budget in prop_oneof![Just(usize::MAX), 0usize..400],
    ) {
        Rig::new(span * S, cap, budget).run(&history)?;
    }
}

fn run(steps: &[Step]) -> Rig {
    let mut rig = Rig::new(30 * S, 2, 2 * std::mem::size_of::<(i64, Known)>());
    if let Err(e) = rig.run(steps) {
        panic!("{e}");
    }
    rig
}

fn call(rs: &[R]) -> Step {
    Step::Call(rs.to_vec(), false, true)
}

#[test]
fn oracle_acknowledgements_polled_out_of_order_stop_at_the_first_pending() {
    let rig = run(&[
        call(&[(1, -10, 0, 0)]),
        call(&[(1, -5, 0, 0)]),
        call(&[(1, -1, 0, 0)]),
        Step::Complete(3),
        Step::Poll(vec![false, true, true]),
        Step::Lose(1, 0, 0, vec![false, true, true], vec![0, 10]),
        call(&[(1, -5, 0, 0)]),
        Step::Poll(vec![true]),
        call(&[(1, -5, 0, 0)]),
    ]);
    assert!(rig.forgettings > 0 && rig.placed.len() >= 3);
}

#[test]
fn oracle_a_log_that_shows_a_call_before_its_acknowledgement_is_polled() {
    let rig = run(&[
        call(&[(3, -50, 0, 0)]),
        call(&[(1, -10, 0, 0), (2, -10, 0, 1)]),
        Step::Complete(2),
        Step::Poll(vec![true, false]),
        call(&[(1, -10, 0, 0), (2, -10, 0, 1)]),
        Step::Lose(0, 0, 0, vec![false], vec![0]),
        call(&[(1, -10, 0, 0), (2, -10, 0, 1), (2, -10, 1, 0)]),
    ]);
    assert_eq!(rig.placed.len(), 1);
    assert!(rig.inflight.iter().any(|(c, _, _)| *c == 2));
}

#[test]
fn oracle_a_loss_taken_while_a_call_inside_it_is_in_flight_then_acknowledged() {
    let rig = run(&[
        call(&[(1, -10, 0, 0)]),
        call(&[(1, -8, 0, 0)]),
        Step::Complete(2),
        Step::Lose(0, 1, 0, vec![false], vec![5]),
        call(&[(1, -10, 0, 0), (1, -8, 0, 0)]),
        Step::Poll(vec![true]),
        call(&[(1, -10, 0, 0), (1, -8, 0, 0)]),
    ]);
    assert!(rig.placed.len() >= 2);
}

#[test]
fn oracle_two_losses_in_a_row_with_facts_noted_between() {
    let rig = run(&[
        call(&[(1, -30, 0, 0), (3, -30, 0, 0)]),
        call(&[(1, -20, 0, 0)]),
        Step::Complete(2),
        Step::Poll(vec![true]),
        Step::Lose(1, 0, 0, vec![true], vec![0, 20]),
        call(&[(1, -25, 1, 0), (3, -10, 0, 0)]),
        Step::Complete(3),
        Step::Lose(0, 0, 0, vec![true], vec![0, 20]),
        call(&[(1, -30, 0, 0), (1, -25, 1, 0), (1, -20, 0, 0)]),
    ]);
    assert!(rig.forgettings >= 2);
}

#[test]
fn oracle_facts_already_in_the_overlay_at_a_forgetting() {
    let rig = run(&[
        call(&[(1, -40, 0, 0)]),
        Step::Complete(1),
        call(&[(2, -1, 0, 2)]),
        call(&[(3, -2, 0, 0), (1, -3, 0, 0), (3, -1, 1, 0)]),
        Step::Lose(0, 0, 0, vec![true, false], vec![0, 15, 30]),
        call(&[(2, -1, 0, 2), (2, -1, 1, 1)]),
    ]);
    assert!(!rig.inflight.is_empty());
}

#[test]
fn oracle_a_reading_below_the_old_reference_after_a_loss_takes_the_reference_read() {
    let rig = run(&[
        call(&[(1, -100, 0, 0)]),
        call(&[(1, 0, 0, 0)]),
        Step::Complete(2),
        Step::Poll(vec![true]),
        Step::Lose(1, 0, 0, vec![true], vec![0, 10, 30]),
    ]);
    assert!(rig.log.span_reads > 0);
}

#[test]
fn oracle_versions_late_and_future_readings_and_an_unsynchronised_clock_after_a_loss() {
    let rig = run(&[
        call(&[(2, -10, 0, 2), (2, -10, 1, 1), (1, -400, 0, 0)]),
        Step::Call(vec![(1, 100, 0, 0), (3, -5, 0, 0)], false, false),
        call(&[(1, 100, 0, 0), (3, -5, 0, 0)]),
        Step::Complete(3),
        Step::Poll(vec![true]),
        Step::Lose(0, 0, 0, vec![true], vec![0, 30]),
        call(&[
            (2, -10, 0, 2),
            (2, -10, 0, 1),
            (2, -10, 2, 3),
            (1, -400, 0, 0),
        ]),
        Step::Call(vec![(3, -60, 0, 0)], false, false),
        call(&[(3, -60, 0, 0), (1, 100, 0, 0)]),
    ]);
    assert!(rig.forgettings > 0);
}

#[test]
fn oracle_a_loss_found_inside_a_range_already_reported_too_wide() {
    let rig = run(&[
        call(&[(3, -20, 0, 0)]),
        call(&[(2, -10, 0, 1)]),
        call(&[(2, -10, 1, 2)]),
        Step::Complete(3),
        Step::Poll(vec![true]),
        Step::Lose(0, 0, 5, vec![true], vec![0]),
        call(&[(2, -10, 2, 3)]),
        Step::Complete(9),
        Step::Poll(vec![true]),
        Step::Lose(2, 0, 0, vec![true], vec![0]),
        call(&[(2, -10, 1, 2), (2, -10, 0, 0), (2, -10, 1, 1)]),
    ]);
    assert!(rig.forgettings >= 2);
}
