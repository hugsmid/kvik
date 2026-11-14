//! The handles: [`Store`] (the writer), [`Reader`] and [`Consumer`].

use std::future::poll_fn;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use jiff::Timestamp;

use crate::backend::{self, Shared};
use crate::backup::{BackupSink, Encryption, Rebuild, RestoreMode};
use crate::config::StoreConfig;
use crate::consume::{Delivery, DeliveryId, Funnel, FunnelId, Select, Start};
use crate::error::{BusyReason, Error, Result};
use crate::health::{Health, Stats};
use crate::read::{
    Aggregate, CalendarPeriod, ChartData, Live, Resolution, View, Window, WindowResult,
};
use crate::series::{ParameterChange, SeriesInfo, SeriesKey, SeriesSpec};
use crate::types::{Accepted, Ack, Fact, Intent, Position, Reading, SeriesId};

/// The writer handle: one per store directory (spec §3 Single writer).
///
/// `Store` is `Send + Sync`; share it between tasks with an `Arc`. Dropping
/// it without [`Store::shutdown`] stops the store as a crash would; with the
/// model backend nothing committed is lost either way.
#[derive(Debug)]
pub struct Store {
    shared: Arc<Shared>,
    epoch: u64,
}

impl Store {
    /// Opens the store in `config.dir`, creating it if it does not exist, and
    /// returns once recovery has made it consistent.
    ///
    /// **Model backend: nothing is written to disk.** The directory only names
    /// an in-memory store within this process, which is lost when the process
    /// ends; opening the same directory again after a shutdown restarts that
    /// store. Fails with [`Error::Locked`] while another writer handle holds
    /// the directory, and with [`Error::SiteZoneMismatch`] when
    /// `config.time_zone` is not the store's zone.
    pub async fn open(config: StoreConfig) -> Result<Store> {
        let (shared, epoch) = backend::open(&config)?;
        Ok(Store { shared, epoch })
    }

    /// Whether the store is durable. `false` with the model backend.
    pub fn is_durable(&self) -> bool {
        false
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        self.shared.dir()
    }

    /// The store's `now`: the window clock, which never runs backwards (spec
    /// §6).
    pub fn now(&self) -> Timestamp {
        self.shared.with(|s| s.now())
    }

    /// Stops the writer and releases the directory: in the durable engine a
    /// final sync and checkpoint; with the model backend the store stays in
    /// memory. Consumers' handles see [`Error::Closed`] afterwards; readers
    /// keep reading what was committed.
    pub async fn shutdown(self) -> Result<()> {
        self.shared.with(|s| s.close(self.epoch));
        Ok(())
    }

    /// Defines a series, or returns the id of a live series defined the same
    /// way. A different definition under a live name is
    /// [`Error::NameInUse`]: retire and define anew.
    pub async fn define_series(&self, spec: SeriesSpec) -> Result<SeriesId> {
        self.shared.with(|s| s.define(self.epoch, spec))
    }

    /// Changes a series' parameters from an effective event time (spec
    /// §2.1). The model backend supports only the retention class, which
    /// takes effect at its position.
    pub async fn set_parameters(
        &self,
        id: SeriesId,
        effective_from: Timestamp,
        change: ParameterChange,
    ) -> Result<()> {
        self.shared
            .with(|s| s.set_parameters(self.epoch, id, effective_from, change))
    }

    /// Adds a name for a series (a rename); the id is unchanged.
    pub async fn alias(&self, id: SeriesId, name: &str) -> Result<()> {
        self.shared.with(|s| s.alias(self.epoch, id, name))
    }

