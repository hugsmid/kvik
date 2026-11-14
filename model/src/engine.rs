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
    Fact, Flags, HOUR, Kind, MAX_TEXT_BYTES, MINUTE, Nanos, Position, Quality, Reading, SECOND,
    SeriesId, Value, ValueType, floor_to,
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
    /// Other bytes in use on the fast disk (fault injection for disk fill).
    pub other_bytes: u64,
    /// Next file id.
    pub next_file: u64,
}

/// A consumer event that is not a fact delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// The store was restored and the consumer's cursor lies beyond what the
    /// restore brought back (§8 Restore step 2).
    GenerationChanged {
        /// The consumer's cursor before the restore.
        cursor: Position,
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
                ManifestOp::Retire { .. } | ManifestOp::Seal { .. } => None,
            })
            .collect()
    }

    /// Retired files with the wall time of retirement.
    pub fn retired(&self) -> BTreeMap<FileId, Nanos> {
        self.manifest
            .iter()
            .filter_map(|r| match r.op {
                ManifestOp::Retire { file } => Some((file, r.at)),
                ManifestOp::Add { .. } | ManifestOp::Seal { .. } => None,
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
                ManifestOp::Seal { .. } => {}
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
    read_only: bool,
    skew_run: BTreeMap<SeriesId, u32>,
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

    /// Whether the engine is up but read-only after a failed write or sync.
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
        // 1. The active segment past the durable mark (§3 Recovery).
        let read_only = self.recover_tail(recovery_sync_fails);
        // Read-only after a failed recovery sync: nothing more is written.
        if !read_only {
            self.clean_up_files();
        }
        // 4. Rebuild volatile state from every readable entry.
        let entries = self.disk.live_entries();
        let mut registry = Registry::default();
        let mut facts = Vec::new();
        for e in entries.values() {
            match &e.body {
                EntryBody::Registry(r) => registry.apply(r),
                EntryBody::Fact(f) => facts.push(f.clone()),
            }
        }
        let current = view::current(facts.iter());
        let mut newest_t = BTreeMap::new();
        let mut history: BTreeMap<(SeriesId, Nanos), Vec<(Value, Quality)>> = BTreeMap::new();
        for f in &facts {
            history
                .entry((f.series, f.t))
                .or_default()
                .push((f.value.clone(), f.quality));
            let n = newest_t.entry(f.series).or_insert(f.t);
            *n = (*n).max(f.t);
        }
        let trees = build_trees(&registry, &current);
        // The next position continues the current generation; a generation
        // started by a restore begins at its first position (§8).
        let generation = self.disk.generation;
        let next_position = entries
            .keys()
            .next_back()
            .filter(|p| p.generation == generation)
            .map_or(Position::first(generation), |p| p.next());
        let consumers = self
            .consumers
            .iter()
            .map(|d| {
                let s = self.sessions.entry(d.id).or_insert(0);
                *s += 1;
                let cursor = self
                    .disk
                    .cursors
                    .get(&d.id)
                    .copied()
                    .unwrap_or(Position::ORIGIN);
                ConsumerRt {
                    decl: d.clone(),
                    queue: VecDeque::new(),
                    // A consumer that is behind catches up from its cursor.
                    mode: if entries.range(cursor.next()..).next().is_some() {
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
            registry,
            entries,
            current,
            newest_t,
            history,
            trees,
            queue: VecDeque::new(),
            next_position,
            consumers,
            read_only: read_only || self.disk.drill,
            skew_run: BTreeMap::new(),
        });
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
                ManifestOp::Retire { .. } | ManifestOp::Seal { .. } => Vec::new(),
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
                self.vol_mut()?.read_only = true;
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
        let local = if self.any_current_detail_evicted() {
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
        Ok(self.tree(s)?.window(start, end, self.now))
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
        if self.vol.as_ref().is_some_and(|v| v.read_only) {
            raise(Level::Fatal, Reason::SyncFailed, &mut reasons);
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
        if !self.disk.bucket.reachable || self.disk.drill {
            return 0;
        }
        // A writer that finds a higher generation claimed stops uploading and
        // becomes read-only (§8 Generations).
        if self
            .disk
            .bucket
            .claims
            .iter()
            .any(|g| *g > self.disk.generation)
        {
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

    /// A new engine restored from the bucket alone (§8), with consumers that
    /// kept no cursor. See [`Model::restore_with_cursors`].
    pub fn restore_from(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        wall: Nanos,
        current_only: bool,
    ) -> Result<Model, Error> {
        Model::restore_with_cursors(
            config,
            consumers,
            bucket,
            &BTreeMap::new(),
            wall,
            current_only,
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
    /// aside. `cursors` are the positions consumers kept from before the
    /// restore: a consumer without one counts as beyond the restored end, and
    /// one at or below it catches up to the restored end, flagged
    /// `replayed`. Every consumer then receives `generation_changed` and
    /// `resync`. With `current_only`, only current chunks are restored first
    /// (partial restore); [`Model::restore_rest`] adds the rest.
    pub fn restore_with_cursors(
        config: Config,
        consumers: Vec<ConsumerDecl>,
        bucket: &mut Bucket,
        cursors: &BTreeMap<ConsumerId, Position>,
        wall: Nanos,
        current_only: bool,
        drill: bool,
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
        m.disk.generation = generation;
        m.disk.restored_end = Some(end);
        m.disk.drill = drill;
        let ids: Vec<ConsumerId> = m.consumers.iter().map(|c| c.id).collect();
        for id in ids {
            let cursor = cursors.get(&id).copied();
            m.disk
                .cursors
                .insert(id, cursor.filter(|c| *c <= end).unwrap_or(end));
            m.signals.entry(id).or_default().extend([
                Signal::GenerationChanged {
                    cursor: cursor.unwrap_or(Position::ORIGIN),
                    restored_end: end,
                },
                Signal::ResyncDue,
            ]);
        }
        m.restore_rest(current_only);
        Ok(m)
    }

    /// Downloads bucket files not yet on the fast disk and restarts.
    pub fn restore_rest(&mut self, current_only: bool) {
        let objects: Vec<(FileId, u64, StoredFile)> = self
            .disk
            .bucket
            .objects
            .iter()
            .map(|(id, (h, c))| (*id, *h, c.clone()))
            .collect();
        for (id, hash, content) in objects {
            if current_only && content.class != FileClass::Current {
                continue;
            }
            if self.disk.files.contains_key(&id) && self.disk.hot.contains(&id) {
                continue;
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
        }
        self.restart();
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
/// log starts at sequence 1 of its first generation, a gap is a missing
/// sequence within a generation, and a later generation starts at sequence 1
/// (§8 Restore step 1). `(g, 0)` when even the first position is missing.
fn restored_end(sorted: &[Position]) -> Position {
    let mut end = Position::new(sorted[0].generation, 0);
    for p in sorted {
        let follows = *p == end.next() || (p.generation > end.generation && p.sequence == 1);
        if !follows {
            break;
        }
        end = *p;
    }
    end
}
