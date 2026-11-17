//! Decisions after a range found lost while the store is open (Q-921 to Q-924).

use super::*;

const WALL: i64 = CLOCK.wall;

fn at(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

/// A newly found lost range, from position `a` to `b`.
fn found(a: u64, b: u64) -> Fresh {
    Fresh {
        first: at(a),
        last: at(b),
        grew: true,
    }
}

/// Series 5, an observed gauge with S = 10 s: `late` by the reference is more than 20 s behind it.
fn with_observed(mut d: Decider) -> Decider {
    let spec = SeriesSpec::new(
        "a/b/power",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .staleness(Duration::from_secs(10));
    let def = definition_of(&spec, 5, Duration::from_secs(300)).unwrap();
    d.registry.apply(&Entry::Definition(Box::new(def)));
    d
}

/// Series 6, as series 5 but late by the lateness bound only after a day, so that the reference alone decides.
fn with_observed_for_a_day(mut d: Decider) -> Decider {
    let spec = SeriesSpec::new(
        "a/b/energy",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .staleness(Duration::from_secs(10));
    let def = definition_of(&spec, 6, Duration::from_secs(86_400)).unwrap();
    d.registry.apply(&Entry::Definition(Box::new(def)));
    d
}

fn placed(mem: &Mem, call: u64) -> Option<(LogPos, LogPos)> {
    let mut ns = mem
        .facts
        .iter()
        .enumerate()
        .filter(|(_, (c, _))| *c == call)
        .map(|(n, _)| n as u64 + 1);
    let first = ns.next()?;
    Some((at(first), at(ns.next_back().unwrap_or(first))))
}

/// The log shows every call below `next`, each is seen acknowledged, and the ranges `fresh` are taken as lost.
fn settle(d: &mut Decider, mem: &mut Mem, next: u64, fresh: &[(u64, u64)]) -> Vec<u64> {
    mem.readable_below = next;
    mem.lost.extend_from_slice(fresh);
    let mut calls: Vec<u64> = mem
        .facts
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| *c < next)
        .collect();
    calls.dedup();
    let acks: Vec<_> = calls.iter().map(|&c| (c, placed(mem, c))).collect();
    let fresh: Vec<_> = fresh.iter().map(|&(a, b)| found(a, b)).collect();
    d.settle(acks, next, &fresh)
}

fn late(p: &Plan) -> bool {
    matches!(p.decisions[..], [Decision::Record { late: true, .. }])
}

fn one(d: &Decider, mem: &mut Mem, r: Reading, call: u64) -> Plan {
    d.plan(&[r], Intent::Normal, CLOCK, call, mem)
}

#[test]
fn a_reading_only_a_lost_version_outranked_is_not_outranked() {
    let mut d = decider(History::default());
    let mut mem = Mem::default();
    let t = WALL - 10 * S;
    commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 1.0, Some(1))],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 2.0, Some(2))],
        Intent::Normal,
        2,
    );
    settle(&mut d, &mut mem, 3, &[]);
    settle(&mut d, &mut mem, 3, &[(2, 2)]);
    let p = one(&d, &mut mem, reading(2, t, 3.0, Some(1)), 3);
    assert_eq!(
        p.decisions,
        vec![Decision::Record {
            item: 0,
            outranked: false,
            absent: false,
            late: false,
            future: false,
            unchecked: false
        }]
    );
}

#[test]
fn a_lost_latest_fact_no_longer_holds_the_lateness_reference() {
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 100 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL, 2.0, None)],
        Intent::Normal,
        2,
    );
    settle(&mut d, &mut mem, 3, &[]);
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 50 * S, 3.0, None),
        3
    )));
    settle(&mut d, &mut mem, 3, &[(2, 2)]);
    assert!(!late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 50 * S, 3.0, None),
        3
    )));
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 130 * S, 3.0, None),
        3
    )));
}

#[test]
fn a_call_acknowledged_inside_a_range_already_lost_leaves_the_history() {
    let replay = || reading(5, WALL - 5 * S, 1.0, None);
    // Lost while the call is in flight, then acknowledged.
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    commit(&mut d, &mut mem, &[replay()], Intent::Normal, 1);
    let fresh = [found(1, 1)];
    mem.lost.push((1, 1));
    assert!(d.settle([(1, None)], 2, &fresh).is_empty());
    assert_eq!(settle(&mut d, &mut mem, 2, &[]), vec![1]);
    assert!(matches!(
        one(&d, &mut mem, replay(), 2).decisions[0],
        Decision::Record { .. }
    ));
    // The range taken before the call's facts joined the window: its acknowledgement forgets them.
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    settle(&mut d, &mut mem, 1, &[(1, 1)]);
    commit(&mut d, &mut mem, &[replay()], Intent::Normal, 1);
    assert!(matches!(
        one(&d, &mut mem, replay(), 2).decisions[0],
        Decision::Duplicate
    ));
    settle(&mut d, &mut mem, 2, &[]);
    assert!(matches!(
        one(&d, &mut mem, replay(), 2).decisions[0],
        Decision::Record { .. }
    ));
}

