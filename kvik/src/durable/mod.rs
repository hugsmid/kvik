//! The durable backend: the interface on the log of `kvik-log` (roadmap 5.1).
//!
//! `Store::open` opens the log writer, whose start recovers the log (`spec.md` §3 Order of a start), then reads the
//! kept log once with a log reader to build the series registry and the state the duplicate rule needs, and starts the
//! writer's threads. Every call that appends an entry is decided against that state and queued in the log; it resolves
//! when the log acknowledges it (after the sync and the mark update, and for an entry that is not a fact or the first
//! batch in a segment after the mark sync, Q-630). Reads go through a log reader, in this process or another, and so
//! see only what is durable. Live state, windows, chart buckets and calendar results are evaluated from the reader's
//! facts (roadmap 5.4, [`windows`]).
//!
//! Consumers, routing, condition funnels and durable cursors are in [`deliver`] and [`routing`] (roadmap 5.2).
//!
//! Not supported yet, with the roadmap item that brings each: starting a consumer at a position or a time and
//! saved points (5.3), windowed operators in funnels, stored summaries (7.x), decimal series, calendar-length
//! interval series and parameter changes other than the retention class, compaction (6.x), backup and restore (8.x).

mod by_id;
mod clock;
mod completion;
mod consumers;
mod deliver;
mod derived;
mod emit;
mod entries;
#[cfg(test)]
mod held_state;
mod history;
mod index;
mod ingest;
mod keys;
mod loss;
mod open;
#[cfg(test)]
mod pause;
mod read;
pub(crate) mod reader;
#[cfg(test)]
mod registering;
mod registry;
mod routing;
mod start;
mod status;
mod summary;
mod windows;

use std::collections::{BTreeMap, VecDeque};
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
use completion::Completion;
pub(crate) use completion::{Emitted, Queued};
use entries::Decided;
use index::{Index, cause_out, fact_out, mark_state_out, pos};
use ingest::{Decider, Decision};
use loss::Latch;
pub(crate) use read::latest::{live as live_at, series as series_at};
pub(crate) use read::view::View as SnapshotView;
pub(crate) use reader::DurableReader;
pub(crate) use reader::{copy as copy_error, losses_of as losses_at};
pub(crate) use windows::{buckets, live, window};

/// The most readings in one call, whatever the ingest queue (`docs/api.md` Writing).
const CALL_READINGS: usize = 65_536;
/// The most plain-encoded bytes in one call.
const CALL_BYTES: usize = 8 << 20;
/// Commit latencies kept for the percentiles.
const LATENCIES: usize = 1_024;
/// The lease refresh of the reader the writer's process keeps.
const READER_LEASE: Duration = Duration::from_secs(30 * 60);

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
    /// The start read the kept log whole into memory (Q-752).
    start_whole: bool,
    /// Damaged regions the start passed over that no lost range records (Q-755).
    start_regions: u64,
    /// Readings recorded unchecked because local data could not be read (Q-750).
    unchecked_readings: u64,
    waits: BTreeMap<Wait, u64>,
    waited: Duration,
    slow_path: u64,
    latencies: VecDeque<Duration>,
    max_latency: Duration,
}

/// A loss's first and last positions.
type LossRange = (Option<Position>, Option<Position>);

/// The writer of a durable store, shared by the clones of its `Store`.
#[derive(Debug)]
pub(crate) struct Writer {
    dir: PathBuf,
    env: Env,
    limits: Limits,
    log: RwLock<Option<Log>>,
    /// A fault of the start, or of a registry file write (`spec.md` §3 Write or sync failure).
    own_fault: Arc<Mutex<Option<LogFault>>>,
    store_id: [u8; 16],
    decider: Mutex<Decider>,
    /// The slow path's reads of local data, taken after the decision lock (roadmap 11.0).
    local: Mutex<Box<dyn keys::LocalFacts>>,
    counters: Arc<Mutex<Counters>>,
    reader: Mutex<Option<Arc<DurableReader>>>,
    /// Acknowledged `data_lost` identities, each with the range it had then (spec §8 Acknowledging a loss: a range
    /// that grows needs a new acknowledgement, and acknowledging none writes nothing).
    acknowledged: Mutex<BTreeMap<Position, LossRange>>,
    /// Calls queued and not yet known to be acknowledged, by call number (Q-123 (b)).
    inflight: Mutex<BTreeMap<u64, Arc<Completion>>>,
    next_call: AtomicU64,
    closed: AtomicBool,
    /// The window clock and the refresh instants of live staleness, shared with the reader of this process.
    clocks: Arc<clock::Clocks>,
    /// Consumers and routing; `None` when the start left the store read-only before the log could be read.
    hub: Option<Arc<deliver::Hub>>,
    /// The losses the scanning readers of this process find (Q-200).
    latch: Arc<Latch>,
    /// The reader `losses` and `health` move (Q-927).
    watch: Mutex<loss::Watch>,
    /// The losses the start found and could not record (`spec.md` §3 Order of a start, step 3).
    start_losses: Vec<LossReport>,
    /// The registry file's `through` as last written, and the lock of every write of the file (Q-1001).
    registry_written: derived::RegistryWritten,
    /// What attaching a bucket warns of under this configuration (Q-970).
    bucket_warning: Option<String>,
}

