//! The handles and the futures they return can move between threads, as
//! `docs/api.md` implies (calls from different tasks, consumers and readers in
//! their own tasks).

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::{at, block_on, store};
use kvik::{
    Consumer, Delivery, DeliveryKind, Error, Funnel, Kind, Reader, Reading, Select, SeriesSpec,
    Start, Store, Value, ValueType,
};

fn assert_send_sync<T: Send + Sync>() {}
fn assert_send<T: Send>(_: &T) {}

#[test]
fn handles_and_results_are_send_and_sync() {
    assert_send_sync::<Store>();
    assert_send_sync::<Reader>();
    assert_send_sync::<Consumer>();
    assert_send_sync::<Delivery>();
    assert_send_sync::<Error>();
    assert_send_sync::<kvik::env::Env>();
    assert_send_sync::<kvik::StoreConfig>();
}

#[test]
fn futures_are_send() {
    let (s, clock) = store("send-futures");
    let config = common::config("send-futures-unused", &clock);
    assert_send(&Store::open(config));
    assert_send(&Reader::open("kvik-test/send-futures"));
    assert_send(&s.define_series(SeriesSpec::default()));
    let readings = [];
    assert_send(&s.emit(&readings));
    assert_send(&s.consumer("c", Start::Now));
    let mut c = block_on(s.consumer("c", Start::Now)).unwrap();
    assert_send(&c.route(Funnel::facts(Select::All)));
    assert_send(&c.next());
    let id = kvik::DeliveryId::Resync;
    assert_send(&c.confirm(id));
    assert_send(&s.shutdown());
}

#[test]
fn writers_and_a_consumer_on_several_threads() {
    let (s, clock) = store("threads");
    let id = block_on(s.define_series(SeriesSpec {
        name: "a/b/c".into(),
        kind: Kind::Gauge {
            value: ValueType::F64,
        },
        ..Default::default()
    }))
    .unwrap();
    let s = Arc::new(s);
    let mut c = block_on(s.consumer("counting", Start::Now)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![id])))).unwrap();
    let consumer = thread::spawn(move || {
        let mut seen = 0;
        while seen < 40 {
            let d = block_on(c.next()).expect("store open");
            if matches!(d.kind, DeliveryKind::Fact(_)) {
                seen += 1;
            }
            block_on(c.confirm(d.id)).unwrap();
        }
        seen
    });
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let s = s.clone();
            thread::spawn(move || {
                for i in 0..10 {
                    let r = Reading::new(id, at(w * 100 + i), Value::F64(i as f64));
                    block_on(s.emit(&[r])).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    assert_eq!(consumer.join().unwrap(), 40);
    clock.advance(Duration::from_secs(1));
    assert_eq!(s.stats().series[&id].facts, 40);
}
