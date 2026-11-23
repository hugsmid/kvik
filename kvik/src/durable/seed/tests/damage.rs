//! Damage below the seed found after the open (Q-1110 (b)), what meets it before the scan does (Q-928), and the seed's
//! own checks (Q-1111).

use std::sync::Arc;

use kvik_log::directory::Family;

use super::super::{PROBE, Probe};
use super::world::*;
use crate::Store;
use crate::consume::{DeliveryKind, Select, Start};
use crate::durable::scan::{Gate, SCAN_GATES};
use crate::health::{Health, LossKind, LossReport, Reason};
use crate::read::View;
use crate::time::{TimeRange, Timestamp};
use crate::types::{Outcome, SeriesId};

fn all() -> TimeRange {
    TimeRange {
        from: Timestamp(i64::MIN),
        to: Timestamp(i64::MAX),
    }
}

fn data_lost(store: &Store) -> Vec<LossReport> {
    store
        .losses()
        .into_iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .collect()
}

/// A store with three sealed segments of facts of every series and a consumer that confirmed none, shut down
/// cleanly with a series directory through its newest batch; the first fact batch, in the oldest sealed segment, is
/// then damaged.
fn damaged_below_the_seed() -> World {
    let mut w = World::new();
    w.step(&Step::Route(0, 0));
    for round in 0..3u8 {
        for k in 0..4u8 {
            w.step(&Step::Emit(
                vec![
                    (1, 0, k, 0, false),
                    (2, 0, k, round, false),
                    (3, 0, k, 0, false),
                ],
                false,
            ));
            w.step(&Step::Advance(1));
        }
        w.step(&Step::Seal);
    }
    w.step(&Step::Emit(vec![(1, 0, 5, 0, false)], false));
    w.shut();
    assert!(sealed(&w.fs, &w.dir) >= 3);
    write_directory(&w.env, &w.dir, newest_batch(&w.env, &w.dir));
    assert_eq!(damage(&w.fs, &w.env, &w.dir, 0), vec![1, 2, 3]);
    w
}

/// Opens `w`'s store with its background scan held until the gate returned opens.
fn open_held(w: &World) -> (Store, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    SCAN_GATES
        .lock()
        .unwrap()
        .insert(w.dir.clone(), gate.clone());
    let env = env_on(&w.fs, &w.clock, w.boot);
    let store = open(&w.dir, &env, false);
    (store, gate)
}

#[test]
fn damage_below_the_seed_is_reported_once_the_scan_ends_and_the_next_start_reads_the_whole_log() {
    let w = damaged_below_the_seed();
    let (store, gate) = open_held(&w);
    let stats = store.stats().ingest;
    assert!(stats.start_seeded && stats.start_scanning);
    assert!(data_lost(&store).is_empty());
    gate.open();
    store.writer().unwrap().wait_scan();
    let found = data_lost(&store);
    assert_eq!(found.len(), 1);
    assert!(!found[0].recorded);
    assert!(!store.stats().ingest.start_scanning);
    block_on(store.shutdown()).unwrap();
    // The fold broke (Q-1002): no directory is left to seed the next start.
    assert!(kvik_log::directory::generations(&*w.env.fs, &w.dir, Family::Series).is_empty());
    let next = open(&w.dir, &env_on(&w.fs, &w.clock, w.boot), false);
    assert!(!next.stats().ingest.start_seeded);
    let again = data_lost(&next);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].identity, found[0].identity);
    block_on(next.shutdown()).unwrap();
}

