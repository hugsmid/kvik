//! A gate's held snapshot is never taken while transitions are deferred (design §4.3, G5; Q-2250).

mod common;

use std::time::Duration;

use common::block_on;
use common::world::gauge;
use kvik::{Backend, Funnel, Gate, Input, Select, Start, Store, Value};

fn run(name: &str, extra_batches: u32) -> (Vec<(kvik::TransitionId, bool)>, u64) {
    let clock = kvik::env::ManualClock::new(common::start());
    let mut limits = kvik::Limits::default();
    limits.consumer_queue = 2;
    let config = common::config_on(Backend::Durable, name, &clock).limits(limits);
    let s = block_on(Store::open(config)).unwrap();
    let a = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let b = block_on(s.define_series(gauge("hall/air/temperature"))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![b])))).unwrap();
    block_on(c.watch("a", Gate::change(Input::value(a)).deadband(5.0))).unwrap();
    while let Some(d) = c.try_next().unwrap() {
        block_on(c.confirm(d.id)).unwrap();
    }
    // Fill the queue with facts of the other series: the consumer is behind with nothing outstanding.
    for k in 0..4 {
        clock.advance(Duration::from_secs(1));
        block_on(s.emit(&[kvik::Reading::new(b, clock.wall(), Value::F64(f64::from(k)))])).unwrap();
    }
    // One transition judged while behind: deferred.
    clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[kvik::Reading::new(a, clock.wall(), Value::F64(50.0))])).unwrap();
    // Later batches that judge nothing new.
    for k in 0..extra_batches {
        clock.advance(Duration::from_secs(1));
        block_on(s.emit(&[kvik::Reading::new(b, clock.wall(), Value::F64(f64::from(k)))])).unwrap();
    }
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let kvik::DeliveryKind::Transition(t) = &d.kind {
            got.push((t.id, d.flags.replayed));
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    let dropped = s.stats().delivery.transitions_not_delivered;
    block_on(s.shutdown()).unwrap();
    (got, dropped)
}

#[test]
fn a_transition_deferred_while_behind_is_delivered_once_after_later_batches_judge_nothing() {
    let (got, dropped) = run("deferred-then-later-batches", 2);
    assert_eq!((got.len(), dropped), (1, 0), "{got:#?}");
}
