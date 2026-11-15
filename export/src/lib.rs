//! Parquet export of kvik facts (roadmap 6.4). The schema is provisional until the owner approves it
//! (`docs/export.md`, Q-190 to Q-197).
//!
//! [`export_series`] streams one series' facts from [`Reader::facts`] into one Parquet file. The exporter holds
//! at most the open row group ([`ExportOptions::row_group_rows`]) plus one batch ([`ExportOptions::batch_rows`]),
//! whatever the number of facts; the bound holds end to end once `Reader::facts` streams (today both backends
//! gather the range first). [`read_rows`] reads a file back into [`ExportRow`]s.
//!
//! ```
//! use kvik_export::{ExportOptions, SeriesSchema, ValueKind, KindName, TimingName, export_rows, read_rows};
//! use kvik::SeriesId;
//!
//! let series = SeriesSchema {
//!     id: SeriesId(1),
//!     name: "plant/line-1/temperature".into(),
//!     unit: "°C".into(),
//!     kind: KindName::Gauge,
//!     value: ValueKind::F64,
//!     changed_at: None,
//!     timing: TimingName::Observed,
//!     interval: None,
//! };
//! let mut file = Vec::new();
//! let summary = export_rows(&series, std::iter::empty(), &ExportOptions::default(), &mut file).unwrap();
//! assert_eq!(summary.rows, 0);
//! let back = read_rows(bytes::Bytes::from(file)).unwrap();
//! assert_eq!(back.metadata("kvik.series_name"), Some("plant/line-1/temperature"));
//! ```

#![forbid(unsafe_code)]

mod batch;
mod error;
mod read;
mod row;
mod schema;

use std::io::Write;

use kvik::{Reader, SeriesId, TimeRange, Timestamp, View};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

pub use batch::QUALITY_NAMES;
pub use error::ExportError;
pub use read::{ExportReader, read_rows};
pub use row::{ExportRow, Flags};
pub use schema::{
    ChangedAt, DECIMAL_PRECISION, EXPORT_VERSION, KindName, MAX_DECIMAL_SCALE, SeriesSchema,
    TimingName, ValueKind, column,
};

/// The zstd level of every column (`docs/export.md` File layout, Q-196).
pub const ZSTD_LEVEL: i32 = 1;

/// How an export reads and writes (`docs/export.md` Options).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportOptions {
    /// The view read (spec §2.3); [`View::Current`] by default (Q-191).
    pub view: View,
    /// Facts with `t` in this half-open range; by default every `t` below `i64::MAX`.
    pub range: TimeRange,
    /// Most rows in a row group: the bound on the writer's memory. Default 131,072.
    pub row_group_rows: usize,
    /// Rows buffered before they are handed to the writer. Default 8,192.
    pub batch_rows: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            view: View::Current,
            range: TimeRange::new(Timestamp(i64::MIN), Timestamp(i64::MAX)),
            row_group_rows: 131_072,
            batch_rows: 8_192,
        }
    }
}

impl ExportOptions {
    /// Sets the view.
    pub fn view(mut self, view: View) -> Self {
        self.view = view;
        self
    }

    /// Sets the range of `t`.
    pub fn range(mut self, range: impl Into<TimeRange>) -> Self {
        self.range = range.into();
        self
    }

    /// Sets the most rows in a row group.
    pub fn row_group_rows(mut self, n: usize) -> Self {
        self.row_group_rows = n;
        self
    }

    /// Sets the rows buffered per batch.
    pub fn batch_rows(mut self, n: usize) -> Self {
        self.batch_rows = n;
        self
    }

    fn metadata(&self) -> Result<Vec<(String, String)>, ExportError> {
        if self.row_group_rows == 0 || self.batch_rows == 0 {
            return Err(ExportError::InvalidOptions {
                reason: "row_group_rows and batch_rows must be at least 1",
            });
        }
        let view = match self.view {
            View::Current => "current".to_string(),
            View::History => "history".to_string(),
            View::AsOf(p) => format!("as_of {}:{}", p.generation, p.sequence),
            View::AsOfTime(t) => format!("as_of_time {}", t.0),
            _ => {
                return Err(ExportError::InvalidOptions {
                    reason: "a view the export does not know",
                });
            }
        };
        Ok(vec![
            ("kvik.view".into(), view),
            ("kvik.range_from_ns".into(), self.range.from.0.to_string()),
            ("kvik.range_to_ns".into(), self.range.to.0.to_string()),
        ])
    }
}

/// What an export wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportSummary {
    /// Rows written.
    pub rows: u64,
    /// Row groups written.
    pub row_groups: usize,
}

/// Exports one series' facts in `options.range` and `options.view` from a reader into one Parquet file.
pub fn export_series<W: Write + Send>(
    reader: &Reader,
    id: SeriesId,
    options: &ExportOptions,
    out: W,
) -> Result<ExportSummary, ExportError> {
    let series = SeriesSchema::from_info(&reader.series(id)?)?;
    let facts = reader.facts(id, options.range, options.view)?;
    export_rows(
        &series,
        facts.map(|f| f.map(ExportRow::from).map_err(ExportError::from)),
        options,
        out,
    )
}

/// Exports rows of one series into one Parquet file, pulling them as it writes. `options.view` and
/// `options.range` are only recorded in the metadata.
pub fn export_rows<W, I>(
    series: &SeriesSchema,
    rows: I,
    options: &ExportOptions,
    out: W,
) -> Result<ExportSummary, ExportError>
where
    W: Write + Send,
    I: IntoIterator<Item = Result<ExportRow, ExportError>>,
{
    let mut kv = series.metadata();
    kv.extend(options.metadata()?);
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(options.row_group_rows))
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(ZSTD_LEVEL)?))
        .set_key_value_metadata(Some(
            kv.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect(),
        ))
        .build();
    let mut columns = batch::Columns::new(series, options.batch_rows)?;
    let mut writer = ArrowWriter::try_new(out, columns.schema().clone(), Some(props))?;
    let mut written = 0u64;
    for row in rows {
        columns.push(&row?)?;
        written += 1;
        if columns.len() >= options.batch_rows {
            writer.write(&columns.finish()?)?;
        }
    }
    if columns.len() > 0 {
        writer.write(&columns.finish()?)?;
    }
    let meta = writer.close()?;
    Ok(ExportSummary {
        rows: written,
        row_groups: meta.num_row_groups(),
    })
}
