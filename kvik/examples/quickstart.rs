//! Open a store, define a gauge and a counter, emit readings, react to a
//! threshold condition, and query a window and a calendar day.
//!
//! Run with `cargo run -p kvik --example quickstart`. The model backend keeps
//! everything in memory: nothing is written to disk.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::Duration;

use kvik::env::{Env, ManualClock};
use kvik::jiff::civil::date;
use kvik::jiff::{SignedDuration, Timestamp};
use kvik::{
    Aggregate, Condition, DeliveryKind, Funnel, Kind, Reading, Select, SeriesSpec, Start, Store,
    StoreConfig, Value, ValueType, Window,
};

/// A minimal executor: any async runtime works, as the store starts no
/// threads of its own.
fn block_on<F: Future>(f: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park();
    }
}

fn main() -> kvik::Result<()> {
    block_on(run())
}

async fn run() -> kvik::Result<()> {
    // A manual clock keeps the example deterministic; `Env::system()` (the
    // default) uses the system clocks.
    let start: Timestamp = "2026-03-02T10:00:00Z".parse().expect("a timestamp");
    let clock = ManualClock::new(start);
    let mut config = StoreConfig::new("quickstart-store", "Europe/London");
    config.env = Env::test(clock.clone());
    let store = Store::open(config).await?;
    println!("store opened; durable: {}", store.is_durable());

    let power = store
        .define_series(SeriesSpec {
            name: "plant/line-1/power".into(),
            kind: Kind::Gauge {
                value: ValueType::F64,
            },
            unit: "W".into(),
            expected_interval: Some(Duration::from_secs(1)),
            ..Default::default()
        })
        .await?;
    let energy = store
        .define_series(SeriesSpec {
            name: "plant/line-1/energy".into(),
            kind: Kind::Counter {
                value: ValueType::F64,
            },
            unit: "Wh".into(),
            expected_interval: Some(Duration::from_secs(1)),
            ..Default::default()
        })
        .await?;

    // A consumer with a threshold condition: true above 3 kW, false again
    // below 2.5 kW.
    let mut alarms = store
        .consumer("load-shedding", Start::Now, Select::None)
        .await?;
    alarms
        .route(Funnel {
            series: Select::Ids(vec![power]),
            operator: None,
            condition: Some(Condition::above(3_000.0).clear_below(2_500.0)),
        })
        .await?;

    // One reading a second for a minute; the load rises past 3 kW at 40 s.
    let mut meter = 0.0;
    for s in 0..60 {
        let t = start + SignedDuration::from_secs(s);
        let watts = if s < 40 { 2_000.0 } else { 3_200.0 };
        meter += watts / 3_600.0;
        let ack = store
            .emit(&[
                Reading::new(power, t, Value::F64(watts)),
                Reading::new(energy, t, Value::F64(meter)),
            ])
            .await?;
        assert!(ack.refused.is_empty());
        clock.advance(Duration::from_secs(1));
        while let Some(d) = alarms.try_next()? {
            if let DeliveryKind::Trigger(trigger) = &d.kind {
                println!("at {}s the condition is {:?}", s, trigger.state);
            }
            alarms.confirm(d.id).await?;
        }
    }

    let reader = store.reader();
    let avg = reader.window(
        power,
        Window::rolling(Duration::from_secs(60)),
        Aggregate::Avg,
    )?;
    println!(
        "average power over the last minute: {:.1} W (coverage {:.0} %)",
        avg.number().unwrap_or(f64::NAN),
        avg.coverage_fraction * 100.0
    );
    let used = reader.window(
        energy,
        Window::rolling(Duration::from_secs(60)),
        Aggregate::Increase,
    )?;
    println!(
        "energy used in the last minute: {:.2} Wh",
        used.number().unwrap_or(0.0)
    );
    let day = reader.calendar(energy, date(2026, 3, 2), Aggregate::Increase)?;
    println!(
        "energy on {} (London): {:.2} Wh, from {} to {}",
        date(2026, 3, 2),
        day.number().unwrap_or(0.0),
        day.from,
        day.to
    );

    store.shutdown().await
}
