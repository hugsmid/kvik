use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::super::Writer;
use super::super::routing::{ConsumerRoutes, GateRec, View};
use super::*;
use crate::config::{LogConfig, StoreConfig};
use crate::consume::{Select, Start};
use crate::env::{Env, ManualClock, MemFs};
use crate::gate::{GateState, Input};
use crate::series::{Kind, SeriesSpec, StateValues, ValueType};

fn open(name: &str) -> (Arc<Writer>, StoreConfig) {
    let (w, c, _) = open_clocked(name);
    (w, c)
}

fn open_clocked(name: &str) -> (Arc<Writer>, StoreConfig, ManualClock) {
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(MemFs::new());
    env.background_timers = false;
    let config = StoreConfig::new(PathBuf::from(format!("durable-unit/{name}")), "UTC")
        .create_if_missing(true)
        .env(env)
        .log(LogConfig {
            segment_size: 16 << 20,
            ..LogConfig::default()
        });
    (Writer::open(&config).unwrap(), config, clock)
}

fn gauge(w: &Writer, name: &str) -> SeriesId {
    let spec = SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "%",
    );
    w.define(&spec).unwrap()
}

fn humidity(series: impl Into<SeriesKey>) -> Gate {
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

fn head(w: &Writer) -> crate::types::Position {
    w.stats().log.durable
}

#[test]
fn watch_validates_first_and_refuses_not_supported_yet_with_gates_off() {
    let (w, _) = open("gates-not-live");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.forbid_gates();
    let before = head(&w);
    assert!(matches!(
        w.watch("c", "humidity", &humidity(s)),
        Err(Error::NotSupportedYet(_))
    ));
    // Validation comes first.
    assert!(matches!(
        w.watch("c", "a/b", &humidity(s)),
        Err(Error::Gate(GateError::Name))
    ));
    assert!(matches!(
        w.watch(
            "c",
            "humidity",
            &Gate::change(Input::value(s)).deadband(-1.0)
        ),
        Err(Error::Gate(GateError::Deadband))
    ));
    assert!(matches!(
        w.watch("c", "humidity", &humidity(SeriesId(99))),
        Err(Error::UnknownSeries(_))
    ));
    assert_eq!(head(&w), before, "nothing is written");
    assert!(w.gates("c").unwrap().is_empty());
}

#[test]
fn an_each_gate_is_refused_not_supported_yet_and_writes_nothing() {
    let (w, _) = open("gates-each");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    let before = head(&w);
    let pump = Gate::set(Input::value_each(), [Value::F64(0.0)]).each("plant/*/code");
    assert!(matches!(
        w.watch("c", "pump", &pump),
        Err(Error::NotSupportedYet(_))
    ));
    assert_eq!(head(&w), before);
}

#[test]
fn an_equal_spec_under_the_name_returns_the_id_in_force_and_writes_nothing() {
    let (w, config) = open("gates-equal");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.allow_gates();
    let id = w.watch("c", "humidity", &humidity(s)).unwrap();
    assert_eq!(id, GateId(1));
    let after = head(&w);
    // By name or by id, the same gate.
    assert_eq!(w.watch("c", "humidity", &humidity(s)).unwrap(), id);
    assert_eq!(
        w.watch("c", "humidity", &humidity("hall/air/humidity"))
            .unwrap(),
        id
    );
    assert_eq!(head(&w), after);
    // A different spec under the name is refused.
    assert!(matches!(
        w.watch(
            "c",
            "humidity",
            &humidity(s).stale_after(Duration::from_secs(60))
        ),
        Err(Error::Gate(GateError::NameInUse))
    ));
    assert_eq!(head(&w), after);
    let listed = w.gates("c").unwrap();
    assert_eq!(listed, vec![(id, "humidity".to_string(), humidity(s))]);
    // The entry survives a restart and is read back the same.
    w.release("c");
    w.shutdown().unwrap();
    drop(w);
    let w = Writer::open(&config).unwrap();
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    assert_eq!(w.gates("c").unwrap(), listed);
}

#[test]
fn unwatch_frees_the_name_and_gate_ids_are_never_reused() {
    let (w, _) = open("gates-unwatch");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.allow_gates();
    assert!(matches!(
        w.unwatch("c", "humidity"),
        Err(Error::Gate(GateError::NoSuchGate))
    ));
    assert_eq!(w.watch("c", "humidity", &humidity(s)).unwrap(), GateId(1));
    let pump = Gate::set(Input::value(s), [Value::F64(0.0)])
        .hold_on(Duration::from_secs(5))
        .hold_off(Duration::from_secs(9));
    assert_eq!(w.watch("c", "pump", &pump).unwrap(), GateId(2));
    let moved = Gate::change(Input::value(s)).deadband(0.5);
    assert_eq!(w.watch("c", "moved", &moved).unwrap(), GateId(3));
    w.unwatch("c", "humidity").unwrap();
    assert_eq!(
        w.gates("c").unwrap(),
        vec![
            (GateId(2), "pump".to_string(), pump),
            (GateId(3), "moved".to_string(), moved),
        ]
    );
    assert_eq!(w.watch("c", "humidity", &humidity(s)).unwrap(), GateId(4));
    assert!(matches!(
        w.unwatch("c", "missing"),
        Err(Error::Gate(GateError::NoSuchGate))
    ));
}

#[test]
fn a_named_state_member_is_checked_against_the_series_names_as_on_the_model() {
    let (w, _) = open("gates-state");
    let spec = SeriesSpec::new(
        "plant/pump/code",
        Kind::State {
            values: StateValues::Named(vec!["off".into(), "on".into(), "fault".into()]),
        },
        "",
    );
    let p = w.define(&spec).unwrap();
    let g = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.allow_gates();
    let set = |series: SeriesId, i| Gate::set(Input::value(series), [Value::State(i)]);
    assert_eq!(w.watch("c", "fault", &set(p, 2)).unwrap(), GateId(1));
    assert!(matches!(
        w.watch("c", "past", &set(p, 3)),
        Err(Error::Gate(GateError::Set))
    ));
    assert!(matches!(
        w.watch("c", "gauge", &set(g, 0)),
        Err(Error::NotSupportedYet(_))
    ));
    let each = Gate::set(Input::value_each(), [Value::State(0)]).each("plant/*/code");
    assert!(matches!(
        w.watch("c", "each", &each),
        Err(Error::NotSupportedYet(_))
    ));
    assert_eq!(w.gates("c").unwrap().len(), 1);
}

#[test]
fn window_inputs_are_not_supported_yet_on_the_durable_backend() {
    let (w, _) = open("gates-window");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.allow_gates();
    let g = Gate::levels(
        Input::window(s, Duration::from_secs(600), crate::read::Aggregate::Avg),
        [Level::above(1.0)],
    );
    assert!(matches!(
        w.watch("c", "w", &g),
        Err(Error::NotSupportedYet(_))
    ));
}

fn spec(name: &str) -> GateSpec {
    GateSpec {
        name: name.into(),
        selector: GateSelector::One(1),
        input: 1,
        kind: EntryKind::Change { deadband: 0.0 },
        stale_after: None,
    }
}

fn view_with(per_consumer: &[usize]) -> View {
    let mut v = View::default();
    for (k, n) in per_consumer.iter().enumerate() {
        let mut r = ConsumerRoutes::default();
        for i in 0..*n {
            let id = u32::try_from(i + 1).unwrap();
            r.gates.insert(
                id,
                GateRec {
                    watched_at: Default::default(),
                    spec: spec(&format!("g{i}")),
                },
            );
            r.max_gate = id;
        }
        v.consumers.insert(format!("c{k}"), r);
    }
    v
}

#[test]
fn the_gate_limits_admit_the_last_gate_and_refuse_one_more() {
    let n = MAX_GATES_PER_CONSUMER;
    assert_eq!(admit(&view_with(&[n - 1]), "c0").unwrap(), 4_096);
    assert!(matches!(
        admit(&view_with(&[n]), "c0"),
        Err(Error::Gate(GateError::TooMany))
    ));
    // 16 consumers of 4,096 fill the store: one more anywhere is refused.
    let mut full = vec![n; MAX_GATES_PER_STORE / n];
    full.push(0);
    assert!(matches!(
        admit(&view_with(&full), "c16"),
        Err(Error::Gate(GateError::TooMany))
    ));
    full[0] = n - 1;
    assert_eq!(admit(&view_with(&full), "c16").unwrap(), 1);
    assert_eq!(admit(&view_with(&[]), "new").unwrap(), 1);
}

fn emit(w: &Arc<Writer>, clock: &ManualClock, s: SeriesId, v: f64) {
    emit_all(
        w,
        &[crate::types::Reading::new(s, clock.wall(), Value::F64(v))],
    );
}

fn emit_all(w: &Arc<Writer>, rs: &[crate::types::Reading]) {
    match w.emit(rs, crate::types::Intent::Normal, true) {
        Ok(crate::durable::Emitted::Ready(_)) => {}
        Ok(crate::durable::Emitted::Queued(mut q)) => {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                if let std::task::Poll::Ready(r) = q.poll(&mut cx) {
                    q.finish(r).unwrap();
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Err(e) => panic!("{e:?}"),
    }
}

fn transitions(w: &Writer, confirm: bool) -> Vec<crate::gate::Transition> {
    let mut out = Vec::new();
    while let Some(d) = w.poll_delivery("c", None).unwrap() {
        if confirm {
            w.confirm("c", d.id).unwrap();
        }
        if let crate::consume::DeliveryKind::Transition(t) = d.kind {
            out.push(t);
        }
    }
    out
}

fn states(ts: &[crate::gate::Transition]) -> Vec<Option<crate::gate::GateState>> {
    ts.iter().map(|t| t.to.clone()).collect()
}

fn cursor(
    w: &Writer,
) -> (
    Option<super::super::deliver::GatePoint>,
    Option<crate::gates::Frontier>,
    usize,
) {
    w.hub().unwrap().run(|d, _| d.gate_cursor("c")).unwrap()
}

#[test]
fn a_confirmed_transition_or_resync_moves_the_gate_cursor_in_memory_and_drops_the_held_snapshot() {
    let (w, _, clock) = open_clocked("gates-confirm");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    emit(&w, &clock, s, 50.0);
    w.watch(
        "c",
        "humidity",
        &Gate::change(Input::value(s)).deadband(5.0),
    )
    .unwrap();
    clock.advance(Duration::from_secs(1));
    emit(&w, &clock, s, 60.0);
    let ts = transitions(&w, false);
    assert_eq!(ts.len(), 2);
    let (confirmed, held, outstanding) = cursor(&w);
    assert_eq!((confirmed, outstanding), (None, 2));
    // G5: held before the oldest outstanding transition was judged.
    let held = held.expect("a held snapshot while transitions are outstanding");
    assert!(held.at.sequence < ts[0].id.at.sequence);
    w.confirm("c", crate::consume::DeliveryId::Transition(ts[0].id))
        .unwrap();
    let (confirmed, held, outstanding) = cursor(&w);
    assert_eq!(outstanding, 1);
    assert!(held.is_some());
    assert!(matches!(
        confirmed,
        Some(super::super::deliver::GatePoint::Transition(id)) if id.at.sequence == ts[0].id.at.sequence
    ));
    w.confirm("c", crate::consume::DeliveryId::Transition(ts[1].id))
        .unwrap();
    let (_, held, outstanding) = cursor(&w);
    assert_eq!((held, outstanding), (None, 0));
    w.release("c");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    let at = head(&w);
    let mut resyncs = 0;
    while let Some(d) = w.poll_delivery("c", None).unwrap() {
        resyncs += usize::from(matches!(d.kind, crate::consume::DeliveryKind::Resync(_)));
        w.confirm("c", d.id).unwrap();
    }
    assert_eq!(resyncs, 1);
    assert!(matches!(
        cursor(&w).0,
        Some(super::super::deliver::GatePoint::Frontier(f)) if f.at.sequence == at.sequence
    ));
}

#[test]
fn no_timer_fires_while_a_batch_assigned_below_the_floor_is_not_yet_judged() {
    let (w, _, clock) = open_clocked("gates-race");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.watch("c", "humidity", &humidity(s)).unwrap();
    emit(&w, &clock, s, 75.0);
    assert_eq!(
        states(&transitions(&w, true)),
        [Some(GateState::Unknown), Some(GateState::Level(0))]
    );
    // A batch is in flight at the floor's position, past the head: the hold due at +120 s must wait for it.
    let hub = w.hub().unwrap().clone();
    let in_flight = hub.run(|d, _| {
        let h = d.head();
        kvik_log::writer::Position {
            generation: h.generation,
            sequence: h.sequence + 1,
        }
    });
    let due = clock.wall().0 + 200 * 1_000_000_000;
    hub.run(|d, _| d.hold_floor(Some((due, in_flight))));
    assert!(
        transitions(&w, true).is_empty(),
        "nothing before the batch in flight is judged"
    );
    // It commits below the hold's due instant and breaks the hold.
    clock.advance(Duration::from_secs(100));
    emit(&w, &clock, s, 50.0);
    hub.run(|d, _| d.hold_floor(None));
    clock.advance(Duration::from_secs(100));
    assert!(transitions(&w, true).is_empty(), "the hold never completed");
}

#[test]
fn a_pump_on_a_store_without_gates_leaves_the_commit_clock_at_the_last_batch() {
    let (w, _, clock) = open_clocked("gates-none-clock");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    emit(&w, &clock, s, 50.0);
    let committed = |w: &Writer| {
        w.hub()
            .unwrap()
            .run(|_, c| c.watch.map(|x| x.commit_clock().floor(i64::MIN).0))
    };
    let before = committed(&w);
    clock.advance(Duration::from_secs(100));
    assert!(transitions(&w, true).is_empty());
    assert_eq!(committed(&w), before);
}

#[test]
fn a_gate_call_failing_mid_batch_reseeds_at_its_item_and_repeats_no_fact() {
    let (w, _, clock) = open_clocked("gates-fail-mid-batch");
    let a = gauge(&w, "hall/air/humidity");
    let b = gauge(&w, "hall/air/temperature");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.route("c", &crate::consume::Funnel::facts(Select::All))
        .unwrap();
    w.watch("c", "a", &Gate::change(Input::value(a)).deadband(5.0))
        .unwrap();
    w.watch("c", "b", &Gate::change(Input::value(b)).deadband(5.0))
        .unwrap();
    emit(&w, &clock, a, 50.0);
    emit(&w, &clock, b, 1.0);
    transitions(&w, true);
    let h = head(&w);
    let second = kvik_log::writer::Position {
        generation: h.generation,
        sequence: h.sequence + 2,
    };
    let hub = w.hub().unwrap().clone();
    hub.run(|d, _| d.fail_gate_at(Some(second)));
    clock.advance(Duration::from_secs(1));
    let rs = [
        crate::types::Reading::new(a, clock.wall(), Value::F64(60.0)),
        crate::types::Reading::new(b, clock.wall(), Value::F64(20.0)),
    ];
    emit_all(&w, &rs);
    let mut facts = Vec::new();
    let mut gates = Vec::new();
    while let Some(d) = w.poll_delivery("c", None).unwrap() {
        match &d.kind {
            crate::consume::DeliveryKind::Fact(f) => facts.push(f.position),
            crate::consume::DeliveryKind::Resync(r) => gates.extend(r.gates.clone()),
            _ => {}
        }
        w.confirm("c", d.id).unwrap();
    }
    hub.run(|d, _| d.fail_gate_at(None));
    assert_eq!(facts.len(), 2, "{facts:?}");
    assert_eq!(
        gates.iter().map(|g| g.state.clone()).collect::<Vec<_>>(),
        [
            GateState::Value(Value::F64(60.0)),
            GateState::Value(Value::F64(20.0))
        ]
    );
}

#[test]
fn the_snapshot_held_for_a_transition_is_from_no_earlier_than_the_batch_before_its_judgement() {
    let (w, _, clock) = open_clocked("gates-held-fresh");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.watch(
        "c",
        "humidity",
        &Gate::change(Input::value(s)).deadband(5.0),
    )
    .unwrap();
    emit(&w, &clock, s, 50.0);
    assert_eq!(transitions(&w, true).len(), 2);
    for v in [51.0, 52.0, 53.0] {
        clock.advance(Duration::from_secs(1));
        emit(&w, &clock, s, v);
        assert!(transitions(&w, true).is_empty());
    }
    let quiet = head(&w);
    clock.advance(Duration::from_secs(1));
    emit(&w, &clock, s, 70.0);
    let ts = transitions(&w, false);
    assert_eq!(ts.len(), 1);
    let held = cursor(&w).1.expect("held while outstanding");
    assert_eq!(held.at.sequence, quiet.sequence);
}
