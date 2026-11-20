use std::time::Duration;

use kvik_log::content::BatchContent;
use kvik_log::facts::FactFlags as LogFlags;
use kvik_log::reader::VoidCause;
use kvik_log::scan::{InvalidRegion, LossCause as ScanCause};

use std::sync::Arc;

use super::super::read::pmap::NODE;
use super::super::read::pmap::count::{self, Counts};
use super::*;
use crate::env::ManualClock;
use crate::series::{Kind, SeriesSpec, ValueType};

fn batch(first: u64, items: Vec<Item>) -> ReadEvent {
    let n: u64 = items.iter().map(Item::sequences).sum();
    let p = |sequence| LogPos {
        generation: 0,
        sequence,
    };
    ReadEvent::Batch(ReadBatch {
        file_id: 1,
        offset: 0,
        first: p(first),
        last: p(first + n - 1),
        commit_time: first as i64,
        content: BatchContent {
            first_sequence: first,
            received_at: first as i64,
            items,
        },
    })
}

const MARK: LogPos = LogPos {
    generation: 0,
    sequence: 100,
};

fn region(start: u64, before: Option<u64>, after: Option<u64>) -> ReadEvent {
    ReadEvent::Corruption {
        file_id: 1,
        region: InvalidRegion {
            start,
            end: start + 64,
            last_sequence_before: before,
            first_sequence_after: after,
            cause: ScanCause::Corruption,
        },
    }
}

fn fact(t: i64, v: f64) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t,
        quality: LogQuality::Good,
        flags: LogFlags::default(),
        value: LogValue::F64(v.to_bits()),
        changed_at: None,
    })
}

fn fact_of(series_id: u32, t: i64) -> Item {
    match fact(t, 1.0) {
        Item::Fact(row) => Item::Fact(FactRow { series_id, ..row }),
        other => other,
    }
}

/// An index of `series` series with 20 facts each in 80 batches, published, then given one batch with one fact:
/// what that batch cost the map.
fn cost_of_one_fact(series: u32) -> Counts {
    let mut i = Index::default();
    let mut next = 1;
    for t in 0..20 {
        // Four batches each, so the commit times' map is the same size whatever the series.
        for ids in (1..=series).collect::<Vec<_>>().chunks(series as usize / 4) {
            let items: Vec<_> = ids.iter().map(|id| fact_of(*id, t)).collect();
            let n = items.len() as u64;
            i.apply(&[batch(next, items)], MARK);
            next += n;
        }
    }
    let published = i.clone();
    let ((), c) = count::during(|| {
        i.apply(&[batch(next, vec![fact_of(series / 2, 20)])], MARK);
    });
    assert_eq!(published.series_facts(series / 2).count(), 20);
    assert_eq!(i.series_facts(series / 2).count(), 21);
    assert_eq!(i.latest_t(series / 2), Some(20));
    c
}

#[test]
fn a_refresh_after_a_publish_costs_about_the_same_whatever_the_number_of_series() {
    let small = cost_of_one_fact(128);
    let large = cost_of_one_fact(16_384);
    // One node a level on each path it changes: one level more for 128 times the series, and a root that grows.
    assert!(
        large.copied <= small.copied + 2 * NODE as u64,
        "{small:?} {large:?}"
    );
    assert!(
        large.allocations <= small.allocations + 2,
        "{small:?} {large:?}"
    );
    assert!(
        large.bytes <= small.bytes + 2 * 1_024,
        "{small:?} {large:?}"
    );
    assert!(
        large.allocations <= 16 && large.bytes <= 24 * 1_024,
        "{large:?}"
    );
}

/// An index of one gauge with `n` facts at seconds 1 to `n`: what a live-state read at the end costs the map.
fn cost_of_live(n: i64) -> Counts {
    let spec = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let def = super::super::registry::definition_of(&spec, 1, Duration::from_secs(300)).unwrap();
    let mut i = Index::default();
    i.apply(
        &[batch(
            1,
            vec![Item::Entry(Box::new(Entry::Definition(Box::new(def))))],
        )],
        MARK,
    );
    let mut next = 2;
    for first in (1..=n).step_by(10_000) {
        let items: Vec<_> = (first..(first + 10_000).min(n + 1))
            .map(|t| fact(t * 1_000_000_000, t as f64))
            .collect();
        let len = items.len() as u64;
        i.apply(&[batch(next, items)], MARK);
        next += len;
    }
    let clocks = super::super::clock::Clocks::new(Arc::new(ManualClock::new(Timestamp(0))));
    let now = (n + 1) * 1_000_000_000;
    let (live, c) = count::during(|| super::super::windows::live_in(&i, &clocks, SeriesId(1), now));
    assert!(matches!(live, Ok(crate::read::Live::Value(_))), "{live:?}");
    c
}

