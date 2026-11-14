//! The public interface end to end, on the model backend.

mod common;

use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use common::{Flag, at, block_on, config, poll_once, start, store};
use kvik::env::ManualClock;
use kvik::{
    Ack, Aggregate, BackupError, CalendarPeriod, CalendarUnit, ChangedAtKind, ChartData, Condition,
    ConditionState, Consumer, ConsumerError, DefineError, Delivery, DeliveryFlags, DeliveryId,
    DeliveryKind, EmitError, Fact, Funnel, Health, Intent, IntervalLength, Kind, Live, LocalDate,
    LocalTime, OpenError, Operator, Outcome, ParameterChange, Position, Quality, QueryError,
    Reader, ReaderConfig, Reading, Reason, Refusal, Resolution, ResyncCause, ResyncPart,
    RetentionClass, Select, SeriesId, SeriesSpec, Span, Start, StateValues, Store, TimeRange,
    Timing, TriggerCause, Value, ValueType, View, Wait, Window, ZoneName,
};

fn gauge(name: &str, expected: u64) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(expected))
}

fn counter(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Counter {
            value: ValueType::F64,
        },
        "Wh",
    )
    .expected_interval(Duration::from_secs(1))
}

fn num(id: SeriesId, secs: i64, v: f64) -> Reading {
    Reading::new(id, at(secs), Value::F64(v))
}

fn refused(ack: &Ack) -> Vec<(usize, Refusal)> {
    ack.refused().collect()
}

fn facts(
    r: &Reader,
    id: SeriesId,
    range: std::ops::Range<kvik::Timestamp>,
    view: View,
) -> Vec<Fact> {
    r.facts(id, range, view)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
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

fn facts_of(ds: &[Delivery]) -> Vec<(Fact, DeliveryFlags)> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Fact(f) => Some((f.clone(), d.flags)),
            _ => None,
        })
        .collect()
}

fn resyncs(ds: &[Delivery]) -> Vec<ResyncPart> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Resync(r) => Some(r.clone()),
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
    // Defining again the same way returns the same id; differently, an error
    // naming the field.
    assert_eq!(
        block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap(),
        g
    );
    assert!(matches!(
        block_on(s.define_series(gauge("plant/line-1/power", 2))),
        Err(DefineError::Conflict {
            field: "expected_interval"
        })
    ));

    let ack = block_on(s.emit(&[
        num(g, 0, 100.0),
        num(g, 5, 200.0),
        num(c, 0, 1_000.0),
        num(c, 5, 1_010.0),
        num(g, 5, 200.0),
        Reading::new(g, at(6), Value::Text("no".into())),
        Reading::new(SeriesId(99), at(6), Value::F64(1.0)),
        Reading::new(g, at(7), Value::Decimal { units: 1, scale: 1 }),
    ]))
    .unwrap();
    assert_eq!(ack.outcomes.len(), 8);
    assert_eq!(ack.recorded(), 4);
    assert_eq!(ack.duplicates_dropped(), 1);
    assert_eq!(ack.outcomes[4], Outcome::Duplicate);
    assert!(matches!(
        ack.outcomes[1],
        Outcome::Recorded {
            outranked: false,
            unchecked: false,
            rounded: false,
            ..
        }
    ));
    assert_eq!(
        refused(&ack),
        vec![
            (5, Refusal::WrongType),
            (6, Refusal::UnknownSeries),
            (7, Refusal::WrongType)
        ]
    );
    assert!(ack.first() < ack.last());

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
        .window(c, Window::absolute(at(0)..at(10)), Aggregate::Increase)
        .unwrap();
    assert_eq!(inc.number(), Some(10.0));
    // The same window relative to now.
    let rel = r
        .window(
            c,
            Window::between(Span::from_secs(-10), Span::ZERO),
            Aggregate::Increase,
        )
        .unwrap();
    assert_eq!(rel.number(), Some(10.0));
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

    let st = s.series_stats(g).unwrap();
    assert_eq!(st.recorded, 2);
    assert_eq!(st.duplicates_dropped, 1);
    assert_eq!(st.refused[&Refusal::WrongType], 2);
    assert!(s.series_stats(SeriesId(99)).is_none());
    assert!(matches!(s.health(), Health::Ok));
    let stats = s.stats();
    assert_eq!(stats.ingest.batches, 1);
    assert!(stats.log.durable > Position::ORIGIN);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn absent_and_non_finite_values_are_recorded_without_a_value() {
    let (s, _clock) = store("absent");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let ack =
        block_on(s.emit(&[Reading::new(g, at(0), Value::Absent), num(g, 1, f64::NAN)])).unwrap();
    assert_eq!(ack.recorded(), 2);
    let fs = facts(&s.reader(), g, at(0)..at(2), View::Current);
    assert!(
        fs.iter()
            .all(|f| f.value == Value::Absent && f.flags.no_value)
    );
    assert!(fs.iter().all(|f| f.quality == Quality::Bad));
}