#[test]
fn a_reference_fact_in_the_overlay_at_a_loss_still_holds_the_lateness_reference() {
    let two_facts = 2 * std::mem::size_of::<(i64, Known)>();
    let mut d = with_observed(decider(History::with_budget(two_facts)));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(1, WALL - 3 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL, 1.0, None)],
        Intent::Normal,
        2,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(3, WALL - S, 1.0, None)],
        Intent::Normal,
        3,
    );
    assert_eq!(d.history.held().overlay, 1, "{:?}", d.history.held());
    mem.readable_below = 2;
    mem.lost.push((1, 1));
    let seen = d.settle(
        [(1, placed(&mem, 1)), (2, None), (3, placed(&mem, 3))],
        4,
        &[found(1, 1)],
    );
    assert_eq!(seen, vec![1]);
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 50 * S, 1.0, None),
        4
    )));
    assert_eq!(mem.span_reads, 0);
    settle(&mut d, &mut mem, 4, &[]);
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 30 * S, 1.0, None),
        4
    )));
}

#[test]
fn a_reference_fact_an_incomplete_read_finds_proves_lateness_and_none_found_is_not_late_and_counted()
 {
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    for (call, secs) in [(1, 100), (2, 60), (3, 0)] {
        let r = reading(5, WALL - secs * S, 1.0, None);
        commit(&mut d, &mut mem, &[r], Intent::Normal, call);
    }
    settle(&mut d, &mut mem, 4, &[(3, 3)]);
    mem.incomplete_from = Some(WALL - 40 * S);
    let p = commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 90 * S, 1.0, None)],
        Intent::Normal,
        4,
    );
    assert!(late(&p));
    assert_eq!(p.reference_unread, 0);
    assert_eq!(
        d.history.reference(5),
        Reference::AtMost {
            upper: WALL,
            least: Some(WALL - 60 * S)
        }
    );
    settle(&mut d, &mut mem, 5, &[(2, 2)]);
    let p = one(&d, &mut mem, reading(5, WALL - 70 * S, 1.0, None), 5);
    assert!(!late(&p));
    assert_eq!(p.reference_unread, 1);
    mem.incomplete_from = None;
    let p = one(&d, &mut mem, reading(5, WALL - 125 * S, 1.0, None), 5);
    assert!(late(&p), "the fact at -90 s, found once the read is whole");
}

#[test]
fn a_read_that_finds_no_reference_narrows_it_and_a_reading_it_then_decides_reads_nothing() {
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 100 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL, 1.0, None)],
        Intent::Normal,
        2,
    );
    settle(&mut d, &mut mem, 3, &[(2, 2)]);
    let p = commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 50 * S, 1.0, None)],
        Intent::Normal,
        3,
    );
    assert!(!late(&p));
    assert_eq!(mem.span_reads, 1);
    assert_eq!(
        d.history.reference(5),
        Reference::AtMost {
            upper: WALL - 30 * S,
            least: Some(WALL - 50 * S)
        }
    );
    assert!(!late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 45 * S, 1.0, None),
        4
    )));
    assert_eq!(mem.span_reads, 1);
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 71 * S, 1.0, None),
        4
    )));
    assert_eq!(
        mem.span_reads, 1,
        "decided by the least known reference fact"
    );
    settle(&mut d, &mut mem, 4, &[(1, 1)]);
    assert!(late(&one(
        &d,
        &mut mem,
        reading(5, WALL - 125 * S, 1.0, None),
        4
    )));
    assert_eq!(
        mem.span_reads, 2,
        "the fact at -50 s, read once the call holding it is shown"
    );
}

