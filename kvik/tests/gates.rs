//! Gates through the public interface (spec §6 Gates, `docs/api.md` Gates): ids, an equal spec writing nothing,
//! every `GateError` a call returns, the per-consumer limit, and the durable backend delivering as the model backend
//! does.

mod common;

use std::time::Duration;

use common::world::gauge;
use common::{block_on, store_on};
use kvik::{
    Backend, Consumer, ConsumerError, Gate, GateError, GateId, Input, Level,
    MAX_GATES_PER_CONSUMER, RouteError, Select, SeriesId, Start, Store, Value,
};

fn humidity(series: impl Into<kvik::SeriesKey>) -> Gate {
    Gate::levels(
        Input::value(series),
        [
            Level::above(70.0)
                .clear(65.0)
                .hold_up(Duration::from_secs(120)),
            Level::above(85.0).clear(80.0),
        ],
    )
    .stale_after(Duration::from_secs(600))
}

fn rig(name: &str, backend: Backend) -> (Store, Consumer, SeriesId) {
    let (s, _) = store_on(backend, name);
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    (s, c, id)
}

fn gate_err(r: Result<GateId, ConsumerError>) -> GateError {
    match r {
        Err(ConsumerError::Gate(e)) => e,
        other => panic!("expected a gate error, got {other:?}"),
    }
}

#[test]
fn watch_returns_new_ids_and_an_equal_spec_writes_nothing() {
    let (s, c, id) = rig("gates-api-equal", Backend::Model);
    let g1 = block_on(c.watch("humidity", humidity(id))).unwrap();
    let g2 = block_on(c.watch("pump", Gate::change(Input::value(id)).deadband(1.0))).unwrap();
    assert_eq!((g1, g2), (GateId(1), GateId(2)));
    let head = s.stats().log.durable;
    assert_eq!(block_on(c.watch("humidity", humidity(id))).unwrap(), g1);
    assert_eq!(
        block_on(c.watch("humidity", humidity("hall/air/humidity"))).unwrap(),
        g1
    );
    assert_eq!(s.stats().log.durable, head, "an equal spec writes nothing");
    let listed = c.gates().unwrap();
    assert_eq!(listed[0], (g1, "humidity".to_string(), humidity(id)));
    assert_eq!(listed.len(), 2);
    block_on(c.unwatch("humidity")).unwrap();
    assert!(s.stats().log.durable > head);
    assert_eq!(
        block_on(c.watch("humidity", humidity(id))).unwrap(),
        GateId(3)
    );
}

#[test]
fn every_gate_error_a_call_returns() {
    let (_s, c, id) = rig("gates-api-errors", Backend::Model);
    let w = |name: &str, g: Gate| gate_err(block_on(c.watch(name, g)));
    assert_eq!(
        w("l", Gate::levels(Input::value(id), [])),
        GateError::Levels
    );
    assert_eq!(
        w("s", Gate::set(Input::value(id), [Value::Absent])),
        GateError::Set
    );
    assert_eq!(
        w("d", Gate::change(Input::value(id)).deadband(f64::NAN)),
        GateError::Deadband
    );
    assert_eq!(
        w("t", humidity(id).stale_after(Duration::ZERO)),
        GateError::Duration
    );
    assert_eq!(
        w(
            "c",
            Gate::change(
                Input::window(id, Duration::from_secs(60), kvik::Aggregate::Avg)
                    .min_coverage(1_001)
            )
        ),
        GateError::Coverage
    );
    assert_eq!(
        w("e", humidity(id).each("hall/*/humidity")),
        GateError::Selector
    );
    assert_eq!(
        w("p", Gate::change(Input::value_each()).each("hall/a*/b")),
        GateError::Pattern(RouteError::PartialWildcard)
    );
    assert_eq!(w("a/b", humidity(id)), GateError::Name);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    assert_eq!(
        w("humidity", humidity(id).stale_after(Duration::from_secs(1))),
        GateError::NameInUse
    );
    assert!(matches!(
        block_on(c.unwatch("missing")),
        Err(ConsumerError::Gate(GateError::NoSuchGate))
    ));
    assert!(matches!(
        block_on(c.watch("x", humidity(SeriesId(999)))),
        Err(ConsumerError::UnknownSeries)
    ));
}

