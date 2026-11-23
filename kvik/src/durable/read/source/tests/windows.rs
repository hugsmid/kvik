//! Windows, chart buckets and calendar results streamed from the sources (roadmap 9.0h) against the fact index's
//! (the evaluation before 9.0h) and the model's, on generated histories with revisions, forecasts and their issue
//! times, late readings, gaps, restarts, power losses and damaged batches, across a change to summer time.

use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::{reader_with, tiny};
use crate::ValueType;
use crate::config::LogConfig;
use crate::durable::read::cache::{LEAST_MEMORY, Sizes};
use crate::durable::read::latest::tests::{Rig, S, Step, quality, value};
use crate::durable::reader::DurableReader;
use crate::durable::tests::block_on;
use crate::durable::windows::{ChunkOverride, indexed};
use crate::error::QueryError;
use crate::read::{
    Aggregate, CalendarPeriod, CalendarWindow, ChartData, Resolution, Window, WindowResult,
};
use crate::series::{ChangedAtKind, Kind, SeriesSpec, Timing};
use crate::time::{LocalDate, Span, TimeRange, Timestamp, ZoneName};
use crate::types::{Fact, Intent, Position, Reading, Value};

/// From the rig's start (1 January 2026) to 00:55Z on 29 March 2026, five minutes before London's clocks go forward.
const TO_SUMMER_TIME: u64 = 87 * 86_400 + 55 * 60;
/// Series the rig defines (`latest::tests::specs`) and the two added here: a counter and a forecast.
const SERIES: usize = 9;
const COUNTER: usize = 7;
const FORECAST: usize = 8;

fn extra_specs() -> [SeriesSpec; 2] {
    let f64 = || ValueType::F64;
    [
        SeriesSpec::new("h/c/energy", Kind::Counter { value: f64() }, "Wh"),
        SeriesSpec::new("h/f/price", Kind::Gauge { value: f64() }, "c")
            .timing(Timing::Forecast)
            .versioned(ChangedAtKind::Time),
    ]
}

#[derive(Clone, Debug)]
enum Act {
    /// Readings (series, seconds from now, value code, quality code, `changed_at`), with revise intent or not.
    Emit(Vec<(usize, i64, u8, u8, u64)>, bool),
    Rig(Step),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let reading = (
        0usize..SERIES,
        prop_oneof![3 => prop::sample::select(vec![-60i64, -30, -4, 0, 4, 30, 90]), 2 => -300i64..120],
        any::<u8>(),
        any::<u8>(),
        0u64..3,
    );
    let act = prop_oneof![
        8 => (prop::collection::vec(reading, 1..8), prop::bool::weighted(0.2))
            .prop_map(|(r, revise)| Act::Emit(r, revise)),
        3 => (0u64..240).prop_map(|s| Act::Rig(Step::Advance(s))),
        1 => Just(Act::Rig(Step::Restart)),
        1 => Just(Act::Rig(Step::PowerLoss)),
        2 => (any::<u16>(), any::<bool>()).prop_map(|(k, unmet)| Act::Rig(Step::Damage(k, unmet))),
    ];
    prop::collection::vec(act, 1..16)
}

fn emit(rig: &mut Rig, rs: &[(usize, i64, u8, u8, u64)], revise: bool) {
    let now = rig.clock.wall().0;
    let readings: Vec<Reading> = rs
        .iter()
        .map(|&(i, secs, v, q, c)| {
            let mut t = now.saturating_add(secs * S);
            if i == 4 {
                t -= t.rem_euclid(4 * S);
            }
            let mut r =
                Reading::new(rig.ids[i], Timestamp(t), value(i, v)).with_quality(quality(q));
            if i == 3 || i == FORECAST {
                r.changed_at = Some(c);
            }
            r
        })
        .collect();
    let intent = if revise {
        Intent::Revise
    } else {
        Intent::Normal
    };
    rig.emit_readings(&readings, intent);
}

/// The windows asked of every series: rolling, ahead of the clock, absolute, and calendar days and months in the
/// site zone across the change to summer time and in named zones.
fn windows(now: Timestamp) -> Vec<Window> {
    let secs = Duration::from_secs;
    let today = CalendarPeriod::Day(LocalDate::new(2026, 3, 29));
    vec![
        Window::rolling(secs(60)),
        Window::rolling(secs(600)),
        Window::between(Span::from_secs(-400), Span::from_secs(-100)),
        Window::between(Span::from_secs(-30), Span::from_secs(300)),
        Window::absolute(now + Span::from_secs(-3_600)..now + Span::from_secs(600)),
        Window::absolute(now + Span::from_secs(10)..now + Span::from_secs(-10)),
        Window::calendar(CalendarWindow::SinceMidnight),
        Window::calendar(CalendarWindow::Period(today)),
        Window::calendar(CalendarWindow::Day(-1)),
        Window::calendar(CalendarWindow::ThisMonth),
        Window::Calendar {
            window: CalendarWindow::Period(today),
            zone: Some(ZoneName::new("Asia/Kolkata")),
        },
        Window::Calendar {
            window: CalendarWindow::Day(0),
            zone: Some(ZoneName::new("America/New_York")),
        },
    ]
}

