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
mod completion;
mod consumers;
mod deliver;
mod emit;
mod entries;
mod index;
mod ingest;
mod loss;
mod open;
pub(crate) mod reader;
mod registry;
mod routing;
mod status;
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
use completion::Completion;
pub(crate) use completion::{Emitted, Queued};
use entries::Decided;
use index::{Index, cause_out, fact_out, mark_state_out, pos};
use ingest::{Decider, Decision};
use loss::Latch;
pub(crate) use reader::DurableReader;
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
    /// The losses the reader of this process finds (Q-200).
    latch: Arc<Latch>,
    /// The losses the start found and could not record (`spec.md` §3 Order of a start, step 3).
    start_losses: Vec<LossReport>,
}

impl Writer {
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
                READER_LEASE,
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

    pub(crate) fn series(&self, key: &SeriesKey) -> Result<SeriesInfo, QueryError> {
        self.reader()?.with(|i, _| i.series(key))
    }
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
