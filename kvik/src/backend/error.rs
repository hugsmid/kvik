//! The backend's own error, converted at the interface into the error type
//! of each call (`docs/api.md` Errors).

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{
    ConsumerError, DefineError, EmitError, Fault, OpenError, QueryError, WriteError,
};
use crate::types::SeriesId;

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("locked")]
    Locked,
    #[error("not a store")]
    NotAStore,
    #[error("configuration {key}: {reason}")]
    Config { key: &'static str, reason: String },
    #[error("closed")]
    Closed,
    #[error("read-only: {0}")]
    ReadOnly(String),
    #[error("sync failed")]
    SyncFailed,
    #[error("unknown series {0}")]
    UnknownSeries(String),
    #[error("series {0} is retired")]
    Retired(SeriesId),
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("name {0:?} in use")]
    NameInUse(String),
    #[error("conflicts with the live series' {0}")]
    Conflict(&'static str),
    #[error("series ceiling of {0} reached")]
    Ceiling(usize),
    #[error("unknown time zone {0:?}")]
    UnknownZone(String),
    #[error("site zone {in_force} in force, {configured} configured")]
    SiteZoneMismatch {
        in_force: String,
        configured: String,
    },
    #[error("route refused: {0}")]
    Route(String),
    #[error("consumer {0:?} in use")]
    ConsumerInUse(String),
    #[error("all {0} consumer slots are taken")]
    TooManyConsumers(usize),
    #[error("bad confirmation")]
    BadConfirm,
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    #[error("aggregate refused: {0}")]
    AggregateRefused(String),
    #[error("restoring")]
    Restoring,
    #[error("too large: {readings} readings, {bytes} bytes")]
    TooLarge { readings: usize, bytes: usize },
    #[error("not supported yet: {0}")]
    NotSupportedYet(&'static str),
    #[error("internal: {0}")]
    Internal(String),
}

fn fault(dir: &Path) -> Fault {
    Fault::SyncFailed {
        path: dir.to_path_buf(),
    }
}

fn internal(dir: &Path, what: String) -> Fault {
    Fault::Io {
        op: "model",
        path: PathBuf::from(dir),
        kind: io::Error::other(what).kind(),
    }
}

impl Error {
    pub(crate) fn open(self, dir: &Path) -> OpenError {
        match self {
            Error::Locked => OpenError::Locked,
            Error::NotAStore => OpenError::NotAStore,
            Error::Config { key, reason } => OpenError::Config { key, reason },
            Error::UnknownZone(_) => OpenError::UnknownZone,
            Error::SiteZoneMismatch {
                in_force,
                configured,
            } => OpenError::SiteZoneMismatch {
                configured,
                in_force,
            },
            other => OpenError::Io {
                path: dir.to_path_buf(),
                source: io::Error::other(other.to_string()),
            },
        }
    }

    pub(crate) fn write(self, dir: &Path) -> WriteError {
        match self {
            Error::Closed => WriteError::Closed,
            Error::ReadOnly(_) => WriteError::ReadOnly(fault(dir)),
            Error::SyncFailed => WriteError::OutcomeUnknown(fault(dir)),
            other => WriteError::OutcomeUnknown(internal(dir, other.to_string())),
        }
    }

    pub(crate) fn emit(self, dir: &Path) -> EmitError {
        match self {
            Error::Closed => EmitError::Closed,
            Error::ReadOnly(_) => EmitError::ReadOnly(fault(dir)),
            Error::SyncFailed => EmitError::OutcomeUnknown(fault(dir)),
            Error::TooLarge { readings, bytes } => EmitError::TooLarge { readings, bytes },
            other => EmitError::OutcomeUnknown(internal(dir, other.to_string())),
        }
    }

    pub(crate) fn define(self, dir: &Path) -> DefineError {
        match self {
            Error::Ceiling(n) => DefineError::Ceiling {
                limit: u32::try_from(n).unwrap_or(u32::MAX),
            },
            Error::NameInUse(_) => DefineError::NameInUse,
            Error::Conflict(field) => DefineError::Conflict { field },
            Error::Invalid { field, reason } => DefineError::Invalid { field, reason },
            Error::UnknownZone(_) => DefineError::UnknownZone,
            Error::UnknownSeries(_) => DefineError::UnknownSeries,
            Error::Retired(_) => DefineError::Retired,
            Error::NotSupportedYet(what) => DefineError::NotSupportedYet { what },
            e @ (Error::Closed | Error::ReadOnly(_) | Error::SyncFailed) => {
                DefineError::Write(e.write(dir))
            }
            other => DefineError::Invalid {
                field: "definition",
                reason: other.to_string(),
            },
        }
    }

    pub(crate) fn query(self) -> QueryError {
        match self {
            Error::UnknownSeries(_) => QueryError::UnknownSeries,
            Error::AggregateRefused(reason) => QueryError::AggregateRefused { reason },
            Error::UnknownZone(_) => QueryError::UnknownZone,
            Error::InvalidQuery(reason) => QueryError::Invalid { reason },
            Error::Closed => QueryError::Closed,
            Error::Restoring => QueryError::NotAvailable {
                reason: "a restore is bringing detail back".into(),
            },
            Error::NotSupportedYet(what) => QueryError::NotAvailable {
                reason: format!("not supported yet: {what}"),
            },
            other => QueryError::NotAvailable {
                reason: other.to_string(),
            },
        }
    }

    pub(crate) fn consumer(self, dir: &Path) -> ConsumerError {
        match self {
            Error::Closed => ConsumerError::Closed,
            Error::ConsumerInUse(n) => ConsumerError::InUse(n),
            Error::TooManyConsumers(n) => ConsumerError::TooManyConsumers(n),
            Error::UnknownSeries(_) => ConsumerError::UnknownSeries,
            Error::Route(r) => ConsumerError::Route(r),
            Error::BadConfirm => ConsumerError::BadConfirm,
            Error::NotSupportedYet(what) => ConsumerError::NotSupportedYet { what },
            e @ (Error::ReadOnly(_) | Error::SyncFailed) => ConsumerError::Write(e.write(dir)),
            other => ConsumerError::Route(other.to_string()),
        }
    }
}
