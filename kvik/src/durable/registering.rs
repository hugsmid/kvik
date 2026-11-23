//! An emit leaves its batch to the `kvik-deliver` thread (Q-706, Q-1119), and a consumer registering during emits is
//! set up after every batch an emit left.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use super::pause::{Gate, Hold, Point};
use super::tests::wait;
use super::{Emitted, Writer};
use crate::config::{LogConfig, StoreConfig};
use crate::consume::{DeliveryKind, Funnel, Select, Start};
use crate::env::{Env, ManualClock, MemFs};
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::Timestamp;
use crate::types::{Intent, Outcome, Position, Reading, SeriesId, Value};

fn open(name: &str) -> (Arc<Writer>, SeriesId, PathBuf, Timestamp) {
    let (w, id, dir, t, _) = open_clock(name);
    (w, id, dir, t)
}

fn open_clock(name: &str) -> (Arc<Writer>, SeriesId, PathBuf, Timestamp, ManualClock) {
    let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
    let mut env = Env::test(clock.clone());
    env.fs = Arc::new(MemFs::new());
    let log = LogConfig {
        segment_size: 16 << 20,
        min_sync_interval: Duration::ZERO,
        ..LogConfig::default()
    };
    let dir = PathBuf::from(format!("durable-unit/registering/{name}"));
    let config = StoreConfig::new(&dir, "Europe/London")
        .create_if_missing(true)
        .env(env)
        .log(log);
    let w = Writer::open(&config).unwrap();
    let spec = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let id = w.define(&spec).unwrap();
    let t = clock.wall();
    (w, id, dir, t, clock)
}

fn emit(w: &Arc<Writer>, id: SeriesId, t: Timestamp, secs: i64) -> Position {
    let t = Timestamp(t.0 + secs * 1_000_000_000);
    let ack = match w.emit(
        &[Reading::new(id, t, Value::F64(1.0))],
        Intent::Normal,
        true,
    ) {
        Ok(Emitted::Queued(q)) => wait(q).unwrap(),
        Ok(Emitted::Ready(a)) => a,
        Err(e) => panic!("{e:?}"),
    };
    match ack.outcomes[0] {
        Outcome::Recorded { position, .. } => position,
        ref o => panic!("{o:?}"),
    }
}

/// The last position the deliverer has taken.
fn taken(w: &Writer) -> u64 {
    w.hub.as_ref().unwrap().run_quiet(|d, _| d.head().sequence)
}

fn facts(w: &Writer, name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    while let Some(d) = w.poll_delivery(name, None).unwrap() {
        match d.kind {
            DeliveryKind::Fact(_) => out.push(d.position.sequence),
            k => panic!("{k:?}"),
        }
    }
    out
}

