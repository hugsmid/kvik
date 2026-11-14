//! The handles: [`Store`] (the writer), [`Reader`] and [`Consumer`]
//! (`docs/api.md` Handles and life cycle).

use std::future::poll_fn;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use crate::backend::{self, Shared};
use crate::backup::{BackupSink, Encryption, Rebuild, RestoreMode};
use crate::config::{ReaderConfig, StoreConfig};
use crate::consume::{Delivery, DeliveryId, Funnel, FunnelId, Select, Start};
use crate::error::{
    BackupError, ConsumerError, DefineError, EmitError, OpenError, QueryError, WriteError,
};
use crate::health::{ConsumerStats, Health, LossReport, SeriesStats, Stats};
use crate::read::{
    Aggregate, CalendarPeriod, CalendarWindow, ChartData, Facts, Live, ReaderState, Resolution,
    View, Window, WindowResult,
};
use crate::series::{ParameterChange, SeriesInfo, SeriesKey, SeriesSpec};
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
/// equivalent to a process crash; with the model backend nothing committed
/// is lost either way.
#[derive(Clone, Debug)]
pub struct Store {
    writer: Arc<Writer>,
}

impl Store {
    fn shared(&self) -> &Shared {
        &self.writer.shared
    }

    fn epoch(&self) -> u64 {
        self.writer.epoch
    }

    fn dir_path(&self) -> &Path {
        self.shared().dir()
    }

    /// Opens the store in `config.dir`, creating it when
    /// `config.create_if_missing` is set and the directory holds none, and
    /// returns once recovery has made it consistent.
    ///
    /// **Model backend: nothing is written to disk.** The directory only names
    /// an in-memory store within this process, which is lost when the process
    /// ends; opening the same directory again after a shutdown restarts that
    /// store. Fails with [`OpenError::Locked`] while another writer holds the
    /// directory, and with [`OpenError::SiteZoneMismatch`] when
    /// `config.time_zone` is not the zone in force.
    pub async fn open(config: StoreConfig) -> Result<Store, OpenError> {
        let (shared, epoch) = backend::open(&config).map_err(|e| e.open(&config.dir))?;
        Ok(Store {
            writer: Arc::new(Writer { shared, epoch }),
        })
    }

    /// Whether the store is durable. `false` with the model backend.
    pub fn is_durable(&self) -> bool {
        false
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        self.dir_path()
    }

    /// The store's `now`: the window clock, which never runs backwards (spec
    /// §6).
    pub fn now(&self) -> Timestamp {
        self.shared().with(|s| s.now())
    }

    /// Stops the writer and releases the directory: new calls on any clone
    /// return `Closed` and consumers' `next()` returns `None`. In the durable
    /// engine a final sync and checkpoint; with the model backend the store
    /// stays in memory and readers keep reading it.
    pub async fn shutdown(&self) -> Result<(), WriteError> {
        self.shared().with(|s| s.close(self.epoch()));
        Ok(())
    }

    /// Defines a series, or returns the id of a live series defined the same
    /// way. A different definition under a live name is
    /// [`DefineError::Conflict`]: retire and define anew.
    pub async fn define_series(&self, spec: SeriesSpec) -> Result<SeriesId, DefineError> {
        self.shared()
            .with(|s| s.define(self.epoch(), spec))
            .map_err(|e| e.define(self.dir_path()))
    }

    /// Changes a series' parameters from an effective event time (spec
    /// §2.1). The model backend supports only the retention class, which
    /// takes effect at its position.
    pub async fn set_parameters(
        &self,
        id: SeriesId,
        effective_from: Timestamp,
        change: ParameterChange,
    ) -> Result<(), DefineError> {
        self.shared()
            .with(|s| s.set_parameters(self.epoch(), id, effective_from, change))
            .map_err(|e| e.define(self.dir_path()))
    }

    /// Raises a decimal series' scale from this entry's position on. Not
    /// supported by the model backend, which has no decimal series.
    pub async fn raise_scale(&self, id: SeriesId, scale: u8) -> Result<(), DefineError> {
        self.shared()
            .with(|s| s.raise_scale(self.epoch(), id, scale))
            .map_err(|e| e.define(self.dir_path()))
    }

    /// Changes the site zone from an effective event time (a site-zone entry,
    /// spec §5.7); then change it in the configuration too.
    pub async fn set_site_zone(
        &self,
        zone: &ZoneName,
        effective_from: Timestamp,
    ) -> Result<(), DefineError> {
        self.shared()
            .with(|s| s.set_site_zone(self.epoch(), zone.as_str(), effective_from))
            .map(|_| ())
            .map_err(|e| e.define(self.dir_path()))
    }

    /// Adds a name for a series (a rename); the id is unchanged.
    pub async fn alias(&self, id: SeriesId, name: &str) -> Result<(), DefineError> {
        self.shared()
            .with(|s| s.alias(self.epoch(), id, name))
            .map_err(|e| e.define(self.dir_path()))
    }

