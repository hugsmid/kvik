//! The model backend against `judge` directly: what a consumer receives through the interface is what
//! [`km::gate::judge`] computes from the model's log (Q-1140, Q-1583).

use std::path::PathBuf;
use std::time::Duration;

use proptest::prelude::*;

use super::*;
use crate::config::{Backend, StoreConfig};
use crate::consume::{Funnel, Select, Start};
use crate::env::{Env, ManualClock};
use crate::gate::{Input, Level};
use crate::series::SeriesSpec;
use crate::types::{Outcome, Reading, SeriesId};
use crate::{Consumer, Store};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: crate::series::ValueType::F64,
        },
        "%",
    )
    .expected_interval(Duration::from_secs(10))
}

#[derive(Clone, Debug)]
enum Op {
    /// A reading of series 0 or 1 after `dt` seconds.
    Emit { series: usize, dt: u64, v: f64 },
    /// The clock moves on `dt` seconds and the store's timers run.
    Tick { dt: u64 },
    /// One of the gates of [`gate`] watched under its name.
    Watch(usize),
    /// The gate under that name unwatched, if in force.
    Unwatch(usize),
}

/// Gates over series 0 with holds, staleness and hysteresis, so that timers and entry causes both occur.
fn gate(k: usize, s: SeriesId) -> Gate {
    match k {
        0 => Gate::levels(
            Input::value(s),
            [
                Level::above(50.0)
                    .clear(40.0)
                    .hold_up(Duration::from_secs(30)),
                Level::above(80.0)
                    .clear(75.0)
                    .hold_down(Duration::from_secs(20)),
            ],
        )
        .stale_after(Duration::from_secs(90)),
        1 => Gate::set(Input::value(s), [Value::F64(0.0), Value::F64(100.0)])
            .hold_on(Duration::from_secs(15))
            .hold_off(Duration::from_secs(5)),
        _ => Gate::change(Input::value(s))
            .deadband(10.0)
            .stale_after(Duration::from_secs(45)),
    }
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0usize..2, 1u64..40, prop_oneof![Just(0.0), Just(100.0), 0f64..100.0])
            .prop_map(|(series, dt, v)| Op::Emit { series, dt, v }),
        2 => (1u64..60).prop_map(|dt| Op::Tick { dt }),
        2 => (0usize..3).prop_map(Op::Watch),
        1 => (0usize..3).prop_map(Op::Unwatch),
    ]
}

