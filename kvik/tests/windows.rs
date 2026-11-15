//! Live state, windows, chart buckets and calendar results of the durable backend against the model backend over
//! generated histories (roadmap 5.4), in the writer's process and in a reader process.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{at, block_on, start};
use kvik::env::{Env, ManualClock, MemFs};
use kvik::{
    Aggregate, Backend, CalendarPeriod, CalendarWindow, ChangedAtKind, ChartData, CounterRules,
    Intent, IntervalLength, Kind, Live, LocalDate, LogConfig, Position, Quality, Reader,
    ReaderConfig, Reading, Resolution, SeriesId, SeriesSpec, Span, StateValues, Store, StoreConfig,
    Timing, Value, ValueType, Window, ZoneName,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

fn number(kind: Kind) -> SeriesSpec {
    SeriesSpec::new("", kind, "")
}

fn gauge() -> Kind {
    Kind::Gauge {
        value: ValueType::F64,
    }
}

fn counter() -> Kind {
    Kind::Counter {
        value: ValueType::F64,
    }
}

fn interval(d: u64, phase: u64) -> Kind {
    Kind::Interval {
        value: ValueType::F64,
        length: IntervalLength::Fixed {
            d: Duration::from_secs(d),
            phase: Duration::from_secs(phase),
        },
    }
}

/// The series of every history: each kind and timing, with and without stored levels in the model.
fn specs() -> Vec<SeriesSpec> {
    let secs = Duration::from_secs;
    vec![
        number(gauge())
            .name("w/g/sparse")
            .expected_interval(secs(10))
            .staleness(secs(30)),
        number(gauge()).name("w/g/dense").expected_interval(secs(1)),
        number(counter()).name("w/c/plain").staleness(secs(30)),
        number(counter())
            .name("w/c/wraps")
            .counter(CounterRules::new(Some(100.0), 0.25)),
        number(Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into(), "auto".into()]),
        })
        .name("w/s/mode")
        .staleness(secs(60)),
        number(Kind::State {
            values: StateValues::Bool,
        })
        .name("w/s/door"),
        number(Kind::Text).name("w/t/note"),
        number(gauge())
            .name("w/p/price")
            .expected_interval(secs(900))
            .timing(Timing::Scheduled)
            .versioned(ChangedAtKind::Version),
        number(gauge())
            .name("w/f/forecast")
            .expected_interval(secs(900))
            .timing(Timing::Forecast)
            .versioned(ChangedAtKind::Time),
        number(interval(60, 0)).name("w/i/minute"),
        number(interval(7, 3)).name("w/i/odd"),
        number(interval(900, 0))
            .name("w/i/scheduled")
            .timing(Timing::Scheduled),
    ]
}

#[derive(Clone, Debug)]
enum Step {
    /// Readings, each (series index, seconds after the start, value code, quality code, `changed_at`), with revise
    /// intent or not.
    Emit(Vec<(usize, i64, u8, u8, u64)>, bool),
    Advance(u64),
    /// The wall clock steps back by this many seconds.
    WallBack(u64),
    SiteZone(usize, i64),
}

const ZONES: [&str; 3] = ["Europe/London", "Asia/Kolkata", "America/New_York"];

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let reading = (
        0usize..12,
        // Mostly the last two hours, some ahead of the clock (future facts) and some far behind (late facts).
        prop_oneof![
            8 => -7_200i64..120,
            1 => 120i64..7_200,
            1 => -90_000i64..-80_000,
        ],
        any::<u8>(),
        any::<u8>(),
        0u64..4,
    );
    let step = prop_oneof![
        8 => (prop::collection::vec(reading, 1..12), prop::bool::weighted(0.1))
            .prop_map(|(r, rev)| Step::Emit(r, rev)),
        3 => prop_oneof![1u64..30, 30u64..4_000].prop_map(Step::Advance),
        1 => (1u64..120).prop_map(Step::WallBack),
        1 => (0usize..ZONES.len(), -7_200i64..7_200).prop_map(|(z, t)| Step::SiteZone(z, t)),
    ];
    prop::collection::vec(step, 1..14)
}

