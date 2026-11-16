//! The durable backend: the interface on the log of `kvik-log` (roadmap 5.1).
//!
//! `Store::open` opens the log writer, whose start recovers the log (`spec.md` §3 Order of a start), then reads the
//! kept log once with a log reader to build the series registry and the state the duplicate rule needs, and
//! starts the writer's threads. Every call that appends an entry is decided against that state and queued in the
//! log; it resolves when the log acknowledges it (after the sync and the mark update, and for an entry that is
//! not a fact after the mark sync). Reads go through a log reader, in this process or another, and so see only
//! what is durable. Live state, windows, chart buckets and calendar results are evaluated from the reader's facts
//! (roadmap 5.4, [`windows`]).
//!
//! Consumers, routing, condition funnels and durable cursors are in [`deliver`] and [`routing`] (roadmap 5.2).
//!
//! Not supported yet, with the roadmap item that brings each: starting a consumer at a position or a time and
//! saved points (5.3), windowed operators in funnels, stored summaries (7.x), decimal series, calendar-length
//! interval series and parameter changes other than the retention class, compaction (6.x), backup and restore (8.x).

mod clock;
mod consumers;
mod deliver;
mod index;
mod ingest;
pub(crate) mod reader;
mod registry;
mod routing;
mod windows;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use kvik_log::content::Item;
use kvik_log::entries::{Entry, ParameterChange as LogChange};
use kvik_log::store::{NewStore, StoreDir, StoreError};
use kvik_log::writer::{
    Ack as LogAck, CommitError, Fault as LogFault, FaultOp, Log, LogWriter,
    MarkSync as LogMarkSync, MarkSyncMode as LogMarkSyncMode, OpenError as LogOpenError,
    Pending as LogPending, Reason as LogReason, SubmitError, Wait as LogWait, WriterConfig,
};

use crate::backend::Error;
use crate::backend::convert;
use crate::config::{Limits, MarkSync, StoreConfig};
use crate::env::{Clock, Env};
use crate::error::{EmitError, Fault, OpenError, QueryError, Wait, WriteError};
use crate::health::{
    ClockStats, DiskStats, Health, IngestStats, Latency, LogStats, LossKind, LossReport,
    MarkSyncMode, Reason, SeriesStats, Stats,
};
use crate::read::{Facts, ReaderState, View};
use crate::series::{IntervalLength, Kind, ParameterChange, SeriesInfo, SeriesKey, SeriesSpec};
use crate::time::{TimeRange, Timestamp, ZoneName};
use crate::types::{Ack, Fact, Intent, Outcome, Position, Reading, SeriesId, TimeSource, Value};
use index::{Index, fact_out, pos};
use ingest::{Decider, Decision};
pub(crate) use reader::DurableReader;
pub(crate) use windows::{buckets, live, window};

/// The most readings in one call, whatever the ingest queue (`docs/api.md` Writing).
const CALL_READINGS: usize = 65_536;
/// The most plain-encoded bytes in one call.
const CALL_BYTES: usize = 8 << 20;
/// Commit latencies kept for the percentiles.
const LATENCIES: usize = 1_024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The log's clocks: the wall clock from the environment, and a monotonic time of this process for the log's
/// timers (group commit, mark sync, sealing), so that a manual clock that stands still does not hold back a sync.
#[derive(Debug)]
pub(crate) struct LogClock {
    env: Arc<dyn Clock>,
    origin: Instant,
}

impl LogClock {
    pub(crate) fn new(env: Arc<dyn Clock>) -> Self {
        LogClock {
            env,
            origin: Instant::now(),
        }
    }
}

impl kvik_log::writer::Clock for LogClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
    fn wall(&self) -> i64 {
        self.env.wall().0
    }
}

/// The interface's fault for a fault of the log.
fn fault_out(f: &LogFault) -> Fault {
    match (f.op, f.kind) {
        (_, io::ErrorKind::StorageFull) => Fault::NoSpace {
            path: f.path.clone(),
        },
        (FaultOp::Sync | FaultOp::MarkSync, _) => Fault::SyncFailed {
            path: f.path.clone(),
        },
        (op, kind) => Fault::Io {
            op: match op {
                FaultOp::Create => "create",
                FaultOp::Write => "write",
                FaultOp::MarkWrite => "mark write",
                FaultOp::Seal => "seal",
                FaultOp::Panic => "writer thread",
                FaultOp::Sync | FaultOp::MarkSync => "sync",
            },
            path: f.path.clone(),
            kind,
        },
    }
}

/// The interface's wait for one of the log's (Q-115 (k)): a segment not yet created ahead holds the queue as a
/// full queue does.
fn wait_out(w: LogWait) -> Wait {
    match w {
        LogWait::QueueFull | LogWait::NoSegment => Wait::QueueFull,
        LogWait::DiskCritical { .. } => Wait::DiskFull,
    }
}

pub(crate) fn store_error(dir: &Path, e: StoreError) -> OpenError {
    match e {
        StoreError::NotAStore => OpenError::NotAStore,
        StoreError::Locked => OpenError::Locked,
        StoreError::Corrupt(what) => OpenError::Corrupt {
            file: dir.join(what),
        },
        StoreError::ManifestCorrupt => OpenError::Corrupt {
            file: dir.join(kvik_log::store::MANIFEST),
        },
        StoreError::Unsupported(_) => OpenError::UnsupportedFormat {
            file: dir.to_path_buf(),
            found: 0,
            supported: u32::from(kvik_log::FORMAT_MAJOR),
        },
        StoreError::Io(source) => OpenError::Io {
            path: dir.to_path_buf(),
            source,
        },
        other => OpenError::Io {
            path: dir.to_path_buf(),
            source: io::Error::other(other.to_string()),
        },
    }
}

/// Counters kept by this process since the store was opened, and those rebuilt from the log.
#[derive(Debug, Default)]
struct Counters {
    series: BTreeMap<u32, SeriesStats>,
    waits: BTreeMap<Wait, u64>,
    waited: Duration,
    latencies: VecDeque<Duration>,
    max_latency: Duration,
}

