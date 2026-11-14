//! The public interface end to end, on the model backend.

mod common;

use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use common::{Flag, at, block_on, config, poll_once, start, store};
use kvik::env::ManualClock;
use kvik::jiff::civil::date;
use kvik::{
    Aggregate, BusyReason, CalendarUnit, ChartData, Condition, ConditionState, Consumer, Delivery,
    DeliveryId, DeliveryKind, Error, Funnel, Health, Intent, IntervalLength, Kind, Live, Operator,
    ParameterChange, Position, Quality, Reading, Refusal, Resolution, Select, SeriesSpec, Start,
    StateValues, Store, TriggerCause, Value, ValueType, View, Window,
};

fn gauge(name: &str, expected: u64) -> SeriesSpec {
    SeriesSpec {
        name: name.into(),
        kind: Kind::Gauge {
            value: ValueType::F64,
        },
        unit: "W".into(),
        expected_interval: Some(Duration::from_secs(expected)),
        ..Default::default()
    }
}

fn counter(name: &str) -> SeriesSpec {
    SeriesSpec {
        name: name.into(),
        kind: Kind::Counter {
            value: ValueType::F64,
        },
        unit: "Wh".into(),
        expected_interval: Some(Duration::from_secs(1)),
        ..Default::default()
    }
}

fn num(id: kvik::SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

/// Takes every ready delivery, confirming each.
fn drain(c: &mut Consumer) -> Vec<Delivery> {
    let mut out = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        block_on(c.confirm(d.id)).unwrap();
        out.push(d);
    }
    out
}

fn triggers(ds: &[Delivery]) -> Vec<ConditionState> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Trigger(t) => Some(t.state.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn define_emit_and_read() {
    let (s, clock) = store("define-emit-read");
    assert!(!s.is_durable());
    let g = block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap();
    let c = block_on(s.define_series(counter("plant/line-1/energy"))).unwrap();
    // Defining again the same way returns the same id; differently, an error.
    assert_eq!(
        block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap(),
        g
    );
    assert!(matches!(
        block_on(s.define_series(gauge("plant/line-1/power", 2))),
        Err(Error::NameInUse(n)) if n == "plant/line-1/power"
    ));

    let ack = block_on(s.emit(&[
        num(g, 0, 100.0),
        num(g, 5, 200.0),
        num(c, 0, 1_000.0),
        num(c, 5, 1_010.0),
        num(g, 5, 200.0),
        Reading::new(g, at(6), Value::Text("no".into())),
        Reading::new(kvik::SeriesId::from_raw(99), at(6), Value::F64(1.0)),
    ]))
    .unwrap();
    assert_eq!(ack.recorded, 4);
    assert_eq!(ack.duplicates_dropped, 1);
    assert_eq!(
        ack.refused,
        vec![(5, Refusal::WrongType), (6, Refusal::UnknownSeries)]
    );
    assert!(ack.first < ack.last);

    clock.advance(Duration::from_secs(10));
    let r = s.reader();
    let w = r
        .window(g, Window::rolling(Duration::from_secs(10)), Aggregate::Avg)
        .unwrap();
    assert_eq!(w.number(), Some(150.0));
    assert_eq!(w.count, 2);
    // Each reading is valid for S = 3 s: 0–3 s and 5–8 s.
    assert_eq!(w.coverage, Duration::from_secs(6));
    let inc = r
        .window(c, Window::between(at(0), at(10)), Aggregate::Increase)
        .unwrap();
    assert_eq!(inc.number(), Some(10.0));
    // An average of a counter is empty, not an error: the reference model's
    // choice; spec §6 refuses some aggregates only on interval series.
    let avg = r
        .window(c, Window::rolling(Duration::from_secs(10)), Aggregate::Avg)
        .unwrap();
    assert_eq!(avg.value, None);

    match r.live(g).unwrap() {
        Live::Value(v) => {
            assert_eq!(v.fact.number(), Some(200.0));
            assert!(v.stale, "10 s after a commit with S = 3 s");
        }
        other => panic!("{other:?}"),
    }

    let info = s.series("plant/line-1/power").unwrap();
    assert_eq!(info.id, g);
    assert_eq!(info.latest_t, Some(at(5)));
    assert_eq!(info.staleness, Some(Duration::from_secs(3)));

    let st = s.stats();
    assert_eq!(st.duplicates_dropped, 1);
    assert_eq!(st.refused[&Refusal::WrongType], 1);
    assert_eq!(st.series[&g].facts, 2);
    assert_eq!(s.health(), Health::Ok);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn views_revise_and_retire() {
    let (s, clock) = store("views");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let first = block_on(s.emit(&[num(g, 0, 1.0)])).unwrap().last.unwrap();
    block_on(s.emit(&[num(g, 0, 2.0)])).unwrap();
    // A replay of the first value is a duplicate; with revise it is recorded.
    assert_eq!(block_on(s.emit(&[num(g, 0, 1.0)])).unwrap().recorded, 0);
    let revised = block_on(s.emit_with(&[num(g, 0, 1.0)], Intent::Revise)).unwrap();
    assert_eq!(revised.recorded, 1);
    clock.advance(Duration::from_secs(1));
    let r = s.reader();
    let range = at(0)..at(1);
    let cur = r.facts(g, range.clone(), View::Current).unwrap();
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].number(), Some(1.0));
    assert!(cur[0].flags.revise);
    let hist = r.facts(g, range.clone(), View::History).unwrap();
    let values: Vec<_> = hist.iter().map(|f| f.number().unwrap()).collect();
    assert_eq!(values, vec![1.0, 2.0, 1.0]);
    let then = r.facts(g, range.clone(), View::AsOf(first)).unwrap();
    assert_eq!(then[0].number(), Some(1.0));
    assert_eq!(then[0].position, first);
    let before = r
        .facts(
            g,
            range,
            View::AsOfTime("2000-01-01T00:00:00Z".parse().unwrap()),
        )
        .unwrap();
    assert!(before.is_empty());

    block_on(s.retire(g, at(10))).unwrap();
    let ack = block_on(s.emit(&[num(g, 9, 1.0), num(g, 10, 1.0)])).unwrap();
    assert_eq!(ack.recorded, 1);
    assert_eq!(ack.refused, vec![(1, Refusal::Retired)]);
    assert!(s.series(g).unwrap().retired_at.is_some());
    assert!(matches!(s.series("a/b/c"), Err(Error::UnknownSeries(_))));
}