#[test]
fn absent_keeps_its_supplied_quality_for_the_duplicate_rule() {
    let (s, _clock) = store("absent-quality");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let absent = |q| Reading {
        quality: q,
        ..Reading::new(g, at(0), Value::Absent)
    };
    let ack = block_on(s.emit(&[absent(Quality::Good), absent(Quality::Bad)])).unwrap();
    assert_eq!(ack.recorded(), 2, "spec §2.4: different supplied qualities");
    let ack = block_on(s.emit(&[absent(Quality::Good)])).unwrap();
    assert_eq!(ack.outcomes, vec![Outcome::Duplicate]);
    assert_eq!(s.series_stats(g).unwrap().absent_supplied, 2);
    let fs = facts(&s.reader(), g, at(0)..at(1), View::History);
    assert!(
        fs.iter()
            .all(|f| f.quality == Quality::Bad && f.flags.no_value)
    );
}

#[test]
fn try_emit_returns_a_pending_ack() {
    let (s, _clock) = store("try-emit");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let pending = s.try_emit(&[num(g, 0, 1.0), num(g, 0, 1.0)]).unwrap();
    let ack = block_on(pending).unwrap();
    assert_eq!(ack.recorded(), 1);
    assert_eq!(ack.duplicates_dropped(), 1);
    // Dropping a Pending cancels nothing.
    drop(s.try_emit(&[num(g, 1, 2.0)]).unwrap());
    assert_eq!(s.series_stats(g).unwrap().recorded, 2);
}

#[test]
fn views_revise_and_retire() {
    let (s, clock) = store("views");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let first = block_on(s.emit(&[num(g, 0, 1.0)])).unwrap().last().unwrap();
    block_on(s.emit(&[num(g, 0, 2.0)])).unwrap();
    // A replay of the first value is a duplicate; with revise it is recorded.
    assert_eq!(block_on(s.emit(&[num(g, 0, 1.0)])).unwrap().recorded(), 0);
    let revised = block_on(s.emit_with(&[num(g, 0, 1.0)], Intent::Revise)).unwrap();
    assert_eq!(revised.recorded(), 1);
    clock.advance(Duration::from_secs(1));
    let r = s.reader();
    let range = at(0)..at(1);
    let cur = facts(&r, g, range.clone(), View::Current);
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].number(), Some(1.0));
    assert!(cur[0].flags.revise);
    let hist = facts(&r, g, range.clone(), View::History);
    let values: Vec<_> = hist.iter().map(|f| f.number().unwrap()).collect();
    assert_eq!(values, vec![1.0, 2.0, 1.0]);
    let then = facts(&r, g, range.clone(), View::AsOf(first));
    assert_eq!(then[0].number(), Some(1.0));
    assert_eq!(then[0].position, first);
    let before = facts(
        &r,
        g,
        range,
        View::AsOfTime("2000-01-01T00:00:00Z".parse().unwrap()),
    );
    assert!(before.is_empty());

    block_on(s.retire(g, at(10))).unwrap();
    let ack = block_on(s.emit(&[num(g, 9, 1.0), num(g, 10, 1.0)])).unwrap();
    assert_eq!(ack.recorded(), 1);
    assert_eq!(refused(&ack), vec![(1, Refusal::Retired)]);
    assert!(s.series(g).unwrap().retired_at.is_some());
    assert!(matches!(s.series("a/b/c"), Err(QueryError::UnknownSeries)));
}