/// The writer of a durable store, shared by the clones of its `Store`.
#[derive(Debug)]
pub(crate) struct Writer {
    dir: PathBuf,
    env: Env,
    limits: Limits,
    log: RwLock<Option<Log>>,
    /// A fault of the start, or of a registry file write (`spec.md` §3 Write or sync failure).
    own_fault: Mutex<Option<LogFault>>,
    store_id: [u8; 16],
    decider: Mutex<Decider>,
    counters: Mutex<Counters>,
    reader: Mutex<Option<Arc<DurableReader>>>,
    acknowledged: Mutex<BTreeSet<Position>>,
    /// Calls queued and not yet known to be acknowledged, by call number (Q-123 (b)).
    inflight: Mutex<BTreeMap<u64, Arc<Completion>>>,
    next_call: AtomicU64,
    closed: AtomicBool,
    /// The window clock and the refresh instants of live staleness, shared with the reader of this process.
    clocks: Arc<clock::Clocks>,
    /// Consumers and routing; `None` when the start left the store read-only before the log could be read.
    hub: Option<Arc<deliver::Hub>>,
}

/// How a queued call's acknowledgement becomes its [`Ack`].
type Finish = Box<dyn FnOnce(Result<LogAck, CommitError>) -> Result<Ack, EmitError> + Send + Sync>;

/// Wakes every waker registered with a [`Completion`].
#[derive(Debug, Default)]
struct FanOut(Mutex<Vec<Waker>>);

impl Wake for FanOut {
    fn wake(self: Arc<Self>) {
        let wakers = std::mem::take(&mut *lock(&self.0));
        for w in wakers {
            w.wake();
        }
    }
}

/// Resolves a [`Completion`] when the log completes its call: the log wakes it on its writer thread, after the
/// covering sync, whether or not anyone waits for the call.
#[derive(Debug)]
struct Resolver(Weak<Completion>);

impl Wake for Resolver {
    fn wake(self: Arc<Self>) {
        if let Some(c) = self.0.upgrade() {
            let _ = c.poll(None);
        }
    }
}

/// What a call does once the log acknowledges it, whoever waits for it.
type OnAck = Box<dyn FnOnce() + Send>;

struct CompletionState {
    pending: Option<LogPending>,
    result: Option<Result<LogAck, CommitError>>,
    on_ack: Option<OnAck>,
}

impl std::fmt::Debug for CompletionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionState")
            .field("pending", &self.pending)
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}

/// The log's acknowledgement of one queued call, shared by the call's own future and by every later call whose
/// outcome depends on it (a duplicate of one of its readings, Q-123 (b)), each of which may wait on it.
#[derive(Debug)]
pub(crate) struct Completion {
    state: Mutex<CompletionState>,
    wakers: Arc<FanOut>,
    resolver: Waker,
}

impl Completion {
    /// A completion whose `on_ack` runs once the log acknowledges the call, on the log's writer thread or in the
    /// first poll that finds the acknowledgement.
    fn new(pending: LogPending, on_ack: OnAck) -> Arc<Self> {
        let c = Arc::new_cyclic(|me| Completion {
            state: Mutex::new(CompletionState {
                pending: Some(pending),
                result: None,
                on_ack: Some(on_ack),
            }),
            wakers: Arc::new(FanOut::default()),
            resolver: Waker::from(Arc::new(Resolver(me.clone()))),
        });
        // Registers the resolver with the log, or resolves at once.
        let _ = c.poll(None);
        c
    }

    /// The call's result once the log has given it; registers `cx`'s waker otherwise.
    pub(crate) fn poll(&self, cx: Option<&Context<'_>>) -> Poll<Result<LogAck, CommitError>> {
        let mut st = lock(&self.state);
        if let Some(r) = &st.result {
            return Poll::Ready(r.clone());
        }
        if let Some(cx) = cx {
            let mut w = lock(&self.wakers.0);
            if !w.iter().any(|x| x.will_wake(cx.waker())) {
                w.push(cx.waker().clone());
            }
        }
        let mut resolver_cx = Context::from_waker(&self.resolver);
        let Some(pending) = st.pending.as_mut() else {
            return Poll::Pending;
        };
        match std::pin::Pin::new(pending).poll(&mut resolver_cx) {
            Poll::Ready(r) => {
                st.result = Some(r.clone());
                st.pending = None;
                let on_ack = st.on_ack.take().filter(|_| r.is_ok());
                drop(st);
                if let Some(f) = on_ack {
                    f();
                }
                Waker::from(self.wakers.clone()).wake();
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// Whether the call is acknowledged.
    fn acknowledged(&self) -> bool {
        matches!(self.poll(None), Poll::Ready(Ok(_)))
    }
}

/// A call waiting for the log: its own queued batch, or the queued call an outcome of it depends on; and how that
/// call's acknowledgement becomes this call's [`Ack`].
pub(crate) struct Queued {
    completion: Arc<Completion>,
    finish: Finish,
}

impl std::fmt::Debug for Queued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queued")
            .field("completion", &self.completion)
            .finish()
    }
}

impl Queued {
    pub(crate) fn poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<LogAck, CommitError>> {
        self.completion.poll(Some(cx))
    }