#[test]
fn a_live_read_visits_a_path_whatever_the_length_of_the_series() {
    let (short, long) = (cost_of_live(1_000), cost_of_live(200_000));
    assert!(long.visited <= short.visited + 4, "{short:?} {long:?}");
    assert_eq!(long.allocations, short.allocations, "{short:?} {long:?}");
    assert!(long.visited <= 16, "{long:?}");
}

#[test]
fn a_void_cuts_facts_and_entries_back_and_rebuilds_the_registry() {
    let spec = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let def = super::super::registry::definition_of(&spec, 1, Duration::from_secs(300)).unwrap();
    let mut i = Index::default();
    i.apply(
        &[
            batch(1, vec![Item::from(Entry::Definition(Box::new(def)))]),
            batch(2, vec![fact(10, 1.0), fact(20, 2.0)]),
            batch(
                4,
                vec![Item::from(Entry::Alias {
                    id: 1,
                    name: "a/b/d".into(),
                })],
            ),
        ],
        MARK,
    );
    assert!(i.registry().resolve("a/b/d").is_some());
    assert_eq!(i.series_facts(1).count(), 2);
    // A start truncated everything above position 2: the second fact and the alias are void.
    i.apply(
        &[ReadEvent::Void {
            above: LogPos {
                generation: 0,
                sequence: 2,
            },
            cause: VoidCause::Rewritten,
        }],
        MARK,
    );
    assert!(i.registry().resolve("a/b/d").is_none());
    assert!(i.registry().resolve("a/b/c").is_some());
    let left: Vec<_> = i.series_facts(1).map(|s| s.row.t).collect();
    assert_eq!(left, vec![10]);
    assert_eq!(i.latest_t(1), Some(10));
    // Positions above the void are taken again.
    i.apply(&[batch(3, vec![fact(30, 3.0)])], MARK);
    assert_eq!(i.latest_t(1), Some(30));
    // Everything void.
    i.apply(
        &[ReadEvent::Void {
            above: LogPos::default(),
            cause: VoidCause::Rewritten,
        }],
        MARK,
    );
    assert!(i.registry().get(1).is_none());
    assert_eq!(i.series_facts(1).count(), 0);
    assert_eq!(
        i.fact_series().count(),
        0,
        "a series the void emptied is gone"
    );
}

#[test]
fn a_region_found_by_a_read_is_a_latched_loss_between_its_batches() {
    let mut i = Index::default();
    let found = i.apply(
        &[
            batch(1, vec![fact(10, 1.0)]),
            region(100, Some(1), Some(4)),
            batch(4, vec![fact(40, 4.0)]),
        ],
        MARK,
    );
    assert_eq!(found.len(), 1);
    let l = &i.losses(&[])[0];
    assert_eq!(
        (l.identity, l.first, l.last),
        (
            Position::new(0, 2),
            Some(Position::new(0, 2)),
            Some(Position::new(0, 3))
        )
    );
    assert_eq!(
        (l.commit_time_before, l.commit_time_after),
        (Some(Timestamp(1)), Some(Timestamp(4)))
    );
    assert!(!l.recorded);
    // Found again by a later pass: one loss.
    assert!(i.apply(&[region(100, Some(1), Some(4))], MARK).is_empty());
    assert_eq!(i.losses(&[]).len(), 1);
}

#[test]
fn a_region_at_the_tail_runs_to_the_marks_position_and_a_record_replaces_it() {
    let mut i = Index::default();
    i.apply(
        &[batch(1, vec![fact(10, 1.0)]), region(100, Some(1), None)],
        MARK,
    );
    let l = &i.losses(&[])[0];
    assert_eq!(l.last, Some(Position::new(0, MARK.sequence)));
    let record = LostRange {
        identity: (0, 2),
        first: (0, 2),
        last: (0, MARK.sequence),
        file_id: 1,
        offset: 100,
        length: 64,
        cause: LogCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: Some(1),
        time_after: None,
        extends: false,
        stamp: None,
    };
    let losses = i.losses(&[record]);
    assert_eq!(losses.len(), 1);
    assert!(losses[0].recorded);
}

#[test]
fn a_region_that_held_no_position_is_no_loss() {
    let mut i = Index::default();
    let found = i.apply(
        &[
            batch(1, vec![fact(10, 1.0)]),
            region(100, Some(1), Some(2)),
            batch(2, vec![fact(20, 2.0)]),
        ],
        MARK,
    );
    assert!(found.is_empty());
}
