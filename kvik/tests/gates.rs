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

/// The deliveries of one script of watches, readings and clock steps, as comparable lines: the backends' positions
/// differ, so each is shown as the number of readings emitted at or before it.
fn script(backend: Backend, name: &str) -> Vec<String> {
    let (s, clock) = store_on(backend, name);
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut emitted = Vec::new();
    let mut out = Vec::new();
    let drain = |c: &mut Consumer, emitted: &[kvik::Position], out: &mut Vec<String>| {
        let at = |p: kvik::Position| emitted.iter().filter(|e| **e <= p).count();
        while let Some(d) = c.try_next().unwrap() {
            if let kvik::DeliveryKind::Transition(t) = &d.kind {
                out.push(format!(
                    "{} @{} due={:?} {:?}->{:?} {:?} r={}",
                    t.name,
                    at(t.id.at),
                    t.id.due.map(|d| d.0 - common::start().0),
                    t.from,
                    t.to,
                    t.value,
                    d.flags.replayed
                ));
            }
            block_on(c.confirm(d.id)).unwrap();
        }
    };
    let emit = |v: f64, emitted: &mut Vec<kvik::Position>| {
        let a = block_on(s.emit(&[kvik::Reading::new(id, clock.wall(), Value::F64(v))])).unwrap();
        if let Some(kvik::Outcome::Recorded { position, .. }) = a.outcomes.first() {
            emitted.push(*position);
        }
    };
    emit(50.0, &mut emitted);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    drain(&mut c, &emitted, &mut out);
    clock.advance(Duration::from_secs(10));
    emit(75.0, &mut emitted);
    drain(&mut c, &emitted, &mut out);
    // The hold completes on the commit clock, then staleness 600 s after the last arrival.
    clock.advance(Duration::from_secs(130));
    drain(&mut c, &emitted, &mut out);
    clock.advance(Duration::from_secs(600));
    drain(&mut c, &emitted, &mut out);
    emit(90.0, &mut emitted);
    drain(&mut c, &emitted, &mut out);
    block_on(s.shutdown()).unwrap();
    out
}

#[test]
fn the_durable_backend_delivers_the_transitions_the_model_backend_does() {
    let model = script(Backend::Model, "gates-api-model");
    let durable = script(Backend::Durable, "gates-api-durable");
    assert_eq!(model.len(), 4, "{model:#?}");
    assert_eq!(durable, model);
}
