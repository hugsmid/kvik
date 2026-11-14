//! Candidate (c): the same chunks written as Parquet.
//!
//! One file per span, one row group per chunk. Columns, in this order; the
//! three value columns appear only when a chunk of their kind is in the file,
//! and are null in rows without a value and in chunks of another kind:
//!
//! | Column | Type | Encoding |
//! |---|---|---|
//! | `t` | INT64 | DELTA_BINARY_PACKED |
//! | `quality` | INT32 (u8) | dictionary (RLE_DICTIONARY) |
//! | `flags` | INT32 (u16) | dictionary |
//! | `position` | INT64 (u64) | DELTA_BINARY_PACKED |
//! | `value` | DOUBLE, nullable | BYTE_STREAM_SPLIT, ALP or dictionary |
//! | `state` | INT32 (u16), nullable | dictionary |
//! | `integer` | INT64, nullable | DELTA_BINARY_PACKED |
//!
//! Every column is compressed with zstd at [`ZSTD_LEVEL`]; data pages hold at
//! most [`BLOCK_ROWS`] rows. Page statistics, and so a column index, are
//! written only for `t` (the own format keeps only first and last `t` per
//! block); offset indexes are written for every column.
//! A reader caches the footer and page index once per file. A range scan uses
//! the page index of `t` to select whole pages, reads them with a row
//! selection and trims the rows outside the range.

use std::sync::{Arc, OnceLock};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type, UInt8Type, UInt16Type, UInt64Type};
use arrow_array::{
    Array, ArrayRef, Float64Array, Int64Array, RecordBatch, UInt8Array, UInt16Array, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::PageIndexPolicy;
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::schema::types::ColumnPath;

use crate::{BLOCK_ROWS, Chunk, Codec, ColumnSizes, FLAG_NO_VALUE, ValueKind, ZSTD_LEVEL};

/// Batch size used when reading.
const READ_BATCH: usize = 16 * 1024;

/// The Parquet candidate, with a choice of encoding for `value`.
#[derive(Clone, Copy, Debug)]
pub struct ParquetCodec {
    value: Encoding,
    name: &'static str,
}

impl ParquetCodec {
    /// (c) `value` as BYTE_STREAM_SPLIT.
    pub const BYTE_STREAM_SPLIT: ParquetCodec = ParquetCodec {
        value: Encoding::BYTE_STREAM_SPLIT,
        name: "Parquet, BYTE_STREAM_SPLIT",
    };
    /// (c) `value` as ALP.
    pub const ALP: ParquetCodec = ParquetCodec {
        value: Encoding::ALP,
        name: "Parquet, ALP",
    };
    /// (c) `value` dictionary-encoded (the writer falls back to plain for a
    /// page when the dictionary outgrows its 1 MiB limit).
    pub const DICTIONARY: ParquetCodec = ParquetCodec {
        value: Encoding::RLE_DICTIONARY,
        name: "Parquet, dictionary",
    };
}

/// One Parquet file holding a set of chunks. Its footer and page index are
/// parsed on first use and then cached, outside the timed encode.
pub struct ParquetFile {
    bytes: Bytes,
    meta: OnceLock<ArrowReaderMetadata>,
    kinds: Vec<ValueKind>,
    /// Whether each chunk has a row with a value.
    has_values: Vec<bool>,
    /// Column index of the value column for each kind (F64, State, I64).
    value_columns: [Option<usize>; 3],
}

impl ParquetFile {
    /// The file's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    fn meta(&self) -> &ArrowReaderMetadata {
        self.meta
            .get_or_init(|| ArrowReaderMetadata::load(&self.bytes, options()).expect("metadata"))
    }

    fn value_column(&self, kind: ValueKind) -> usize {
        self.value_columns[kind_index(kind)].expect("value column for the kind")
    }
}

const KINDS: [ValueKind; 3] = [ValueKind::F64, ValueKind::State, ValueKind::I64];

fn kind_index(kind: ValueKind) -> usize {
    match kind {
        ValueKind::F64 => 0,
        ValueKind::State => 1,
        ValueKind::I64 => 2,
    }
}

