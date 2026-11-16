//! `log.sync_interval_floor` reaches the durable backend's writer, and `Stats.log` reports the batch-sync interval
//! in force and the smoothed sync time (spec §3 Group commit; `api.md` Configuration and `LogStats`).

mod common;

use std::time::Duration;

use common::{at, block_on, start};
use kvik::env::{Env, ManualClock};
use kvik::{Backend, Kind, LogConfig, Reading, SeriesSpec, Store, StoreConfig, Value, ValueType};

/// A floor far above eight in-memory syncs (each copies a 16 MiB segment, tens of milliseconds), so that it is
/// what sets the interval.
const FLOOR: Duration = Duration::from_secs(1);
const CEILING: Duration = Duration::from_secs(2);

fn open(name: &str, backend: Backend, floor: Duration) -> Store {
    let mut log = LogConfig::default();
    log.segment_size = 16 << 20;
    log.min_sync_interval = CEILING;
    log.sync_interval_floor = floor;
    let config = StoreConfig::new(format!("sync-interval/{name}"), "Etc/UTC")
        .create_if_missing(true)
        .backend(backend)
        .env(Env::test(ManualClock::new(start())))
        .log(log);
    block_on(Store::open(config)).unwrap()
}

fn write(s: &Store) {
    let spec = SeriesSpec::new(
        "site/meter/power",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let id = block_on(s.define_series(spec)).unwrap();
    block_on(s.emit(&[Reading::new(id, at(1), Value::F64(1.0))])).unwrap();
}

#[test]
fn the_durable_backend_takes_the_floor_and_reports_the_interval_in_force() {
    for (name, floor) in [("adaptive", FLOOR), ("fixed", CEILING)] {
        let s = open(name, Backend::Durable, floor);
        write(&s);
        let log = s.stats().log;
        let d = log.sync_time.expect("a batch sync was timed");
        assert_eq!(
            log.sync_interval,
            (d * 8).clamp(floor, CEILING),
            "floor {floor:?}"
        );
        block_on(s.shutdown()).unwrap();
    }
}

#[test]
fn the_model_backend_reports_no_interval_and_no_sync_time() {
    let s = open("model", Backend::Model, FLOOR);
    write(&s);
    let log = s.stats().log;
    assert_eq!(log.sync_interval, Duration::ZERO);
    assert_eq!(log.sync_time, None);
    block_on(s.shutdown()).unwrap();
}
