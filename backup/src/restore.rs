//! The restore driver (roadmap 8.3b). It owns the new directory's `Appender`. A drill makes no `put`. A file is
//! installed only once the reported hash equals its copy and the plaintext hash equals its description's.

use std::io;
use std::time::Duration;

use kvik_files::appender::AppendError;
use kvik_files::desc::FileRef;
use kvik_files::record::RecordError;
use kvik_files::restore::RestoreRecord;
use kvik_log::manifest::LostRange;
use kvik_sink_api::SinkError;

use crate::DriverFuture;
use crate::envelope::EnvelopeError;
use crate::plan::{PlanError, PlanMode};
use crate::upload::Refused;

mod driver;
mod judge;
mod local;
mod probe;
mod source;

pub use driver::Restorer;

/// Drives a restore one file at a time.
pub trait RestoreDriver: Send {
    /// Finds the store id, checks the key (`WrongKey` before any file is judged), fetches `STORE`, probes the bucket
    /// (Full; format A.17 Probe, Q-214), reads the claims by `stat` (Q-163), claims (Full), reads the source records, plans, syncs the record in `MANIFEST`, uploads it
    /// and reads the claims again (Full). A directory with a restore record resumes; files and no record:
    /// [`RestoreError::NotEmpty`].
    fn prepare<'a>(&'a mut self, wall: i64) -> DriverFuture<'a, Result<Prepared, RestoreError>>;
    /// Fetches, verifies and installs one file.
    fn step<'a>(&'a mut self, now: Duration) -> DriverFuture<'a, RestoreStep>;
    /// Where the restore stands.
    fn progress(&self) -> RestoreProgress;
}

/// A prepared restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// The store id found.
    pub store_id: [u8; 16],
    /// The restore record.
    pub record: RestoreRecord,
    /// The directory already held this record.
    pub resumed: bool,
    /// Files of the source generation in the bucket that no record read names, which the restore leaves out (Q-613);
    /// empty on a resume.
    pub unnamed: Vec<FileRef>,
    /// The latest last-uploaded marker could not be read and an earlier one was checked instead (Q-614); `None` on a
    /// resume.
    pub marker_fallback: Option<MarkerFallback>,
    /// Records a marker names that the bucket does not hold, accepted by [`RestoreConfig::accept_earlier_state`]
    /// (Q-962); `None` on a resume.
    pub earlier_state: Option<EarlierState>,
}

/// A restore checked an earlier marker than the latest (Q-614).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerFallback {
    /// The generation.
    pub generation: u32,
    /// The sequences passed over, latest first: not readable, or no longer in the bucket.
    pub unreadable: Vec<u64>,
    /// The sequence checked.
    pub used: u64,
}

/// What a restore that accepts an earlier state left out (Q-962).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EarlierState {
    /// The generation whose marker names the records.
    pub generation: u32,
    /// The marker's sequence number.
    pub sequence: u64,
    /// The records the marker names that the bucket does not hold.
    pub missing: Vec<u64>,
    /// The last record read, if any: the epochs after it up to `marked_end` are not restored.
    pub read_end: Option<u64>,
    /// The last record the marker names.
    pub marked_end: u64,
}

/// What one [`RestoreDriver::step`] did.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreStep {
    /// A file is installed.
    Installed {
        /// The file.
        file: FileRef,
    },
    /// Fetching failed for now: call again after `after`.
    Retry {
        /// How long to wait.
        after: Duration,
        /// The file.
        file: FileRef,
        /// Why.
        reason: String,
    },
    /// A file is lost (Q-82 thresholds met), recorded by the driver; the restore continues.
    Lost {
        /// The file.
        file: FileRef,
        /// The range recorded.
        loss: LostRange,
    },
    /// Every file is installed or lost.
    Done,
    /// The restore cannot continue.
    Failed(RestoreError),
}

/// Files and bytes of a restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreProgress {
    /// Files listed.
    pub files_total: u64,
    /// Files left.
    pub files_left: u64,
    /// Bytes listed.
    pub bytes_total: u64,
    /// Bytes left.
    pub bytes_left: u64,
}

