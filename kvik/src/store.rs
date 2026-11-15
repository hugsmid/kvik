//! The handles: [`Store`] (the writer), [`Reader`] and [`Consumer`]
//! (`docs/api.md` Handles and life cycle).

use std::future::{Future, poll_fn};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::backend::{self, Shared};
use crate::backup::{BackupSink, Encryption, Rebuild, RestoreMode};
use crate::config::Backend;
use crate::config::{ReaderConfig, StoreConfig};
use crate::consume::{Delivery, DeliveryId, Funnel, FunnelId, Select, Start};
use crate::durable::{self, DurableReader, Emitted};
use crate::error::{
    BackupError, ConsumerError, DefineError, EmitError, OpenError, QueryError, WriteError,
};
use crate::health::{ConsumerStats, Health, LossReport, SeriesStats, Stats};
use crate::read::{
    Aggregate, CalendarPeriod, CalendarWindow, ChartData, Facts, Live, ReaderState, Resolution,
    View, Window, WindowResult,
};
use crate::series::{ExcessPrecision, ParameterChange, SeriesInfo, SeriesKey, SeriesSpec};
use crate::time::{TimeRange, Timestamp, ZoneName};
use crate::types::{Ack, Fact, FactRow, Intent, Pending, Position, Reading, SeriesId};

/// The writer, shared by its clones; dropping the last one stops the store
/// as a crash would.
#[derive(Debug)]
struct Writer {
    shared: Arc<Shared>,
    epoch: u64,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.shared.with(|s| s.close(self.epoch));
    }
}

/// The writer handle: one per store directory (spec §3 Single writer).
///
/// `Store` is `Clone + Send + Sync`; clones share the one writer. Dropping
/// the last clone (consumers hold one) without [`Store::shutdown`] is
/// equivalent to a process crash: nothing acknowledged is lost (with the
/// model backend, within the process).
#[derive(Clone, Debug)]
pub struct Store {
    inner: StoreInner,
}

#[derive(Clone, Debug)]
enum StoreInner {
    Model(Arc<Writer>),
    Durable(Arc<durable::Writer>),
}

/// Runs `$model` with the model writer bound to `$m`, or `$durable` with the durable writer bound to `$d`.
macro_rules! by_backend {
    ($self:expr, $m:ident => $model:expr, $d:ident => $durable:expr) => {
        match &$self.inner {
            StoreInner::Model($m) => $model,
            StoreInner::Durable($d) => $durable,
        }
    };
}

impl Store {
    fn dir_path(&self) -> &Path {
        by_backend!(self, m => m.shared.dir(), d => d.dir())
    }

    /// Opens the store in `config.dir`, creating it when
    /// `config.create_if_missing` is set and the directory holds none, and
    /// returns once recovery has made it consistent.
    ///
    /// With the durable backend (the default) this is the log's start
    /// (spec §3 Order of a start) on `config.env`'s file system. **With the
    /// model backend nothing is written to disk:** the directory only names an
    /// in-memory store within this process, which is lost when the process
    /// ends; opening the same directory again after a shutdown restarts that
    /// store. Fails with [`OpenError::Locked`] while another writer holds the
    /// directory, and with [`OpenError::SiteZoneMismatch`] when
    /// `config.time_zone` is not the zone in force.
    pub async fn open(config: StoreConfig) -> Result<Store, OpenError> {
        let inner = match config.backend {
            Backend::Model => {
                let (shared, epoch) = backend::open(&config).map_err(|e| e.open(&config.dir))?;
                StoreInner::Model(Arc::new(Writer { shared, epoch }))
            }
            Backend::Durable => StoreInner::Durable(durable::Writer::open(&config)?),
        };
        Ok(Store { inner })
    }

    /// Whether the store is durable: `false` with the model backend.
    pub fn is_durable(&self) -> bool {
        matches!(self.inner, StoreInner::Durable(_))
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        self.dir_path()
    }

    /// The store's `now`: the window clock, which never runs backwards (spec
    /// §6).
    pub fn now(&self) -> Timestamp {
        by_backend!(self, m => m.shared.with(|s| s.now()), d => d.now())
    }

