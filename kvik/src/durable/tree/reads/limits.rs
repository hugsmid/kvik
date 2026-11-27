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
