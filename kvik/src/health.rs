//! Health and statistics (spec §11; `docs/api.md` Health and statistics).

use std::collections::BTreeMap;
use std::fmt;

use jiff::Timestamp;

use crate::error::{BusyReason, Refusal};
use crate::types::{Position, SeriesId};

/// A reason in a health report (spec §11 Health).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HealthReason {
    /// The store was shut down or stopped.
    Closed,
    /// A write or sync failed: read-only until restarted.
    SyncFailed,
    /// A higher generation is claimed in the bucket (spec §8 Generations).
    Superseded,
    /// A restore failed, naming the file if known.
    RestoreFailed {
        /// The file.
        file: Option<String>,
    },
    /// The store refused to start, such as for another site zone.
    OpenRefused,
    /// A file is quarantined.
    Quarantined {
        /// The file.
        file: String,
    },
    /// Free space below the low mark.
    LowDisk,
    /// Free space below the critical mark: ingest waits.
    CriticalDisk,
    /// Sealed files wait for upload.
    UploadStalled,
    /// A series' source clock appears skewed.
    ClockSkew(SeriesId),
    /// A lost range, by corruption or a file lost from the bucket.
    DataLost {
        /// First position.
        from: Position,
        /// Last position.
        to: Position,
    },
}

impl fmt::Display for HealthReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthReason::Closed => f.write_str("the store is closed"),
            HealthReason::SyncFailed => f.write_str("a write or sync failed"),
            HealthReason::Superseded => f.write_str("a higher generation is claimed"),
            HealthReason::RestoreFailed { file: Some(file) } => {
                write!(f, "the restore failed at {file}")
            }
            HealthReason::RestoreFailed { file: None } => f.write_str("the restore failed"),
            HealthReason::OpenRefused => f.write_str("the store refused to start"),
            HealthReason::Quarantined { file } => write!(f, "file {file} is quarantined"),
            HealthReason::LowDisk => f.write_str("free space below the low mark"),
            HealthReason::CriticalDisk => f.write_str("free space below the critical mark"),
            HealthReason::UploadStalled => f.write_str("uploads are stalled"),
            HealthReason::ClockSkew(s) => write!(f, "the source clock of {s} appears skewed"),
            HealthReason::DataLost { from, to } => write!(f, "data lost from {from} to {to}"),
        }
    }
}

/// Health (spec §11).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Health {
    /// All well.
    Ok,
    /// Working, with reasons.
    Degraded {
        /// Reasons.
        reasons: Vec<HealthReason>,
    },
    /// Not accepting writes.
    ReadOnly {
        /// The first fatal reason.
        cause: HealthReason,
        /// Every reason.
        reasons: Vec<HealthReason>,
    },
}

/// Per-series statistics.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SeriesStats {
    /// Facts recorded.
    pub facts: u64,
    /// The greatest `t` over its recorded facts.
    pub latest_t: Option<Timestamp>,
    /// Counter regressions (spec §5.5).
    pub counter_regressions: u64,
    /// Calendar-length interval readings found displaced (spec §5.4).
    pub interval_displaced: u64,
}

/// Per-consumer statistics.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ConsumerStats {
    /// The durable cursor.
    pub cursor: Position,
    /// Whether it is catching up from the log.
    pub catching_up: bool,
    /// Funnels in force.
    pub funnels: usize,
    /// Condition evaluations skipped during a restore (spec §6 Conditions).
    pub evaluations_unknown: u64,
}

/// Statistics: every refusal, drop, repeat and delay the specification
/// counts (spec §1.5, §11). Fields the backend cannot measure are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Stats {
    /// Identical repeats dropped (spec §2.4), all series.
    pub duplicates_dropped: u64,
    /// Readings recorded although the duplicate check could not run.
    pub duplicates_unchecked: u64,
    /// Refusals by reason.
    pub refused: BTreeMap<Refusal, u64>,
    /// Back-pressure waits by reason.
    pub waits: BTreeMap<BusyReason, u64>,
    /// Facts flagged late.
    pub late: u64,
    /// Facts flagged future.
    pub future: u64,
    /// NaN or infinite values recorded without a value.
    pub non_finite: u64,
    /// Negative counter values recorded without a value.
    pub negative_counter: u64,
    /// Facts outranked when committed.
    pub outranked_on_arrival: u64,
    /// Consumer switches to catch-up.
    pub overflows: u64,
    /// Batches discarded by recovery.
    pub batches_discarded: u64,
    /// Per series.
    pub series: BTreeMap<SeriesId, SeriesStats>,
    /// Per consumer, by name.
    pub consumers: BTreeMap<String, ConsumerStats>,
    /// Latest commit latency, if measured.
    pub commit_latency: Option<std::time::Duration>,
    /// Bytes waiting for backup, if a sink is attached.
    pub backup_lag: Option<u64>,
    /// Files and bytes a restore has still to bring back.
    pub restore_remaining: Option<(u64, u64)>,
}