    /// Retires a series at event time `t_r`: readings with `t ≥ t_r` are
    /// refused from then on; the series stays reachable by id.
    pub async fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<(), DefineError> {
        self.shared()
            .with(|s| s.retire(self.epoch(), id, t_r))
            .map_err(|e| e.define(self.dir_path()))
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        self.shared()
            .with(|s| s.series(key.into()))
            .map_err(|e| e.query())
    }

    /// Emits a call's readings as one batch and returns its outcomes once it
    /// is committed (synced in the durable engine; held in memory only with
    /// the model backend).
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
        let guard = WaitGuard::new(self.writer.shared.clone());
        let mut first = true;
        poll_fn(|cx| {
            self.shared().with(|s| {
                let r = s.emit(self.epoch(), readings, intent);
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
                        Poll::Ready(Err(e.emit(self.dir_path())))
                    }
                }
            })
        })
        .await
    }

    /// Emits without waiting: [`EmitError::Busy`] under back-pressure, with
    /// nothing queued. The returned [`Pending`] resolves to the call's
    /// outcomes and may be dropped. With the model backend the batch is
    /// committed before this returns.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError> {
        self.try_emit_with(readings, Intent::Normal)
    }

    fn try_emit_with(&self, readings: &[Reading], intent: Intent) -> Result<Pending, EmitError> {
        match self.shared().with(|s| {
            let r = s.emit(self.epoch(), readings, intent);
            if let Ok(Err(w)) = &r {
                s.note_wait(None, *w, true);
            }
            r
        }) {
            Ok(Ok(ack)) => Ok(Pending::ready(Ok(ack))),
            Ok(Err(w)) => Err(EmitError::Busy(w)),
            Err(e) => Err(e.emit(self.dir_path())),
        }
    }

    /// Runs the timer evaluations due by the clock now: window expiry,
    /// held-for and stale-for conditions, scheduled values taking effect
    /// (spec §4 Timers, §6 Cadence).
    ///
    /// **Temporary.** With the model backend timers advance only when the
    /// store is called: here, and before every emit and delivery poll. The
    /// engine runs them on its own threads, and this call will go.
    pub fn tick(&self) -> Result<(), WriteError> {
        self.shared()
            .with(|s| s.tick(self.epoch()))
            .map_err(|e| e.write(self.dir_path()))
    }

    /// A consumer by name. A new consumer starts at `start`; an existing one
    /// resumes after its cursor, with replay then a resync (spec §6 Restart
    /// and resume). One handle per name at a time.
    ///
    /// `emits` is the series the consumer may emit, so that routing can
    /// refuse a cycle (spec §4 Routing). The model backend accepts only
    /// [`Select::None`]; anything else is
    /// [`ConsumerError::NotSupportedYet`].
    pub async fn consumer(
        &self,
        name: &str,
        start: Start,
        emits: Select,
    ) -> Result<Consumer, ConsumerError> {
        self.shared()
            .with(|s| s.consumer(self.epoch(), name, start, &emits))
            .map_err(|e| e.consumer(self.dir_path()))?;
        Ok(Consumer {
            store: self.clone(),
            name: name.to_string(),
        })
    }

    /// A read-only handle on this store.
    pub fn reader(&self) -> Reader {
        Reader {
            shared: self.writer.shared.clone(),
        }
    }

    /// Health (spec §11).
    pub fn health(&self) -> Health {
        self.shared().with(|s| s.health())
    }

    /// Statistics (spec §11).
    pub fn stats(&self) -> Stats {
        self.shared().with(|s| s.stats())
    }

    /// Per-series statistics.
    pub fn series_stats(&self, id: SeriesId) -> Option<SeriesStats> {
        self.shared().with(|s| s.series_stats(id))
    }

    /// Per-consumer statistics (an addition of the model backend).
    pub fn consumer_stats(&self, name: &str) -> Option<ConsumerStats> {
        self.shared().with(|s| s.consumer_stats(name))
    }

    /// Every loss report, for ever.
    pub fn losses(&self) -> Vec<LossReport> {
        self.shared().with(|s| s.losses())
    }

    /// Acknowledges losses by their identities; a `DataLost` degrades health
    /// until acknowledged.
    pub async fn acknowledge_losses(&self, identities: &[Position]) -> Result<(), WriteError> {
        self.shared()
            .with(|s| s.acknowledge_losses(self.epoch(), identities))
            .map_err(|e| e.write(self.dir_path()))
    }

    /// Acknowledges a rebuilt manifest record (spec §7 Manifest). The model
    /// backend never rebuilds one.
    pub async fn acknowledge_manifest_rebuild(&self, epoch: u64) -> Result<(), WriteError> {
        self.shared()
            .with(|s| s.acknowledge_manifest_rebuild(self.epoch(), epoch))
            .map_err(|e| e.write(self.dir_path()))
    }

    /// Orphaned facts (spec §2.1), for re-ingest. Not available with the
    /// model backend.
    pub fn export_orphaned(
        &self,
        _range: RangeInclusive<Position>,
    ) -> Result<Vec<Fact>, QueryError> {
        Err(QueryError::NotAvailable {
            reason: "orphaned facts are not modelled".into(),
        })
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
/// writer shuts down. Reads see only committed data (durable in the engine;
/// in memory with the model backend). `Send + Sync` and cheap to clone.
#[derive(Clone, Debug)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    /// A reader on the store in `config.dir`. With the model backend the
    /// store must have been opened in this process (else
    /// [`OpenError::NotAStore`]).
    pub fn open(config: ReaderConfig) -> Result<Reader, OpenError> {
        Ok(Reader {
            shared: backend::find(&config.dir).map_err(|e| e.open(&config.dir))?,
        })
    }

    /// Re-reads the mark and renews the lease.
    pub fn refresh(&self) -> Result<ReaderState, QueryError> {
        self.shared.with(|s| s.refresh()).map_err(|e| e.query())
    }

    /// Live state: the latest valid value, with staleness (spec §2.3).
    pub fn live(&self, id: SeriesId) -> Result<Live, QueryError> {
        self.shared.with(|s| s.live(id)).map_err(|e| e.query())
    }

    /// A window result (spec §6).
    pub fn window(
        &self,
        id: SeriesId,
        window: Window,
        aggregate: Aggregate,
    ) -> Result<WindowResult, QueryError> {
        self.shared
            .with(|s| s.window(id, &window, &aggregate))
            .map_err(|e| e.query())
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
        self.shared
            .with(|s| s.buckets(id, range, resolution))
            .map_err(|e| e.query())
    }

    /// Facts with `t` in a range, in a view (spec §2.3).
    pub fn facts(
        &self,
        id: SeriesId,
        range: impl Into<TimeRange>,
        view: View,
    ) -> Result<Facts, QueryError> {
        let range = range.into();
        self.shared
            .with(|s| s.facts(id, range, view))
            .map_err(|e| e.query())
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
        self.shared
            .with(|s| s.at(id, t, view))
            .map_err(|e| e.query())
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
        self.shared
            .with(|s| s.interval(id, t))
            .map_err(|e| e.query())
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo, QueryError> {
        self.shared
            .with(|s| s.series(key.into()))
            .map_err(|e| e.query())
    }

    /// What the files report: loss reports, not the writer's counters.
    pub fn health(&self) -> Health {
        let losses: Vec<_> = self
            .losses()
            .into_iter()
            .filter(|l| !l.acknowledged)
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
        self.shared.with(|s| s.losses())
    }
}

/// A consumer: its funnels, deliveries and confirmations (spec §4). `Send`,
/// not `Clone`; it holds a share of the store.
///
/// Deliveries are at least once: de-duplicate fact deliveries by position and
/// condition deliveries by funnel and sequence. A consumer that emits
/// readings uses [`Consumer::try_emit`], never the store's handle.
#[derive(Debug)]
pub struct Consumer {
    store: Store,
    name: String,
}

impl Consumer {
    fn shared(&self) -> &Shared {
        self.store.shared()
    }

    fn epoch(&self) -> u64 {
        self.store.epoch()
    }

    /// The consumer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a funnel; a log entry that takes effect at its position.
    pub async fn route(&self, funnel: Funnel) -> Result<FunnelId, ConsumerError> {
        self.shared()
            .with(|s| s.route(self.epoch(), &self.name, funnel))
            .map_err(|e| e.consumer(self.store.dir_path()))
    }

    /// Removes a funnel; a log entry that takes effect at its position.
    pub async fn unroute(&self, funnel: FunnelId) -> Result<(), ConsumerError> {
        self.shared()
            .with(|s| s.unroute(self.epoch(), &self.name, funnel))
            .map_err(|e| e.consumer(self.store.dir_path()))
    }

    /// The funnels in force.
    pub fn routes(&self) -> Result<Vec<(FunnelId, Funnel)>, ConsumerError> {
        self.shared()
            .with(|s| s.routes(self.epoch(), &self.name))
            .map_err(|e| e.consumer(self.store.dir_path()))
    }

    /// The next delivery, waiting for one; `None` once the store is shut
    /// down. Any other error also ends the stream with `None`;
    /// [`Consumer::try_next`] returns it.
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
            .map_err(|e| e.consumer(self.store.dir_path()))
    }

    /// Confirms a delivery and every delivery received before it. A resync is
    /// confirmed by its last part; nothing further is delivered until then.
    pub async fn confirm(&self, id: DeliveryId) -> Result<(), ConsumerError> {
        self.shared()
            .with(|s| s.confirm(self.epoch(), &self.name, id))
            .map_err(|e| e.consumer(self.store.dir_path()))
    }

    /// Emits readings on behalf of this consumer, without waiting (spec §4
    /// Concurrency): a reading on a series outside its `emits` is refused
    /// [`Refusal::NotDeclared`](crate::Refusal::NotDeclared). With the model
    /// backend a consumer emits nothing, so every reading is refused.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError> {
        self.shared()
            .with(|s| s.consumer_emit(self.epoch(), &self.name, readings))
            .map(|ack| Pending::ready(Ok(ack)))
            .map_err(|e| e.emit(self.store.dir_path()))
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let epoch = self.epoch();
        self.shared().with(|s| s.release(epoch, &self.name));
    }
}
