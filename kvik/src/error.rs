//! Errors (`docs/api.md` Errors): a refusal of one reading, waiting
//! (back-pressure) and a fault are distinct. None of these types is `Clone`
//! or `PartialEq`, so that variants can carry I/O errors; compare with
//! `matches!`.

use std::io;
use std::path::PathBuf;

use crate::time::Timestamp;
use crate::types::Position;

/// Why one reading was refused; reported per index in
/// [`Ack`](crate::Ack) and counted per series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum Refusal {
    /// No series with this id.
    #[error("unknown series")]
    UnknownSeries,
    /// The series is retired and `t` is at or after its retirement time.
    #[error("series is retired")]
    Retired,
    /// The value does not fit the series' kind and value type.
    #[error("value does not fit the series")]
    WrongType,
    /// A state index outside the series' declared names.
    #[error("state index outside the declared names")]
    StateOutOfRange,
    /// Text longer than 255 bytes.
    #[error("text longer than 255 bytes")]
    TextTooLong,
    /// A reading of a versioned series without `changed_at` (spec §2.2).
    #[error("changed_at missing on a versioned series")]
    ChangedAtMissing,
    /// A reading with `changed_at` on a series that is not versioned.
    #[error("changed_at on a series that is not versioned")]
    ChangedAtUnexpected,
    /// `revise` intent on a versioned series (spec §2.4).
    #[error("revise on a versioned series")]
    ReviseOnVersioned,
    /// A reading of an interval series whose `t` is not an interval start.
    #[error("t is not an interval start")]
    OffPhase,
    /// A consumer emitted a reading on a series outside its `emits`.
    #[error("series not in the consumer's emits")]
    NotDeclared,
    /// More decimal places than the series' scale, under the policy
    /// `ExcessPrecision::Refuse` (spec §2.1).
    #[error("needs decimal scale {scale_needed}")]
    ExcessPrecision {
        /// The scale the value needs.
        scale_needed: u8,
    },
}

/// Why an emit waits (spec §3 Back-pressure, §2.5): neither a refusal nor a
/// fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum Wait {
    /// The ingest queue is full.
    #[error("ingest queue full")]
    QueueFull,
    /// Free space is below the critical mark.
    #[error("disk below the critical mark")]
    DiskFull,
    /// A host-stamped reading while the clock is not synchronised.
    #[error("host clock not synchronised")]
    UnsyncedClock,
}

/// A fault: a failed write or sync. The store is read-only after one.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Fault {
    /// An I/O operation failed.
    #[error("{op} on {path:?} failed: {kind}")]
    Io {
        /// The operation.
        op: &'static str,
        /// The file.
        path: PathBuf,
        /// The error kind.
        kind: io::ErrorKind,
    },
    /// A sync failed.
    #[error("sync of {path:?} failed")]
    SyncFailed {
        /// The file.
        path: PathBuf,
    },
    /// A manifest record is corrupt and no copy can be checked for.
    #[error("manifest record of epoch {epoch} is corrupt")]
    ManifestCorrupt {
        /// Its epoch.
        epoch: u64,
    },
    /// No space left.
    #[error("no space for {path:?}")]
    NoSpace {
        /// The file.
        path: PathBuf,
    },
}

/// The error of one emit call; nothing was queued unless it says otherwise.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EmitError {
    /// Back-pressure, from `try_emit` only: nothing was queued.
    #[error("busy: {0}")]
    Busy(Wait),
    /// Over the call limits: at most `min(65,536, limits.ingest_queue)`
    /// readings and 8 MiB plain-encoded; the caller splits.
    #[error("call too large: {readings} readings, about {bytes} bytes")]
    TooLarge {
        /// Readings in the call.
        readings: usize,
        /// Estimated plain-encoded bytes.
        bytes: usize,
    },
    /// The store is read-only after a fault.
    #[error("read-only: {0}")]
    ReadOnly(Fault),
    /// The store is shut down.
    #[error("the store is closed")]
    Closed,
    /// The batch may or may not be stored; never acknowledged.
    #[error("outcome unknown: {0}")]
    OutcomeUnknown(Fault),
}

/// The error of opening a store or a reader.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// Another writer holds the directory, also in this process.
    #[error("another writer holds the store directory")]
    Locked,
    /// No store in the directory (and `create_if_missing` is off, or this is
    /// a reader).
    #[error("not a store")]
    NotAStore,
    /// A file's store id does not match.
    #[error("store id of {file:?} does not match")]
    StoreIdMismatch {
        /// The file.
        file: PathBuf,
    },
    /// A file has a newer major format version or an unknown critical
    /// element.
    #[error("{file:?} has format {found}, supported {supported}")]
    UnsupportedFormat {
        /// The file.
        file: PathBuf,
        /// The version found.
        found: u32,
        /// The version supported.
        supported: u32,
    },
    /// The configured site zone is not the zone in force.
    #[error("site zone {configured} configured, but {in_force} is in force")]
    SiteZoneMismatch {
        /// The configured zone.
        configured: String,
        /// The zone in force.
        in_force: String,
    },
    /// The site zone is not in the tzdata release.
    #[error("unknown time zone")]
    UnknownZone,
    /// A file is corrupt.
    #[error("{file:?} is corrupt")]
    Corrupt {
        /// The file.
        file: PathBuf,
    },
    /// The platform lacks what the engine needs.
    #[error("unsupported platform: {reason}")]
    UnsupportedPlatform {
        /// Why.
        reason: String,
    },
    /// A reader could not read the mark.
    #[error("mark unreadable")]
    MarkUnreadable,
    /// A configuration value is out of its limits.
    #[error("configuration {key}: {reason}")]
    Config {
        /// The key.
        key: &'static str,
        /// Why.
        reason: String,
    },
    /// I/O failed.
    #[error("I/O on {path:?}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
}