#[test]
fn a_loss_found_inside_a_range_already_reported_too_wide_forgets_its_facts() {
    let mut d = decider(History::default());
    let mut mem = Mem::default();
    let t = WALL - 10 * S;
    commit(
        &mut d,
        &mut mem,
        &[reading(1, t - S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 1.0, Some(1))],
        Intent::Normal,
        2,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 2.0, Some(2))],
        Intent::Normal,
        3,
    );
    // Reported lost to the head (Q-937); the log lost position 1 only.
    mem.readable_below = 4;
    mem.lost.push((1, 1));
    let acks: Vec<_> = (1..4).map(|c| (c, placed(&mem, c))).collect();
    d.settle(acks, 4, &[found(1, 3)]);
    let p = commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 3.0, Some(3))],
        Intent::Normal,
        4,
    );
    assert!(matches!(p.decisions[0], Decision::Record { .. }));
    assert_eq!(d.history.held().revised_keys, 1);
    mem.readable_below = 5;
    mem.lost.push((3, 3));
    d.settle([(4, placed(&mem, 4))], 5, &[found(3, 3)]);
    let replay = one(&d, &mut mem, reading(2, t, 2.0, Some(2)), 5);
    assert!(
        matches!(
            replay.decisions[0],
            Decision::Record {
                outranked: true,
                ..
            }
        ),
        "{replay:?}"
    );
    let lower = one(&d, &mut mem, reading(2, t, 9.0, Some(1)), 5);
    assert!(matches!(
        lower.decisions[0],
        Decision::Record {
            outranked: true,
            ..
        }
    ));
    commit(
        &mut d,
        &mut mem,
        &[reading(2, t, 4.0, Some(4))],
        Intent::Normal,
        5,
    );
    assert_eq!(d.history.held().revised_keys, 1);
    mem.readable_below = 6;
    let again = Fresh {
        grew: false,
        ..found(3, 3)
    };
    d.settle([(5, placed(&mem, 5))], 6, &[again]);
    assert_eq!(
        d.history.held().revised_keys,
        1,
        "the same damage found again forgets nothing"
    );
}

#[test]
fn a_reference_fact_several_parts_below_the_first_is_found_by_the_doubling_walk() {
    let mut d = with_observed_for_a_day(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(6, WALL - 3_600 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(6, WALL, 1.0, None)],
        Intent::Normal,
        2,
    );
    settle(&mut d, &mut mem, 3, &[(2, 2)]);
    let p = commit(
        &mut d,
        &mut mem,
        &[reading(6, WALL - 3_700 * S, 1.0, None)],
        Intent::Normal,
        3,
    );
    assert!(late(&p));
    assert_eq!(mem.span_reads, 3, "15, 30 and 60 minutes");
    assert_eq!(
        d.history.reference(6),
        Reference::Exact(Some(WALL - 3_600 * S))
    );
}

#[test]
fn a_reference_fact_found_below_an_incomplete_part_leaves_the_reference_at_most_its_old_value() {
    let mut d = with_observed_for_a_day(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(6, WALL - 3_600 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    commit(
        &mut d,
        &mut mem,
        &[reading(6, WALL, 1.0, None)],
        Intent::Normal,
        2,
    );
    settle(&mut d, &mut mem, 3, &[(2, 2)]);
    mem.incomplete_from = Some(WALL - 600 * S);
    let p = one(&d, &mut mem, reading(6, WALL - 3_700 * S, 1.0, None), 3);
    assert!(late(&p));
    assert_eq!(
        p.references,
        vec![(
            6,
            Reference::AtMost {
                upper: WALL,
                least: Some(WALL - 3_600 * S)
            }
        )]
    );
}

#[test]
fn a_fact_taken_while_the_clock_is_unsynchronised_leaves_the_lateness_reference_alone() {
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 100 * S, 1.0, None)],
        Intent::Normal,
        1,
    );
    let unsynced = Clock {
        synced: false,
        ..CLOCK
    };
    let mut plan = d.plan(
        &[reading(5, WALL, 2.0, None)],
        Intent::Normal,
        unsynced,
        2,
        &mut mem,
    );
    d.apply(&mut plan);
    let p = commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL - 50 * S, 3.0, None)],
        Intent::Normal,
        3,
    );
    assert!(!late(&p));
}

/// Q-925 as proposed (the switch in `Decider::read_reference`): with rule (b) the reading is late.
#[test]
fn a_reading_whose_reference_cannot_be_read_is_not_late_and_is_counted() {
    let mut d = with_observed(decider(History::default()));
    let mut mem = Mem::default();
    commit(
        &mut d,
        &mut mem,
        &[reading(5, WALL, 1.0, None)],
        Intent::Normal,
        1,
    );
    settle(&mut d, &mut mem, 2, &[(1, 1)]);
    mem.incomplete_from = Some(WALL - 200 * S);
    let p = one(&d, &mut mem, reading(5, WALL - 50 * S, 1.0, None), 2);
    assert!(!late(&p));
    assert_eq!(p.reference_unread, 1);
    assert_eq!(
        p.references,
        vec![(
            5,
            Reference::AtMost {
                upper: WALL,
                least: None
            }
        )]
    );
}