const COL_T: usize = 0;
const COL_QUALITY: usize = 1;
const COL_FLAGS: usize = 2;
const COL_POSITION: usize = 3;

fn schema(value_columns: &[Option<usize>; 3]) -> SchemaRef {
    let mut fields = vec![
        Field::new("t", DataType::Int64, false),
        Field::new("quality", DataType::UInt8, false),
        Field::new("flags", DataType::UInt16, false),
        Field::new("position", DataType::UInt64, false),
    ];
    for (k, col) in value_columns.iter().enumerate() {
        if col.is_some() {
            fields.push(match KINDS[k] {
                ValueKind::F64 => Field::new("value", DataType::Float64, true),
                ValueKind::State => Field::new("state", DataType::UInt16, true),
                ValueKind::I64 => Field::new("integer", DataType::Int64, true),
            });
        }
    }
    Arc::new(Schema::new(fields))
}

/// Writer properties: zstd, pages of [`BLOCK_ROWS`] rows, page statistics
/// (and so a column index) only for `stats_column`, and the given encoding
/// per column, dictionary for [`Encoding::RLE_DICTIONARY`]. Offset indexes are
/// written for every column, so that a row selection can skip pages.
fn properties(stats_column: &str, columns: &[(&str, Encoding)]) -> WriterProperties {
    let mut b = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(ZSTD_LEVEL).expect("zstd level"),
        ))
        .set_data_page_row_count_limit(BLOCK_ROWS)
        .set_write_batch_size(1024)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_column_statistics_enabled(ColumnPath::from(stats_column), EnabledStatistics::Page)
        .set_max_row_group_row_count(None);
    for &(name, encoding) in columns {
        let path = ColumnPath::from(name);
        b = if encoding == Encoding::RLE_DICTIONARY {
            b.set_column_dictionary_enabled(path, true)
        } else {
            b.set_column_dictionary_enabled(path.clone(), false)
                .set_column_encoding(path, encoding)
        };
    }
    b.build()
}

fn batch(schema: &SchemaRef, value_columns: &[Option<usize>; 3], c: &Chunk) -> RecordBatch {
    let n = c.len();
    let has = |i: usize| c.flags[i] & FLAG_NO_VALUE == 0;
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(c.t.clone())),
        Arc::new(UInt8Array::from(c.quality.clone())),
        Arc::new(UInt16Array::from(c.flags.clone())),
        Arc::new(UInt64Array::from(c.position.clone())),
    ];
    for (k, col) in value_columns.iter().enumerate() {
        if col.is_none() {
            continue;
        }
        let kind = KINDS[k];
        let own = kind == c.kind;
        columns.push(match kind {
            ValueKind::F64 if own => Arc::new(
                (0..n)
                    .map(|i| has(i).then(|| f64::from_bits(c.value[i])))
                    .collect::<Float64Array>(),
            ),
            ValueKind::F64 => Arc::new(Float64Array::new_null(n)),
            ValueKind::State if own => Arc::new(
                (0..n)
                    .map(|i| {
                        has(i).then(|| u16::try_from(c.value[i]).expect("state index fits u16"))
                    })
                    .collect::<UInt16Array>(),
            ),
            ValueKind::State => Arc::new(UInt16Array::new_null(n)),
            ValueKind::I64 if own => Arc::new(
                (0..n)
                    .map(|i| has(i).then_some(c.value[i] as i64))
                    .collect::<Int64Array>(),
            ),
            ValueKind::I64 => Arc::new(Int64Array::new_null(n)),
        });
    }
    RecordBatch::try_new(schema.clone(), columns).expect("record batch")
}

