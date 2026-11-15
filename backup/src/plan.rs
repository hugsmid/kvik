//! What a restore installs, from the records it read (roadmap 8.3a; pure, and differential against the model's
//! `restore_from`). The restored log ends before the first listed file with no copy-verified record.

use kvik_files::catalogue::Catalogue;
use kvik_files::desc::FileRef;
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_log::manifest::RecordPosition;

/// What a plan is for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanMode {
    /// A restore that claims a generation.
    Full,
    /// A drill: claims nothing, uploads nothing.
    Drill,
}

/// What a plan is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanInput<'a> {
    /// The generation claimed by the executor; a drill passes the highest claimed + 1.
    pub generation: u32,
    /// The reset stamp's nonce.
    pub nonce: u64,
    /// The generation of the records read.
    pub source_generation: u32,
    /// The fold of the records read (Q-108 applied by the reader).
    pub source: &'a Catalogue,
    /// Full or drill.
    pub mode: PlanMode,
    /// Positions a recovery entry names as `after`; empty for a log with no gap.
    pub recovery_after: &'a [RecordPosition],
}

/// The restore record to write and the files to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    /// The record.
    pub record: RestoreRecord,
    /// The files to fetch, ascending by position.
    pub fetch: Vec<RestoredFile>,
}

/// Why no plan can be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// No records were read.
    #[error("no manifest records")]
    NoRecords,
    /// No generation is left to claim.
    #[error("no generation is left")]
    GenerationsExhausted,
    /// A file at or below a carried end has no copy.
    #[error("file {file:?} has no verified copy")]
    NotCopied {
        /// The file.
        file: FileRef,
    },
    /// No file holds a position at or below the end.
    #[error("no file holds {at:?}")]
    MissingBelowEnd {
        /// The position.
        at: RecordPosition,
    },
    /// The records contradict each other.
    #[error("invalid source: {0}")]
    Invalid(&'static str),
}

/// The signature of the planner, so that a restore driver takes a hand-written one until 8.3a's lands.
pub type PlanFn = fn(&PlanInput<'_>) -> Result<RestorePlan, PlanError>;