impl Writer {
    pub(crate) fn store_id(&self) -> [u8; 16] {
        self.store_id
    }

    pub(crate) fn bucket_warning(&self) -> Option<&str> {
        self.bucket_warning.as_deref()
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
                (READER_LEASE, READER_LEASE),
                self.clocks.clone(),
                self.latch.clone(),
            )
            .map_err(|e| QueryError::NotAvailable {
                reason: e.to_string(),
            })?,
        );
        *r = Some(opened.clone());
        Ok(opened)
    }

    /// From the deliverer's held state; the reader of the process only when there is no deliverer or its
    /// summaries could not be made exact.
    pub(crate) fn series(&self, key: &SeriesKey) -> Result<SeriesInfo, QueryError> {
        if let Some(r) = self.held(|d| d.series(key)) {
            return r.map_err(Error::query);
        }
        self.reader()?.with_latest(|v| read::latest::series(v, key))
    }
}

/// Each side's share of `limits.window_memory`: the writer's decision windows and the deliverer's windows (Q-751).
fn window_budget(l: &Limits) -> usize {
    usize::try_from(l.window_memory / 2).unwrap_or(usize::MAX)
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
    let info = r.with_latest(|v| read::latest::series(v, &SeriesKey::Id(id)))?;
    interval_of(&info, t).map_err(Error::query)
}

fn interval_of(info: &SeriesInfo, t: Timestamp) -> Result<(Timestamp, Timestamp), Error> {
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
}

/// What the reader held at a view's cut.
pub(crate) fn state_at(v: &SnapshotView) -> ReaderState {
    reader::state_of(v)
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

    use super::Writer;
    use super::completion::Emitted;
    use super::pause::{Gate, Point};
    use crate::config::{LogConfig, StoreConfig};
    use crate::env::{Env, ManualClock, MemFs};
    use crate::error::EmitError;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::types::{Intent, Reading, Value};

    #[test]
    fn each_side_gets_half_of_window_memory() {
        let limits = crate::config::Limits {
            window_memory: 64 << 20,
            ..crate::config::Limits::default()
        };
        assert_eq!(super::window_budget(&limits), 32 << 20);
    }

    /// Runs a future to its end on this thread.
    pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut f = std::pin::pin!(f);
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    /// Polls a queued call to its end on this thread.
    pub(super) fn wait(mut q: super::Queued) -> Result<crate::types::Ack, EmitError> {
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
        let paused = Gate::new(&dir, Point::AfterDecision);

        let w2 = w.clone();
        let first = reading.clone();
        let original = std::thread::spawn(move || match w2.emit(&first, Intent::Normal, false) {
            Ok(Emitted::Queued(q)) => wait(q),
            Ok(Emitted::Ready(a)) => Ok(a),
            Err(e) => Err(e),
        });
        paused.reached();
        // The duplicate, decided now, must wait for the original's call.
        let duplicate = match w.emit(&reading, Intent::Normal, false) {
            Ok(Emitted::Queued(q)) => q,
            Ok(Emitted::Ready(a)) => panic!("acknowledged before the original: {:?}", a.outcomes),
            Err(e) => panic!("{e:?}"),
        };
        paused.release();
        sync_seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the original's sync started");
        sync_release.send(()).unwrap();
        assert!(matches!(wait(duplicate), Err(EmitError::OutcomeUnknown(_))));
        assert!(matches!(
            original.join().unwrap(),
            Err(EmitError::OutcomeUnknown(_))
        ));
        fs.set_sync_hook(None);
    }
}