    /// Retires a series at event time `t_r`: readings with `t ≥ t_r` are
    /// refused from then on; the series stays reachable by id.
    pub async fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<()> {
        self.shared.with(|s| s.retire(self.epoch, id, t_r))
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo> {
        self.shared.with(|s| s.series(key.into()))
    }

    /// Emits a batch and returns once it is committed (synced in the durable
    /// engine; held in memory only with the model backend). Refused readings are
    /// reported per index; the rest are recorded or dropped as duplicates.
    ///
    /// Waits under back-pressure (spec §3). With the model backend it waits
    /// only for clock synchronisation (host-stamped readings, spec §2.5),
    /// retried whenever the store is called again; a batch larger than the
    /// ingest queue, or free space below the critical mark, returns
    /// [`Error::Busy`] instead, as nothing would end the wait.
    pub async fn emit(&self, readings: &[Reading]) -> Result<Ack> {
        self.emit_with(readings, Intent::Normal).await
    }

    /// [`Store::emit`] with an intent.
    pub async fn emit_with(&self, readings: &[Reading], intent: Intent) -> Result<Ack> {
        let mut first = true;
        poll_fn(|cx| {
            self.shared.with(|s| {
                let r = s.emit(self.epoch, readings, intent, first);
                first = false;
                match r {
                    Ok(Ok(ack)) => Poll::Ready(Ok(ack)),
                    Ok(Err(BusyReason::UnsyncedClock)) => {
                        s.register(cx.waker());
                        Poll::Pending
                    }
                    // Nothing in the model backend drains a full queue or
                    // frees disk while the caller waits, so these are
                    // returned.
                    Ok(Err(busy)) => Poll::Ready(Err(Error::Busy(busy))),
                    Err(e) => Poll::Ready(Err(e)),
                }
            })
        })
        .await
    }

    /// Emits without waiting: [`Error::Busy`] under back-pressure, with
    /// nothing of the batch accepted. Returns what is known when the batch
    /// enters the queue: the readings refused at once. Positions and
    /// duplicates are known only after the commit (with the model backend
    /// the batch is committed before this returns, but that is not part of
    /// the interface).
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Accepted> {
        self.shared
            .with(|s| s.emit(self.epoch, readings, Intent::Normal, true))?
            .map(|ack| Accepted::new(ack.refused))
            .map_err(Error::Busy)
    }

    /// Runs the timer evaluations due by the clock now: window expiry,
    /// held-for and stale-for conditions, scheduled values taking effect
    /// (spec §4 Timers, §6 Cadence).
    ///
    /// **Temporary.** With the model backend timers advance only when the
    /// store is called: here, and before every emit and delivery poll.
    /// The durable engine runs them itself, every second, and this call will
    /// go.
    pub fn tick(&self) -> Result<()> {
        self.shared.with(|s| s.tick(self.epoch))
    }

    /// A consumer by name. A new consumer starts at `start`; an existing one
    /// resumes after its cursor, with replay then `resync` (spec §6 Restart
    /// and resume). One handle per name at a time.
    ///
    /// `emits` is the series the consumer may emit, so that routing can
    /// refuse a cycle (spec §4 Routing). The model backend accepts only
    /// [`Select::None`]; anything else is [`Error::NotSupportedYet`].
    pub async fn consumer(&self, name: &str, start: Start, emits: Select) -> Result<Consumer> {
        self.shared
            .with(|s| s.consumer(self.epoch, name, start, &emits))?;
        Ok(Consumer {
            shared: self.shared.clone(),
            epoch: self.epoch,
            name: name.to_string(),
        })
    }

    /// A read-only handle on this store.
    pub fn reader(&self) -> Reader {
        Reader {
            shared: self.shared.clone(),
        }
    }

    /// Health (spec §11).
    pub fn health(&self) -> Health {
        self.shared.with(|s| s.health())
    }

    /// Statistics (spec §11).
    pub fn stats(&self) -> Stats {
        self.shared.with(|s| s.stats())
    }

    /// Attaches a backup sink (spec §8). Not supported yet.
    pub async fn attach_backup<S: BackupSink>(&self, _sink: S, _key: Encryption) -> Result<()> {
        Err(Error::NotSupportedYet { what: "backup" })
    }

    /// Restores a store onto an empty directory from a sink (spec §8
    /// Restore); returns the restored end. Not supported yet.
    pub async fn restore<S: BackupSink>(
        _dir: impl Into<PathBuf>,
        _sink: S,
        _key: Encryption,
        _mode: RestoreMode,
    ) -> Result<Position> {
        Err(Error::NotSupportedYet { what: "restore" })
    }

    /// The facts a restore set aside in a range, each with its series name,
    /// for re-ingest (spec §8 Restore step 1). Not supported yet.
    pub fn export_set_aside(
        &self,
        _range: RangeInclusive<Position>,
    ) -> Result<Vec<(String, Fact)>> {
        Err(Error::NotSupportedYet {
            what: "export of set-aside facts",
        })
    }

    /// Recomputes derived structures from the facts. Not supported yet.
    pub async fn rebuild(&self, _what: Rebuild) -> Result<()> {
        Err(Error::NotSupportedYet { what: "rebuild" })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.shared.with(|s| s.close(self.epoch));
    }
}

/// A read-only handle (`docs/api.md` Reading). Takes no lock on the
/// directory and keeps reading after the writer shuts down. Reads see only
/// committed data (durable in the engine; in memory with the model backend).
/// `Send + Sync` and cheap to clone.
#[derive(Clone, Debug)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    /// A reader on the store in `dir`. With the model backend the store must
    /// have been opened in this process.
    pub async fn open(dir: impl AsRef<Path>) -> Result<Reader> {
        Ok(Reader {
            shared: backend::find(dir.as_ref())?,
        })
    }

    /// Live state: the latest valid value, with staleness (spec §2.3).
    pub fn live(&self, id: SeriesId) -> Result<Live> {
        self.shared.with(|s| s.live(id))
    }

    /// A window result (spec §6).
    pub fn window(
        &self,
        id: SeriesId,
        window: Window,
        aggregate: Aggregate,
    ) -> Result<WindowResult> {
        self.shared.with(|s| s.window(id, &window, &aggregate))
    }

