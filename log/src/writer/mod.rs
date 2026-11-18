//! The commit path (`spec.md` §3 Commit, Group commit, Durable mark, Segments, Sealing, Write or sync failure, Disk
//! space; `format.md` A.4, A.10, A.12, A.13).
//!
//! The writer is a **synchronous core**, [`LogWriter`], that makes every decision and performs every file operation
//! of the `kvik-writer` thread, driven by explicit times: [`LogWriter::step`] takes the monotonic time and the wall
//! clock and returns the completed calls and the next time it needs to run. The `kvik-maint` thread's work is in
//! [`create_segment`] and [`seal_segment`]; the writer hands it [`SegmentRequest`]s and [`SealJob`]s and takes the
//! results back. [`Log`] runs both on their named threads; it adds no decisions of its own, so tests drive the core
//! deterministically with a manual clock and the in-memory file system.
//!
//! One batch is one sync: calls queued while a sync is in flight form the next batch. The writer writes a batch only
//! after the previous batch's sync has returned and the mark has moved (`spec.md` §3 Recovery rule 1).
//!
//! ```
//! use std::path::Path;
//! use std::sync::Arc;
//! use std::time::Duration;
//! use kvik_log::content::Item;
//! use kvik_log::entries::Entry;
//! use kvik_log::fs::MemFs;
//! use kvik_log::store::{NewStore, StoreDir};
//! use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};
//!
//! let new = NewStore {
//!     store_id: [1; 16],
//!     created_at: 0,
//!     site_zone: "Etc/UTC".into(),
//!     tzdata: "2026a".into(),
//!     software: "example".into(),
//! };
//! let store = StoreDir::open(Arc::new(MemFs::new()), Path::new("store"), Some(&new)).unwrap();
//! let writer = LogWriter::open(store, WriterConfig::default(), Some([2; 16]), Duration::ZERO, 0).unwrap();
//! let log = Log::start(writer, Arc::new(SystemClock::default())).unwrap();
//! let alias = Item::from(Entry::Alias { id: 1, name: "hall".into() });
//! // Acknowledged after the batch's sync and, as it holds an entry that is not a fact, the mark's sync.
//! let ack = log.submit(vec![alias]).unwrap().wait().unwrap();
//! assert_eq!(ack.first.sequence, 1);
//! log.shutdown().unwrap();
//! ```

mod gather;
mod generation;
mod maint;
mod options;
mod pacing;
mod queue;
pub mod recovery;
mod seg_index;
pub mod sources;
mod threads;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub use generation::init_generation;
pub use maint::{AheadSegment, CreateError, DescribeError, SealError, SealJob, SegmentRequest};
pub use maint::{create_segment, describe_sealed, seal_segment, seal_segment_described};
pub use options::{GenerationStart, SealListener, StartOptions};
pub use queue::{CallSource, IngestQueue, QueueFull};
pub use recovery::{EntryReport, FoundLoss, LossKind, StartReport, StartRule};
pub use threads::{Clock, Log, Pending, Room, SubmitError, SystemClock, Watch};

use crate::batch::SegmentIdentity;
use crate::block::{BLOCK_HEADER_LEN, block_type};
use crate::body::COMMIT_LEN;
use crate::content::{BatchContent, Item, ItemRef};
use crate::entries::{Entry, IntervalLength};
use crate::error::EncodeError;
use crate::fs::{Fs, FsFile};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::lease::LeaseTable;
use crate::mark::{MarkSlot, SLOT_OFFSETS};
use crate::segment::{SealSummary, SealedFile};
use crate::store::{LEASES_DIR, MARK, StoreDir, StoreError};
use pacing::{Budget, SyncPace};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// How often the writer checks free space.
pub const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long a creation that failed for lack of space waits before it is tried again when free space is unknown:
/// each try zero-fills until it fails, so it is not repeated every second (`spec.md` §3 Segments, Q-115 (d)).
pub const UNKNOWN_SPACE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// How long a blocked seal waits before it is tried again.
pub const SEAL_RETRY_INTERVAL: Duration = Duration::from_secs(10);
/// The wait before a description that failed to open or read its segment is tried again; it doubles at each further
/// failure up to [`DESCRIBE_RETRY_MAX`] (Q-221 (c)).
pub const DESCRIBE_RETRY_INTERVAL: Duration = Duration::from_secs(10);
/// The longest wait between tries of a description that fails to open or read its segment (Q-221 (c)).
pub const DESCRIBE_RETRY_MAX: Duration = Duration::from_secs(600);
/// The most entries in one call, whatever the queue's capacity (`api.md` Writing).
pub const MAX_CALL_ENTRIES: usize = 65_536;
const DAY_NS: i64 = 86_400_000_000_000;
/// The default `log.sync_interval_floor` (Q-710, Q-981).
pub const DEFAULT_SYNC_INTERVAL_FLOOR: Duration = Duration::from_millis(2);
/// The batch syncs the sync-rate budget holds: how many may start back to back after a quiet period (Q-985).
pub const SYNC_BURST: u64 = pacing::BURST;

/// When the mark file is synced (`api.md` `log.mark_sync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkSync {
    /// On a timer, `mark_sync_interval` (default 1 s), and at once after a batch holding an entry that is not a fact
    /// or the first batch in a segment (Q-630).
    Interval(Duration),
    /// After every mark update, before the batch is acknowledged.
    EveryCommit,
}

/// The writer's configuration (`api.md` Configuration, the `log.*` and `limits.*` keys of roadmap section 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterConfig {
    /// `log.segment_size`: 16 MiB to 1 GiB, a multiple of 4,096.
    pub segment_size: u64,
    /// `log.seal_interval`: a segment in use this long is sealed, if it holds a batch.
    pub seal_interval: Duration,
    /// `log.min_sync_interval`: the ceiling of the batch-sync interval, and the interval before the first sample;
    /// zero disables the sync-rate budget (spec §3 Group commit, Q-981).
    pub min_sync_interval: Duration,
    /// `log.sync_interval_floor`: the least batch-sync interval; at or above `min_sync_interval` the interval is
    /// fixed (spec §3 Group commit, Q-710, Q-981).
    pub sync_interval_floor: Duration,
    /// `log.max_batch_bytes`: 64 KiB to 8 MiB.
    pub max_batch_bytes: u64,
    /// `log.mark_sync`.
    pub mark_sync: MarkSync,
    /// `limits.ingest_queue`: at least 1,024 entries.
    pub ingest_queue: usize,
    /// `limits.disk_low`, its fraction, in per cent of the file system. Not applied yet: the file-system seam gives
    /// no size (Q-115 (e)).
    pub disk_low_percent: u8,
    /// `limits.disk_low`, its bytes.
    pub disk_low_bytes: u64,
    /// `limits.disk_critical`.
    pub disk_critical: u64,
    /// Lifts the lower limits of the sizes, for tests with small segments: a segment of at least 4,096 bytes, a
    /// multiple of 8; any batch size and queue capacity of at least 1.
    #[doc(hidden)]
    pub relaxed_limits: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            segment_size: 64 * MIB,
            seal_interval: Duration::from_secs(300),
            min_sync_interval: Duration::from_millis(10),
            sync_interval_floor: DEFAULT_SYNC_INTERVAL_FLOOR,
            max_batch_bytes: MIB,
            mark_sync: MarkSync::Interval(Duration::from_secs(1)),
            ingest_queue: 65_536,
            disk_low_percent: 5,
            disk_low_bytes: 2 * GIB,
            disk_critical: 512 * MIB,
            relaxed_limits: false,
        }
    }
}

