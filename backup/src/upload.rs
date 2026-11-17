//! The upload driver (roadmap 8.1c, `spec.md` §8 Continuous backup, Hashes): `STORE` first; per file `seal` → `put`
//! → `Verified::check` → a copy-verified record (`0x27`); then each record whose live files are all copied, in epoch
//! order, after `settle`. The queue is recomputed from the manifest's snapshot (Q-280, Q-282); the record cursor from
//! a listing of the bucket at the first step.

use std::io;
use std::time::Duration;

use kvik_files::desc::FileRef;
use kvik_log::manifest::RecordPosition;
use kvik_sink_api::{SinkError, VerifyError};

use crate::DriverFuture;
use crate::envelope::EnvelopeError;
use crate::port::PortError;

mod driver;
mod mark;
mod probe;

pub use driver::Uploader;

/// Drives uploads one object at a time.
pub trait BackupDriver: Send {
    /// The attach checks (Q-162): conditional create, reported checksums, no other store's files, and the key
    /// against the bucket's `STORE`.
    fn probe<'a>(&'a mut self) -> DriverFuture<'a, Result<(), Refused>>;
    /// Uploads at most one object; call again at once after [`Step::Progress`].
    fn step<'a>(&'a mut self, now: Duration) -> DriverFuture<'a, Step>;
    /// Where the uploads stand.
    fn progress(&self) -> Progress;
}

/// What one [`BackupDriver::step`] did.
#[derive(Debug)]
#[non_exhaustive]
pub enum Step {
    /// An object was uploaded or a record appended.
    Progress,
    /// Nothing is queued.
    Idle,
    /// A provisional rebuild holds uploads.
    Held,
    /// A transient sink fault: call again after `after`.
    Retry {
        /// How long to wait.
        after: Duration,
        /// The fault.
        error: SinkError,
    },
    /// Uploads cannot continue.
    Failed(UploadError),
}

/// Why a bucket is refused at attach.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Refused {
    /// The bucket ignores conditional writes.
    #[error("the bucket does not honour conditional writes")]
    NoConditionalWrites,
    /// The bucket does not check or report checksums.
    #[error("the bucket does not check checksums")]
    NoChecksums,
    /// The bucket holds another store's files.
    #[error("the bucket holds another store's files")]
    OtherStore {
        /// That store's id.
        store_id: [u8; 16],
    },
    /// The bucket's `STORE` was sealed with another key.
    #[error("the bucket's store was sealed with another key")]
    WrongKey,
    /// The sink failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// Why uploads stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UploadError {
    /// The manifest port failed.
    #[error(transparent)]
    Port(#[from] PortError),
    /// Reading a local file failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Sealing failed.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// A local file's plaintext hash is not its description's.
    #[error("file {file:?} does not have its recorded hash")]
    LocalHash {
        /// The file.
        file: FileRef,
    },
    /// The bucket reported other content than was uploaded.
    #[error(transparent)]
    Verify(#[from] VerifyError),
    /// A restore claimed a generation above the store's: uploads stop for good and the store becomes read-only
    /// (`spec.md` §8 Generations; Q-265).
    #[error("generation {by} was claimed by a restore")]
    Superseded {
        /// The highest generation claimed.
        by: u32,
    },
}

/// Where the uploads stand; "stalled" is the caller's rule over `failing_since`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The last position whose file, sealed-segment and copy-verified records are all uploaded.
    pub backed_up_through: Option<RecordPosition>,
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
    /// When the current run of failures began.
    pub failing_since: Option<Duration>,
    /// The last error.
    pub last_error: Option<String>,
}

/// The upload driver's limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadConfig {
    /// The largest object the sink takes.
    pub max_object: u64,
    /// The first retry delay.
    pub retry_first: Duration,
    /// The longest retry delay.
    pub retry_max: Duration,
}
