//! Reading an export file back into rows, from its Parquet types alone: the embedded Arrow schema is ignored, so
//! the read sees what another tool sees.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Decimal128Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use kvik::{Position, Quality, SeriesId, Timestamp, Value};
use parquet::arrow::arrow_reader::{
    ArrowReaderOptions, ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder,
};
use parquet::file::reader::ChunkReader;

use crate::batch::QUALITY_NAMES;
use crate::schema::{EXPORT_VERSION, column};
use crate::{ExportError, ExportRow, Flags};

/// The rows of an export file, streamed one record batch at a time.
pub struct ExportReader {
    batches: ParquetRecordBatchReader,
    batch: Option<RecordBatch>,
    next: usize,
    metadata: Vec<(String, String)>,
    row_groups: usize,
}

impl std::fmt::Debug for ExportReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportReader")
            .field("next", &self.next)
            .field("row_groups", &self.row_groups)
            .finish_non_exhaustive()
    }
}

/// Opens an export file for reading back.
pub fn read_rows<R: ChunkReader + 'static>(input: R) -> Result<ExportReader, ExportError> {
    let options = ArrowReaderOptions::new().with_skip_arrow_metadata(true);
    let builder = ParquetRecordBatchReaderBuilder::try_new_with_options(input, options)?;
    let file = builder.metadata().file_metadata();
    let metadata: Vec<(String, String)> = file
        .key_value_metadata()
        .map(|kv| {
            kv.iter()
                .map(|e| (e.key.clone(), e.value.clone().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    let version = metadata
        .iter()
        .find(|(k, _)| k == "kvik.export_version")
        .map(|(_, v)| v.as_str());
    // `docs/export.md` Compatibility: a new version is a breaking change, so its columns cannot be trusted (Q-1322).
    if version != Some(EXPORT_VERSION) {
        return Err(bad(format!("export version {version:?}")));
    }
    let row_groups = builder.metadata().num_row_groups();
    Ok(ExportReader {
        batches: builder.build()?,
        batch: None,
        next: 0,
        metadata,
        row_groups,
    })
}

impl ExportReader {
    /// A value of the file's key-value metadata (`docs/export.md` Metadata).
    pub fn metadata(&self, key: &str) -> Option<&str> {
        self.metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The number of row groups in the file.
    pub fn row_groups(&self) -> usize {
        self.row_groups
    }
}

impl Iterator for ExportReader {
    type Item = Result<ExportRow, ExportError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(b) = &self.batch
                && self.next < b.num_rows()
            {
                let i = self.next;
                self.next += 1;
                return Some(decode(b, i));
            }
            match self.batches.next()? {
                Ok(b) => {
                    if let Some(name) = null_in_never_null(&b) {
                        return Some(Err(bad(format!("a null in column {name}"))));
                    }
                    self.batch = Some(b);
                    self.next = 0;
                }
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}

/// The first column `docs/export.md` Columns declares never null that holds a null (Q-1322).
fn null_in_never_null(b: &RecordBatch) -> Option<&'static str> {
    [
        column::SERIES_ID,
        column::SERIES_NAME,
        column::T,
        column::GENERATION,
        column::SEQUENCE,
        column::RECEIVED_AT,
        column::QUALITY,
        column::SUPPLIED_QUALITY,
        column::CHANGED_AT,
        column::INTERVAL_END,
    ]
    .into_iter()
    .chain(column::FLAGS)
    .find(|name| b.column_by_name(name).is_some_and(|c| c.null_count() > 0))
}

fn bad(reason: impl Into<String>) -> ExportError {
    ExportError::NotAnExport {
        reason: reason.into(),
    }
}

fn col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a dyn Array, ExportError> {
    b.column_by_name(name)
        .map(|c| c.as_ref())
        .ok_or_else(|| bad(format!("no column {name}")))
}

fn typed<'a, T: Array + 'static>(c: &'a dyn Array, name: &str) -> Result<&'a T, ExportError> {
    c.as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| bad(format!("column {name} has type {}", c.data_type())))
}

fn ts(b: &RecordBatch, name: &str, i: usize) -> Result<i64, ExportError> {
    let c = col(b, name)?;
    Ok(typed::<arrow_array::PrimitiveArray<TimestampNanosecondType>>(c, name)?.value(i))
}

fn quality(b: &RecordBatch, name: &str, i: usize) -> Result<Quality, ExportError> {
    let c = col(b, name)?;
    let s = typed::<arrow_array::StringArray>(c, name)?.value(i);
    match QUALITY_NAMES.iter().position(|q| *q == s) {
        Some(0) => Ok(Quality::Good),
        Some(1) => Ok(Quality::Estimated),
        Some(2) => Ok(Quality::Suspect),
        Some(3) => Ok(Quality::Bad),
        _ => Err(bad(format!("unknown quality {s:?}"))),
    }
}

fn value(b: &RecordBatch, i: usize) -> Result<Value, ExportError> {
    let c = col(b, column::VALUE)?;
    if c.is_null(i) {
        return Ok(Value::Absent);
    }
    Ok(match c.data_type() {
        DataType::Float64 => {
            Value::F64(c.as_primitive::<arrow_array::types::Float64Type>().value(i))
        }
        DataType::Decimal128(_, scale) => {
            let scale = u8::try_from(*scale).map_err(|_| bad("negative decimal scale"))?;
            let v = c.as_primitive::<Decimal128Type>().value(i);
            let stored = col(b, column::STORED_SCALE)?;
            if stored.is_null(i) {
                return Err(bad("decimal value without a stored scale"));
            }
            let s = stored
                .as_primitive_opt::<UInt8Type>()
                .ok_or_else(|| bad("stored_scale is not UINT_8"))?
                .value(i);
            let div = scale
                .checked_sub(s)
                .and_then(|k| 10i128.checked_pow(u32::from(k)))
                .ok_or_else(|| bad("stored scale above the column's scale"))?;
            if v % div != 0 {
                return Err(bad("decimal value finer than its stored scale"));
            }
            let units = i64::try_from(v / div).map_err(|_| bad("decimal units beyond i64"))?;
            Value::Decimal { units, scale: s }
        }
        DataType::Boolean => Value::Bool(c.as_boolean().value(i)),
        DataType::UInt16 => Value::State(c.as_primitive::<UInt16Type>().value(i)),
        DataType::Utf8 => Value::Text(c.as_string::<i32>().value(i).to_string()),
        t => return Err(bad(format!("value column of type {t}"))),
    })
}

fn decode(b: &RecordBatch, i: usize) -> Result<ExportRow, ExportError> {
    let mut flags = [false; 7];
    for (f, name) in flags.iter_mut().zip(column::FLAGS) {
        *f = typed::<arrow_array::BooleanArray>(col(b, name)?, name)?.value(i);
    }
    let changed_at = match b.column_by_name(column::CHANGED_AT) {
        None => None,
        Some(c) => Some(match c.data_type() {
            DataType::Timestamp(..) => u64::try_from(ts(b, column::CHANGED_AT, i)?)
                .map_err(|_| bad("negative changed_at"))?,
            DataType::UInt64 => c.as_primitive::<UInt64Type>().value(i),
            t => return Err(bad(format!("changed_at column of type {t}"))),
        }),
    };
    Ok(ExportRow {
        series: SeriesId(
            col(b, column::SERIES_ID)?
                .as_primitive_opt::<UInt32Type>()
                .ok_or_else(|| bad("series_id is not UINT_32"))?
                .value(i),
        ),
        t: Timestamp(ts(b, column::T, i)?),
        position: Position::new(
            col(b, column::GENERATION)?
                .as_primitive_opt::<UInt32Type>()
                .ok_or_else(|| bad("generation is not UINT_32"))?
                .value(i),
            col(b, column::SEQUENCE)?
                .as_primitive_opt::<UInt64Type>()
                .ok_or_else(|| bad("sequence is not UINT_64"))?
                .value(i),
        ),
        received_at: Timestamp(ts(b, column::RECEIVED_AT, i)?),
        value: value(b, i)?,
        quality: quality(b, column::QUALITY, i)?,
        supplied_quality: quality(b, column::SUPPLIED_QUALITY, i)?,
        flags: Flags::from_array(flags),
        changed_at,
    })
}