    /// Stops the writer and releases the directory: new calls on any clone
    /// return `Closed` and consumers' `next()` returns `None`. With the
    /// durable backend the calls already queued are committed, then the final
    /// mark sync; with the model backend the store stays in memory and
    /// readers keep reading it.
    pub async fn shutdown(&self) -> Result<(), WriteError> {
        by_backend!(self,
            m => {
                m.shared.with(|s| s.close(m.epoch));
                Ok(())
            },
            d => d.shutdown())
    }

    /// Defines a series, or returns the id of a live series defined the same
    /// way. A different definition under a live name is
    /// [`DefineError::Conflict`]: retire and define anew. Resolves after the
    /// mark sync that covers the definition (durable backend).
    pub async fn define_series(&self, spec: SeriesSpec) -> Result<SeriesId, DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared.with(|s| s.define(m.epoch, spec)).map_err(|e| e.define(dir)),
            d => d.define(&spec).map_err(|e| e.define(dir)))
    }

    /// Changes a series' parameters from an effective event time (spec
    /// §2.1). Both backends support only the retention class, which takes
    /// effect at its position.
    pub async fn set_parameters(
        &self,
        id: SeriesId,
        effective_from: Timestamp,
        change: ParameterChange,
    ) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared
                .with(|s| s.set_parameters(m.epoch, id, effective_from, change))
                .map_err(|e| e.define(dir)),
            d => d.set_parameters(id, &change).map_err(|e| e.define(dir)))
    }

    /// Raises a decimal series' scale from this entry's position on. Not
    /// supported yet by either backend, which have no decimal series.
    pub async fn raise_scale(&self, id: SeriesId, scale: u8) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared.with(|s| s.raise_scale(m.epoch, id, scale)).map_err(|e| e.define(dir)),
            d => d.decimal_only(id).map_err(|e| e.define(dir)))
    }

    /// Changes a decimal series' excess-precision policy from this entry's
    /// position on (spec §2.1 `excess precision`). Not supported yet by
    /// either backend, which have no decimal series.
    pub async fn set_excess_precision(
        &self,
        id: SeriesId,
        policy: ExcessPrecision,
    ) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared
                .with(|s| s.set_excess_precision(m.epoch, id, policy))
                .map_err(|e| e.define(dir)),
            d => d.decimal_only(id).map_err(|e| e.define(dir)))
    }

    /// Changes the site zone from an effective event time (a site-zone entry,
    /// spec §5.7); then change it in the configuration too.
    pub async fn set_site_zone(
        &self,
        zone: &ZoneName,
        effective_from: Timestamp,
    ) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared
                .with(|s| s.set_site_zone(m.epoch, zone.as_str(), effective_from))
                .map(|_| ())
                .map_err(|e| e.define(dir)),
            d => d.set_site_zone(zone, effective_from).map_err(|e| e.define(dir)))
    }

    /// Adds a name for a series (a rename); the id is unchanged.
    pub async fn alias(&self, id: SeriesId, name: &str) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared.with(|s| s.alias(m.epoch, id, name)).map_err(|e| e.define(dir)),
            d => d.alias(id, name).map_err(|e| e.define(dir)))
    }

    /// Retires a series at event time `t_r`: readings with `t ≥ t_r` are
    /// refused from then on; the series stays reachable by id.
    pub async fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<(), DefineError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared.with(|s| s.retire(m.epoch, id, t_r)).map_err(|e| e.define(dir)),
            d => d.retire(id, t_r).map_err(|e| e.define(dir)))
    }

    /// What the store knows about a series, by id or by name. With the
    /// durable backend: what is durable.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        let key = key.into();
        by_backend!(self,
            m => m.shared.with(|s| s.series(key)).map_err(|e| e.query()),
            d => d.series(&key))
    }

    /// Emits a call's readings as one batch and returns its outcomes once it
    /// is committed (synced, with the mark moved past it, with the durable
    /// backend; held in memory only with the model backend).
    ///
    /// Waits under back-pressure, as long as it lasts; the reason is in
    /// `stats().ingest.waiting` and health. With the model backend a waiting
    /// call is retried whenever the store is called again (an emit, a tick);
    /// a full disk there never clears. A caller that must not wait uses
    /// [`Store::try_emit`].
    pub async fn emit(&self, readings: &[Reading]) -> Result<Ack, EmitError> {
        self.emit_with(readings, Intent::Normal).await
    }

    /// [`Store::emit`] with an intent.
    pub async fn emit_with(&self, readings: &[Reading], intent: Intent) -> Result<Ack, EmitError> {
        match &self.inner {
            StoreInner::Model(m) => model_emit(m, readings, intent).await,
            StoreInner::Durable(d) => {
                let mut queued: Option<Pending> = None;
                let mut counted = false;
                poll_fn(|cx| {
                    if let Some(p) = queued.as_mut() {
                        return Pin::new(p).poll(cx);
                    }
                    match d.emit(readings, intent, true) {
                        Ok(Emitted::Ready(ack)) => Poll::Ready(Ok(ack)),
                        Ok(Emitted::Queued(q)) => {
                            let mut p = Pending::queued(q);
                            let r = Pin::new(&mut p).poll(cx);
                            queued = Some(p);
                            r
                        }
                        // Only an unsynchronised clock makes a waiting call busy: there is no event to wait
                        // for, so the call looks again shortly.
                        Err(EmitError::Busy(w)) => {
                            if !counted {
                                d.count_wait(w);
                                counted = true;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                        Err(e) => Poll::Ready(Err(e)),
                    }
                })
                .await
            }
        }
    }

    /// Emits without waiting: [`EmitError::Busy`] under back-pressure, with
    /// nothing queued. The returned [`Pending`] resolves to the call's
    /// outcomes and may be dropped. With the model backend the batch is
    /// committed before this returns.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError> {
        self.try_emit_with(readings, Intent::Normal)
    }

    fn try_emit_with(&self, readings: &[Reading], intent: Intent) -> Result<Pending, EmitError> {
        let dir = self.dir_path();
        match &self.inner {
            StoreInner::Model(m) => match m.shared.with(|s| {
                let r = s.emit(m.epoch, readings, intent);
                if let Ok(Err(w)) = &r {
                    s.note_wait(None, *w, true);
                }
                r
            }) {
                Ok(Ok(ack)) => Ok(Pending::ready(Ok(ack))),
                Ok(Err(w)) => Err(EmitError::Busy(w)),
                Err(e) => Err(e.emit(dir)),
            },
            StoreInner::Durable(d) => match d.emit(readings, intent, false) {
                Ok(Emitted::Ready(ack)) => Ok(Pending::ready(Ok(ack))),
                Ok(Emitted::Queued(q)) => Ok(Pending::queued(q)),
                Err(EmitError::Busy(w)) => {
                    d.count_wait(w);
                    Err(EmitError::Busy(w))
                }
                Err(e) => Err(e),
            },
        }
    }

    /// Runs the timer evaluations due by the clock now: window expiry,
    /// held-for and stale-for conditions, scheduled values taking effect
    /// (spec §4 Timers, §6 Cadence).
    ///
    /// **Temporary.** With the model backend timers advance only when the
    /// store is called: here, and before every emit and delivery poll. The
    /// durable backend has no timers to run yet. This call will go.
    pub fn tick(&self) -> Result<(), WriteError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared.with(|s| s.tick(m.epoch)).map_err(|e| e.write(dir)),
            _d => Ok(()))
    }

    /// A consumer by name. A new consumer starts at `start`; an existing one
    /// resumes after its cursor, with replay then a resync (spec §6 Restart
    /// and resume). One handle per name at a time.
    ///
    /// `emits` is the series the consumer may emit, so that routing can
    /// refuse a cycle (spec §4 Routing). The model backend accepts only
    /// [`Select::None`]; anything else is
    /// [`ConsumerError::NotSupportedYet`]. The durable backend has no
    /// consumers yet (roadmap 5.2).
    pub async fn consumer(
        &self,
        name: &str,
        start: Start,
        emits: Select,
    ) -> Result<Consumer, ConsumerError> {
        let dir = self.dir_path();
        match &self.inner {
            StoreInner::Model(m) => {
                m.shared
                    .with(|s| s.consumer(m.epoch, name, start, &emits))
                    .map_err(|e| e.consumer(dir))?;
                Ok(Consumer {
                    writer: m.clone(),
                    name: name.to_string(),
                })
            }
            StoreInner::Durable(_) => Err(ConsumerError::NotSupportedYet {
                what: "consumers and routing on the durable backend (roadmap 5.2)",
            }),
        }
    }

    /// A read-only handle on this store. With the durable backend it reads
    /// the log up to the durable mark, as a reader process does.
    pub fn reader(&self) -> Reader {
        Reader {
            inner: by_backend!(self,
                m => ReaderInner::Model(m.shared.clone()),
                d => ReaderInner::Store(d.clone())),
        }
    }

    /// Health (spec §11).
    pub fn health(&self) -> Health {
        by_backend!(self, m => m.shared.with(|s| s.health()), d => d.health())
    }

    /// Statistics (spec §11).
    pub fn stats(&self) -> Stats {
        by_backend!(self, m => m.shared.with(|s| s.stats()), d => d.stats())
    }

    /// Per-series statistics. With the durable backend the counts of
    /// recorded, late and future facts are rebuilt from the log at every
    /// open; the others count since the store was opened.
    pub fn series_stats(&self, id: SeriesId) -> Option<SeriesStats> {
        by_backend!(self, m => m.shared.with(|s| s.series_stats(id)), d => d.series_stats(id))
    }

    /// Per-consumer statistics (an addition of the model backend).
    pub fn consumer_stats(&self, name: &str) -> Option<ConsumerStats> {
        by_backend!(self, m => m.shared.with(|s| s.consumer_stats(name)), _d => None)
    }

    /// Every loss report, for ever.
    pub fn losses(&self) -> Vec<LossReport> {
        by_backend!(self, m => m.shared.with(|s| s.losses()), d => d.losses())
    }

    /// Acknowledges losses by their identities; a `DataLost` degrades health
    /// until acknowledged. With the durable backend the acknowledgement is
    /// held in memory until the store closes: the manifest's acknowledgement
    /// record is not written yet.
    pub async fn acknowledge_losses(&self, identities: &[Position]) -> Result<(), WriteError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared
                .with(|s| s.acknowledge_losses(m.epoch, identities))
                .map_err(|e| e.write(dir)),
            d => d.acknowledge_losses(identities))
    }

    /// Acknowledges a rebuilt manifest record (spec §7 Manifest). Neither
    /// backend rebuilds one yet.
    pub async fn acknowledge_manifest_rebuild(&self, epoch: u64) -> Result<(), WriteError> {
        let dir = self.dir_path();
        by_backend!(self,
            m => m.shared
                .with(|s| s.acknowledge_manifest_rebuild(m.epoch, epoch))
                .map_err(|e| e.write(dir)),
            d => d.acknowledge_manifest_rebuild())
    }

    /// Orphaned facts (spec §2.1), for re-ingest. Not available with the
    /// model backend.
    pub fn export_orphaned(
        &self,
        range: RangeInclusive<Position>,
    ) -> Result<Vec<Fact>, QueryError> {
        by_backend!(self,
            _m => Err(QueryError::NotAvailable {
                reason: "orphaned facts are not modelled".into(),
            }),
            d => d.export_orphaned(range))
    }

    /// Attaches a backup sink (spec §8). Not supported yet.
    pub async fn attach_backup<S: BackupSink>(
        &self,
        _sink: S,
        _key: Encryption,
    ) -> Result<(), BackupError> {
        Err(BackupError::NotSupportedYet { what: "backup" })
    }

    /// Restores a store onto an empty directory from a sink (spec §8
    /// Restore); returns the restored end. Not supported yet.
    pub async fn restore<S: BackupSink>(
        _dir: impl Into<PathBuf>,
        _sink: S,
        _key: Encryption,
        _mode: RestoreMode,
    ) -> Result<Position, BackupError> {
        Err(BackupError::NotSupportedYet { what: "restore" })
    }

    /// The facts a restore set aside in a range, each with its series name,
    /// for re-ingest (spec §8 Restore step 1). Not supported yet.
    pub fn export_set_aside(
        &self,
        _range: RangeInclusive<Position>,
    ) -> Result<Vec<(String, Fact)>, BackupError> {
        Err(BackupError::NotSupportedYet {
            what: "export of set-aside facts",
        })
    }

    /// Recomputes derived structures from the facts. Not supported yet.
    pub async fn rebuild(&self, _what: Rebuild) -> Result<(), BackupError> {
        Err(BackupError::NotSupportedYet { what: "rebuild" })
    }
}