#[test]
fn threshold_condition_delivers_transitions() {
    let (s, clock) = store("threshold");
    let g = block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap();
    let mut c = block_on(s.consumer("load-shedding", Start::Now, Select::None)).unwrap();
    let f = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(3_000.0).clear_below(2_500.0)),
    }))
    .unwrap();
    assert_eq!(c.routes().unwrap()[0].0, f);

    block_on(s.emit(&[num(g, 0, 2_000.0)])).unwrap();
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(g, 1, 3_500.0)])).unwrap();
    let ds = drain(&mut c);
    assert_eq!(triggers(&ds).last(), Some(&ConditionState::True));
    let t = ds
        .iter()
        .find(|d| matches!(&d.kind, DeliveryKind::Trigger(t) if t.state == ConditionState::True))
        .unwrap();
    assert!(matches!(t.id, DeliveryId::Condition { funnel, .. } if funnel == f));
    assert!(!t.flags.replayed);

    // Between the levels: no transition; below the clear level: false.
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(g, 2, 2_800.0)])).unwrap();
    assert!(triggers(&drain(&mut c)).is_empty());
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(g, 3, 2_000.0)])).unwrap();
    assert_eq!(triggers(&drain(&mut c)), vec![ConditionState::False]);
}

#[test]
fn rolling_operator_and_stale_for_timer() {
    let (s, clock) = store("stale-for");
    let g = block_on(s.define_series(gauge("x/y/power", 1))).unwrap();
    let mut c = block_on(s.consumer("watch", Start::Now, Select::None)).unwrap();
    let stale = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::stale_for(Duration::from_secs(2))),
    }))
    .unwrap();
    block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: Some(Operator::rolling(Aggregate::Avg, Duration::from_secs(10))),
        condition: Some(Condition::above(50.0)),
    }))
    .unwrap();
    block_on(s.emit(&[num(g, 0, 100.0)])).unwrap();
    drain(&mut c);
    // S = 3 s; stale for 2 s more: true after 5 s without a reading, found
    // by the timer.
    clock.advance(Duration::from_secs(6));
    s.tick().unwrap();
    let ds = drain(&mut c);
    let t = ds
        .iter()
        .find_map(|d| match &d.kind {
            DeliveryKind::Trigger(t) if t.funnel == stale => Some(t.clone()),
            _ => None,
        })
        .expect("a stale-for trigger");
    assert_eq!(t.state, ConditionState::True);
    assert_eq!(t.cause, TriggerCause::Timer);
}

