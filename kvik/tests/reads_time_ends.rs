//! Probes of the full review of reads, query and export: calls a dashboard can make with an open-ended range.
//! Each test passes when the call returns (a result or an error) and fails when it panics.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use common::durable::{gauge, num};
use common::{BACKENDS, at, block_on, config_on};
use kvik::env::ManualClock;
use kvik::{
    Aggregate, Backend, IntervalLength, Kind, Resolution, SeriesId, SeriesSpec, Store, Timestamp,
    ValueType, View, Window,
};

const MIN: Timestamp = Timestamp(i64::MIN);
const MAX: Timestamp = Timestamp(i64::MAX);

fn store(backend: Backend, name: &str) -> (Store, SeriesId, SeriesId) {
    let clock = ManualClock::new(common::start());
    let s = block_on(Store::open(config_on(backend, name, &clock))).unwrap();
    let g = block_on(s.define_series(gauge("p/g/power"))).unwrap();
    let i = block_on(s.define_series(SeriesSpec::new(
        "p/i/energy",
        Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Fixed {
                d: Duration::from_secs(4),
                phase: Duration::from_secs(1),
            },
        },
        "kWh",
    )))
    .unwrap();
    block_on(s.emit(&[num(g, -30, 1.0), num(g, -20, 2.0), num(g, -10, 3.0)])).unwrap();
    (s, g, i)
}

/// Runs `call` on each backend; the names of the backends where it panicked.
fn panics<R: std::fmt::Debug>(
    name: &str,
    call: impl Fn(&Store, SeriesId, SeriesId) -> R,
) -> Vec<String> {
    let mut out = Vec::new();
    for b in BACKENDS {
        let (s, g, i) = store(b, &format!("{name}-{b:?}"));
        match catch_unwind(AssertUnwindSafe(|| call(&s, g, i))) {
            Ok(r) => eprintln!("{name} on {b:?}: {r:?}"),
            Err(_) => out.push(format!("{b:?}")),
        }
    }
    out
}

fn none(v: Vec<String>) {
    assert!(v.is_empty(), "panicked on {v:?}");
}

#[test]
fn auto_buckets_up_to_the_last_instant_return() {
    none(panics("auto-to-max", |s, g, _| {
        s.reader()
            .buckets(g, at(-60)..MAX, Resolution::Auto { max_points: 2 })
            .map(|_| ())
    }));
}

#[test]
fn auto_buckets_up_to_the_last_instant_with_few_facts_return() {
    none(panics("auto-to-max-raw", |s, g, _| {
        s.reader()
            .buckets(g, at(-60)..MAX, Resolution::Auto { max_points: 2_000 })
            .map(|_| ())
    }));
}

#[test]
fn auto_buckets_from_the_first_instant_return() {
    none(panics("auto-from-min", |s, g, _| {
        s.reader()
            .buckets(g, MIN..at(0), Resolution::Auto { max_points: 2 })
            .map(|_| ())
    }));
}

#[test]
fn auto_buckets_over_every_instant_return() {
    none(panics("auto-all", |s, g, _| {
        s.reader()
            .buckets(g, MIN..MAX, Resolution::Auto { max_points: 2 })
            .map(|_| ())
    }));
}

#[test]
fn step_buckets_over_every_instant_return() {
    none(panics("step-all", |s, g, _| {
        s.reader()
            .buckets(g, MIN..MAX, Resolution::Step(Duration::from_secs(3_600)))
            .map(|_| ())
    }));
}

#[test]
fn step_buckets_ending_at_the_last_instant_return() {
    none(panics("step-to-max", |s, g, _| {
        // 300 years in one bucket of 400: far below the bucket limit.
        s.reader()
            .buckets(
                g,
                at(-60)..MAX,
                Resolution::Step(Duration::from_secs(400 * 365 * 86_400)),
            )
            .map(|_| ())
    }));
}

#[test]
fn a_window_over_every_instant_returns() {
    none(panics("window-all", |s, g, i| {
        let r = s.reader();
        (
            r.window(g, Window::absolute(MIN..MAX), Aggregate::Avg).map(|_| ()),
            r.window(i, Window::absolute(MIN..MAX), Aggregate::Sum).map(|_| ()),
            r.window_of(&[g, i], Window::absolute(MIN..at(0)), Aggregate::Sum).map(|_| ()),
        )
    }));
}

#[test]
fn at_and_interval_at_the_extremes_return() {
    none(panics("at-extremes", |s, g, i| {
        let r = s.reader();
        (
            r.at(g, MIN, View::Current).map(|_| ()),
            r.at(g, MAX, View::History).map(|_| ()),
            r.interval(i, MAX).map(|_| ()),
        )
    }));
}

#[test]
fn interval_at_the_first_instant_returns() {
    none(panics("interval-min", |s, _, i| s.reader().interval(i, MIN)));
}

#[test]
fn facts_over_every_instant_return() {
    none(panics("facts-all", |s, g, i| {
        let r = s.reader();
        r.facts_of(&[g, i], MIN..MAX, View::History)
            .map(|f| f.count())
    }));
}

/// An open-ended chart of a series with more facts than points: the usual dashboard call.
#[test]
fn auto_buckets_of_many_facts_up_to_the_last_instant_return() {
    none(panics("auto-to-max-many", |s, g, _| {
        let more: Vec<_> = (0..200).map(|k| num(g, 100 + k, k as f64)).collect();
        block_on(s.emit(&more)).unwrap();
        s.reader()
            .buckets(g, at(-60)..MAX, Resolution::Auto { max_points: 100 })
            .map(|_| ())
    }));
}