    pub(crate) fn finish(self, r: Result<LogAck, CommitError>) -> Result<Ack, EmitError> {
        (self.finish)(r)
    }
}

/// What an emit gives back: the outcomes at once, or a call queued in the log.
pub(crate) enum Emitted {
    Ready(Ack),
    Queued(Queued),
}

impl Writer {
    /// Opens the store (`docs/api.md` Opening).
    pub(crate) fn open(config: &StoreConfig) -> Result<Arc<Writer>, OpenError> {
        config.validate()?;
        let dir = config.dir.clone();
        let env = config.env.clone();
        jiff::tz::TimeZone::get(&config.time_zone).map_err(|_| OpenError::UnknownZone)?;
        let wall = env.clock.wall().0;
        let new = config.create_if_missing.then(|| {
            let mut store_id = [0u8; 16];
            env.entropy.fill(&mut store_id);
            NewStore {
                store_id,
                created_at: wall,
                site_zone: config.time_zone.clone(),
                tzdata: tzdata_name(env.tz.current().version()),
                software: format!("kvik {}", env!("CARGO_PKG_VERSION")),
            }
        });
        let store =
            StoreDir::open(env.fs.clone(), &dir, new.as_ref()).map_err(|e| store_error(&dir, e))?;
        let descriptor = store.descriptor().clone();
        let clock = Arc::new(LogClock::new(env.clock.clone()));
        let wcfg = writer_config(config);
        let writer = LogWriter::open(
            store,
            wcfg,
            env.boot.current(),
            kvik_log::writer::Clock::monotonic(&*clock),
            wall,
        )
        .map_err(|e| match e {
            LogOpenError::Store(e) => store_error(&dir, e),
            LogOpenError::Config(c) => OpenError::Config {
                key: "log",
                reason: c.0.into(),
            },
            other => OpenError::Io {
                path: dir.clone(),
                source: io::Error::other(other.to_string()),
            },
        })?;
        let start_fault = writer.fault().cloned();
        // The kept log, read once: the registry and the history of every key.
        let index = match DurableReader::open(&dir, &env, Duration::from_secs(30 * 60)) {
            Ok(r) => {
                let mut index = None;
                r.with(|i, _| {
                    index = Some(i.clone());
                    Ok(())
                })
                .map_err(|e| OpenError::Io {
                    path: dir.clone(),
                    source: io::Error::other(e.to_string()),
                })?;
                index.unwrap_or_default()
            }
            // A start that could not write may leave no readable mark: the store opens read-only.
            Err(_) if start_fault.is_some() => Index::default(),
            Err(e) => return Err(e),
        };
        // The site zone in force must be the configured one (`docs/api.md` Opening).
        let tz = env.tz.current();
        let canonical = |z: &str| tz.canonical(z).unwrap_or_else(|| z.to_string());
        let in_force = index
            .registry()
            .site_zone(&descriptor.site_zone)
            .to_string();
        if canonical(&in_force) != canonical(&config.time_zone) {
            return Err(OpenError::SiteZoneMismatch {
                configured: config.time_zone.clone(),
                in_force,
            });
        }
        let tzdata_in_force = index.registry().tzdata(&descriptor.tzdata).to_string();
        let mut counters = Counters::default();
        for id in index.fact_series() {
            let st = counters.series.entry(id).or_default();
            for f in index.series_facts(id) {
                st.recorded += 1;
                st.late += u64::from(f.row.flags.late);
                st.future += u64::from(f.row.flags.future);
            }
        }
        // The derived check of the registry file (Q-33, as far as roadmap 5.1 reaches): the registry is rebuilt from
        // the kept log at every start, and the file, which must equal what the kept log gives up to the last synced
        // mark, is written again whenever it does not. Its id floor is the writer's, which recovery folded in from
        // the mark, the kept log, its recovery entries and the file read, so a lost definition's id stays assigned.
        let mut start_fault = start_fault;
        if start_fault.is_none() {
            let stats = writer.stats();
            let expected = index.registry_file(stats.mark_synced, stats.max_series_id);
            let current = kvik_log::registry::read_latest(&*env.fs, &dir, &descriptor.store_id)
                .ok()
                .flatten();
            if current.is_none_or(|(_, f)| f != expected)
                && let Err(e) =
                    kvik_log::registry::write(&*env.fs, &dir, descriptor.store_id, wall, &expected)
            {
                start_fault = Some(registry_fault(&dir, &e));
            }
        }
        // Consumers resume from their cursor files and the kept log (`spec.md` §3 Order of a start, step 7).
        let clocks = Arc::new(clock::Clocks::new(env.clock.clone()));
        let hub = match deliver::Deliverer::open(
            &dir,
            &env,
            &clocks,
            descriptor.store_id,
            config.limits.consumer_queue,
            config.limits.max_consumers,
        ) {
            Ok(d) => Some(deliver::Hub::new(d, env.clone(), &dir, clocks.clone())),
            Err(_) if start_fault.is_some() => None,
            Err(source) => {
                return Err(OpenError::Io {
                    path: dir.clone(),
                    source,
                });
            }
        };
        let log = Log::start(writer, clock).map_err(|source| OpenError::Io {
            path: dir.clone(),
            source,
        })?;
        if let Some(h) = &hub {
            h.start(log.watch()).map_err(|source| OpenError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        let w = Arc::new(Writer {
            dir,
            limits: config.limits.clone(),
            log: RwLock::new(Some(log)),
            own_fault: Mutex::new(start_fault),
            store_id: descriptor.store_id,
            decider: Mutex::new(Decider::from_index(&index)),
            counters: Mutex::new(counters),
            reader: Mutex::new(None),
            acknowledged: Mutex::new(BTreeSet::new()),
            inflight: Mutex::new(BTreeMap::new()),
            next_call: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            clocks,
            hub,
            env,
        });
        // A tzdata entry when the host's release is not the version in force, before ingest (`docs/api.md`
        // Opening). A release whose name is not a valid entry is left out.
        let release = tzdata_name(tz.version());
        if w.fault().is_none() && release != tzdata_in_force && !release.is_empty() {
            let mut d = lock(&w.decider);
            w.commit_entry(&mut d, Entry::Tzdata { version: release })
                .map_err(|e| OpenError::Io {
                    path: w.dir.clone(),
                    source: io::Error::other(e.to_string()),
                })?;
        }
        Ok(w)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn log(&self) -> RwLockReadGuard<'_, Option<Log>> {
        self.log.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn fault(&self) -> Option<LogFault> {
        lock(&self.own_fault)
            .clone()
            .or_else(|| self.hub.as_ref().and_then(|h| lock(&h.fault).clone()))
            .or_else(|| self.log().as_ref().and_then(Log::fault))
    }

    /// Fails if the store is closed or read-only.
    fn writable(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        if let Some(f) = self.fault() {
            return Err(Error::ReadOnly(f.to_string()));
        }
        Ok(())
    }

    /// The store's `now`, which never runs backwards (spec §6 `now`).
    pub(crate) fn now(&self) -> Timestamp {
        Timestamp(self.clocks.now())
    }

    /// The reader this process uses for its own reads, opened on first use.
    pub(crate) fn reader(&self) -> Result<Arc<DurableReader>, QueryError> {
        let mut r = lock(&self.reader);
        if let Some(r) = r.as_ref() {
            return Ok(r.clone());
        }
        let opened = Arc::new(
            DurableReader::open_with(
                &self.dir,
                &self.env,
                Duration::from_secs(30 * 60),
                self.clocks.clone(),
            )
            .map_err(|e| QueryError::NotAvailable {
                reason: e.to_string(),
            })?,
        );
        *r = Some(opened.clone());
        Ok(opened)
    }

    /// Clean shutdown: final commit and mark sync, threads joined, lock released.
    pub(crate) fn shutdown(&self) -> Result<(), WriteError> {
        self.closed.store(true, Ordering::SeqCst);
        let log = self
            .log
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(log) = log else {
            return Ok(());
        };
        let floor = log.stats().max_series_id;
        let shut = log.shutdown();
        // Every cursor file whose state changed, after the last mark sync; consumers waiting are woken.
        if let Some(h) = &self.hub {
            h.closed.store(true, Ordering::SeqCst);
            if shut.is_ok()
                && let Err(e) = h.run(|d, c| {
                    let r = d.flush_all(c);
                    d.close();
                    r
                })
            {
                h.failed(&e);
            }
            h.run(|_, _| ());
        }
        if let Err(f) = shut {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        if let Some(f) = self.hub.as_ref().and_then(|h| lock(&h.fault).clone()) {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        if let Some(f) = lock(&self.own_fault).clone() {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        // The registry file again, up to the mark the clean shutdown synced.
        let file = self.reader().ok().and_then(|r| {
            r.with(|i, log| Ok(i.registry_file(log.state().durable, floor)))
                .ok()
        });
        if let Some(file) = file {
            let wall = self.env.clock.wall().0;
            if let Err(e) =
                kvik_log::registry::write(&*self.env.fs, &self.dir, self.store_id, wall, &file)
            {
                let f = registry_fault(&self.dir, &e);
                *lock(&self.own_fault) = Some(f.clone());
                return Err(WriteError::ReadOnly(fault_out(&f)));
            }
        }
        Ok(())
    }

    /// Queues one entry and waits for its acknowledgement, holding the decision lock throughout, so no later
    /// decision sees an entry that is not yet durable. Applies it to the registry once acknowledged.
    fn commit_entry(&self, d: &mut Decider, entry: Entry) -> Result<LogAck, Error> {
        self.writable()?;
        let pending = {
            let log = self.log();
            let log = log.as_ref().ok_or(Error::Closed)?;
            log.submit(vec![Item::Entry(entry.clone())])
                .map_err(|e| self.submit_error(e))?
        };
        let ack = pending.wait().map_err(|e| match e {
            CommitError::ShutDown => Error::Closed,
            CommitError::ReadOnly => Error::ReadOnly("the store is read-only".into()),
            CommitError::OutcomeUnknown => Error::SyncFailed,
        })?;
        d.registry.apply(&entry);
        // Routing sees the entry now, at its position; consumers waiting are woken once the decision lock is
        // released, by the next call or the `kvik-deliver` thread.
        if let Some(h) = &self.hub {
            h.run_quiet(|dl, c| dl.pump(c));
        }
        Ok(ack)
    }

    fn submit_error(&self, e: SubmitError) -> Error {
        match e {
            SubmitError::ReadOnly => Error::ReadOnly("the store is read-only".into()),
            SubmitError::ShutDown => Error::Closed,
            SubmitError::Busy(w) => Error::Internal(format!("busy: {}", wait_out(w))),
            SubmitError::Call(c) => Error::Invalid {
                field: "entry",
                reason: c.to_string(),
            },
        }
    }

    // ----- series -----

    pub(crate) fn define(&self, spec: &SeriesSpec) -> Result<SeriesId, Error> {
        self.writable()?;
        let lateness = self.limits.default_lateness_bound;
        let mut d = lock(&self.decider);
        if let Some(s) = d.registry.resolve(&spec.name) {
            let new = registry::definition_of(spec, s.def.id, lateness)?;
            return match registry::difference(&new, &s.def) {
                None => Ok(SeriesId(s.def.id)),
                Some(field) => Err(Error::Conflict(field)),
            };
        }
        // Validate before an id is taken.
        registry::definition_of(spec, 1, lateness)?;
        let ceiling = self.limits.series_ceiling as usize;
        if d.registry.len() >= ceiling {
            return Err(Error::Ceiling(ceiling));
        }
        let floor = self.log().as_ref().map_or(0, |l| l.stats().max_series_id);
        let id = d
            .registry
            .next_id(floor)
            .ok_or(Error::Ceiling(u32::MAX as usize))?;
        let def = registry::definition_of(spec, id, lateness)?;
        self.commit_entry(&mut d, Entry::Definition(Box::new(def)))?;
        Ok(SeriesId(id))
    }

    fn live(&self, d: &Decider, id: SeriesId) -> Result<(), Error> {
        let s = d
            .registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        if s.retired() {
            return Err(Error::Retired(id));
        }
        Ok(())
    }

    pub(crate) fn alias(&self, id: SeriesId, name: &str) -> Result<(), Error> {
        self.writable()?;
        let mut d = lock(&self.decider);
        self.live(&d, id)?;
        if name.is_empty() || name.len() > 255 {
            return Err(Error::Invalid {
                field: "name",
                reason: "1 to 255 bytes".into(),
            });
        }
        // A name the series already carries is given again: it becomes the display name, as in the model.
        if d.registry.resolve(name).is_some_and(|s| s.def.id != id.0) {
            return Err(Error::NameInUse(name.into()));
        }
        // An alias that would close a routing cycle is refused (`spec.md` §4 Routing).
        if let Some(h) = &self.hub
            && let Some(c) = h.run(|dl, c| {
                dl.pump(c);
                dl.view().cycle_with(None, Some((id.0, name)))
            })
        {
            return Err(Error::RouteCycle(c.join(" -> ")));
        }
        self.commit_entry(
            &mut d,
            Entry::Alias {
                id: id.0,
                name: name.into(),
            },
        )
        .map(|_| ())
    }

    pub(crate) fn retire(&self, id: SeriesId, t_r: Timestamp) -> Result<(), Error> {
        self.writable()?;
        let mut d = lock(&self.decider);
        let s = d
            .registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        // Retiring a retired series changes nothing, as in the model.
        if s.retired() {
            return Ok(());
        }
        self.commit_entry(
            &mut d,
            Entry::Retirement {
                id: id.0,
                t_r: t_r.0,
            },
        )
        .map(|_| ())
    }

    pub(crate) fn set_parameters(
        &self,
        id: SeriesId,
        change: &ParameterChange,
    ) -> Result<(), Error> {
        self.writable()?;
        let mut d = lock(&self.decider);
        d.registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        if change.expected_interval.is_some()
            || change.staleness.is_some()
            || change.lateness_bound.is_some()
            || change.counter.is_some()
        {
            return Err(Error::NotSupportedYet(
                "parameter changes other than the retention class",
            ));
        }
        let Some(r) = change.retention else {
            return Ok(());
        };
        let (class, period) = match convert::retention(r)? {
            kvik_model::registry::RetentionClass::LongTerm => (0, None),
            kvik_model::registry::RetentionClass::Diagnostic { period } => (1, Some(period)),
        };
        self.commit_entry(
            &mut d,
            Entry::ParameterChange(LogChange {
                id: id.0,
                retention_class: Some(class),
                retention_period: period,
                ..LogChange::default()
            }),
        )
        .map(|_| ())
    }

    /// Decimal series are not supported yet: a raise or a policy change names an existing series or fails.
    pub(crate) fn decimal_only(&self, id: SeriesId) -> Result<(), Error> {
        self.writable()?;
        let d = lock(&self.decider);
        d.registry
            .get(id.0)
            .ok_or_else(|| Error::UnknownSeries(id.to_string()))?;
        Err(Error::NotSupportedYet(
            "decimal series, their scale raises and excess-precision policy",
        ))
    }

    pub(crate) fn set_site_zone(&self, zone: &ZoneName, effective: Timestamp) -> Result<(), Error> {
        self.writable()?;
        let z = zone.as_str();
        jiff::tz::TimeZone::get(z).map_err(|_| Error::UnknownZone(z.into()))?;
        if z.is_empty() || z.len() > 63 {
            return Err(Error::UnknownZone(z.into()));
        }
        let mut d = lock(&self.decider);
        self.commit_entry(
            &mut d,
            Entry::SiteZone {
                zone: z.into(),
                effective: effective.0,
            },
        )
        .map(|_| ())
    }

    pub(crate) fn series(&self, key: &SeriesKey) -> Result<SeriesInfo, QueryError> {
        self.reader()?.with(|i, _| i.series(key))
    }

    // ----- writing -----

    /// Decides a call and queues it in the log. With `wait`, waits while the ingest queue is full; without, a
    /// wait is [`EmitError::Busy`] with nothing queued.
    pub(crate) fn emit(
        self: &Arc<Self>,
        readings: &[Reading],
        intent: Intent,
        wait: bool,
    ) -> Result<Emitted, EmitError> {
        self.writable().map_err(|e| e.emit(&self.dir))?;
        // Timers due run before a call is decided, as the model backend runs them.
        self.run_timers();
        let bytes: usize = readings
            .iter()
            .map(|r| {
                32 + match &r.value {
                    Value::Text(t) => t.len(),
                    _ => 8,
                }
            })
            .sum();
        let limit = CALL_READINGS.min(self.limits.ingest_queue);
        if readings.len() > limit || bytes > CALL_BYTES {
            return Err(EmitError::TooLarge {
                readings: readings.len(),
                bytes,
            });
        }
        let clock = ingest::Clock {
            wall: self.env.clock.wall().0,
            synced: self.env.clock.synchronised(),
            future_tolerance: convert::dur_nanos(self.limits.future_tolerance).unwrap_or(i64::MAX),
        };
        // Host-stamped readings wait until the clock is synchronised (spec §2.5).
        if !clock.synced && readings.iter().any(|r| r.source == TimeSource::Host) {
            return self.busy(Wait::UnsyncedClock);
        }
        let mut d = lock(&self.decider);
        let call = self.next_call.fetch_add(1, Ordering::SeqCst);
        let plan = d.plan(readings, intent, clock, call);
        let started = Instant::now();
        let queued = if plan.items.is_empty() {
            None
        } else {
            // The log's read guard is dropped before an error is mapped, which reads the log's fault again.
            let r = {
                let log = self.log();
                let log = log.as_ref().ok_or(EmitError::Closed)?;
                if wait {
                    log.submit(plan.items.clone())
                } else {
                    log.try_submit(plan.items.clone())
                }
            };
            if wait {
                self.note_waited(started.elapsed());
            }
            match r {
                Ok(p) => Some(p),
                Err(SubmitError::Busy(w)) => {
                    drop(d);
                    return self.busy(wait_out(w));
                }
                Err(SubmitError::ReadOnly) => {
                    return Err(EmitError::ReadOnly(self.fault_now()));
                }
                Err(SubmitError::ShutDown) => return Err(EmitError::Closed),
                Err(SubmitError::Call(_)) => {
                    return Err(EmitError::TooLarge {
                        readings: readings.len(),
                        bytes,
                    });
                }
            }
        };
        d.apply(&plan);
        // The call is tracked before the decision lock is released, so a later call that duplicates one of its
        // readings always finds it (lock order: decisions, then calls in flight).
        let mut inflight = lock(&self.inflight);
        // Calls known to be acknowledged need no tracking: a later outcome resting on them is final.
        inflight.retain(|_, c| !c.acknowledged());
        let tracked = match queued {
            Some(pending) => {
                let (clocks, ids) = (self.clocks.clone(), refreshing(&plan));
                let c = Completion::new(
                    pending,
                    Box::new(move || {
                        for id in ids {
                            clocks.refresh(id);
                        }
                    }),
                );
                inflight.insert(call, c.clone());
                Some((c, true))
            }
            // Nothing to record. A duplicate of a reading queued by an earlier call that is not yet acknowledged
            // resolves when that call does, with its result (Q-123 (b)): an acknowledgement means durable.
            None => plan
                .depends
                .and_then(|c| inflight.get(&c).cloned())
                .map(|c| (c, false)),
        };
        drop(inflight);
        drop(d);
        #[cfg(test)]
        tests::after_decision(&self.dir);
        self.count(readings, &plan);
        let decisions = plan.decisions;
        let Some((completion, own)) = tracked else {
            return Ok(Emitted::Ready(Ack::new(outcomes(&decisions, None))));
        };
        let me = self.clone();
        Ok(Emitted::Queued(Queued {
            completion,
            finish: Box::new(move |r| {
                if own {
                    me.note_latency(started.elapsed());
                }
                if r.is_ok() {
                    me.deliver();
                }
                match r {
                    Ok(ack) if own => Ok(Ack::new(outcomes(&decisions, Some(ack.first)))),
                    Ok(_) => Ok(Ack::new(outcomes(&decisions, None))),
                    Err(CommitError::ShutDown) => Err(EmitError::Closed),
                    Err(CommitError::ReadOnly) => Err(EmitError::ReadOnly(me.fault_now())),
                    Err(CommitError::OutcomeUnknown) => {
                        Err(EmitError::OutcomeUnknown(me.fault_now()))
                    }
                }
            }),
        }))
    }

    /// The wait a call meets.
    fn busy(&self, w: Wait) -> Result<Emitted, EmitError> {
        Err(EmitError::Busy(w))
    }

    /// Counts a call that waits (`IngestStats::waits`), once per call.
    pub(crate) fn count_wait(&self, w: Wait) {
        *lock(&self.counters).waits.entry(w).or_insert(0) += 1;
    }

    /// Adds the time a queued call spent waiting for room in the ingest queue.
    fn note_waited(&self, d: Duration) {
        // A submit that found room at once did not wait.
        if d > Duration::from_millis(1) {
            lock(&self.counters).waited += d;
        }
    }

    fn note_latency(&self, d: Duration) {
        let mut c = lock(&self.counters);
        if c.latencies.len() == LATENCIES {
            c.latencies.pop_front();
        }
        c.latencies.push_back(d);
        c.max_latency = c.max_latency.max(d);
    }

    fn fault_now(&self) -> Fault {
        self.fault().map_or(
            Fault::SyncFailed {
                path: self.dir.clone(),
            },
            |f| fault_out(&f),
        )
    }

    fn count(&self, readings: &[Reading], plan: &ingest::Plan) {
        let mut c = lock(&self.counters);
        for (r, d) in readings.iter().zip(&plan.decisions) {
            let st = c.series.entry(r.series.0).or_default();
            match d {
                Decision::Refused(x) => *st.refused.entry(*x).or_insert(0) += 1,
                Decision::Duplicate => st.duplicates_dropped += 1,
                Decision::Record {
                    item,
                    outranked,
                    absent,
                } => {
                    st.recorded += 1;
                    st.outranked_on_arrival += u64::from(*outranked);
                    st.absent_supplied += u64::from(*absent);
                    if let Some(Item::Fact(row)) = plan.items.get(*item) {
                        st.late += u64::from(row.flags.late);
                        st.future += u64::from(row.flags.future);
                    }
                }
            }
        }
    }

    // ----- health and statistics -----

    pub(crate) fn losses(&self) -> Vec<LossReport> {
        let acked = lock(&self.acknowledged).clone();
        self.reader()
            .and_then(|r| r.losses())
            .unwrap_or_default()
            .into_iter()
            .map(|mut l| {
                l.acknowledged = acked.contains(&l.identity);
                l
            })
            .collect()
    }

    /// Acknowledges losses. Held in memory for this process: the manifest's acknowledgement record is not
    /// written yet.
    pub(crate) fn acknowledge_losses(&self, ids: &[Position]) -> Result<(), WriteError> {
        self.writable().map_err(|e| e.write(&self.dir))?;
        lock(&self.acknowledged).extend(ids.iter().copied());
        Ok(())
    }

    pub(crate) fn acknowledge_manifest_rebuild(&self) -> Result<(), WriteError> {
        self.writable().map_err(|e| e.write(&self.dir))
    }

    pub(crate) fn health(&self) -> Health {
        if self.closed.load(Ordering::SeqCst) {
            return Health::Closed;
        }
        let mut reasons: Vec<Reason> = Vec::new();
        if let Some(log) = self.log().as_ref() {
            for r in log.reasons() {
                reasons.push(match r {
                    LogReason::IngestWaiting(w) => Reason::IngestWaiting(wait_out(w)),
                    LogReason::DiskLow { free } => Reason::DiskLow { free },
                    LogReason::DiskCritical { free } => Reason::DiskCritical { free },
                    LogReason::SealBlocked { file_id } => Reason::SealBlocked { file_id },
                    LogReason::SegmentDamaged { file_id } => Reason::SegmentDamaged { file_id },
                });
            }
        }
        reasons.extend(
            self.losses()
                .into_iter()
                .filter(|l| l.kind == LossKind::DataLost && !l.acknowledged)
                .map(Reason::DataLost),
        );
        match (self.fault(), reasons.is_empty()) {
            (Some(f), _) => Health::ReadOnly {
                cause: fault_out(&f),
                reasons,
            },
            (None, true) => Health::Ok,
            (None, false) => Health::Degraded { reasons },
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        let log = self.log();
        let ls = log.as_ref().map(Log::stats);
        let waiting = log.as_ref().and_then(|l| {
            l.reasons().into_iter().find_map(|r| match r {
                LogReason::IngestWaiting(w) => Some(wait_out(w)),
                _ => None,
            })
        });
        let queue_depth = log.as_ref().map_or(0, Log::queue_depth);
        drop(log);
        let c = lock(&self.counters);
        let mut sorted: Vec<Duration> = c.latencies.iter().copied().collect();
        sorted.sort();
        let pct = |p: usize| {
            sorted
                .get((sorted.len() * p / 100).min(sorted.len().saturating_sub(1)))
                .copied()
                .unwrap_or_default()
        };
        let free = self.env.fs.free_space(&self.dir).unwrap_or(0);
        Stats {
            ingest: IngestStats {
                queue_depth,
                waiting,
                waited: c.waited,
                waits: c.waits.clone(),
                batches: ls.as_ref().map_or(0, |s| s.batches),
                syncs: ls.as_ref().map_or(0, |s| s.syncs),
                commit_latency: Latency {
                    p50: pct(50),
                    p99: pct(99),
                    max: c.max_latency,
                },
            },
            log: LogStats {
                durable: ls.as_ref().map_or(Position::ORIGIN, |s| pos(s.durable)),
                mark_synced: ls.as_ref().map_or(Position::ORIGIN, |s| pos(s.mark_synced)),
                mark_sync: match ls.as_ref().map(|s| s.mark_sync) {
                    Some(LogMarkSyncMode::EveryCommit { reason }) => MarkSyncMode::EveryCommit {
                        reason: format!("{reason:?}"),
                    },
                    _ => MarkSyncMode::Interval,
                },
                open_file_id: ls.as_ref().and_then(|s| s.open_file_id),
                sealed_segments: ls.as_ref().map_or(0, |s| s.sealed_segments),
                bytes: ls.as_ref().map_or(0, |s| s.bytes),
                manifest_rebuilds: 0,
                loss_identities_rebuilt: 0,
            },
            disk: DiskStats { free },
            clock: ClockStats::default(),
            delivery: self
                .hub
                .as_ref()
                .map(|h| h.run_quiet(|d, _| d.delivery_stats()))
                .unwrap_or_default(),
        }
    }

    pub(crate) fn series_stats(&self, id: SeriesId) -> Option<SeriesStats> {
        lock(&self.decider).registry.get(id.0)?;
        let mut st = lock(&self.counters)
            .series
            .get(&id.0)
            .cloned()
            .unwrap_or_default();
        if let Ok(r) = self.reader() {
            let _ = r.with(|i, _| {
                st.latest_t = i.latest_t(id.0).map(Timestamp);
                Ok(())
            });
        }
        Some(st)
    }

    /// Orphaned facts with positions in a range (spec §2.1).
    pub(crate) fn export_orphaned(
        &self,
        range: std::ops::RangeInclusive<Position>,
    ) -> Result<Vec<Fact>, QueryError> {
        self.reader()?.with(|i, _| Ok(orphaned(i, &range)))
    }
}

/// The fault of a failed registry file write.
fn registry_fault(dir: &Path, e: &io::Error) -> LogFault {
    LogFault {
        op: FaultOp::Write,
        path: dir.join(kvik_log::registry::REGISTRY_DIR),
        kind: e.kind(),
        message: e.to_string(),
    }
}

/// The ASCII release name of at most 15 bytes a tzdata entry and the store descriptor carry; anything else is
/// left out.
fn tzdata_name(v: &str) -> String {
    if v.is_empty() || v.len() > 15 || !v.is_ascii() {
        String::new()
    } else {
        v.to_string()
    }
}

fn writer_config(config: &StoreConfig) -> WriterConfig {
    let l = &config.log;
    let lim = &config.limits;
    WriterConfig {
        segment_size: l.segment_size,
        seal_interval: l.seal_interval,
        min_sync_interval: l.min_sync_interval,
        max_batch_bytes: l.max_batch_bytes,
        mark_sync: match l.mark_sync {
            MarkSync::Interval(d) => LogMarkSync::Interval(d),
            MarkSync::EveryCommit => LogMarkSync::EveryCommit,
        },
        ingest_queue: lim.ingest_queue,
        disk_low_percent: (lim.disk_low.fraction * 100.0).round() as u8,
        disk_low_bytes: lim.disk_low.bytes,
        disk_critical: lim.disk_critical,
        ..WriterConfig::default()
    }
}

/// One outcome per reading; positions follow the call's first, each recorded fact taking one.
fn outcomes(decisions: &[Decision], first: Option<kvik_log::writer::Position>) -> Vec<Outcome> {
    decisions
        .iter()
        .map(|d| match d {
            Decision::Refused(x) => Outcome::Refused(*x),
            Decision::Duplicate => Outcome::Duplicate,
            Decision::Record {
                item, outranked, ..
            } => {
                let f = first.unwrap_or_default();
                Outcome::Recorded {
                    position: Position::new(f.generation, f.sequence + *item as u64),
                    outranked: *outranked,
                    unchecked: false,
                    rounded: false,
                    out_of_range: false,
                }
            }
        })
        .collect()
}

fn orphaned(i: &Index, range: &std::ops::RangeInclusive<Position>) -> Vec<Fact> {
    let mut out: Vec<Fact> = i
        .orphaned()
        .keys()
        .flat_map(|id| i.series_facts(*id))
        .filter(|s| range.contains(&pos(s.position)))
        .map(fact_out)
        .collect();
    out.sort_by_key(|f| f.position);
    out
}

/// The series whose live staleness a call refreshes when the log acknowledges it, after the covering sync, whether
/// or not the caller waits (spec §5.4): those of its facts that are recorded, not outranked, not bad quality and not
/// future.
fn refreshing(plan: &ingest::Plan) -> Vec<u32> {
    plan.decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Record {
                item,
                outranked: false,
                ..
            } => match plan.items.get(*item) {
                Some(Item::Fact(row))
                    if row.quality != kvik_log::facts::Quality::Bad
                        && !matches!(row.value, kvik_log::facts::Value::Absent { .. })
                        && !row.flags.future =>
                {
                    Some(row.series_id)
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Reads of a durable store: the queries a log reader can answer.
pub(crate) fn facts(
    r: &DurableReader,
    id: SeriesId,
    range: TimeRange,
    view: View,
) -> Result<Facts, QueryError> {
    r.with(|i, _| i.facts(id, range, view)).map(Facts::new)
}

/// The latest fact at or before `t` that a window can credit (not bad quality), in a view, ignoring the
/// staleness limit.
pub(crate) fn at(
    r: &DurableReader,
    id: SeriesId,
    t: Timestamp,
    view: View,
) -> Result<Option<Fact>, QueryError> {
    let Some(end) = t.0.checked_add(1) else {
        return Ok(None);
    };
    r.with(|i, _| {
        let fs = i.facts(
            id,
            TimeRange::new(Timestamp(i64::MIN), Timestamp(end)),
            view,
        )?;
        Ok(fs
            .into_iter()
            .filter(|f| f.quality != crate::types::Quality::Bad)
            .max_by_key(|f| (f.t, f.position)))
    })
}

/// The interval of a fixed-length interval series that contains `t`.
pub(crate) fn interval(
    r: &DurableReader,
    id: SeriesId,
    t: Timestamp,
) -> Result<(Timestamp, Timestamp), QueryError> {
    r.with(|i, _| {
        let info = i.series(&SeriesKey::Id(id))?;
        match info.spec.kind {
            Kind::Interval {
                length: IntervalLength::Fixed { d, phase },
                ..
            } => {
                let (d, phase) = (
                    i128::from(convert::dur_nanos(d)?),
                    i128::from(convert::dur_nanos(phase)?),
                );
                let start = (i128::from(t.0) - phase).div_euclid(d) * d + phase;
                let outside = || Error::InvalidQuery("interval outside the time range".into());
                let end = i64::try_from(start + d).map_err(|_| outside())?;
                let start = i64::try_from(start).map_err(|_| outside())?;
                Ok((Timestamp(start), Timestamp(end)))
            }
            Kind::Interval { .. } => Err(Error::NotSupportedYet(
                "calendar-length interval series (a local day or month)",
            )),
            _ => Err(Error::InvalidQuery("not an interval series".into())),
        }
    })
}

/// The reader state of a durable reader.
pub(crate) fn refresh(r: &DurableReader) -> Result<ReaderState, QueryError> {
    r.refresh()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use super::{Emitted, Writer};
    use crate::config::{LogConfig, StoreConfig};
    use crate::env::{Env, ManualClock, MemFs};
    use crate::error::EmitError;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::types::{Intent, Reading, Value};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    /// A pause point right after a call's decision, for the store in the given directory.
    static AFTER_DECISION: Mutex<Option<(PathBuf, Hook)>> = Mutex::new(None);

    pub(super) fn after_decision(dir: &Path) {
        let hook = AFTER_DECISION
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(d, _)| d == dir)
            .map(|(_, h)| h.clone());
        if let Some(h) = hook {
            h();
        }
    }

    /// Polls a queued call to its end on this thread.
    fn wait(mut q: super::Queued) -> Result<crate::types::Ack, EmitError> {
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(r) = q.poll(&mut cx) {
                return q.finish(r);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A duplicate decided while the original's call has been decided but not yet returned still finds that call
    /// and waits for it: here the original's sync fails, so neither is acknowledged (Q-123 (b)).
    #[test]
    fn a_duplicate_decided_right_after_the_original_waits_for_it() {
        let fs = MemFs::new();
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let mut env = Env::test(clock.clone());
        env.fs = Arc::new(fs.clone());
        let log = LogConfig {
            segment_size: 16 << 20,
            ..LogConfig::default()
        };
        let dir = PathBuf::from("durable-unit/decided");
        let config = StoreConfig::new(&dir, "Europe/London")
            .create_if_missing(true)
            .env(env)
            .log(log);
        let w = Writer::open(&config).unwrap();
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let id = w.define(&spec).unwrap();
        let reading = [Reading::new(id, clock.wall(), Value::F64(1.0))];

        // The original's batch sync is held, then fails.
        let (sync_entered, sync_seen) = mpsc::channel::<()>();
        let (sync_release, sync_wait) = mpsc::channel::<()>();
        let sync_wait = Mutex::new(sync_wait);
        let sync_armed = Arc::new(AtomicBool::new(true));
        let failing = fs.clone();
        let armed = sync_armed.clone();
        fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
            let in_use = p
                .to_str()
                .is_some_and(|p| p.ends_with("-0000000000000001.open"));
            if in_use && armed.swap(false, Ordering::SeqCst) {
                sync_entered.send(()).unwrap();
                sync_wait.lock().unwrap().recv().unwrap();
                failing.fail_syncs(true);
            }
        })));
        // The original's call pauses right after its decision.
        let (paused_tx, paused) = mpsc::channel::<()>();
        let (resume, resume_rx) = mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let pause_armed = AtomicBool::new(true);
        *AFTER_DECISION.lock().unwrap() = Some((
            dir.clone(),
            Arc::new(move || {
                if pause_armed.swap(false, Ordering::SeqCst) {
                    paused_tx.send(()).unwrap();
                    resume_rx.lock().unwrap().recv().unwrap();
                }
            }),
        ));

        let w2 = w.clone();
        let first = reading.clone();
        let original = std::thread::spawn(move || match w2.emit(&first, Intent::Normal, false) {
            Ok(Emitted::Queued(q)) => wait(q),
            Ok(Emitted::Ready(a)) => Ok(a),
            Err(e) => Err(e),
        });
        paused
            .recv_timeout(Duration::from_secs(5))
            .expect("the original paused after its decision");
        // The duplicate, decided now, must wait for the original's call.
        let duplicate = match w.emit(&reading, Intent::Normal, false) {
            Ok(Emitted::Queued(q)) => q,
            Ok(Emitted::Ready(a)) => panic!("acknowledged before the original: {:?}", a.outcomes),
            Err(e) => panic!("{e:?}"),
        };
        resume.send(()).unwrap();
        sync_seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the original's sync started");
        sync_release.send(()).unwrap();
        assert!(matches!(wait(duplicate), Err(EmitError::OutcomeUnknown(_))));
        assert!(matches!(
            original.join().unwrap(),
            Err(EmitError::OutcomeUnknown(_))
        ));
        *AFTER_DECISION.lock().unwrap() = None;
        fs.set_sync_hook(None);
    }
}