/// An emit on the model backend: retried whenever the store is called again while it waits.
async fn model_emit(
    m: &Arc<Writer>,
    readings: &[Reading],
    intent: Intent,
) -> Result<Ack, EmitError> {
    let guard = WaitGuard::new(m.shared.clone());
    let mut first = true;
    let dir = m.shared.dir();
    poll_fn(|cx| {
        m.shared.with(|s| {
            let r = s.emit(m.epoch, readings, intent);
            match r {
                Ok(Ok(ack)) => {
                    s.clear_wait(guard.token);
                    Poll::Ready(Ok(ack))
                }
                Ok(Err(w)) => {
                    s.note_wait(Some(guard.token), w, first);
                    first = false;
                    s.register(cx.waker());
                    Poll::Pending
                }
                Err(e) => {
                    s.clear_wait(guard.token);
                    Poll::Ready(Err(e.emit(dir)))
                }
            }
        })
    })
    .await
}

/// Removes a waiting emit's entry when its future completes or is dropped.
struct WaitGuard {
    shared: Arc<Shared>,
    token: u64,
}

impl WaitGuard {
    fn new(shared: Arc<Shared>) -> Self {
        let token = shared.with(|s| s.wait_token());
        WaitGuard { shared, token }
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.shared.with(|s| s.clear_wait(self.token));
    }
}