/// A reading of series `i` from its codes: values that rise, fall back (counter resets and regressions), carry no
/// value or are bad, at a `t` on the series' phase.
fn reading(id: SeriesId, spec: &SeriesSpec, secs: i64, v: u8, q: u8, changed: u64) -> Reading {
    let mut t = secs;
    if let Kind::Interval {
        length: IntervalLength::Fixed { d, phase },
        ..
    } = &spec.kind
    {
        let (d, phase) = (d.as_secs() as i64, phase.as_secs() as i64);
        t -= (t - phase).rem_euclid(d);
    }
    let value = match (&spec.kind, v % 16) {
        (_, 15) => Value::Absent,
        (
            Kind::State {
                values: StateValues::Named(_),
            },
            _,
        ) => Value::State(u16::from(v % 3)),
        (Kind::State { .. }, _) => Value::Bool(v.is_multiple_of(2)),
        (Kind::Text, _) => Value::Text(format!("n{}", v % 4)),
        (Kind::Counter { .. }, 14) => Value::F64(-1.0),
        (_, k) => Value::F64(f64::from(k) * 7.5 + f64::from(v / 16) * 0.25),
    };
    let quality = [
        Quality::Good,
        Quality::Good,
        Quality::Estimated,
        Quality::Bad,
    ][usize::from(q % 4)];
    let mut r = Reading::new(id, at(t), value).with_quality(quality);
    if spec.versioned.is_some() {
        r = r.changed_at(changed);
    }
    r
}

/// Every query result, in a comparable form. Positions and `late` differ between the backends by design
/// (`tests/durable.rs`), and so does `received_at` after the wall clock steps back (the durable log keeps the
/// batch's commit time, which never runs backwards); they are cleared in facts.
fn queries(
    r: &Reader,
    ids: &[SeriesId],
    specs: &[SeriesSpec],
    observed_stale: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let clean = |mut f: kvik::Fact| {
        f.position = Position::ORIGIN;
        f.flags.late = false;
        f.received_at = kvik::Timestamp(0);
        f
    };
    let windows = [
        Window::rolling(Duration::from_secs(60)),
        Window::rolling(Duration::from_secs(3_600)),
        Window::between(Span::from_secs(-7_000), Span::from_secs(-3_599)),
        Window::between(Span::from_secs(-30), Span::from_secs(1_800)),
        Window::between(Span::from_secs(100), Span::from_secs(4_000)),
        Window::absolute(at(-3_617)..at(-1_000)),
        Window::absolute(at(-86_000)..at(0)),
        Window::calendar(CalendarWindow::SinceMidnight),
        Window::calendar(CalendarWindow::ThisMonth),
        Window::calendar(CalendarWindow::Day(1)),
        Window::calendar(CalendarWindow::Month(-1)),
        Window::calendar(CalendarWindow::Period(CalendarPeriod::Day(LocalDate::new(
            2026, 1, 5,
        )))),
        Window::Calendar {
            window: CalendarWindow::Period(CalendarPeriod::Day(LocalDate::new(2026, 1, 5))),
            zone: Some(ZoneName::new("Asia/Kolkata")),
        },
        Window::Calendar {
            window: CalendarWindow::Day(-1),
            zone: Some(ZoneName::new("America/New_York")),
        },
        Window::Calendar {
            window: CalendarWindow::Day(0),
            zone: Some(ZoneName::new("Not/AZone")),
        },
    ];
    for (id, spec) in ids.iter().zip(specs) {
        let mut aggs = vec![
            Aggregate::Avg,
            Aggregate::Sum,
            Aggregate::Integral,
            Aggregate::Min,
            Aggregate::Max,
            Aggregate::Count,
            Aggregate::Increase,
            Aggregate::Rate,
            Aggregate::Last,
        ];
        match &spec.kind {
            Kind::State {
                values: StateValues::Named(_),
            } => aggs.push(Aggregate::TimeInState(Value::State(1))),
            Kind::State { .. } => aggs.push(Aggregate::TimeInState(Value::Bool(true))),
            _ => {}
        }
        let live = match r.live(*id) {
            Ok(Live::Value(mut v)) => {
                v.fact = clean(v.fact);
                if !observed_stale && spec.timing == Timing::Observed {
                    v.stale = false;
                }
                format!("{v:?}")
            }
            other => format!("{other:?}"),
        };
        out.push(format!("{id} live {live}"));
        for w in &windows {
            for a in &aggs {
                out.push(format!(
                    "{id} {w:?} {a:?} {:?}",
                    r.window(*id, w.clone(), a.clone())
                ));
            }
        }
        for res in [
            Resolution::Auto { max_points: 4 },
            Resolution::Auto { max_points: 2_000 },
            Resolution::Step(Duration::from_secs(300)),
            Resolution::Step(Duration::from_secs(7)),
        ] {
            let got = match r.buckets(*id, at(-7_230)..at(3_600), res) {
                Ok(ChartData::Facts(fs)) => {
                    format!("{:?}", fs.into_iter().map(clean).collect::<Vec<_>>())
                }
                other => format!("{other:?}"),
            };
            out.push(format!("{id} buckets {res:?} {got}"));
        }
        out.push(format!(
            "{id} month {:?}",
            r.calendar(
                *id,
                CalendarPeriod::Month {
                    year: 2026,
                    month: 1
                },
                Aggregate::Count
            )
        ));
    }
    out
}

