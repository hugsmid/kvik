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

mod reader;

pub use reader::Reader;
use reader::ReaderInner;

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
    /// [`DefineError::Conflict`]: retire and define anew. A name without
    /// three non-empty `/`-separated segments is [`DefineError::Invalid`]
    /// (spec §2.1 `name`). Resolves after the mark sync that covers the
    /// definition (durable backend).
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

    /// Adds a name for a series (a rename); the id is unchanged. A name
    /// without three non-empty `/`-separated segments is
    /// [`DefineError::Invalid`] (spec §2.1 `name`).
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
        d => {
            d.tick();
            Ok(())
        })
    }

    /// A consumer by name. A new consumer starts at `start`; an existing one
    /// resumes after its cursor, with replay then a resync (spec §6 Restart
    /// and resume). One handle per name at a time.
    ///
    /// `emits` is the series the consumer may emit, so that routing can
    /// refuse a cycle (spec §4 Routing). The model backend accepts only
    /// [`Select::None`]; anything else is
    /// [`ConsumerError::NotSupportedYet`]. On the durable backend a change of
    /// `emits` is a routing change, refused with
    /// [`ConsumerError::RouteCycle`] if it closes a cycle. A new consumer
    /// starts after the position before `start`, with no resync; one whose
    /// cursor file was lost is first sent a resync of cause
    /// [`ResyncCause::NoCursor`](crate::ResyncCause::NoCursor) (Q-131). An
    /// unknown saved point is [`ConsumerError::UnknownPoint`]. The model
    /// backend supports only [`Start::Now`].
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
                    inner: ConsumerInner::Model(m.clone()),
                    name: name.to_string(),
                })
            }
            StoreInner::Durable(d) => {
                d.take_consumer(name, start, &emits)
                    .map_err(|e| e.consumer(dir))?;
                Ok(Consumer {
                    inner: ConsumerInner::Durable(d.clone()),
                    name: name.to_string(),
                })
            }
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

    /// Per-consumer statistics (an addition of the interface).
    pub fn consumer_stats(&self, name: &str) -> Option<ConsumerStats> {
        by_backend!(self, m => m.shared.with(|s| s.consumer_stats(name)), d => d.consumer_stats(name))
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
    pub async fn attach_backup<S: BackupSink + 'static>(
        &self,
        _sink: S,
        _key: Encryption,
    ) -> Result<(), BackupError> {
        Err(BackupError::NotSupportedYet { what: "backup" })
    }

    /// Restores a store onto an empty directory from a sink (spec §8
    /// Restore); returns the restored end. Not supported yet.
    pub async fn restore<S: BackupSink + 'static>(
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

/// A consumer: its funnels, deliveries and confirmations (spec §4). `Send`,
/// not `Clone`; it holds a share of the store.
///
/// Deliveries are at least once: de-duplicate fact deliveries by position and
/// condition deliveries by funnel and sequence. A consumer that emits
/// readings uses [`Consumer::try_emit`], never the store's handle.
#[derive(Debug)]
pub struct Consumer {
    inner: ConsumerInner,
    name: String,
}

#[derive(Debug)]
enum ConsumerInner {
    Model(Arc<Writer>),
    Durable(Arc<durable::Writer>),
}

impl Consumer {
    fn dir(&self) -> &Path {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.dir(),
            ConsumerInner::Durable(d) => d.dir(),
        }
    }

    /// The consumer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a funnel; a log entry that takes effect at its position. A funnel
    /// equal to one in force is that funnel: its id is returned and nothing
    /// is written (Q-91 (c)).
    pub async fn route(&self, funnel: Funnel) -> Result<FunnelId, ConsumerError> {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.route(w.epoch, &self.name, funnel)),
            ConsumerInner::Durable(d) => d.route(&self.name, &funnel),
        }
        .map_err(|e| e.consumer(self.dir()))
    }

    /// Removes a funnel; a log entry that takes effect at its position.
    pub async fn unroute(&self, funnel: FunnelId) -> Result<(), ConsumerError> {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.unroute(w.epoch, &self.name, funnel)),
            ConsumerInner::Durable(d) => d.unroute(&self.name, funnel),
        }
        .map_err(|e| e.consumer(self.dir()))
    }

    /// The funnels in force.
    pub fn routes(&self) -> Result<Vec<(FunnelId, Funnel)>, ConsumerError> {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.routes(w.epoch, &self.name)),
            ConsumerInner::Durable(d) => d.routes(&self.name),
        }
        .map_err(|e| e.consumer(self.dir()))
    }

    /// The next delivery, waiting for one; `None` when no more will come.
    /// After `None`, call [`Consumer::try_next`]: `Err(ConsumerError::Closed)`
    /// is a clean close, anything else a fault.
    ///
    /// With the model backend nothing runs in the background: a waiting call
    /// is woken by the next emit, routing change, confirmation or
    /// [`Store::tick`]. With the durable backend it is woken when a batch is
    /// acknowledged and by the timers.
    pub async fn next(&mut self) -> Option<Delivery> {
        poll_fn(|cx| match &self.inner {
            ConsumerInner::Model(w) => {
                w.shared
                    .with(|s| match s.poll_delivery(w.epoch, &self.name) {
                        Ok(Some(d)) => Poll::Ready(Some(d)),
                        Ok(None) => {
                            s.register(cx.waker());
                            Poll::Pending
                        }
                        Err(_) => Poll::Ready(None),
                    })
            }
            ConsumerInner::Durable(d) => match d.poll_delivery(&self.name, Some(cx.waker())) {
                Ok(Some(x)) => Poll::Ready(Some(x)),
                Ok(None) => Poll::Pending,
                Err(_) => Poll::Ready(None),
            },
        })
        .await
    }

    /// The next delivery if one is ready, without waiting.
    pub fn try_next(&mut self) -> Result<Option<Delivery>, ConsumerError> {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.poll_delivery(w.epoch, &self.name)),
            ConsumerInner::Durable(d) => d.poll_delivery(&self.name, None),
        }
        .map_err(|e| e.consumer(self.dir()))
    }

    /// Confirms a delivery and every delivery received before it. A resync is
    /// confirmed by its last part; nothing further is delivered until then.
    pub async fn confirm(&self, id: DeliveryId) -> Result<(), ConsumerError> {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.confirm(w.epoch, &self.name, id)),
            ConsumerInner::Durable(d) => d.confirm(&self.name, id),
        }
        .map_err(|e| e.consumer(self.dir()))
    }

    /// Emits readings on behalf of this consumer, without waiting (spec §4
    /// Concurrency): a reading on a series outside its `emits` is refused
    /// [`Refusal::NotDeclared`](crate::Refusal::NotDeclared). With the model
    /// backend a consumer emits nothing, so every reading is refused.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError> {
        match &self.inner {
            ConsumerInner::Model(w) => w
                .shared
                .with(|s| s.consumer_emit(w.epoch, &self.name, readings))
                .map(|ack| Pending::ready(Ok(ack)))
                .map_err(|e| e.emit(self.dir())),
            ConsumerInner::Durable(d) => match d.consumer_emit(&self.name, readings)? {
                Emitted::Ready(ack) => Ok(Pending::ready(Ok(ack))),
                Emitted::Queued(q) => Ok(Pending::queued(q)),
            },
        }
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        match &self.inner {
            ConsumerInner::Model(w) => w.shared.with(|s| s.release(w.epoch, &self.name)),
            ConsumerInner::Durable(d) => d.release(&self.name),
        }
    }
}