#[test]
fn a_consumer_holds_at_most_4096_gates() {
    let (_s, c, id) = rig("gates-api-limit", Backend::Model);
    for i in 0..MAX_GATES_PER_CONSUMER {
        block_on(c.watch(&format!("g{i}"), Gate::change(Input::value(id)))).unwrap();
    }
    assert_eq!(
        gate_err(block_on(
            c.watch("one-more", Gate::change(Input::value(id)))
        )),
        GateError::TooMany
    );
    // An equal spec under a name in force is still the gate in force at the limit.
    assert_eq!(
        block_on(c.watch("g0", Gate::change(Input::value(id)))).unwrap(),
        GateId(1)
    );
}

#[derive(Clone, Copy)]
enum Op {
    Emit(f64),
    Watch,
    Advance(u64),
    /// Steps the host's wall clock alone, by seconds either way.
    Jump(i64),
    Drain,
}

/// The transitions one script of watches, readings and clock steps delivers, as comparable lines: the backends'
/// positions differ, so each is shown as the number of readings emitted at or before it.
fn script(backend: Backend, name: &str, ops: &[Op]) -> Vec<String> {
    let (s, clock) = store_on(backend, name);
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut emitted: Vec<kvik::Position> = Vec::new();
    let mut out = Vec::new();
    for op in ops {
        match *op {
            Op::Emit(v) => {
                let a = block_on(s.emit(&[kvik::Reading::new(id, clock.wall(), Value::F64(v))]))
                    .unwrap();
                if let Some(kvik::Outcome::Recorded { position, .. }) = a.outcomes.first() {
                    emitted.push(*position);
                }
            }
            Op::Watch => {
                block_on(c.watch("humidity", humidity(id))).unwrap();
            }
            Op::Advance(secs) => clock.advance(Duration::from_secs(secs)),
            Op::Jump(secs) => clock.jump_wall(kvik::Span::from_secs(secs)),
            Op::Drain => {
                let at = |p: kvik::Position| emitted.iter().filter(|e| **e <= p).count();
                let mut got = Vec::new();
                while let Some(d) = c.try_next().unwrap() {
                    if let kvik::DeliveryKind::Transition(t) = &d.kind {
                        got.push(format!(
                            "{} @{} due={:?} {:?}->{:?} {:?} r={}",
                            t.name,
                            at(t.id.at),
                            t.id.due.map(|d| (d.0 - common::start().0) / 1_000_000_000),
                            t.from,
                            t.to,
                            t.value,
                            d.flags.replayed
                        ));
                    }
                    block_on(c.confirm(d.id)).unwrap();
                }
                out.push(got.join("; "));
            }
        }
    }
    block_on(s.shutdown()).unwrap();
    out
}

fn on_both(name: &str, ops: &[Op]) -> Vec<String> {
    let model = script(Backend::Model, &format!("{name}-model"), ops);
    let durable = script(Backend::Durable, &format!("{name}-durable"), ops);
    assert_eq!(durable, model);
    model
}

#[test]
fn the_durable_backend_delivers_the_transitions_the_model_backend_does() {
    use Op::*;
    let out = on_both(
        "gates-api-both",
        &[
            Emit(50.0),
            Watch,
            Drain,
            Advance(10),
            Emit(75.0),
            Drain,
            Advance(130),
            Drain,
            Advance(600),
            Drain,
            Emit(90.0),
            Drain,
        ],
    );
    assert!(
        out[2].contains("->Some(Level(1))") && out[3].contains("->Some(Stale)"),
        "{out:#?}"
    );
}

#[test]
fn a_forward_clock_step_completes_a_hold_and_fires_staleness_at_once() {
    use Op::*;
    let out = on_both(
        "gates-api-forward",
        &[Emit(50.0), Watch, Emit(75.0), Drain, Jump(86_400), Drain],
    );
    let stepped = &out[1];
    assert!(
        stepped.contains("->Some(Level(1))") && stepped.contains("->Some(Stale)"),
        "{stepped}"
    );
}

/// The model backend's commit times follow the wall clock back, which its judgement refuses, so this one is checked
/// against what `judge` gives on the durable log: every commit at or after the last, the hold due 120 s after it,
/// and the reading after the step older in event time than the live one.
#[test]
fn a_backward_clock_step_pauses_timers_until_the_clock_catches_up() {
    use Op::*;
    let out = script(
        Backend::Durable,
        "gates-api-backward",
        &[
            Emit(50.0),
            Watch,
            Emit(75.0),
            Drain,
            Jump(-3_600),
            Advance(130),
            Drain,
            Emit(80.0),
            Drain,
            Jump(3_600),
            Drain,
        ],
    );
    assert_eq!(
        out,
        [
            "humidity @1 due=None None->Some(Level(0)) Some(F64(50.0)) r=false",
            "",
            "",
            "humidity @3 due=Some(120) Some(Level(0))->Some(Level(1)) Some(F64(75.0)) r=false",
        ]
    );
}

