//! Health, loss reports and statistics (spec §11; `docs/api.md` Health and
//! statistics).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::error::{Fault, Refusal, Wait};
use crate::time::{Span, Timestamp};
use crate::types::{Position, SeriesId};

/// Health (spec §11).
#[derive(Debug)]
#[non_exhaustive]
pub enum Health {
    /// All well.
    Ok,
    /// Working, with reasons.
    Degraded {
        /// Reasons.
        reasons: Vec<Reason>,
    },
    /// Not accepting writes after a fault.
    ReadOnly {
        /// The fault.
        cause: Fault,
        /// Every reason.
        reasons: Vec<Reason>,
    },
    /// The writer was shut down (an addition of the model backend; readers
    /// keep reading).
    Closed,
}

/// A reason in a health report.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// A lost range, held until acknowledged.
    DataLost(LossReport),
    /// An emit is waiting, for this reason.
    IngestWaiting(Wait),
    /// Free space below the low mark.
    DiskLow {
        /// Free bytes.
        free: u64,
    },
    /// Free space below the critical mark: ingest waits.
    DiskCritical {
        /// Free bytes.
        free: u64,
    },
    /// A segment cannot be sealed.
    SealBlocked {
        /// The segment.
        file_id: u64,
    },
    /// A sealed segment that no manifest record lists cannot be read whole or fails its checks, so it cannot be
    /// listed or backed up; held while the store runs and raised again at each start until repaired (Q-221 (c)).
    SegmentDamaged {
        /// The segment.
        file_id: u64,
    },
    /// A corrupt manifest record was rebuilt from the local files, until
    /// acknowledged.
    ManifestRebuilt {
        /// Its epoch.
        epoch: u64,
        /// Rebuilt while deep could not be reached, after
        /// `manifest.rebuild_wait` (spec §7 Manifest): all uploads,
        /// evictions and removals are held until deep answers, and the
        /// rebuild cannot be acknowledged until it is final.
        provisional: bool,
    },
    /// A series' source clock appears skewed (spec §9 row 5; an addition of
    /// the model backend).
    ClockSkew(SeriesId),
    /// Uploads have failed for longer than the stall limit.
    BackupStalled {
        /// When the failures began.
        since: Timestamp,
        /// Files queued.
        queued_files: u64,
        /// The last error.
        error: String,
    },
    /// The bucket was refused at attach.
    BackupRefused(BackupRefusal),
    /// A restore cannot continue past a file.
    RestoreFailed {
        /// The file.
        file_id: u64,
        /// Why.
        reason: String,
    },
    /// A restore set positions aside; information, not degraded (Q-90).
    SetAside(SetAsideReport),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::DataLost(r) => write!(f, "data lost at {}", r.identity),
            Reason::IngestWaiting(w) => write!(f, "ingest waiting: {w}"),
            Reason::DiskLow { free } => write!(f, "free space low: {free} bytes"),
            Reason::DiskCritical { free } => write!(f, "free space critical: {free} bytes"),
            Reason::SealBlocked { file_id } => write!(f, "segment {file_id} cannot be sealed"),
            Reason::SegmentDamaged { file_id } => write!(f, "sealed segment {file_id} is damaged"),
            Reason::ManifestRebuilt { epoch, provisional } => {
                let how = if *provisional { " provisionally" } else { "" };
                write!(f, "manifest record of epoch {epoch} rebuilt{how}")
            }
            Reason::ClockSkew(s) => write!(f, "the source clock of {s} appears skewed"),
            Reason::BackupStalled {
                queued_files,
                error,
                ..
            } => write!(
                f,
                "backup stalled with {queued_files} files queued: {error}"
            ),
            Reason::BackupRefused(r) => write!(f, "backup refused: {r}"),
            Reason::RestoreFailed { file_id, reason } => {
                write!(f, "restore of file {file_id} failed: {reason}")
            }
            Reason::SetAside(r) => write!(f, "positions {} to {} set aside", r.first, r.last),
        }
    }
}