    /// A chart query: raw facts or buckets with count, min, max and coverage.
    pub fn buckets(
        &self,
        id: SeriesId,
        range: Range<Timestamp>,
        resolution: Resolution,
    ) -> Result<ChartData> {
        self.shared
            .with(|s| s.buckets(id, (range.start, range.end), resolution))
    }

    /// Facts with `t` in a range, in a view (spec §2.3).
    pub fn facts(&self, id: SeriesId, range: Range<Timestamp>, view: View) -> Result<Vec<Fact>> {
        self.shared
            .with(|s| s.facts(id, (range.start, range.end), view))
    }

    /// A calendar day or month result in the site zone in force at each
    /// day's start: a window over its span (spec §5.7).
    pub fn calendar(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        aggregate: Aggregate,
    ) -> Result<WindowResult> {
        let window = Window::Calendar {
            window: crate::read::CalendarWindow::Period(period.into()),
            zone: None,
        };
        self.shared.with(|s| s.window(id, &window, &aggregate))
    }

    /// A calendar day or month result in a named IANA zone for the whole
    /// span (spec §5.7).
    pub fn calendar_in(
        &self,
        id: SeriesId,
        period: impl Into<CalendarPeriod>,
        zone: &str,
        aggregate: Aggregate,
    ) -> Result<WindowResult> {
        let window = Window::Calendar {
            window: crate::read::CalendarWindow::Period(period.into()),
            zone: Some(zone.to_string()),
        };
        self.shared.with(|s| s.window(id, &window, &aggregate))
    }

    /// The interval of an interval series that contains `t`: `[start, end)`.
    pub fn interval(&self, id: SeriesId, t: Timestamp) -> Result<(Timestamp, Timestamp)> {
        self.shared.with(|s| s.interval(id, t))
    }

    /// What the store knows about a series, by id or by name.
    pub fn series(&self, key: impl Into<SeriesKey>) -> Result<SeriesInfo> {
        self.shared.with(|s| s.series(key.into()))
    }
}

/// A consumer: its funnels, deliveries and confirmations (spec §4).
///
/// Deliveries are at least once: de-duplicate fact deliveries by position and
/// condition deliveries by funnel and sequence; on a `resync`, forget every
/// position seen above the durable cursor. A consumer that emits readings
/// uses [`Consumer::try_emit`], never the store's handle, so that its
/// readings are checked against its `emits`.
#[derive(Debug)]
pub struct Consumer {
    shared: Arc<Shared>,
    epoch: u64,
    name: String,
}

impl Consumer {
    /// The consumer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a funnel; a log entry that takes effect at its position.
    pub async fn route(&self, funnel: Funnel) -> Result<FunnelId> {
        self.shared
            .with(|s| s.route(self.epoch, &self.name, funnel))
    }

    /// Removes a funnel; a log entry that takes effect at its position.
    pub async fn unroute(&self, funnel: FunnelId) -> Result<()> {
        self.shared
            .with(|s| s.unroute(self.epoch, &self.name, funnel))
    }

    /// The funnels in force.
    pub fn routes(&self) -> Result<Vec<(FunnelId, Funnel)>> {
        self.shared.with(|s| s.routes(self.epoch, &self.name))
    }

    /// The next delivery, waiting for one. `Ok(None)` once the store is
    /// closed; any other error is returned.
    ///
    /// With the model backend nothing runs in the background: a waiting call
    /// is woken by the next emit, routing change or [`Store::tick`].
    pub async fn next(&mut self) -> Result<Option<Delivery>> {
        poll_fn(|cx| {
            self.shared
                .with(|s| match s.poll_delivery(self.epoch, &self.name) {
                    Ok(Some(d)) => Poll::Ready(Ok(Some(d))),
                    Ok(None) => {
                        s.register(cx.waker());
                        Poll::Pending
                    }
                    Err(Error::Closed) => Poll::Ready(Ok(None)),
                    Err(e) => Poll::Ready(Err(e)),
                })
        })
        .await
    }

    /// Emits readings on behalf of this consumer, without waiting (spec §4
    /// Concurrency): a reading on a series outside its `emits` is refused
    /// [`Refusal::NotDeclared`](crate::Refusal::NotDeclared). With the model
    /// backend a consumer emits nothing, so every reading is refused.
    pub fn try_emit(&self, readings: &[Reading]) -> Result<Accepted> {
        self.shared
            .with(|s| s.consumer_emit(self.epoch, &self.name, readings))
    }

    /// The next delivery if one is ready, without waiting.
    pub fn try_next(&mut self) -> Result<Option<Delivery>> {
        self.shared
            .with(|s| s.poll_delivery(self.epoch, &self.name))
    }

    /// Confirms a delivery and every delivery received before it; advances
    /// the consumer's cursor and the latches of the conditions they carried.
    pub async fn confirm(&self, id: DeliveryId) -> Result<()> {
        self.shared.with(|s| s.confirm(self.epoch, &self.name, id))
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        self.shared.with(|s| s.release(self.epoch, &self.name));
    }
}