/// A read-only handle (`docs/api.md` Reader processes, Reading). Calls are
/// blocking and take no lock on the directory; it keeps reading after the
/// writer shuts down. Reads see only committed data: durable, up to the mark,
/// with the durable backend; in memory with the model backend. `Send + Sync`
/// and cheap to clone.
#[derive(Clone, Debug)]
pub struct Reader {
    inner: ReaderInner,
}

#[derive(Clone, Debug)]
enum ReaderInner {
    Model(Arc<Shared>),
    /// A reader process on the durable log.
    Process(Arc<DurableReader>),
    /// The reader of a durable store's own process, opened on first use.
    Store(Arc<durable::Writer>),
}

impl Reader {
    /// A reader on the store in `config.dir`. With the model backend the
    /// store must have been opened in this process (else
    /// [`OpenError::NotAStore`]); with the durable backend it reads the
    /// directory on `config.env`'s file system, in this process or another.
    pub fn open(config: ReaderConfig) -> Result<Reader, OpenError> {
        let inner = match config.backend {
            Backend::Model => {
                ReaderInner::Model(backend::find(&config.dir).map_err(|e| e.open(&config.dir))?)
            }
            Backend::Durable => ReaderInner::Process(Arc::new(DurableReader::open(
                &config.dir,
                &config.env,
                config.lease_refresh,
            )?)),
        };
        Ok(Reader { inner })
    }

