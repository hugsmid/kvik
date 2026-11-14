//! The engine model: ingest, the log and commit (§3), delivery (§4), live
//! summaries (§5), compaction and files (§7), tiers, backup and restore (§8).
//!
//! The model is a deterministic state machine. What is on disk (or in another
//! tier) lives in [`Disk`] and survives a crash; everything else lives in a
//! volatile part that a crash discards and [`Model::restart`] rebuilds from the
//! disk alone. Crashes and faults are injected at named points.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::condition::{Condition, ConditionState, ResumeHistory, Runner};
use crate::registry::{
    Registry, RegistryError, RegistryEvent, RetentionClass, SeriesDef, SeriesOptions, Timing,
};
use crate::summary::{Aggregate, Node, Params};
use crate::tree::{LEVELS, SeriesTree, TreeError};
use crate::types::{
    Fact, Flags, HOUR, Kind, MAX_TEXT_BYTES, MINUTE, Nanos, Position, QUARTER, Quality, Reading,
    SECOND, SeriesId, TSource, Value, ValueType, floor_to,
};
use crate::view;

/// Engine configuration. Defaults follow the spec where it gives one.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bounded ingest queue (§3 back-pressure).
    pub ingest_capacity: usize,
    /// A reading more than this far ahead of `received_at` is `future` (§9 row 4).
    pub future_tolerance: Nanos,
    /// Lateness bound (§2.2): a reading older than this relative to
    /// `received_at` is `late`, as is one earlier than the series' latest
    /// recorded `t` by more than 2 × S.
    pub lateness_bound: Nanos,
    /// `received_at − t` above this counts towards clock-skew health (§9 row 5).
    pub skew_threshold: Nanos,
    /// Consecutive skewed readings before health reports clock skew.
    pub skew_count: u32,
    /// Ceiling on the number of series (§9 row 30).
    pub series_ceiling: usize,
    /// Seal the active segment after this many batches (stands in for 64 MiB).
    pub seal_after_batches: usize,
    /// Seal the active segment after this much wall time (§3: 5 minutes).
    pub seal_interval: Nanos,
    /// Grace period before a retired file may be removed locally, on the
    /// writer's monotonic clock (§7: 1 hour).
    pub grace: Nanos,
    /// A lease whose counter has not changed for this long on the writer's
    /// monotonic clock expires (§7: 1 hour).
    pub lease_expiry: Nanos,
    /// Compacted chunks of a series overlapping one 15-minute span are
    /// merged when they number more than this (§7: 4).
    pub merge_threshold: usize,
    /// Age after which revision chunks and sealed segments leave the fast disk
    /// (§8: 90 days).
    pub eviction_age: Nanos,
    /// Age per series after which its revision chunks leave the fast disk,
    /// `None` for never (§8 Eviction at 90 days); `eviction_age` for a series
    /// not listed.
    pub revision_age: BTreeMap<SeriesId, Option<Nanos>>,
    /// Fast-disk capacity in bytes.
    pub disk_bytes: u64,
    /// Bytes one log entry costs on disk (model accounting).
    pub entry_bytes: u64,
    /// Low mark: compaction pauses, health degraded; tiering and eviction
    /// continue (§3).
    pub low_mark: u64,
    /// Critical mark: ingest waits (§3).
    pub critical_mark: u64,
    /// Fill ratio (percent) above which verified sealed log segments, then
    /// the oldest current detail, leave the fast disk (§8: 70 %).
    pub fill_threshold_pct: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ingest_capacity: 1024,
            future_tolerance: 60 * SECOND,
            lateness_bound: 5 * MINUTE,
            skew_threshold: 5 * MINUTE,
            skew_count: 3,
            series_ceiling: 100_000,
            seal_after_batches: 8,
            seal_interval: 5 * MINUTE,
            grace: HOUR,
            lease_expiry: HOUR,
            merge_threshold: 4,
            eviction_age: 90 * 24 * HOUR,
            revision_age: BTreeMap::new(),
            disk_bytes: 1 << 40,
            entry_bytes: 64,
            low_mark: 2 << 30,
            critical_mark: 512 << 20,
            fill_threshold_pct: 70,
        }
    }
}

/// Consumer identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConsumerId(pub u32);

/// Which series a funnel selects.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    /// Every series.
    All,
    /// The listed series.
    Series(BTreeSet<SeriesId>),
}

impl Selection {
    fn matches(&self, s: SeriesId) -> bool {
        match self {
            Selection::All => true,
            Selection::Series(set) => set.contains(&s),
        }
    }
}

/// A consumer declared in configuration, with an every-reading funnel.
#[derive(Clone, Debug)]
pub struct ConsumerDecl {
    /// Identifier.
    pub id: ConsumerId,
    /// Series selection.
    pub selection: Selection,
    /// Bounded queue length.
    pub capacity: usize,
}

/// Funnel identifier, unique within its consumer (§4 Funnel). Funnel 0 is
/// the consumer's configured selection ([`ConsumerDecl::selection`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunnelId(pub u32);

impl FunnelId {
    /// The funnel of a consumer's configured selection.
    pub const CONFIGURED: FunnelId = FunnelId(0);
}

/// A rolling-window operator `[now − width, now)` (§6).
#[derive(Clone, Debug, PartialEq)]
pub struct Operator {
    /// Aggregate over the window.
    pub aggregate: Aggregate,
    /// Window width.
    pub width: Nanos,
}

/// A funnel (§4): a series selection, an optional operator and an optional
/// condition. A funnel without a condition delivers every fact it selects; a
/// condition funnel delivers transitions of its condition, evaluated on the
/// operator's value (or, without an operator, on the live-state value).
#[derive(Clone, Debug, PartialEq)]
pub struct Funnel {
    /// Series selection; a condition funnel selects exactly one series (§6
    /// Multi-series).
    pub selection: Selection,
    /// Operator.
    pub operator: Option<Operator>,
    /// Condition.
    pub condition: Option<Condition>,
}

impl Funnel {
    /// A funnel that delivers every fact of the selection.
    pub fn facts(selection: Selection) -> Self {
        Funnel {
            selection,
            operator: None,
            condition: None,
        }
    }

    /// A condition funnel over one series.
    pub fn condition(series: SeriesId, operator: Option<Operator>, condition: Condition) -> Self {
        Funnel {
            selection: Selection::Series([series].into_iter().collect()),
            operator,
            condition: Some(condition),
        }
    }

    /// The one series a condition funnel watches.
    fn series(&self) -> Option<SeriesId> {
        match &self.selection {
            Selection::Series(set) if set.len() == 1 => set.iter().next().copied(),
            _ => None,
        }
    }
}

/// A routing change (§4 Routing): a log entry that takes effect at its
/// position.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteChange {
    /// A funnel is added to a consumer.
    Add {
        /// Consumer.
        consumer: ConsumerId,
        /// Funnel identifier.
        funnel: FunnelId,
        /// The funnel.
        spec: Funnel,
    },
    /// A funnel is removed from a consumer.
    Remove {
        /// Consumer.
        consumer: ConsumerId,
        /// Funnel identifier.
        funnel: FunnelId,
    },
}

/// Body of a log entry.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryBody {
    /// A reading fact.
    Fact(Fact),
    /// A registry event.
    Registry(RegistryEvent),
    /// A routing change.
    Route(RouteChange),
}

/// A log entry: every entry, fact or registry event, has a position.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Position.
    pub position: Position,
    /// Host clock at commit.
    pub received_at: Nanos,
    /// Content.
    pub body: EntryBody,
}

impl Entry {
    /// The fact, if this entry is one.
    pub fn fact(&self) -> Option<&Fact> {
        match &self.body {
            EntryBody::Fact(f) => Some(f),
            EntryBody::Registry(_) | EntryBody::Route(_) => None,
        }
    }
}

/// File identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u64);

/// What a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    /// Log segment of arrival batches.
    Segment,
    /// Compacted chunk of facts that were current at compaction (and registry entries).
    Current,
    /// Compacted chunk of revisions.
    Revision,
}

/// Format version this model writes and reads (§9 row 32).
pub const FORMAT_VERSION: u32 = 1;

/// A file. Its content is immutable once sealed.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredFile {
    /// Identifier.
    pub id: FileId,
    /// Class.
    pub class: FileClass,
    /// Format version in the header.
    pub version: u32,
    /// Batches; a compacted chunk is one batch.
    pub batches: Vec<Vec<Entry>>,
    /// Whether sealed (immutable, eligible for backup).
    pub sealed: bool,
    /// Wall time the file was opened.
    pub opened_at: Nanos,
    /// When the file was sealed, on the engine clock `now`; for a compacted
    /// file, when its oldest input was sealed.
    pub sealed_at: Option<Nanos>,
}

impl StoredFile {
    /// All entries.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.batches.iter().flatten()
    }

    /// Content hash (stands in for SHA-256 of the stored bytes).
    pub fn hash(&self) -> u64 {
        fnv(format!("{:?}{:?}{}", self.class, self.batches, self.version).as_bytes())
    }

    fn bytes(&self, per_entry: u64) -> u64 {
        self.entries().count() as u64 * per_entry
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A manifest record (§7).
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestRecord {
    /// Epoch, increasing by one per record.
    pub epoch: u64,
    /// Wall time.
    pub at: Nanos,
    /// Operation.
    pub op: ManifestOp,
}

/// A tier (§8 Tiers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Fast local disk.
    Hot,
    /// Another local path.
    Cold,
    /// The bucket.
    Deep,
}

/// A file a manifest record adds.
#[derive(Clone, Debug, PartialEq)]
pub struct Added {
    /// File.
    pub file: FileId,
    /// Hash of the file as stored.
    pub hash: u64,
    /// Log segments compacted into this file and the others the record adds;
    /// they are not retired (§7: log segments are never retired by
    /// compaction).
    pub from: Vec<FileId>,
}

/// A file a manifest record moves between tiers (§7: a tier move keeps the
/// file's identity and changes its location).
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    /// File.
    pub file: FileId,
    /// The tier it leaves.
    pub from: Tier,
    /// Where its verified copy is.
    pub to: Tier,
}

/// Manifest operation.
#[derive(Clone, Debug, PartialEq)]
pub enum ManifestOp {
    /// Adds files, retires files and moves files between tiers, atomically
    /// (§7 Manifest).
    Publish {
        /// Files added.
        added: Vec<Added>,
        /// Files retired: replaced by the files added.
        retired: Vec<FileId>,
        /// Files moved between tiers.
        moved: Vec<Moved>,
    },
    /// A segment is sealed; records the hash of the file as stored.
    Seal {
        /// File.
        file: FileId,
        /// Hash of the sealed file.
        hash: u64,
    },
    /// The restore record (§8 Restore step 1), written before ingest is
    /// accepted.
    Restore {
        /// The generation the restore claimed.
        generation: u32,
        /// Position of the tree checkpoint it restores from, if any.
        checkpoint: Option<Position>,
        /// The restored end.
        restored_end: Position,
    },
}

/// A tree checkpoint (§5.1, §8 Continuous backup): the stored nodes of every
/// series, the registry file and each series' latest recorded `t`, all as of
/// its position. It carries no facts.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// Position up to which it is complete.
    pub position: Position,
    /// The registry as of the position.
    pub registry: Registry,
    /// Each series' latest recorded `t` as of the position.
    pub latest_t: BTreeMap<SeriesId, Nanos>,
    /// The version index as of the position (§2.4, §8 Continuous backup):
    /// the highest recorded `changed_at` of every key of a versioned series.
    pub versions: BTreeMap<(SeriesId, Nanos), u64>,
    /// The revised-key index as of the position (§2.4, §8 Continuous
    /// backup): for every key with more than one recorded fact, each fact's
    /// value, adapter-supplied quality and `changed_at`. The model keeps each
    /// fact's position with them.
    pub revised: BTreeMap<(SeriesId, Nanos), Vec<IndexEntry>>,
    /// Stored nodes per series, without readings.
    pub trees: BTreeMap<SeriesId, SeriesTree>,
}

/// One fact of a key in the revised-key index (§2.4).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexEntry {
    /// Position of the fact.
    pub position: Position,
    /// Value.
    pub value: Value,
    /// Adapter-supplied quality.
    pub quality: Quality,
    /// `changed_at`, in a versioned series.
    pub changed_at: Option<u64>,
}

/// A restore in progress or finished, kept on the local disk with its
/// record (§8 Restore).
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreState {
    /// The generation it claimed.
    pub generation: u32,
    /// The restored end.
    pub restored_end: Position,
    /// The tree checkpoint it restores from, downloaded in step 1.
    pub checkpoint: Option<Checkpoint>,
    /// The bucket objects it restores from. The model has no backed-up
    /// manifest records; this list stands in for the files they list.
    pub sources: BTreeSet<FileId>,
    /// Latest `t` per series from the checkpoint and the restored log
    /// (§8 Restore step 2).
    pub latest_t: BTreeMap<SeriesId, Nanos>,
    /// Spans `(series, 15-minute start)` whose nodes have been recomputed
    /// from facts and compared with the checkpoint (§8 Restore step 4).
    pub verified: BTreeSet<(SeriesId, Nanos)>,
    /// Spans where the checkpoint differed from the recomputed nodes.
    pub differences: Vec<(SeriesId, Nanos)>,
    /// Cursors the consumers kept from before the restore.
    pub kept_cursors: BTreeMap<ConsumerId, Position>,
    /// Whether step 1 is complete: the checkpoint and the log after it are
    /// local, the latest `t` per series is known and cursors are settled.
    /// Ingest is accepted only then.
    pub step_one_done: bool,
}

/// A restore that claimed a generation but has not yet written its record.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingRestore {
    /// Cursors the consumers kept from before the restore.
    pub cursors: BTreeMap<ConsumerId, Position>,
    /// Whether it is a drill.
    pub drill: bool,
}

/// A query result with its flags (§11 Query).
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The result.
    pub node: Node,
    /// Served while a restore is still bringing detail back (§8 Restore
    /// step 2).
    pub restoring: bool,
    /// A minute in the span is flagged `summary_stale` (§5.6).
    pub summary_stale: bool,
}

/// The backup bucket (deep tier).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bucket {
    /// Whether the bucket can be reached.
    pub reachable: bool,
    /// Stored objects with the hash the bucket reports.
    pub objects: BTreeMap<FileId, (u64, StoredFile)>,
    /// Fault injection: the next upload arrives corrupted.
    pub corrupt_next_upload: bool,
    /// Generations claimed by restores, each by a create-if-absent marker
    /// (§8 Generations).
    pub claims: BTreeSet<u32>,
    /// Tree checkpoints by position (§8 Continuous backup).
    pub checkpoints: BTreeMap<Position, Checkpoint>,
    /// Files retired by a manifest record whose added files are all in the
    /// bucket. The model uploads no manifest records (§8 Continuous backup);
    /// this set stands in for what they say, so that a restore does not
    /// restore a file and the file that replaced it.
    pub retired: BTreeSet<FileId>,
}

/// Outcome of a batch that was written but not synced when the machine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Nothing reached the disk.
    Lost,
    /// Part of the batch reached the disk; its last block fails its checksum.
    Torn,
    /// The whole batch reached the disk despite the missing sync.
    Survived,
}

/// Where a commit crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the batch is formed, before anything is written.
    BeforeWrite,
    /// After the write, before the sync completes (§9 row 7).
    AfterWrite(WriteOutcome),
    /// After the sync, before the durable mark is updated: the batch is on
    /// disk but was never acknowledged or delivered.
    AfterSyncBeforeMark,
    /// After the sync and the mark update, before live state and delivery
    /// (§9 row 8).
    AfterSync,
    /// After delivering to the first `n` consumers.
    DuringDelivery(usize),
}

/// A fault injected into a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The process stops; the machine keeps running (same boot). Recovery
    /// truncates to the durable mark (§3), so an unsynced batch is discarded
    /// whatever reached the page cache (§9 row 7).
    Crash(CrashPoint),
    /// The machine loses power after the write and before the sync: the
    /// process stops and the boot id changes ([`Model::power_loss`]).
    /// Recovery keeps complete batches with valid checksums (§9 row 7a).
    PowerLoss(WriteOutcome),
    /// The sync call returns an error (§9 row 12). The engine becomes
    /// read-only; a restart on the same boot truncates to the durable mark.
    SyncError(WriteOutcome),
}

/// Where a compaction crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionCrash {
    /// New files written, not yet published (§9 row 9).
    AfterWrite,
    /// New files published, inputs not yet retired (§9 row 10).
    AfterPublish,
}

/// Back-pressure (§3): the reading is not refused; the emit waits until it can
/// enter the queue. The model is synchronous, so it returns the wait to the
/// caller, which emits the same reading again later. Every wait is counted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, thiserror::Error)]
pub enum Wait {
    /// Free space is below the critical mark.
    #[error("disk below critical mark")]
    DiskFull,
    /// The ingest queue is full.
    #[error("ingest queue full")]
    QueueFull,
    /// A host-stamped reading while the system has not reported clock
    /// synchronisation (§2.5): it waits rather than being stamped with a
    /// wrong time.
    #[error("host clock not synchronised")]
    UnsyncedClock,
}

/// Why an emit did not enter the ingest queue.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EmitError {
    /// Back-pressure: emit the same reading again later.
    #[error("wait: {0}")]
    Wait(#[from] Wait),
    /// Refused for good.
    #[error("refused: {0}")]
    Refused(#[from] Refusal),
}

/// Why a reading was refused at ingest. Every refusal is counted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, thiserror::Error)]
pub enum Refusal {
    /// Engine stopped (crashed; restart needed).
    #[error("engine is not running")]
    Down,
    /// Engine is read-only after a failed write or sync.
    #[error("engine is read-only")]
    ReadOnly,
    /// Unknown series.
    #[error("unknown series")]
    UnknownSeries,
    /// Series is retired.
    #[error("series is retired")]
    Retired,
    /// Value does not fit the series' type.
    #[error("value type mismatch")]
    WrongType,
    /// Text longer than 255 bytes.
    #[error("text too long")]
    TextTooLong,
    /// A reading of a versioned series without `changed_at` (§2.2).
    #[error("changed_at missing on a versioned series")]
    ChangedAtMissing,
    /// A reading with `changed_at` on a series that is not versioned (§2.2).
    #[error("changed_at on a series that is not versioned")]
    ChangedAtUnexpected,
    /// `revise` intent on a versioned series (§2.4): a return to an earlier
    /// value there is a fact with a higher `changed_at`.
    #[error("revise on a versioned series")]
    ReviseOnVersioned,
    /// A reading of an interval series whose `t` is off its phase (§2.1).
    #[error("t off the interval phase")]
    OffPhase,
}

/// Engine errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// The engine crashed; call [`Model::restart`].
    #[error("engine crashed")]
    Crashed,
    /// The engine is read-only.
    #[error("engine is read-only")]
    ReadOnly,
    /// The sync failed; the engine is now read-only.
    #[error("sync failed")]
    SyncFailed,
    /// Registry error.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// Tree error.
    #[error(transparent)]
    Tree(#[from] TreeError),
    /// A consumer confirmed a position it has not received.
    #[error("confirmation beyond received position")]
    BadConfirm,
    /// A file's format version is newer than this reader (§9 row 32).
    #[error("file {0:?} has format version {1}, newer than {FORMAT_VERSION}")]
    NewerFormat(FileId, u32),
    /// Compaction and tiering paused for low disk.
    #[error("paused: disk below low mark")]
    Paused,
    /// A restore from a bucket with no backed-up files is refused (§8
    /// Generations).
    #[error("nothing to restore")]
    EmptyBucket,
    /// Refused while a restore is bringing detail back: a query below
    /// 1-minute resolution, or a tree checkpoint (§8 Restore step 2).
    #[error("restoring")]
    Restoring,
    /// A routing change names a consumer that is not declared.
    #[error("unknown consumer {0:?}")]
    UnknownConsumer(ConsumerId),
    /// A routing change is refused.
    #[error("route refused: {0}")]
    BadRoute(&'static str),
    /// A downloaded file holds a position whose content differs from the
    /// copy already local (§8 Restore step 3).
    #[error("file {file:?} holds {position} with different content")]
    Conflict {
        /// The downloaded file.
        file: FileId,
        /// The position.
        position: Position,
    },
}

/// Health level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// All well.
    Ok,
    /// Degraded.
    Degraded,
    /// Fatal.
    Fatal,
}

/// A reason in a health report.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// A write or sync failed.
    SyncFailed,
    /// A higher generation is claimed: this writer stopped uploading and is
    /// read-only (§8 Generations).
    Superseded,
    /// Starting or resuming a restore failed; the engine is down.
    RestoreFailed,
    /// A file is quarantined.
    Quarantined(FileId),
    /// Below the low mark.
    LowDisk,
    /// Below the critical mark.
    CriticalDisk,
    /// Sealed files wait for upload while the bucket is unreachable.
    UploadStalled,
    /// A series' source clock appears skewed.
    ClockSkew(SeriesId),
}

