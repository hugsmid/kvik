//! The error of an export or a read-back.

use kvik::{Position, QueryError, SeriesId};

/// Why an export or a read-back failed. An export that fails leaves its output incomplete.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// The store refused the read.
    #[error("read failed: {0}")]
    Query(#[from] QueryError),
    /// The Parquet writer or reader failed, including its I/O.
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Building or reading an Arrow batch failed.
    #[error("arrow: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// The options cannot be used.
    #[error("invalid options: {reason}")]
    InvalidOptions {
        /// Why.
        reason: &'static str,
    },
    /// The series has a kind or value type the schema does not cover yet.
    #[error("series not supported by the export: {reason}")]
    UnsupportedSeries {
        /// Why.
        reason: &'static str,
    },
    /// A fact of another series reached the exporter.
    #[error("fact at {position} belongs to series {found}, not {expected}")]
    WrongSeries {
        /// The fact's position.
        position: Position,
        /// The series being exported.
        expected: SeriesId,
        /// The fact's series.
        found: SeriesId,
    },
    /// A fact's value does not match the series' value type.
    #[error("fact at {position}: value does not match the series' value type")]
    WrongValue {
        /// The fact's position.
        position: Position,
    },
    /// A fact with a value and the `no_value` flag, or without a value and without the flag (spec §2.5).
    #[error("fact at {position}: value present or absent against its no_value flag")]
    NoValueMismatch {
        /// The fact's position.
        position: Position,
    },
    /// A decimal fact is stored at a higher scale than the file's decimal column (Q-192).
    #[error("fact at {position}: stored scale {stored} above the export scale {export}")]
    ScaleAboveExport {
        /// The fact's position.
        position: Position,
        /// Its stored scale.
        stored: u8,
        /// The scale of the file's decimal column.
        export: u8,
    },
    /// A decimal value does not fit the file's decimal column.
    #[error("fact at {position}: decimal value out of the column's range")]
    DecimalOutOfRange {
        /// The fact's position.
        position: Position,
    },
    /// `changed_at` missing on a versioned series, or present on another (spec §2.2).
    #[error("fact at {position}: changed_at does not match the series' versioning")]
    ChangedAtMismatch {
        /// The fact's position.
        position: Position,
    },
    /// A `changed_at` of kind `time` above `i64::MAX`, which a Parquet timestamp cannot hold; ingest refuses one (Q-195).
    #[error("fact at {position}: changed_at {changed_at} beyond the timestamp range")]
    ChangedAtOutOfRange {
        /// The fact's position.
        position: Position,
        /// The value.
        changed_at: u64,
    },
    /// An interval end beyond the timestamp range.
    #[error("fact at {position}: interval end beyond the timestamp range")]
    IntervalEndOutOfRange {
        /// The fact's position.
        position: Position,
    },
    /// A partitioned export could not open one of its files.
    #[error("cannot open {path}: {source}")]
    Open {
        /// The file's relative path.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// A file that does not follow the export schema.
    #[error("not an export file: {reason}")]
    NotAnExport {
        /// Why.
        reason: String,
    },
}
