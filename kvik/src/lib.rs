//! kvik: an embedded time-series store for device and sensor data.
//!
//! This crate is the public interface described in `docs/api.md`, following the
//! rules of `docs/spec.md`. Applications can write adapters (which emit
//! readings) and consumers (which react to them) against it today.
//!
//! # Backends
//!
//! [`StoreConfig::backend`] chooses what stands behind a [`Store`]:
//!
//! - [`Backend::Durable`] (the default): the log engine. Every write is
//!   committed to the log in the store directory, on the file system of
//!   [`env::Env`], and acknowledged only once it is durable; a [`Reader`], in
//!   this process or another, reads up to the durable mark. It supports
//!   series definitions, writing, facts in every view, series lookup, live
//!   state, windows, chart buckets, calendar results and consumers so far;
//!   windowed operators in funnels return `NotSupportedYet`.
//! - [`Backend::Model`]: the executable reference model (`kvik-model`), an
//!   in-memory, exact and slow implementation of the whole interface. Nothing
//!   is written to disk: [`Store::open`] only uses the directory as a name,
//!   and within one process opening the same directory again after
//!   [`Store::shutdown`] (or after dropping the store, which acts as a crash)
//!   finds the store as it was left, so restarts, catch-up and `resync` can
//!   be exercised in tests.
//!
//! The crate `README.md` lists what each backend does not support yet and
//! every difference from `docs/api.md`.
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
//! `Store` calls that wait are `async` and runtime-neutral. The durable
//! backend runs the log's `kvik-writer` and `kvik-maint` threads, started by
//! [`Store::open`] and joined by [`Store::shutdown`]. The model backend
//! starts no threads, and every call completes at once except
//! [`Consumer::next`], which waits for a delivery, and [`Store::emit`], which
//! waits under back-pressure; its timers (window expiry, held-for and
//! stale-for conditions, scheduled values) advance only when the store is
//! called, and [`Store::tick`] is a temporary way to run them.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use kvik::env::{Env, ManualClock};
//! use kvik::{Aggregate, Backend, Kind, Reading, SeriesSpec, Store, StoreConfig, Value, ValueType, Window};
//!
//! # fn block_on<F: std::future::Future>(f: F) -> F::Output {
//! #     let mut f = std::pin::pin!(f);
//! #     let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
//! #     loop { if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
//! # }
//! # block_on(async {
//! let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
//! // Windows are not on the durable backend yet: this example runs on the model.
//! let config = StoreConfig::new("doc-example-store", "Europe/London")
//!     .create_if_missing(true)
//!     .backend(Backend::Model)
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

// The store's drivers and its manifest, wired by roadmap 8.1d and 8.3d.
use {kvik_backup as _, kvik_files as _};

mod backend;
mod backup;
mod config;
mod consume;
mod durable;
mod error;
mod fold;
#[cfg_attr(not(test), allow(dead_code))]
mod gates;
mod health;
mod pattern;
mod provisional;
mod read;
mod rules;
mod series;
mod store;
mod time;
mod types;

pub mod env;

pub use backup::{
    BackupSink, Encryption, ListPage, Rebuild, RestoreMode, RestoreScope, SinkError, SinkFuture,
    Stored, Verified, VerifyError,
};
pub use config::{
    Backend, DiskLow, Limits, LogConfig, MarkSync, ReaderConfig, StoreConfig, TierConfig,
    WINDOW_MEMORY_DESKTOP, WINDOW_MEMORY_MINIMAL,
};
pub use consume::{
    Condition, ConditionState, Delivery, DeliveryFlags, DeliveryId, DeliveryKind, Funnel, FunnelId,
    FunnelState, Operator, RESYNC_PART_FUNNELS, ResyncCause, ResyncPart, Select, Start, Trigger,
    TriggerCause,
};
pub use error::{
    BackupError, ConsumerError, DefineError, EmitError, Fault, OpenError, QueryError, Refusal,
    RouteError, Wait, WriteError,
};
pub use health::{
    BackupRefusal, BackupStats, ClockJump, ClockStats, ConsumerStats, DeliveryStats, DiskStats,
    Health, IngestStats, Latency, LogStats, LossCause, LossKind, LossReport, MarkState,
    MarkSyncMode, Reason, RestoreStats, SeriesStats, SetAsideReport, Stats,
};
pub use provisional::{
    Accepted, PROVISIONAL_MAX_BEHIND, Provisional, ProvisionalEvent, ProvisionalFact,
    ProvisionalOptions, Withdrawal,
};
pub use read::{
    Aggregate, Aggregated, Answer, Appended, Bucket, CalendarPeriod, CalendarWindow, ChartData,
    CloseReason, Facts, Live, LiveEvent, LiveId, LiveOptions, LiveValue, Part, Query, ReaderState,
    Resolution, ResultFlags, Selection, SessionOptions, Snapshot, UpdateBatch, Updated, View,
    Window, WindowResult,
};
pub use series::{
    CalendarUnit, ChangedAtKind, CounterRules, ExcessPrecision, IntervalLength, Kind,
    ParameterChange, RetentionClass, SeriesInfo, SeriesKey, SeriesSpec, StateValues, Timing,
    ValueType,
};
pub use store::{Consumer, LiveSession, Reader, Store};
pub use time::{LocalDate, LocalTime, ParseError, Span, TimeRange, Timestamp, ZoneName};
pub use types::{
    Ack, Fact, FactFlags, FactRow, Intent, Outcome, Pending, Position, Quality, Reading,
    ResetStamp, SavedPoint, SeriesId, TimeSource, Value,
};