/// Health report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// Worst level.
    pub level: Level,
    /// Reasons.
    pub reasons: Vec<Reason>,
}

/// Counters ("no silent loss", §1.5). Kept across restarts for observation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counters {
    /// Identical repeats dropped (§2.4).
    pub duplicates_dropped: u64,
    /// Refusals by reason.
    pub refused: BTreeMap<Refusal, u64>,
    /// Back-pressure waits by reason (§3: the wait is measured).
    pub waits: BTreeMap<Wait, u64>,
    /// Facts flagged late.
    pub late: u64,
    /// Facts flagged future.
    pub future: u64,
    /// Batches discarded by recovery (torn or lost).
    pub batches_discarded: u64,
    /// Consumer switches to catch-up mode.
    pub overflows: u64,
    /// Unpublished compaction outputs discarded at restart.
    pub compaction_discarded: u64,
    /// Readings recorded although the duplicate rule could not be checked
    /// because the current fact was not on the machine (§2.4).
    pub duplicates_unchecked: u64,
    /// NaN or infinite values recorded as bad quality without a value (§2.5).
    pub non_finite: u64,
    /// Negative counter values recorded as bad quality without a value
    /// (§2.5, §5.5).
    pub negative_counter: u64,
    /// Uploads the bucket rejected for a hash mismatch.
    pub uploads_rejected: u64,
    /// Compaction or tiering runs skipped for low disk.
    pub paused: u64,
    /// Restored spans whose recomputed nodes differed from the checkpoint
    /// (§8 Restore step 4).
    pub checkpoint_differences: u64,
    /// Facts of versioned series outranked when they were committed (§4
    /// Outranked facts).
    pub outranked_on_arrival: u64,
}

/// A delivery to a consumer.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    /// Position.
    pub position: Position,
    /// The fact.
    pub fact: Fact,
    /// Produced during catch-up (§4).
    pub replayed: bool,
    /// Outranked when it was committed (§4 Outranked facts).
    pub outranked: bool,
}

/// What a consumer saw, recorded by the model as an outside observer.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    /// Consumer session (increments when the engine restarts or the consumer reconnects).
    pub session: u32,
    /// Position.
    pub position: Position,
    /// Replayed flag.
    pub replayed: bool,
}

/// Identity of a delivery (§4 At-least-once): consumers de-duplicate fact
/// deliveries by position and condition deliveries by `(funnel, sequence)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeliveryId {
    /// A fact delivery.
    Fact(Position),
    /// A condition delivery.
    Timer {
        /// Generation in which it was issued: a restore starts the sequences
        /// again (see `OPEN-QUESTIONS.md` Q-61).
        generation: u32,
        /// Funnel.
        funnel: FunnelId,
        /// Per-funnel sequence.
        sequence: u64,
    },
    /// `generation_changed` (at most one per consumer session).
    GenerationChanged,
    /// `resync` (at most one per consumer session).
    Resync,
}

/// What produced a condition delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerCause {
    /// An evaluation on a routed fact at commit (§6 Cadence).
    Fact,
    /// An evaluation on the funnel's timer (§4 Timers, §6 Cadence).
    Timer,
    /// Re-attaching to live after catch-up, when the present state differs
    /// from the last state emitted (see `OPEN-QUESTIONS.md`).
    Reattach,
}

/// A transition of a condition funnel.
#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    /// Funnel.
    pub funnel: FunnelId,
    /// The funnel's incarnation: the position of the routing entry that
    /// added it ([`Position::ORIGIN`] for the configured funnel).
    pub incarnation: Position,
    /// Generation in which it was issued.
    pub generation: u32,
    /// Per-funnel sequence, never repeated for the funnel (§4 Order).
    pub sequence: u64,
    /// The state the transition leads to.
    pub state: ConditionState,
    /// What produced it.
    pub cause: TriggerCause,
}

/// Condition state in a `resync`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionNow {
    /// The condition state evaluated now.
    pub state: ConditionState,
    /// Whether it differs from the latch, or there is no latch to compare
    /// (§4 Resync, §6 Restart and resume): a transition, to be acted on.
    pub transition: bool,
}

/// One funnel's part of a `resync`.
#[derive(Clone, Debug, PartialEq)]
pub struct FunnelState {
    /// Funnel.
    pub funnel: FunnelId,
    /// The funnel's incarnation (see [`Trigger::incarnation`]).
    pub incarnation: Position,
    /// Live state (§2.3) of each selected series.
    pub live: Vec<(SeriesId, Option<Fact>)>,
    /// Condition state, for a condition funnel.
    pub condition: Option<ConditionNow>,
}

/// A `resync` delivery (§4 Resync).
#[derive(Clone, Debug, PartialEq)]
pub struct Resync {
    /// Every funnel in force, in funnel order.
    pub funnels: Vec<FunnelState>,
    /// The positions `(after, up_to]` it was sent instead of replaying,
    /// when the cursor was too far behind for catch-up from local log
    /// segments (§4 Resync, §8 Behind consumers).
    pub replaced: Option<(Position, Position)>,
    /// How many `resync`s were issued to this consumer before this one (a
    /// stamp of the model, so tests can tell which state of the store it was
    /// issued in).
    pub issued: usize,
}

/// What a delivery carries.
#[derive(Clone, Debug, PartialEq)]
pub enum DeliveryKind {
    /// A fact.
    Fact(Fact),
    /// A condition transition.
    Trigger(Trigger),
    /// The store was restored and started a new generation (§8 Restore
    /// step 2).
    GenerationChanged {
        /// The cursor the consumer kept from before the restore.
        cursor: Option<Position>,
        /// The last restored position.
        restored_end: Position,
    },
    /// Live and condition state now.
    Resync(Resync),
}

/// A delivery of any kind, in the order the consumer receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    /// For a fact, its position; for a condition delivery, the last
    /// committed position at evaluation (§4 Order); for `resync`, the
    /// position up to which the stream before it is complete; for
    /// `generation_changed`, the restored end.
    pub position: Position,
    /// Produced during catch-up (§4 Replay); facts only.
    pub replayed: bool,
    /// A fact outranked when it was committed (§4 Outranked facts): it must
    /// not be acted on.
    pub outranked: bool,
    /// Content.
    pub kind: DeliveryKind,
}

impl Envelope {
    /// The delivery's identity.
    pub fn id(&self) -> DeliveryId {
        match &self.kind {
            DeliveryKind::Fact(_) => DeliveryId::Fact(self.position),
            DeliveryKind::Trigger(t) => DeliveryId::Timer {
                generation: t.generation,
                funnel: t.funnel,
                sequence: t.sequence,
            },
            DeliveryKind::GenerationChanged { .. } => DeliveryId::GenerationChanged,
            DeliveryKind::Resync(_) => DeliveryId::Resync,
        }
    }

    /// The fact, for a fact delivery.
    pub fn fact(&self) -> Option<&Fact> {
        match &self.kind {
            DeliveryKind::Fact(f) => Some(f),
            _ => None,
        }
    }

    /// The `resync`, for a resync delivery.
    pub fn resync(&self) -> Option<&Resync> {
        match &self.kind {
            DeliveryKind::Resync(r) => Some(r),
            _ => None,
        }
    }

    /// The trigger, for a condition delivery.
    pub fn trigger(&self) -> Option<&Trigger> {
        match &self.kind {
            DeliveryKind::Trigger(t) => Some(t),
            _ => None,
        }
    }
}

/// A delivery as received, recorded by the model as an outside observer.
#[derive(Clone, Debug, PartialEq)]
pub struct Received {
    /// Consumer session.
    pub session: u32,
    /// The delivery.
    pub delivery: Envelope,
}

/// A condition funnel's latch (§6 Latch): the last state its consumer
/// confirmed, and the position at which it was emitted.
#[derive(Clone, Debug, PartialEq)]
pub struct Latch {
    /// The incarnation of the funnel it belongs to (see
    /// [`Trigger::incarnation`]); a latch of an earlier incarnation of the
    /// same funnel id is never used.
    pub incarnation: Position,
    /// State.
    pub state: ConditionState,
    /// Position of the delivery that carried it.
    pub position: Position,
}

/// Result of a commit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitReport {
    /// Positions recorded.
    pub positions: Vec<Position>,
    /// Readings dropped as duplicates.
    pub duplicates: usize,
}

/// Result of [`Model::read`]: where each file was found, and the facts.
pub type ReadResult = (Vec<(FileId, Locality)>, Vec<Fact>);

/// A reader holding a manifest epoch (§7).
#[derive(Clone, Debug, PartialEq)]
pub struct Reader {
    /// Epoch.
    pub epoch: u64,
    /// Files listed at that epoch.
    pub files: Vec<FileId>,
    /// Its lease file, if it is a reader process.
    pub lease: Option<u64>,
}

/// A reader's lease file (§7 Leases).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The epoch the reader holds.
    pub epoch: u64,
    /// Boot id of the machine when the reader wrote it, from the same source
    /// as the mark's.
    pub boot: u64,
    /// Incremented by the reader at every refresh.
    pub counter: u64,
}

/// Where a reader found a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locality {
    /// On the fast disk.
    Hot,
    /// Only in the cold or deep tier.
    Remote,
    /// Nowhere readable.
    Missing,
}

/// State that survives a crash.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    /// Content of every file ever written.
    pub files: BTreeMap<FileId, StoredFile>,
    /// Files present on the fast disk.
    pub hot: BTreeSet<FileId>,
    /// Files present in the cold tier (copies verified on write).
    pub cold: BTreeSet<FileId>,
    /// Hot copies that no longer match their hash (fault injection).
    pub corrupt: BTreeSet<FileId>,
    /// Quarantined hot copies.
    pub quarantined: BTreeSet<FileId>,
    /// Deep tier.
    pub bucket: Bucket,
    /// Manifest.
    pub manifest: Vec<ManifestRecord>,
    /// Active segment.
    pub active: Option<FileId>,
    /// The durable mark (§3): updated after each successful sync. It is
    /// memory-mapped and survives a process crash; after a power loss its boot
    /// id no longer matches the machine.
    pub mark: Option<Mark>,
    /// What a power loss leaves of the active segment's batches past the mark
    /// (written, not covered by the mark). `None` when there are none.
    pub tail_fate: Option<WriteOutcome>,
    /// Durable consumer cursors (last confirmed position).
    pub cursors: BTreeMap<ConsumerId, Position>,
    /// Durable latches, one per condition funnel, written with the cursor
    /// when the consumer confirms the delivery that carried the state (§4
    /// Consumer state, §6 Latch).
    pub latches: BTreeMap<(ConsumerId, FunnelId), Latch>,
    /// The last sequence issued per condition funnel, written when a
    /// condition delivery is issued, so a sequence is never issued twice for
    /// a funnel, across restarts too (see `OPEN-QUESTIONS.md`).
    pub sequences: BTreeMap<(ConsumerId, FunnelId), u64>,
    /// `generation_changed` owed to each consumer after a restore (§8
    /// Restore step 2): the cursor it kept and the restored end. Delivered
    /// before its `resync`, after any catch-up to the restored end.
    pub generation_changed: BTreeMap<ConsumerId, (Option<Position>, Position)>,
    /// Current generation (§2.2): 0 until a restore starts a new one (§8).
    pub generation: u32,
    /// The restored end (§8 Restore step 1): the last position before the
    /// first gap in the restored log, if this store was restored. Positions of
    /// older generations beyond it are set aside: kept, but outside views,
    /// trees and delivery.
    pub restored_end: Option<Position>,
    /// A restore drill: read-only, and it claims no generation (§8).
    pub drill: bool,
    /// This writer found a higher generation claimed: it stopped uploading
    /// and stays read-only across restarts (§8 Generations).
    pub superseded: bool,
    /// The restore this store came from, once its record is written.
    pub restore: Option<RestoreState>,
    /// A restore that claimed a generation and stopped before its record.
    pub pending_restore: Option<PendingRestore>,
    /// Tree checkpoints taken locally, uploaded once the log up to their
    /// position is backed up.
    pub checkpoints: Vec<Checkpoint>,
    /// Why the last attempt to start or resume a restore failed; the engine
    /// stays down meanwhile.
    pub restore_failed: Option<Error>,
    /// Lease files of reader processes, by id (§7 Leases).
    pub leases: BTreeMap<u64, Lease>,
    /// Next lease id.
    pub next_lease: u64,
    /// Other bytes in use on the fast disk (fault injection for disk fill).
    pub other_bytes: u64,
    /// Next file id.
    pub next_file: u64,
}

/// A consumer event that is not a fact delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// The store was restored and started a new generation; every consumer
    /// receives it, after any catch-up to the restored end (§8 Restore step
    /// 2).
    GenerationChanged {
        /// The cursor the consumer kept from before the restore; `None` for a
        /// consumer without one, which is treated as beyond the restored end.
        cursor: Option<Position>,
        /// The last restored position.
        restored_end: Position,
    },
    /// A `resync` was queued (§4); the delivery itself is a
    /// [`DeliveryKind::Resync`].
    ResyncDue,
}

/// The durable mark (§3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Boot id of the machine when the mark was written.
    pub boot: u64,
    /// Segment the mark points into.
    pub file: FileId,
    /// Batches of that segment covered by the mark.
    pub batches: usize,
    /// Last position covered.
    pub last: Position,
}

impl Disk {
    /// Batches of `f` that readers may see: all of them, except in the active
    /// segment, where only those up to the durable mark count (§3).
    pub fn visible_batches(&self, f: FileId) -> &[Vec<Entry>] {
        let file = &self.files[&f];
        if self.active != Some(f) {
            return &file.batches;
        }
        let n = match self.mark {
            Some(m) if m.file == f => m.batches.min(file.batches.len()),
            _ => 0,
        };
        &file.batches[..n]
    }

    fn visible_entries(&self, f: FileId, file: &StoredFile) -> Vec<Entry> {
        if self.active == Some(f) {
            self.visible_batches(f).iter().flatten().cloned().collect()
        } else {
            file.entries().cloned().collect()
        }
    }

    /// Batches of the active segment past the durable mark.
    pub fn tail_len(&self) -> usize {
        self.active.map_or(0, |f| {
            self.files[&f].batches.len() - self.visible_batches(f).len()
        })
    }

    /// Next manifest epoch.
    fn epoch(&self) -> u64 {
        self.manifest.last().map_or(1, |r| r.epoch + 1)
    }

    /// Appends one record that adds `added` (each with `from`), retires
    /// `retired` and moves `moved`, atomically (§7 Manifest). Returns its
    /// epoch.
    fn publish(
        &mut self,
        at: Nanos,
        added: &[FileId],
        from: Vec<FileId>,
        retired: Vec<FileId>,
        moved: Vec<Moved>,
    ) -> u64 {
        let epoch = self.epoch();
        let added = added
            .iter()
            .map(|f| Added {
                file: *f,
                hash: self.files[f].hash(),
                from: from.clone(),
            })
            .collect();
        self.manifest.push(ManifestRecord {
            epoch,
            at,
            op: ManifestOp::Publish {
                added,
                retired,
                moved,
            },
        });
        epoch
    }