fn aggregates(i: usize) -> Vec<Aggregate> {
    let mut a = vec![
        Aggregate::Avg,
        Aggregate::Sum,
        Aggregate::Integral,
        Aggregate::Max,
        Aggregate::Count,
        Aggregate::Increase,
        Aggregate::Rate,
        Aggregate::Last,
    ];
    match i {
        5 => a.push(Aggregate::TimeInState(Value::State(0))),
        6 => a.push(Aggregate::TimeInState(Value::Bool(true))),
        _ => {}
    }
    a
}

fn resolutions() -> [Resolution; 5] {
    [
        Resolution::Auto { max_points: 3 },
        Resolution::Auto { max_points: 2_000 },
        Resolution::Step(Duration::from_secs(2)),
        Resolution::Step(Duration::from_secs(7)),
        Resolution::Step(Duration::from_secs(60)),
    ]
}

/// Facts with what differs between the backends by design set aside: positions, receive times and `late`.
fn clean(mut f: Fact) -> Fact {
    f.position = Position::ORIGIN;
    f.received_at = Timestamp(0);
    f.flags.late = false;
    f
}

fn shown(r: Result<WindowResult, QueryError>) -> String {
    format!("{r:?}")
}

fn shown_chart(r: Result<ChartData, QueryError>, model: bool) -> String {
    match r {
        Ok(ChartData::Facts(fs)) if model => {
            format!("{:?}", fs.into_iter().map(clean).collect::<Vec<_>>())
        }
        other => format!("{other:?}"),
    }
}

/// Every query of every series from `got` equals the fact index's, and the model's while no batch is damaged; a
/// tiny cap may refuse only with `NotAvailable`. The many-series calls over every series at once, in another order and
/// with one repeated, equal each series' own result, or fail as the first failing series in id order does (Q-1131).
/// Returns the queries compared.
fn check(rig: &Rig, small: u64) -> Result<u64, TestCaseError> {
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let readers = [
        reader_with(&rig.env, &rig.dir, tiny(small)),
        reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY)),
    ];
    let model = rig.model.reader();
    let now = rig.clock.wall();
    let range: TimeRange = (now + Span::from_secs(-400)..now + Span::from_secs(200)).into();
    let mut n = 0;
    let compare = |k: usize, want: &str, got: String| -> Result<(), TestCaseError> {
        if k == 0 && got.contains("NotAvailable") {
            return Ok(());
        }
        prop_assert_eq!(want, got.as_str(), "reader {}", k);
        Ok(())
    };
    let stream = |r: &std::sync::Arc<DurableReader>, id, w: &Window, a: &Aggregate| {
        shown(crate::durable::window(r, id, w, a))
    };
    let mut windows_got = std::collections::HashMap::new();
    let mut charts_got = std::collections::HashMap::new();
    for (i, id) in rig.ids.iter().enumerate() {
        for (wi, w) in windows(now).into_iter().enumerate() {
            for (gi, a) in aggregates(i).into_iter().enumerate() {
                let want = shown(indexed::window(&oracle, *id, &w, &a));
                for (k, r) in readers.iter().enumerate() {
                    let got = stream(r, *id, &w, &a);
                    compare(k, &want, got.clone())?;
                    windows_got.insert((k, wi, gi, i), got);
                }
                if !rig.faulted {
                    let m = shown(model.window(*id, w.clone(), a.clone()));
                    prop_assert_eq!(&want, &m, "{} {:?} {:?}", i, w, a);
                }
                n += 1;
            }
        }
        for (ri, res) in resolutions().into_iter().enumerate() {
            let want = shown_chart(indexed::buckets(&oracle, *id, range, res), false);
            for (k, r) in readers.iter().enumerate() {
                let got = shown_chart(crate::durable::buckets(r, *id, range, res), false);
                compare(k, &want, got.clone())?;
                charts_got.insert((k, ri, i), got);
            }
            if !rig.faulted {
                let want = shown_chart(indexed::buckets(&oracle, *id, range, res), true);
                let m = shown_chart(model.buckets(*id, range, res), true);
                prop_assert_eq!(&want, &m, "{} {:?}", i, res);
            }
            n += 1;
        }
    }
    let mut order: Vec<usize> = (0..rig.ids.len()).rev().collect();
    order.push(order.len() / 2);
    let many: Vec<_> = order.iter().map(|&i| rig.ids[i]).collect();
    let mut by_id: Vec<usize> = (0..rig.ids.len()).collect();
    by_id.sort_by_key(|&i| rig.ids[i]);
    for (k, r) in readers.iter().enumerate() {
        for (wi, w) in windows(now).into_iter().enumerate() {
            for (gi, a) in aggregates(0).into_iter().enumerate() {
                let each = |i: usize| windows_got[&(k, wi, gi, i)].clone();
                let got = crate::durable::window_of(r, &many, &w, &a)
                    .map(|v| v.into_iter().map(|x| shown(Ok(x))).collect::<Vec<_>>());
                many_equal(k, &order, &by_id, each, got)?;
                n += 1;
            }
        }
        for (ri, res) in resolutions().into_iter().enumerate() {
            let each = |i: usize| charts_got[&(k, ri, i)].clone();
            let got = crate::durable::buckets_of(r, &many, range, res).map(|v| {
                v.into_iter()
                    .map(|x| shown_chart(Ok(x), false))
                    .collect::<Vec<_>>()
            });
            many_equal(k, &order, &by_id, each, got)?;
            n += 1;
        }
    }
    Ok(n)
}

