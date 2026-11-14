//! The handles and the futures they return can move between threads, as
//! `docs/api.md` implies (calls from different tasks, consumers and readers in
//! their own tasks).

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::{at, block_on, store};
use kvik::{
    Consumer, ConsumerError, DefineError, Delivery, DeliveryKind, EmitError, Funnel, Kind,
    OpenError, Pending, QueryError, Reader, Reading, Select, SeriesSpec, Start, Store, Value,
    ValueType,
};

fn assert_send_sync<T: Send + Sync>() {}
fn assert_send<T: Send>(_: &T) {}

#[test]
fn handles_and_results_are_send_and_sync() {
    // `docs/api.md` Handles and life cycle: Store is Clone + Send + Sync,
    // Consumer is Send, Reader is Send + Sync.
    fn assert_clone<T: Clone>() {}
    assert_clone::<Store>();
    assert_send_sync::<Store>();
    assert_send_sync::<Reader>();
    assert_send_sync::<Consumer>();
    assert_send_sync::<Delivery>();
    assert_send_sync::<Pending>();
    assert_send_sync::<EmitError>();
    assert_send_sync::<OpenError>();
    assert_send_sync::<DefineError>();
    assert_send_sync::<QueryError>();
    assert_send_sync::<ConsumerError>();
    assert_send_sync::<kvik::env::Env>();
    assert_send_sync::<kvik::StoreConfig>();
}

#[test]
fn futures_are_send() {
    let (s, clock) = store("send-futures");
    let config = common::config("send-futures-unused", &clock);
    assert_send(&Store::open(config));
    assert_send(&s.define_series(SeriesSpec::default()));
    let readings = [];
    assert_send(&s.emit(&readings));
    assert_send(&s.consumer("c", Start::Now, Select::None));
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    assert_send(&c.route(Funnel::facts(Select::All)));
    assert_send(&c.next());
    let id = kvik::DeliveryId::Resync { part: 1 };
    assert_send(&c.confirm(id));
    assert_send(&s.shutdown());
}

#[test]
fn writers_and_a_consumer_on_several_threads() {
    let (s, clock) = store("threads");
    let id = block_on(s.define_series(SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "",
    )))
    .unwrap();
    let s = Arc::new(s);
    let mut c = block_on(s.consumer("counting", Start::Now, Select::None)).unwrap();
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
    assert_eq!(s.series_stats(id).unwrap().recorded, 40);
}