/// A configuration value outside its limits.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("configuration key {0} is outside its limits")]
pub struct ConfigError(pub &'static str);

impl WriterConfig {
    /// Checks every key against its limits.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let relaxed = self.relaxed_limits;
        let seg_ok = if relaxed {
            self.segment_size >= 4096 && self.segment_size.is_multiple_of(8)
        } else {
            (16 * MIB..=GIB).contains(&self.segment_size) && self.segment_size.is_multiple_of(4096)
        };
        if !seg_ok || self.segment_size > GIB {
            return Err(ConfigError("log.segment_size"));
        }
        if self.seal_interval.is_zero() {
            return Err(ConfigError("log.seal_interval"));
        }
        let min_batch = if relaxed { 1 } else { 64 << 10 };
        if !(min_batch..=8 * MIB).contains(&self.max_batch_bytes) {
            return Err(ConfigError("log.max_batch_bytes"));
        }
        if matches!(self.mark_sync, MarkSync::Interval(d) if d.is_zero()) {
            return Err(ConfigError("log.mark_sync"));
        }
        if self.ingest_queue < if relaxed { 1 } else { 1024 } {
            return Err(ConfigError("limits.ingest_queue"));
        }
        if self.disk_low_percent > 100 {
            return Err(ConfigError("limits.disk_low"));
        }
        Ok(())
    }

    /// The most entries in one call: `min(65,536, limits.ingest_queue)`.
    pub fn call_limit(&self) -> usize {
        MAX_CALL_ENTRIES.min(self.ingest_queue)
    }
}

/// A position in the log: `(generation, sequence)` (`format.md` A, common rules).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// The generation.
    pub generation: u32,
    /// The sequence; 0 means none.
    pub sequence: u64,
}

/// One caller's facts and entries, committed in one batch or not at all, in order (`api.md` Writing: one call is
/// one batch unit).
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub(crate) id: u64,
    items: Vec<Item>,
    data_len: u64,
    sequences: u64,
    has_non_fact: bool,
    alone: bool,
}

/// Why a call cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// A call needs at least one entry.
    #[error("a call needs at least one entry")]
    Empty,
    /// More entries than the call limit, or more than 8 MiB of data blocks.
    #[error("the call is too large; split it")]
    TooLarge,
    /// Content that would not form valid blocks.
    #[error("invalid content: {0}")]
    Invalid(EncodeError),
}

impl Call {
    /// A call of `items`, at most `limit` entries ([`WriterConfig::call_limit`]). Its content is encoded once
    /// here, so a batch formed from valid calls always encodes.
    pub fn new(items: Vec<Item>, limit: usize) -> Result<Self, CallError> {
        if items.is_empty() {
            return Err(CallError::Empty);
        }
        if items.len() > limit.min(MAX_CALL_ENTRIES) {
            return Err(CallError::TooLarge);
        }
        let content = BatchContent {
            first_sequence: 1,
            received_at: 0,
            items,
        };
        let builder = content.builder().map_err(|e| match e {
            EncodeError::TooLarge => CallError::TooLarge,
            e => CallError::Invalid(e),
        })?;
        let items = content.items;
        let has_non_fact = items.iter().any(|i| matches!(i, Item::Entry(_)));
        let alone = items
            .iter()
            .any(|i| matches!(i, Item::Entry(e) if e.is_recovery()));
        Ok(Self {
            alone,
            id: 0,
            sequences: items.iter().map(Item::sequences).sum(),
            data_len: builder.data_len(),
            items,
            has_non_fact,
        })
    }

    /// The id the queue gave it; 0 before it was queued.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Its entries.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether it holds no entry (never, for a call made by [`Call::new`]).
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Bytes of its data blocks alone: what it adds to a batch's size (Q-115 (a)).
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Whether it holds an entry that is not a fact.
    pub fn has_non_fact(&self) -> bool {
        self.has_non_fact
    }

    /// Whether it must be alone in its batch: it holds a recovery entry.
    pub fn is_alone(&self) -> bool {
        self.alone
    }
}

/// The acknowledgement of a call: the positions of its first and last entries. Its entries hold consecutive
/// positions in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ack {
    /// The first entry's position.
    pub first: Position,
    /// The last entry's position.
    pub last: Position,
}

/// Why a call was not acknowledged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    /// Its batch was written but a write or sync failed before its acknowledgement: it may or may not be stored,
    /// and it is never acknowledged (`spec.md` §3 Write or sync failure).
    #[error(
        "the outcome is unknown: the store became read-only while the call was being committed"
    )]
    OutcomeUnknown,
    /// The store was read-only before the call was written: it was not stored.
    #[error("the store is read-only")]
    ReadOnly,
    /// The log was shut down before the call was written: it was not stored.
    #[error("the log was shut down")]
    ShutDown,
}

/// The operation that failed when the store became read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FaultOp {
    /// Creating a file.
    Create,
    /// Writing a log segment.
    Write,
    /// Syncing a log segment or the log directory.
    Sync,
    /// Writing the mark.
    MarkWrite,
    /// Syncing the mark.
    MarkSync,
    /// Reading or encoding a segment for its seal.
    Seal,
    /// A thread of the writer (`kvik-writer` or `kvik-maint`) panicked.
    Panic,
}

/// A write or sync failure: the store is read-only until a restart (`spec.md` §3 Write or sync failure).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{op:?} failed on {path:?}: {message}")]
pub struct Fault {
    /// What failed.
    pub op: FaultOp,
    /// The file or directory.
    pub path: PathBuf,
    /// The error's kind.
    pub kind: io::ErrorKind,
    /// The error's message.
    pub message: String,
}

/// The read-only flag shared by the writer and the maintenance work: set at the site of the first write or sync
/// failure, on either thread, and checked before every file operation, so none starts after a fault is known.
#[derive(Debug, Default)]
pub struct Halt {
    set: AtomicBool,
    fault: Mutex<Option<Fault>>,
}

