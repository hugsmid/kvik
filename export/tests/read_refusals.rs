//! Files `read_rows` refuses (Q-1322): a `kvik.export_version` it does not know, and a null in a never-null column.

mod common;

use std::sync::Arc;

use arrow_array::{RecordBatch, new_null_array};
use arrow_schema::Schema;
use common::{export_to_vec, f64_series, read_back, row};
use kvik::Value;
use kvik_export::{ChangedAt, ExportError, ExportOptions, ExportRow, SeriesSchema, read_rows};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

const VERSION: &str = "kvik.export_version";

fn versioned() -> (SeriesSchema, Vec<ExportRow>) {
    let series = SeriesSchema {
        changed_at: Some(ChangedAt::Version),
        ..f64_series(1)
    };
    let rows = (0..2)
        .map(|k| ExportRow {
            changed_at: Some(5),
            ..row(1, k, k as u64 + 1, Value::F64(1.0))
        })
        .collect();
    (series, rows)
}

fn file() -> Vec<u8> {
    let (series, rows) = versioned();
    export_to_vec(&series, &rows, &ExportOptions::default()).unwrap()
}

/// `file` written again by another writer, with its metadata and its one batch changed.
fn rewritten(
    file: &[u8],
    meta: impl Fn(&mut Vec<KeyValue>),
    batch: impl Fn(RecordBatch) -> RecordBatch,
) -> Vec<u8> {
    let options = ArrowReaderOptions::new().with_skip_arrow_metadata(true);
    let b = ParquetRecordBatchReaderBuilder::try_new_with_options(
        bytes::Bytes::copy_from_slice(file),
        options,
    )
    .unwrap();
    let mut kv = b
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .unwrap_or_default();
    meta(&mut kv);
    let mut batches: Vec<RecordBatch> = b.build().unwrap().map(Result::unwrap).collect();
    assert_eq!(batches.len(), 1);
    let one = batch(batches.remove(0));
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(kv))
        .build();
    let mut out = Vec::new();
    let mut w = ArrowWriter::try_new(&mut out, one.schema(), Some(props)).unwrap();
    w.write(&one).unwrap();
    w.close().unwrap();
    out
}

fn read_all(file: Vec<u8>) -> Result<Vec<ExportRow>, ExportError> {
    read_rows(bytes::Bytes::from(file))?.collect()
}

fn with_version(v: Option<&str>) -> impl Fn(&mut Vec<KeyValue>) {
    move |kv| {
        kv.retain(|e| e.key != VERSION);
        if let Some(v) = v {
            kv.push(KeyValue::new(VERSION.into(), v.to_string()));
        }
    }
}

#[test]
fn a_file_written_again_unchanged_by_another_writer_reads_back_the_same_rows() {
    let f = file();
    let again = rewritten(&f, with_version(Some("1")), |b| b);
    assert_eq!(read_all(again).unwrap(), read_back(&f));
}

#[test]
fn a_file_of_an_unknown_or_missing_export_version_is_refused() {
    for v in [Some("2"), Some("0"), Some(""), None] {
        let err = read_all(rewritten(&file(), with_version(v), |b| b)).unwrap_err();
        assert!(
            matches!(err, ExportError::NotAnExport { .. }),
            "{v:?}: {err}"
        );
    }
}

#[test]
fn a_null_in_any_never_null_column_is_refused() {
    let f = file();
    let names: Vec<String> = {
        let b =
            ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(&f)).unwrap();
        b.schema()
            .fields()
            .iter()
            .filter(|c| !c.is_nullable())
            .map(|c| c.name().clone())
            .collect()
    };
    assert!(names.len() >= 15, "{names:?}");
    for name in names {
        let nulled = rewritten(
            &f,
            |_| {},
            |b| {
                let schema = b.schema();
                let k = schema.index_of(&name).unwrap();
                let mut fields: Vec<_> =
                    schema.fields().iter().map(|c| c.as_ref().clone()).collect();
                fields[k] = fields[k].clone().with_nullable(true);
                let mut columns = b.columns().to_vec();
                columns[k] = new_null_array(columns[k].data_type(), b.num_rows());
                RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
            },
        );
        let err = read_all(nulled).unwrap_err();
        assert!(
            matches!(err, ExportError::NotAnExport { .. }),
            "{name}: {err}"
        );
    }
}