struct Rig {
    store: Store,
    clock: ManualClock,
    consumer: Consumer,
    series: [SeriesId; 2],
    dir: PathBuf,
    got: Vec<Delivery>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let dir = PathBuf::from(format!("gates-oracle/{name}"));
        let config = StoreConfig::new(dir.clone(), "UTC")
            .create_if_missing(true)
            .backend(Backend::Model)
            .env(Env::test(clock.clone()));
        let store = block_on(Store::open(config)).unwrap();
        let a = block_on(store.define_series(gauge("hall/air/humidity"))).unwrap();
        let b = block_on(store.define_series(gauge("hall/air/other"))).unwrap();
        let consumer = block_on(store.consumer("c", Start::Now, Select::None)).unwrap();
        block_on(consumer.route(Funnel::facts(Select::Ids(vec![a, b])))).unwrap();
        Rig {
            store,
            clock,
            consumer,
            series: [a, b],
            dir,
            got: Vec::new(),
        }
    }

    fn apply(&mut self, op: &Op) {
        match *op {
            Op::Emit { series, dt, v } => {
                self.clock.advance(Duration::from_secs(dt));
                let r = Reading::new(self.series[series], self.clock.wall(), Value::F64(v));
                let ack = block_on(self.store.emit(&[r])).unwrap();
                assert!(matches!(ack.outcomes[0], Outcome::Recorded { .. }));
            }
            Op::Tick { dt } => {
                self.clock.advance(Duration::from_secs(dt));
                self.store.tick().unwrap();
            }
            Op::Watch(k) => {
                block_on(
                    self.consumer
                        .watch(&format!("g{k}"), gate(k, self.series[0])),
                )
                .unwrap();
            }
            Op::Unwatch(k) => {
                let name = format!("g{k}");
                if self
                    .consumer
                    .gates()
                    .unwrap()
                    .iter()
                    .any(|(_, n, _)| *n == name)
                {
                    block_on(self.consumer.unwatch(&name)).unwrap();
                }
            }
        }
        self.drain();
    }

    fn drain(&mut self) {
        while let Some(d) = self.consumer.try_next().unwrap() {
            block_on(self.consumer.confirm(d.id)).unwrap();
            self.got.push(d);
        }
    }

    /// `judge` over the model's log for the consumer, at the model's now, as the interface shows it.
    fn judged(
        &self,
    ) -> Vec<(
        TransitionId,
        Option<GateState>,
        Option<GateState>,
        TransitionCause,
    )> {
        let shared = crate::backend::find(&self.dir).unwrap();
        shared.with(|s| {
            let cid = s.consumers["c"].id;
            let history = s.model.gate_history(cid).unwrap();
            let j = km::gate::judge(&history, s.model.now).unwrap();
            j.transitions
                .iter()
                .map(|t| {
                    let d = s.transition_out("c", t);
                    (d.id, d.from, d.to, d.cause)
                })
                .collect()
        })
    }

    fn delivered(
        &self,
    ) -> Vec<(
        TransitionId,
        Option<GateState>,
        Option<GateState>,
        TransitionCause,
    )> {
        self.got
            .iter()
            .filter_map(|d| match &d.kind {
                DeliveryKind::Transition(t) => Some((t.id, t.from.clone(), t.to.clone(), t.cause)),
                _ => None,
            })
            .collect()
    }

    /// Transitions follow their position's fact and precede later ones; every delivery is in position order.
    fn check_order(&self) {
        for w in self.got.windows(2) {
            assert!(w[0].position <= w[1].position, "{:?} then {:?}", w[0], w[1]);
            if let (DeliveryKind::Transition(_), DeliveryKind::Fact(_)) = (&w[0].kind, &w[1].kind) {
                assert!(
                    w[0].position < w[1].position,
                    "a fact after a transition at its own position"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_model_backend_delivers_what_judge_computes_from_its_log(
        ops in prop::collection::vec(op(), 1..40),
        case in any::<u64>(),
    ) {
        let mut rig = Rig::new(&format!("prop-{case}"));
        for o in &ops {
            rig.apply(o);
        }
        rig.clock.advance(Duration::from_secs(600));
        rig.store.tick().unwrap();
        rig.drain();
        rig.check_order();
        prop_assert_eq!(rig.delivered(), rig.judged());
    }
}

#[test]
fn a_watch_starts_from_none_and_a_crossing_after_its_hold_is_a_timer_transition() {
    let mut rig = Rig::new("pinned-hold");
    rig.apply(&Op::Emit {
        series: 0,
        dt: 1,
        v: 10.0,
    });
    rig.apply(&Op::Watch(0));
    rig.apply(&Op::Emit {
        series: 0,
        dt: 1,
        v: 60.0,
    });
    rig.apply(&Op::Emit {
        series: 1,
        dt: 10,
        v: 1.0,
    });
    rig.apply(&Op::Tick { dt: 40 });
    let got = rig.delivered();
    assert_eq!(got, rig.judged());
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].1, None);
    assert_eq!(got[0].2, Some(GateState::Level(0)));
    assert!(matches!(got[0].3, TransitionCause::Entry { .. }));
    assert_eq!(got[1].2, Some(GateState::Level(1)));
    assert!(matches!(
        got[1].3,
        TransitionCause::Timer {
            kind: TimerKind::Hold,
            ..
        }
    ));
    // The timer's `at` is the last position of the whole log before its instant: the other series' fact.
    let other = rig
        .got
        .iter()
        .rev()
        .find(|d| matches!(&d.kind, DeliveryKind::Fact(f) if f.series == rig.series[1]))
        .map(|d| d.position)
        .unwrap();
    assert_eq!(got[1].0.at, other);
}

#[test]
fn a_timer_after_a_funnel_change_takes_that_entry_as_its_at() {
    let mut rig = Rig::new("pinned-other");
    rig.apply(&Op::Watch(0));
    rig.apply(&Op::Emit {
        series: 0,
        dt: 1,
        v: 60.0,
    });
    rig.clock.advance(Duration::from_secs(5));
    block_on(rig.consumer.route(Funnel::facts(Select::None))).unwrap();
    let route_at = rig.store.stats().log.durable;
    rig.apply(&Op::Tick { dt: 40 });
    let got = rig.delivered();
    assert_eq!(got, rig.judged());
    let timer = got
        .iter()
        .find(|t| matches!(t.3, TransitionCause::Timer { .. }))
        .unwrap();
    assert_eq!(timer.0.at, route_at);
}

#[test]
fn unconfirmed_transitions_come_again_flagged_replayed_after_a_restart_and_a_resync_carries_gate_states()
 {
    let mut rig = Rig::new("replay");
    rig.apply(&Op::Watch(2));
    rig.apply(&Op::Emit {
        series: 0,
        dt: 1,
        v: 30.0,
    });
    // One more transition, received and not confirmed.
    rig.clock.advance(Duration::from_secs(1));
    let r = Reading::new(rig.series[0], rig.clock.wall(), Value::F64(70.0));
    block_on(rig.store.emit(&[r])).unwrap();
    let mut pending = Vec::new();
    while let Some(d) = rig.consumer.try_next().unwrap() {
        if matches!(d.kind, DeliveryKind::Transition(_)) {
            pending.push(d);
        } else {
            block_on(rig.consumer.confirm(d.id)).unwrap();
        }
    }
    assert_eq!(pending.len(), 1);
    block_on(rig.store.shutdown()).unwrap();
    let config = StoreConfig::new(rig.dir.clone(), "UTC")
        .backend(Backend::Model)
        .env(Env::test(rig.clock.clone()));
    let store = block_on(Store::open(config)).unwrap();
    let mut c = block_on(store.consumer("c", Start::Now, Select::None)).unwrap();
    let mut again = Vec::new();
    let mut gates = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        match &d.kind {
            DeliveryKind::Transition(_) => again.push(d.clone()),
            DeliveryKind::Resync(p) => gates.extend(p.gates.clone()),
            _ => {}
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, pending[0].id);
    assert!(again[0].flags.replayed);
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].state, GateState::Value(Value::F64(70.0)));
    assert_eq!(
        Some(again[0].id),
        gates[0].set_by.map(DeliveryId::Transition)
    );
}