fn append(batch: &RecordBatch, kind: ValueKind, value_col: usize, out: &mut Chunk) {
    out.t
        .extend_from_slice(batch.column(COL_T).as_primitive::<Int64Type>().values());
    let v = batch.column(value_col);
    match kind {
        ValueKind::F64 => {
            let v = v.as_primitive::<Float64Type>();
            if v.null_count() == 0 {
                out.value.extend(v.values().iter().map(|x| x.to_bits()));
            } else {
                out.value
                    .extend(v.iter().map(|x| x.map_or(0, f64::to_bits)));
            }
        }
        ValueKind::State => {
            let v = v.as_primitive::<UInt16Type>();
            out.value.extend(v.iter().map(|x| x.map_or(0, u64::from)));
        }
        ValueKind::I64 => {
            let v = v.as_primitive::<Int64Type>();
            if v.null_count() == 0 {
                out.value.extend(v.values().iter().map(|&x| x as u64));
            } else {
                out.value
                    .extend(v.iter().map(|x| x.map_or(0, |x| x as u64)));
            }
        }
    }
    out.quality.extend_from_slice(
        batch
            .column(COL_QUALITY)
            .as_primitive::<UInt8Type>()
            .values(),
    );
    out.flags.extend_from_slice(
        batch
            .column(COL_FLAGS)
            .as_primitive::<UInt16Type>()
            .values(),
    );
    out.position.extend_from_slice(
        batch
            .column(COL_POSITION)
            .as_primitive::<UInt64Type>()
            .values(),
    );
}

fn options() -> ArrowReaderOptions {
    ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required)
}

impl ParquetCodec {
    /// Checks that the writer used the intended encodings and page size.
    /// Panics otherwise.
    pub fn check_layout(&self, file: &ParquetFile) {
        let md = file.meta().metadata();
        assert_eq!(md.num_row_groups(), file.kinds.len());
        for (i, (rg, &kind)) in md.row_groups().iter().zip(&file.kinds).enumerate() {
            let has = |col: usize, e: Encoding| rg.column(col).encodings().any(|x| x == e);
            assert!(has(COL_T, Encoding::DELTA_BINARY_PACKED), "t encoding");
            assert!(
                has(COL_POSITION, Encoding::DELTA_BINARY_PACKED),
                "position encoding"
            );
            assert!(
                has(COL_QUALITY, Encoding::RLE_DICTIONARY),
                "quality encoding"
            );
            assert!(has(COL_FLAGS, Encoding::RLE_DICTIONARY), "flags encoding");
            let col = file.value_column(kind);
            if file.has_values[i] {
                let expected = match kind {
                    ValueKind::F64 => self.value,
                    ValueKind::State => Encoding::RLE_DICTIONARY,
                    ValueKind::I64 => Encoding::DELTA_BINARY_PACKED,
                };
                assert!(has(col, expected), "value encoding");
            }
        }
        for i in 0..md.num_row_groups() {
            let rows = md.row_group(i).num_rows() as usize;
            for col in 0..md.row_group(i).num_columns() {
                let pages = md
                    .page_index_for_row_group(i)
                    .offset_index(col)
                    .expect("offset index")
                    .page_locations()
                    .len();
                assert_eq!(
                    pages,
                    rows.div_ceil(BLOCK_ROWS),
                    "pages of column {col} in row group {i}"
                );
            }
        }
    }
}

impl Codec for ParquetCodec {
    type Encoded = ParquetFile;

