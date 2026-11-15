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

/// Drives a restore one file at a time.
pub trait RestoreDriver: Send {
    /// Finds the store id, checks the key (`WrongKey` before any file is judged), fetches `STORE`, reads the claims
    /// by `stat` (Q-163), claims (Full), reads the source records, plans, syncs the record in `MANIFEST`, uploads it
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
    /// The bucket holds several stores.
    #[error("the bucket holds {} stores", .0.len())]
    SeveralStores(Vec<[u8; 16]>),
    /// A higher generation was claimed by another restore.
    #[error("generation {by} was claimed by another restore")]
    Superseded {
        /// The higher generation.
        by: u32,
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
}