impl Halt {
    /// Whether the store is read-only.
    pub fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }

    /// The first fault.
    pub fn fault(&self) -> Option<Fault> {
        self.fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records `f` unless a fault is already recorded, and sets the flag.
    pub fn set(&self, f: Fault) {
        let mut g = self.fault.lock().unwrap_or_else(PoisonError::into_inner);
        if g.is_none() {
            *g = Some(f);
        }
        self.set.store(true, Ordering::SeqCst);
    }
}

/// Why ingest waits (`api.md` Writing, back-pressure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wait {
    /// The ingest queue is full.
    QueueFull,
    /// Free space is below the critical mark, or a segment could not be created for lack of space.
    DiskCritical {
        /// Free bytes at the last check; 0 if unknown.
        free: u64,
    },
    /// The batch needs the next segment, which is not created yet.
    NoSegment,
}

/// A health reason the log reports (`api.md` Health).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reason {
    /// Ingest waits.
    IngestWaiting(Wait),
    /// Free space below the low mark.
    DiskLow {
        /// Free bytes.
        free: u64,
    },
    /// Free space below the critical mark, or a segment creation that failed for lack of space.
    DiskCritical {
        /// Free bytes; 0 if unknown.
        free: u64,
    },
    /// A segment's seal is blocked; it is retried.
    SealBlocked {
        /// The segment.
        file_id: u64,
    },
    /// A sealed segment no manifest record lists fails its checks (Q-221 (c)).
    SegmentDamaged {
        /// The segment.
        file_id: u64,
    },
    /// A sealed segment no manifest record lists still cannot be opened or read once its retry wait has reached
    /// [`DESCRIBE_RETRY_MAX`]; retries go on (Q-640).
    SegmentUnreadable {
        /// The segment.
        file_id: u64,
    },
    /// Never raised: a reason no other crate maps (Q-221 (d)).
    #[cfg(feature = "unmapped-reason")]
    #[doc(hidden)]
    Unmapped,
}

/// Why the mark is synced at every commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EveryCommitReason {
    /// Configured.
    Configured,
    /// The platform has no boot identity (`format.md` A.10).
    NoBootId,
}

/// How the mark is synced (`api.md` `LogStats::mark_sync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkSyncMode {
    /// On the timer, and at once after a batch holding an entry that is not a fact or the first batch in a segment.
    Interval(Duration),
    /// At every commit.
    EveryCommit {
        /// Why.
        reason: EveryCommitReason,
    },
}

/// The writer's statistics (`api.md` `LogStats`, `IngestStats`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogStats {
    /// The last durable position: synced, with the mark moved past it.
    pub durable: Position,
    /// The last position the mark file names on stable storage.
    pub mark_synced: Position,
    /// How the mark is synced.
    pub mark_sync: MarkSyncMode,
    /// The segment batches go to; `None` before the first is taken into use.
    pub open_file_id: Option<u64>,
    /// Segments sealed by this writer.
    pub sealed_segments: u64,
    /// Bytes of batches written.
    pub bytes: u64,
    /// Batches committed.
    pub batches: u64,
    /// Log syncs (batch syncs and the syncs that take a segment into use).
    pub syncs: u64,
    /// Mark syncs.
    pub mark_syncs: u64,
    /// The highest series id assigned.
    pub max_series_id: u32,
    /// The batch-sync interval in force: the budget refills one batch sync per interval (spec §3 Group commit).
    pub sync_interval: Duration,
    /// The smoothed duration of a batch sync; `None` before the first sample (Q-712).
    pub sync_time: Option<Duration>,
}

/// A finished call: its id and its result.
pub type Done = (u64, Result<Ack, CommitError>);

/// What a [`LogWriter::step`] did.
#[derive(Debug, Default)]
pub struct Step {
    /// Calls finished, with their ids.
    pub done: Vec<Done>,
    /// When the writer next needs to run, if no call arrives first; `None` when it needs no timer.
    pub next: Option<Duration>,
}

/// Why the writer could not open.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The store directory.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A [`StartOptions`] field that this build does not act on yet (roadmap 8.3c).
    #[error("not supported yet: {what}")]
    NotSupportedYet {
        /// The field.
        what: &'static str,
    },
}

#[derive(Debug)]
struct OpenSegment {
    file: Box<dyn FsFile>,
    id: SegmentIdentity,
    len: u64,
    write_offset: u64,
    summary: Option<SealSummary>,
    has_entries: bool,
    index: seg_index::SegIndex,
    in_use_since: Duration,
}

#[derive(Debug, Default)]
struct Disk {
    free: Option<u64>,
    checked_at: Option<Duration>,
    critical: bool,
    low: bool,
    /// A creation failed for lack of space; it is requested again when a check passes.
    creation_waits: bool,
    /// The first check after that failure, while free space is unknown: the retry comes
    /// [`UNKNOWN_SPACE_RETRY_INTERVAL`] later.
    waits_since: Option<Duration>,
}

/// The writer's synchronous core. See the module documentation.
#[derive(Debug)]
pub struct LogWriter {
    fs: Arc<dyn Fs>,
    store: StoreDir,
    log_dir: PathBuf,
    cfg: WriterConfig,
    store_id: [u8; 16],
    generation: u32,
    boot_id: [u8; 16],
    mark_mode: MarkSyncMode,
    mark: Option<Box<dyn FsFile>>,
    mark_path: PathBuf,
    mark_slot: MarkSlot,
    /// The slot that does not hold the last synced mark: every update until the next mark sync goes there.
    mark_next_slot: usize,
    /// An update was written since the last mark sync.
    mark_written: bool,
    mark_dirty: bool,
    mark_synced: u64,
    last_mark_sync: Duration,
    open: Option<OpenSegment>,
    ahead: Option<AheadSegment>,
    requested: bool,
    next_file_id: u64,
    last_sequence: u64,
    last_commit_time: i64,
    max_series_id: u32,
    intervals: HashMap<u32, IntervalLength>,
    /// The sync-rate budget (spec §3 Group commit, Q-980).
    budget: Budget,
    /// Times batch syncs for the interval; with none, the interval stays at `min_sync_interval`.
    sync_clock: Option<Arc<dyn Clock>>,
    pace: SyncPace,
    fault: Option<Fault>,
    halt: Arc<Halt>,
    seals: Vec<SealJob>,
    blocked: Vec<(SealJob, Duration)>,
    seal_blocked: BTreeSet<u64>,
    sealing: usize,
    /// Sealed segments no manifest record listed at the start, described one at a time (Q-221 (b)).
    unlisted: VecDeque<SegmentIdentity>,
    /// The segment being described, with the wait that preceded this try (zero for the first).
    describing: Option<(SegmentIdentity, Duration)>,
    /// Unlisted segments whose description failed to open or read them: when each is tried again, and the wait.
    describe_retry: Vec<(SegmentIdentity, Duration, Duration)>,
    /// Unlisted segments that failed a check this run (Q-221 (c)).
    damaged: BTreeSet<u64>,
    /// Unlisted segments whose retry wait has reached its cap without a read succeeding (Q-640).
    unreadable: BTreeSet<u64>,
    described: Vec<SealedFile>,
    disk: Disk,
    waiting: Option<Wait>,
    stats: LogStats,
    report: StartReport,
    leases: LeaseTable,
    /// The last batch's entries and calls, kept until the calls are acknowledged ([`LogWriter::release_batch`]).
    batch: Vec<Item>,
    batch_calls: Vec<Call>,
}