#[derive(Default)]
struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn with_no_consumer_an_emit_returns_without_taking_its_batch() {
    let (w, id, dir, t) = open("none");
    let held = Hold::new(&dir, Point::DeliverThread);
    let p = emit(&w, id, t, 0);
    assert!(taken(&w) < p.sequence);
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(5);
    while taken(&w) < p.sequence {
        assert!(
            Instant::now() < deadline,
            "the delivery thread never took it"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    w.shutdown().unwrap();
}

#[test]
fn with_no_consumer_an_emit_never_waits_for_the_deliverer() {
    let (w, id, _dir, t) = open("unlocked");
    let hub = w.hub.clone().unwrap();
    let returned = hub.run_quiet(|_, _| {
        let (tx, rx) = std::sync::mpsc::channel();
        let w = w.clone();
        std::thread::spawn(move || tx.send(emit(&w, id, t, 0)).unwrap());
        rx.recv_timeout(Duration::from_secs(5)).is_ok()
    });
    assert!(returned, "the emit waited for the deliverer's lock");
    w.shutdown().unwrap();
}

#[test]
fn an_emit_while_a_consumer_is_set_up_leaves_its_batch_and_the_consumer_starts_after_it() {
    let (w, id, dir, t) = open("set-up");
    let held = Hold::new(&dir, Point::DeliverThread);
    let set_up = Gate::new(&dir, Point::TakeSetUp);
    let taker = {
        let w = w.clone();
        std::thread::spawn(move || w.take_consumer("c", Start::Now, &Select::None))
    };
    set_up.reached();
    let p = emit(&w, id, t, 0);
    assert!(taken(&w) < p.sequence);
    set_up.release();
    taker.join().unwrap().unwrap();
    w.route("c", &Funnel::facts(Select::All)).unwrap();
    let q = emit(&w, id, t, 1);
    assert_eq!(facts(&w, "c"), vec![q.sequence]);
    drop(held);
    w.shutdown().unwrap();
}

#[test]
fn an_emit_acknowledged_before_a_take_and_returning_after_it_has_taken_its_batch() {
    let (w, id, dir, t) = open("acknowledged");
    let acked = Gate::new(&dir, Point::AfterAck);
    let emitter = {
        let w = w.clone();
        std::thread::spawn(move || emit(&w, id, t, 0))
    };
    acked.reached();
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.route("c", &Funnel::facts(Select::All)).unwrap();
    acked.release();
    let p = emitter.join().unwrap();
    assert!(taken(&w) >= p.sequence);
    let q = emit(&w, id, t, 1);
    assert_eq!(facts(&w, "c"), vec![q.sequence]);
    w.shutdown().unwrap();
}

#[test]
fn a_consumer_taken_after_an_emit_left_its_batch_starts_at_or_after_that_batch() {
    let (w, id, dir, t) = open("left");
    let held = Hold::new(&dir, Point::DeliverThread);
    let p = emit(&w, id, t, 0);
    assert!(taken(&w) < p.sequence);
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    assert!(w.consumer_stats("c").unwrap().cursor >= p);
    drop(held);
    w.shutdown().unwrap();
}

#[test]
fn an_emit_waits_while_a_consumer_is_set_up_instead_of_leaving_its_batch() {
    let (w, id, dir, t) = open("setting-up");
    let setting_up = Gate::new(&dir, Point::SetUpPump);
    let taker = {
        let w = w.clone();
        std::thread::spawn(move || w.take_consumer("c", Start::Now, &Select::None))
    };
    setting_up.reached();
    let (tx, rx) = std::sync::mpsc::channel();
    let emitter = {
        let w = w.clone();
        std::thread::spawn(move || tx.send(emit(&w, id, t, 0)).unwrap())
    };
    assert!(
        rx.recv_timeout(Duration::from_secs(1)).is_err(),
        "an emit returned without its batch while a consumer was being set up"
    );
    setting_up.release();
    taker.join().unwrap().unwrap();
    let p = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    emitter.join().unwrap();
    assert!(taken(&w) >= p.sequence);
    w.shutdown().unwrap();
}

#[test]
fn with_a_consumer_an_emit_returns_before_its_batch_is_taken_and_the_delivery_thread_wakes_it() {
    let (w, id, dir, t) = open("consumer");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.route("c", &Funnel::facts(Select::All)).unwrap();
    let held = Hold::new(&dir, Point::DeliverThread);
    let woken = Arc::new(Woken::default());
    assert!(
        w.poll_delivery("c", Some(&Waker::from(woken.clone())))
            .unwrap()
            .is_none()
    );
    let p = emit(&w, id, t, 0);
    assert!(!woken.0.load(Ordering::SeqCst));
    assert!(taken(&w) < p.sequence);
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !woken.0.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the consumer was never woken");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(facts(&w, "c"), vec![p.sequence]);
    w.shutdown().unwrap();
}

#[test]
fn a_batch_read_back_after_the_clock_moved_is_taken_as_of_its_acknowledgement() {
    let (w, id, dir, t, clock) = open_clock("stamped");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.route("c", &Funnel::facts(Select::All)).unwrap();
    let held = Hold::new(&dir, Point::DeliverThread);
    let p = emit(&w, id, t, 0);
    let acked = w.clocks.now();
    clock.advance(Duration::from_secs(60));
    let hub = w.hub.clone().unwrap();
    let stamped = hub.run_quiet(|d, c| {
        d.pump(c);
        d.stamped()
    });
    assert!(taken(&w) >= p.sequence);
    assert_eq!(stamped.now, acked);
    drop(held);
    w.shutdown().unwrap();
}

#[test]
fn an_acknowledgement_noted_before_a_call_that_does_not_take_batches_still_times_its_batch() {
    let (w, id, dir, t, clock) = open_clock("stats-between");
    w.take_consumer("c", Start::Now, &Select::None).unwrap();
    w.route("c", &Funnel::facts(Select::All)).unwrap();
    let held = Hold::new(&dir, Point::DeliverThread);
    let p = emit(&w, id, t, 0);
    let acked = w.clocks.now();
    let _ = w.consumer_stats("c");
    clock.advance(Duration::from_secs(60));
    let stamped = w.hub.clone().unwrap().run_quiet(|d, c| {
        d.pump(c);
        d.stamped()
    });
    assert!(taken(&w) >= p.sequence);
    assert_eq!(stamped.now, acked);
    drop(held);
    w.shutdown().unwrap();
}
