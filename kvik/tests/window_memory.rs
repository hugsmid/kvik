//! Chart buckets, windows and calendar results stay flat with the store's age (roadmap 9.0h): a fresh reader's peak
//! heap and time for each over the last hour or day of a store of 1, 7 and 30 days of 37 series, event time
//! accelerated. One reading per series every `KVIK_WINDOW_MEMORY_STEP` seconds (300 by default; 1 is the home rate,
//! `docs/measurements/9.0h-bucket-reads.md`). Alone in its binary, so that the allocator counts this test only.

#![forbid(unsafe_code)]

mod common;

use std::time::{Duration, Instant};

use common::block_on;
use common::durable::World;
use kvik::{
    Aggregate, CalendarPeriod, ChartData, Kind, LocalDate, Reader, ReaderConfig, Reading,
    Resolution, SeriesId, SeriesSpec, Span, Timestamp, Value, ValueType, Window,
};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const SERIES: usize = 37;
const DAY: i64 = 86_400;
/// The reader's memory cap, the default (`ReaderConfig::memory`).
const CAP: usize = 64 << 20;
/// What a read holds outside the cap (`docs/api.md` Reader): its own state, about 1 MiB, and the lookups' cache
/// filling within the cap's share is counted in the cap.
const OVERHEAD: usize = 1 << 20;

fn step() -> i64 {
    std::env::var("KVIK_WINDOW_MEMORY_STEP")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300)
}

/// The most the heap held above where it stood before `f`, and the time `f` took.
fn measure(f: impl FnOnce()) -> (usize, Duration) {
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    let t = Instant::now();
    f();
    (PEAK.peak_usage().saturating_sub(base), t.elapsed())
}

/// Readings from `from` to `to` seconds after the start, a round of every series each `step` seconds; the clock
/// keeps up with event time.
fn write(w: &World, ids: &[SeriesId], from: i64, to: i64, step: i64) {
    let s = w.open();
    let base = common::start();
    let rounds_per_call = (2_000 / ids.len() as i64).max(1);
    let mut k = from;
    while k < to {
        let mut readings = Vec::new();
        for _ in 0..rounds_per_call {
            if k >= to {
                break;
            }
            for (i, id) in ids.iter().enumerate() {
                let v = if i % 2 == 0 {
                    (k % 3_600) as f64
                } else {
                    k as f64
                };
                readings.push(Reading::new(*id, base + Span::from_secs(k), Value::F64(v)));
            }
            k += step;
        }
        let last = readings.last().map(|r| r.t).unwrap();
        if w.clock.wall() < last {
            w.clock
                .advance(Duration::from_nanos((last.0 - w.clock.wall().0) as u64));
        }
        block_on(s.emit(&readings)).unwrap();
    }
    block_on(s.shutdown()).unwrap();
}

/// What reads on a fresh reader took, after its first refresh: the first bucket read's peak and time (it fills the
/// lookups' cache, Q-1069), then each read's again, and the heap the reader holds after them.
struct Reads {
    first: (usize, Duration),
    each: [(usize, Duration); 3],
    held: usize,
}

fn reads(w: &World, id: SeriesId, end: Timestamp, date: LocalDate) -> Reads {
    let base = PEAK.current_usage();
    let r = Reader::open(ReaderConfig::new(&w.dir).env(w.env()).memory(CAP as u64)).unwrap();
    r.live(id).unwrap();
    let hour = end + Span::from_secs(-3_600)..end;
    let buckets = || {
        r.buckets(id, hour.clone(), Resolution::Step(Duration::from_secs(60)))
            .unwrap();
    };
    let first = measure(buckets);
    let each = [
        measure(buckets),
        measure(|| {
            r.window(
                id,
                Window::rolling(Duration::from_secs(3_600)),
                Aggregate::Avg,
            )
            .unwrap();
        }),
        measure(|| {
            r.calendar(id, CalendarPeriod::Day(date), Aggregate::Avg)
                .unwrap();
        }),
    ];
    let held = PEAK.current_usage().saturating_sub(base);
    Reads { first, each, held }
}

