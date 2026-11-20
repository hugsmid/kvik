//! The errors of a durable reader: from the log reader's, and copies of one that several calls report.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kvik_log::reader::ReaderError;

use crate::error::{OpenError, QueryError};

pub(crate) fn open_error(dir: &Path, e: ReaderError) -> OpenError {
    match e {
        ReaderError::Store(e) => crate::durable::store_error(dir, e),
        ReaderError::MarkUnreadable => OpenError::MarkUnreadable,
        ReaderError::Unsupported { path, .. } => OpenError::UnsupportedFormat {
            file: path,
            found: 0,
            supported: u32::from(kvik_log::FORMAT_MAJOR),
        },
        ReaderError::Io { path, source } => OpenError::Io { path, source },
        other => OpenError::Corrupt {
            file: PathBuf::from(format!("{}: {other}", dir.display())),
        },
    }
}

/// A copy of an error that several calls report: the error of a refresh one call led and others followed.
pub(crate) fn copy(e: &QueryError) -> QueryError {
    use QueryError as Q;
    match e {
        Q::UnknownSeries => Q::UnknownSeries,
        Q::DetailNotLocal { local_from } => Q::DetailNotLocal {
            local_from: *local_from,
        },
        Q::AggregateRefused { reason } => Q::AggregateRefused {
            reason: reason.clone(),
        },
        Q::UnknownZone => Q::UnknownZone,
        Q::NotAvailable { reason } => Q::NotAvailable {
            reason: reason.clone(),
        },
        Q::LostRange { id } => Q::LostRange { id: *id },
        Q::MarkUnreadable => Q::MarkUnreadable,
        Q::UnsupportedFormat { file } => Q::UnsupportedFormat { file: file.clone() },
        Q::Corrupt { file } => Q::Corrupt { file: file.clone() },
        Q::Io { path, source } => Q::Io {
            path: path.clone(),
            source: match source.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(source.kind(), source.to_string()),
            },
        },
        Q::Invalid { reason } => Q::Invalid {
            reason: reason.clone(),
        },
        Q::Closed => Q::Closed,
        Q::CutExpired => Q::CutExpired,
    }
}

pub(super) fn unshare(e: Arc<QueryError>) -> QueryError {
    Arc::try_unwrap(e).unwrap_or_else(|e| copy(&e))
}

pub(crate) fn query_error(dir: &Path, e: ReaderError) -> QueryError {
    match e {
        ReaderError::MarkUnreadable => QueryError::MarkUnreadable,
        ReaderError::Unsupported { path, .. } => QueryError::UnsupportedFormat { file: path },
        ReaderError::Content { path, .. } | ReaderError::TooLong { path, .. } => {
            QueryError::Corrupt { file: path }
        }
        ReaderError::Io { path, source } => QueryError::Io { path, source },
        ReaderError::CutEnded(_) => QueryError::CutExpired,
        ReaderError::SegmentMissing { file_id } => QueryError::NotAvailable {
            reason: format!("segment {file_id} is missing"),
        },
        other => QueryError::Corrupt {
            file: PathBuf::from(format!("{}: {other}", dir.display())),
        },
    }
}