/// A history on a store: the results of every query at the middle and at the end.
fn run(
    s: &Store,
    clock: &ManualClock,
    history: &[Step],
    reader: impl Fn() -> Reader,
) -> Vec<String> {
    let specs = specs();
    let ids: Vec<SeriesId> = specs
        .iter()
        .map(|spec| block_on(s.define_series(spec.clone())).unwrap())
        .collect();
    let mut out = Vec::new();
    for (k, step) in history.iter().enumerate() {
        match step {
            Step::Emit(rs, revise) => {
                let readings: Vec<Reading> = rs
                    .iter()
                    .map(|(i, t, v, q, c)| reading(ids[*i], &specs[*i], *t, *v, *q, *c))
                    .collect();
                let intent = if *revise {
                    Intent::Revise
                } else {
                    Intent::Normal
                };
                let ack = block_on(s.emit_with(&readings, intent)).unwrap();
                out.push(format!("emit {}", ack.recorded()));
            }
            Step::Advance(secs) => clock.advance(Duration::from_secs(*secs)),
            Step::WallBack(secs) => clock.jump_wall(Span::from_secs(-(*secs as i64))),
            Step::SiteZone(z, t) => {
                block_on(s.set_site_zone(&ZoneName::new(ZONES[*z]), at(*t))).unwrap();
            }
        }
        if k == history.len() / 2 {
            out.extend(queries(&reader(), &ids, &specs, true));
        }
    }
    out.extend(queries(&reader(), &ids, &specs, true));
    out
}

fn next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