#[test]
fn a_gate_that_overflows_its_queue_counts_what_it_drops_and_sends_the_present_state_on_reattaching()
{
    let clock = kvik::env::ManualClock::new(common::start());
    let mut limits = kvik::Limits::default();
    limits.consumer_queue = 2;
    let config = common::config_on(Backend::Durable, "gates-api-overflow", &clock).limits(limits);
    let s = block_on(Store::open(config)).unwrap();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.watch("moved", Gate::change(Input::value(id)).deadband(5.0))).unwrap();
    for k in 0..10 {
        clock.advance(Duration::from_secs(1));
        block_on(s.emit(&[kvik::Reading::new(
            id,
            clock.wall(),
            Value::F64(f64::from(k) * 10.0),
        )]))
        .unwrap();
    }
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let kvik::DeliveryKind::Transition(t) = &d.kind {
            got.push(t.clone());
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    assert!(s.stats().delivery.transitions_not_delivered > 0);
    assert!(got.windows(2).all(|w| w[0].id < w[1].id), "{got:#?}");
    assert_eq!(
        got.last().and_then(|t| t.to.clone()),
        Some(kvik::GateState::Value(Value::F64(90.0)))
    );
    assert!(got.len() < 11);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_transition_confirmed_before_an_overflow_does_not_come_again_after_a_second_take_while_quiet() {
    let clock = kvik::env::ManualClock::new(common::start());
    let mut limits = kvik::Limits::default();
    limits.consumer_queue = 2;
    let config =
        common::config_on(Backend::Durable, "gates-api-retake-quiet", &clock).limits(limits);
    let s = block_on(Store::open(config)).unwrap();
    let a = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let b = block_on(s.define_series(gauge("hall/air/temperature"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.watch("a", Gate::change(Input::value(a)).deadband(5.0))).unwrap();
    block_on(c.watch("b", Gate::change(Input::value(b)).deadband(5.0))).unwrap();
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[kvik::Reading::new(b, clock.wall(), Value::F64(1.0))])).unwrap();
    let mut confirmed = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let kvik::DeliveryKind::Transition(t) = &d.kind {
            confirmed.push(t.id);
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    assert!(!confirmed.is_empty());
    for k in 0..12 {
        clock.advance(Duration::from_secs(1));
        block_on(s.emit(&[kvik::Reading::new(
            a,
            clock.wall(),
            Value::F64(f64::from(k) * 10.0),
        )]))
        .unwrap();
    }
    assert!(s.stats().delivery.transitions_not_delivered > 0);
    drop(c);
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut again = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let kvik::DeliveryKind::Transition(t) = &d.kind
            && confirmed.contains(&t.id)
        {
            again.push((t.name.clone(), t.id, d.flags.replayed));
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    assert!(
        again.is_empty(),
        "confirmed transitions delivered again: {again:#?}"
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn every_start_reseeds_the_gates_with_no_replay_and_set_by_none_until_the_cursor_file_holds_the_gate_cursor()
 {
    let clock = kvik::env::ManualClock::new(common::start());
    let config = common::config_on(Backend::Durable, "gates-api-reseed", &clock);
    let s = block_on(Store::open(config.clone())).unwrap();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.watch("humidity", humidity(id))).unwrap();
    block_on(s.emit(&[kvik::Reading::new(id, clock.wall(), Value::F64(75.0))])).unwrap();
    // Handed out, not confirmed: a replay would send it again.
    while c.try_next().unwrap().is_some() {}
    drop(c);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = block_on(Store::open(config)).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut gates = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        match &d.kind {
            kvik::DeliveryKind::Transition(t) => panic!("replayed {t:?}"),
            kvik::DeliveryKind::Resync(r) => gates.extend(r.gates.clone()),
            _ => {}
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    assert_eq!(gates.len(), 1);
    assert_eq!(
        (gates[0].state.clone(), gates[0].set_by),
        (kvik::GateState::Level(0), None)
    );
    assert_eq!(s.stats().delivery.gates_reseeded, 1);
    block_on(s.shutdown()).unwrap();
}