/// A many-series result, shown per series, against each series' own (`each`): equal at every place of `order`, or the
/// error of the first series in id order (`by_id`) whose own result is an error. Reader 0 may refuse with
/// `NotAvailable` where the series' own results do not.
fn many_equal(
    k: usize,
    order: &[usize],
    by_id: &[usize],
    each: impl Fn(usize) -> String,
    got: Result<Vec<String>, QueryError>,
) -> Result<(), TestCaseError> {
    let failed = by_id
        .iter()
        .map(|&i| each(i))
        .find(|s| s.starts_with("Err"));
    if k == 0 && failed.as_ref().is_some_and(|f| f.contains("NotAvailable")) {
        return Ok(());
    }
    match got {
        Ok(v) => {
            prop_assert!(
                failed.is_none(),
                "reader {}: many succeeded where {:?}",
                k,
                failed
            );
            prop_assert_eq!(v.len(), order.len());
            for (s, &i) in v.iter().zip(order) {
                prop_assert_eq!(s, &each(i), "reader {} series {}", k, i);
            }
        }
        Err(e) => {
            let e = shown(Err(e));
            if k == 0 && e.contains("NotAvailable") {
                return Ok(());
            }
            prop_assert_eq!(Some(e), failed, "reader {}", k);
        }
    }
    Ok(())
}

#[test]
fn windows_buckets_and_calendar_results_equal_the_fact_index_and_the_model_at_every_cap() {
    // Each case runs some thousands of queries on four readers and the model: a thirty-second of the configured
    // cases, as for the facts' differential.
    let mut runner = TestRunner::new(ProptestConfig {
        cases: (ProptestConfig::default().cases / 32).max(8),
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let log = LogConfig {
        seal_interval: Duration::from_millis(20),
        ..LogConfig::default()
    };
    let compared = std::cell::Cell::new(0);
    runner
        .run(
            &(
                acts(),
                prop::sample::select(vec![6u64, 24]),
                prop::sample::select(vec![1usize, 2, 3]),
            ),
            |(history, small, chunk)| {
                let _chunks = ChunkOverride::set(chunk);
                let mut rig = Rig::with_log("windows", log.clone());
                for spec in extra_specs() {
                    let store = rig.store.as_ref().expect("an open store");
                    let id = block_on(store.define_series(spec.clone())).unwrap();
                    assert_eq!(block_on(rig.model.define_series(spec)).unwrap(), id);
                    rig.ids.push(id);
                }
                assert_eq!(rig.ids.len(), SERIES);
                assert_eq!(rig.ids[COUNTER].0 + 1, rig.ids[FORECAST].0);
                rig.clock.advance(Duration::from_secs(TO_SUMMER_TIME));
                for (k, a) in history.iter().enumerate() {
                    match a {
                        Act::Emit(rs, revise) => emit(&mut rig, rs, *revise),
                        Act::Rig(s) => rig.step(s)?,
                    }
                    if k == history.len() / 2 {
                        compared.set(compared.get() + check(&rig, small)?);
                    }
                }
                compared.set(compared.get() + check(&rig, small)?);
                rig.shut();
                Ok(())
            },
        )
        .unwrap();
    assert!(compared.get() > 0);
}