#[test]
fn pattern_funnel_follows_new_definitions() {
    let (s, clock) = store("patterns");
    let a = block_on(s.define_series(gauge("plant/a/power", 1))).unwrap();
    let other = block_on(s.define_series(gauge("plant/a/energy", 1))).unwrap();
    let mut c = block_on(s.consumer("all-power", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Names("plant/*/power".into())))).unwrap();
    let b = block_on(s.define_series(gauge("plant/b/power", 1))).unwrap();
    block_on(s.emit(&[num(a, 0, 1.0), num(other, 0, 2.0), num(b, 0, 3.0)])).unwrap();
    clock.advance(Duration::from_secs(1));
    let series: Vec<_> = drain(&mut c)
        .into_iter()
        .filter_map(|d| match d.kind {
            DeliveryKind::Fact(f) => Some(f.series),
            _ => None,
        })
        .collect();
    assert_eq!(series, vec![a, b]);
}

#[test]
fn restart_replays_then_resyncs() {
    let clock = ManualClock::new(start());
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    let g = block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let facts = block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let cond = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }))
    .unwrap();
    for (i, v) in [5.0, 6.0, 20.0].into_iter().enumerate() {
        block_on(s.emit(&[num(g, i as i64, v)])).unwrap();
        clock.advance(Duration::from_secs(1));
    }
    // Confirm only the first fact.
    let d = c.try_next().unwrap().unwrap();
    let DeliveryKind::Fact(first) = &d.kind else {
        panic!("{d:?}")
    };
    assert_eq!(first.number(), Some(5.0));
    block_on(c.confirm(d.id)).unwrap();
    let cursor = s.stats().consumers["alarms"].cursor;
    assert_eq!(cursor, first.position);

    // Restart: a second writer is refused while the first is open.
    assert!(matches!(
        block_on(Store::open(config("restart", &clock))),
        Err(Error::Locked { .. })
    ));
    block_on(s.shutdown()).unwrap();
    assert!(matches!(c.try_next(), Err(Error::Closed)));
    clock.advance(Duration::from_secs(60));
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let ds = drain(&mut c);
    let replayed: Vec<_> = ds
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Fact(f) => {
                assert!(d.flags.replayed);
                Some(f.number().unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(replayed, vec![6.0, 20.0]);
    let resync = ds
        .iter()
        .find_map(|d| match &d.kind {
            DeliveryKind::Resync(r) => Some(r.clone()),
            _ => None,
        })
        .expect("a resync after the replay");
    let funnels: Vec<_> = resync.funnels.iter().map(|f| f.funnel).collect();
    assert_eq!(funnels, vec![facts, cond]);
    let fs = &resync.funnels[1];
    assert_eq!(fs.condition, Some(ConditionState::True));
    // The true transition was delivered but never confirmed, so the latch
    // does not hold it: the resync is a transition, to be acted on.
    assert!(fs.transition);
    match &fs.live[0].1 {
        Live::Value(v) => assert_eq!(v.fact.number(), Some(20.0)),
        other => panic!("{other:?}"),
    }
    // The data survived the restart.
    let w = s
        .reader()
        .window(g, Window::between(at(0), at(3)), Aggregate::Count)
        .unwrap();
    assert_eq!(w.number(), Some(3.0));
    // Dropping the store acts as a crash; the store opens again.
    drop(c);
    drop(s);
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    assert_eq!(s.series(g).unwrap().latest_t, Some(at(2)));
    // Everything was confirmed, the resync included, so the latch holds
    // true: the resync after this restart is no transition.
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let resync = drain(&mut c)
        .into_iter()
        .find_map(|d| match d.kind {
            DeliveryKind::Resync(r) => Some(r),
            _ => None,
        })
        .expect("a resync");
    assert_eq!(resync.funnels[1].condition, Some(ConditionState::True));
    assert!(!resync.funnels[1].transition);
}

#[test]
fn next_waits_until_an_emit() {
    let (s, _clock) = store("next-waits");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let woken = Arc::new(Flag::default());
    let mut next = std::pin::pin!(c.next());
    assert!(poll_once(next.as_mut(), &woken).is_pending());
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    assert!(woken.take());
    match poll_once(next.as_mut(), &woken) {
        Poll::Ready(Ok(Some(d))) => assert!(matches!(d.kind, DeliveryKind::Fact(_))),
        other => panic!("{other:?}"),
    }
}

#[test]
fn back_pressure() {
    let clock = ManualClock::new(start());
    let mut cfg = config("back-pressure", &clock);
    cfg.limits.ingest_queue = 2;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let three = [num(g, 0, 1.0), num(g, 1, 1.0), num(g, 2, 1.0)];
    assert!(matches!(
        s.try_emit(&three),
        Err(Error::Busy(BusyReason::QueueFull))
    ));
    // A batch larger than the queue would wait forever: emit returns.
    assert!(matches!(
        block_on(s.emit(&three)),
        Err(Error::Busy(BusyReason::QueueFull))
    ));
    // Host-stamped readings wait while the clock is not synchronised.
    clock.set_synchronised(false);
    let host = [num(g, 0, 1.0).host_stamped()];
    assert!(matches!(
        s.try_emit(&host),
        Err(Error::Busy(BusyReason::UnsyncedClock))
    ));
    let device = s.try_emit(&[num(g, 1, 2.0)]).unwrap();
    assert!(device.refused.is_empty());
    let woken = Arc::new(Flag::default());
    let mut emit = std::pin::pin!(s.emit(&host));
    assert!(poll_once(emit.as_mut(), &woken).is_pending());
    clock.set_synchronised(true);
    s.tick().unwrap();
    assert!(woken.take());
    match poll_once(emit.as_mut(), &woken) {
        Poll::Ready(Ok(ack)) => assert_eq!(ack.recorded, 1),
        other => panic!("{other:?}"),
    }
    let st = s.stats();
    assert_eq!(st.waits[&BusyReason::QueueFull], 2);
    assert_eq!(st.waits[&BusyReason::UnsyncedClock], 2);
}

#[test]
fn interval_series_and_calendar_results() {
    let (s, clock) = store("calendar");
    let half_hour = Duration::from_secs(1_800);
    let energy = block_on(s.define_series(SeriesSpec {
        name: "site/meter/import".into(),
        kind: Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Fixed {
                d: half_hour,
                phase: Duration::ZERO,
            },
        },
        unit: "kWh".into(),
        ..Default::default()
    }))
    .unwrap();
    let g = block_on(s.define_series(gauge("site/room/temperature", 60))).unwrap();
    let ack = block_on(s.emit(&[
        num(energy, -7_200, 1.5),
        num(energy, -5_400, 2.5),
        num(energy, -6_300, 9.0),
    ]))
    .unwrap();
    assert_eq!(ack.refused, vec![(2, Refusal::OffPhase)]);
    let temps: Vec<_> = (0..=10).map(|m| num(g, -3_600 + 60 * m, 20.0)).collect();
    block_on(s.emit(&temps)).unwrap();
    clock.advance(Duration::from_secs(1));

    let r = s.reader();
    assert_eq!(
        r.interval(energy, at(-6_300)).unwrap(),
        (at(-7_200), at(-5_400))
    );
    let day = r
        .calendar(energy, date(2026, 1, 5), Aggregate::Sum)
        .unwrap();
    assert_eq!(day.number(), Some(4.0));
    assert!(!day.flags.apportioned);
    let part = r
        .window(
            energy,
            Window::between(at(-6_300), at(-3_600)),
            Aggregate::Sum,
        )
        .unwrap();
    assert_eq!(part.number(), Some(3.25));
    assert!(part.flags.apportioned);
    // Avg, integral, increase and time-in-state are refused on an interval
    // series (spec §6 Aggregates).
    assert!(matches!(
        r.window(energy, Window::between(at(-7_200), at(0)), Aggregate::Avg),
        Err(Error::AggregateNotApplicable)
    ));

    let day = r.calendar(g, date(2026, 1, 5), Aggregate::Avg).unwrap();
    assert_eq!(day.number(), Some(20.0));
    assert_eq!(day.count, 11);
    assert_eq!(day.from, "2026-01-05T00:00:00Z".parse().unwrap());
    let ny = r
        .calendar_in(g, date(2026, 1, 5), "America/New_York", Aggregate::Count)
        .unwrap();
    assert_eq!(ny.from, "2026-01-05T05:00:00Z".parse().unwrap());
    assert_eq!(ny.number(), Some(11.0));
    let month = r
        .calendar(
            g,
            kvik::CalendarPeriod::Month {
                year: 2026,
                month: 1,
            },
            Aggregate::Count,
        )
        .unwrap();
    assert_eq!(month.from, "2026-01-01T00:00:00Z".parse().unwrap());
    assert_eq!(month.to, "2026-02-01T00:00:00Z".parse().unwrap());
    assert_eq!(month.number(), Some(11.0));

    match r
        .buckets(g, at(-3_600)..at(0), Resolution::Auto { max_points: 100 })
        .unwrap()
    {
        ChartData::Facts(f) => assert_eq!(f.len(), 11),
        other => panic!("{other:?}"),
    }
    match r
        .buckets(g, at(-3_600)..at(0), Resolution::Auto { max_points: 5 })
        .unwrap()
    {
        ChartData::Buckets { step, buckets } => {
            assert_eq!(step, Duration::from_secs(900));
            assert_eq!(buckets.len(), 4);
            assert_eq!(buckets[0].count, 11);
            assert_eq!(buckets[0].max, Some(20.0));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn state_and_text_series() {
    let (s, clock) = store("states");
    let mode = block_on(s.define_series(SeriesSpec {
        name: "hvac/unit/mode".into(),
        kind: Kind::State {
            values: StateValues::Named(vec!["off".into(), "heat".into(), "cool".into()]),
        },
        ..Default::default()
    }))
    .unwrap();
    let note = block_on(s.define_series(SeriesSpec {
        name: "hvac/unit/note".into(),
        kind: Kind::Text,
        ..Default::default()
    }))
    .unwrap();
    let ack = block_on(
        s.emit(&[
            Reading::new(mode, at(0), Value::State(1)),
            Reading::new(mode, at(10), Value::State(0)),
            Reading::new(mode, at(20), Value::State(7)),
            Reading::new(note, at(0), Value::Text("filter changed".into()))
                .with_quality(Quality::Estimated),
        ]),
    )
    .unwrap();
    assert_eq!(ack.refused, vec![(2, Refusal::UnknownState)]);
    clock.advance(Duration::from_secs(30));
    let r = s.reader();
    let heat = r
        .window(
            mode,
            Window::between(at(0), at(30)),
            Aggregate::TimeInState(Value::State(1)),
        )
        .unwrap();
    assert_eq!(heat.number(), Some(10.0));
    match r.live(mode).unwrap() {
        Live::Value(v) => assert_eq!(v.fact.value, Some(Value::State(0))),
        other => panic!("{other:?}"),
    }
    let notes = r.facts(note, at(0)..at(1), View::Current).unwrap();
    assert_eq!(notes[0].value, Some(Value::Text("filter changed".into())));
    assert_eq!(notes[0].quality, Quality::Estimated);
}

#[test]
fn unsupported_items_say_so() {
    let (s, _clock) = store("unsupported");
    let decimal = SeriesSpec {
        name: "a/b/decimal".into(),
        kind: Kind::Gauge {
            value: ValueType::Decimal { scale: 1 },
        },
        ..Default::default()
    };
    assert!(matches!(
        block_on(s.define_series(decimal)),
        Err(Error::NotSupportedYet { .. })
    ));
    let gas_day = SeriesSpec {
        name: "a/b/gas".into(),
        kind: Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Calendar {
                unit: CalendarUnit::Day {
                    start: kvik::jiff::civil::time(5, 0, 0, 0),
                },
                zone: "Europe/London".into(),
            },
        },
        ..Default::default()
    };
    assert!(matches!(
        block_on(s.define_series(gas_day)),
        Err(Error::NotSupportedYet { .. })
    ));
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let change = ParameterChange {
        expected_interval: Some(Duration::from_secs(5)),
        ..Default::default()
    };
    assert!(matches!(
        block_on(s.set_parameters(g, at(0), change)),
        Err(Error::NotSupportedYet { .. })
    ));
    let retention = ParameterChange {
        retention: Some(kvik::RetentionClass::Diagnostic {
            period: Duration::from_secs(86_400),
        }),
        ..Default::default()
    };
    block_on(s.set_parameters(g, at(0), retention)).unwrap();
    assert!(matches!(
        block_on(s.consumer("late", Start::Position(Position::ORIGIN), Select::None)),
        Err(Error::NotSupportedYet { .. })
    ));
    let c = block_on(s.consumer("ops", Start::Now, Select::None)).unwrap();
    let operator_only = Funnel {
        series: Select::Ids(vec![g]),
        operator: Some(Operator::rolling(Aggregate::Avg, Duration::from_secs(60))),
        condition: None,
    };
    assert!(matches!(
        block_on(c.route(operator_only)),
        Err(Error::NotSupportedYet { .. })
    ));
    assert!(matches!(
        block_on(s.rebuild(kvik::Rebuild::Derived)),
        Err(Error::NotSupportedYet { .. })
    ));
}

#[test]
fn opening_rules() {
    let clock = ManualClock::new(start());
    assert!(matches!(
        block_on(kvik::Reader::open("kvik-test/never-opened")),
        Err(Error::NotFound { .. })
    ));
    let mut bad = config("zones", &clock);
    bad.time_zone = "Not/AZone".into();
    assert!(matches!(
        block_on(Store::open(bad)),
        Err(Error::UnknownZone(_))
    ));
    let s = block_on(Store::open(config("zones", &clock))).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    let reader = block_on(kvik::Reader::open("kvik-test/zones")).unwrap();
    assert_eq!(reader.series(g).unwrap().name, "a/b/c");
    block_on(s.shutdown()).unwrap();
    // Readers are independent of the writer (`docs/api.md` Opening): they
    // keep reading what was committed after it shuts down.
    assert!(matches!(reader.live(g), Ok(Live::Value(_))));
    let mut other = config("zones", &clock);
    other.time_zone = "Europe/Oslo".into();
    assert!(matches!(
        block_on(Store::open(other)),
        Err(Error::SiteZoneMismatch { .. })
    ));
    let s = block_on(Store::open(config("zones", &clock))).unwrap();
    assert!(matches!(reader.live(g), Ok(Live::Value(_))));
    assert_eq!(s.health(), Health::Ok);
    // One handle per consumer name at a time.
    let c = block_on(s.consumer("one", Start::Now, Select::None)).unwrap();
    assert!(matches!(
        block_on(s.consumer("one", Start::Now, Select::None)),
        Err(Error::ConsumerInUse(_))
    ));
    drop(c);
    block_on(s.consumer("one", Start::Now, Select::None)).unwrap();
}

#[test]
fn on_change_and_held_for_conditions() {
    let (s, clock) = store("on-change-held-for");
    let g = block_on(s.define_series(gauge("a/b/level", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let change = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::on_change(5.0)),
    }))
    .unwrap();
    let held = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::held_for(50.0, Duration::from_secs(3))),
    }))
    .unwrap();
    let by_funnel = |ds: Vec<Delivery>, f| -> Vec<ConditionState> {
        ds.into_iter()
            .filter_map(|d| match d.kind {
                DeliveryKind::Trigger(t) if t.funnel == f => Some(t.state),
                _ => None,
            })
            .collect()
    };
    block_on(s.emit(&[num(g, 0, 60.0)])).unwrap();
    let ds = drain(&mut c);
    assert_eq!(
        by_funnel(ds.clone(), change),
        vec![ConditionState::Value(Some(60.0))]
    );
    assert!(by_funnel(ds, held).is_empty(), "not held long enough yet");
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(g, 1, 62.0)])).unwrap();
    assert!(
        by_funnel(drain(&mut c), change).is_empty(),
        "inside the dead-band"
    );
    clock.advance(Duration::from_secs(3));
    s.tick().unwrap();
    assert_eq!(by_funnel(drain(&mut c), held), vec![ConditionState::True]);
}