fn reach(t: i64, interval: Option<&IntervalLength>) -> i64 {
    match interval {
        None => t,
        Some(IntervalLength::Fixed { length, .. }) => t.saturating_add(*length),
        Some(IntervalLength::Day { .. }) => t.saturating_add(50 * DAY_NS / 24),
        Some(IntervalLength::Month { .. }) => t.saturating_add(33 * DAY_NS),
    }
}

impl LogWriter {
    /// Opens the writer on a store (`spec.md` §3 Order of a start): the first start of a new store creates the
    /// mark; any other start recovers the log ([`recovery`]) and resumes after what it kept. `boot_id` is `None`
    /// where the platform has none; the mark is then synced at every commit (`format.md` A.10). `now` and `wall` are
    /// the monotonic time and the wall clock. A start whose write or sync fails returns a read-only writer
    /// ([`LogWriter::fault`]) holding what it found ([`LogWriter::start_report`]).
    pub fn open(
        store: StoreDir,
        cfg: WriterConfig,
        boot_id: Option<[u8; 16]>,
        now: Duration,
        wall: i64,
    ) -> Result<Self, OpenError> {
        Self::open_with(store, cfg, boot_id, now, wall, StartOptions::default())
    }

    /// The start behind [`LogWriter::open_with`], once the options it does not act on yet are refused.
    fn start(
        store: StoreDir,
        cfg: WriterConfig,
        boot_id: Option<[u8; 16]>,
        now: Duration,
        wall: i64,
        mut opts: StartOptions,
    ) -> Result<Self, OpenError> {
        cfg.validate()?;
        let halt = Arc::new(Halt::default());
        let leases_dir = store.dir().join(LEASES_DIR);
        let r = recovery::start(&store, &cfg, boot_id, now, wall, &halt, &mut opts)?;
        let fs = store.fs().clone();
        let log_dir = store.log_dir();
        let store_id = store.descriptor().store_id;
        let generation = opts.generation.as_ref().map_or(0, |s| s.generation);
        let mark_mode = match (boot_id, cfg.mark_sync) {
            (None, _) => MarkSyncMode::EveryCommit {
                reason: EveryCommitReason::NoBootId,
            },
            (Some(_), MarkSync::EveryCommit) => MarkSyncMode::EveryCommit {
                reason: EveryCommitReason::Configured,
            },
            (Some(_), MarkSync::Interval(d)) => MarkSyncMode::Interval(d),
        };
        let at = |sequence| Position {
            generation,
            sequence,
        };
        let synced = if r.mark_dirty {
            0
        } else {
            r.mark_slot.last_sequence
        };
        // At the start every lease of this boot counts as first observed now; void ones are deleted (§7 Leases).
        let mut leases = LeaseTable::new(boot_id.unwrap_or_default());
        if r.fault.is_none() {
            let _ = leases.scan(&*store.fs().clone(), &leases_dir, &store_id, now);
        }
        Ok(Self {
            leases,
            stats: LogStats {
                durable: at(r.last_sequence),
                mark_synced: at(synced),
                mark_sync: mark_mode,
                open_file_id: r.open.as_ref().map(|o| o.id.file_id),
                sealed_segments: 0,
                bytes: 0,
                batches: 0,
                syncs: 0,
                mark_syncs: 0,
                max_series_id: r.max_series_id,
                sync_interval: SyncPace::default()
                    .interval(cfg.sync_interval_floor, cfg.min_sync_interval),
                sync_time: None,
            },
            fs,
            store,
            mark_path: log_dir.join(MARK),
            log_dir,
            cfg,
            store_id,
            generation,
            boot_id: boot_id.unwrap_or_default(),
            mark_mode,
            mark: r.mark,
            mark_slot: r.mark_slot,
            mark_next_slot: r.mark_next_slot,
            mark_written: false,
            mark_dirty: r.mark_dirty,
            mark_synced: synced,
            last_mark_sync: now,
            open: r.open,
            ahead: r.ahead,
            requested: false,
            next_file_id: r.next_file_id,
            last_sequence: r.last_sequence,
            last_commit_time: r.last_commit_time,
            max_series_id: r.max_series_id,
            intervals: r.intervals,
            budget: Budget::default(),
            sync_clock: None,
            pace: SyncPace::default(),
            fault: r.fault,
            halt,
            seals: r.seals,
            blocked: Vec::new(),
            seal_blocked: BTreeSet::new(),
            sealing: 0,
            unlisted: r.unlisted.into(),
            describing: None,
            describe_retry: Vec::new(),
            damaged: BTreeSet::new(),
            unreadable: BTreeSet::new(),
            described: Vec::new(),
            disk: Disk::default(),
            waiting: None,
            report: r.report,
            batch: Vec::new(),
            batch_calls: Vec::new(),
        })
    }

    /// What the start found and did (`spec.md` §3 Recovery).
    pub fn start_report(&self) -> &StartReport {
        &self.report
    }

    /// The fault that made the store read-only, if any.
    pub fn fault(&self) -> Option<&Fault> {
        self.fault.as_ref()
    }

    /// The read-only flag to pass to [`create_segment`] and [`seal_segment`].
    pub fn halt(&self) -> Arc<Halt> {
        self.halt.clone()
    }

    /// Takes over a fault recorded by the maintenance work.
    fn sync_halt(&mut self) {
        if self.fault.is_none() && self.halt.is_set() {
            self.fault = self.halt.fault();
            self.waiting = None;
        }
    }

    /// The statistics.
    pub fn stats(&self) -> &LogStats {
        &self.stats
    }

    /// The configuration.
    pub fn config(&self) -> &WriterConfig {
        &self.cfg
    }

    /// The store directory.
    pub fn store(&self) -> &StoreDir {
        &self.store
    }

    /// Why ingest waits, if it does.
    pub fn waiting(&self) -> Option<Wait> {
        self.waiting
    }