/// Why a restore failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// The bucket's objects were sealed with another key.
    #[error("the bucket was sealed with another key")]
    WrongKey,
    /// The directory holds files and no restore record.
    #[error("the directory is not empty")]
    NotEmpty,
    /// The bucket holds no store.
    #[error("the bucket holds no store")]
    NoStore,
    /// The bucket fails the probe a full restore makes before it claims (format A.17 Probe; Q-214).
    #[error("the bucket is refused: {0}")]
    Refused(Refused),
    /// The bucket holds several stores.
    #[error("the bucket holds {} stores", .0.len())]
    SeveralStores(Vec<[u8; 16]>),
    /// A higher generation was claimed by another restore.
    #[error("generation {by} was claimed by another restore")]
    Superseded {
        /// The higher generation.
        by: u32,
    },
    /// A record the listing of a generation's records does not show is reported by `stat`: the listing lags; retry
    /// (Q-304, Q-611, Q-612).
    #[error("the listing of generation {generation}'s records is incomplete; retry")]
    ListingIncomplete {
        /// The generation.
        generation: u32,
    },
    /// A record of the generation read names a predecessor the bucket does not hold before it (format A.17; Q-611).
    #[error(
        "generation {generation}'s record {epoch} names record {previous}, which breaks the chain of its records"
    )]
    BrokenChain {
        /// The generation.
        generation: u32,
        /// The record.
        epoch: u64,
        /// The epoch it names.
        previous: u64,
    },
    /// The generation read has records and no last-uploaded marker: a bucket written before the marker, one that
    /// lost it, or a first record whose marker is not yet put (format A.17; Q-614). [`RestoreConfig::accept_unmarked`]
    /// restores it without the marker check.
    #[error(
        "generation {generation} has records and no last-uploaded marker: written before markers, the marker is lost, or its first marker is not yet put (try again)"
    )]
    MarkerMissing {
        /// The generation.
        generation: u32,
    },
    /// No marker of the generation read opens and decodes, or the latest readable one is of a version this build does
    /// not read (Q-614); `sequence` is the marker that failed.
    #[error("generation {generation}'s last-uploaded marker {sequence} is unreadable: {reason}")]
    MarkerUnreadable {
        /// The generation.
        generation: u32,
        /// The marker's sequence number.
        sequence: u64,
        /// Why.
        reason: String,
    },
    /// The latest marker names record `epoch` (the last uploaded, or an end of the records a restore reads), which
    /// the bucket does not report (Q-614).
    #[error(
        "generation {generation}'s last-uploaded marker {sequence} names record {epoch}, which the bucket does not hold"
    )]
    MarkerUnreached {
        /// The generation.
        generation: u32,
        /// The marker's sequence number.
        sequence: u64,
        /// The record.
        epoch: u64,
    },
    /// The records read end before the range the latest marker names, though the bucket lists both its ends
    /// (Q-614).
    #[error(
        "generation {generation}'s last-uploaded marker {sequence} names records {marked:?}, and the restore reads {read:?}"
    )]
    MarkerMismatch {
        /// The generation.
        generation: u32,
        /// The marker's sequence number.
        sequence: u64,
        /// The first and last record the marker names.
        marked: (u64, u64),
        /// The first and last record read, if any.
        read: Option<(u64, u64)>,
    },
    /// The marker named a record that `stat` reports and the listing did not show at each of `tries` consecutive
    /// `prepare` calls of this driver ([`MARKER_LISTING_TRIES`]; Q-614): the listing lags longer than the retries.
    #[error(
        "the listing of generation {generation}'s records did not show record {epoch}, which its last-uploaded marker names, in {tries} tries"
    )]
    MarkerNotListed {
        /// The generation.
        generation: u32,
        /// The record.
        epoch: u64,
        /// The tries.
        tries: u32,
    },
    /// No plan can be made.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The sink failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
    /// A local file operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Opening an object failed.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// A downloaded record is invalid.
    #[error(transparent)]
    Record(#[from] RecordError),
    /// Appending to the new manifest failed.
    #[error(transparent)]
    Append(#[from] AppendError),
}

/// The restore driver's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreConfig {
    /// Full or drill.
    pub mode: PlanMode,
    /// Failed fetches of one file before it is lost (Q-82).
    pub lost_tries: u32,
    /// How long one file's fetches fail before it is lost (Q-82).
    pub lost_over: Duration,
    /// The first retry delay.
    pub retry_first: Duration,
    /// The longest retry delay.
    pub retry_max: Duration,
    /// Restores a generation that has records and no last-uploaded marker without the marker check, instead of
    /// failing with [`RestoreError::MarkerMissing`] (Q-614). A marker present is always checked.
    pub accept_unmarked: bool,
    /// Restores what the bucket holds when a marker names records it does not hold, reporting them in
    /// [`Prepared::earlier_state`], instead of failing with [`RestoreError::MarkerUnreached`] (Q-962; off by default).
    pub accept_earlier_state: bool,
}

/// Consecutive `prepare` calls of one driver that may find the marker ahead of the listing: the earlier ones fail as
/// [`RestoreError::ListingIncomplete`], to be retried, the last as [`RestoreError::MarkerNotListed`] (Q-614).
pub const MARKER_LISTING_TRIES: u32 = 10;
