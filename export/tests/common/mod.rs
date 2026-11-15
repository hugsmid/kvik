//! Helpers shared by the export tests: an executor, test stores and a bitwise row comparison.

#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kvik::env::{Env, ManualClock};
use kvik::{Backend, LogConfig, Position, Quality, SeriesId, Store, StoreConfig, Timestamp, Value};
use kvik_export::{
    ExportError, ExportOptions, ExportReader, ExportRow, Flags, KindName, SeriesSchema, TimingName,
    ValueKind, export_rows, read_rows,
};

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on this thread; panics after ten seconds.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "the future did not complete");
        thread::park_timeout(left);
    }
}

/// Noon UTC on a Monday in January.
pub fn start() -> Timestamp {
    "2026-01-05T12:00:00Z".parse().unwrap()
}

/// `start() + secs`.
pub fn at(secs: i64) -> Timestamp {
    Timestamp(start().0 + secs * 1_000_000_000)
}

/// A new store on a backend, on a manual clock at [`start`] and an in-memory file system.
pub fn store_on(backend: Backend, name: &str) -> (Store, ManualClock) {
    let clock = ManualClock::new(start());
    let dir = match backend {
        Backend::Model => format!("kvik-export-test/{name}"),
        _ => format!("kvik-export-test/durable/{name}"),
    };
    let mut log = LogConfig::default();
    log.segment_size = 16 << 20;
    let config = StoreConfig::new(dir, "Europe/London")
        .create_if_missing(true)
        .backend(backend)
        .env(Env::test(clock.clone()))
        .log(log);
    (block_on(Store::open(config)).unwrap(), clock)
}

/// Both backends.
pub const BACKENDS: [Backend; 2] = [Backend::Model, Backend::Durable];

/// An `f64` gauge schema for synthetic rows.
pub fn f64_series(id: u32) -> SeriesSchema {
    SeriesSchema {
        id: SeriesId(id),
        name: "site/meter/power".into(),
        unit: "W".into(),
        kind: KindName::Gauge,
        value: ValueKind::F64,
        changed_at: None,
        timing: TimingName::Observed,
        interval: None,
    }
}

/// A good row of series `id` at `secs` after [`start`], position `(0, seq)`.
pub fn row(id: u32, secs: i64, seq: u64, value: Value) -> ExportRow {
    ExportRow {
        series: SeriesId(id),
        t: at(secs),
        position: Position::new(0, seq),
        received_at: at(secs),
        value,
        quality: Quality::Good,
        supplied_quality: Quality::Good,
        flags: Flags::default(),
        changed_at: None,
    }
}

/// Exports rows to memory.
pub fn export_to_vec(
    series: &SeriesSchema,
    rows: &[ExportRow],
    options: &ExportOptions,
) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    export_rows(series, rows.iter().cloned().map(Ok), options, &mut out)?;
    Ok(out)
}

/// Opens an exported file.
pub fn open(file: &[u8]) -> ExportReader {
    read_rows(Bytes::copy_from_slice(file)).unwrap()
}

/// Every row of an exported file.
pub fn read_back(file: &[u8]) -> Vec<ExportRow> {
    open(file).collect::<Result<_, _>>().unwrap()
}

/// A value compared bitwise: `f64` by its bits, so `-0.0` differs from `0.0`.
#[derive(Debug, PartialEq)]
pub enum Bits {
    F64(u64),
    Other(Value),
}

/// Rows with their values made bitwise comparable.
pub fn bits(rows: &[ExportRow]) -> Vec<(ExportRow, Bits)> {
    rows.iter()
        .map(|r| {
            let v = match &r.value {
                Value::F64(x) => Bits::F64(x.to_bits()),
                v => Bits::Other(v.clone()),
            };
            (
                ExportRow {
                    value: Value::Absent,
                    ..r.clone()
                },
                v,
            )
        })
        .collect()
}
