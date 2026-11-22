//! A reader's queries allocate within its memory cap (`ReaderConfig::memory`, Q-1063): the peak of the heap while
//! `facts` streams 104,000 text facts from four maximal batches whose event times interleave, and while `at` reads.
//! Measured above the heap before each call against what one pass alone is granted; the first call also runs the
//! reader's first refresh, which keeps the bytes of the segment it follows (Q-1069). Alone in its binary, so that the
//! allocator counts this test only.

#![forbid(unsafe_code)]

mod common;

use std::path::Path;

use common::block_on;
use common::durable::World;
use kvik::{Kind, Reader, ReaderConfig, Reading, SeriesSpec, Timestamp, Value, View};
use kvik_log::directory::Family;
use kvik_log::fs::Fs as _;
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const BATCHES: i64 = 4;
const PER_BATCH: i64 = 26_000;
/// What a query allocates outside the cap's accounting: the stream, its plan and cut, a key's fold, the error and
/// iterator state.
const OVERHEAD: usize = 1 << 20;
/// What the lookups' log reader keeps at most (`kvik_log::reader::LOOKUP_CACHE_BYTES`), in the cache's share.
const LOOKUPS: usize = 8 << 20;
/// Batches one refresh part holds: a part ends at the batch that brings it to `durable::start::PART` items (Q-1107).
const PART_BATCHES: usize = (1usize << 16).div_ceil(PER_BATCH as usize);
/// A batch's rows as a refresh part holds them: each text's 255 bytes and its 64-byte item.
const ROWS: usize = PER_BATCH as usize * (255 + 64);
/// What the first refresh takes above the segment bytes it keeps and one part's rows: one batch's scan and the latest
/// values' fold, measured at 2.0 MiB.
const SCAN: usize = 3 << 20;
/// What the first refresh takes above the segment bytes it keeps; over before the first pass is admitted (Q-1069).
const REFRESH: usize = SCAN + PART_BATCHES * ROWS;

/// The grant one pass takes alone at `cap` (Q-1063): the cap less the cache's share, all of it under two least
/// grants of 17 MiB, else half.
fn lone_pass(cap: usize) -> usize {
    let free = cap - (cap / 4).max(LOOKUPS);
    if free < 2 * (17 << 20) {
        free
    } else {
        free / 2
    }
}

/// The most the heap held above where it stood before `f`.
fn peak_of(f: impl FnOnce()) -> usize {
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    f();
    PEAK.peak_usage().saturating_sub(base)
}

#[test]
fn facts_and_at_allocate_within_the_memory_cap() {
    let w = World::new("query-memory");
    let mut config = w.config();
    config.log.max_batch_bytes = 8 << 20;
    let s = block_on(kvik::Store::open(config)).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let first = common::start().0 - 1_000_000_000_000;
    for b in 0..BATCHES {
        let readings: Vec<Reading> = (0..PER_BATCH)
            .map(|k| {
                Reading::new(
                    id,
                    Timestamp(first + (k * BATCHES + b) * 1_000_000),
                    Value::Text(format!("{b}{k:0>254}")),
                )
            })
            .collect();
        block_on(s.emit(&readings)).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    // No series directory: the first refresh folds the log from its start, a whole part, whatever the directory
    // written at the seal had reached (Q-1107).
    for (family, _, p) in kvik_log::directory::list(&*w.fs, Path::new(&w.dir)).unwrap() {
        if family == Family::Series {
            w.fs.remove_file(&p).unwrap();
        }
    }
    // The log reader keeps the bytes of the last segment it read between refreshes, so that a refresh reads only
    // what is new: here the sealed segment holding the four batches; the open ones after it are preallocated.
    let followed = w
        .segments()
        .iter()
        .filter(|(p, _)| p.extension().is_some_and(|e| e == "kvl"))
        .map(|(_, b)| b.len())
        .max()
        .unwrap();
    let all = Timestamp(i64::MIN)..Timestamp(i64::MAX);
    let last = Timestamp(first + (PER_BATCH * BATCHES - 1) * 1_000_000);
    for cap in [32u64 << 20, 64 << 20] {
        let grant = lone_pass(cap as usize);
        let alone = Reader::open(ReaderConfig::new(&w.dir).env(w.env()).memory(cap)).unwrap();
        let refresh = peak_of(|| drop(alone.live(id).unwrap()));
        eprintln!(
            "first refresh at {} MiB: peak {refresh} bytes, followed {followed}",
            cap >> 20
        );
        assert!(
            refresh <= followed + REFRESH,
            "first refresh at {} MiB: {refresh} bytes, the followed segment {followed}",
            cap >> 20
        );
        drop(alone);
        let r = Reader::open(ReaderConfig::new(&w.dir).env(w.env()).memory(cap)).unwrap();
        let first = peak_of(|| {
            let n = r
                .facts(id, all.clone(), View::History)
                .unwrap()
                .map(Result::unwrap)
                .count();
            assert_eq!(n, (BATCHES * PER_BATCH) as usize);
        });
        eprintln!("first facts at {} MiB: peak {first} bytes", cap >> 20);
        // The lookups' cache fills in this call, within the cap's share: two segments' maps, a few KiB here.
        assert!(
            first <= followed + (grant + OVERHEAD).max(REFRESH),
            "first facts at {} MiB: {first} bytes, the followed segment {followed}",
            cap >> 20
        );
        for view in [View::History, View::Current] {
            let mut n = 0;
            let peak = peak_of(|| {
                for f in r.facts(id, all.clone(), view).unwrap() {
                    f.unwrap();
                    n += 1;
                }
            });
            assert_eq!(n, (BATCHES * PER_BATCH) as usize);
            eprintln!("facts {view:?} at {} MiB: peak {peak} bytes", cap >> 20);
            assert!(
                peak <= grant + OVERHEAD,
                "{view:?} at {} MiB: {peak} bytes",
                cap >> 20
            );
        }
        let mut got = None;
        let peak = peak_of(|| got = r.at(id, last, View::Current).unwrap());
        assert_eq!(got.map(|f| f.t), Some(last));
        eprintln!("at at {} MiB: peak {peak} bytes", cap >> 20);
        assert!(peak <= grant + OVERHEAD, "at: {peak} bytes");
    }
}
