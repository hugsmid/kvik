//! A reader process's memory does not grow with the facts it folds (roadmap 9.0c): the heap a reader holds after
//! catching up over seven times the facts it had; a reader that also answers windows holds no more, since windows
//! read the sources rather than build the fact index (roadmap 9.0h, Q-1109). Alone in its binary, so that the
//! allocator counts this test only, and with the store shut down while it counts.

#![forbid(unsafe_code)]

mod common;

use std::alloc::System;

use common::block_on;
use common::durable::{World, gauge, num};
use kvik::{Aggregate, Reader, SeriesId, Window};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const SERIES: usize = 400;

/// Rounds `from..to` of one reading per series, a second apart.
fn write(w: &World, ids: &[SeriesId], from: usize, to: usize) {
    let s = w.open();
    for k in from..to {
        let readings: Vec<_> = ids
            .iter()
            .map(|id| num(*id, k as i64 - 1_000_000, k as f64))
            .collect();
        block_on(s.emit(&readings)).unwrap();
    }
    block_on(s.shutdown()).unwrap();
}

/// The heap allocated and not freed while `f` runs (a reallocation's change is in both counts already).
fn held(f: impl FnOnce()) -> isize {
    let region = Region::new(GLOBAL);
    f();
    let c = region.change();
    c.bytes_allocated as isize - c.bytes_deallocated as isize
}

fn latest(r: &Reader, ids: &[SeriesId]) {
    let snap = r.snapshot().unwrap();
    for id in ids {
        snap.live(*id).unwrap();
        snap.series(*id).unwrap();
    }
}

#[test]
fn a_readers_heap_does_not_grow_with_the_facts_it_folds() {
    let w = World::new("latest-memory");
    let s = w.open();
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|k| {
            block_on(s.define_series(gauge(&format!("site/d{}/m{}", k / 20, k % 20)))).unwrap()
        })
        .collect();
    block_on(s.shutdown()).unwrap();
    let rounds = 100;
    write(&w, &ids, 0, rounds);
    let bounded = w.reader();
    let windowed = w.reader();
    latest(&bounded, &ids);
    latest(&windowed, &ids);
    windowed
        .window(
            ids[0],
            Window::rolling(std::time::Duration::from_secs(60)),
            Aggregate::Avg,
        )
        .unwrap();
    write(&w, &ids, rounds, 8 * rounds);
    // 700 rounds of 400 readings: 280,000 facts more.
    let grown = held(|| {
        bounded.refresh().unwrap();
        latest(&bounded, &ids);
    });
    let windows = held(|| {
        windowed
            .window(
                ids[0],
                Window::rolling(std::time::Duration::from_secs(60)),
                Aggregate::Avg,
            )
            .unwrap();
    });
    // At most the bytes of the one log segment it follows (16 MiB here) and a part of a lookup.
    assert!(grown < 20 << 20, "{grown} bytes");
    // A fact index over the 280,000 facts would hold about 40 MiB.
    assert!(
        windows < grown + (4 << 20),
        "a reader that answers windows: {windows} bytes, the reader {grown}"
    );
}