/// Why a bucket is refused at attach (spec §8 Deep).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BackupRefusal {
    /// The bucket ignores conditional writes.
    #[error("the bucket does not honour conditional writes")]
    NoConditionalWrites,
    /// The bucket does not check or report checksums.
    #[error("the bucket does not check checksums")]
    NoChecksums,
    /// The bucket holds another store's files.
    #[error("the bucket holds another store's files")]
    OtherStore,
    /// The bucket's store was sealed with another key.
    #[error("the bucket's store was sealed with another key")]
    WrongKey,
}

/// The positions one restore record set aside (spec §8 Restore step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SetAsideReport {
    /// The first position set aside.
    pub first: Position,
    /// The last position set aside.
    pub last: Position,
    /// Facts set aside.
    pub facts: u64,
    /// The first commit time set aside.
    pub commit_time_first: Timestamp,
    /// The last commit time set aside.
    pub commit_time_last: Timestamp,
}

/// Backup statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackupStats {
    /// A sink is attached.
    pub attached: bool,
    /// The last position whose files and records are all uploaded.
    pub backed_up_through: Option<Position>,
    /// Files queued.
    pub queued_files: u64,
    /// Bytes queued.
    pub queued_bytes: u64,
    /// Records queued.
    pub queued_records: u64,
    /// Files uploaded.
    pub uploaded_files: u64,
    /// Bytes uploaded.
    pub uploaded_bytes: u64,
    /// Retries.
    pub retries: u64,
}

/// Restore statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RestoreStats {
    /// A restore is installing files.
    pub restoring: bool,
    /// The store is a drill's.
    pub drill: bool,
    /// The generation of the latest restore record.
    pub generation: u32,
    /// The end it restored.
    pub restored_end: Option<Position>,
    /// Files left.
    pub files_left: u64,
    /// Bytes left.
    pub bytes_left: u64,
}

/// Whether a loss is certain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LossKind {
    /// Facts were lost.
    DataLost,
    /// A truncated tail may have held acknowledged facts.
    PossibleLoss,
}

/// What caused a loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LossCause {
    /// Corruption with no other copy.
    Corruption,
    /// A torn tail truncated at recovery.
    TruncatedTail,
    /// A file the bucket lost.
    LostInBucket,
    /// Content that failed validation.
    InvalidContent,
    /// Not known: a loss found again by a manifest rebuild whose cause no
    /// valid record or recovery entry gives (spec §7 Manifest).
    Unknown,
}

/// The state of the mark at recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkState {
    /// Readable.
    Readable,
    /// Missing.
    Missing,
    /// Unreadable.
    Unreadable,
    /// Stale.
    Stale,
}

/// A loss report (spec §11 Health).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LossReport {
    /// The first position of its range when first reported.
    pub identity: Position,
    /// Kind.
    pub kind: LossKind,
    /// Cause.
    pub cause: LossCause,
    /// First position lost, if known.
    pub first: Option<Position>,
    /// Last position lost, if known.
    pub last: Option<Position>,
    /// The file, if known.
    pub file_id: Option<u64>,
    /// Bytes.
    pub bytes: u64,
    /// Commit blocks still readable in it.
    pub readable_commit_blocks: u32,
    /// The mark's state.
    pub mark_state: MarkState,
    /// The boot identity of the mark.
    pub boot_id: Option<[u8; 16]>,
    /// Commit time of the batch before.
    pub commit_time_before: Option<Timestamp>,
    /// Commit time of the batch after.
    pub commit_time_after: Option<Timestamp>,
    /// Consumers whose cursor file was missing.
    pub cursors_missing: Vec<String>,
    /// Recorded in the manifest.
    pub recorded: bool,
    /// Acknowledged.
    pub acknowledged: bool,
}

/// A jump of the wall clock measured against the monotonic clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClockJump {
    /// When it was seen (wall clock after the jump).
    pub at: Timestamp,
    /// By how much.
    pub by: Span,
}

/// Commit latency percentiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Latency {
    /// Median.
    pub p50: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// Largest.
    pub max: Duration,
}

