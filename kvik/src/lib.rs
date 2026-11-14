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
//! return [`Error::NotSupportedYet`]; the crate `README.md` lists them.
//!
//! # Shape
//!
//! - [`Store`]: the one writer handle per store directory: series definition,
//!   [`Store::emit`], consumers, health and statistics.
//! - [`Reader`]: read-only queries (live state, windows, calendar results,
//!   facts in any view, chart buckets).
//! - [`Consumer`]: funnels with optional operators and conditions; deliveries
//!   are received with [`Consumer::next`] or [`Consumer::try_next`] and
//!   confirmed with [`Consumer::confirm`].
//! - [`mod@env`]: the seams to the outside world (clock, file system, boot
//!   identity), each with a production and a test implementation.
//!
//! The interface is asynchronous, as in `docs/api.md`, but does not depend on
//! any runtime and starts no threads. With the model backend every call
//! completes at once, except [`Consumer::next`], which waits for a delivery,
//! and [`Store::emit`], which waits under back-pressure.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use kvik::env::{Env, ManualClock};
//! use kvik::{Kind, Reading, SeriesSpec, Store, StoreConfig, Value, ValueType, Window, Aggregate};
//!
//! # fn block_on<F: std::future::Future>(f: F) -> F::Output {
//! #     let mut f = std::pin::pin!(f);
//! #     let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
//! #     loop { if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
//! # }
//! # block_on(async {
//! let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
//! let mut config = StoreConfig::new("doc-example-store", "Europe/London");
//! config.env = Env::test(clock.clone());
//! let store = Store::open(config).await?;
//!
//! let id = store.define_series(SeriesSpec {
//!     name: "plant/line-1/temperature".into(),
//!     kind: Kind::Gauge { value: ValueType::F64 },
//!     unit: "°C".into(),
//!     expected_interval: Some(Duration::from_secs(1)),
//!     ..Default::default()
//! }).await?;
//!
//! let t = clock.wall();
//! store.emit(&[Reading::new(id, t, Value::F64(21.5))]).await?;
//! clock.advance(Duration::from_secs(1));
//!
//! let w = store.reader().window(id, Window::rolling(Duration::from_secs(1)), Aggregate::Avg)?;
//! assert_eq!(w.number(), Some(21.5));
//! store.shutdown().await?;
//! # Ok::<(), kvik::Error>(())
//! # }).unwrap();
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
mod types;

pub mod env;

pub use backup::{BackupSink, Encryption, Rebuild, RestoreMode, Stored};
pub use config::{Limits, LogConfig, StoreConfig, TierConfig};
pub use consume::{
    Condition, ConditionState, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId,
    FunnelState, Operator, Resync, Select, Start, Trigger, TriggerCause,
};
pub use error::{BusyReason, Error, Refusal, Result};
pub use health::{ConsumerStats, Health, HealthReason, SeriesStats, Stats};
pub use read::{
    Aggregate, Aggregated, Bucket, CalendarPeriod, CalendarWindow, ChartData, Live, LiveValue,
    Resolution, ResultFlags, View, Window, WindowResult,
};
pub use series::{
    CalendarUnit, ChangedAtKind, CounterRules, IntervalLength, Kind, ParameterChange,
    RetentionClass, SeriesInfo, SeriesKey, SeriesSpec, StateValues, Timing, ValueType,
};
#[cfg(feature = "model-backend")]
pub use store::{Consumer, Reader, Store};
pub use types::{
    Ack, Fact, FactFlags, Intent, Position, Quality, Reading, SeriesId, TimeSource, Value,
};

/// Re-exported so that callers use the same time types as the interface.
pub use jiff;