    /// The health reasons (a fault is reported by [`LogWriter::fault`]).
    pub fn reasons(&self) -> Vec<Reason> {
        let mut r = Vec::new();
        if let Some(w) = self.waiting {
            r.push(Reason::IngestWaiting(w));
        }
        let free = self.disk.free.unwrap_or(0);
        if self.disk.critical || self.disk.creation_waits {
            r.push(Reason::DiskCritical { free });
        } else if self.disk.low {
            r.push(Reason::DiskLow { free });
        }
        r.extend(
            self.seal_blocked
                .iter()
                .map(|&file_id| Reason::SealBlocked { file_id }),
        );
        r.extend(
            self.damaged
                .iter()
                .map(|&file_id| Reason::SegmentDamaged { file_id }),
        );
        r.extend(
            self.unreadable
                .iter()
                .map(|&file_id| Reason::SegmentUnreadable { file_id }),
        );
        r
    }

    /// Times each batch sync on `clock`, so that the interval follows the device (spec §3 Group commit, Q-712);
    /// [`Log::start`] passes its clock.
    pub fn set_sync_clock(&mut self, clock: Arc<dyn Clock>) {
        self.sync_clock = Some(clock);
    }

    /// The batch-sync interval in force: `min_sync_interval` before the first sample (Q-712).
    pub fn sync_interval(&self) -> Duration {
        self.pace
            .interval(self.cfg.sync_interval_floor, self.cfg.min_sync_interval)
    }

    /// The earliest time the next batch sync may start: at once while the sync-rate budget holds a sync, else when
    /// its refill reaches one (spec §3 Group commit, Q-980).
    pub fn sync_ready_at(&self) -> Duration {
        self.budget.ready_at(self.sync_interval())
    }

    fn fail(&mut self, op: FaultOp, path: &Path, e: &io::Error) {
        self.fail_with(Fault {
            op,
            path: path.to_path_buf(),
            kind: e.kind(),
            message: e.to_string(),
        });
    }

    fn fail_with(&mut self, f: Fault) {
        self.halt.set(f);
        if self.fault.is_none() {
            self.fault = self.halt.fault();
        }
        self.waiting = None;
    }

    /// The next segment to create ahead of use, if one is wanted and not already asked for: always one beyond the
    /// segment in use, except while a creation waits for free space.
    pub fn segment_request(&mut self, wall: i64) -> Option<SegmentRequest> {
        self.sync_halt();
        if self.fault.is_some()
            || self.ahead.is_some()
            || self.requested
            || self.disk.creation_waits
        {
            return None;
        }
        self.requested = true;
        let id = SegmentIdentity {
            store_id: self.store_id,
            file_id: self.next_file_id,
            generation: self.generation,
        };
        self.next_file_id += 1;
        Some(SegmentRequest {
            id,
            log_dir: self.log_dir.clone(),
            len: self.cfg.segment_size,
            created_at: wall,
        })
    }

    /// The result of a [`SegmentRequest`]. A creation that failed for lack of space is asked for again when a
    /// free-space check passes (free space at least `disk_critical` + `segment_size`, checked every second; with free
    /// space unknown, [`UNKNOWN_SPACE_RETRY_INTERVAL`] later); any other failure makes the store read-only.
    pub fn segment_created(&mut self, result: Result<AheadSegment, CreateError>) {
        self.requested = false;
        match result {
            Ok(s) if self.fault.is_none() => {
                self.ahead = Some(s);
                if self.waiting == Some(Wait::NoSegment) {
                    self.waiting = None;
                }
            }
            Ok(_) => {}
            Err(CreateError::NoSpace) => {
                // The id is not reused: the next request takes a new one (A.12: never reused).
                self.disk.creation_waits = true;
                self.disk.checked_at = None;
                self.disk.waits_since = None;
            }
            Err(CreateError::Fault(f)) => self.fail_with(f),
            Err(CreateError::Halted) => self.sync_halt(),
        }
    }