/// A fresh reader's chart query of every series over the last day at once (Q-1110), after a first read that fills the
/// lookups' cache: its peak, its time and the bytes of its results.
fn many(w: &World, ids: &[SeriesId], end: Timestamp) -> (usize, Duration, usize) {
    let r = Reader::open(ReaderConfig::new(&w.dir).env(w.env()).memory(CAP as u64)).unwrap();
    let day = end + Span::from_secs(-DAY)..end;
    let auto = Resolution::Auto {
        max_points: MAX_POINTS,
    };
    r.buckets_of(ids, day.clone(), auto).unwrap();
    let mut out = 0;
    let (peak, took) = measure(|| {
        let charts = r.buckets_of(ids, day, auto).unwrap();
        out = charts
            .iter()
            .map(|c| match c {
                ChartData::Facts(f) => f.capacity() * std::mem::size_of::<kvik::FactRow>(),
                ChartData::Buckets { buckets, .. } => {
                    buckets.capacity() * std::mem::size_of::<kvik::Bucket>()
                }
                _ => 0,
            })
            .sum();
    });
    (peak, took, out)
}

/// The most points of the many-series chart query.
const MAX_POINTS: usize = 1_500;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

#[test]
fn bucket_window_and_calendar_reads_stay_flat_with_the_stores_age() {
    let step = step();
    let w = World::new("window-memory");
    let s = w.open();
    let ids: Vec<SeriesId> = (0..SERIES)
        .map(|k| {
            let kind = if k % 2 == 0 {
                Kind::Gauge {
                    value: ValueType::F64,
                }
            } else {
                Kind::Counter {
                    value: ValueType::F64,
                }
            };
            block_on(s.define_series(SeriesSpec::new(format!("site/r{k}/m"), kind, ""))).unwrap()
        })
        .collect();
    block_on(s.shutdown()).unwrap();
    let mut written = 0;
    let mut warm = Vec::new();
    // The local day each store ends in: it starts at 12:00Z on 5 January 2026, in London winter time.
    for (days, date) in [(1, (2026, 1, 6)), (7, (2026, 1, 12)), (30, (2026, 2, 4))] {
        let date = LocalDate::new(date.0, date.1, date.2);
        write(&w, &ids, written, days * DAY, step);
        written = days * DAY;
        let end = common::start() + Span::from_secs(written);
        let t = Instant::now();
        let got = [reads(&w, ids[0], end, date), reads(&w, ids[1], end, date)];
        eprintln!(
            "{days} days, a reading every {step} s, {:.0} s to read:",
            t.elapsed().as_secs_f64()
        );
        for (k, r) in got.iter().enumerate() {
            let [(b, bt), (wi, wt), (c, ct)] = r.each;
            eprintln!(
                "  {}: first buckets {} B {:.1} ms; buckets {b} B {:.1} ms; window {wi} B {:.1} ms; \
                 calendar {c} B {:.1} ms; held {} B",
                ["gauge", "counter"][k],
                r.first.0,
                ms(r.first.1),
                ms(bt),
                ms(wt),
                ms(ct),
                r.held
            );
            assert!(r.first.0 <= CAP + OVERHEAD, "first read at {days} days");
            warm.push((days, k, r.each.map(|e| e.0)));
        }
        // Each series holds a chunk, its results and, with an automatic resolution, its raw facts until they pass
        // the most points.
        let (peak, took, out) = many(&w, &ids, end);
        let heads = SERIES * (MAX_POINTS + 1) * std::mem::size_of::<kvik::FactRow>();
        eprintln!(
            "  every series' day: {peak} B {:.1} ms; results {out} B",
            ms(took)
        );
        assert!(
            peak <= CAP + OVERHEAD + heads + out,
            "{peak} bytes for every series at {days} days"
        );
    }
    for (days, k, peaks) in &warm {
        let first = warm.iter().find(|w| w.0 == 1 && w.1 == *k).unwrap().2;
        for (peak, one) in peaks.iter().zip(first) {
            // Flat: the 30-day store's fact index alone would be over 30 times the 1-day store's.
            assert!(*peak <= CAP + OVERHEAD, "{peak} bytes at {days} days");
            assert!(
                *peak <= one * 2 + OVERHEAD,
                "{peak} bytes at {days} days, {one} at 1 day"
            );
        }
    }
}