#[test]
fn the_fact_at_a_time() {
    let (s, clock) = store("at");
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let first = block_on(s.emit(&[num(g, 0, 1.0), num(g, 10, 2.0)]))
        .unwrap()
        .last()
        .unwrap();
    block_on(s.emit(&[num(g, 10, 3.0), num(g, 20, 9.0).with_quality(Quality::Bad)])).unwrap();
    clock.advance(Duration::from_secs(30));
    let r = s.reader();
    assert!(r.at(g, at(-1), View::Current).unwrap().is_none());
    assert_eq!(
        r.at(g, at(5), View::Current).unwrap().unwrap().number(),
        Some(1.0)
    );
    // Ignoring the staleness limit (S = 3 s); a bad fact is not creditable.
    let f = r.at(g, at(25), View::Current).unwrap().unwrap();
    assert_eq!((f.t, f.number()), (at(10), Some(3.0)));
    assert_eq!(
        r.at(g, at(25), View::AsOf(first))
            .unwrap()
            .unwrap()
            .number(),
        Some(2.0)
    );
    assert!(matches!(
        r.at(SeriesId(77), at(0), View::Current),
        Err(QueryError::UnknownSeries)
    ));
}

#[test]
fn forecast_series_carry_values_into_windows() {
    let (s, _clock) = store("forecast");
    let f = block_on(
        s.define_series(
            gauge("grid/area/price-forecast", 3_600)
                .timing(Timing::Forecast)
                .versioned(ChangedAtKind::Time),
        ),
    )
    .unwrap();
    // Issued now, about the next two hours; then re-issued for the first.
    let issue1 = start().as_nanos() as u64;
    let first = block_on(s.emit(&[
        num(f, 3_600, 10.0).changed_at(issue1),
        num(f, 7_200, 20.0).changed_at(issue1),
    ]))
    .unwrap()
    .last()
    .unwrap();
    block_on(s.emit(&[num(f, 3_600, 12.0).changed_at(issue1 + 1)])).unwrap();
    let r = s.reader();
    // A forecast series has no live state.
    assert!(matches!(r.live(f).unwrap(), Live::None));
    // The fact at a time: the version the view selects, with changed_at.
    let now = r.at(f, at(5_000), View::Current).unwrap().unwrap();
    assert_eq!(
        (now.number(), now.changed_at),
        (Some(12.0), Some(issue1 + 1))
    );
    let then = r.at(f, at(5_000), View::AsOf(first)).unwrap().unwrap();
    assert_eq!((then.number(), then.changed_at), (Some(10.0), Some(issue1)));
    // Buckets of 15 minutes from 01:00 to 02:00 after now: only the first
    // holds a reading; the others carry its value in. Min and max are over
    // readings only; the mean comes from the carried-in value.
    match r
        .buckets(
            f,
            at(3_600)..at(7_200),
            Resolution::Step(Duration::from_secs(900)),
        )
        .unwrap()
    {
        ChartData::Buckets { buckets, .. } => {
            assert_eq!(buckets.len(), 4);
            assert_eq!(buckets[0].min, Some(Value::F64(12.0)));
            assert_eq!(buckets[0].max, Some(Value::F64(12.0)));
            for b in &buckets[1..] {
                assert_eq!(b.count, 0);
                assert_eq!((&b.min, &b.max), (&None, &None));
                assert_eq!(b.mean, Some(12.0));
                assert_eq!(b.coverage, Duration::from_secs(900));
            }
        }
        other => panic!("{other:?}"),
    }
    // A window starting after the first value carries it in.
    let w = r
        .window(f, Window::absolute(at(5_400)..at(7_200)), Aggregate::Avg)
        .unwrap();
    assert_eq!(w.number(), Some(12.0));
    assert_eq!(w.count, 0);
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
    let series: Vec<_> = facts_of(&drain(&mut c))
        .into_iter()
        .map(|(f, _)| f.series)
        .collect();
    assert_eq!(series, vec![a, b]);
}