#[test]
fn critical_disk_makes_ingest_wait() {
    let clock = ManualClock::new(start());
    let mut cfg = config("critical-disk", &clock);
    // Above the model's fast-disk capacity, so free space is always below it.
    cfg.limits.critical_mark = 2 << 40;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    assert!(matches!(
        s.try_emit(&[num(g, 0, 1.0)]),
        Err(Error::Busy(BusyReason::DiskFull))
    ));
    assert!(matches!(s.health(), Health::Degraded { .. }));
}

#[test]
fn consumer_before_any_series() {
    let (s, clock) = store("consumer-first");
    let mut c = block_on(s.consumer("early", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(c.route(Funnel::facts(Select::Names("site/*/power".into())))).unwrap();
    assert!(c.try_next().unwrap().is_none());
    let g = block_on(s.define_series(gauge("site/a/power", 1))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    clock.advance(Duration::from_secs(1));
    let facts = drain(&mut c)
        .into_iter()
        .filter(|d| matches!(d.kind, DeliveryKind::Fact(_)))
        .count();
    assert_eq!(
        facts, 1,
        "one fact delivery per position, whatever the funnels"
    );
    assert!(matches!(
        block_on(s.define_series(SeriesSpec {
            retention: kvik::RetentionClass::Diagnostic {
                period: Duration::ZERO
            },
            ..gauge("site/b/power", 1)
        })),
        Err(Error::InvalidDefinition(_))
    ));
    assert!(matches!(
        s.series("site/b/power"),
        Err(Error::UnknownSeries(_))
    ));
}

fn facts_of(ds: &[Delivery]) -> Vec<(kvik::Fact, kvik::DeliveryFlags)> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Fact(f) => Some((f.clone(), d.flags)),
            _ => None,
        })
        .collect()
}