    /// Log segments already compacted (§7).
    pub fn compacted_segments(&self) -> BTreeSet<FileId> {
        self.manifest
            .iter()
            .flat_map(|r| match &r.op {
                ManifestOp::Publish { added, .. } => {
                    added.iter().flat_map(|a| a.from.clone()).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    /// Files ever published.
    pub fn published(&self) -> BTreeSet<FileId> {
        self.manifest
            .iter()
            .flat_map(|r| match &r.op {
                ManifestOp::Publish { added, .. } => added.iter().map(|a| a.file).collect(),
                ManifestOp::Seal { .. } | ManifestOp::Restore { .. } => Vec::new(),
            })
            .collect()
    }

    /// Retired files, each with the record that retired it: its epoch and
    /// the files that replaced it (added by the same record).
    pub fn retired(&self) -> BTreeMap<FileId, (u64, Vec<FileId>)> {
        let mut out = BTreeMap::new();
        for r in &self.manifest {
            if let ManifestOp::Publish { added, retired, .. } = &r.op {
                for f in retired {
                    out.insert(*f, (r.epoch, added.iter().map(|a| a.file).collect()));
                }
            }
        }
        out
    }

    /// Files listed (published and not retired) at `epoch`.
    pub fn listed_at(&self, epoch: u64) -> Vec<FileId> {
        let mut set = BTreeSet::new();
        for r in self.manifest.iter().take_while(|r| r.epoch <= epoch) {
            if let ManifestOp::Publish { added, retired, .. } = &r.op {
                set.extend(added.iter().map(|a| a.file));
                for f in retired {
                    set.remove(f);
                }
            }
        }
        set.into_iter().collect()
    }

    /// The latest epoch, or 0 before the first record.
    pub fn latest_epoch(&self) -> u64 {
        self.manifest.last().map_or(0, |r| r.epoch)
    }

    /// Whether a verified copy exists outside the fast disk.
    pub fn verified_elsewhere(&self, f: FileId) -> bool {
        self.cold.contains(&f) || self.in_bucket(f)
    }

    /// Whether the bucket holds the file with the manifest's hash.
    pub fn in_bucket(&self, f: FileId) -> bool {
        let want = self.manifest_hash(f);
        self.bucket
            .objects
            .get(&f)
            .is_some_and(|(h, _)| Some(*h) == want)
    }

    /// The latest hash the manifest records for a file.
    pub fn manifest_hash(&self, f: FileId) -> Option<u64> {
        self.manifest.iter().rev().find_map(|r| match &r.op {
            ManifestOp::Seal { file, hash } if *file == f => Some(*hash),
            ManifestOp::Publish { added, .. } => added.iter().find(|a| a.file == f).map(|a| a.hash),
            _ => None,
        })
    }

    /// Where a file can be read from.
    pub fn locality(&self, f: FileId) -> Locality {
        if self.hot.contains(&f) && !self.corrupt.contains(&f) && !self.quarantined.contains(&f) {
            Locality::Hot
        } else if self.verified_elsewhere(f) {
            Locality::Remote
        } else {
            Locality::Missing
        }
    }

    /// Readable content of a file from any tier.
    fn readable(&self, f: FileId) -> Option<&StoredFile> {
        match self.locality(f) {
            Locality::Hot => self.files.get(&f),
            Locality::Remote => {
                if self.cold.contains(&f) {
                    self.files.get(&f)
                } else {
                    self.bucket.objects.get(&f).map(|(_, c)| c)
                }
            }
            Locality::Missing => None,
        }
    }

    /// Every readable entry except those set aside beyond a restored end
    /// (§8 Restore step 1).
    pub fn live_entries(&self) -> BTreeMap<Position, Entry> {
        let mut entries = self.all_entries();
        if let Some(end) = self.restored_end {
            entries.retain(|p, _| p.generation >= self.generation || *p <= end);
        }
        entries
    }

    /// Every distinct entry readable from any tier, by position.
    pub fn all_entries(&self) -> BTreeMap<Position, Entry> {
        let mut out = BTreeMap::new();
        for f in self.published() {
            if let Some(file) = self.readable(f) {
                for e in self.visible_entries(f, file) {
                    out.entry(e.position).or_insert(e);
                }
            }
        }
        out
    }

    /// Every distinct fact readable from any tier.
    pub fn all_facts(&self) -> Vec<Fact> {
        self.all_entries()
            .values()
            .filter_map(|e| e.fact().cloned())
            .collect()
    }

    /// Whether a file has a readable copy on a local tier: hot, or cold
    /// (§8 Tiers; deep is the only tier off the machine).
    pub fn is_local(&self, f: FileId) -> bool {
        self.locality(f) == Locality::Hot || self.cold.contains(&f)
    }

    /// Whether some published file has no copy on a local tier.
    pub fn any_detail_off_machine(&self) -> bool {
        self.published().into_iter().any(|f| !self.is_local(f))
    }

    /// Positions of entries held by a file on a local tier (hot or cold).
    pub fn local_positions(&self) -> BTreeSet<Position> {
        let mut out = BTreeSet::new();
        for f in self.published() {
            if self.is_local(f) {
                out.extend(
                    self.visible_entries(f, &self.files[&f])
                        .iter()
                        .map(|e| e.position),
                );
            }
        }
        out
    }

    /// Bytes used on the fast disk.
    pub fn used_bytes(&self, per_entry: u64) -> u64 {
        let files: u64 = self
            .hot
            .iter()
            .map(|f| self.files[f].bytes(per_entry))
            .sum();
        files + self.other_bytes
    }

    /// Sealed, published files whose backup is not yet verified.
    pub fn upload_queue(&self) -> Vec<FileId> {
        self.published()
            .into_iter()
            .filter(|f| self.files[f].sealed && !self.in_bucket(*f))
            .collect()
    }
}

#[derive(Clone, Debug)]
enum Mode {
    Live,
    CatchUp { next: Position },
}

#[derive(Clone, Debug)]
struct ConsumerRt {
    decl: ConsumerDecl,
    queue: VecDeque<Envelope>,
    mode: Mode,
    session: u32,
    /// Highest position among the deliveries taken in this session.
    last_received: Position,
    /// Deliveries taken in this session and not yet confirmed, in order.
    unconfirmed: Vec<Envelope>,
    /// On resume: catch-up replays up to `stop`, then `resync` (§6 Restart
    /// and resume).
    resume: Option<Position>,
    /// Running conditions of the condition funnels in force; rebuilt on
    /// resume, never replayed (§6).
    runners: BTreeMap<FunnelId, Runner>,
    /// The last state emitted per condition funnel in this session.
    emitted: BTreeMap<FunnelId, ConditionState>,
}

/// State a crash discards.
#[derive(Clone, Debug)]
struct Volatile {
    registry: Registry,
    entries: BTreeMap<Position, Entry>,
    current: BTreeMap<(SeriesId, Nanos), Fact>,
    newest_t: BTreeMap<SeriesId, Nanos>,
    /// Every recorded fact's identity per key, for the duplicate rule.
    history: History,
    /// The version index (§2.4), exact also while a restore brings detail
    /// back (from the checkpoint and the replay, §8 Restore step 2).
    versions: Versions,
    /// While a restore brings detail back, the checkpoint's position and
    /// version index: facts after the checkpoint are judged outranked
    /// against it as well as against the facts before them.
    cp_versions: Option<(Position, Versions)>,
    trees: BTreeMap<SeriesId, SeriesTree>,
    /// Queued readings with their `revise` intent.
    queue: VecDeque<(Reading, bool)>,
    next_position: Position,
    /// Routing changes by position (§4 Routing).
    routes: BTreeMap<Position, RouteChange>,
    consumers: Vec<ConsumerRt>,
    /// Read-only for any cause: a failed write or sync, a drill, or a higher
    /// generation claimed.
    read_only: bool,
    /// A write or sync failed (§3), as opposed to read-only by design.
    sync_failed: bool,
    skew_run: BTreeMap<SeriesId, u32>,
    /// While a restore is bringing detail back (§8 Restore steps 2–4).
    restore: Option<RestoreRt>,
    /// The writer's monotonic clock at its start (§7 grace and leases).
    started: Nanos,
    /// The monotonic instant of each retiring record written since the start,
    /// by epoch (§7 Retired files).
    retired_mono: BTreeMap<u64, Nanos>,
    /// Per lease, the counter last observed and when the writer first
    /// observed it (§7 Leases).
    leases_seen: BTreeMap<u64, (u64, Nanos)>,
}

/// Volatile state derived from the disk alone.
struct Derived {
    registry: Registry,
    entries: BTreeMap<Position, Entry>,
    routes: BTreeMap<Position, RouteChange>,
    current: BTreeMap<(SeriesId, Nanos), Fact>,
    newest_t: BTreeMap<SeriesId, Nanos>,
    history: History,
    versions: Versions,
    cp_versions: Option<(Position, Versions)>,
    trees: BTreeMap<SeriesId, SeriesTree>,
    restore: Option<RestoreRt>,
}

/// What the duplicate rule compares of a recorded fact (§2.4): its value,
/// adapter-supplied quality and `changed_at`, with its position to tell
/// whether it is local and how it ranks.
#[derive(Clone, Debug, PartialEq)]
struct Known {
    position: Position,
    value: Value,
    quality: Quality,
    changed_at: Option<u64>,
}

impl Known {
    fn of(f: &Fact) -> Self {
        Known {
            position: f.position,
            value: f.value.clone(),
            quality: f.supplied_quality,
            changed_at: f.changed_at,
        }
    }

    fn identical(&self, value: &Value, quality: Quality) -> bool {
        self.value.identical(value) && self.quality == quality
    }
}

/// Whether a fact was outranked when it was committed (§2.3): a fact of its
/// key with a lower position has a higher `changed_at`. A function of the
/// log, so live delivery and catch-up agree on it.
fn outranked_at_commit(
    history: &History,
    checkpoint: Option<&(Position, Versions)>,
    f: &Fact,
) -> bool {
    let Some(c) = f.changed_at else {
        return false;
    };
    // While restoring, facts at or before the checkpoint's position may not
    // be local; for a fact after it, its version index stands for them.
    let before_checkpoint = checkpoint.is_some_and(|(p, versions)| {
        f.position > *p && versions.get(&(f.series, f.t)).is_some_and(|top| *top > c)
    });
    before_checkpoint
        || history.get(&(f.series, f.t)).is_some_and(|ks| {
            ks.iter()
                .any(|k| k.position < f.position && k.changed_at.is_some_and(|kc| kc > c))
        })
}

/// The version index (§2.4): the highest recorded `changed_at` of every key
/// of a versioned series.
type Versions = BTreeMap<(SeriesId, Nanos), u64>;

/// The recorded facts of each key, as the duplicate rule sees them.
type History = BTreeMap<(SeriesId, Nanos), Vec<Known>>;

/// What ingest and the tree need while a restore is bringing detail back.
#[derive(Clone, Debug)]
struct RestoreRt {
    /// Latest `t` per series from the checkpoint and the restored log.
    latest_t: BTreeMap<SeriesId, Nanos>,
    /// Whether every current chunk, and with them the revised-key index, is
    /// back.
    index_back: bool,
    /// Keys whose only restored fact is not local.
    remote_single: BTreeSet<(SeriesId, Nanos)>,
    /// Minutes `(series, start)` for which the checkpoint holds a node.
    checkpoint_minutes: BTreeSet<(SeriesId, Nanos)>,
    /// Spans `(series, 15-minute start)` whose detail is not all local.
    not_back: BTreeSet<(SeriesId, Nanos)>,
}

/// The engine model.
#[derive(Clone, Debug)]
pub struct Model {
    /// Configuration.
    pub config: Config,
    /// Consumers declared in configuration.
    pub consumers: Vec<ConsumerDecl>,
    /// What survives a crash.
    pub disk: Disk,
    /// Counters.
    pub counters: Counters,
    /// What each consumer has received, as observed from outside.
    pub observed: BTreeMap<ConsumerId, Vec<Observed>>,
    /// `generation_changed` and `resync` issued to each consumer, in the
    /// order they were queued.
    pub signals: BTreeMap<ConsumerId, Vec<Signal>>,
    /// Every delivery each consumer has received, of any kind, in order.
    pub received: BTreeMap<ConsumerId, Vec<Received>>,
    /// Every delivery each consumer has confirmed, in order.
    pub confirmed: BTreeMap<ConsumerId, Vec<Envelope>>,
    /// Host wall clock (may jump).
    pub wall: Nanos,
    /// Monotonic clock (only advances).
    pub mono: Nanos,
    /// Engine UTC clock for window ends: never runs backwards (§6).
    pub now: Nanos,
    /// Boot id of the machine; a power loss starts a new boot.
    pub boot: u64,
    /// Whether the system reports clock synchronisation (§2.5). Until it
    /// does, batches carry `unsynced_clock`, `future` and `late` are not set
    /// and host-stamped readings wait.
    pub clock_synchronised: bool,
    vol: Option<Volatile>,
    sessions: BTreeMap<ConsumerId, u32>,
}

impl Model {
    /// A new, empty store with the given consumers. Wall and monotonic clocks
    /// start at `wall`.
    pub fn new(config: Config, consumers: Vec<ConsumerDecl>, wall: Nanos) -> Self {
        let mut disk = Disk::default();
        disk.bucket.reachable = true;
        for c in &consumers {
            disk.cursors.insert(c.id, Position::ORIGIN);
        }
        let mut m = Model {
            config,
            consumers,
            disk,
            counters: Counters::default(),
            observed: BTreeMap::new(),
            signals: BTreeMap::new(),
            received: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            wall,
            mono: 0,
            now: wall,
            boot: 0,
            clock_synchronised: true,
            vol: None,
            sessions: BTreeMap::new(),
        };
        m.restart();
        m
    }

    /// Whether the engine is running (not crashed).
    pub fn is_up(&self) -> bool {
        self.vol.is_some()
    }

    fn vol(&self) -> Result<&Volatile, Error> {
        self.vol.as_ref().ok_or(Error::Crashed)
    }

    fn vol_mut(&mut self) -> Result<&mut Volatile, Error> {
        self.vol.as_mut().ok_or(Error::Crashed)
    }

    // ----- clocks -----

    /// Advances both clocks.
    pub fn advance(&mut self, d: Nanos) {
        assert!(d >= 0);
        self.wall += d;
        self.mono += d;
        self.now = (self.now + d).max(self.wall);
    }

    /// Moves the wall clock only (NTP correction, §9 row 22).
    pub fn jump_wall(&mut self, d: Nanos) {
        self.wall += d;
        self.now = self.now.max(self.wall);
    }

    // ----- crash and recovery -----

    /// Stops the machine now: volatile state is lost.
    pub fn crash(&mut self) {
        self.vol = None;
    }

    /// The machine loses power: the process stops and the next start is a new
    /// boot, so the page cache and the boot id of the mark are gone (§3).
    pub fn power_loss(&mut self) {
        self.boot += 1;
        self.crash();
    }

    /// Whether the engine is up but read-only: after a failed write or sync
    /// (§3), as a restore drill, or superseded by a higher generation (§8).
    pub fn is_read_only(&self) -> bool {
        self.vol.as_ref().is_some_and(|v| v.read_only)
    }

    /// Starts the engine from the disk alone (§3 "Recovery").
    pub fn restart(&mut self) {
        self.restart_with(false);
    }

    /// Starts the engine from the disk alone; with `recovery_sync_fails`, the
    /// sync that recovery performs after a reboot fails (§3 Recovery: "If that
    /// sync fails, stay read-only").
    pub fn restart_with(&mut self, recovery_sync_fails: bool) {
        self.vol = None;
        // A restore that stopped before its record starts again with the
        // next generation (§8 Restore).
        if self.disk.restore.is_none()
            && let Some(pending) = self.disk.pending_restore.clone()
        {
            self.restart_restore(pending);
            return;
        }
        // A restore that stopped after its record finishes step 1 before it
        // accepts ingest (§8 Restore).
        if self.disk.restore.as_ref().is_some_and(|r| !r.step_one_done) {
            if let Err(e) = self.finish_step_one() {
                self.disk.restore_failed = Some(e);
                return;
            }
            self.disk.restore_failed = None;
        }
        // 1. The active segment past the durable mark (§3 Recovery).
        let sync_failed = self.recover_tail(recovery_sync_fails);
        // Read-only after a failed recovery sync: nothing more is written.
        if !sync_failed {
            self.clean_up_files();
        }
        self.verify_restored();
        // 4. Rebuild volatile state from every readable entry.
        let d = self.derive();
        // The next position continues the current generation; a generation
        // started by a restore begins at its first position (§8).
        let generation = self.disk.generation;
        let next_position = d
            .entries
            .keys()
            .next_back()
            .filter(|p| p.generation == generation)
            .map_or(Position::first(generation), |p| p.next());
        // A consumer that is behind catches up from its cursor. Every later
        // start is a resume (§6 Restart and resume): replay, then `resync`.
        let mut resuming = Vec::new();
        let mut consumers = Vec::new();
        for (i, c) in self.consumers.iter().enumerate() {
            let s = self.sessions.entry(c.id).or_insert(0);
            *s += 1;
            let cursor = self
                .disk
                .cursors
                .get(&c.id)
                .copied()
                .unwrap_or(Position::ORIGIN);
            let behind = d.entries.range(cursor.next()..).next().is_some();
            let mode = if *s == 1 && behind {
                Mode::CatchUp {
                    next: cursor.next(),
                }
            } else {
                Mode::Live
            };
            if *s > 1 {
                resuming.push((i, cursor));
            }
            consumers.push(ConsumerRt {
                decl: c.clone(),
                queue: VecDeque::new(),
                mode,
                session: *s,
                last_received: cursor,
                unconfirmed: Vec::new(),
                resume: None,
                runners: BTreeMap::new(),
                emitted: BTreeMap::new(),
            });
        }
        self.vol = Some(Volatile {
            registry: d.registry,
            entries: d.entries,
            current: d.current,
            newest_t: d.newest_t,
            history: d.history,
            versions: d.versions,
            cp_versions: d.cp_versions,
            trees: d.trees,
            queue: VecDeque::new(),
            next_position,
            routes: d.routes,
            consumers,
            read_only: sync_failed || self.disk.drill || self.disk.superseded,
            sync_failed,
            skew_run: BTreeMap::new(),
            restore: d.restore,
            started: self.mono,
            retired_mono: BTreeMap::new(),
            // At writer start, every lease of the current boot counts as
            // first observed then (§7 Leases).
            leases_seen: self
                .disk
                .leases
                .iter()
                .filter(|(_, l)| l.boot == self.boot)
                .map(|(id, l)| (*id, (l.counter, self.mono)))
                .collect(),
        });
        for (i, cursor) in resuming {
            self.begin_resume(i, cursor);
        }
    }

    /// Derives registry, views, duplicate history and trees from the disk
    /// alone. While a restore is bringing detail back, the registry and trees
    /// start from its checkpoint and only entries after the checkpoint's
    /// position are applied (§8 Restore step 1).
    fn derive(&self) -> Derived {
        let entries = self.disk.live_entries();
        let restoring = self.disk.restore.as_ref().filter(|_| self.restoring());
        let cp = restoring.and_then(|r| r.checkpoint.as_ref());
        let since = cp.map_or(Position::ORIGIN, |c| c.position);
        let mut registry = cp.map_or_else(Registry::default, |c| c.registry.clone());
        let mut routes = BTreeMap::new();
        let mut facts = Vec::new();
        for (p, e) in &entries {
            match &e.body {
                EntryBody::Registry(r) if *p > since => registry.apply(r),
                EntryBody::Registry(_) => {}
                EntryBody::Route(r) => {
                    routes.insert(*p, r.clone());
                }
                EntryBody::Fact(f) => facts.push(f.clone()),
            }
        }
        // The version index: from the checkpoint and the facts after it
        // while restoring, so the rank of a fact whose outranking fact is
        // not yet local is still known (§8 Restore step 2).
        let mut versions = cp.map_or_else(BTreeMap::new, |c| c.versions.clone());
        for f in &facts {
            if let Some(c) = f.changed_at {
                let v = versions.entry((f.series, f.t)).or_insert(c);
                *v = (*v).max(c);
            }
        }
        let current = view::current(
            facts
                .iter()
                .filter(|f| f.changed_at.is_none_or(|c| c >= versions[&(f.series, f.t)])),
        );
        let mut newest_t = restoring.map_or_else(BTreeMap::new, |r| r.latest_t.clone());
        let mut history: History = BTreeMap::new();
        for f in &facts {
            history
                .entry((f.series, f.t))
                .or_default()
                .push(Known::of(f));
            let n = newest_t.entry(f.series).or_insert(f.t);
            *n = (*n).max(f.t);
        }
        // While restoring, the checkpoint's revised-key index, so a replay of
        // an earlier value of a revised key is dropped before the current
        // chunks are back (§8 Restore step 2).
        if let Some(cp) = cp {
            for (key, entries) in &cp.revised {
                let known = history.entry(*key).or_default();
                for e in entries {
                    if !known.iter().any(|k| k.position == e.position) {
                        known.push(Known {
                            position: e.position,
                            value: e.value.clone(),
                            quality: e.quality,
                            changed_at: e.changed_at,
                        });
                    }
                }
            }
        }
        let cp_versions = cp.map(|c| (c.position, c.versions.clone()));
        let (trees, restore) = match restoring {
            None => (build_trees(&registry, &current), None),
            Some(r) => {
                let (trees, rt) = self.restoring_trees(r, &registry, &entries, &current);
                if rt.index_back {
                    // The revised-key index: every value of each key with
                    // more than one restored fact (§2.4).
                    let revised = self.revised_keys(r);
                    for f in self.listed_facts(r) {
                        if revised.contains(&(f.series, f.t)) {
                            let known = history.entry((f.series, f.t)).or_default();
                            if !known.iter().any(|k| k.position == f.position) {
                                known.push(Known::of(&f));
                            }
                        }
                    }
                }
                (trees, Some(rt))
            }
        };
        Derived {
            registry,
            entries,
            routes,
            current,
            newest_t,
            history,
            versions,
            cp_versions,
            trees,
            restore,
        }
    }

    /// Replaces the derived parts of the running engine, keeping consumers,
    /// the ingest queue and the next position.
    fn rederive(&mut self) {
        let d = self.derive();
        if let Some(v) = self.vol.as_mut() {
            v.registry = d.registry;
            v.entries = d.entries;
            v.routes = d.routes;
            v.current = d.current;
            v.newest_t = d.newest_t;
            v.history = d.history;
            v.versions = d.versions;
            v.cp_versions = d.cp_versions;
            v.trees = d.trees;
            v.restore = d.restore;
        }
    }

    /// Trees while a restore is bringing detail back. Each series starts from
    /// the checkpoint's nodes. A span whose detail is all local is recomputed
    /// from facts (§8 Restore step 4). Elsewhere a minute holding a fact after
    /// the checkpoint's position is recomputed from the local facts; if the
    /// checkpoint holds a node for that minute, facts at or before its
    /// position are missing, so the minute is flagged `summary_stale` (§5.6,
    /// row 29). Other minutes keep the checkpoint's node.
    fn restoring_trees(
        &self,
        r: &RestoreState,
        registry: &Registry,
        entries: &BTreeMap<Position, Entry>,
        current: &BTreeMap<(SeriesId, Nanos), Fact>,
    ) -> (BTreeMap<SeriesId, SeriesTree>, RestoreRt) {
        let listed = self.listed_facts(r);
        let since = r
            .checkpoint
            .as_ref()
            .map_or(Position::ORIGIN, |c| c.position);
        let mut count: BTreeMap<(SeriesId, Nanos), Vec<Position>> = BTreeMap::new();
        let mut not_back = BTreeSet::new();
        for f in &listed {
            count.entry((f.series, f.t)).or_default().push(f.position);
            if !entries.contains_key(&f.position) {
                not_back.insert((f.series, floor_to(f.t, QUARTER)));
            }
        }
        let remote_single = count
            .into_iter()
            .filter(|(_, ps)| ps.len() == 1 && !entries.contains_key(&ps[0]))
            .map(|(k, _)| k)
            .collect();
        let mut checkpoint_minutes = BTreeSet::new();
        let mut trees = BTreeMap::new();
        for d in registry.all() {
            let params = params_of(d);
            let mut tree = r
                .checkpoint
                .as_ref()
                .and_then(|c| c.trees.get(&d.id))
                .map_or_else(|| SeriesTree::new(params), SeriesTree::nodes_only);
            // Minutes covered by a checkpoint node of the finest level the
            // series stores (§5.1 density rule).
            let held: BTreeSet<Nanos> = match (0..LEVELS.len()).find(|l| tree.stores(*l)) {
                Some(0) => tree.stored(0).map(|n| n.start).collect(),
                Some(l) => tree
                    .stored(l)
                    .flat_map(|n| (n.start..n.end).step_by(MINUTE as usize))
                    .collect(),
                None => BTreeSet::new(),
            };
            checkpoint_minutes.extend(held.iter().map(|m| (d.id, *m)));
            let mut after: BTreeMap<Nanos, bool> = held.iter().map(|m| (*m, false)).collect();
            for ((_, t), f) in current.range((d.id, Nanos::MIN)..=(d.id, Nanos::MAX)) {
                *after.entry(floor_to(*t, MINUTE)).or_insert(false) |= f.position > since;
            }
            for (m, after) in after {
                let back = !not_back.contains(&(d.id, floor_to(m, QUARTER)));
                if back || after {
                    tree.update_minute(m, &minute_readings(current, d.id, m));
                    tree.set_stale(m, !back && held.contains(&m));
                }
            }
            trees.insert(d.id, tree);
        }
        let rt = RestoreRt {
            latest_t: r.latest_t.clone(),
            index_back: self.index_back(r),
            remote_single,
            checkpoint_minutes,
            not_back,
        };
        (trees, rt)
    }

    /// Discards unpublished compaction outputs (§9 rows 9 and 10). A
    /// record publishes outputs and retires inputs atomically, so nothing
    /// else is left to finish.
    fn clean_up_files(&mut self) {
        let published = self.disk.published();
        let orphans: Vec<FileId> = self
            .disk
            .hot
            .iter()
            .filter(|f| !published.contains(f))
            .copied()
            .collect();
        for f in orphans {
            self.disk.hot.remove(&f);
            self.counters.compaction_discarded += 1;
        }
    }

    /// Same boot id as the mark: truncate everything after it. Different boot
    /// id, or no mark: keep the batches that reached the disk whole (a torn or
    /// lost batch is truncated), sync, and move the mark. Returns whether the
    /// engine must stay read-only because that sync failed.
    fn recover_tail(&mut self, sync_fails: bool) -> bool {
        let Some(active) = self.disk.active else {
            // No open segment: after a reboot the mark is still synced and
            // stamped with the new boot id.
            self.disk.tail_fate = None;
            let Some(mark) = self.disk.mark else {
                return false;
            };
            if mark.boot == self.boot {
                return false;
            }
            if sync_fails {
                return true;
            }
            self.sync_mark();
            return false;
        };
        let visible = self.disk.visible_batches(active).len();
        let tail = self.disk.files[&active].batches.len() - visible;
        let same_boot = self.disk.mark.is_some_and(|m| m.boot == self.boot);
        let keep = if same_boot {
            false
        } else {
            matches!(self.disk.tail_fate, None | Some(WriteOutcome::Survived))
        };
        if !keep && tail > 0 {
            self.disk
                .files
                .get_mut(&active)
                .expect("segment")
                .batches
                .truncate(visible);
            self.counters.batches_discarded += tail as u64;
        }
        // What is left on disk is now whole.
        self.disk.tail_fate = None;
        if same_boot {
            return false;
        }
        if sync_fails {
            return true;
        }
        self.sync_mark();
        false
    }

    /// Moves the durable mark to the end of the active segment, stamped with
    /// the current boot id (after a successful sync).
    fn sync_mark(&mut self) {
        let Some(active) = self.disk.active else {
            if let Some(mark) = self.disk.mark.as_mut() {
                mark.boot = self.boot;
            }
            return;
        };
        let file = &self.disk.files[&active];
        let last = file
            .entries()
            .last()
            .map(|e| e.position)
            .or(self.disk.mark.map(|m| m.last));
        if let Some(last) = last {
            self.disk.mark = Some(Mark {
                boot: self.boot,
                file: active,
                batches: file.batches.len(),
                last,
            });
        }
    }

    // ----- registry -----

    fn commit_registry(&mut self, ev: RegistryEvent) -> Result<Position, Error> {
        let wall = self.wall;
        let v = self.vol_mut()?;
        if v.read_only {
            return Err(Error::ReadOnly);
        }
        let position = v.next_position;
        v.next_position = position.next();
        let entry = Entry {
            position,
            received_at: wall,
            body: EntryBody::Registry(ev.clone()),
        };
        // Durable first; only then does the registry see the change.
        self.append_batch(vec![entry.clone()]);
        self.sync_mark();
        let v = self.vol_mut()?;
        v.registry.apply(&ev);
        v.entries.insert(position, entry);
        self.maybe_seal();
        Ok(position)
    }

    /// Defines a series, or returns the id of an identical live one.
    pub fn define(
        &mut self,
        name: &str,
        kind: Kind,
        value_type: ValueType,
        unit: &str,
        staleness: impl Into<Option<Nanos>>,
    ) -> Result<SeriesId, Error> {
        self.define_with(
            name,
            kind,
            value_type,
            unit,
            staleness,
            SeriesOptions::default(),
        )
    }

    /// Defines a series with the definition fields beyond kind, type and
    /// unit (§2.1: versioned, timing), or returns the id of an identical live
    /// one.
    pub fn define_with(
        &mut self,
        name: &str,
        kind: Kind,
        value_type: ValueType,
        unit: &str,
        staleness: impl Into<Option<Nanos>>,
        options: SeriesOptions,
    ) -> Result<SeriesId, Error> {
        let staleness = staleness.into();
        let ceiling = self.config.series_ceiling;
        let plan = self
            .vol()?
            .registry
            .plan_define(name, kind, value_type, unit, staleness, options, ceiling)?;
        match plan {
            Err(existing) => Ok(existing),
            Ok(ev) => {
                let RegistryEvent::Define { id, .. } = ev else {
                    unreachable!()
                };
                self.commit_registry(ev)?;
                let params = self.params(id)?;
                self.vol_mut()?.trees.insert(id, SeriesTree::new(params));
                Ok(id)
            }
        }
    }

    /// Changes kind, type or unit: retires the live series of that name and
    /// defines a new one with the same name (§9 row 24).
    pub fn redefine(
        &mut self,
        name: &str,
        kind: Kind,
        value_type: ValueType,
        unit: &str,
        staleness: impl Into<Option<Nanos>>,
    ) -> Result<SeriesId, Error> {
        let staleness = staleness.into();
        if let Some(old) = self.vol()?.registry.resolve(name).map(|d| d.id) {
            let at = self.now;
            self.retire(old, at)?;
        }
        self.define(name, kind, value_type, unit, staleness)
    }

    /// Adds an alias (rename, §9 row 23).
    pub fn rename(&mut self, id: SeriesId, new_name: &str) -> Result<(), Error> {
        let r = &self.vol()?.registry;
        let d = r.get(id).ok_or(RegistryError::Unknown(id))?;
        if d.retired {
            return Err(RegistryError::Retired(id).into());
        }
        if r.resolve(new_name).is_some_and(|o| o.id != id) {
            return Err(RegistryError::NameInUse(new_name.to_string()).into());
        }
        self.commit_registry(RegistryEvent::Alias {
            id,
            name: new_name.to_string(),
        })?;
        Ok(())
    }

    /// Retires a series effective at event time `at` (§2.1, §9 row 25).
    pub fn retire(&mut self, id: SeriesId, at: Nanos) -> Result<(), Error> {
        let d = self
            .vol()?
            .registry
            .get(id)
            .ok_or(RegistryError::Unknown(id))?;
        if d.retired {
            return Ok(());
        }
        self.commit_registry(RegistryEvent::Retire { id, at })?;
        Ok(())
    }

    /// Changes a series' retention class (§2.1): a registry entry that takes
    /// effect at its position, not by event time, and moves no detail back.
    pub fn set_retention(&mut self, id: SeriesId, class: RetentionClass) -> Result<(), Error> {
        let d = self
            .vol()?
            .registry
            .get(id)
            .ok_or(RegistryError::Unknown(id))?;
        if class == d.retention {
            return Ok(());
        }
        if matches!(class, RetentionClass::Diagnostic { period } if period <= 0) {
            return Err(RegistryError::BadRetention.into());
        }
        self.commit_registry(RegistryEvent::Retention { id, class })?;
        Ok(())
    }

    /// The registry.
    pub fn registry(&self) -> Result<&Registry, Error> {
        Ok(&self.vol()?.registry)
    }

    fn params(&self, id: SeriesId) -> Result<Params, Error> {
        let d = self
            .vol()?
            .registry
            .get(id)
            .ok_or(RegistryError::Unknown(id))?;
        Ok(params_of(d))
    }

    // ----- ingest -----

    /// Emits a reading into the bounded ingest queue.
    pub fn emit(&mut self, r: Reading) -> Result<(), EmitError> {
        self.emit_with(r, false)
    }

    /// Emits a reading with `revise` intent: it bypasses the duplicate check,
    /// so a deliberate return to an earlier value is recorded (§2.4).
    pub fn emit_revise(&mut self, r: Reading) -> Result<(), EmitError> {
        self.emit_with(r, true)
    }

    fn emit_with(&mut self, r: Reading, revise: bool) -> Result<(), EmitError> {
        let res = self.check(&r, revise);
        match res {
            Ok(()) => {
                self.vol
                    .as_mut()
                    .expect("checked")
                    .queue
                    .push_back((r, revise));
                Ok(())
            }
            Err(EmitError::Wait(w)) => {
                *self.counters.waits.entry(w.clone()).or_insert(0) += 1;
                Err(w.into())
            }
            Err(EmitError::Refused(e)) => {
                *self.counters.refused.entry(e.clone()).or_insert(0) += 1;
                Err(e.into())
            }
        }
    }

    fn check(&self, r: &Reading, revise: bool) -> Result<(), EmitError> {
        let v = self.vol.as_ref().ok_or(Refusal::Down)?;
        if v.read_only {
            return Err(Refusal::ReadOnly.into());
        }
        if self.free_bytes() < self.config.critical_mark {
            return Err(Wait::DiskFull.into());
        }
        let d = v.registry.get(r.series).ok_or(Refusal::UnknownSeries)?;
        if d.retired_at.is_some_and(|at| r.t >= at) {
            return Err(Refusal::Retired.into());
        }
        if d.options.interval.is_some_and(|iv| !iv.on_phase(r.t)) {
            return Err(Refusal::OffPhase.into());
        }
        match (d.options.versioned.is_some(), r.changed_at.is_some()) {
            (true, false) => return Err(Refusal::ChangedAtMissing.into()),
            (false, true) => return Err(Refusal::ChangedAtUnexpected.into()),
            (true, true) if revise => return Err(Refusal::ReviseOnVersioned.into()),
            _ => {}
        }
        let non_finite = matches!(r.value, Value::Num(x) if !x.is_finite());
        if !non_finite && !r.value.fits(d.value_type) {
            return Err(Refusal::WrongType.into());
        }
        if let Value::Str(s) = &r.value
            && s.len() > MAX_TEXT_BYTES
        {
            return Err(Refusal::TextTooLong.into());
        }
        if r.t_source == TSource::Host && !self.clock_synchronised {
            return Err(Wait::UnsyncedClock.into());
        }
        if v.queue.len() >= self.config.ingest_capacity {
            return Err(Wait::QueueFull.into());
        }
        Ok(())
    }

    /// Readings waiting in the ingest queue.
    pub fn queued(&self) -> usize {
        self.vol.as_ref().map_or(0, |v| v.queue.len())
    }

    /// Forms a batch from the queue and commits it: write, sync, then (and only
    /// then) live state and delivery (§3). `fault` injects a crash or a failed
    /// sync.
    pub fn commit(&mut self, fault: Option<Fault>) -> Result<CommitReport, Error> {
        let wall = self.wall;
        let cfg = self.config.clone();
        // Positions on a local tier, when some detail is not (§2.4); a
        // restore has its own rule (§8 Restore step 2).
        let restoring = self.vol.as_ref().is_some_and(|v| v.restore.is_some());
        let local =
            (!restoring && self.disk.any_detail_off_machine()).then(|| self.disk.local_positions());
        let v = self.vol.as_mut().ok_or(Error::Crashed)?;
        if v.read_only {
            return Err(Error::ReadOnly);
        }
        // Form the batch: stamp and flag; drop a reading identical to any fact
        // in the history of its key, including earlier facts of this batch,
        // unless it carries `revise` intent (§2.4). Identity compares the
        // value and the adapter-supplied quality. The check never reads
        // off-machine data: if the current fact of the key is on no local
        // tier, the reading is recorded and counted `duplicates_unchecked`
        // (§2.4, §12 D5).
        //
        // In a versioned series identity also includes `changed_at`; a fact
        // whose `changed_at` is above every one of its key's (the version
        // index) is never a duplicate; one whose `changed_at` equals that of
        // a fact off every local tier is recorded and counted unchecked. A
        // fact below the version index is outranked (§2.3, §4).
        let mut batch: Vec<Entry> = Vec::new();
        let mut batch_history: History = BTreeMap::new();
        let mut batch_versions: Versions = BTreeMap::new();
        let mut outranked: BTreeSet<Position> = BTreeSet::new();
        let mut batch_newest: BTreeMap<SeriesId, Nanos> = BTreeMap::new();
        let mut duplicates = 0;
        let mut unchecked = 0;
        let mut non_finite = 0;
        let mut negative = 0;
        let synced = self.clock_synchronised;
        let mut next = v.next_position;
        let readings: Vec<(Reading, bool)> = v.queue.drain(..).collect();
        for (r, revise) in readings {
            let Some(def) = v.registry.get(r.series) else {
                continue;
            };
            // A retirement committed while the reading waited in the queue
            // (§2.1): refused and counted.
            if def.retired_at.is_some_and(|at| r.t >= at) {
                *self.counters.refused.entry(Refusal::Retired).or_insert(0) += 1;
                continue;
            }
            let supplied = r.quality;
            // NaN and ±∞ (§12 D1), and a negative counter value (§5.5), are
            // recorded as bad quality without a value, flagged and counted;
            // the duplicate rule compares them as "no value" (§2.4).
            let (value, mut quality) = match r.value {
                Value::Num(x) if !x.is_finite() => {
                    non_finite += 1;
                    (Value::Absent, Quality::Bad)
                }
                Value::Num(x) if def.kind == Kind::Counter && x < 0.0 => {
                    negative += 1;
                    (Value::Absent, Quality::Bad)
                }
                other => (other, r.quality),
            };
            let no_value = matches!(value, Value::Absent);
            let observed = def.options.timing == Timing::Observed;
            // For a reading of an observed interval series `t + d` stands
            // for `t` in `future` and in the second clause of `late` (§2.2).
            let reach = match (observed, def.options.interval) {
                (true, Some(iv)) => r.t.saturating_add(iv.length),
                _ => r.t,
            };
            // `future` (§2.2): observed series only, and not before the
            // clock is synchronised (§2.5).
            let future = synced && observed && reach > wall.saturating_add(cfg.future_tolerance);
            if future {
                quality = quality.worst(Quality::Suspect);
            }
            let key = (r.series, r.t);
            let is_local = |k: &Known| local.as_ref().is_none_or(|l| l.contains(&k.position));
            let recorded: Vec<(&Known, bool)> = v
                .history
                .get(&key)
                .into_iter()
                .flatten()
                .map(|k| (k, is_local(k)))
                .chain(
                    batch_history
                        .get(&key)
                        .into_iter()
                        .flatten()
                        .map(|k| (k, true)),
                )
                .collect();
            let top = v
                .versions
                .get(&key)
                .copied()
                .max(batch_versions.get(&key).copied());
            // Whether the check is needed at all (§2.4).
            let checked = match r.changed_at {
                None => true,
                Some(c) => top.is_some_and(|top| c <= top),
            };
            let (seen, off_machine) = match r.changed_at {
                None => {
                    let off = local.is_some()
                        && !batch_history.contains_key(&key)
                        && v.current
                            .get(&key)
                            .is_some_and(|c| !is_local(&Known::of(c)));
                    let seen = recorded.iter().any(|(k, _)| k.identical(&value, supplied));
                    (seen, off)
                }
                Some(c) => {
                    let same: Vec<&(&Known, bool)> = recorded
                        .iter()
                        .filter(|(k, _)| k.changed_at == Some(c))
                        .collect();
                    let seen = same
                        .iter()
                        .any(|(k, local)| *local && k.identical(&value, supplied));
                    (seen, !seen && same.iter().any(|(_, local)| !local))
                }
            };
            if !revise && off_machine {
                unchecked += 1;
            } else if seen && !revise {
                duplicates += 1;
                continue;
            }
            // During a restore the check cannot run against facts that are
            // not yet local (§8 Restore step 2, §2.4).
            if !revise
                && checked
                && !off_machine
                && v.restore.as_ref().is_some_and(|rt| {
                    if rt.index_back {
                        rt.remote_single.contains(&key)
                    } else {
                        rt.latest_t.get(&r.series).is_some_and(|l| r.t <= *l)
                    }
                })
            {
                unchecked += 1;
            }
            // The latest recorded `t`: earlier batches and this one.
            let newest = batch_newest
                .get(&r.series)
                .copied()
                .max(v.newest_t.get(&r.series).copied());
            // `late` (§2.2): on an observed series, earlier than the latest
            // recorded `t` by more than 2 × S; on any series, older than the
            // lateness bound. Neither before the clock is synchronised (§2.5).
            let def = v.registry.get(r.series).expect("checked above");
            let out_of_order = match (newest, def.staleness) {
                (Some(n), Some(s)) => observed && r.t < n.saturating_sub(2 * s),
                _ => false,
            };
            let bound = def.options.lateness_bound.unwrap_or(cfg.lateness_bound);
            let late = synced && (out_of_order || wall.saturating_sub(reach) > bound);
            let fact = Fact {
                series: r.series,
                t: r.t,
                position: next,
                received_at: wall,
                value,
                quality,
                supplied_quality: supplied,
                changed_at: r.changed_at,
                flags: Flags {
                    future,
                    late,
                    revise,
                    host_time: r.t_source == TSource::Host,
                    unsynced_clock: !synced,
                    no_value,
                },
            };
            next = next.next();
            let n = batch_newest.entry(r.series).or_insert(r.t);
            *n = (*n).max(r.t);
            batch_history.entry(key).or_default().push(Known::of(&fact));
            if let Some(c) = fact.changed_at {
                if top.is_some_and(|top| c < top) {
                    outranked.insert(fact.position);
                }
                let v = batch_versions.entry(key).or_insert(c);
                *v = (*v).max(c);
            }
            batch.push(Entry {
                position: fact.position,
                received_at: wall,
                body: EntryBody::Fact(fact),
            });
        }
        v.next_position = next;
        self.counters.duplicates_dropped += duplicates as u64;
        self.counters.non_finite += non_finite;
        self.counters.negative_counter += negative;
        let mut report = CommitReport {
            positions: batch.iter().map(|e| e.position).collect(),
            duplicates,
        };
        if batch.is_empty() {
            return Ok(report);
        }
        if fault == Some(Fault::Crash(CrashPoint::BeforeWrite)) {
            self.crash();
            return Err(Error::Crashed);
        }
        // Write: the batch is in the active segment, past the durable mark.
        self.append_batch(batch.clone());
        match fault {
            Some(Fault::Crash(CrashPoint::AfterWrite(outcome))) => {
                self.disk.tail_fate = Some(outcome);
                self.crash();
                return Err(Error::Crashed);
            }
            Some(Fault::PowerLoss(outcome)) => {
                self.disk.tail_fate = Some(outcome);
                self.power_loss();
                return Err(Error::Crashed);
            }
            Some(Fault::SyncError(outcome)) => {
                self.disk.tail_fate = Some(outcome);
                let v = self.vol_mut()?;
                v.read_only = true;
                v.sync_failed = true;
                report.positions.clear();
                return Err(Error::SyncFailed);
            }
            _ => {}
        }
        // Sync: the batch is on disk.
        if fault == Some(Fault::Crash(CrashPoint::AfterSyncBeforeMark)) {
            self.disk.tail_fate = Some(WriteOutcome::Survived);
            self.crash();
            return Err(Error::Crashed);
        }
        // Durable mark; only now may anything see the batch.
        self.sync_mark();
        if fault == Some(Fault::Crash(CrashPoint::AfterSync)) {
            self.crash();
            return Err(Error::Crashed);
        }
        // Live state.
        self.counters.duplicates_unchecked += unchecked;
        self.counters.outranked_on_arrival += outranked.len() as u64;
        self.publish_live(&batch, &outranked)?;
        // An outranked fact is delivered, flagged, but never fed to a
        // condition (§4 Outranked facts).
        let fed: Vec<Entry> = batch
            .iter()
            .filter(|e| !outranked.contains(&e.position))
            .cloned()
            .collect();
        // Delivery.
        let stop_after = match fault {
            Some(Fault::Crash(CrashPoint::DuringDelivery(n))) => Some(n),
            _ => None,
        };
        let n_consumers = self.vol()?.consumers.len();
        for i in 0..n_consumers {
            if stop_after == Some(i) {
                self.crash();
                return Err(Error::Crashed);
            }
            self.deliver_live(i, &batch, &outranked);
            self.evaluate_funnels(i, Some(&fed));
        }
        if stop_after.is_some_and(|n| n >= n_consumers) {
            self.crash();
            return Err(Error::Crashed);
        }
        self.maybe_seal();
        Ok(report)
    }

    /// Convenience: emit all readings, then commit once without faults.
    pub fn ingest(&mut self, readings: Vec<Reading>) -> Result<CommitReport, Error> {
        for r in readings {
            let _ = self.emit(r);
        }
        self.commit(None)
    }

    fn ensure_active(&mut self) {
        if self.disk.active.is_none() {
            let id = FileId(self.disk.next_file);
            self.disk.next_file += 1;
            self.disk.files.insert(
                id,
                StoredFile {
                    id,
                    class: FileClass::Segment,
                    version: FORMAT_VERSION,
                    batches: Vec::new(),
                    sealed: false,
                    opened_at: self.wall,
                    sealed_at: None,
                },
            );
            self.disk.hot.insert(id);
            self.disk
                .publish(self.wall, &[id], Vec::new(), Vec::new(), Vec::new());
            self.disk.active = Some(id);
        }
    }

    fn append_batch(&mut self, batch: Vec<Entry>) {
        self.ensure_active();
        let id = self.disk.active.expect("active");
        self.disk
            .files
            .get_mut(&id)
            .expect("segment")
            .batches
            .push(batch);
    }

    fn maybe_seal(&mut self) {
        if let Some(id) = self.disk.active {
            let f = &self.disk.files[&id];
            if f.batches.len() >= self.config.seal_after_batches
                || self.wall - f.opened_at >= self.config.seal_interval
            {
                self.seal_active();
            }
        }
    }

    /// Seals the active segment now.
    pub fn seal_active(&mut self) {
        if self.disk.tail_len() > 0 {
            // Batches past the durable mark (failed sync): the segment is not
            // sealed until a restart has decided what reached the disk.
            return;
        }
        if let Some(id) = self.disk.active.take() {
            let wall = self.wall;
            let now = self.now;
            let f = self.disk.files.get_mut(&id).expect("segment");
            f.sealed = true;
            // On the engine clock `now`, as the retention class (§8).
            f.sealed_at = Some(now);
            let hash = self.disk.files[&id].hash();
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: wall,
                op: ManifestOp::Seal { file: id, hash },
            });
        }
    }

    fn publish_live(
        &mut self,
        batch: &[Entry],
        outranked: &BTreeSet<Position>,
    ) -> Result<(), Error> {
        let cfg = self.config.clone();
        let restoring = self.vol.as_ref().is_some_and(|v| v.restore.is_some());
        let local =
            (!restoring && self.disk.any_detail_off_machine()).then(|| self.disk.local_positions());
        let v = self.vol.as_mut().ok_or(Error::Crashed)?;
        let mut touched: BTreeSet<(SeriesId, Nanos)> = BTreeSet::new();
        for e in batch {
            v.entries.insert(e.position, e.clone());
            let Some(f) = e.fact() else { continue };
            v.history
                .entry((f.series, f.t))
                .or_default()
                .push(Known::of(f));
            if let Some(c) = f.changed_at {
                let top = v.versions.entry((f.series, f.t)).or_insert(c);
                *top = (*top).max(c);
            }
            let n = v.newest_t.entry(f.series).or_insert(f.t);
            *n = (*n).max(f.t);
            // An outranked fact changes no current fact and no node (§2.3,
            // §5.6).
            if !outranked.contains(&e.position) {
                v.current.insert((f.series, f.t), f.clone());
                touched.insert((f.series, floor_to(f.t, MINUTE)));
            }
            if f.flags.late {
                self.counters.late += 1;
            }
            if f.flags.future {
                self.counters.future += 1;
            }
            let skewed = f.received_at - f.t > cfg.skew_threshold;
            let run = v.skew_run.entry(f.series).or_insert(0);
            *run = if skewed { *run + 1 } else { 0 };
        }
        for (s, minute) in touched {
            if let Some(rt) = &v.restore {
                let stale = rt.checkpoint_minutes.contains(&(s, minute))
                    && rt.not_back.contains(&(s, floor_to(minute, QUARTER)));
                let rs = minute_readings(&v.current, s, minute);
                let tree = v.trees.get_mut(&s).expect("tree for defined series");
                tree.update_minute(minute, &rs);
                tree.set_stale(minute, stale);
                continue;
            }
            let mut rs: Vec<(Nanos, Value)> = Vec::new();
            let mut missing = false;
            for (_, f) in v.current.range((s, minute)..(s, minute + MINUTE)) {
                if let Some(local) = &local
                    && !local.contains(&f.position)
                    && !batch.iter().any(|e| e.position == f.position)
                {
                    missing = true;
                    continue;
                }
                if f.summarised() {
                    rs.push((f.t, f.value.clone()));
                }
            }
            let tree = v.trees.get_mut(&s).expect("tree for defined series");
            tree.update_minute(minute, &rs);
            tree.set_stale(minute, missing);
        }
        Ok(())
    }

    // ----- routing (§4 Routing) -----

    /// Adds a funnel to a consumer at run time: a log entry that takes effect
    /// at its position (§4 Routing). Facts at later positions are routed by
    /// it, in live delivery and in catch-up alike; a condition funnel starts
    /// with no latch and a fresh condition, and is first evaluated on the
    /// next routed fact or timer.
    pub fn add_route(
        &mut self,
        consumer: ConsumerId,
        funnel: FunnelId,
        spec: Funnel,
    ) -> Result<Position, Error> {
        let decl = self
            .consumers
            .iter()
            .find(|c| c.id == consumer)
            .ok_or(Error::UnknownConsumer(consumer))?;
        let v = self.vol()?;
        if routes_in_force(decl, &v.routes, None).contains_key(&funnel) {
            return Err(Error::BadRoute("funnel already in force"));
        }
        match (&spec.operator, &spec.condition) {
            (Some(_), None) => {
                return Err(Error::BadRoute(
                    "an operator without a condition is not modelled",
                ));
            }
            (op, Some(cond)) => {
                if op.is_some() && matches!(cond, Condition::HeldFor { .. }) {
                    return Err(Error::BadRoute(
                        "a held-for condition over a windowed operator is not modelled",
                    ));
                }
                let s = spec
                    .series()
                    .ok_or(Error::BadRoute("a condition funnel selects one series"))?;
                let Some(d) = v.registry.get(s) else {
                    return Err(Error::BadRoute("unknown series"));
                };
                if op
                    .as_ref()
                    .is_some_and(|o| crate::summary::refused(d.kind, &o.aggregate))
                {
                    return Err(Error::BadRoute(
                        "avg, integral, increase and time-in-state are refused on an interval series",
                    ));
                }
                // §4 Future facts, §6 Cadence.
                match (d.options.timing, cond) {
                    (Timing::Forecast, Condition::StaleFor { .. }) => {
                        return Err(Error::BadRoute(
                            "a stale-for condition on a forecast series is rejected",
                        ));
                    }
                    (Timing::Forecast, _) if op.is_none() => {
                        return Err(Error::BadRoute(
                            "a condition over a forecast series needs a windowed operator",
                        ));
                    }
                    (Timing::Scheduled, Condition::StaleFor { .. }) => {
                        return Err(Error::BadRoute(
                            "a stale-for condition on a scheduled series is not modelled",
                        ));
                    }
                    _ => {}
                }
            }
            (None, None) => {}
        }
        self.commit_route(RouteChange::Add {
            consumer,
            funnel,
            spec,
        })
    }

    /// Removes a funnel from a consumer at run time; it routes nothing at
    /// later positions (§4 Routing). Its latch is dropped.
    pub fn remove_route(
        &mut self,
        consumer: ConsumerId,
        funnel: FunnelId,
    ) -> Result<Position, Error> {
        let decl = self
            .consumers
            .iter()
            .find(|c| c.id == consumer)
            .ok_or(Error::UnknownConsumer(consumer))?;
        if !routes_in_force(decl, &self.vol()?.routes, None).contains_key(&funnel) {
            return Err(Error::BadRoute("funnel not in force"));
        }
        let p = self.commit_route(RouteChange::Remove { consumer, funnel })?;
        self.disk.latches.remove(&(consumer, funnel));
        Ok(p)
    }

    /// The funnels of a consumer in force now.
    pub fn routes(&self, consumer: ConsumerId) -> Result<BTreeMap<FunnelId, Funnel>, Error> {
        let decl = self
            .consumers
            .iter()
            .find(|c| c.id == consumer)
            .ok_or(Error::UnknownConsumer(consumer))?;
        Ok(routes_in_force(decl, &self.vol()?.routes, None))
    }

    fn commit_route(&mut self, change: RouteChange) -> Result<Position, Error> {
        let wall = self.wall;
        let v = self.vol_mut()?;
        if v.read_only {
            return Err(Error::ReadOnly);
        }
        let position = v.next_position;
        v.next_position = position.next();
        let entry = Entry {
            position,
            received_at: wall,
            body: EntryBody::Route(change.clone()),
        };
        // Durable first; only then does routing see the change.
        self.append_batch(vec![entry.clone()]);
        self.sync_mark();
        let v = self.vol_mut()?;
        v.entries.insert(position, entry);
        v.routes.insert(position, change);
        for i in 0..v.consumers.len() {
            self.sync_runners(i);
        }
        self.maybe_seal();
        Ok(position)
    }

    // ----- delivery -----

    fn consumer_index(&self, id: ConsumerId) -> Option<usize> {
        self.vol
            .as_ref()?
            .consumers
            .iter()
            .position(|c| c.decl.id == id)
    }

    /// The last committed position, or `None` before the first.
    fn head(&self) -> Option<Position> {
        self.vol
            .as_ref()
            .and_then(|v| v.entries.keys().next_back().copied())
    }

    fn deliver_live(&mut self, i: usize, batch: &[Entry], outranked: &BTreeSet<Position>) {
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        for e in batch {
            let Some(f) = e.fact() else { continue };
            if !routed(&c.decl, &v.routes, f) {
                continue;
            }
            match c.mode {
                Mode::Live => {
                    if c.queue.len() < c.decl.capacity {
                        c.queue.push_back(Envelope {
                            position: e.position,
                            replayed: false,
                            outranked: outranked.contains(&e.position),
                            kind: DeliveryKind::Fact(f.clone()),
                        });
                    } else {
                        // Catch up from the first position not enqueued
                        // in this session (§4 Overflow).
                        c.mode = Mode::CatchUp { next: e.position };
                        self.counters.overflows += 1;
                    }
                }
                Mode::CatchUp { .. } => {}
            }
        }
    }

    /// Creates a condition for each condition funnel in force that has none
    /// and drops those of funnels no longer in force. A consumer waiting for
    /// its `resync` gets its conditions there instead.
    fn sync_runners(&mut self, i: usize) {
        let mono = self.mono;
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        if c.resume.is_some() {
            return;
        }
        let funnels = routes_in_force(&c.decl, &v.routes, None);
        c.runners.retain(|f, _| funnels.contains_key(f));
        c.emitted.retain(|f, _| funnels.contains_key(f));
        for (id, f) in funnels {
            if let Some(cond) = f.condition
                && !c.runners.contains_key(&id)
            {
                let r = Runner::new(cond, mono);
                c.emitted.insert(id, r.state());
                c.runners.insert(id, r);
            }
        }
    }

    /// The value a condition funnel's condition sees now: the operator over
    /// its window ending at `now`, or without an operator the live-state
    /// value (§2.3).
    fn funnel_value(&self, f: &Funnel) -> Option<f64> {
        let s = f.series()?;
        match &f.operator {
            Some(op) => self
                .window(s, self.now - op.width, self.now)
                .ok()?
                .aggregate(&op.aggregate)?
                .to_f64(),
            None => match self.latest(s).ok()??.value {
                Value::Num(x) => Some(x),
                _ => None,
            },
        }
    }

    /// What a resume rebuilds of a condition's past (§6 Restart and resume,
    /// "rebuilt from the tree and recent window as of now"). Held for: how
    /// long the live-state value has been above its level without a break,
    /// judged exactly on the facts and the values they hold until the next
    /// fact. Stale for: the time since the series' last reading arrived,
    /// measured on the host clock that stamped its `received_at`, never
    /// against `now`, which a backward correction leaves ahead (§4 Timers;
    /// the measure itself is Q-16).
    fn resume_history(&self, f: &Funnel, cond: &Condition) -> ResumeHistory {
        let mut h = ResumeHistory::default();
        let (Some(s), Some(v)) = (f.series(), self.vol.as_ref()) else {
            return h;
        };
        match cond {
            Condition::HeldFor { above, .. } => {
                // Facts that set the live-state value, latest first: the run
                // of values above the level starts at the earliest fact of
                // the unbroken run that reaches now.
                let mut start = None;
                for (_, x) in v.current.range((s, Nanos::MIN)..=(s, self.now)).rev() {
                    if !x.summarised() {
                        continue;
                    }
                    if matches!(x.value, Value::Num(y) if y > *above) {
                        start = Some(x.t);
                    } else {
                        break;
                    }
                }
                h.above_for = start.map_or(0, |t| self.now - t);
            }
            Condition::StaleFor { .. } => {
                // The last arrival is the last committed reading, whose
                // stamp a backward correction may have put below earlier
                // ones.
                // An outranked fact refreshes nothing (§4 Outranked facts).
                h.since_arrival = v
                    .entries
                    .values()
                    .filter_map(Entry::fact)
                    .rfind(|x| {
                        x.series == s && !outranked_at_commit(&v.history, v.cp_versions.as_ref(), x)
                    })
                    .map(|x| (self.wall - x.received_at).max(0));
            }
            Condition::Threshold { .. } | Condition::OnChange { .. } => {}
        }
        h
    }

    /// Evaluates a consumer's condition funnels: on a committed batch (those
    /// whose series has a fact in it) or on the timer (all of them). Replayed
    /// facts never reach a condition (§4 Replay). A consumer waiting for its
    /// `resync` is not evaluated; its conditions are rebuilt then (§6).
    fn evaluate_funnels(&mut self, i: usize, batch: Option<&[Entry]>) {
        if self.vol.as_ref().expect("up").consumers[i].resume.is_some() {
            return;
        }
        self.sync_runners(i);
        let v = self.vol.as_ref().expect("up");
        let c = &v.consumers[i];
        let funnels = routes_in_force(&c.decl, &v.routes, None);
        for (id, f) in funnels {
            let Some(s) = f.series().filter(|_| f.condition.is_some()) else {
                continue;
            };
            let arrived = batch.map(|b| b.iter().filter_map(Entry::fact).any(|x| x.series == s));
            if arrived == Some(false) {
                continue;
            }
            let value = self.funnel_value(&f);
            let mono = self.mono;
            let runner = self.vol.as_mut().expect("up").consumers[i]
                .runners
                .get_mut(&id)
                .expect("runner for condition funnel");
            let mut events = Vec::new();
            if arrived == Some(true) {
                events.extend(runner.on_arrival(mono));
            }
            events.extend(runner.on_value(mono, value));
            if batch.is_none() {
                events.extend(runner.on_tick(mono));
            }
            let cause = if batch.is_some() {
                TriggerCause::Fact
            } else {
                TriggerCause::Timer
            };
            for e in events {
                self.emit_trigger(i, id, e.state(), cause);
            }
        }
    }

    /// Issues a condition delivery with the funnel's next sequence and the
    /// last committed position (§4 Order). A consumer that is catching up,
    /// or whose queue is full, is not sent it: the state is not emitted, and
    /// a full queue switches the consumer to catch-up (§4 Overflow). On
    /// re-attaching it receives the present state if that differs from the
    /// last emitted (see `OPEN-QUESTIONS.md`).
    fn emit_trigger(
        &mut self,
        i: usize,
        funnel: FunnelId,
        state: ConditionState,
        cause: TriggerCause,
    ) {
        let head = self.head().unwrap_or(Position::ORIGIN);
        let generation = self.disk.generation;
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        if !matches!(c.mode, Mode::Live) {
            return;
        }
        let incarnation = incarnation(&c.decl, &v.routes, funnel).unwrap_or(Position::ORIGIN);
        if c.queue.len() >= c.decl.capacity {
            c.mode = Mode::CatchUp { next: head.next() };
            self.counters.overflows += 1;
            return;
        }
        let seq = self.disk.sequences.entry((c.decl.id, funnel)).or_insert(0);
        *seq += 1;
        c.queue.push_back(Envelope {
            position: head,
            replayed: false,
            outranked: false,
            kind: DeliveryKind::Trigger(Trigger {
                funnel,
                incarnation,
                generation,
                sequence: *seq,
                state: state.clone(),
                cause,
            }),
        });
        c.emitted.insert(funnel, state);
    }

    /// Evaluates every condition funnel on its timer (§4 Timers, §6 Cadence:
    /// every second by default; the model evaluates when called). Timers use
    /// the monotonic clock.
    pub fn tick(&mut self) -> Result<(), Error> {
        let n = self.vol()?.consumers.len();
        for i in 0..n {
            self.evaluate_funnels(i, None);
        }
        Ok(())
    }

    /// Positions held by log segments on a local tier (hot or cold).
    fn local_log_positions(&self) -> BTreeSet<Position> {
        let mut out = BTreeSet::new();
        for f in self.disk.published() {
            let file = &self.disk.files[&f];
            let local = self.disk.locality(f) == Locality::Hot
                || (self.disk.cold.contains(&f) && !self.disk.quarantined.contains(&f));
            if file.class == FileClass::Segment && local {
                out.extend(
                    self.disk
                        .visible_entries(f, file)
                        .iter()
                        .map(|e| e.position),
                );
            }
        }
        out
    }

    /// Starts a consumer's resume after downtime (§6 Restart and resume): the
    /// entries after its cursor up to the last committed position are
    /// replayed through catch-up, then `resync` follows. If any of them is
    /// not in a local log segment, the cursor is too far behind: `resync`
    /// replaces the replay (§4 Resync, §8 Behind consumers). After a §3
    /// Recovery rule 3 recovery the last committed position is the last kept
    /// batch, so the one `resync` serves both rules.
    fn begin_resume(&mut self, i: usize, cursor: Position) {
        let stop = self.head().unwrap_or(cursor).max(cursor);
        let local = self.local_log_positions();
        let v = self.vol.as_mut().expect("up");
        let behind: Vec<Position> = v.entries.range(cursor.next()..).map(|(p, _)| *p).collect();
        let c = &mut v.consumers[i];
        if behind.is_empty() {
            c.mode = Mode::Live;
            self.issue_resync(i, stop, None);
        } else if behind.iter().all(|p| local.contains(p)) {
            c.mode = Mode::CatchUp {
                next: cursor.next(),
            };
            c.resume = Some(stop);
        } else {
            c.mode = Mode::Live;
            self.issue_resync(i, stop, Some((cursor, stop)));
        }
    }

    /// Queues any `generation_changed` owed, then `resync` with the live
    /// state and the condition state now of every funnel in force (§4
    /// Resync). Conditions are rebuilt as of now from the latch on disk; a
    /// state that differs from the latch, or with no latch, is a transition.
    fn issue_resync(
        &mut self,
        i: usize,
        position: Position,
        replaced: Option<(Position, Position)>,
    ) {
        let v = self.vol.as_ref().expect("up");
        let decl = v.consumers[i].decl.clone();
        let funnels = routes_in_force(&decl, &v.routes, None);
        let mut states = Vec::new();
        let mut runners = BTreeMap::new();
        let mut emitted = BTreeMap::new();
        for (id, f) in &funnels {
            let series: Vec<SeriesId> = match &f.selection {
                Selection::All => v.registry.all().map(|d| d.id).collect(),
                Selection::Series(set) => set.iter().copied().collect(),
            };
            let live = series
                .into_iter()
                .map(|s| (s, self.latest(s).ok().flatten()))
                .collect();
            let inc = incarnation(&decl, &v.routes, *id).unwrap_or(Position::ORIGIN);
            let condition = f.condition.as_ref().map(|cond| {
                let latch = self
                    .disk
                    .latches
                    .get(&(decl.id, *id))
                    .filter(|l| l.incarnation == inc)
                    .map(|l| &l.state);
                let history = self.resume_history(f, cond);
                let runner = Runner::resume(
                    cond.clone(),
                    self.mono,
                    self.funnel_value(f),
                    latch,
                    history,
                );
                let state = runner.state();
                let transition = cond.is_transition(&state, latch);
                runners.insert(*id, runner);
                emitted.insert(*id, state.clone());
                ConditionNow { state, transition }
            });
            states.push(FunnelState {
                funnel: *id,
                incarnation: inc,
                live,
                condition,
            });
        }
        // Owed until the consumer confirms it.
        let gc = self.disk.generation_changed.get(&decl.id).copied();
        let signals = self.signals.entry(decl.id).or_default();
        let issued = signals
            .iter()
            .filter(|x| matches!(x, Signal::ResyncDue))
            .count();
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        if let Some((cursor, restored_end)) = gc {
            c.queue.push_back(Envelope {
                position: restored_end,
                replayed: false,
                outranked: false,
                kind: DeliveryKind::GenerationChanged {
                    cursor,
                    restored_end,
                },
            });
            signals.push(Signal::GenerationChanged {
                cursor,
                restored_end,
            });
        }
        c.queue.push_back(Envelope {
            position,
            replayed: false,
            outranked: false,
            kind: DeliveryKind::Resync(Resync {
                funnels: states,
                replaced,
                issued,
            }),
        });
        signals.push(Signal::ResyncDue);
        c.runners = runners;
        c.emitted = emitted;
        c.resume = None;
    }

    /// On re-attaching to live after catch-up, a condition whose present
    /// state differs from the last state emitted to the consumer is sent now.
    fn reattach(&mut self, i: usize) {
        let c = &self.vol.as_ref().expect("up").consumers[i];
        let changed: Vec<(FunnelId, ConditionState)> = c
            .runners
            .iter()
            .filter(|(id, r)| c.emitted.get(id) != Some(&r.state()))
            .map(|(id, r)| (*id, r.state()))
            .collect();
        for (id, state) in changed {
            self.emit_trigger(i, id, state, TriggerCause::Reattach);
        }
    }

    /// Engine-side catch-up for a consumer: reads up to `max` log entries from
    /// its catch-up position into its queue, applying the routes in force at
    /// each position, flagged `replayed`. On resume it stops at the last
    /// position committed before the resume to queue `resync`. Switches back
    /// to live once it reaches the head.
    pub fn catch_up(&mut self, id: ConsumerId, max: usize) -> Result<(), Error> {
        self.vol()?;
        let Some(i) = self.consumer_index(id) else {
            return Ok(());
        };
        let mut reads = 0usize;
        loop {
            let v = self.vol.as_mut().expect("up");
            let c = &mut v.consumers[i];
            let Mode::CatchUp { mut next } = c.mode else {
                return Ok(());
            };
            // Entries in position order from `next`, across generations.
            let mut paused = false;
            for (p, e) in v.entries.range(next..) {
                if c.resume.is_some_and(|stop| *p > stop) {
                    break;
                }
                if reads >= max || c.queue.len() >= c.decl.capacity {
                    paused = true;
                    break;
                }
                reads += 1;
                if let Some(f) = e.fact()
                    && routed(&c.decl, &v.routes, f)
                {
                    c.queue.push_back(Envelope {
                        position: *p,
                        replayed: true,
                        outranked: outranked_at_commit(&v.history, v.cp_versions.as_ref(), f),
                        kind: DeliveryKind::Fact(f.clone()),
                    });
                }
                next = p.next();
            }
            c.mode = Mode::CatchUp { next };
            if paused {
                return Ok(());
            }
            if let Some(stop) = c.resume {
                self.issue_resync(i, stop, None);
                continue;
            }
            c.mode = Mode::Live;
            self.reattach(i);
            return Ok(());
        }
    }

    /// Takes up to `max` deliveries of any kind from the consumer's queue.
    pub fn receive(&mut self, id: ConsumerId, max: usize) -> Result<Vec<Envelope>, Error> {
        let v = self.vol_mut()?;
        let Some(c) = v.consumers.iter_mut().find(|c| c.decl.id == id) else {
            return Ok(Vec::new());
        };
        let n = max.min(c.queue.len());
        let out: Vec<Envelope> = c.queue.drain(..n).collect();
        let session = c.session;
        for d in &out {
            c.last_received = c.last_received.max(d.position);
        }
        c.unconfirmed.extend(out.iter().cloned());
        let obs = self.observed.entry(id).or_default();
        for d in &out {
            if d.fact().is_some() {
                obs.push(Observed {
                    session,
                    position: d.position,
                    replayed: d.replayed,
                });
            }
        }
        self.received
            .entry(id)
            .or_default()
            .extend(out.iter().map(|d| Received {
                session,
                delivery: d.clone(),
            }));
        Ok(out)
    }

    /// The consumer takes up to `max` deliveries from its queue; the fact
    /// deliveries among them are returned (see [`Model::receive`] for all).
    pub fn poll(&mut self, id: ConsumerId, max: usize) -> Result<Vec<Delivery>, Error> {
        Ok(self
            .receive(id, max)?
            .into_iter()
            .filter_map(|d| match d.kind {
                DeliveryKind::Fact(fact) => Some(Delivery {
                    position: d.position,
                    fact,
                    replayed: d.replayed,
                    outranked: d.outranked,
                }),
                _ => None,
            })
            .collect())
    }

    /// The consumer confirms it has processed everything up to `p`: every
    /// delivery taken in this session at or before `p`. The cursor, and the
    /// latch of each condition funnel whose state those deliveries carried,
    /// are durable at once.
    pub fn confirm(&mut self, id: ConsumerId, p: Position) -> Result<(), Error> {
        let i = self.consumer_index(id).ok_or(Error::BadConfirm)?;
        let c = &self.vol()?.consumers[i];
        if p > c.last_received {
            return Err(Error::BadConfirm);
        }
        let n = c.unconfirmed.iter().take_while(|d| d.position <= p).count();
        self.confirm_prefix(i, n, p);
        Ok(())
    }

    /// The consumer confirms a delivery by its identity, and with it every
    /// delivery taken before it in this session.
    pub fn confirm_delivery(&mut self, id: ConsumerId, d: DeliveryId) -> Result<(), Error> {
        let i = self.consumer_index(id).ok_or(Error::BadConfirm)?;
        let c = &self.vol()?.consumers[i];
        let n = c
            .unconfirmed
            .iter()
            .rposition(|x| x.id() == d)
            .ok_or(Error::BadConfirm)?
            + 1;
        let p = c.unconfirmed[..n]
            .iter()
            .map(|x| x.position)
            .max()
            .expect("non-empty");
        self.confirm_prefix(i, n, p);
        Ok(())
    }

    fn confirm_prefix(&mut self, i: usize, n: usize, p: Position) {
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        let id = c.decl.id;
        let done: Vec<Envelope> = c.unconfirmed.drain(..n).collect();
        let funnels = routes_in_force(&c.decl, &v.routes, None);
        let decl = c.decl.clone();
        // A latch is written only for the incarnation of the funnel that
        // emitted the state, if it is still in force.
        let mut latch = |f: FunnelId, inc: Position, state: &ConditionState, position: Position| {
            if funnels.get(&f).is_some_and(|x| x.condition.is_some())
                && incarnation(&decl, &v.routes, f) == Some(inc)
            {
                self.disk.latches.insert(
                    (id, f),
                    Latch {
                        incarnation: inc,
                        state: state.clone(),
                        position,
                    },
                );
            }
        };
        let mut gc_confirmed = false;
        for d in &done {
            match &d.kind {
                DeliveryKind::Trigger(t) => latch(t.funnel, t.incarnation, &t.state, d.position),
                DeliveryKind::Resync(r) => {
                    for f in &r.funnels {
                        if let Some(cn) = &f.condition {
                            latch(f.funnel, f.incarnation, &cn.state, d.position);
                        }
                    }
                }
                DeliveryKind::GenerationChanged { .. } => gc_confirmed = true,
                DeliveryKind::Fact(_) => {}
            }
        }
        if gc_confirmed {
            self.disk.generation_changed.remove(&id);
        }
        let cur = self.disk.cursors.entry(id).or_insert(Position::ORIGIN);
        *cur = (*cur).max(p);
        self.confirmed.entry(id).or_default().extend(done);
    }

    /// The consumer process restarted: its queue is dropped and it resumes
    /// from its durable cursor: catch-up, then `resync` (§6 Restart and
    /// resume, §9 rows 15 and 16).
    pub fn reconnect(&mut self, id: ConsumerId) -> Result<(), Error> {
        self.vol()?;
        let cursor = self
            .disk
            .cursors
            .get(&id)
            .copied()
            .unwrap_or(Position::ORIGIN);
        let s = self.sessions.entry(id).or_insert(0);
        *s += 1;
        let session = *s;
        let Some(i) = self.consumer_index(id) else {
            return Ok(());
        };
        let c = &mut self.vol_mut()?.consumers[i];
        c.queue.clear();
        c.unconfirmed.clear();
        c.runners.clear();
        c.emitted.clear();
        c.resume = None;
        c.session = session;
        c.last_received = cursor;
        self.begin_resume(i, cursor);
        Ok(())
    }

    /// Whether the consumer is in catch-up mode.
    pub fn in_catch_up(&self, id: ConsumerId) -> bool {
        self.vol.as_ref().is_some_and(|v| {
            v.consumers
                .iter()
                .any(|c| c.decl.id == id && matches!(c.mode, Mode::CatchUp { .. }))
        })
    }

    /// Runs catch-up, receiving and confirming for every consumer until all
    /// are live with empty queues.
    pub fn drain(&mut self) -> Result<(), Error> {
        let ids: Vec<ConsumerId> = self.consumers.iter().map(|c| c.id).collect();
        loop {
            let mut progress = false;
            for &id in &ids {
                self.catch_up(id, usize::MAX)?;
                let got = self.receive(id, usize::MAX)?;
                if let Some(d) = got.last() {
                    self.confirm_delivery(id, d.id())?;
                    progress = true;
                }
            }
            let quiet = self
                .vol()?
                .consumers
                .iter()
                .all(|c| matches!(c.mode, Mode::Live) && c.queue.is_empty());
            if quiet && !progress {
                return Ok(());
            }
        }
    }

    // ----- views and summaries -----

    /// All durable facts known to the running engine, by position.
    pub fn facts(&self) -> Result<Vec<Fact>, Error> {
        Ok(self
            .vol()?
            .entries
            .values()
            .filter_map(|e| e.fact().cloned())
            .collect())
    }

    /// All durable entries known to the running engine.
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        Ok(self.vol()?.entries.values().cloned().collect())
    }

    /// The live current view.
    pub fn current(&self) -> Result<&BTreeMap<(SeriesId, Nanos), Fact>, Error> {
        Ok(&self.vol()?.current)
    }

    /// Live state (§2.3): the current fact with the greatest `t ≤ now` and
    /// quality not bad, `t + d ≤ now` for an observed interval series (§2.2);
    /// a forecast series has none.
    pub fn latest(&self, s: SeriesId) -> Result<Option<Fact>, Error> {
        let v = self.vol()?;
        let Some(d) = v.registry.get(s) else {
            return Ok(None);
        };
        if d.options.timing == Timing::Forecast {
            return Ok(None);
        }
        let params = params_of(d);
        Ok(v.current
            .range((s, Nanos::MIN)..=(s, self.now))
            .rev()
            .map(|(_, f)| f)
            .find(|f| f.summarised() && params.ready_at(f.t) <= self.now)
            .cloned())
    }

    /// The live summary tree of a series.
    pub fn tree(&self, s: SeriesId) -> Result<&SeriesTree, Error> {
        self.vol()?
            .trees
            .get(&s)
            .ok_or(Error::Registry(RegistryError::Unknown(s)))
    }

    /// Live trees of all series.
    pub fn trees(&self) -> Result<&BTreeMap<SeriesId, SeriesTree>, Error> {
        Ok(&self.vol()?.trees)
    }

    /// Rebuilds every derived structure from the facts alone and returns the
    /// trees (§1.2). Does not change the running engine.
    pub fn rebuild_trees(&self) -> Result<BTreeMap<SeriesId, SeriesTree>, Error> {
        let v = self.vol()?;
        let facts: Vec<Fact> = v
            .entries
            .values()
            .filter_map(|e| e.fact().cloned())
            .collect();
        let current = view::current(facts.iter());
        Ok(build_trees(&v.registry, &current))
    }

    /// A window `[start, end)` over a series from the tree, evaluated at the
    /// engine clock `now` (§6).
    pub fn window(&self, s: SeriesId, start: Nanos, end: Nanos) -> Result<Node, Error> {
        self.query_window(s, start, end).map(|a| a.node)
    }

    /// A window with its flags. While a restore is bringing detail back, only
    /// windows at 1-minute resolution are served, flagged `restoring`
    /// (§8 Restore step 2); a bound inside a minute is refused.
    pub fn query_window(&self, s: SeriesId, start: Nanos, end: Nanos) -> Result<Answer, Error> {
        let restoring = self.restoring();
        // Resolution is judged on the requested bounds; clipping at `now`
        // does not make a window finer.
        if restoring && (floor_to(start, MINUTE) != start || floor_to(end, MINUTE) != end) {
            return Err(Error::Restoring);
        }
        let tree = self.tree(s)?;
        let until = tree.params.credit_limit(end, self.now).max(start);
        Ok(Answer {
            node: tree.window(start, end, self.now),
            restoring,
            summary_stale: tree.any_stale(start, until),
        })
    }

    /// Live staleness of a scheduled series (§5.4): stale when `now` is later
    /// than the `t` of its live-state fact by more than S, or when it has no
    /// live-state fact; commits do not refresh it. `None` for any other
    /// series: a forecast series has no live staleness, and an observed
    /// series is measured on the monotonic clock by stale-for conditions
    /// (see Q-16).
    pub fn scheduled_stale(&self, s: SeriesId) -> Result<Option<bool>, Error> {
        let d = self
            .vol()?
            .registry
            .get(s)
            .ok_or(RegistryError::Unknown(s))?;
        if d.options.timing != Timing::Scheduled {
            return Ok(None);
        }
        let staleness = d.staleness;
        Ok(Some(self.latest(s)?.is_none_or(|f| {
            staleness.is_some_and(|limit| self.now - f.t > limit)
        })))
    }

    /// Decreases of a counter counted as regressions over its current view
    /// (§5.5, §12 D2: each contributes 0 and is counted, `counter_regressions`).
    pub fn counter_regressions(&self, s: SeriesId) -> Result<u64, Error> {
        let tree = self.tree(s)?;
        let rs = tree.readings();
        let (Some(first), Some(last)) = (rs.first(), rs.last()) else {
            return Ok(0);
        };
        let node = crate::summary::direct(&tree.params, first.0, last.0 + 1, &rs);
        Ok(match node.body {
            crate::summary::Body::Counter { regressions, .. } => regressions,
            _ => 0,
        })
    }

    /// Fetches missing detail for a minute and recomputes its path (§5.6).
    pub fn fetch_detail(&mut self, s: SeriesId, minute: Nanos) -> Result<(), Error> {
        let v = self.vol_mut()?;
        let rs: Vec<(Nanos, Value)> = v
            .current
            .range((s, minute)..(s, minute + MINUTE))
            .map(|(_, f)| f)
            .filter(|f| f.summarised())
            .map(|f| (f.t, f.value.clone()))
            .collect();
        let tree = v.trees.get_mut(&s).ok_or(RegistryError::Unknown(s))?;
        tree.update_minute(minute, &rs);
        tree.set_stale(minute, false);
        Ok(())
    }

    // ----- health -----

    /// Free bytes on the fast disk.
    pub fn free_bytes(&self) -> u64 {
        self.config
            .disk_bytes
            .saturating_sub(self.disk.used_bytes(self.config.entry_bytes))
    }

    /// Current health.
    pub fn health(&self) -> Health {
        let mut reasons = Vec::new();
        let mut level = Level::Ok;
        let mut raise = |l: Level, r: Reason, reasons: &mut Vec<Reason>| {
            level = level.max(l);
            reasons.push(r);
        };
        // A drill is read-only by design and raises nothing of its own.
        if self.vol.as_ref().is_some_and(|v| v.sync_failed) {
            raise(Level::Fatal, Reason::SyncFailed, &mut reasons);
        }
        if self.disk.superseded {
            raise(Level::Fatal, Reason::Superseded, &mut reasons);
        }
        if self.disk.restore_failed.is_some() {
            raise(Level::Fatal, Reason::RestoreFailed, &mut reasons);
        }
        for f in &self.disk.quarantined {
            raise(Level::Fatal, Reason::Quarantined(*f), &mut reasons);
        }
        let free = self.free_bytes();
        if free < self.config.critical_mark {
            raise(Level::Degraded, Reason::CriticalDisk, &mut reasons);
        } else if free < self.config.low_mark {
            raise(Level::Degraded, Reason::LowDisk, &mut reasons);
        }
        if !self.disk.bucket.reachable && !self.disk.upload_queue().is_empty() {
            raise(Level::Degraded, Reason::UploadStalled, &mut reasons);
        }
        if let Some(v) = &self.vol {
            for (s, run) in &v.skew_run {
                if *run >= self.config.skew_count {
                    raise(Level::Degraded, Reason::ClockSkew(*s), &mut reasons);
                }
            }
        }
        Health { level, reasons }
    }

    // ----- compaction, manifest, tiers -----

    /// Whether a position is set aside beyond a restored end (§8 Restore
    /// step 1).
    fn set_aside(&self, p: Position) -> bool {
        self.disk
            .restored_end
            .is_some_and(|end| p.generation < self.disk.generation && p > end)
    }

    /// Positions held by live compacted files, and while a restore brings
    /// detail back, by the compacted files it will bring back: a log segment
    /// whose positions they hold is not compacted again.
    fn chunk_positions(&self) -> BTreeSet<Position> {
        let mut out = BTreeSet::new();
        for f in self.disk.listed_at(self.disk.latest_epoch()) {
            let file = &self.disk.files[&f];
            if file.class != FileClass::Segment {
                out.extend(file.entries().map(|e| e.position));
            }
        }
        if let Some(r) = self.disk.restore.as_ref().filter(|_| self.restoring()) {
            for f in &r.sources {
                let file = &self.disk.bucket.objects[f].1;
                if file.class != FileClass::Segment {
                    out.extend(file.entries().map(|e| e.position));
                }
            }
        }
        out
    }

    /// Writes the chunks for `entries` (§7): facts outranked by a fact among
    /// the same entries go to revision chunks, every other fact to current
    /// chunks, one of each per series; facts set aside beyond a restored
    /// end take no part in the ranking and go to current chunks; other
    /// entries (registry and routing) go to one chunk of their own. Each
    /// output keeps `sealed_at`, the seal time of its oldest input, so its age
    /// (§8 Eviction at 90 days) is not reset by being rewritten. Returns the
    /// new files, unpublished.
    fn write_chunks(&mut self, entries: Vec<Entry>, sealed_at: Option<Nanos>) -> Vec<FileId> {
        let mut best: BTreeMap<(SeriesId, Nanos), (u64, Position)> = BTreeMap::new();
        for f in entries.iter().filter_map(Entry::fact) {
            if !self.set_aside(f.position) {
                let b = best.entry((f.series, f.t)).or_insert(f.rank());
                *b = (*b).max(f.rank());
            }
        }
        let mut groups: BTreeMap<(Option<SeriesId>, u8), Vec<Entry>> = BTreeMap::new();
        for e in entries {
            let group = match e.fact() {
                Some(f) if !self.set_aside(f.position) && best[&(f.series, f.t)] != f.rank() => {
                    (Some(f.series), 1)
                }
                Some(f) => (Some(f.series), 0),
                None => (None, 0),
            };
            groups.entry(group).or_default().push(e);
        }
        let mut outputs = Vec::new();
        for ((_, class), mut entries) in groups {
            entries.sort_by_key(|e| e.position);
            let id = FileId(self.disk.next_file);
            self.disk.next_file += 1;
            self.disk.files.insert(
                id,
                StoredFile {
                    id,
                    class: if class == 0 {
                        FileClass::Current
                    } else {
                        FileClass::Revision
                    },
                    version: FORMAT_VERSION,
                    batches: vec![entries],
                    sealed: true,
                    opened_at: self.wall,
                    sealed_at: sealed_at.or(Some(self.now)),
                },
            );
            self.disk.hot.insert(id);
            outputs.push(id);
        }
        outputs
    }

    /// The earliest seal time among `files`.
    fn oldest_seal(&self, files: &[FileId]) -> Option<Nanos> {
        files
            .iter()
            .filter_map(|f| self.disk.files[f].sealed_at)
            .min()
    }

    /// Verifies compaction outputs against their inputs: the same entries,
    /// each with identical content (§7: fact count and per-series checksum).
    fn verify_chunks(&self, inputs: &[Entry], outputs: &[FileId]) {
        let mut ins: Vec<&Entry> = inputs.iter().collect();
        let mut outs: Vec<&Entry> = outputs
            .iter()
            .flat_map(|f| self.disk.files[f].entries())
            .collect();
        ins.sort_by_key(|e| e.position);
        outs.sort_by_key(|e| e.position);
        assert_eq!(ins, outs, "compaction verification");
    }

    /// Compacts the sealed log segments not yet compacted into per-series
    /// current and revision chunks, and publishes them all with one manifest
    /// record (§7). Facts are ranked within these inputs only, so a current
    /// chunk may hold a fact outranked later or by a fact in another file.
    /// The input segments stay listed: compaction never retires a log
    /// segment (§7, §8). Returns the new files.
    pub fn compact(&mut self, crash: Option<CompactionCrash>) -> Result<Vec<FileId>, Error> {
        self.vol()?;
        if self.free_bytes() < self.config.low_mark {
            self.counters.paused += 1;
            return Err(Error::Paused);
        }
        let retired = self.disk.retired();
        let compacted = self.disk.compacted_segments();
        let covered = self.chunk_positions();
        let inputs: Vec<FileId> = self
            .disk
            .published()
            .into_iter()
            .filter(|f| {
                let file = &self.disk.files[f];
                file.class == FileClass::Segment
                    && file.sealed
                    && !retired.contains_key(f)
                    && !compacted.contains(f)
                    && !file.entries().all(|e| covered.contains(&e.position))
            })
            .collect();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let entries: Vec<Entry> = inputs
            .iter()
            .flat_map(|f| self.disk.files[f].entries())
            .filter(|e| !covered.contains(&e.position))
            .cloned()
            .collect();
        let oldest = self.oldest_seal(&inputs);
        let outputs = self.write_chunks(entries.clone(), oldest);
        if crash == Some(CompactionCrash::AfterWrite) {
            self.crash();
            return Err(Error::Crashed);
        }
        self.verify_chunks(&entries, &outputs);
        self.disk
            .publish(self.wall, &outputs, inputs, Vec::new(), Vec::new());
        if crash == Some(CompactionCrash::AfterPublish) {
            self.crash();
            return Err(Error::Crashed);
        }
        Ok(outputs)
    }

    /// The series of a compacted chunk, and the span `[first t, last t]` of
    /// its facts.
    fn chunk_span(&self, f: FileId) -> Option<(SeriesId, Nanos, Nanos)> {
        let mut facts = self.disk.files[&f].entries().filter_map(Entry::fact);
        let first = facts.next()?;
        let (lo, hi) = facts.fold((first.t, first.t), |(lo, hi), x| (lo.min(x.t), hi.max(x.t)));
        Some((first.series, lo, hi))
    }

    /// Compaction of compacted files (§7): for each series whose live
    /// current chunks on the fast disk overlapping some 15-minute span
    /// number more than `merge_threshold`, merges the chunks overlapping
    /// such a span into new current and revision chunks (ranked within the
    /// merged chunks), and publishes the outputs and retires the inputs in
    /// one manifest record. Returns the new files.
    pub fn merge_chunks(&mut self, crash: Option<CompactionCrash>) -> Result<Vec<FileId>, Error> {
        self.vol()?;
        if self.free_bytes() < self.config.low_mark {
            self.counters.paused += 1;
            return Err(Error::Paused);
        }
        let mut by_series: BTreeMap<SeriesId, Vec<(FileId, Nanos, Nanos)>> = BTreeMap::new();
        for f in self.disk.listed_at(self.disk.latest_epoch()) {
            if self.disk.files[&f].class == FileClass::Current
                && self.disk.locality(f) == Locality::Hot
                && let Some((s, lo, hi)) = self.chunk_span(f)
            {
                by_series.entry(s).or_default().push((f, lo, hi));
            }
        }
        let mut all = Vec::new();
        for chunks in by_series.into_values() {
            let mut count: BTreeMap<Nanos, usize> = BTreeMap::new();
            for (_, lo, hi) in &chunks {
                let mut q = floor_to(*lo, QUARTER);
                while q <= *hi {
                    *count.entry(q).or_default() += 1;
                    q += QUARTER;
                }
            }
            let crowded: BTreeSet<Nanos> = count
                .into_iter()
                .filter(|(_, n)| *n > self.config.merge_threshold)
                .map(|(q, _)| q)
                .collect();
            let inputs: Vec<FileId> = chunks
                .iter()
                .filter(|(_, lo, hi)| crowded.range(floor_to(*lo, QUARTER)..=*hi).next().is_some())
                .map(|(f, _, _)| *f)
                .collect();
            if inputs.is_empty() {
                continue;
            }
            let entries: Vec<Entry> = inputs
                .iter()
                .flat_map(|f| self.disk.files[f].entries())
                .cloned()
                .collect();
            let oldest = self.oldest_seal(&inputs);
            let outputs = self.write_chunks(entries.clone(), oldest);
            if crash == Some(CompactionCrash::AfterWrite) {
                self.crash();
                return Err(Error::Crashed);
            }
            self.verify_chunks(&entries, &outputs);
            let epoch = self
                .disk
                .publish(self.wall, &outputs, Vec::new(), inputs, Vec::new());
            let mono = self.mono;
            self.vol_mut()?.retired_mono.insert(epoch, mono);
            if crash == Some(CompactionCrash::AfterPublish) {
                self.crash();
                return Err(Error::Crashed);
            }
            all.extend(outputs);
        }
        Ok(all)
    }

    /// The latest manifest epoch and the files it lists, with no lease: an
    /// inspection of the manifest, not a reader process (see
    /// [`Model::lease_reader`]).
    pub fn open_reader(&self) -> Reader {
        let epoch = self.disk.latest_epoch();
        Reader {
            epoch,
            files: self.disk.listed_at(epoch),
            lease: None,
        }
    }

    /// A reader process (§7 Readers): takes the latest epoch and writes a
    /// lease file naming it, with this machine's boot id and a refresh
    /// counter. It needs nothing from the writer's memory, so it works while
    /// the writer is down.
    pub fn lease_reader(&mut self) -> Reader {
        let id = self.disk.next_lease;
        self.disk.next_lease += 1;
        let epoch = self.disk.latest_epoch();
        self.disk.leases.insert(
            id,
            Lease {
                epoch,
                boot: self.boot,
                counter: 0,
            },
        );
        Reader {
            epoch,
            files: self.disk.listed_at(epoch),
            lease: Some(id),
        }
    }

    /// A reader refreshes its lease (§7: at least every 30 minutes): it
    /// increments the refresh counter and, with `latest`, takes the latest
    /// epoch, which the lease then names. The boot id stays the one the lease
    /// was written with, so a lease from before a reboot stays void. A reader
    /// without a lease file is returned unchanged.
    pub fn refresh_reader(&mut self, r: &Reader, latest: bool) -> Reader {
        let Some(lease) = r.lease.and_then(|id| self.disk.leases.get_mut(&id)) else {
            return r.clone();
        };
        if latest {
            lease.epoch = self.disk.manifest.last().map_or(0, |x| x.epoch);
        }
        lease.counter += 1;
        let epoch = lease.epoch;
        Reader {
            epoch,
            files: self.disk.listed_at(epoch),
            lease: r.lease,
        }
    }

    /// A reader ends and removes its lease file.
    pub fn close_reader(&mut self, r: &Reader) {
        if let Some(id) = r.lease {
            self.disk.leases.remove(&id);
        }
    }

    /// The reader processes with a lease file.
    pub fn readers(&self) -> Vec<Reader> {
        self.disk
            .leases
            .iter()
            .map(|(id, l)| Reader {
                epoch: l.epoch,
                files: self.disk.listed_at(l.epoch),
                lease: Some(*id),
            })
            .collect()
    }

    /// The writer reads the lease files (§7 Leases): a lease of another boot
    /// is void; a lease whose counter differs from the value last observed is
    /// observed now.
    fn observe_leases(&mut self) {
        let (boot, mono) = (self.boot, self.mono);
        let Some(v) = self.vol.as_mut() else { return };
        v.leases_seen
            .retain(|id, _| self.disk.leases.get(id).is_some_and(|l| l.boot == boot));
        for (id, l) in &self.disk.leases {
            if l.boot != boot {
                continue;
            }
            match v.leases_seen.get(id) {
                Some((counter, _)) if *counter == l.counter => {}
                _ => {
                    v.leases_seen.insert(*id, (l.counter, mono));
                }
            }
        }
    }

    /// Epochs named by live leases: of this boot, with a counter that has
    /// changed within `lease_expiry` of the writer's monotonic clock since it
    /// was first observed (§7 Leases).
    pub fn live_lease_epochs(&mut self) -> Vec<u64> {
        self.observe_leases();
        let Some(v) = self.vol.as_ref() else {
            return Vec::new();
        };
        self.disk
            .leases
            .iter()
            .filter(|(id, l)| {
                l.boot == self.boot
                    && v.leases_seen
                        .get(id)
                        .is_some_and(|(_, at)| self.mono - at < self.config.lease_expiry)
            })
            .map(|(_, l)| l.epoch)
            .collect()
    }

    /// Reads every file a reader's epoch lists: where each was found, and the
    /// facts (deduplicated by position). Refuses newer format versions.
    pub fn read(&self, r: &Reader) -> Result<ReadResult, Error> {
        let mut loc = Vec::new();
        let mut facts = BTreeMap::new();
        for f in &r.files {
            let l = self.disk.locality(*f);
            loc.push((*f, l));
            if let Some(file) = self.disk.readable(*f) {
                if file.version > FORMAT_VERSION {
                    return Err(Error::NewerFormat(*f, file.version));
                }
                // Log segments only up to the durable mark (§3).
                for e in self.disk.visible_entries(*f, file) {
                    if let Some(fact) = e.fact() {
                        facts.insert(e.position, fact.clone());
                    }
                }
            }
        }
        Ok((loc, facts.into_values().collect()))
    }

    /// Uploads queued sealed files to the bucket, verifying hashes (§8).
    pub fn upload(&mut self) -> usize {
        if !self.disk.bucket.reachable || self.disk.drill || self.disk.superseded {
            return 0;
        }
        // A writer that finds a higher generation claimed stops uploading and
        // becomes read-only (§8 Generations), durably, so a restart keeps it.
        if self
            .disk
            .bucket
            .claims
            .iter()
            .any(|g| *g > self.disk.generation)
        {
            self.disk.superseded = true;
            if let Some(v) = self.vol.as_mut() {
                v.read_only = true;
            }
            return 0;
        }
        let mut n = 0;
        for f in self.disk.upload_queue() {
            let Some(want) = self.disk.manifest_hash(f) else {
                continue;
            };
            let content = self.disk.files[&f].clone();
            let sent = content.hash();
            if std::mem::take(&mut self.disk.bucket.corrupt_next_upload) || sent != want {
                self.counters.uploads_rejected += 1;
                continue;
            }
            self.disk.bucket.objects.insert(f, (want, content));
            n += 1;
        }
        // After its files, each manifest record (§8 Continuous backup).
        for r in &self.disk.manifest {
            if let ManifestOp::Publish { added, retired, .. } = &r.op
                && !retired.is_empty()
                && added.iter().all(|a| self.disk.in_bucket(a.file))
            {
                self.disk.bucket.retired.extend(retired.iter().copied());
            }
        }
        n
    }

    /// Copies a file to the cold tier (verified on write).
    pub fn copy_to_cold(&mut self, f: FileId) {
        if self.disk.locality(f) == Locality::Hot {
            self.disk.cold.insert(f);
        }
    }

    /// Removes files from the fast disk where the rules allow (§7 retirement,
    /// §8 eviction and fill threshold). Returns the files removed.
    pub fn housekeeping(&mut self) -> Result<Vec<FileId>, Error> {
        // Tiering and eviction free space and continue at every mark (§3
        // Disk space).
        self.vol()?;
        let mut removed = Vec::new();
        let retired = self.disk.retired();
        // Retired files (§7): removed locally only after the grace period on
        // the writer's monotonic clock since the later of the retirement
        // record and the writer's start, once the files that replaced them
        // are verified in another tier, and when no live lease names an
        // older epoch. A copy leaves a tier only when another tier holds a
        // verified copy (§1.4, invariant 12).
        let leases = self.live_lease_epochs();
        let v = self.vol()?;
        let removable: Vec<FileId> = retired
            .iter()
            .filter(|(f, (epoch, replacements))| {
                let since = v.retired_mono.get(epoch).copied().unwrap_or(v.started);
                (self.disk.hot.contains(f) || self.disk.cold.contains(f))
                    && self.mono - since >= self.config.grace
                    && replacements
                        .iter()
                        .all(|r| self.disk.verified_elsewhere(*r))
                    && !leases.iter().any(|e| e < epoch)
            })
            .map(|(f, _)| *f)
            .collect();
        for f in removable {
            if self.disk.hot.contains(&f) && self.disk.verified_elsewhere(f) {
                self.disk.hot.remove(&f);
                removed.push(f);
            }
            if self.disk.cold.contains(&f) && self.disk.in_bucket(f) {
                self.disk.cold.remove(&f);
            }
        }
        // Files that leave hot by the tier rules (§8): each needs a verified
        // copy in cold or deep first, and the moves are recorded in one
        // manifest record (§7: a tier move keeps the file's identity).
        let registry = self.vol()?.registry.clone();
        let movable = |m: &Model, f: FileId| {
            m.disk.verified_elsewhere(f) && m.disk.active != Some(f) && !retired.contains_key(&f)
        };
        let mut moves: Vec<FileId> = Vec::new();
        for f in self.disk.hot.iter().copied() {
            if !movable(self, f) {
                continue;
            }
            let file = &self.disk.files[&f];
            let age = |limit: Nanos| file.sealed_at.is_some_and(|s| self.now - s >= limit);
            // Retention class (§8): a diagnostic series' raw detail whose span
            // ends more than its retention period before `now`.
            let span = self.chunk_span(f);
            let expired = span.is_some_and(|(s, _, hi)| {
                registry.get(s).is_some_and(|d| match d.retention {
                    RetentionClass::Diagnostic { period } => hi < self.now - period,
                    RetentionClass::LongTerm => false,
                })
            });
            let leave = match file.class {
                // Eviction at 90 days (§8).
                FileClass::Segment => age(self.config.eviction_age),
                // Age per series, "never" allowed; or the retention class,
                // whichever is earlier.
                FileClass::Revision => {
                    let limit = span.map_or(Some(self.config.eviction_age), |(s, _, _)| {
                        self.config
                            .revision_age
                            .get(&s)
                            .copied()
                            .unwrap_or(Some(self.config.eviction_age))
                    });
                    limit.is_some_and(age) || expired
                }
                // Current detail stays, except as the retention class allows.
                FileClass::Current => expired,
            };
            if leave {
                moves.push(f);
            }
        }
        for f in &moves {
            self.disk.hot.remove(f);
        }
        // Fill pressure (§8, §12 D4): above the fill threshold, verified
        // sealed log segments leave first, oldest first, then the oldest
        // current raw detail, verified first. The registry's chunk stays.
        let limit = self.config.disk_bytes / 100 * self.config.fill_threshold_pct;
        let mut segments: Vec<FileId> = self
            .disk
            .hot
            .iter()
            .copied()
            .filter(|f| self.disk.files[f].class == FileClass::Segment && movable(self, *f))
            .collect();
        segments.sort();
        let mut current: Vec<(Nanos, FileId)> = self
            .disk
            .hot
            .iter()
            .copied()
            .filter(|f| self.disk.files[f].class == FileClass::Current && movable(self, *f))
            .filter_map(|f| self.chunk_span(f).map(|(_, _, hi)| (hi, f)))
            .collect();
        current.sort();
        for f in segments
            .into_iter()
            .chain(current.into_iter().map(|(_, f)| f))
        {
            if self.disk.used_bytes(self.config.entry_bytes) <= limit {
                break;
            }
            self.disk.hot.remove(&f);
            moves.push(f);
        }
        if !moves.is_empty() {
            let moved = moves
                .iter()
                .map(|f| Moved {
                    file: *f,
                    from: Tier::Hot,
                    to: if self.disk.cold.contains(f) {
                        Tier::Cold
                    } else {
                        Tier::Deep
                    },
                })
                .collect();
            self.disk
                .publish(self.wall, &[], Vec::new(), Vec::new(), moved);
        }
        removed.extend(moves);
        Ok(removed)
    }

    /// Fault injection: a hot copy no longer matches its hash.
    pub fn corrupt_hot(&mut self, f: FileId) {
        self.disk.corrupt.insert(f);
    }

    /// Checks hot copies of sealed files against the manifest hash; mismatching
    /// files are quarantined, never repaired in place (§9 row 11).
    pub fn scrub(&mut self) -> Vec<FileId> {
        let bad: Vec<FileId> = self
            .disk
            .hot
            .iter()
            .filter(|f| self.disk.corrupt.contains(f) && !self.disk.quarantined.contains(f))
            .copied()
            .collect();
        for f in &bad {
            self.disk.quarantined.insert(*f);
        }
        bad
    }

    /// Replaces a quarantined hot copy with a verified copy from another tier.
    /// Returns whether a copy was found.
    pub fn restore_file(&mut self, f: FileId) -> bool {
        if self.disk.quarantined.contains(&f) && self.disk.verified_elsewhere(f) {
            self.disk.corrupt.remove(&f);
            self.disk.quarantined.remove(&f);
            true
        } else {
            false
        }
    }

    /// Takes a tree checkpoint at the last durable position: the stored
    /// nodes, the registry and each series' latest recorded `t` (§5.1, §8
    /// Continuous backup). It is uploaded by [`Model::upload_checkpoints`].
    /// Refused while a restore is bringing detail back.
    pub fn checkpoint(&mut self) -> Result<Position, Error> {
        if self.restoring() {
            return Err(Error::Restoring);
        }
        let v = self.vol()?;
        let position = v
            .entries
            .keys()
            .next_back()
            .copied()
            .unwrap_or(Position::ORIGIN);
        let cp = Checkpoint {
            position,
            registry: v.registry.clone(),
            latest_t: v.newest_t.clone(),
            versions: v.versions.clone(),
            revised: v
                .history
                .iter()
                .filter(|(_, ks)| ks.len() > 1)
                .map(|(key, ks)| {
                    let entries = ks
                        .iter()
                        .map(|k| IndexEntry {
                            position: k.position,
                            value: k.value.clone(),
                            quality: k.quality,
                            changed_at: k.changed_at,
                        })
                        .collect();
                    (*key, entries)
                })
                .collect(),
            trees: v.trees.iter().map(|(s, t)| (*s, t.nodes_only())).collect(),
        };
        self.disk.checkpoints.push(cp);
        Ok(position)
    }

    /// Uploads queued files, then every local tree checkpoint whose position
    /// the backed-up log reaches: each position up to it is held by a log
    /// segment in the bucket (§8 Continuous backup). Returns the checkpoints
    /// uploaded.
    pub fn upload_checkpoints(&mut self) -> usize {
        // Uploading also notices a higher generation claimed (§8 Generations).
        self.upload();
        if !self.disk.bucket.reachable || self.disk.drill || self.disk.superseded {
            return 0;
        }
        let backed: BTreeSet<Position> = self
            .disk
            .bucket
            .objects
            .values()
            .filter(|(_, f)| f.class == FileClass::Segment)
            .flat_map(|(_, f)| f.entries().map(|e| e.position))
            .collect();
        let live: Vec<Position> = self.disk.live_entries().into_keys().collect();
        let mut n = 0;
        for cp in &self.disk.checkpoints {
            if self.disk.bucket.checkpoints.contains_key(&cp.position) {
                continue;
            }
            if live
                .iter()
                .take_while(|p| **p <= cp.position)
                .all(|p| backed.contains(p))
            {
                self.disk.bucket.checkpoints.insert(cp.position, cp.clone());
                n += 1;
            }
        }
        n
    }

    /// Whether a restore is still bringing detail back: some file it
    /// restores from is not yet local (§8 Restore step 2).
    pub fn restoring(&self) -> bool {
        self.disk.restore.as_ref().is_some_and(|r| {
            let published = self.disk.published();
            !r.sources.iter().all(|f| published.contains(f))
        })
    }

    /// Whether the store still carries the label `derived-unverified`: it was
    /// restored from a checkpoint and some span the checkpoint covers has not
    /// yet been recomputed from facts and compared (§8 Restore step 4).
    pub fn checkpoint_unverified(&self) -> bool {
        self.disk.restore.as_ref().is_some_and(|r| {
            r.checkpoint.is_some() && !self.verify_candidates(r).is_subset(&r.verified)
        })
    }

    /// A new engine restored from the bucket alone (§8), with consumers that
    /// kept no cursor. See [`Model::restore_with_cursors`].
    pub fn restore_from(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        wall: Nanos,
        in_progress: bool,
    ) -> Result<Model, Error> {
        Model::restore_with_cursors(
            config,
            consumers,
            bucket,
            &BTreeMap::new(),
            wall,
            in_progress,
            false,
        )
    }

    /// A new engine restored from the bucket alone (§8).
    ///
    /// The restore claims the generation one above the highest claimed or
    /// present in the bucket (a drill claims nothing and stays read-only),
    /// and its positions start at sequence 1. A bucket with nothing backed up
    /// is refused. The restored end is the last position before the first gap
    /// in the restored log; later positions of older generations are set
    /// aside. The restore writes its record, downloads the latest tree
    /// checkpoint at or below the restored end and the log segments after its
    /// position (with no checkpoint, the whole log), and then accepts ingest.
    /// `cursors` are the positions consumers kept from before the restore: a
    /// consumer without one counts as beyond the restored end, and one at or
    /// below it catches up to the restored end, flagged `replayed`, if the
    /// log segments after it are local; otherwise it only receives `resync`.
    /// Every consumer then receives `generation_changed` and `resync`. With
    /// `in_progress`, the restore returns as soon as it accepts ingest;
    /// [`Model::restore_step`] brings the rest back. Otherwise it runs to the
    /// end.
    pub fn restore_with_cursors(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        cursors: &BTreeMap<ConsumerId, Position>,
        wall: Nanos,
        in_progress: bool,
        drill: bool,
    ) -> Result<Model, Error> {
        let mut m = Model::restore_impl(config, consumers, bucket, cursors, wall, drill, Stop::No)?;
        if !in_progress {
            m.restore_rest()?;
        }
        Ok(m)
    }

    /// A restore that claims its generation and stops (the machine crashes)
    /// before it writes its record. [`Model::restart`] then starts the
    /// restore again with the next generation (§8 Restore).
    pub fn restore_interrupted_before_record(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        cursors: &BTreeMap<ConsumerId, Position>,
        wall: Nanos,
    ) -> Result<Model, Error> {
        Model::restore_impl(
            config,
            consumers,
            bucket,
            cursors,
            wall,
            false,
            Stop::BeforeRecord,
        )
    }

    /// A restore that writes its record, downloads the first `downloads`
    /// files of step 1 and stops (the machine crashes) before step 1 is
    /// complete. [`Model::restart`] then finishes step 1 before it accepts
    /// ingest (§8 Restore).
    pub fn restore_interrupted_after_record(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        cursors: &BTreeMap<ConsumerId, Position>,
        wall: Nanos,
        downloads: usize,
    ) -> Result<Model, Error> {
        Model::restore_impl(
            config,
            consumers,
            bucket,
            cursors,
            wall,
            false,
            Stop::AfterRecord(downloads),
        )
    }

    fn restore_impl(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        cursors: &BTreeMap<ConsumerId, Position>,
        wall: Nanos,
        drill: bool,
        stop: Stop,
    ) -> Result<Model, Error> {
        let mut restored: Vec<Position> = bucket
            .objects
            .values()
            .flat_map(|(_, f)| f.entries().map(|e| e.position))
            .collect();
        if restored.is_empty() {
            return Err(Error::EmptyBucket);
        }
        restored.sort_unstable();
        restored.dedup();
        let end = restored_end(&restored);
        let highest = restored
            .iter()
            .map(|p| p.generation)
            .chain(bucket.claims.iter().copied())
            .max()
            .unwrap_or(0);
        let generation = if drill {
            // The generation a restore would claim, unclaimed, so the drill
            // sets aside exactly what that restore would.
            highest + 1
        } else {
            // Create-if-absent: if the marker exists, take the next.
            let mut g = highest + 1;
            while !bucket.claims.insert(g) {
                g += 1;
            }
            g
        };
        let mut m = Model::new(config, consumers, wall);
        m.disk.bucket = bucket.clone();
        // New files never take the identity of a bucket file not yet local.
        m.disk.next_file = bucket.objects.keys().next_back().map_or(0, |f| f.0 + 1);
        m.disk.generation = generation;
        m.disk.drill = drill;
        if stop == Stop::BeforeRecord {
            m.disk.pending_restore = Some(PendingRestore {
                cursors: cursors.clone(),
                drill,
            });
            m.crash();
            return Ok(m);
        }
        // The restore record, written before ingest is accepted (step 1).
        let checkpoint = bucket
            .checkpoints
            .range(..=end)
            .next_back()
            .map(|(_, c)| c.clone());
        m.disk.restored_end = Some(end);
        let epoch = m.disk.epoch();
        m.disk.manifest.push(ManifestRecord {
            epoch,
            at: wall,
            op: ManifestOp::Restore {
                generation,
                checkpoint: checkpoint.as_ref().map(|c| c.position),
                restored_end: end,
            },
        });
        let latest_t = checkpoint
            .as_ref()
            .map_or_else(BTreeMap::new, |c| c.latest_t.clone());
        m.disk.restore = Some(RestoreState {
            generation,
            restored_end: end,
            checkpoint,
            sources: bucket
                .objects
                .keys()
                .filter(|f| !bucket.retired.contains(f))
                .copied()
                .collect(),
            latest_t,
            verified: BTreeSet::new(),
            differences: Vec::new(),
            kept_cursors: cursors.clone(),
            step_one_done: false,
        });
        if let Stop::AfterRecord(n) = stop {
            for f in m.step_one_files().into_iter().take(n) {
                m.download(f)?;
            }
            m.crash();
            return Ok(m);
        }
        m.finish_step_one()?;
        m.restart();
        Ok(m)
    }

    /// Completes step 1, also after a crash part-way through it: downloads
    /// the step-1 files not yet local, takes the latest `t` per series from
    /// the checkpoint and the restored log, and settles each consumer's
    /// cursor (step 2): a kept cursor continues only if the log segments after
    /// it, up to the restored end, are local.
    fn finish_step_one(&mut self) -> Result<(), Error> {
        let published = self.disk.published();
        for f in self.step_one_files() {
            if !published.contains(&f) {
                self.download(f)?;
            }
        }
        let local = self.disk.live_entries();
        let r = self.disk.restore.as_mut().expect("restore state");
        let end = r.restored_end;
        r.latest_t = r
            .checkpoint
            .as_ref()
            .map_or_else(BTreeMap::new, |c| c.latest_t.clone());
        for f in local.values().filter_map(Entry::fact) {
            if f.position <= end {
                let n = r.latest_t.entry(f.series).or_insert(f.t);
                *n = (*n).max(f.t);
            }
        }
        let r = self.disk.restore.clone().expect("restore state");
        let listed: BTreeSet<Position> = self.listed_entries(&r).into_keys().collect();
        let segments = self.local_segment_positions();
        let ids: Vec<ConsumerId> = self.consumers.iter().map(|c| c.id).collect();
        for id in ids {
            let cursor = r.kept_cursors.get(&id).copied();
            let continues = cursor.filter(|c| {
                *c <= end
                    && listed
                        .iter()
                        .filter(|p| *p > c && **p <= end)
                        .all(|p| segments.contains(p))
            });
            self.disk.cursors.insert(id, continues.unwrap_or(end));
            self.disk.generation_changed.insert(id, (cursor, end));
        }
        self.disk
            .restore
            .as_mut()
            .expect("restore state")
            .step_one_done = true;
        Ok(())
    }

    /// Starts a restore that stopped before its record again, with the next
    /// generation.
    /// If that fails, the engine stays down, the pending restore is kept for
    /// the next restart, and health reports why.
    fn restart_restore(&mut self, pending: PendingRestore) {
        let mut bucket = self.disk.bucket.clone();
        match Model::restore_impl(
            self.config.clone(),
            self.consumers.clone(),
            &mut bucket,
            &pending.cursors,
            self.wall,
            pending.drill,
            Stop::No,
        ) {
            Ok(fresh) => {
                self.disk = fresh.disk;
                self.vol = fresh.vol;
                self.signals = fresh.signals;
                self.received = fresh.received;
                self.confirmed = fresh.confirmed;
                self.sessions = fresh.sessions;
            }
            Err(e) => {
                // A claim is a marker in the bucket (§8 Generations): it
                // stays even when the restore that made it fails.
                self.disk.bucket.claims = bucket.claims;
                self.disk.restore_failed = Some(e);
            }
        }
    }

    /// Downloads the next file the restore still needs, in the order of
    /// §8 Restore step 3, verifies the spans whose detail is now all local
    /// (step 4) and returns the file; `None` once nothing is missing.
    pub fn restore_step(&mut self) -> Result<Option<FileId>, Error> {
        self.vol()?;
        let Some(next) = self.restore_queue().into_iter().next() else {
            return Ok(None);
        };
        self.download(next)?;
        self.verify_restored();
        self.rederive();
        Ok(Some(next))
    }

    /// Runs [`Model::restore_step`] until nothing is missing.
    pub fn restore_rest(&mut self) -> Result<(), Error> {
        while self.restore_step()?.is_some() {}
        Ok(())
    }

    /// Files the restore still needs after step 1, in download order.
    pub fn restore_queue(&self) -> Vec<FileId> {
        let Some(r) = &self.disk.restore else {
            return Vec::new();
        };
        let published = self.disk.published();
        let mut q: Vec<FileId> = r
            .sources
            .iter()
            .filter(|f| !published.contains(f))
            .copied()
            .collect();
        q.sort_by_key(|f| (download_rank(self.disk.bucket.objects[f].1.class), *f));
        q
    }

    /// Step 1: the log segments holding a position after the checkpoint's
    /// (every segment with no checkpoint). Where no segment holds a position
    /// up to the restored end (a segment missing from the bucket), the
    /// first file in download order that holds it.
    fn step_one_files(&self) -> Vec<FileId> {
        let r = self.disk.restore.as_ref().expect("restore state");
        let since = r
            .checkpoint
            .as_ref()
            .map_or(Position::ORIGIN, |c| c.position);
        let objects = &self.disk.bucket.objects;
        let mut chosen: Vec<FileId> = r
            .sources
            .iter()
            .filter(|f| {
                let file = &objects[f].1;
                file.class == FileClass::Segment && file.entries().any(|e| e.position > since)
            })
            .copied()
            .collect();
        let mut needed: BTreeSet<Position> = self
            .listed_entries(r)
            .into_keys()
            .filter(|p| *p > since)
            .collect();
        for f in &chosen {
            for e in objects[f].1.entries() {
                needed.remove(&e.position);
            }
        }
        let mut rest: Vec<FileId> = r
            .sources
            .iter()
            .filter(|f| !chosen.contains(f))
            .copied()
            .collect();
        rest.sort_by_key(|f| (download_rank(objects[f].1.class), *f));
        for f in rest {
            if needed.is_empty() {
                break;
            }
            let file = &objects[&f].1;
            if file.entries().any(|e| needed.contains(&e.position)) {
                for e in file.entries() {
                    needed.remove(&e.position);
                }
                chosen.push(f);
            }
        }
        chosen
    }

    /// Installs a bucket file on the fast disk. A position it shares with a
    /// file already local must have identical content (§8 Restore step 3).
    fn download(&mut self, id: FileId) -> Result<(), Error> {
        let (hash, content) = self.disk.bucket.objects[&id].clone();
        let local = self.disk.all_entries();
        for e in content.entries() {
            if local.get(&e.position).is_some_and(|l| l != e) {
                return Err(Error::Conflict {
                    file: id,
                    position: e.position,
                });
            }
        }
        self.disk.next_file = self.disk.next_file.max(id.0 + 1);
        self.disk.files.insert(id, content);
        self.disk.hot.insert(id);
        debug_assert_eq!(hash, self.disk.files[&id].hash());
        self.disk
            .publish(self.wall, &[id], Vec::new(), Vec::new(), Vec::new());
        Ok(())
    }

    /// Entries of the files the restore restores from, up to the restored end.
    fn listed_entries(&self, r: &RestoreState) -> BTreeMap<Position, Entry> {
        let mut out = BTreeMap::new();
        for f in &r.sources {
            for e in self.disk.bucket.objects[f].1.entries() {
                if e.position <= r.restored_end {
                    out.entry(e.position).or_insert_with(|| e.clone());
                }
            }
        }
        out
    }

    /// Facts of the files the restore restores from, up to the restored end.
    fn listed_facts(&self, r: &RestoreState) -> Vec<Fact> {
        self.listed_entries(r)
            .into_values()
            .filter_map(|e| e.fact().cloned())
            .collect()
    }

    /// Keys with more than one restored fact: those the revised-key index
    /// holds.
    fn revised_keys(&self, r: &RestoreState) -> BTreeSet<(SeriesId, Nanos)> {
        let mut seen = BTreeSet::new();
        let mut revised = BTreeSet::new();
        for f in self.listed_facts(r) {
            if !seen.insert((f.series, f.t)) {
                revised.insert((f.series, f.t));
            }
        }
        revised
    }

    /// Whether every current chunk the restore restores from is local. The
    /// revised-key index counts as back with them (see `OPEN-QUESTIONS.md`
    /// Q-39).
    fn index_back(&self, r: &RestoreState) -> bool {
        let published = self.disk.published();
        r.sources.iter().all(|f| {
            self.disk.bucket.objects[f].1.class != FileClass::Current || published.contains(f)
        })
    }

    /// Positions held by local log segments.
    fn local_segment_positions(&self) -> BTreeSet<Position> {
        self.disk
            .published()
            .into_iter()
            .filter(|f| {
                self.disk.files[f].class == FileClass::Segment
                    && self.disk.locality(*f) == Locality::Hot
            })
            .flat_map(|f| self.disk.files[&f].entries().map(|e| e.position))
            .collect()
    }

    /// Spans the checkpoint covers: those where it holds a node, and those
    /// holding a restored fact at or before its position.
    fn verify_candidates(&self, r: &RestoreState) -> BTreeSet<(SeriesId, Nanos)> {
        let Some(cp) = &r.checkpoint else {
            return BTreeSet::new();
        };
        let mut out: BTreeSet<(SeriesId, Nanos)> = cp
            .trees
            .iter()
            .flat_map(|(s, t)| t.stored(0).map(move |n| (*s, floor_to(n.start, QUARTER))))
            .collect();
        out.extend(
            self.listed_facts(r)
                .iter()
                .filter(|f| f.position <= cp.position)
                .map(|f| (f.series, floor_to(f.t, QUARTER))),
        );
        out
    }

    /// Step 4: for each span the checkpoint covers whose detail is now all
    /// local, recomputes its nodes from the facts as of the checkpoint's
    /// position and compares them with the checkpoint's; a difference is
    /// recorded and counted. The live tree is recomputed from the facts
    /// whatever the outcome (`restoring_trees`), so the recomputed value wins.
    fn verify_restored(&mut self) {
        let Some(r) = self.disk.restore.clone() else {
            return;
        };
        let Some(cp) = &r.checkpoint else {
            return;
        };
        let local = self.disk.live_entries();
        let listed = self.listed_facts(&r);
        let not_back: BTreeSet<(SeriesId, Nanos)> = listed
            .iter()
            .filter(|f| !local.contains_key(&f.position))
            .map(|f| (f.series, floor_to(f.t, QUARTER)))
            .collect();
        let before: Vec<Fact> = local
            .values()
            .filter_map(Entry::fact)
            .filter(|f| f.position <= cp.position)
            .cloned()
            .collect();
        let mut verified = r.verified.clone();
        let mut differences = r.differences.clone();
        for (s, q) in self.verify_candidates(&r) {
            if verified.contains(&(s, q)) || not_back.contains(&(s, q)) {
                continue;
            }
            verified.insert((s, q));
            let params = cp
                .registry
                .get(s)
                .map(params_of)
                .or_else(|| cp.trees.get(&s).map(|t| t.params));
            let same = params.is_some_and(|params| {
                let span = before
                    .iter()
                    .filter(|f| f.series == s && floor_to(f.t, QUARTER) == q);
                let rs: Vec<(Nanos, Value)> = view::as_of_position(span, cp.position)
                    .into_values()
                    .filter(Fact::summarised)
                    .map(|f| (f.t, f.value))
                    .collect();
                let check = SeriesTree::build(params, &rs);
                let held = cp
                    .trees
                    .get(&s)
                    .cloned()
                    .unwrap_or_else(|| SeriesTree::new(params));
                // The stored levels only (§5.1 density rule).
                (!params.stores(1) || held.node(1, q) == check.node(1, q))
                    && (!params.stores(0)
                        || (0..QUARTER / MINUTE)
                            .all(|i| held.node(0, q + i * MINUTE) == check.node(0, q + i * MINUTE)))
            });
            if !same {
                differences.push((s, q));
                self.counters.checkpoint_differences += 1;
            }
        }
        let state = self.disk.restore.as_mut().expect("restore state");
        state.verified = verified;
        state.differences = differences;
    }
}

/// The summarised current readings of one minute of a series.
fn minute_readings(
    current: &BTreeMap<(SeriesId, Nanos), Fact>,
    s: SeriesId,
    minute: Nanos,
) -> Vec<(Nanos, Value)> {
    current
        .range((s, minute)..(s, minute + MINUTE))
        .map(|(_, f)| f)
        .filter(|f| f.summarised())
        .map(|f| (f.t, f.value.clone()))
        .collect()
}

/// Where a restore stops (the machine crashes), for crash tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// It does not stop.
    No,
    /// After claiming its generation, before its record.
    BeforeRecord,
    /// After its record and the first `n` downloads of step 1.
    AfterRecord(usize),
}