#[test]
fn restart_replays_then_resyncs() {
    let clock = ManualClock::new(start());
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    let g = block_on(s.define_series(gauge("plant/line-1/power", 1))).unwrap();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let fact_funnel = block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
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
    assert_eq!(s.consumer_stats("alarms").unwrap().cursor, first.position);

    // A second writer is refused while the first is open.
    assert!(matches!(
        block_on(Store::open(config("restart", &clock))),
        Err(OpenError::Locked)
    ));
    // Shutdown through a clone closes every clone.
    let other = s.clone();
    block_on(other.shutdown()).unwrap();
    // After a shutdown, `next` ends and `try_next` tells a clean close.
    assert!(block_on(c.next()).is_none());
    assert!(matches!(c.try_next(), Err(ConsumerError::Closed)));
    assert!(matches!(
        block_on(s.emit(&[num(g, 9, 1.0)])),
        Err(EmitError::Closed)
    ));
    assert!(matches!(s.health(), Health::Closed));
    drop((s, other, c));
    clock.advance(Duration::from_secs(60));
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let ds = drain(&mut c);
    let replayed: Vec<_> = facts_of(&ds)
        .into_iter()
        .map(|(f, flags)| {
            assert!(flags.replayed);
            f.number().unwrap()
        })
        .collect();
    assert_eq!(replayed, vec![6.0, 20.0]);
    let parts = resyncs(&ds);
    assert_eq!(parts.len(), 1);
    let resync = &parts[0];
    assert_eq!(resync.cause, ResyncCause::Resume);
    assert_eq!((resync.part, resync.last), (1, true));
    assert_eq!(resync.forget_above, None);
    let funnels: Vec<_> = resync.funnels.iter().map(|f| f.funnel).collect();
    assert_eq!(funnels, vec![fact_funnel, cond]);
    let fs = &resync.funnels[1];
    assert_eq!(fs.series, g);
    assert_eq!(fs.condition, Some(ConditionState::True));
    // The true transition was delivered but never confirmed, so the latch
    // does not hold it: the resync is a transition, to be acted on.
    assert!(fs.transition);
    match &fs.live {
        Live::Value(v) => assert_eq!(v.fact.number(), Some(20.0)),
        other => panic!("{other:?}"),
    }
    // The data survived the restart.
    let w = s
        .reader()
        .window(g, Window::absolute(at(0)..at(3)), Aggregate::Count)
        .unwrap();
    assert_eq!(w.number(), Some(3.0));
    // Dropping the last clone acts as a crash; the store opens again.
    drop(c);
    drop(s);
    let s = block_on(Store::open(config("restart", &clock))).unwrap();
    assert_eq!(s.series(g).unwrap().latest_t, Some(at(2)));
    // Everything was confirmed, the resync included, so the latch holds
    // true: the resync after this restart is no transition.
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    let parts = resyncs(&drain(&mut c));
    assert_eq!(parts[0].funnels[1].condition, Some(ConditionState::True));
    assert!(!parts[0].funnels[1].transition);
}

#[test]
fn a_large_resync_arrives_in_parts() {
    let (s, _clock) = store("resync-parts");
    let ids: Vec<SeriesId> = (0..300)
        .map(|i| block_on(s.define_series(gauge(&format!("site/s{i}/power"), 1))).unwrap())
        .collect();
    let c = block_on(s.consumer("wide", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(ids.clone())))).unwrap();
    block_on(s.emit(&[num(ids[0], 0, 1.0)])).unwrap();
    // The consumer process restarts: replay, then a resync of 300 funnel
    // states in two parts.
    drop(c);
    let mut c = block_on(s.consumer("wide", Start::Now, Select::None)).unwrap();
    let mut parts = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let DeliveryKind::Resync(p) = &d.kind {
            parts.push((d.id, p.clone()));
        }
    }
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].1.funnels.len(), 256);
    assert_eq!(parts[1].1.funnels.len(), 44);
    assert_eq!((parts[0].1.last, parts[1].1.last), (false, true));
    // Nothing further until the last part is confirmed; confirming an
    // earlier part does nothing.
    block_on(s.emit(&[num(ids[1], 1, 2.0)])).unwrap();
    assert!(c.try_next().unwrap().is_none());
    block_on(c.confirm(parts[0].0)).unwrap();
    assert!(c.try_next().unwrap().is_none());
    block_on(c.confirm(parts[1].0)).unwrap();
    let after = c.try_next().unwrap().unwrap();
    assert!(matches!(&after.kind, DeliveryKind::Fact(f) if f.series == ids[1]));
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
        Poll::Ready(Some(d)) => assert!(matches!(d.kind, DeliveryKind::Fact(_))),
        other => panic!("{other:?}"),
    }
}

