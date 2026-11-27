//! What a tree read holds and which roots and parts it passes over.

use std::sync::Arc;
use std::time::Duration;

use kvik_tree::root::Covered;

use super::{HOUR, MIN, Rig, assert_t7_on, history};
use crate::durable::buckets_of;
use crate::durable::read::cache::Sizes;
use crate::durable::reader::DurableReader;
use crate::durable::windows::nodes::{FETCHED_MOST, NODES_USED};
use crate::read::Resolution;
use crate::time::{TimeRange, Timestamp};
use crate::types::SeriesId;

/// A reader in a second process, the writer shut down, with `sizes` for its memory.
fn second_reader(rig: &Rig, sizes: Option<Sizes>) -> Arc<DurableReader> {
    rig.w.shutdown().unwrap();
    let mut env = crate::env::Env::test(rig.clock.clone());
    env.fs = Arc::new(rig.fs.clone());
    env.background_timers = false;
    let hour = Duration::from_secs(3_600);
    let r = DurableReader::open(&rig.dir, &env, hour, hour).unwrap();
    Arc::new(match sizes {
        Some(s) => r.with_memory(s),
        None => r,
    })
}

fn quarter(t: i64) -> i64 {
    t - t.rem_euclid(15 * MIN)
}

#[test]
fn a_chart_over_more_uncovered_readings_than_the_memory_hint_holds_them_not_and_takes_facts() {
    let mut rig = Rig::new("uncovered-memory");
    let first: Vec<(i64, u8)> = (0..4_000i64)
        .map(|k| (k * 3 * MIN, (k % 200) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    let now = rig.now();
    rig.narrow(vec![Covered {
        level: 2,
        from: quarter(now - 3 * HOUR),
        to: quarter(now - HOUR),
    }]);
    let cap = 512 * 1024;
    let r = second_reader(
        &rig,
        Some(Sizes {
            cap,
            block: 0,
            min_rows: 1,
            lookups: 0,
        }),
    );
    let hint = r.memory().rows_hint();
    let ids: Vec<SeriesId> = rig.ids.iter().map(|(id, _)| *id).collect();
    FETCHED_MOST.with(|c| c.set(0));
    let before = NODES_USED.with(std::cell::Cell::get);
    buckets_of(
        &r,
        &ids,
        TimeRange::new(Timestamp(now - 8 * 24 * HOUR), Timestamp(now)),
        Resolution::Step(Duration::from_secs(3_600)),
    )
    .unwrap();
    let held = FETCHED_MOST.with(std::cell::Cell::get);
    assert!(held <= hint, "{held} bytes held against a hint of {hint}");
    assert_eq!(
        NODES_USED.with(std::cell::Cell::get),
        before,
        "taken from facts"
    );
    assert_t7_on(&r, &rig, &[(now - 8 * 24 * HOUR, now)]);
}

/// The stored nodes one hourly chart over the last 40 hours uses.
fn nodes_used(rig: &Rig) -> u64 {
    let ids: Vec<SeriesId> = rig.ids.iter().map(|(id, _)| *id).collect();
    let now = rig.now();
    let before = NODES_USED.with(std::cell::Cell::get);
    buckets_of(
        &rig.reader(),
        &ids,
        TimeRange::new(Timestamp(now - 40 * HOUR), Timestamp(now)),
        Resolution::Step(Duration::from_secs(3_600)),
    )
    .unwrap();
    NODES_USED.with(std::cell::Cell::get) - before
}

#[test]
fn a_newer_root_of_another_generation_store_or_reset_or_with_k_past_the_cut_is_passed_over_for_the_fit_one()
 {
    let mut rig = Rig::new("unfit-roots");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 89 * super::S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    rig.checkpoint();
    let fit = rig.newest_root();
    assert!(nodes_used(&rig) > 0);
    let unfit: [(&str, fn(&mut kvik_tree::root::Root)); 4] = [
        ("another generation", |r| r.header.generation += 1),
        ("another store", |r| r.header.store_id = [0xee; 16]),
        ("another reset", |r| {
            r.reset = kvik_log::mark::ResetStamp {
                sequence: 1,
                nonce: 99,
            }
        }),
        ("K past the cut", |r| r.checkpoint.sequence += 1 << 40),
    ];
    for (why, alter) in unfit {
        let mut r = fit.clone();
        r.covered = Vec::new();
        alter(&mut r);
        rig.publish(r);
        assert!(nodes_used(&rig) > 0, "{why}: passed over");
    }
    let mut r = fit;
    r.covered = Vec::new();
    rig.publish(r);
    assert_eq!(nodes_used(&rig), 0, "a fit root covering nothing is taken");
}

#[test]
fn a_series_retired_in_the_stretch_is_charted_from_facts_and_equals_the_fact_path() {
    let mut rig = Rig::new("retired");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 7 * MIN, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    assert!(nodes_used(&rig) > 0);
    let now = rig.now();
    for (id, _) in &rig.ids {
        rig.w.retire(*id, Timestamp(now - 30 * HOUR)).unwrap();
    }
    assert_eq!(nodes_used(&rig), 0, "a node-changing entry in the stretch");
    assert_t7_on(&rig.reader(), &rig, &super::ranges(now));
}

#[test]
fn a_part_that_decodes_but_is_not_the_one_the_root_records_is_damaged_and_charts_equal_the_fact_path()
 {
    let mut rig = Rig::new("not-recorded");
    let first: Vec<(i64, u8)> = (0..1_500i64)
        .map(|k| (k * 97 * super::S, (k % 251) as u8))
        .collect();
    history(&mut rig, &first, &[], true);
    let mut r = rig.newest_root();
    for p in &mut r.parts {
        p.file_crc32c ^= 1;
    }
    rig.publish(r);
    let reader = rig.reader();
    assert_t7_on(&reader, &rig, &super::ranges(rig.now()));
    assert!(reader.tree_counts().damaged > 0);
}