/// Order of downloads after step 1: current chunks first, then revision
/// chunks, then log segments (§8 Restore step 3).
fn download_rank(class: FileClass) -> u8 {
    match class {
        FileClass::Current => 0,
        FileClass::Revision => 1,
        FileClass::Segment => 2,
    }
}

fn params_of(d: &SeriesDef) -> Params {
    Params {
        counter: d.options.counter,
        expected_interval: d.expected_interval(),
        timing: d.options.timing,
        interval: d.options.interval,
        ..Params::new(d.kind, d.staleness)
    }
}

/// The funnels of a consumer in force at `before` (routing changes at
/// earlier positions applied), or now with `None` (§4 Routing).
fn routes_in_force(
    decl: &ConsumerDecl,
    routes: &BTreeMap<Position, RouteChange>,
    before: Option<Position>,
) -> BTreeMap<FunnelId, Funnel> {
    let mut out = BTreeMap::from([(FunnelId::CONFIGURED, Funnel::facts(decl.selection.clone()))]);
    for (p, change) in routes {
        if before.is_some_and(|b| *p >= b) {
            break;
        }
        match change {
            RouteChange::Add {
                consumer,
                funnel,
                spec,
            } if *consumer == decl.id => {
                out.insert(*funnel, spec.clone());
            }
            RouteChange::Remove { consumer, funnel } if *consumer == decl.id => {
                out.remove(funnel);
            }
            _ => {}
        }
    }
    out
}