#[test]
fn auto_resolution_counts_from_the_aligned_floor() {
    let (s, _clock) = store("auto-aligned");
    let g = block_on(s.define_series(gauge("a/b/c", 60))).unwrap();
    let minutes: Vec<_> = (0..=60).map(|m| num(g, -3_600 + 60 * m, 1.0)).collect();
    block_on(s.emit(&minutes)).unwrap();
    let r = s.reader();
    // 11:00:30 to 12:00:30: quarter-hours from 11:00 would be five buckets.
    match r
        .buckets(g, at(-3_570)..at(30), Resolution::Auto { max_points: 4 })
        .unwrap()
    {
        ChartData::Buckets { step, buckets } => {
            assert!(buckets.len() <= 4, "{} buckets", buckets.len());
            assert_eq!(step, Duration::from_secs(1_800));
            assert_eq!(buckets[0].from, at(-3_570));
        }
        other => panic!("{other:?}"),
    }
    match r
        .buckets(
            g,
            at(-3_600)..at(0),
            Resolution::Step(Duration::from_secs(600)),
        )
        .unwrap()
    {
        ChartData::Buckets { step, buckets } => {
            assert_eq!(step, Duration::from_secs(600));
            assert_eq!(buckets.len(), 6);
            assert!(buckets.iter().all(|b| b.count == 10));
        }
        other => panic!("{other:?}"),
    }
}