/// Ingest statistics.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct IngestStats {
    /// Readings queued now.
    pub queue_depth: usize,
    /// Why an emit is waiting now, if one is.
    pub waiting: Option<Wait>,
    /// Total time emits have waited (not measured by the model backend).
    pub waited: Duration,
    /// Waits by reason.
    pub waits: BTreeMap<Wait, u64>,
    /// Batches committed.
    pub batches: u64,
    /// Log syncs.
    pub syncs: u64,
    /// Commit latency (zero with the model backend).
    pub commit_latency: Latency,
}

/// How the mark is synced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkSyncMode {
    /// On a timer.
    #[default]
    Interval,
    /// At every commit.
    EveryCommit {
        /// Why.
        reason: String,
    },
}

/// Log statistics.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LogStats {
    /// The durable mark (in memory with the model backend).
    pub durable: Position,
    /// The position the mark file was last synced at.
    pub mark_synced: Position,
    /// How the mark is synced.
    pub mark_sync: MarkSyncMode,
    /// The open segment.
    pub open_file_id: Option<u64>,
    /// Sealed segments.
    pub sealed_segments: u64,
    /// Bytes in the log (model accounting).
    pub bytes: u64,
    /// Manifest records rebuilt from local files.
    pub manifest_rebuilds: u64,
    /// Losses a manifest rebuild listed under their first missing position,
    /// because no valid record named their identity (spec §7 Manifest,
    /// invariant 27).
    pub loss_identities_rebuilt: u64,
}

/// Disk statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiskStats {
    /// Free bytes (from the reference model's simulated disk with the model
    /// backend).
    pub free: u64,
}

/// Clock statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClockStats {
    /// Jumps seen.
    pub jumps: u64,
    /// The last jump.
    pub last: Option<ClockJump>,
}

/// Statistics: every refusal, drop, repeat and delay the specification
/// counts (spec §1.5, §11).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Stats {
    /// Ingest.
    pub ingest: IngestStats,
    /// Log.
    pub log: LogStats,
    /// Disk.
    pub disk: DiskStats,
    /// Clock.
    pub clock: ClockStats,
    /// Delivery (durable backend).
    pub delivery: DeliveryStats,
}

/// What taking the committed log for delivery met (durable backend; zero on the model backend).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeliveryStats {
    /// Reads of the log that failed and were tried again at the next one.
    pub refresh_errors: u64,
    /// Invalid regions below the durable mark the delivery reader passed over (`spec.md` §3 Recovery rule 2).
    pub corruptions: u64,
}

/// Per-series statistics.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SeriesStats {
    /// Facts recorded.
    pub recorded: u64,
    /// Identical repeats dropped.
    pub duplicates_dropped: u64,
    /// Recorded although the duplicate check could not run.
    pub duplicates_unchecked: u64,
    /// Refusals by reason.
    pub refused: BTreeMap<Refusal, u64>,
    /// Facts outranked when committed.
    pub outranked_on_arrival: u64,
    /// Facts flagged late.
    pub late: u64,
    /// Facts flagged future.
    pub future: u64,
    /// Orphaned facts (spec §2.1).
    pub orphaned_facts: u64,
    /// Decimal values rounded.
    pub rounded: u64,
    /// Scale raises.
    pub scale_raises: u64,
    /// Readings submitted as `Value::Absent`, recorded with quality bad
    /// whatever quality was supplied (spec §2.5).
    pub absent_supplied: u64,
    /// Decimal values out of range, recorded with quality bad (spec §2.5).
    pub out_of_range: u64,
    /// The greatest `t` over its recorded facts (an addition).
    pub latest_t: Option<Timestamp>,
    /// Counter regressions (spec §5.5; an addition).
    pub counter_regressions: u64,
    /// Calendar-length interval readings found displaced (spec §5.4).
    pub interval_displaced: u64,
}

/// Per-consumer statistics (a key of roadmap 5.x; an addition of the model
/// backend).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ConsumerStats {
    /// The durable cursor (in memory with the model backend).
    pub cursor: Position,
    /// Whether it is catching up from the log.
    pub catching_up: bool,
    /// Funnels in force.
    pub funnels: usize,
}
