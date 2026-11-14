//! kvik: an embedded time-series store for device and sensor data.
//!
//! This crate is the public interface described in `docs/api.md`, following the
//! rules of `docs/spec.md`. Applications can write adapters (which emit
//! readings) and consumers (which react to them) against it today.
//!
//! # Backend: in-memory, not durable, for development and tests
//!
//! **Nothing is written to disk yet.** With the default feature
//! `model-backend`, every [`Store`] is backed by the executable reference model
//! (`kvik-model`): an in-memory, exact and slow implementation of the
//! specification's semantics. All data lives in the memory of the current
//! process and is lost when the process ends. [`Store::open`] takes a
//! directory, but only uses it as a name: within one process, opening the same
//! directory again after [`Store::shutdown`] (or after dropping the store,
//! which acts as a crash) finds the store as it was left, so restarts,
//! catch-up and `resync` can be exercised in tests.
//!
//! The durable log engine (roadmap section 4 onwards) will replace the backend
//! behind the same interface. Items the reference model does not support yet
//! return a `NotSupportedYet` error (or, for reads, `QueryError::NotAvailable`);
//! the crate `README.md` lists them and every difference from `docs/api.md`.
//!
//! # Shape
//!
//! - [`Store`]: the writer (`Clone`; one per store directory): series
//!   definition, [`Store::emit`] and [`Store::try_emit`], consumers, health
//!   and statistics.
//! - [`Reader`]: blocking read-only queries (live state, windows, calendar
//!   results, facts in any view, the fact at a time, chart buckets).
//! - [`Consumer`]: funnels with optional operators and conditions; deliveries
//!   are received with [`Consumer::next`] or [`Consumer::try_next`] and
//!   confirmed with [`Consumer::confirm`].
//! - [`mod@env`]: the seams to the outside world (clock, file system, boot
//!   identity, tzdata, entropy), each with a production and a test
//!   implementation.
//!
//! Only std types appear in signatures: [`Timestamp`] and [`Span`] are
//! nanoseconds, configuration durations are `std::time::Duration`. The
//! optional feature `jiff` adds conversions to and from `jiff` 0.2.
//!
//! `Store` calls that wait are `async` and runtime-neutral; the model backend
//! starts no threads, and every call completes at once except
//! [`Consumer::next`], which waits for a delivery, and [`Store::emit`], which
//! waits under back-pressure. Timers (window expiry, held-for and stale-for
//! conditions, scheduled values) advance only when the store is called;
//! [`Store::tick`] is a temporary way to run them.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use kvik::env::{Env, ManualClock};
//! use kvik::{Aggregate, Kind, Reading, SeriesSpec, Store, StoreConfig, Value, ValueType, Window};
//!
//! # fn block_on<F: std::future::Future>(f: F) -> F::Output {
//! #     let mut f = std::pin::pin!(f);
//! #     let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
//! #     loop { if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
//! # }
//! # block_on(async {
//! let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
//! let config = StoreConfig::new("doc-example-store", "Europe/London")
//!     .create_if_missing(true)
//!     .env(Env::test(clock.clone()));
//! let store = Store::open(config).await.unwrap();
//!
//! let spec = SeriesSpec::new("plant/line-1/temperature", Kind::Gauge { value: ValueType::F64 }, "°C")
//!     .expected_interval(Duration::from_secs(1));
//! let id = store.define_series(spec).await.unwrap();
//!
//! let ack = store.emit(&[Reading::new(id, clock.wall(), Value::F64(21.5))]).await.unwrap();
//! assert_eq!(ack.recorded(), 1);
//! clock.advance(Duration::from_secs(1));
//!
//! let w = store.reader().window(id, Window::rolling(Duration::from_secs(1)), Aggregate::Avg).unwrap();
//! assert_eq!(w.number(), Some(21.5));
//! store.shutdown().await.unwrap();
//! # });
//! ```

#![forbid(unsafe_code)]

#[cfg(not(feature = "model-backend"))]
compile_error!(
    "kvik needs a backend: enable the `model-backend` feature (in-memory, not durable); \
     the durable log engine arrives from roadmap item 4.2"
);

#[cfg(feature = "model-backend")]
mod backend;
mod backup;
mod config;
mod consume;
mod error;
mod health;
mod read;
mod series;
#[cfg(feature = "model-backend")]
mod store;
mod time;
mod types;

pub mod env;

pub use backup::{BackupSink, Encryption, Rebuild, RestoreMode, Stored};
pub use config::{DiskLow, Limits, LogConfig, MarkSync, ReaderConfig, StoreConfig, TierConfig};
pub use consume::{
    Condition, ConditionState, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId,
    FunnelState, Operator, RESYNC_PART_FUNNELS, ResyncCause, ResyncPart, Select, Start, Trigger,
    TriggerCause,
};
pub use error::{
    BackupError, ConsumerError, DefineError, EmitError, Fault, OpenError, QueryError, Refusal,
    Wait, WriteError,
};
pub use health::{
    ClockJump, ClockStats, ConsumerStats, DiskStats, Health, IngestStats, Latency, LogStats,
    LossCause, LossKind, LossReport, MarkState, MarkSyncMode, Reason, SeriesStats, Stats,
};
pub use read::{
    Aggregate, Aggregated, Bucket, CalendarPeriod, CalendarWindow, ChartData, Facts, Live,
    LiveValue, ReaderState, Resolution, ResultFlags, View, Window, WindowResult,
};
pub use series::{
    CalendarUnit, ChangedAtKind, CounterRules, ExcessPrecision, IntervalLength, Kind,
    ParameterChange, RetentionClass, SeriesInfo, SeriesKey, SeriesSpec, StateValues, Timing,
    ValueType,
};
#[cfg(feature = "model-backend")]
pub use store::{Consumer, Reader, Store};
pub use time::{LocalDate, LocalTime, ParseError, Span, TimeRange, Timestamp, ZoneName};
pub use types::{
    Ack, Fact, FactFlags, FactRow, Intent, Outcome, Pending, Position, Quality, Reading, SeriesId,
    TimeSource, Value,
};