    fn name(&self) -> &'static str {
        self.name
    }

    fn encode(&self, chunks: &[&Chunk]) -> ParquetFile {
        let mut value_columns = [None; 3];
        let mut next = COL_POSITION + 1;
        for (k, slot) in value_columns.iter_mut().enumerate() {
            if chunks.iter().any(|c| c.kind == KINDS[k]) {
                *slot = Some(next);
                next += 1;
            }
        }
        let schema = schema(&value_columns);
        let props = properties(
            "t",
            &[
                ("t", Encoding::DELTA_BINARY_PACKED),
                ("value", self.value),
                ("integer", Encoding::DELTA_BINARY_PACKED),
                ("position", Encoding::DELTA_BINARY_PACKED),
            ],
        );
        let mut w = ArrowWriter::try_new(Vec::new(), schema.clone(), Some(props)).expect("writer");
        for c in chunks {
            assert!(!c.is_empty(), "empty chunks are not written");
            w.write(&batch(&schema, &value_columns, c)).expect("write");
            w.flush().expect("flush");
        }
        ParquetFile {
            bytes: Bytes::from(w.into_inner().expect("close")),
            meta: OnceLock::new(),
            kinds: chunks.iter().map(|c| c.kind).collect(),
            has_values: chunks
                .iter()
                .map(|c| c.flags.iter().any(|f| f & FLAG_NO_VALUE == 0))
                .collect(),
            value_columns,
        }
    }

    fn sizes(&self, f: &ParquetFile) -> Vec<ColumnSizes> {
        let md = f.meta().metadata();
        let mut out: Vec<ColumnSizes> = md
            .row_groups()
            .iter()
            .map(|rg| {
                let c = |i: usize| rg.column(i).compressed_size() as u64;
                ColumnSizes {
                    t: c(COL_T),
                    value: f.value_columns.iter().flatten().map(|&i| c(i)).sum(),
                    quality_flags: c(COL_QUALITY) + c(COL_FLAGS),
                    position: c(COL_POSITION),
                    overhead: 0,
                }
            })
            .collect();
        let columns: u64 = out.iter().map(ColumnSizes::total).sum();
        let rest = f.bytes.len() as u64 - columns;
        let n = out.len() as u64;
        for (i, s) in out.iter_mut().enumerate() {
            s.overhead = rest / n + u64::from((i as u64) < rest % n);
        }
        out
    }

    fn check(&self, f: &ParquetFile) {
        self.check_layout(f);
    }

    fn open(&self, f: &ParquetFile) {
        std::hint::black_box(ArrowReaderMetadata::load(&f.bytes, options()).expect("metadata"));
    }

    fn decode(&self, f: &ParquetFile, index: usize) -> Chunk {
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(f.bytes.clone(), f.meta().clone())
                .with_row_groups(vec![index])
                .with_batch_size(READ_BATCH)
                .build()
                .expect("reader");
        let kind = f.kinds[index];
        let col = f.value_column(kind);
        let mut out = Chunk::new(kind);
        for b in reader {
            append(&b.expect("batch"), kind, col, &mut out);
        }
        out
    }

    fn decode_range(&self, f: &ParquetFile, index: usize, from: i64, to: i64) -> Chunk {
        let md = f.meta().metadata();
        let rows = md.row_group(index).num_rows() as usize;
        let pi = md.page_index_for_row_group(index);
        let Some(ColumnIndexMetaData::INT64(ci)) = pi.column_index(COL_T) else {
            panic!("no column index for t");
        };
        let locations = pi
            .offset_index(COL_T)
            .expect("offset index")
            .page_locations();
        let ranges = (0..locations.len()).filter_map(|p| {
            let min = *ci.min_value(p).expect("min");
            let max = *ci.max_value(p).expect("max");
            (max >= from && min < to).then(|| {
                let start = locations[p].first_row_index as usize;
                let end = locations
                    .get(p + 1)
                    .map_or(rows, |l| l.first_row_index as usize);
                start..end
            })
        });
        let selection = RowSelection::from_consecutive_ranges(ranges, rows);
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(f.bytes.clone(), f.meta().clone())
                .with_row_groups(vec![index])
                .with_row_selection(selection)
                .with_batch_size(READ_BATCH)
                .build()
                .expect("reader");
        let kind = f.kinds[index];
        let col = f.value_column(kind);
        let mut tmp = Chunk::new(kind);
        for b in reader {
            append(&b.expect("batch"), kind, col, &mut tmp);
        }
        tmp.slice_t(from, to)
    }

    fn i64_column_size(&self, columns: &[&[i64]]) -> u64 {
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
        let props = properties("x", &[("x", Encoding::DELTA_BINARY_PACKED)]);
        let mut w = ArrowWriter::try_new(Vec::new(), schema.clone(), Some(props)).expect("writer");
        for col in columns.iter().filter(|c| !c.is_empty()) {
            let b = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int64Array::from(col.to_vec())) as ArrayRef],
            )
            .expect("batch");
            w.write(&b).expect("write");
            w.flush().expect("flush");
        }
        let md = w.close().expect("close");
        md.row_groups()
            .iter()
            .map(|rg| rg.column(0).compressed_size() as u64)
            .sum()
    }
}