/// The incarnation of a consumer's funnel in force now: the position of the
/// routing entry that added it, or [`Position::ORIGIN`] for the configured
/// funnel; `None` if it is not in force.
fn incarnation(
    decl: &ConsumerDecl,
    routes: &BTreeMap<Position, RouteChange>,
    funnel: FunnelId,
) -> Option<Position> {
    let mut inc = (funnel == FunnelId::CONFIGURED).then_some(Position::ORIGIN);
    for (p, change) in routes {
        match change {
            RouteChange::Add {
                consumer,
                funnel: f,
                ..
            } if *consumer == decl.id && *f == funnel => inc = Some(*p),
            RouteChange::Remove {
                consumer,
                funnel: f,
            } if *consumer == decl.id && *f == funnel => inc = None,
            _ => {}
        }
    }
    inc
}

/// Whether a fact is routed to a consumer by a funnel without a condition in
/// force at its position.
fn routed(decl: &ConsumerDecl, routes: &BTreeMap<Position, RouteChange>, f: &Fact) -> bool {
    routes_in_force(decl, routes, Some(f.position))
        .values()
        .any(|x| x.condition.is_none() && x.selection.matches(f.series))
}

fn build_trees(
    registry: &Registry,
    current: &BTreeMap<(SeriesId, Nanos), Fact>,
) -> BTreeMap<SeriesId, SeriesTree> {
    registry
        .all()
        .map(|d| {
            let rs: Vec<(Nanos, Value)> = view::summarised_series(current, d.id)
                .into_iter()
                .map(|f| (f.t, f.value))
                .collect();
            (d.id, SeriesTree::build(params_of(d), &rs))
        })
        .collect()
}

/// The last position before the first gap in sorted, distinct positions: the
/// log starts at `0:1`, as every store begins in generation 0, a gap is a
/// missing sequence within a generation, and a later generation starts at
/// sequence 1 after at least one position (§8 Restore step 1).
/// [`Position::ORIGIN`] when `0:1` is missing or nothing was restored.
fn restored_end(sorted: &[Position]) -> Position {
    let mut end = Position::ORIGIN;
    for p in sorted {
        let follows = *p == end.next()
            || (end.sequence > 0 && p.generation > end.generation && p.sequence == 1);
        if !follows {
            break;
        }
        end = *p;
    }
    end
}