    /// Seal jobs ready to run: segments whose successor is in use and synced, and blocked seals whose retry time
    /// has come.
    pub fn take_seal_jobs(&mut self, now: Duration) -> Vec<SealJob> {
        self.sync_halt();
        if self.fault.is_some() {
            return Vec::new();
        }
        let mut out = std::mem::take(&mut self.seals);
        let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.blocked)
            .into_iter()
            .partition(|(_, at)| *at <= now);
        self.blocked = wait;
        out.extend(due.into_iter().map(|(j, _)| j));
        self.sealing += out.len();
        out
    }

    /// One sealed segment that no manifest record listed at the start, to describe with [`describe_sealed`] in
    /// [`LogWriter::log_dir`]: only while no seal is ready or running, none is being described and the store is
    /// writable (Q-221 (b)). A segment whose retry time has come goes first.
    pub fn take_unlisted(&mut self, now: Duration) -> Option<SegmentIdentity> {
        self.sync_halt();
        if self.fault.is_some() || self.describing.is_some() || self.seals_running() {
            return None;
        }
        let due = self
            .describe_retry
            .iter()
            .enumerate()
            .filter(|(_, (_, at, _))| *at <= now)
            .min_by_key(|(_, (_, at, _))| *at)
            .map(|(i, _)| i);
        let (id, waited) = match due {
            Some(i) => {
                let (id, _, waited) = self.describe_retry.swap_remove(i);
                (id, waited)
            }
            None => (self.unlisted.pop_front()?, Duration::ZERO),
        };
        self.describing = Some((id, waited));
        Some(id)
    }

    /// The result of describing a segment [`LogWriter::take_unlisted`] handed out (Q-221 (c)). A failure to open or
    /// read it is tried again after [`DESCRIBE_RETRY_INTERVAL`], doubled at each further failure up to
    /// [`DESCRIBE_RETRY_MAX`]; once the wait reaches that cap health is degraded as [`Reason::SegmentUnreadable`]
    /// until a read succeeds (Q-640). A segment that fails its checks stays unlisted, sets no fault and degrades
    /// health as [`Reason::SegmentDamaged`] until the writer stops; the next start tries it again.
    pub fn unlisted_done(&mut self, result: Result<SealedFile, DescribeError>, now: Duration) {
        let Some((id, waited)) = self.describing.take() else {
            return;
        };
        if !matches!(result, Err(DescribeError::Io(_))) {
            self.unreadable.remove(&id.file_id);
        }
        match result {
            Ok(f) => self.described.push(f),
            Err(DescribeError::Io(_)) => {
                let wait = if waited.is_zero() {
                    DESCRIBE_RETRY_INTERVAL
                } else {
                    waited.saturating_mul(2).min(DESCRIBE_RETRY_MAX)
                };
                if wait >= DESCRIBE_RETRY_MAX {
                    self.unreadable.insert(id.file_id);
                }
                self.describe_retry
                    .push((id, now.saturating_add(wait), wait));
            }
            Err(DescribeError::NotSealed | DescribeError::Invalid(_)) => {
                self.damaged.insert(id.file_id);
            }
        }
    }

    /// The log directory.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// The result of a seal job.
    pub fn seal_done(
        &mut self,
        job: SealJob,
        result: Result<SealedFile, SealError>,
        now: Duration,
    ) {
        self.sealing = self.sealing.saturating_sub(1);
        match result {
            Ok(f) => {
                self.seal_blocked.remove(&job.file_id());
                self.stats.sealed_segments += 1;
                self.described.push(f);
            }
            Err(SealError::Blocked(_)) => {
                self.seal_blocked.insert(job.file_id());
                self.blocked.push((job, now + SEAL_RETRY_INTERVAL));
            }
            Err(SealError::Fault(f)) => self.fail_with(f),
            Err(SealError::Halted) => self.sync_halt(),
        }
    }

    /// The descriptions of segments sealed, and of sealed segments no manifest record listed at the start, since the
    /// last call, each once (Q-220 (d)). None leaves before the start's lost-range records are synced (Q-290).
    pub fn take_sealed(&mut self) -> Vec<SealedFile> {
        std::mem::take(&mut self.described)
    }

    /// Whether seal jobs are ready to run or running; blocked seals waiting for their retry are not counted.
    pub fn seals_running(&self) -> bool {
        !self.seals.is_empty() || self.sealing > 0
    }

    fn check_disk(&mut self, now: Duration) {
        if self
            .disk
            .checked_at
            .is_some_and(|at| now < at + DISK_CHECK_INTERVAL)
        {
            return;
        }
        self.disk.checked_at = Some(now);
        // Free space unknown (no source on this platform, or the call failed): no mark applies (Q-115 (e)).
        let free = self.fs.free_space(&self.log_dir).ok();
        self.disk.free = free;
        self.disk.critical = free.is_some_and(|f| f < self.cfg.disk_critical);
        self.disk.low = free.is_some_and(|f| f < self.cfg.disk_low_bytes);
        if self.disk.creation_waits {
            let passes = match free {
                Some(f) => f >= self.cfg.disk_critical + self.cfg.segment_size,
                None => {
                    let since = *self.disk.waits_since.get_or_insert(now);
                    now >= since + UNKNOWN_SPACE_RETRY_INTERVAL
                }
            };
            if passes {
                self.disk.creation_waits = false;
                self.disk.waits_since = None;
            }
        }
    }

    fn ingest_blocked_by_disk(&self) -> bool {
        self.disk.critical || self.disk.creation_waits
    }

    /// One turn of the writer thread at monotonic time `now` and wall clock `wall`: checks free space, syncs the
    /// mark when its timer is due, seals a segment by time, and commits one batch from `source` if a log sync may
    /// start. After a fault, every call in `source` fails with [`CommitError::ReadOnly`]. The previous turn's
    /// batch is freed first.
    pub fn step(&mut self, now: Duration, wall: i64, source: &mut dyn CallSource) -> Step {
        self.release_batch();
        let mut step = Step::default();
        self.sync_halt();
        if self.fault.is_none() {
            self.check_disk(now);
            self.timed_mark_sync(now);
        }
        if self.fault.is_none() {
            self.timed_seal(now);
        }
        // A read-only store starts no file operation, the deletion of a lease included.
        if self.fault.is_none() {
            self.scan_leases(now);
        }
        if self.fault.is_none() {
            let disk_wait = self.ingest_blocked_by_disk();
            if disk_wait && !source.is_empty() {
                self.waiting = Some(Wait::DiskCritical {
                    free: self.disk.free.unwrap_or(0),
                });
            } else if self
                .waiting
                .is_some_and(|w| matches!(w, Wait::DiskCritical { .. }))
            {
                self.waiting = None;
            }
            if !disk_wait && !source.is_empty() && now >= self.sync_ready_at() {
                let calls = source.take(self.cfg.max_batch_bytes);
                let mut back = Vec::new();
                match self.commit(calls, now, wall, &mut back) {
                    Some(done) => step.done = done,
                    None => self.waiting = Some(Wait::NoSegment),
                }
                if !back.is_empty() {
                    source.put_back(back);
                }
            }
        }
        if self.fault.is_some() {
            let mut rest = Vec::new();
            while !source.is_empty() {
                rest.extend(source.take(u64::MAX));
            }
            step.done
                .extend(rest.into_iter().map(|c| (c.id, Err(CommitError::ReadOnly))));
            return step;
        }
        step.next = self.next_deadline(now, !source.is_empty());
        step
    }

    fn next_deadline(&self, now: Duration, queued: bool) -> Option<Duration> {
        let mut next: Option<Duration> = None;
        let mut at = |t: Duration| next = Some(next.map_or(t, |n| n.min(t)));
        if queued && self.waiting != Some(Wait::NoSegment) {
            if self.ingest_blocked_by_disk() {
                at(self.disk.checked_at.unwrap_or(now) + DISK_CHECK_INTERVAL);
            } else {
                at(self.sync_ready_at().max(now));
            }
        }
        if self.disk.creation_waits {
            match (self.disk.free, self.disk.waits_since) {
                (None, Some(since)) => at(since + UNKNOWN_SPACE_RETRY_INTERVAL),
                _ => at(self.disk.checked_at.unwrap_or(now) + DISK_CHECK_INTERVAL),
            }
        }
        if let (true, MarkSyncMode::Interval(d)) = (self.mark_dirty, self.mark_mode) {
            at(self.last_mark_sync + d);
        }
        // A timed seal needs the segment created ahead; without it the writer waits for the creation, which
        // wakes it, rather than for the seal time.
        if let Some(o) = self
            .open
            .as_ref()
            .filter(|o| o.summary.is_some() && self.ahead.is_some())
        {
            at(o.in_use_since + self.cfg.seal_interval);
        }
        if let Some(t) = self.blocked.iter().map(|(_, t)| *t).min() {
            at(t);
        }
        // Only while `take_unlisted` could hand the retry out; the description or seal that closes that gate wakes
        // the writer when it finishes, so a due retry never spins the loop.
        if self.fault.is_none()
            && self.describing.is_none()
            && !self.seals_running()
            && let Some(t) = self.describe_retry.iter().map(|(_, t, _)| *t).min()
        {
            at(t);
        }
        next
    }

    /// Looks at the lease files when a look is due; an idle writer looks at its next turn. Failures are left for
    /// the next look: a late look only makes an expiry later.
    fn scan_leases(&mut self, now: Duration) {
        if self.leases.scan_due(now) {
            let dir = self.store.dir().join(LEASES_DIR);
            let _ = self.leases.scan(&*self.fs, &dir, &self.store_id, now);
        }
    }

    /// The reader leases as last seen (`spec.md` §7 Leases).
    pub fn leases(&self) -> &LeaseTable {
        &self.leases
    }

    /// Syncs the mark now if an update since its last sync is not on stable storage, whatever its timer: for a
    /// caller about to write a file that names a recent position (`spec.md` §3 Durable mark).
    pub fn sync_mark_now(&mut self, now: Duration) {
        if self.mark_dirty && self.fault.is_none() {
            self.sync_mark(now);
        }
    }

    fn timed_mark_sync(&mut self, now: Duration) {
        if let (true, MarkSyncMode::Interval(d)) = (self.mark_dirty, self.mark_mode)
            && now >= self.last_mark_sync + d
        {
            self.sync_mark(now);
        }
    }

    /// Whether the store is read-only, a fault recorded by the maintenance work included. Checked before every
    /// file operation of the writer, so none starts once a fault on either thread is known.
    fn halted(&mut self) -> bool {
        self.sync_halt();
        self.fault.is_some()
    }

    fn sync_mark(&mut self, now: Duration) {
        if self.halted() {
            return;
        }
        // A writer that is not read-only holds the mark file.
        let Some(mark) = self.mark.as_ref() else {
            return;
        };
        if let Err(e) = mark.sync_data() {
            let p = self.mark_path.clone();
            self.fail(FaultOp::MarkSync, &p, &e);
            return;
        }
        // The slot written since the last sync now holds the last synced mark.
        if self.mark_written {
            self.mark_next_slot = 1 - self.mark_next_slot;
            self.mark_written = false;
        }
        self.mark_dirty = false;
        self.last_mark_sync = now;
        self.mark_synced = self.mark_slot.last_sequence;
        self.stats.mark_synced = Position {
            generation: self.generation,
            sequence: self.mark_synced,
        };
        self.stats.mark_syncs += 1;
    }

    /// A segment in use for `seal_interval` with at least one batch is sealed: the segment created ahead is taken
    /// into use by its header rewrite and its own `sync_data`, and the old one goes to be sealed (`format.md` A.4
    /// In use). Without a segment created ahead, the seal waits for one.
    fn timed_seal(&mut self, now: Duration) {
        let due = self
            .open
            .as_ref()
            .is_some_and(|o| o.summary.is_some() && now >= o.in_use_since + self.cfg.seal_interval);
        if !due || self.ahead.is_none() || self.halted() {
            return;
        }
        let Some(next) = self.take_into_use(self.last_sequence + 1, now) else {
            return;
        };
        if self.halted() {
            return;
        }
        if let Err(e) = next.file.sync_data() {
            let p = maint::segment_path(&self.log_dir, &next.id, false);
            self.fail(FaultOp::Sync, &p, &e);
            return;
        }
        // Not a batch sync: it spends nothing of the budget (Q-982).
        self.stats.syncs += 1;
        self.roll_to(next);
    }

    /// Writes the header of the segment created ahead with its real `base_sequence`; the caller syncs it.
    fn take_into_use(&mut self, base_sequence: u64, now: Duration) -> Option<OpenSegment> {
        let a = self.ahead.take()?;
        let mut h = FileHeader::new(
            kind::LOG_SEGMENT,
            a.id.generation,
            a.id.file_id,
            a.created_at,
            a.id.store_id,
        );
        h.base_sequence = base_sequence;
        if let Err(e) = a.file.write_at(&h.encode(), 0) {
            self.fail(FaultOp::Write, &a.path, &e);
            return None;
        }
        Some(OpenSegment {
            file: a.file,
            id: a.id,
            len: a.len,
            write_offset: HEADER_LEN as u64,
            summary: None,
            has_entries: false,
            index: seg_index::SegIndex::default(),
            in_use_since: now,
        })
    }

    /// Makes `next` the segment in use; the old one, if it holds a batch, goes to be sealed.
    fn roll_to(&mut self, next: OpenSegment) {
        self.stats.open_file_id = Some(next.id.file_id);
        if let Some(old) = self.open.replace(next)
            && let Some(summary) = old.summary
        {
            self.seals.push(SealJob {
                end: old.write_offset,
                file: old.file,
                id: old.id,
                log_dir: self.log_dir.clone(),
                summary,
                records: old.index.into_records(),
                has_entries: old.has_entries,
                prepared: Default::default(),
            });
        }
    }

    /// Commits one batch of `calls`: write, sync, mark update, the mark sync where required, then the
    /// acknowledgements. Calls left out of the batch go to `back`, in order: all of them, with `None`, if the batch
    /// needs the next segment and none is created yet.
    fn commit(
        &mut self,
        mut calls: Vec<Call>,
        now: Duration,
        wall: i64,
        back: &mut Vec<Call>,
    ) -> Option<Vec<Done>> {
        let mut items = Vec::with_capacity(calls.iter().map(Call::len).sum());
        let counts = gather::gather(&mut calls, &mut items);
        let mut content = BatchContent {
            first_sequence: self.last_sequence + 1,
            received_at: wall.max(self.last_commit_time),
            items,
        };
        let done = self.commit_content(&mut calls, counts, &mut content, now, back);
        self.batch = content.items;
        self.batch_calls = calls;
        done
    }

    /// [`LogWriter::commit`] of `content`, the entries of `calls` (`counts` of each) moved into it; a call put back
    /// gets its entries back.
    fn commit_content(
        &mut self,
        calls: &mut Vec<Call>,
        mut counts: Vec<usize>,
        content: &mut BatchContent,
        now: Duration,
        back: &mut Vec<Call>,
    ) -> Option<Vec<Done>> {
        let first = content.first_sequence;
        let commit_time = content.received_at;
        // Calls that encode alone may still not encode together (merged blocks that pass 8 MiB, or a recovery entry
        // that must be alone in its batch): the batch then takes the first call only. A lone call encodes as it did
        // when it was made, whatever its first sequence.
        let builder = match content.builder() {
            Ok(b) => b,
            Err(_) if calls.len() > 1 => {
                gather::give_back(&mut calls[1..], &counts[1..], &mut content.items);
                back.extend(calls.drain(1..));
                counts.truncate(1);
                content.builder().expect("a valid call encodes alone")
            }
            Err(_) => unreachable!("a valid call encodes alone"),
        };
        let batch_len = builder.data_len() + (BLOCK_HEADER_LEN + COMMIT_LEN) as u64;
        let fits = self
            .open
            .as_ref()
            .is_some_and(|o| o.len - o.write_offset >= batch_len);
        let mut next = None;
        if !fits {
            if self.ahead.is_none() {
                gather::give_back(calls, &counts, &mut content.items);
                let rest = std::mem::take(back);
                back.append(calls);
                back.extend(rest);
                return None;
            }
            if self.halted() {
                return Some(Self::finished(calls, CommitError::ReadOnly));
            }
            next = self.take_into_use(first, now);
            if next.is_none() {
                return Some(Self::unknown(calls));
            }
        }
        // Nothing of this batch is written yet: a fault known now leaves it unstored.
        if self.halted() {
            return Some(Self::finished(calls, CommitError::ReadOnly));
        }
        let seg = next
            .as_ref()
            .or(self.open.as_ref())
            .expect("a segment in use");
        let at = seg.write_offset;
        let bytes = builder
            .encode(&seg.id, at, commit_time)
            .expect("a batch of valid calls encodes");
        let path = maint::segment_path(&self.log_dir, &seg.id, false);
        if let Err(e) = seg.file.write_at(&bytes, at) {
            self.fail(FaultOp::Write, &path, &e);
            return Some(Self::unknown(calls));
        }
        if self.halted() {
            return Some(Self::unknown(calls));
        }
        self.budget.spend(now, self.sync_interval());
        self.stats.syncs += 1;
        let seg = next
            .as_ref()
            .or(self.open.as_ref())
            .expect("a segment in use");
        let before = self.sync_clock.as_ref().map(|c| c.monotonic());
        if let Err(e) = seg.file.sync_data() {
            self.fail(FaultOp::Sync, &path, &e);
            return Some(Self::unknown(calls));
        }
        if let (Some(c), Some(b)) = (&self.sync_clock, before) {
            self.pace
                .sample(pacing::nanos(c.monotonic().saturating_sub(b)));
            self.stats.sync_interval = self.sync_interval();
            self.stats.sync_time = self.pace.estimate();
        }
        let last = content.last_sequence();
        // The batch is on stable storage: account for it in the segment's summary and index.
        let intervals = &mut self.intervals;
        let max_series_id = &mut self.max_series_id;
        let seg = next
            .as_mut()
            .or(self.open.as_mut())
            .expect("a segment in use");
        // Q-630: the first batch in a segment is acknowledged only once a synced mark names it.
        let first_in_segment = seg.summary.is_none();
        seg.write_offset += bytes.len() as u64;
        seg.summary = Some(match seg.summary {
            None => SealSummary {
                entry_count: last + 1 - first,
                first_sequence: first,
                last_sequence: last,
                first_commit_time: commit_time,
                last_commit_time: commit_time,
                batch_count: 1,
            },
            Some(s) => SealSummary {
                entry_count: s.entry_count + (last + 1 - first),
                last_sequence: last,
                last_commit_time: commit_time,
                batch_count: s.batch_count + 1,
                ..s
            },
        });
        seg.has_entries |= builder.block_types().any(|t| t == block_type::ENTRIES);
        let (file_id, offset) = (seg.id.file_id, seg.write_offset);
        for item in &content.items {
            match item.view() {
                ItemRef::Fact(r) => {
                    let reach = reach(r.t, intervals.get(&r.series_id));
                    seg.index.note(r.series_id, r.t, reach);
                }
                ItemRef::Entry(Entry::Definition(d)) => {
                    *max_series_id = (*max_series_id).max(d.id);
                    if let Some(i) = &d.interval {
                        intervals.insert(d.id, i.clone());
                    }
                }
                ItemRef::Entry(_) => {}
            }
        }
        if let Some(n) = next {
            self.roll_to(n);
        }
        self.last_sequence = last;
        self.last_commit_time = commit_time;
        self.stats.bytes += bytes.len() as u64;
        self.stats.batches += 1;
        self.stats.max_series_id = self.max_series_id;
        // The mark moves: one positional write of the slot that does not hold the last synced mark; until the next
        // mark sync, later updates replace this one in the same slot (A.10 Update).
        let slot = MarkSlot {
            counter: self.mark_slot.counter + 1,
            file_id,
            offset,
            last_sequence: last,
            max_series_id: self.max_series_id,
            boot_id: self.boot_id,
            ..self.mark_slot
        };
        let mp = self.mark_path.clone();
        if self.halted() {
            return Some(Self::unknown(calls));
        }
        let written = match self.mark.as_ref() {
            Some(m) => m.write_at(&slot.encode(), SLOT_OFFSETS[self.mark_next_slot] as u64),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        };
        if let Err(e) = written {
            self.fail(FaultOp::MarkWrite, &mp, &e);
            return Some(Self::unknown(calls));
        }
        self.mark_slot = slot;
        self.mark_written = true;
        self.mark_dirty = true;
        let forced = matches!(self.mark_mode, MarkSyncMode::EveryCommit { .. })
            || first_in_segment
            || calls.iter().any(Call::has_non_fact);
        if forced {
            self.sync_mark(now);
            if self.fault.is_some() {
                return Some(Self::unknown(calls));
            }
        }
        self.stats.durable = Position {
            generation: self.generation,
            sequence: last,
        };
        let mut seq = first;
        Some(
            calls
                .iter()
                .map(|c| {
                    let ack = Ack {
                        first: Position {
                            generation: self.generation,
                            sequence: seq,
                        },
                        last: Position {
                            generation: self.generation,
                            sequence: seq + c.sequences - 1,
                        },
                    };
                    seq += c.sequences;
                    (c.id, Ok(ack))
                })
                .collect(),
        )
    }

    /// Frees the last batch's entries and calls: the writer thread does so once the calls are acknowledged, off
    /// the path to their acknowledgement; [`LogWriter::step`] does so at its start.
    pub(crate) fn release_batch(&mut self) {
        self.batch = Vec::new();
        self.batch_calls = Vec::new();
    }

    /// Entries and calls of the last batch still held.
    #[cfg(test)]
    pub(crate) fn batch_held(&self) -> usize {
        self.batch.len() + self.batch_calls.len()
    }

    fn unknown(calls: &[Call]) -> Vec<Done> {
        Self::finished(calls, CommitError::OutcomeUnknown)
    }

    fn finished(calls: &[Call], e: CommitError) -> Vec<Done> {
        calls.iter().map(|c| (c.id, Err(e.clone()))).collect()
    }

    /// Clean shutdown: syncs the mark if it moved since its last sync. Nothing is written after a fault. Seal jobs
    /// are the caller's to finish first.
    pub fn shutdown(&mut self, now: Duration) -> Result<(), Fault> {
        self.sync_halt();
        if let Some(f) = &self.fault {
            return Err(f.clone());
        }
        if self.mark_dirty {
            self.sync_mark(now);
        }
        self.fault.clone().map_or(Ok(()), Err)
    }

    /// The mark as last written (on stable storage once synced).
    pub fn mark(&self) -> MarkSlot {
        self.mark_slot
    }
}