    fn durable(&self) -> Option<Result<Arc<DurableReader>, QueryError>> {
        match &self.inner {
            ReaderInner::Model(_) => None,
            ReaderInner::Process(r) => Some(Ok(r.clone())),
            ReaderInner::Store(w) => Some(w.reader()),
        }
    }

    /// Runs `model` on the model store, or `durable` on the durable reader.
    fn on<R>(
        &self,
        model: impl FnOnce(&Shared) -> Result<R, QueryError>,
        durable: impl FnOnce(&DurableReader) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        match &self.inner {
            ReaderInner::Model(s) => model(s),
            _ => durable(&*self.durable().expect("a durable reader")?),
        }
    }

    /// Re-reads the mark, the manifest tail and the new entries, and renews
    /// the lease.
    pub fn refresh(&self) -> Result<ReaderState, QueryError> {
        self.on(
            |s| s.with(|s| s.refresh()).map_err(|e| e.query()),
            durable::refresh,
        )
    }

    /// Live state: the latest valid value, with staleness (spec §2.3).
    pub fn live(&self, id: SeriesId) -> Result<Live, QueryError> {
        self.on(
            |s| s.with(|s| s.live(id)).map_err(|e| e.query()),
            |r| durable::live(r, id),
        )
    }

    /// A window result (spec §6).
    pub fn window(
        &self,
        id: SeriesId,
        window: Window,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        self.on(
            |s| {
                s.with(|s| s.window(id, &window, &aggregate))
                    .map_err(|e| e.query())
            },
            |r| durable::window(r, id, &window, &aggregate),
        )
    }