#[test]
fn back_pressure_and_call_limits() {
    let clock = ManualClock::new(start());
    let mut cfg = config("back-pressure", &clock);
    cfg.limits.ingest_queue = 1_024;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    let many: Vec<_> = (0..1_025).map(|i| num(g, i, 1.0)).collect();
    assert!(matches!(
        s.try_emit(&many),
        Err(EmitError::TooLarge {
            readings: 1_025,
            ..
        })
    ));
    assert!(matches!(
        block_on(s.emit(&many)),
        Err(EmitError::TooLarge { .. })
    ));
    // Host-stamped readings wait while the clock is not synchronised.
    clock.set_synchronised(false);
    let host = [num(g, 0, 1.0).host_stamped()];
    assert!(matches!(
        s.try_emit(&host),
        Err(EmitError::Busy(Wait::UnsyncedClock))
    ));
    let device = block_on(s.try_emit(&[num(g, 1, 2.0)]).unwrap()).unwrap();
    assert_eq!(device.recorded(), 1);
    let woken = Arc::new(Flag::default());
    let mut emit = std::pin::pin!(s.emit(&host));
    assert!(poll_once(emit.as_mut(), &woken).is_pending());
    // The wait is named in statistics and health while it lasts.
    assert_eq!(s.stats().ingest.waiting, Some(Wait::UnsyncedClock));
    assert!(matches!(
        s.health(),
        Health::Degraded { reasons } if reasons == vec![Reason::IngestWaiting(Wait::UnsyncedClock)]
    ));
    clock.set_synchronised(true);
    s.tick().unwrap();
    assert!(woken.take());
    match poll_once(emit.as_mut(), &woken) {
        Poll::Ready(Ok(ack)) => assert_eq!(ack.recorded(), 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.stats().ingest.waiting, None);
    assert_eq!(s.stats().ingest.waits[&Wait::UnsyncedClock], 2);
}

#[test]
fn critical_disk_makes_ingest_wait() {
    let clock = ManualClock::new(start());
    let mut cfg = config("critical-disk", &clock);
    // Above the model's fast-disk capacity, so free space is always below it.
    cfg.limits.disk_critical = 2 << 40;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    assert!(matches!(
        s.try_emit(&[num(g, 0, 1.0)]),
        Err(EmitError::Busy(Wait::DiskFull))
    ));
    let woken = Arc::new(Flag::default());
    {
        let readings = [num(g, 0, 1.0)];
        let mut emit = std::pin::pin!(s.emit(&readings));
        assert!(poll_once(emit.as_mut(), &woken).is_pending());
        assert!(matches!(
            s.health(),
            Health::Degraded { reasons } if reasons.contains(&Reason::IngestWaiting(Wait::DiskFull))
        ));
    }
    // A dropped emit no longer waits.
    match s.health() {
        Health::Degraded { reasons } => {
            assert!(matches!(reasons.as_slice(), [Reason::DiskCritical { .. }]));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn interval_series_and_calendar_results() {
    let (s, clock) = store("calendar");
    let half_hour = Duration::from_secs(1_800);
    let energy = block_on(s.define_series(SeriesSpec::new(
        "site/meter/import",
        Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Fixed {
                d: half_hour,
                phase: Duration::ZERO,
            },
        },
        "kWh",
    )))
    .unwrap();
    let g = block_on(s.define_series(gauge("site/room/temperature", 60))).unwrap();
    let ack = block_on(s.emit(&[
        num(energy, -7_200, 1.5),
        num(energy, -5_400, 2.5),
        num(energy, -6_300, 9.0),
    ]))
    .unwrap();
    assert_eq!(refused(&ack), vec![(2, Refusal::OffPhase)]);
    let temps: Vec<_> = (0..=10).map(|m| num(g, -3_600 + 60 * m, 20.0)).collect();
    block_on(s.emit(&temps)).unwrap();
    clock.advance(Duration::from_secs(1));

    let r = s.reader();
    let jan5 = LocalDate::new(2026, 1, 5);
    assert_eq!(
        r.interval(energy, at(-6_300)).unwrap(),
        (at(-7_200), at(-5_400))
    );
    let day = r.calendar(energy, jan5, Aggregate::Sum).unwrap();
    assert_eq!(day.number(), Some(4.0));
    assert!(!day.flags.apportioned);
    let part = r
        .window(
            energy,
            Window::absolute(at(-6_300)..at(-3_600)),
            Aggregate::Sum,
        )
        .unwrap();
    assert_eq!(part.number(), Some(3.25));
    assert!(part.flags.apportioned);
    // Avg, integral, increase and time-in-state are refused on an interval
    // series (spec §6 Aggregates).
    assert!(matches!(
        r.window(energy, Window::absolute(at(-7_200)..at(0)), Aggregate::Avg),
        Err(QueryError::AggregateRefused { .. })
    ));

    let day = r.calendar(g, jan5, Aggregate::Avg).unwrap();
    assert_eq!(day.number(), Some(20.0));
    assert_eq!(day.count, 11);
    assert_eq!(day.from, "2026-01-05T00:00:00Z".parse().unwrap());
    let ny = r
        .calendar_in(
            g,
            jan5,
            &ZoneName::new("America/New_York"),
            Aggregate::Count,
        )
        .unwrap();
    assert_eq!(ny.from, "2026-01-05T05:00:00Z".parse().unwrap());
    assert_eq!(ny.number(), Some(11.0));
    let month = r
        .calendar(
            g,
            CalendarPeriod::Month {
                year: 2026,
                month: 1,
            },
            Aggregate::Count,
        )
        .unwrap();
    assert_eq!(month.from, "2026-01-01T00:00:00Z".parse().unwrap());
    assert_eq!(month.to, "2026-02-01T00:00:00Z".parse().unwrap());
    assert_eq!(month.number(), Some(11.0));
    assert!(matches!(
        r.calendar(g, LocalDate::new(2026, 2, 30), Aggregate::Count),
        Err(QueryError::Invalid { .. })
    ));

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
            assert_eq!(buckets[0].max, Some(Value::F64(20.0)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn state_and_text_series() {
    let (s, clock) = store("states");
    let mode = block_on(s.define_series(SeriesSpec::new(
        "hvac/unit/mode",
        Kind::State {
            values: StateValues::Named(vec!["off".into(), "heat".into(), "cool".into()]),
        },
        "",
    )))
    .unwrap();
    let note =
        block_on(s.define_series(SeriesSpec::new("hvac/unit/note", Kind::Text, ""))).unwrap();
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
    assert_eq!(refused(&ack), vec![(2, Refusal::StateOutOfRange)]);
    clock.advance(Duration::from_secs(30));
    let r = s.reader();
    let heat = r
        .window(
            mode,
            Window::absolute(at(0)..at(30)),
            Aggregate::TimeInState(Value::State(1)),
        )
        .unwrap();
    assert_eq!(heat.number(), Some(10.0));
    match r.live(mode).unwrap() {
        Live::Value(v) => assert_eq!(v.fact.value, Value::State(0)),
        other => panic!("{other:?}"),
    }
    let notes = facts(&r, note, at(0)..at(1), View::Current);
    assert_eq!(notes[0].value, Value::Text("filter changed".into()));
    assert_eq!(notes[0].quality, Quality::Estimated);
}

#[test]
fn unsupported_items_say_so() {
    let (s, _clock) = store("unsupported");
    let decimal = SeriesSpec::new(
        "a/b/decimal",
        Kind::Gauge {
            value: ValueType::decimal(),
        },
        "",
    );
    assert!(matches!(
        block_on(s.define_series(decimal)),
        Err(DefineError::NotSupportedYet { .. })
    ));
    let gas_day = SeriesSpec::new(
        "a/b/gas",
        Kind::Interval {
            value: ValueType::F64,
            length: IntervalLength::Calendar {
                unit: CalendarUnit::Day {
                    start: LocalTime::from_hms(5, 0, 0).unwrap(),
                },
                zone: ZoneName::new("Europe/London"),
            },
        },
        "",
    );
    assert!(matches!(
        block_on(s.define_series(gas_day)),
        Err(DefineError::NotSupportedYet { .. })
    ));
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    assert!(matches!(
        block_on(s.raise_scale(g, 6)),
        Err(DefineError::NotSupportedYet { .. })
    ));
    let change = ParameterChange::default().expected_interval(Duration::from_secs(5));
    assert!(matches!(
        block_on(s.set_parameters(g, at(0), change)),
        Err(DefineError::NotSupportedYet { .. })
    ));
    let retention = ParameterChange::default().retention(RetentionClass::Diagnostic {
        period: Duration::from_secs(86_400),
    });
    block_on(s.set_parameters(g, at(0), retention)).unwrap();
    assert!(matches!(
        block_on(s.consumer("late", Start::Position(Position::ORIGIN), Select::None)),
        Err(ConsumerError::NotSupportedYet { .. })
    ));
    let c = block_on(s.consumer("ops", Start::Now, Select::None)).unwrap();
    let operator_only = Funnel {
        series: Select::Ids(vec![g]),
        operator: Some(Operator::rolling(Aggregate::Avg, Duration::from_secs(60))),
        condition: None,
    };
    assert!(matches!(
        block_on(c.route(operator_only)),
        Err(ConsumerError::NotSupportedYet { .. })
    ));
    assert!(matches!(
        block_on(s.rebuild(kvik::Rebuild::Derived)),
        Err(BackupError::NotSupportedYet { .. })
    ));
    assert!(matches!(
        s.export_orphaned(Position::ORIGIN..=Position::ORIGIN),
        Err(QueryError::NotAvailable { .. })
    ));
}

#[test]
fn opening_rules() {
    let clock = ManualClock::new(start());
    assert!(matches!(
        Reader::open(ReaderConfig::new("kvik-test/never-opened")),
        Err(OpenError::NotAStore)
    ));
    assert!(matches!(
        block_on(Store::open(
            config("not-created", &clock).create_if_missing(false)
        )),
        Err(OpenError::NotAStore)
    ));
    let mut limits = kvik::Limits::default();
    limits.ingest_queue = 10;
    let small = config("small-queue", &clock).limits(limits);
    assert!(matches!(
        block_on(Store::open(small)),
        Err(OpenError::Config {
            key: "limits.ingest_queue",
            ..
        })
    ));
    let mut log = kvik::LogConfig::default();
    log.segment_size = 1 << 20;
    let segment = config("odd-segment", &clock)
        .log(log)
        .tiers(kvik::TierConfig::default());
    assert!(matches!(
        block_on(Store::open(segment)),
        Err(OpenError::Config {
            key: "log.segment_size",
            ..
        })
    ));
    let mut bad = config("zones", &clock);
    bad.time_zone = "Not/AZone".into();
    assert!(matches!(
        block_on(Store::open(bad)),
        Err(OpenError::UnknownZone)
    ));
    let s = block_on(Store::open(config("zones", &clock))).unwrap();
    let g = block_on(s.define_series(gauge("a/b/c", 1))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    let reader =
        Reader::open(ReaderConfig::new("kvik-test/zones").lease_refresh(Duration::from_secs(600)))
            .unwrap();
    assert_eq!(reader.series(g).unwrap().name, "a/b/c");
    let state = reader.refresh().unwrap();
    assert!(state.durable > Position::ORIGIN);
    block_on(s.shutdown()).unwrap();
    // Readers are independent of the writer (`docs/api.md` Handles and life
    // cycle): they keep reading what was committed after it shuts down.
    assert!(matches!(reader.live(g), Ok(Live::Value(_))));
    let mut other = config("zones", &clock);
    other.time_zone = "Europe/Oslo".into();
    assert!(matches!(
        block_on(Store::open(other)),
        Err(OpenError::SiteZoneMismatch { .. })
    ));
    let s = block_on(Store::open(config("zones", &clock))).unwrap();
    assert!(matches!(reader.live(g), Ok(Live::Value(_))));
    assert!(matches!(s.health(), Health::Ok));
    assert!(matches!(reader.health(), Health::Ok));
    assert!(s.losses().is_empty());
    block_on(s.acknowledge_losses(&[])).unwrap();
    block_on(s.acknowledge_manifest_rebuild(1)).unwrap();
    // One handle per consumer name at a time.
    let c = block_on(s.consumer("one", Start::Now, Select::None)).unwrap();
    assert!(matches!(
        block_on(s.consumer("one", Start::Now, Select::None)),
        Err(ConsumerError::InUse(_))
    ));
    drop(c);
    block_on(s.consumer("one", Start::Now, Select::None)).unwrap();
}

#[test]
fn site_zone_change_and_clock_jumps() {
    let (s, clock) = store("site-zone");
    assert!(matches!(
        block_on(s.set_site_zone(&ZoneName::new("Not/AZone"), at(0))),
        Err(DefineError::UnknownZone)
    ));
    block_on(s.set_site_zone(&ZoneName::new("Europe/Oslo"), at(86_400))).unwrap();
    drop(s);
    // Reopened, the zone in force is the new one.
    assert!(matches!(
        block_on(Store::open(config("site-zone", &clock))),
        Err(OpenError::SiteZoneMismatch { .. })
    ));
    let mut oslo = config("site-zone", &clock);
    oslo.time_zone = "Europe/Oslo".into();
    let s = block_on(Store::open(oslo)).unwrap();
    clock.jump_wall(Span::from_secs(-30));
    let stats = s.stats();
    assert_eq!(stats.clock.jumps, 1);
    assert_eq!(stats.clock.last.unwrap().by, Span::from_secs(-30));
    // The window clock never runs backwards.
    assert_eq!(s.now(), start());
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
fn consumer_before_any_series() {
    let (s, clock) = store("consumer-first");
    let mut c = block_on(s.consumer("early", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(c.route(Funnel::facts(Select::Names("site/*/power".into())))).unwrap();
    assert!(c.try_next().unwrap().is_none());
    let g = block_on(s.define_series(gauge("site/a/power", 1))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    clock.advance(Duration::from_secs(1));
    assert_eq!(
        facts_of(&drain(&mut c)).len(),
        1,
        "one fact delivery per position, whatever the funnels"
    );
    assert!(matches!(
        block_on(s.define_series(
            gauge("site/b/power", 1).retention(RetentionClass::Diagnostic {
                period: Duration::ZERO
            })
        )),
        Err(DefineError::Invalid {
            field: "retention",
            ..
        })
    ));
    assert!(matches!(
        s.series("site/b/power"),
        Err(QueryError::UnknownSeries)
    ));
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
            TimeRange::new(at(-3_600), at(0)),
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
struct ReadingWaker(Reader, SeriesId, Arc<Flag>);

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
    let v =
        block_on(s.define_series(gauge("grid/area/price", 1).versioned(ChangedAtKind::Version)))
            .unwrap();
    let mut c = block_on(s.consumer("prices", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![v])))).unwrap();
    let ack = block_on(s.emit(&[
        num(v, 0, 1.0),
        num(v, 0, 10.0).changed_at(2),
        num(v, 0, 5.0).changed_at(1),
    ]))
    .unwrap();
    assert_eq!(refused(&ack), vec![(0, Refusal::ChangedAtMissing)]);
    assert_eq!(ack.recorded(), 2);
    assert!(matches!(
        ack.outcomes[2],
        Outcome::Recorded {
            outranked: true,
            ..
        }
    ));
    let revise = block_on(s.emit_with(&[num(v, 0, 1.0).changed_at(3)], Intent::Revise)).unwrap();
    assert_eq!(refused(&revise), vec![(0, Refusal::ReviseOnVersioned)]);
    clock.advance(Duration::from_secs(1));
    let fs = facts_of(&drain(&mut c));
    assert_eq!(fs.len(), 2);
    assert!(!fs[0].1.outranked);
    assert!(
        fs[1].1.outranked,
        "a lower changed_at than the current fact"
    );
    let cur = facts(&s.reader(), v, at(0)..at(1), View::Current);
    assert_eq!(cur[0].number(), Some(10.0));
    assert_eq!(cur[0].changed_at, Some(2));
    assert_eq!(s.series_stats(v).unwrap().outranked_on_arrival, 1);
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
    assert_eq!(s.series_stats(g).unwrap().future, 1);
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
                .last()
                .unwrap(),
        );
    }
    assert!(s.consumer_stats("slow").unwrap().catching_up);
    let fs = facts_of(&drain(&mut c));
    let got: Vec<_> = fs.iter().map(|(f, _)| f.position).collect();
    assert_eq!(got, positions);
    assert!(
        fs.iter().any(|(_, fl)| fl.replayed),
        "some came from catch-up"
    );
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
        Err(ConsumerError::NotSupportedYet { .. })
    ));
    let c = block_on(s.consumer("quiet", Start::Now, Select::None)).unwrap();
    let ack = block_on(c.try_emit(&[num(g, 0, 1.0)]).unwrap()).unwrap();
    assert_eq!(refused(&ack), vec![(0, Refusal::NotDeclared)]);
    assert_eq!(s.series_stats(g).unwrap().refused[&Refusal::NotDeclared], 1);
}
