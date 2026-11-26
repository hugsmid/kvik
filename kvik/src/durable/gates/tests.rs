use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::super::Writer;
use super::super::routing::{ConsumerRoutes, GateRec, View};
use super::*;
use crate::config::{LogConfig, StoreConfig};
use crate::consume::{Select, Start};
use crate::env::{Env, ManualClock, MemFs};
use crate::gate::Input;
use crate::series::{Kind, SeriesSpec, ValueType};

fn open(name: &str) -> (Arc<Writer>, StoreConfig) {
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock);
    env.fs = Arc::new(MemFs::new());
    env.background_timers = false;
    let config = StoreConfig::new(PathBuf::from(format!("durable-unit/{name}")), "UTC")
        .create_if_missing(true)
        .env(env)
        .log(LogConfig {
            segment_size: 16 << 20,
            ..LogConfig::default()
        });
    (Writer::open(&config).unwrap(), config)
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
fn watch_validates_and_refuses_not_supported_yet_until_gates_are_live() {
    let (w, _) = open("gates-not-live");
    let s = gauge(&w, "hall/air/humidity");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
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
    let pump = Gate::set(Input::value_each(), [Value::F64(0.0)]).each("plant/*/code");
    assert_eq!(w.watch("c", "pump", &pump).unwrap(), GateId(2));
    w.unwatch("c", "humidity").unwrap();
    assert_eq!(w.gates("c").unwrap().len(), 1);
    assert_eq!(w.watch("c", "humidity", &humidity(s)).unwrap(), GateId(3));
    assert!(matches!(
        w.unwatch("c", "missing"),
        Err(Error::Gate(GateError::NoSuchGate))
    ));
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