/// The error of a call that appends an entry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WriteError {
    /// The store is read-only after a fault.
    #[error("read-only: {0}")]
    ReadOnly(Fault),
    /// The store is shut down.
    #[error("the store is closed")]
    Closed,
    /// The entry may or may not be stored.
    #[error("outcome unknown: {0}")]
    OutcomeUnknown(Fault),
}

/// The error of a series definition or change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DefineError {
    /// The series ceiling is reached.
    #[error("series ceiling of {limit} reached")]
    Ceiling {
        /// The ceiling.
        limit: u32,
    },
    /// The name belongs to a live series, or another series uses the alias.
    #[error("name in use")]
    NameInUse,
    /// The name is in use with a different definition.
    #[error("conflicts with the live series' {field}")]
    Conflict {
        /// The differing field.
        field: &'static str,
    },
    /// A field is invalid.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// Why.
        reason: String,
    },
    /// A zone is not in the tzdata release.
    #[error("unknown time zone")]
    UnknownZone,
    /// No such series.
    #[error("unknown series")]
    UnknownSeries,
    /// The series is retired.
    #[error("series is retired")]
    Retired,
    /// A scale raise that does not raise.
    #[error("scale not raised: currently {current}")]
    ScaleNotRaised {
        /// The current scale.
        current: u8,
    },
    /// The largest stored magnitude would overflow at the new scale.
    #[error("scale overflow: largest magnitude {largest_magnitude}")]
    ScaleOverflow {
        /// The largest magnitude stored.
        largest_magnitude: i64,
    },
    /// Writing the entry failed.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// Named by the interface but not supported by the current backend.
    #[error("not supported yet: {what}")]
    NotSupportedYet {
        /// What.
        what: &'static str,
    },
}

/// The error of a read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// No such series.
    #[error("unknown series")]
    UnknownSeries,
    /// No span over local detail remains (spec §6 Old spans).
    #[error("detail not local")]
    DetailNotLocal {
        /// Where local detail starts, if anywhere.
        local_from: Option<Timestamp>,
    },
    /// The aggregate does not apply to the series (spec §6 Aggregates).
    #[error("aggregate refused: {reason}")]
    AggregateRefused {
        /// Why.
        reason: String,
    },
    /// A zone is not in the tzdata release.
    #[error("unknown time zone")]
    UnknownZone,
    /// Not available from this handle or backend.
    #[error("not available: {reason}")]
    NotAvailable {
        /// Why.
        reason: String,
    },
    /// The range touches a lost range.
    #[error("lost range {id}")]
    LostRange {
        /// The loss's identity.
        id: Position,
    },
    /// The mark could not be read.
    #[error("mark unreadable")]
    MarkUnreadable,
    /// A file has an unsupported format.
    #[error("{file:?} has an unsupported format")]
    UnsupportedFormat {
        /// The file.
        file: PathBuf,
    },
    /// A file is corrupt.
    #[error("{file:?} is corrupt")]
    Corrupt {
        /// The file.
        file: PathBuf,
    },
    /// I/O failed.
    #[error("I/O on {path:?}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
    /// The query is malformed, such as a window whose start is not before its
    /// end or a date that does not exist.
    #[error("invalid query: {reason}")]
    Invalid {
        /// Why.
        reason: String,
    },
    /// The store was stopped and its data is gone (never with the model
    /// backend after a shutdown, which keeps it readable).
    #[error("the store is closed")]
    Closed,
}

/// The error of a consumer call. `docs/api.md` leaves funnel compilation
/// errors to roadmap 5.2; this is the model backend's set.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConsumerError {
    /// The store is shut down.
    #[error("the store is closed")]
    Closed,
    /// A handle for this consumer is already live.
    #[error("consumer {0:?} already has a live handle")]
    InUse(String),
    /// Every consumer slot is taken.
    #[error("all {0} consumer slots are taken")]
    TooManyConsumers(usize),
    /// A funnel names a series that does not exist.
    #[error("unknown series")]
    UnknownSeries,
    /// A routing change is refused (spec §4, §6 Cadence).
    #[error("route refused: {0}")]
    Route(String),
    /// A route, a change of `emits` or an alias would close a cycle between
    /// consumers (spec §4 Routing). Not produced by the model backend, which
    /// accepts only consumers that emit nothing.
    #[error("routing cycle: {0}")]
    RouteCycle(String),
    /// A confirmation names a delivery that was not received.
    #[error("confirmation of a delivery that was not received")]
    BadConfirm,
    /// Writing the routing entry failed.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// Named by the interface but not supported by the current backend.
    #[error("not supported yet: {what}")]
    NotSupportedYet {
        /// What.
        what: &'static str,
    },
}

/// The error of backup, restore and rebuild calls, settled with roadmap 8.2.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackupError {
    /// Not supported by the current backend.
    #[error("not supported yet: {what}")]
    NotSupportedYet {
        /// What.
        what: &'static str,
    },
}
