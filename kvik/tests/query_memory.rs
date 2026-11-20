//! A reader's queries allocate within its memory cap (`ReaderConfig::memory`, Q-1063): the peak of the heap while
//! `facts` streams 104,000 text facts from four maximal batches whose event times interleave, and while `at` reads,
//! measured above the heap before the call. Alone in its binary, so that the allocator counts this test only.

#![forbid(unsafe_code)]

mod common;

use common::block_on;
use common::durable::World;
use kvik::{Kind, Reader, ReaderConfig, Reading, SeriesSpec, Timestamp, Value, View};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const BATCHES: i64 = 4;
const PER_BATCH: i64 = 26_000;
/// What a query allocates outside the cap's accounting: the stream, its plan and cut, a key's fold, the error and
/// iterator state.
const OVERHEAD: usize = 1 << 20;

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
    let all = Timestamp(i64::MIN)..Timestamp(i64::MAX);
    let last = Timestamp(first + (PER_BATCH * BATCHES - 1) * 1_000_000);
    for cap in [32u64 << 20, 64 << 20] {
        let r = Reader::open(ReaderConfig::new(&w.dir).env(w.env()).memory(cap)).unwrap();
        r.facts(id, Timestamp(0)..Timestamp(1), View::History)
            .unwrap()
            .for_each(drop);
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
                peak <= cap as usize + OVERHEAD,
                "{view:?} at {} MiB: {peak} bytes",
                cap >> 20
            );
        }
        let mut got = None;
        let peak = peak_of(|| got = r.at(id, last, View::Current).unwrap());
        assert_eq!(got.map(|f| f.t), Some(last));
        eprintln!("at at {} MiB: peak {peak} bytes", cap >> 20);
        assert!(peak <= cap as usize + OVERHEAD, "at: {peak} bytes");
    }
}