/// A waker that reads the store when woken and when dropped: it deadlocks if
/// the store wakes or drops it under its own lock.
struct ReadingWaker(kvik::Reader, kvik::SeriesId, Arc<Flag>);

impl std::task::Wake for ReadingWaker {
    fn wake(self: Arc<Self>) {
        self.0.series(self.1).unwrap();
        std::task::Wake::wake(self.2.clone());
    }
}

impl Drop for ReadingWaker {
    fn drop(&mut self) {
        self.0.series(self.1).unwrap();
    }
}

#[test]
fn wakers_run_outside_the_store_lock() {
    let (s, _clock) = store("waker-lock");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let woken = Arc::new(Flag::default());
    {
        let waker = std::task::Waker::from(Arc::new(ReadingWaker(s.reader(), g, woken.clone())));
        let mut next = std::pin::pin!(c.next());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(next.as_mut().poll(&mut cx).is_pending());
    }
    // The store now holds the only reference to the waker.
    let s = Arc::new(s);
    let (tx, rx) = std::sync::mpsc::channel();
    let writer = s.clone();
    std::thread::spawn(move || {
        let r = block_on(writer.emit(&[num(g, 0, 1.0)]));
        tx.send(r.is_ok()).unwrap();
    });
    if rx.recv_timeout(Duration::from_secs(5)) != Ok(true) {
        // The store is deadlocked: dropping the handles would block too.
        std::mem::forget(c);
        std::mem::forget(s);
        panic!("the emit did not complete: a waker ran under the store lock");
    }
    assert!(woken.take());
}