    /// A chart query: raw facts or buckets with count, min, max, mean and
    /// coverage.
    pub fn buckets(
        &self,
        id: SeriesId,
        range: impl Into<TimeRange>,
        resolution: Resolution,
    ) -> Result<ChartData, QueryError> {
        let range = range.into();
        self.on(
            |s| {
                s.with(|s| s.buckets(id, range, resolution))
                    .map_err(|e| e.query())
            },
            |r| durable::buckets(r, id, range, resolution),
        )
    }

    /// Facts with `t` in a range, in a view (spec §2.3).
    pub fn facts(
        &self,
        id: SeriesId,
        range: impl Into<TimeRange>,
        view: View,
    ) -> Result<Facts, QueryError> {
        let range = range.into();
        self.on(
            |s| s.with(|s| s.facts(id, range, view)).map_err(|e| e.query()),
            |r| durable::facts(r, id, range, view),
        )
    }

    /// The latest fact at or before `t` that a window can credit (not bad
    /// quality, not displaced), in a view, ignoring the staleness limit; for
    /// a versioned series the version the view selects.
    pub fn at(
        &self,
        id: SeriesId,
        t: Timestamp,
        view: View,
    ) -> Result<Option<FactRow>, QueryError> {
        self.on(
            |s| s.with(|s| s.at(id, t, view)).map_err(|e| e.query()),
            |r| durable::at(r, id, t, view),
        )
    }

    /// A calendar day or month result in the site zone: a window over its
    /// span (spec §5.7).
    pub fn calendar(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        let window = Window::Calendar {
            window: CalendarWindow::Period(period.into()),
            zone: None,
        };
        self.window(id, window, aggregate)
    }

    /// A calendar day or month result in a named IANA zone for the whole
    /// span (spec §5.7).
    pub fn calendar_in(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        zone: &ZoneName,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        let window = Window::Calendar {
            window: CalendarWindow::Period(period.into()),
            zone: Some(zone.clone()),
        };
        self.window(id, window, aggregate)
    }

    /// The interval of an interval series that contains `t`: `[start, end)`.
    pub fn interval(
        &self,
        id: SeriesId,
        t: Timestamp,
    ) -> Result<(Timestamp, Timestamp), QueryError> {
        self.on(
            |s| s.with(|s| s.interval(id, t)).map_err(|e| e.query()),
            |r| durable::interval(r, id, t),
        )
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        let key = key.into();
        self.on(
            |s| s.with(|s| s.series(key.clone())).map_err(|e| e.query()),
            |r| r.with(|i, _| i.series(&key)),
        )
    }

    /// What the files report: loss reports, not the writer's counters.
    pub fn health(&self) -> Health {
        let losses: Vec<_> = self
            .losses()
            .into_iter()
            .filter(|l| !l.acknowledged && l.kind == crate::health::LossKind::DataLost)
            .map(crate::health::Reason::DataLost)
            .collect();
        if losses.is_empty() {
            Health::Ok
        } else {
            Health::Degraded { reasons: losses }
        }
    }

