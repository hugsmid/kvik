//! Gates through the public interface (spec §6 Gates, `docs/api.md` Gates): ids, an equal spec writing nothing,
//! every `GateError` a call returns, the per-consumer limit, and the durable backend refusing until its engine
//! exists.

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

#[test]
fn the_durable_backend_validates_a_watch_and_refuses_not_supported_yet() {
    let (_s, c, id) = rig("gates-api-durable", Backend::Durable);
    assert!(matches!(
        block_on(c.watch("humidity", humidity(id))),
        Err(ConsumerError::NotSupportedYet { .. })
    ));
    assert_eq!(
        gate_err(block_on(c.watch("l", Gate::levels(Input::value(id), [])))),
        GateError::Levels
    );
    assert!(c.gates().unwrap().is_empty());
    assert!(matches!(
        block_on(c.unwatch("humidity")),
        Err(ConsumerError::Gate(GateError::NoSuchGate))
    ));
}
