//! Errors: refusals, back-pressure and faults are distinct (`docs/api.md`
//! Principles).

use std::path::PathBuf;

use crate::types::SeriesId;

/// Result with the crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every error the interface returns.
///
/// A refusal of one reading is not an error of the call: it is reported per
/// index in [`Ack::refused`](crate::Ack::refused) and counted. A fault (a
/// failed write or sync) makes the store read-only (spec §3).
///
/// `Error` is neither `Clone` nor `PartialEq`, so that later variants can
/// carry I/O errors; compare with `matches!`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Another writer handle holds the store directory (spec §3 Single writer).
    #[error("another writer holds the store directory {dir:?}")]
    Locked {
        /// The directory.
        dir: PathBuf,
    },
    /// No store exists at the directory.
    #[error("no store at {dir:?}")]
    NotFound {
        /// The directory.
        dir: PathBuf,
    },
    /// The store was shut down or stopped; the handle is no longer usable.
    #[error("the store is closed")]
    Closed,
    /// The store is read-only after a fault, as a restore drill, or because a
    /// higher generation is claimed (spec §3, §8).
    #[error("the store is read-only: {cause}")]
    ReadOnly {
        /// Why.
        cause: String,
    },
    /// Back-pressure: [`Store::try_emit`](crate::Store::try_emit) does not wait
    /// (spec §3 Back-pressure). Nothing of the batch was accepted.
    #[error("busy: {0}")]
    Busy(BusyReason),
    /// A single item was refused, such as a time outside an interval
    /// series' intervals.
    #[error("refused: {0}")]
    Refused(Refusal),
    /// No series with this id or name.
    #[error("unknown series {0}")]
    UnknownSeries(String),
    /// The series is retired (spec §2.1).
    #[error("series {0} is retired")]
    Retired(SeriesId),
    /// The series definition is invalid (spec §2.1).
    #[error("invalid series definition: {0}")]
    InvalidDefinition(String),
    /// A live series uses the name with a different kind, value type, scale,
    /// unit or option; retire it and define anew (`docs/api.md` Series).
    #[error("the name {0:?} is in use by a live series with a different definition")]
    NameInUse(String),
    /// The configured ceiling on series is reached (spec §9 row 30).
    #[error("series ceiling of {0} reached")]
    SeriesCeiling(usize),
    /// The time zone is not in the tzdata in force.
    #[error("unknown time zone {0:?}")]
    UnknownZone(String),
    /// The configured site zone is not the store's zone in force; changing it
    /// is a log entry, never a reopen (spec §5.7, §12 D3).
    #[error("the store's site zone is {store}, not {configured}")]
    SiteZoneMismatch {
        /// The store's zone.
        store: String,
        /// The configured zone.
        configured: String,
    },
    /// A route, a change of `emits` or an alias would close a cycle between
    /// consumers (spec §4 Routing); names the cycle. Not produced by the model
    /// backend, which accepts only consumers that emit nothing.
    #[error("routing cycle: {0}")]
    RouteCycle(String),
    /// A routing change is refused (spec §4, §6 Cadence).
    #[error("route refused: {0}")]
    Route(String),
    /// A handle for this consumer is already live.
    #[error("consumer {0:?} already has a live handle")]
    ConsumerInUse(String),
    /// Every consumer slot is taken ([`Limits::max_consumers`](crate::Limits::max_consumers)).
    #[error("all {0} consumer slots are taken")]
    TooManyConsumers(usize),
    /// A confirmation names a delivery that was not received in this session.
    #[error("confirmation of a delivery that was not received")]
    BadConfirm,
    /// A query is malformed, such as a window whose start is not before its
    /// end.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// The aggregate does not apply to the series' kind (spec §6 Aggregates).
    #[error("aggregate not applicable to this series")]
    AggregateNotApplicable,
    /// No span over local detail remains for the window (spec §6 Old spans).
    #[error("detail not local")]
    DetailNotLocal,
    /// Refused while a restore is bringing detail back (spec §8 Restore).
    #[error("restoring")]
    Restoring,
    /// A time outside the range the store can represent (UTC nanoseconds in
    /// an `i64`).
    #[error("time out of range")]
    TimeOutOfRange,
    /// A failed write or sync: the store is now read-only (spec §3).
    #[error("fault: {0}")]
    Fault(String),
    /// Named by the specification and the interface, but not supported by
    /// the current backend yet.
    #[error("not supported yet: {what}")]
    NotSupportedYet {
        /// What is not supported.
        what: &'static str,
    },
    /// An error of the backend that has no other mapping.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Why an emit has to wait (spec §3 Back-pressure, §2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum BusyReason {
    /// The ingest queue is full.
    #[error("ingest queue full")]
    QueueFull,
    /// Free space is below the critical mark.
    #[error("disk below the critical mark")]
    DiskFull,
    /// A host-stamped reading while the clock is not synchronised: it waits
    /// rather than being stamped with a wrong time (spec §2.5).
    #[error("host clock not synchronised")]
    UnsyncedClock,
}

/// Why one reading was refused (spec §2). Every refusal is counted.
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
    UnknownState,
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
    /// A reading of an interval series whose `t` is not an interval start
    /// (spec §2.1).
    #[error("t is not an interval start")]
    OffPhase,
    /// `t` outside the range the store can represent.
    #[error("time out of range")]
    TimeOutOfRange,
    /// A consumer emitted a reading on a series outside its `emits` (spec §4
    /// Routing).
    #[error("series not in the consumer's emits")]
    NotDeclared,
}