#[test]
fn versioned_series_and_outranked_facts() {
    let (s, clock) = store("versioned");
    let v = block_on(s.define_series(SeriesSpec {
        versioned: Some(kvik::ChangedAtKind::Version),
        ..gauge("grid/area/price", 1)
    }))
    .unwrap();
    let mut c = block_on(s.consumer("prices", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![v])))).unwrap();
    let ack = block_on(s.emit(&[
        num(v, 0, 1.0),
        num(v, 0, 10.0).changed_at(2),
        num(v, 0, 5.0).changed_at(1),
    ]))
    .unwrap();
    assert_eq!(ack.refused, vec![(0, Refusal::ChangedAtMissing)]);
    assert_eq!(ack.recorded, 2);
    assert_eq!(
        block_on(s.emit_with(&[num(v, 0, 1.0).changed_at(3)], Intent::Revise))
            .unwrap()
            .refused,
        vec![(0, Refusal::ReviseOnVersioned)]
    );
    clock.advance(Duration::from_secs(1));
    let fs = facts_of(&drain(&mut c));
    assert_eq!(fs.len(), 2);
    assert!(!fs[0].1.outranked);
    assert!(
        fs[1].1.outranked,
        "a lower changed_at than the current fact"
    );
    let cur = s.reader().facts(v, at(0)..at(1), View::Current).unwrap();
    assert_eq!(cur[0].number(), Some(10.0));
    assert_eq!(cur[0].changed_at, Some(2));
    assert_eq!(s.stats().outranked_on_arrival, 1);
}