#[test]
fn a_read_that_meets_damage_below_the_seed_before_the_scan_reports_it_and_the_scan_completes_the_report()
 {
    let w = damaged_below_the_seed();
    let (store, gate) = open_held(&w);
    let facts = store
        .reader()
        .facts(SeriesId(1), all(), View::History)
        .unwrap()
        .collect::<Vec<_>>();
    assert!(facts.iter().all(Result::is_ok));
    assert_eq!(facts.len(), 12);
    let found = data_lost(&store);
    assert_eq!(found.len(), 1);
    assert!(store.stats().ingest.start_scanning);
    gate.open();
    store.writer().unwrap().wait_scan();
    let after = data_lost(&store);
    assert_eq!(after.len(), 1);
    assert_eq!(
        (after[0].identity, after[0].first, after[0].last),
        (found[0].identity, found[0].first, found[0].last)
    );
    // The scan read the batch after the region, as the start from the whole log does.
    assert!(after[0].commit_time_after.is_some());
    block_on(store.shutdown()).unwrap();
}

#[test]
fn a_replay_of_a_lost_fact_decided_before_the_scan_meets_the_damage_and_is_recorded_unchecked() {
    let w = damaged_below_the_seed();
    let (store, gate) = open_held(&w);
    let lost = to_readings(T0, &[(1, 0, 0, 0, false)]);
    let ack = block_on(store.emit(&lost)).unwrap();
    assert!(matches!(
        ack.outcomes[..],
        [Outcome::Recorded {
            unchecked: true,
            ..
        }]
    ));
    gate.open();
    store.writer().unwrap().wait_scan();
    assert_eq!(data_lost(&store).len(), 1);
    block_on(store.shutdown()).unwrap();
}

#[test]
fn a_consumer_behind_the_seed_that_meets_its_damage_before_the_scan_skips_it_and_the_scan_reports_it()
 {
    let w = damaged_below_the_seed();
    let (store, gate) = open_held(&w);
    let mut c = block_on(store.consumer(CONSUMERS[0], Start::Now, Select::None)).unwrap();
    let mut facts = Vec::new();
    while let Ok(Some(d)) = c.try_next() {
        let _ = block_on(c.confirm(d.id));
        if let DeliveryKind::Fact(f) = d.kind {
            facts.push(f.position);
        }
    }
    // Every fact but the damaged batch's three; a region a consumer meets first is counted, not reported (Q-928).
    assert_eq!(facts.len(), 3 * 4 * 3 + 1 - 3);
    assert!(data_lost(&store).is_empty());
    assert!(
        matches!(store.health(), Health::Degraded { reasons } if reasons.iter().any(|r| matches!(r,
            Reason::LocalDataUnreadable { regions, .. } if *regions > 0)))
    );
    gate.open();
    store.writer().unwrap().wait_scan();
    let found = data_lost(&store);
    assert_eq!(found.len(), 1);
    let lost = found[0].first.unwrap()..=found[0].last.unwrap();
    assert!(facts.iter().all(|p| !lost.contains(p)));
    drop(c);
    block_on(store.shutdown()).unwrap();
}

#[test]
fn a_registry_file_behind_the_seed_makes_the_start_read_the_whole_log() {
    let mut w = World::new();
    w.step(&Step::Emit(vec![(1, 0, 1, 0, false)], false));
    w.shut();
    write_directory(&w.env, &w.dir, newest_batch(&w.env, &w.dir));
    let store_id = kvik_log::store::read_descriptor(&*w.env.fs, &w.dir)
        .unwrap()
        .store_id;
    let (_, mut file) = kvik_log::registry::read_latest(&*w.env.fs, &w.dir, &store_id)
        .unwrap()
        .unwrap();
    let keep = file.entries.len() - 1;
    file.entries.truncate(keep);
    file.through = file.entries.last().map_or(0, |(s, _, _)| *s);
    kvik_log::registry::write(&*w.env.fs, &w.dir, store_id, w.clock.wall().0, &file).unwrap();
    PROBE.set(Probe::default());
    let store = open(&w.dir, &env_on(&w.fs, &w.clock, w.boot), false);
    assert!(!store.stats().ingest.start_seeded);
    assert!(store.series(SeriesId(3)).is_ok());
    block_on(store.shutdown()).unwrap();
}