    /// Every loss report, for ever.
    pub fn losses(&self) -> Vec<LossReport> {
        match &self.inner {
            ReaderInner::Model(s) => s.with(|s| s.losses()),
            ReaderInner::Store(w) => w.losses(),
            ReaderInner::Process(r) => r.losses().unwrap_or_default(),
        }
    }
}

/// A consumer: its funnels, deliveries and confirmations (spec §4). `Send`,
/// not `Clone`; it holds a share of the store. Only the model backend has
/// consumers yet.
///
/// Deliveries are at least once: de-duplicate fact deliveries by position and
/// condition deliveries by funnel and sequence. A consumer that emits
/// readings uses [`Consumer::try_emit`], never the store's handle.
#[derive(Debug)]
pub struct Consumer {
    writer: Arc<Writer>,
    name: String,
}

impl Consumer {
    fn shared(&self) -> &Shared {
        &self.writer.shared
    }

    fn epoch(&self) -> u64 {
        self.writer.epoch
    }

    /// The consumer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a funnel; a log entry that takes effect at its position.
    pub async fn route(&self, funnel: Funnel) -> Result<FunnelId, ConsumerError> {
        self.shared()
            .with(|s| s.route(self.epoch(), &self.name, funnel))
            .map_err(|e| e.consumer(self.writer.shared.dir()))
    }

    /// Removes a funnel; a log entry that takes effect at its position.
    pub async fn unroute(&self, funnel: FunnelId) -> Result<(), ConsumerError> {
        self.shared()
            .with(|s| s.unroute(self.epoch(), &self.name, funnel))
            .map_err(|e| e.consumer(self.writer.shared.dir()))
    }

    /// The funnels in force.
    pub fn routes(&self) -> Result<Vec<(FunnelId, Funnel)>, ConsumerError> {
        self.shared()
            .with(|s| s.routes(self.epoch(), &self.name))
            .map_err(|e| e.consumer(self.writer.shared.dir()))
    }

    /// The next delivery, waiting for one; `None` when no more will come.
    /// After `None`, call [`Consumer::try_next`]: `Err(ConsumerError::Closed)`
    /// is a clean close, anything else a fault.
    ///
    /// With the model backend nothing runs in the background: a waiting call
    /// is woken by the next emit, routing change, confirmation or
    /// [`Store::tick`].
    pub async fn next(&mut self) -> Option<Delivery> {
        poll_fn(|cx| {
            self.shared()
                .with(|s| match s.poll_delivery(self.epoch(), &self.name) {
                    Ok(Some(d)) => Poll::Ready(Some(d)),
                    Ok(None) => {
                        s.register(cx.waker());
                        Poll::Pending
                    }
                    Err(_) => Poll::Ready(None),
                })
        })
        .await
    }

    /// The next delivery if one is ready, without waiting.
    pub fn try_next(&mut self) -> Result<Option<Delivery>, ConsumerError> {
        self.shared()
            .with(|s| s.poll_delivery(self.epoch(), &self.name))
            .map_err(|e| e.consumer(self.writer.shared.dir()))
    }

    /// Confirms a delivery and every delivery received before it. A resync is
    /// confirmed by its last part; nothing further is delivered until then.
    pub async fn confirm(&self, id: DeliveryId) -> Result<(), ConsumerError> {
        self.shared()
            .with(|s| s.confirm(self.epoch(), &self.name, id))
            .map_err(|e| e.consumer(self.writer.shared.dir()))
    }

    /// Emits readings on behalf of this consumer, without waiting (spec §4
    /// Concurrency): a reading on a series outside its `emits` is refused
    /// [`Refusal::NotDeclared`](crate::Refusal::NotDeclared). With the model
    /// backend a consumer emits nothing, so every reading is refused.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError> {
        self.shared()
            .with(|s| s.consumer_emit(self.epoch(), &self.name, readings))
            .map(|ack| Pending::ready(Ok(ack)))
            .map_err(|e| e.emit(self.writer.shared.dir()))
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let epoch = self.epoch();
        self.shared().with(|s| s.release(epoch, &self.name));
    }
}