#[test]
fn future_facts_are_flagged() {
    let (s, _clock) = store("future");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(s.emit(&[num(g, 120, 1.0)])).unwrap();
    let fs = facts_of(&drain(&mut c));
    assert!(fs[0].0.flags.future);
    assert!(fs[0].1.future);
    assert_eq!(fs[0].0.quality, Quality::Suspect);
    // Ignored by live state until its time comes.
    assert!(matches!(s.reader().live(g).unwrap(), Live::None));
}

#[test]
fn overflow_catches_up_without_gap_or_repeat() {
    let clock = ManualClock::new(start());
    let mut cfg = config("overflow", &clock);
    cfg.limits.consumer_queue = 4;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let mut c = block_on(s.consumer("slow", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let mut positions = Vec::new();
    for i in 0..10 {
        positions.push(
            block_on(s.emit(&[num(g, i, i as f64)]))
                .unwrap()
                .last
                .unwrap(),
        );
    }
    let fs = facts_of(&drain(&mut c));
    let got: Vec<_> = fs.iter().map(|(f, _)| f.position).collect();
    assert_eq!(got, positions);
    assert!(
        fs.iter().any(|(_, fl)| fl.replayed),
        "some came from catch-up"
    );
    assert!(s.stats().overflows >= 1);
}

#[test]
fn a_consumer_taken_again_replays_then_resyncs() {
    let (s, _clock) = store("reconnect");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    let first = c.try_next().unwrap().unwrap();
    assert!(!first.flags.replayed);
    // Not confirmed; the consumer process restarts.
    drop(c);
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let ds = drain(&mut c);
    assert!(matches!(&ds[0].kind, DeliveryKind::Fact(f) if f.number() == Some(1.0)));
    assert!(ds[0].flags.replayed);
    assert!(matches!(ds[1].kind, DeliveryKind::Resync(_)));
}

#[test]
fn alias_reroutes_patterns_and_unroute_stops_delivery() {
    let (s, clock) = store("alias-unroute");
    let e = block_on(s.define_series(gauge("site/a/energy", 1))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let f = block_on(c.route(Funnel::facts(Select::Names("site/*/power".into())))).unwrap();
    block_on(s.emit(&[num(e, 0, 1.0)])).unwrap();
    assert!(facts_of(&drain(&mut c)).is_empty());
    block_on(s.alias(e, "site/a/power")).unwrap();
    block_on(s.emit(&[num(e, 1, 2.0)])).unwrap();
    assert_eq!(facts_of(&drain(&mut c)).len(), 1);
    block_on(c.unroute(f)).unwrap();
    assert!(c.routes().unwrap().is_empty());
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(e, 2, 3.0)])).unwrap();
    assert!(facts_of(&drain(&mut c)).is_empty());
}

#[test]
fn consumer_emits() {
    let (s, _clock) = store("emits");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    assert!(matches!(
        block_on(s.consumer("writer", Start::Now, Select::Ids(vec![g]))),
        Err(Error::NotSupportedYet { .. })
    ));
    let c = block_on(s.consumer("quiet", Start::Now, Select::None)).unwrap();
    let accepted = c.try_emit(&[num(g, 0, 1.0)]).unwrap();
    assert_eq!(accepted.refused, vec![(0, Refusal::NotDeclared)]);
    assert_eq!(s.stats().refused[&Refusal::NotDeclared], 1);
}