#[test]
fn durable_results_equal_the_model_s() {
    // Each case opens a durable store and runs some thousands of queries on each backend.
    let cases = (ProptestConfig::default().cases / 8).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&steps(), |history| {
            let n = next();
            let m_clock = ManualClock::new(start());
            let model = block_on(Store::open(
                StoreConfig::new(format!("kvik-test/windows/{n}"), "Europe/London")
                    .create_if_missing(true)
                    .backend(Backend::Model)
                    .env(Env::test(m_clock.clone())),
            ))
            .unwrap();
            let d_clock = ManualClock::new(start());
            let mut env = Env::test(d_clock.clone());
            env.fs = Arc::new(MemFs::new());
            let dir = format!("windows/{n}");
            let mut log = LogConfig::default();
            log.segment_size = 16 << 20;
            log.min_sync_interval = Duration::ZERO;
            let durable = block_on(Store::open(
                StoreConfig::new(&dir, "Europe/London")
                    .create_if_missing(true)
                    .env(env.clone())
                    .log(log),
            ))
            .unwrap();
            let m = run(&model, &m_clock, &history, || model.reader());
            let d = run(&durable, &d_clock, &history, || durable.reader());
            prop_assert_eq!(m.len(), d.len());
            for (a, b) in m.iter().zip(&d) {
                prop_assert_eq!(a, b);
            }
            // A reader process gives the same results, apart from the live staleness of observed series, which it
            // measures in event time (Q-140). Its window clock starts at its own open, so it is compared only where
            // the wall clock never stepped back.
            if !history.iter().any(|s| matches!(s, Step::WallBack(_))) {
                let ids: Vec<SeriesId> = (1..=specs().len() as u32).map(SeriesId).collect();
                let process = Reader::open(ReaderConfig::new(&dir).env(env.clone())).unwrap();
                let want = queries(&model.reader(), &ids, &specs(), false);
                let got = queries(&process, &ids, &specs(), false);
                for (a, b) in want.iter().zip(&got) {
                    prop_assert_eq!(a, b);
                }
            }
            block_on(durable.shutdown()).unwrap();
            block_on(model.shutdown()).unwrap();
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_reader_process_answers_after_shutdown_and_power_loss() {
    let clock = ManualClock::new(start());
    let mut env = Env::test(clock.clone());
    let fs = Arc::new(MemFs::new());
    let boot = Arc::new(kvik::env::FixedBootId::new(Some([1; 16])));
    env.fs = fs.clone();
    env.boot = boot.clone();
    let config = || {
        let mut log = LogConfig::default();
        log.segment_size = 16 << 20;
        StoreConfig::new("windows/process", "Europe/London")
            .create_if_missing(true)
            .env(env.clone())
            .log(log)
    };
    let s = block_on(Store::open(config())).unwrap();
    let g = block_on(
        s.define_series(
            number(gauge())
                .name("w/g/power")
                .expected_interval(Duration::from_secs(1))
                .staleness(Duration::from_secs(3)),
        ),
    )
    .unwrap();
    let readings: Vec<Reading> = [(-10, 100.0), (-2, 200.0)]
        .iter()
        .map(|(t, v)| Reading::new(g, at(*t), Value::F64(*v)))
        .collect();
    block_on(s.emit(&readings)).unwrap();
    let process = Reader::open(ReaderConfig::new("windows/process").env(env.clone())).unwrap();
    let avg = |r: &Reader| {
        r.window(g, Window::absolute(at(-20)..at(0)), Aggregate::Avg)
            .unwrap()
    };
    let stale = |r: &Reader| match r.live(g).unwrap() {
        Live::Value(v) => (v.fact.number(), v.stale),
        other => panic!("{other:?}"),
    };
    let want = avg(&s.reader());
    // Held for S = 3 s: 100 from -10 s to -7 s, 200 from -2 s to 0 s.
    assert_eq!(want.number(), Some((300.0 + 400.0) / 5.0));
    assert_eq!(want.coverage, Duration::from_secs(5));
    assert_eq!(avg(&process), want);
    clock.advance(Duration::from_secs(2));
    // The writer's process measures live staleness since the commit (2 s); a reader process, which has no commit
    // instant, in event time (4 s since the fact, Q-140).
    assert_eq!(stale(&s.reader()), (Some(200.0), false));
    assert_eq!(stale(&process), (Some(200.0), true));
    // After a shutdown the reader keeps answering.
    block_on(s.shutdown()).unwrap();
    assert_eq!(avg(&process), want);
    // A store opened again has no commit instant either until its next commit.
    let s = block_on(Store::open(config())).unwrap();
    assert_eq!(stale(&s.reader()), (Some(200.0), true));
    block_on(s.emit(&[Reading::new(g, at(-1), Value::F64(300.0))])).unwrap();
    assert_eq!(stale(&s.reader()), (Some(300.0), false));
    // A power loss keeps what was acknowledged: after the next start's recovery, a reader process answers from
    // it, as does the writer's own reader, both in event time (3 s since the fact, not more than S).
    fs.crash_after(0);
    drop(s);
    fs.power_loss();
    boot.set(Some([2; 16]));
    let s = block_on(Store::open(config())).unwrap();
    let after = Reader::open(ReaderConfig::new("windows/process").env(env.clone())).unwrap();
    for r in [&after, &s.reader()] {
        let w = r
            .window(g, Window::absolute(at(-20)..at(0)), Aggregate::Count)
            .unwrap();
        assert_eq!(w.number(), Some(3.0));
        assert_eq!(stale(r), (Some(300.0), false));
    }
    clock.advance(Duration::from_secs(1));
    assert_eq!(stale(&after), (Some(300.0), true));
    block_on(s.shutdown()).unwrap();
}
