//! The engine model: ingest, the log and commit (§3), delivery (§4), live
//! summaries (§5), compaction and files (§7), tiers, backup and restore (§8).
//!
//! The model is a deterministic state machine. What is on disk (or in another
//! tier) lives in [`Disk`] and survives a crash; everything else lives in a
//! volatile part that a crash discards and [`Model::restart`] rebuilds from the
//! disk alone. Crashes and faults are injected at named points.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::registry::{Registry, RegistryError, RegistryEvent, SeriesDef};
use crate::summary::{Node, Params};
use crate::tree::{SeriesTree, TreeError};
use crate::types::{
    Fact, Flags, HOUR, Kind, MAX_TEXT_BYTES, MINUTE, Nanos, Position, QUARTER, Quality, Reading,
    SECOND, SeriesId, Value, ValueType, floor_to,
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
    /// Grace period before a retired file may leave the fast disk (§7: 1 hour).
    pub grace: Nanos,
    /// Age after which revision chunks and sealed segments leave the fast disk
    /// (§8: 90 days).
    pub eviction_age: Nanos,
    /// Fast-disk capacity in bytes.
    pub disk_bytes: u64,
    /// Bytes one log entry costs on disk (model accounting).
    pub entry_bytes: u64,
    /// Low mark: compaction pauses, health degraded (§3). The model also
    /// pauses tiering (see `OPEN-QUESTIONS.md`).
    pub low_mark: u64,
    /// Critical mark: ingest waits (§3).
    pub critical_mark: u64,
    /// Fill ratio (percent) above which the oldest current detail moves out (§8: 70 %).
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
            eviction_age: 90 * 24 * HOUR,
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

/// Body of a log entry.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryBody {
    /// A reading fact.
    Fact(Fact),
    /// A registry event.
    Registry(RegistryEvent),
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
            EntryBody::Registry(_) => None,
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
    /// Wall time the file was sealed.
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

/// Manifest operation.
#[derive(Clone, Debug, PartialEq)]
pub enum ManifestOp {
    /// A file is published; once published, `replaces` are to be retired.
    Add {
        /// File.
        file: FileId,
        /// Inputs this file replaces.
        replaces: Vec<FileId>,
        /// Hash of the file as stored.
        hash: u64,
    },
    /// A file is retired.
    Retire {
        /// File.
        file: FileId,
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
    /// Stored nodes per series, without readings.
    pub trees: BTreeMap<SeriesId, SeriesTree>,
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
    /// Uploads the bucket rejected for a hash mismatch.
    pub uploads_rejected: u64,
    /// Compaction or tiering runs skipped for low disk.
    pub paused: u64,
    /// Restored spans whose recomputed nodes differed from the checkpoint
    /// (§8 Restore step 4).
    pub checkpoint_differences: u64,
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
    /// A `resync` is due (§4). Its contents (live state and condition state)
    /// are not modelled yet.
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

    fn add(&mut self, at: Nanos, file: FileId, replaces: Vec<FileId>) {
        let hash = self.files[&file].hash();
        let epoch = self.epoch();
        self.manifest.push(ManifestRecord {
            epoch,
            at,
            op: ManifestOp::Add {
                file,
                replaces,
                hash,
            },
        });
    }

    fn retire(&mut self, at: Nanos, file: FileId) {
        let epoch = self.epoch();
        self.manifest.push(ManifestRecord {
            epoch,
            at,
            op: ManifestOp::Retire { file },
        });
    }

    /// Files ever published.
    pub fn published(&self) -> BTreeSet<FileId> {
        self.manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Add { file, .. } => Some(file),
                ManifestOp::Retire { .. }
                | ManifestOp::Seal { .. }
                | ManifestOp::Restore { .. } => None,
            })
            .collect()
    }

    /// Retired files with the wall time of retirement.
    pub fn retired(&self) -> BTreeMap<FileId, Nanos> {
        self.manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Retire { file } => Some((file, r.at)),
                ManifestOp::Add { .. } | ManifestOp::Seal { .. } | ManifestOp::Restore { .. } => {
                    None
                }
            })
            .collect()
    }

    /// Files listed (published and not retired) at `epoch`.
    pub fn listed_at(&self, epoch: u64) -> Vec<FileId> {
        let mut set = BTreeSet::new();
        for r in self.manifest.iter().take_while(|r| r.epoch <= epoch) {
            match r.op {
                ManifestOp::Add { file, .. } => {
                    set.insert(file);
                }
                ManifestOp::Retire { file } => {
                    set.remove(&file);
                }
                ManifestOp::Seal { .. } | ManifestOp::Restore { .. } => {}
            }
        }
        set.into_iter().collect()
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
        self.manifest.iter().rev().find_map(|r| match r.op {
            ManifestOp::Add { file, hash, .. } | ManifestOp::Seal { file, hash } if file == f => {
                Some(hash)
            }
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

    /// Positions of facts that are readable from the fast disk.
    pub fn local_positions(&self) -> BTreeSet<Position> {
        let mut out = BTreeSet::new();
        for f in self.published() {
            if self.locality(f) == Locality::Hot {
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
    queue: VecDeque<Delivery>,
    mode: Mode,
    session: u32,
    last_received: Position,
}

/// State a crash discards.
#[derive(Clone, Debug)]
struct Volatile {
    registry: Registry,
    entries: BTreeMap<Position, Entry>,
    current: BTreeMap<(SeriesId, Nanos), Fact>,
    newest_t: BTreeMap<SeriesId, Nanos>,
    /// Every recorded `(value, quality)` per key, for the duplicate rule.
    history: BTreeMap<(SeriesId, Nanos), Vec<(Value, Quality)>>,
    trees: BTreeMap<SeriesId, SeriesTree>,
    /// Queued readings with their `revise` intent.
    queue: VecDeque<(Reading, bool)>,
    next_position: Position,
    consumers: Vec<ConsumerRt>,
    /// Read-only for any cause: a failed write or sync, a drill, or a higher
    /// generation claimed.
    read_only: bool,
    /// A write or sync failed (§3), as opposed to read-only by design.
    sync_failed: bool,
    skew_run: BTreeMap<SeriesId, u32>,
    /// While a restore is bringing detail back (§8 Restore steps 2–4).
    restore: Option<RestoreRt>,
}

/// Volatile state derived from the disk alone.
struct Derived {
    registry: Registry,
    entries: BTreeMap<Position, Entry>,
    current: BTreeMap<(SeriesId, Nanos), Fact>,
    newest_t: BTreeMap<SeriesId, Nanos>,
    history: BTreeMap<(SeriesId, Nanos), Vec<(Value, Quality)>>,
    trees: BTreeMap<SeriesId, SeriesTree>,
    restore: Option<RestoreRt>,
}

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
    /// Events other than fact deliveries each consumer has received.
    pub signals: BTreeMap<ConsumerId, Vec<Signal>>,
    /// Host wall clock (may jump).
    pub wall: Nanos,
    /// Monotonic clock (only advances).
    pub mono: Nanos,
    /// Engine UTC clock for window ends: never runs backwards (§6).
    pub now: Nanos,
    /// Boot id of the machine; a power loss starts a new boot.
    pub boot: u64,
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
            wall,
            mono: 0,
            now: wall,
            boot: 0,
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
        let consumers = self
            .consumers
            .iter()
            .map(|c| {
                let s = self.sessions.entry(c.id).or_insert(0);
                *s += 1;
                let cursor = self
                    .disk
                    .cursors
                    .get(&c.id)
                    .copied()
                    .unwrap_or(Position::ORIGIN);
                ConsumerRt {
                    decl: c.clone(),
                    queue: VecDeque::new(),
                    // A consumer that is behind catches up from its cursor.
                    mode: if d.entries.range(cursor.next()..).next().is_some() {
                        Mode::CatchUp {
                            next: cursor.next(),
                        }
                    } else {
                        Mode::Live
                    },
                    session: *s,
                    last_received: cursor,
                }
            })
            .collect();
        self.vol = Some(Volatile {
            registry: d.registry,
            entries: d.entries,
            current: d.current,
            newest_t: d.newest_t,
            history: d.history,
            trees: d.trees,
            queue: VecDeque::new(),
            next_position,
            consumers,
            read_only: sync_failed || self.disk.drill || self.disk.superseded,
            sync_failed,
            skew_run: BTreeMap::new(),
            restore: d.restore,
        });
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
        let mut facts = Vec::new();
        for (p, e) in &entries {
            match &e.body {
                EntryBody::Registry(r) if *p > since => registry.apply(r),
                EntryBody::Registry(_) => {}
                EntryBody::Fact(f) => facts.push(f.clone()),
            }
        }
        let current = view::current(facts.iter());
        let mut newest_t = restoring.map_or_else(BTreeMap::new, |r| r.latest_t.clone());
        let mut history: BTreeMap<(SeriesId, Nanos), Vec<(Value, Quality)>> = BTreeMap::new();
        for f in &facts {
            history
                .entry((f.series, f.t))
                .or_default()
                .push((f.value.clone(), f.quality));
            let n = newest_t.entry(f.series).or_insert(f.t);
            *n = (*n).max(f.t);
        }
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
                            history
                                .entry((f.series, f.t))
                                .or_default()
                                .push((f.value.clone(), f.quality));
                        }
                    }
                }
                (trees, Some(rt))
            }
        };
        Derived {
            registry,
            entries,
            current,
            newest_t,
            history,
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
            v.current = d.current;
            v.newest_t = d.newest_t;
            v.history = d.history;
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
            let held: BTreeSet<Nanos> = tree.stored(0).map(|n| n.start).collect();
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

    /// Discards unpublished compaction outputs and resumes retirements of
    /// files whose replacements are published (§3 Recovery).
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
        let retired = self.disk.retired();
        let pending: Vec<FileId> = self
            .disk
            .manifest
            .iter()
            .flat_map(|r| match &r.op {
                ManifestOp::Add { replaces, .. } => replaces.clone(),
                ManifestOp::Retire { .. }
                | ManifestOp::Seal { .. }
                | ManifestOp::Restore { .. } => Vec::new(),
            })
            .filter(|f| !retired.contains_key(f))
            .collect();
        for f in pending {
            let at = self.wall;
            self.disk.retire(at, f);
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
        let staleness = staleness.into();
        let ceiling = self.config.series_ceiling;
        let plan = self
            .vol()?
            .registry
            .plan_define(name, kind, value_type, unit, staleness, ceiling)?;
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
        let res = self.check(&r);
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

    fn check(&self, r: &Reading) -> Result<(), EmitError> {
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
        let non_finite = matches!(r.value, Value::Num(x) if !x.is_finite());
        if !non_finite && !r.value.fits(d.value_type) {
            return Err(Refusal::WrongType.into());
        }
        if let Value::Str(s) = &r.value
            && s.len() > MAX_TEXT_BYTES
        {
            return Err(Refusal::TextTooLong.into());
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
        let v = self.vol.as_mut().ok_or(Error::Crashed)?;
        if v.read_only {
            return Err(Error::ReadOnly);
        }
        // Form the batch: stamp and flag; drop a reading identical to any fact
        // in the history of its key, including earlier facts of this batch,
        // unless it carries `revise` intent (§2.4).
        let mut batch: Vec<Entry> = Vec::new();
        let mut batch_history: BTreeMap<(SeriesId, Nanos), Vec<(Value, Quality)>> = BTreeMap::new();
        let mut batch_newest: BTreeMap<SeriesId, Nanos> = BTreeMap::new();
        let mut duplicates = 0;
        let mut unchecked = 0;
        let mut non_finite = 0;
        let mut next = v.next_position;
        let readings: Vec<(Reading, bool)> = v.queue.drain(..).collect();
        for (r, revise) in readings {
            let (value, mut quality) = match r.value {
                Value::Num(x) if !x.is_finite() => {
                    non_finite += 1;
                    (Value::Absent, Quality::Bad)
                }
                other => (other, r.quality),
            };
            let future = r.t > wall.saturating_add(cfg.future_tolerance);
            if future {
                quality = quality.worst(Quality::Suspect);
            }
            let key = (r.series, r.t);
            let seen = v
                .history
                .get(&key)
                .into_iter()
                .chain(batch_history.get(&key))
                .flatten()
                .any(|(hv, hq)| hv.identical(&value) && *hq == quality);
            if seen && !revise {
                duplicates += 1;
                continue;
            }
            // During a restore the check cannot run against facts that are
            // not yet local (§8 Restore step 2, §2.4).
            if !revise
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
            let newest = batch_newest
                .get(&r.series)
                .or_else(|| v.newest_t.get(&r.series))
                .copied();
            let s = v.registry.get(r.series).and_then(|d| d.staleness);
            let out_of_order = match (newest, s) {
                (Some(n), Some(s)) => r.t < n.saturating_sub(2 * s),
                _ => false,
            };
            let late = out_of_order || wall - r.t > cfg.lateness_bound;
            let fact = Fact {
                series: r.series,
                t: r.t,
                position: next,
                received_at: wall,
                value,
                quality,
                flags: Flags {
                    future,
                    late,
                    revise,
                },
            };
            next = next.next();
            let n = batch_newest.entry(r.series).or_insert(r.t);
            *n = (*n).max(r.t);
            batch_history
                .entry(key)
                .or_default()
                .push((fact.value.clone(), fact.quality));
            batch.push(Entry {
                position: fact.position,
                received_at: wall,
                body: EntryBody::Fact(fact),
            });
        }
        v.next_position = next;
        self.counters.duplicates_dropped += duplicates as u64;
        self.counters.non_finite += non_finite;
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
        self.publish_live(&batch)?;
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
            self.deliver_live(i, &batch);
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
            self.disk.add(self.wall, id, Vec::new());
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
            let f = self.disk.files.get_mut(&id).expect("segment");
            f.sealed = true;
            f.sealed_at = Some(wall);
            let hash = self.disk.files[&id].hash();
            let epoch = self.disk.epoch();
            self.disk.manifest.push(ManifestRecord {
                epoch,
                at: wall,
                op: ManifestOp::Seal { file: id, hash },
            });
        }
    }

    fn publish_live(&mut self, batch: &[Entry]) -> Result<(), Error> {
        let cfg = self.config.clone();
        let restoring = self.vol.as_ref().is_some_and(|v| v.restore.is_some());
        let local = if !restoring && self.any_current_detail_evicted() {
            Some(self.disk.local_positions())
        } else {
            None
        };
        let v = self.vol.as_mut().ok_or(Error::Crashed)?;
        let mut touched: BTreeSet<(SeriesId, Nanos)> = BTreeSet::new();
        for e in batch {
            v.entries.insert(e.position, e.clone());
            let Some(f) = e.fact() else { continue };
            if let Some(local) = &local
                && let Some(prev) = v.current.get(&(f.series, f.t))
                && !local.contains(&prev.position)
            {
                self.counters.duplicates_unchecked += 1;
            }
            v.current.insert((f.series, f.t), f.clone());
            v.history
                .entry((f.series, f.t))
                .or_default()
                .push((f.value.clone(), f.quality));
            let n = v.newest_t.entry(f.series).or_insert(f.t);
            *n = (*n).max(f.t);
            touched.insert((f.series, floor_to(f.t, MINUTE)));
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

    fn any_current_detail_evicted(&self) -> bool {
        self.disk
            .published()
            .iter()
            .any(|f| self.disk.files[f].class == FileClass::Current && !self.disk.hot.contains(f))
    }

    // ----- delivery -----

    fn deliver_live(&mut self, i: usize, batch: &[Entry]) {
        let v = self.vol.as_mut().expect("up");
        let c = &mut v.consumers[i];
        for e in batch {
            let Some(f) = e.fact() else { continue };
            if !c.decl.selection.matches(f.series) {
                continue;
            }
            match c.mode {
                Mode::Live => {
                    if c.queue.len() < c.decl.capacity {
                        c.queue.push_back(Delivery {
                            position: e.position,
                            fact: f.clone(),
                            replayed: false,
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

    /// Engine-side catch-up for a consumer: reads up to `max` log entries from
    /// its catch-up position into its queue, flagged `replayed`. Switches back
    /// to live once it reaches the head.
    pub fn catch_up(&mut self, id: ConsumerId, max: usize) -> Result<(), Error> {
        let v = self.vol_mut()?;
        let Some(c) = v.consumers.iter_mut().find(|c| c.decl.id == id) else {
            return Ok(());
        };
        let Mode::CatchUp { mut next } = c.mode else {
            return Ok(());
        };
        // Entries in position order from `next`, across generations.
        let mut at_head = true;
        for (reads, (p, e)) in v.entries.range(next..).enumerate() {
            if reads >= max || c.queue.len() >= c.decl.capacity {
                at_head = false;
                next = *p;
                break;
            }
            if let Some(f) = e.fact()
                && c.decl.selection.matches(f.series)
            {
                c.queue.push_back(Delivery {
                    position: *p,
                    fact: f.clone(),
                    replayed: true,
                });
            }
            next = p.next();
        }
        c.mode = if at_head {
            Mode::Live
        } else {
            Mode::CatchUp { next }
        };
        Ok(())
    }

    /// The consumer takes up to `max` deliveries from its queue.
    pub fn poll(&mut self, id: ConsumerId, max: usize) -> Result<Vec<Delivery>, Error> {
        let v = self.vol_mut()?;
        let Some(c) = v.consumers.iter_mut().find(|c| c.decl.id == id) else {
            return Ok(Vec::new());
        };
        let n = max.min(c.queue.len());
        let out: Vec<Delivery> = c.queue.drain(..n).collect();
        let session = c.session;
        if let Some(d) = out.last() {
            c.last_received = d.position;
        }
        let obs = self.observed.entry(id).or_default();
        for d in &out {
            obs.push(Observed {
                session,
                position: d.position,
                replayed: d.replayed,
            });
        }
        Ok(out)
    }

    /// The consumer confirms it has processed everything up to `p`; the cursor
    /// is durable at once.
    pub fn confirm(&mut self, id: ConsumerId, p: Position) -> Result<(), Error> {
        let v = self.vol()?;
        let c = v
            .consumers
            .iter()
            .find(|c| c.decl.id == id)
            .ok_or(Error::BadConfirm)?;
        if p > c.last_received {
            return Err(Error::BadConfirm);
        }
        let cur = self.disk.cursors.entry(id).or_insert(Position::ORIGIN);
        *cur = (*cur).max(p);
        Ok(())
    }

    /// The consumer process restarted: its queue is dropped and it resumes
    /// from its durable cursor in catch-up mode.
    pub fn reconnect(&mut self, id: ConsumerId) -> Result<(), Error> {
        let cursor = self
            .disk
            .cursors
            .get(&id)
            .copied()
            .unwrap_or(Position::ORIGIN);
        let s = self.sessions.entry(id).or_insert(0);
        *s += 1;
        let session = *s;
        let v = self.vol_mut()?;
        if let Some(c) = v.consumers.iter_mut().find(|c| c.decl.id == id) {
            c.queue.clear();
            c.mode = Mode::CatchUp {
                next: cursor.next(),
            };
            c.session = session;
            c.last_received = cursor;
        }
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

    /// Runs catch-up, polling and confirming for every consumer until all are
    /// live with empty queues.
    pub fn drain(&mut self) -> Result<(), Error> {
        let ids: Vec<ConsumerId> = self.consumers.iter().map(|c| c.id).collect();
        loop {
            let mut progress = false;
            for &id in &ids {
                self.catch_up(id, usize::MAX)?;
                let got = self.poll(id, usize::MAX)?;
                if let Some(d) = got.last() {
                    self.confirm(id, d.position)?;
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
    /// quality not bad.
    pub fn latest(&self, s: SeriesId) -> Result<Option<Fact>, Error> {
        Ok(self
            .vol()?
            .current
            .range((s, Nanos::MIN)..=(s, self.now))
            .rev()
            .map(|(_, f)| f)
            .find(|f| f.summarised())
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
        let until = end.min(self.now).max(start);
        let tree = self.tree(s)?;
        Ok(Answer {
            node: tree.window(start, end, self.now),
            restoring,
            summary_stale: tree.any_stale(start, until),
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

    /// Compacts all sealed, unretired segments into a current chunk and a
    /// revision chunk (§7). Returns the new files.
    pub fn compact(&mut self, crash: Option<CompactionCrash>) -> Result<Vec<FileId>, Error> {
        self.vol()?;
        if self.free_bytes() < self.config.low_mark {
            self.counters.paused += 1;
            return Err(Error::Paused);
        }
        let retired = self.disk.retired();
        let inputs: Vec<FileId> = self
            .disk
            .published()
            .into_iter()
            .filter(|f| {
                let file = &self.disk.files[f];
                file.class == FileClass::Segment && file.sealed && !retired.contains_key(f)
            })
            .collect();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let current = self.current()?.clone();
        let mut cur_entries = Vec::new();
        let mut rev_entries = Vec::new();
        for f in &inputs {
            for e in self.disk.files[f].entries() {
                match e.fact() {
                    Some(fact) if current[&(fact.series, fact.t)].position != fact.position => {
                        rev_entries.push(e.clone());
                    }
                    _ => cur_entries.push(e.clone()),
                }
            }
        }
        let mut outputs = Vec::new();
        for (class, entries) in [
            (FileClass::Current, cur_entries),
            (FileClass::Revision, rev_entries),
        ] {
            if entries.is_empty() {
                continue;
            }
            let id = FileId(self.disk.next_file);
            self.disk.next_file += 1;
            self.disk.files.insert(
                id,
                StoredFile {
                    id,
                    class,
                    version: FORMAT_VERSION,
                    batches: vec![entries],
                    sealed: true,
                    opened_at: self.wall,
                    sealed_at: Some(self.wall),
                },
            );
            self.disk.hot.insert(id);
            outputs.push(id);
        }
        if crash == Some(CompactionCrash::AfterWrite) {
            self.crash();
            return Err(Error::Crashed);
        }
        // Verify: same multiset of entries in and out.
        let mut ins: Vec<&Entry> = inputs
            .iter()
            .flat_map(|f| self.disk.files[f].entries())
            .collect();
        let mut outs: Vec<&Entry> = outputs
            .iter()
            .flat_map(|f| self.disk.files[f].entries())
            .collect();
        ins.sort_by_key(|e| e.position);
        outs.sort_by_key(|e| e.position);
        assert_eq!(ins, outs, "compaction verification");
        // Publish.
        for (i, f) in outputs.iter().enumerate() {
            let replaces = if i + 1 == outputs.len() {
                inputs.clone()
            } else {
                Vec::new()
            };
            self.disk.add(self.wall, *f, replaces);
        }
        if crash == Some(CompactionCrash::AfterPublish) {
            self.crash();
            return Err(Error::Crashed);
        }
        for f in inputs {
            self.disk.retire(self.wall, f);
        }
        Ok(outputs)
    }

    /// Opens a reader at the latest manifest epoch.
    pub fn open_reader(&self) -> Reader {
        let epoch = self.disk.epoch() - 1;
        Reader {
            epoch,
            files: self.disk.listed_at(epoch),
        }
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
        if self.free_bytes() < self.config.low_mark {
            self.counters.paused += 1;
            return Err(Error::Paused);
        }
        let mut removed = Vec::new();
        let retired = self.disk.retired();
        let hot: Vec<FileId> = self.disk.hot.iter().copied().collect();
        for f in hot {
            if !self.disk.verified_elsewhere(f) || self.disk.active == Some(f) {
                continue;
            }
            let file = &self.disk.files[&f];
            let remove = if let Some(at) = retired.get(&f) {
                self.wall - at >= self.config.grace
            } else {
                matches!(file.class, FileClass::Revision | FileClass::Segment)
                    && file
                        .sealed_at
                        .is_some_and(|s| self.wall - s >= self.config.eviction_age)
            };
            if remove {
                self.disk.hot.remove(&f);
                removed.push(f);
            }
        }
        // Fill threshold: the oldest current detail moves out, verified first.
        let limit = self.config.disk_bytes / 100 * self.config.fill_threshold_pct;
        let mut current: Vec<FileId> = self
            .disk
            .hot
            .iter()
            .filter(|f| {
                self.disk.files[f].class == FileClass::Current
                    && !retired.contains_key(f)
                    && self.disk.verified_elsewhere(**f)
            })
            .copied()
            .collect();
        current.sort();
        for f in current {
            if self.disk.used_bytes(self.config.entry_bytes) <= limit {
                break;
            }
            self.disk.hot.remove(&f);
            removed.push(f);
        }
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
            sources: bucket.objects.keys().copied().collect(),
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
            self.signals.entry(id).or_default().extend([
                Signal::GenerationChanged {
                    cursor,
                    restored_end: end,
                },
                Signal::ResyncDue,
            ]);
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
    /// up to the restored end (the model's compaction retires segments), the
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
        let epoch = self.disk.epoch();
        self.disk.manifest.push(ManifestRecord {
            epoch,
            at: self.wall,
            op: ManifestOp::Add {
                file: id,
                replaces: Vec::new(),
                hash,
            },
        });
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
                held.node(1, q) == check.node(1, q)
                    && (0..QUARTER / MINUTE)
                        .all(|i| held.node(0, q + i * MINUTE) == check.node(0, q + i * MINUTE))
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
    Params::new(d.kind, d.staleness)
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
